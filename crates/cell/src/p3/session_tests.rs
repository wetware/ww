//! Composed guest cancellation through the production P3 transport linker.

use super::{add_transport_to_linker, GrantedTransport, HostTransport, TransportHostState};
use anyhow::{Context as _, Result};
use authority::system_capnp::{executor, membrane};
use capnp::capability::Promise;
use capnp_rpc::{rpc_twoparty_capnp::Side, twoparty::VatNetwork, RpcSystem};
use std::cell::RefCell;
use std::collections::BTreeSet;
use std::future::Future;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{oneshot, watch};
use tokio::task::JoinHandle;
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};
use wasmtime::component::{Component, FutureReader, Linker, ResourceTable};
use wasmtime::{Config, Engine, Store, StoreContextMut};
use wasmtime_wasi::{WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};

const ARTIFACT_ENV: &str = "WW_RPC_SESSION_P3_FIXTURE";
const TIMEOUT: Duration = Duration::from_secs(20);

#[derive(Clone)]
struct Observer {
    events: Arc<Mutex<Vec<u32>>>,
    seen: watch::Sender<BTreeSet<u32>>,
}

impl Observer {
    fn new() -> Self {
        Self {
            events: Arc::default(),
            seen: watch::channel(BTreeSet::new()).0,
        }
    }

    fn event(&self, code: u32) {
        self.events.lock().unwrap().push(code);
        self.seen.send_modify(|seen| {
            seen.insert(code);
        });
    }

    async fn wait(&self, code: u32) -> wasmtime::Result<()> {
        let mut seen = self.seen.subscribe();
        loop {
            if seen.borrow_and_update().contains(&code) {
                return Ok(());
            }
            seen.changed().await.map_err(wasmtime::Error::new)?;
        }
    }

    fn events(&self) -> Vec<u32> {
        self.events.lock().unwrap().clone()
    }
}

struct State {
    wasi: WasiCtx,
    table: ResourceTable,
    transport: GrantedTransport,
    observer: Observer,
}

impl WasiView for State {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.wasi,
            table: &mut self.table,
        }
    }
}

impl TransportHostState for State {
    fn granted_transport(&mut self) -> &mut GrantedTransport {
        &mut self.transport
    }
}

fn add_observer(linker: &mut Linker<State>) -> wasmtime::Result<()> {
    let mut observer = linker.instance("wetware:session-cancel/observer@0.1.0")?;
    observer.func_wrap(
        "event",
        |store: StoreContextMut<'_, State>, (code,): (u32,)| {
            store.data().observer.event(code);
            Ok(())
        },
    )?;
    observer.func_wrap(
        "wait-event",
        |mut store: StoreContextMut<'_, State>, (code,): (u32,)| {
            let observer = store.data().observer.clone();
            Ok((FutureReader::new(&mut store, async move {
                observer.wait(code).await
            })?,))
        },
    )?;
    Ok(())
}

#[derive(Default)]
struct Counts {
    cid: AtomicUsize,
    spawn: AtomicUsize,
}

struct Peer {
    counts: Arc<Counts>,
    exports: RefCell<Vec<membrane::Client>>,
}

impl executor::Server for Peer {
    fn cid(
        self: capnp::capability::Rc<Self>,
        _: executor::CidParams,
        mut results: executor::CidResults,
    ) -> impl Future<Output = capnp::Result<()>> + 'static {
        self.counts.cid.fetch_add(1, Ordering::Relaxed);
        results.get().set_cid("session-probe");
        Promise::ok(())
    }

    fn spawn(
        self: capnp::capability::Rc<Self>,
        params: executor::SpawnParams,
        _: executor::SpawnResults,
    ) -> impl Future<Output = capnp::Result<()>> + 'static {
        self.counts.spawn.fetch_add(1, Ordering::Relaxed);
        self.exports
            .borrow_mut()
            .push(params.get().unwrap().get_membrane().unwrap());
        // Retaining the real imported export and unresolved response is the
        // pressure on connection-owned cleanup. A later cid is the ordered
        // dispatch barrier, so no sleeps or native-task scheduling assumption
        // establish readiness.
        Promise::from_future(std::future::pending())
    }
}

struct Driver {
    task: JoinHandle<capnp::Result<()>>,
    counts: Arc<Counts>,
    bootstrap: membrane::Client,
}

impl Drop for Driver {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn peer() -> (Driver, GrantedTransport) {
    let (host, grant) = HostTransport::bounded_pair();
    let (read, write) = tokio::io::split(host);
    let counts = Arc::new(Counts::default());
    let bootstrap: executor::Client = capnp_rpc::new_client(Peer {
        counts: counts.clone(),
        exports: RefCell::default(),
    });
    let mut rpc = RpcSystem::new(
        Box::new(VatNetwork::new(
            read.compat(),
            write.compat_write(),
            Side::Server,
            Default::default(),
        )),
        Some(bootstrap.client),
    );
    let bootstrap = rpc.bootstrap(Side::Client);
    (
        Driver {
            task: tokio::task::spawn_local(rpc),
            counts,
            bootstrap,
        },
        grant,
    )
}

async fn bounded<F: Future>(future: F) -> Result<F::Output> {
    tokio::time::timeout(TIMEOUT, future)
        .await
        .context("guest session cancellation watchdog elapsed")
}

fn regrant(store: &mut Store<State>, grant: GrantedTransport) {
    // This fixture can exercise independent sessions in a surviving Store.
    // Production still grants one endpoint exactly once; only this test State
    // replaces an already-consumed grant after the explicit readiness barrier.
    assert!(
        store.data().transport.endpoint.is_none(),
        "previous endpoint was not consumed"
    );
    store.data_mut().transport = grant;
}

fn before(events: &[u32], first: u32, second: u32) {
    let position = |code| {
        events
            .iter()
            .position(|event| *event == code)
            .unwrap_or_else(|| panic!("missing event {code}: {events:?}"))
    };
    assert!(
        position(first) < position(second),
        "event {first} must precede {second}: {events:?}"
    );
}

async fn scenario(engine: &Engine, component: &Component, mode: u32, cancel: bool) -> Result<()> {
    let observer = Observer::new();
    let (victim_peer, grant) = peer();
    let mut store = Store::new(
        engine,
        State {
            wasi: WasiCtxBuilder::new().inherit_stderr().build(),
            table: ResourceTable::new(),
            transport: grant,
            observer: observer.clone(),
        },
    );
    let mut linker = Linker::new(engine);
    wasmtime_wasi::p3::add_to_linker(&mut linker)?;
    add_transport_to_linker(&mut linker)?;
    add_observer(&mut linker)?;
    let instance = linker.instantiate_async(&mut store, component).await?;
    let run = instance.get_typed_func::<(u32, bool), (u32,)>(&mut store, "run")?;
    let sibling = instance.get_typed_func::<(FutureReader<()>,), (u32,)>(&mut store, "sibling")?;
    let reenter = instance.get_typed_func::<(), (u32,)>(&mut store, "reenter")?;
    let run_call = run.start_call_concurrent(&mut store, (mode, cancel))?;
    let ready = observer.clone();
    bounded(store.run_concurrent(async move |_| ready.wait(12).await)).await???;

    if mode == 2 {
        // Exercise an actual exported bootstrap over the driven session and
        // retain its import after cancellation. The server-side tracked owner
        // must disappear despite this retained external client.
        let response = victim_peer.bootstrap.graft_request().send();
        let result =
            bounded(store.run_concurrent(async move |_| response.promise.await)).await???;
        assert_eq!(result.get()?.get_peer_id()?, b"guest-bootstrap");
    }
    let (sibling_peer, grant) = peer();
    regrant(&mut store, grant);
    let (release_sibling, sibling_gate) = oneshot::channel::<()>();
    let gate = FutureReader::new(&mut store, async move {
        sibling_gate.await.map_err(wasmtime::Error::new)?;
        wasmtime::error::Ok(())
    })?;
    let sibling_call = sibling.start_call_concurrent(&mut store, (gate,))?;

    // Retain both independent host-root handles outside the event loop. Even
    // a regression trapping the parent cannot erase the sibling handle or
    // prevent the host from releasing its gate.
    let main_result =
        bounded(store.run_concurrent(async move |access| {
            run.finish_call_concurrent(access, run_call).await
        }))
        .await;
    let events_after_main = observer.events();
    assert!(
        release_sibling.send(()).is_ok(),
        "independent sibling gate disappeared"
    );
    let sibling_result = bounded(store.run_concurrent(async move |access| {
        sibling.finish_call_concurrent(access, sibling_call).await
    }))
    .await;
    assert_eq!(main_result???.0, 42, "mode={mode} cancel={cancel}");
    assert_eq!(sibling_result???.0, 99, "mode={mode} cancel={cancel}");
    assert_eq!(sibling_peer.counts.cid.load(Ordering::Relaxed), 2);
    assert_eq!(sibling_peer.counts.spawn.load(Ordering::Relaxed), 0);
    assert_eq!(
        victim_peer.counts.cid.load(Ordering::Relaxed),
        if mode == 1 || mode == 2 { 2 } else { 0 }
    );
    assert_eq!(
        victim_peer.counts.spawn.load(Ordering::Relaxed),
        if mode == 1 || mode == 2 { 2 } else { 0 }
    );

    let (reentry_peer, grant) = peer();
    regrant(&mut store, grant);
    let result = bounded(
        store.run_concurrent(async move |access| reenter.call_concurrent(access, ()).await),
    )
    .await???;
    assert_eq!(result.0, 123);
    assert_eq!(reentry_peer.counts.cid.load(Ordering::Relaxed), 1);

    let events = observer.events();
    for code in [
        12, 30, 40, 21, 22, 23, 24, 25, 26, 70, 71, 110, 41, 50, 31, 32, 90,
    ] {
        assert!(events.contains(&code), "missing event {code}: {events:?}");
    }
    for (first, second) in [
        (12, 40),
        (30, 40),
        (21, 41),
        (26, 41),
        (70, 71),
        (71, 41),
        (110, 41),
        (41, 50),
        (50, 31),
        (31, 32),
        (32, 90),
    ] {
        before(&events, first, second);
    }
    if mode != 1 {
        before(&events, 20, 41);
    }
    if mode == 3 {
        before(&events, 20, 13);
        before(&events, 21, 13);
    }
    if mode == 1 || mode == 2 {
        before(&events, 111, 41);
    }
    if cancel {
        assert!(events.contains(&60), "cancelled component gate must close");
        assert!(!events.contains(&120), "cancelled app body completed");
        let start = events.iter().position(|event| *event == 40).unwrap();
        assert!(
            !events[start + 1..]
                .iter()
                .any(|event| matches!(event, 100 | 101)),
            "cancelled victim/app body repolled: {events:?}"
        );
    } else if mode == 1 || mode == 2 {
        assert!(
            events.contains(&120),
            "live task wake did not complete the app"
        );
    }
    assert!(
        !events_after_main.contains(&31),
        "sibling resumed before host released its gate"
    );
    eprintln!("guest-session-cancel mode={mode} cancel={cancel}: {events:?}");
    Ok(())
}

async fn exercise(mode: u32) -> Result<()> {
    let artifact = PathBuf::from(std::env::var_os(ARTIFACT_ENV).context(
        "WW_RPC_SESSION_P3_FIXTURE must name the freshly composed guest-session-cancel component",
    )?);
    let mut config = Config::new();
    config.wasm_component_model_async(true);
    config.wasm_component_model_threading(true);
    let engine = Engine::new(&config)?;
    let component = Component::from_file(&engine, artifact)?;
    tokio::task::LocalSet::new()
        .run_until(async {
            for cancel in [false, true] {
                scenario(&engine, &component, mode, cancel).await?;
            }
            Ok(())
        })
        .await
}

#[tokio::test]
#[ignore = "requires the composed guest-session-cancel fixture"]
async fn p3_guest_session_unpolled_raw_owner_cancellation() -> Result<()> {
    exercise(0).await
}

#[tokio::test]
#[ignore = "requires the composed guest-session-cancel fixture"]
async fn p3_guest_session_run_cancellation() -> Result<()> {
    exercise(1).await
}

#[tokio::test]
#[ignore = "requires the composed guest-session-cancel fixture"]
async fn p3_guest_session_serve_cancellation() -> Result<()> {
    exercise(2).await
}

#[tokio::test]
#[ignore = "requires the composed guest-session-cancel fixture"]
async fn p3_guest_session_extracted_fields_do_not_detach() -> Result<()> {
    exercise(3).await
}
