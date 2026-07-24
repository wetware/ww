//! Membrane overhead microbenchmark (roadmap D13).
//!
//! Measures the cost the membrane adds over a bare local capability on two
//! paths:
//!   * `ping` — a small call/return with no capabilities in the results. This
//!     is the pure per-call filter overhead (a `Policy::check` plus the request
//!     re-copy on `send`).
//!   * `child` — a call that returns a capability, so the membrane must rewrap
//!     the returned cap. This is the deep-copy path that upstream capnp
//!     cap-table pluggability would eliminate; the recorded ratio is the
//!     tripwire for whether that upstream work is justified (D13).
//!
//! Reuses the crate's toy `Thing` interface by including the build-generated
//! code directly, so the bench needs no public test surface.

use std::future::Future;
use std::rc::Rc;

use capnp::capability::Rc as CapRc;
use capnp::traits::HasTypeId;
use capnp::Error;

use criterion::{criterion_group, criterion_main, Criterion};
use tokio::runtime::Builder;
use tokio::task::LocalSet;

use membrane::{membrane, Allowlist, Policy};

// Must be named `test_thing_capnp`: the generated code refers to its own
// siblings via `crate::test_thing_capnp::...`.
mod test_thing_capnp {
    include!(concat!(env!("OUT_DIR"), "/test_thing_capnp.rs"));
}
use test_thing_capnp::thing;

const PING: u16 = 0;
const CHILD: u16 = 2;

struct ThingImpl;

impl thing::Server for ThingImpl {
    fn ping(
        self: CapRc<Self>,
        _params: thing::PingParams,
        mut results: thing::PingResults,
    ) -> impl Future<Output = Result<(), Error>> + 'static {
        results.get().set_msg("pong");
        std::future::ready(Ok(()))
    }

    fn forbidden(
        self: CapRc<Self>,
        _params: thing::ForbiddenParams,
        mut results: thing::ForbiddenResults,
    ) -> impl Future<Output = Result<(), Error>> + 'static {
        results.get().set_msg("secret");
        std::future::ready(Ok(()))
    }

    fn child(
        self: CapRc<Self>,
        _params: thing::ChildParams,
        mut results: thing::ChildResults,
    ) -> impl Future<Output = Result<(), Error>> + 'static {
        results.get().set_thing(capnp_rpc::new_client(ThingImpl));
        std::future::ready(Ok(()))
    }

    async fn echo(
        self: CapRc<Self>,
        _params: thing::EchoParams,
        mut results: thing::EchoResults,
    ) -> Result<(), Error> {
        results.get().set_msg("echo");
        Ok(())
    }
}

fn policy() -> Rc<dyn Policy> {
    Rc::new(
        Allowlist::new()
            .allow(thing::Client::TYPE_ID, PING)
            .allow(thing::Client::TYPE_ID, CHILD),
    )
}

fn bench_membrane(c: &mut Criterion) {
    let rt = Builder::new_current_thread().build().unwrap();
    let local = LocalSet::new();

    let (bare, membraned): (thing::Client, thing::Client) = local.block_on(&rt, async {
        let bare: thing::Client = capnp_rpc::new_client(ThingImpl);
        let raw: thing::Client = capnp_rpc::new_client(ThingImpl);
        let membraned = membrane(raw, policy());
        (bare, membraned)
    });

    let mut ping = c.benchmark_group("ping_roundtrip");
    ping.bench_function("bare", |b| {
        b.iter(|| {
            local.block_on(&rt, async {
                bare.ping_request().send().promise.await.unwrap();
            });
        });
    });
    ping.bench_function("membraned", |b| {
        b.iter(|| {
            local.block_on(&rt, async {
                membraned.ping_request().send().promise.await.unwrap();
            });
        });
    });
    ping.finish();

    let mut child = c.benchmark_group("child_returns_cap");
    child.bench_function("bare", |b| {
        b.iter(|| {
            local.block_on(&rt, async {
                let resp = bare.child_request().send().promise.await.unwrap();
                let _c: thing::Client = resp.get().unwrap().get_thing().unwrap();
            });
        });
    });
    child.bench_function("membraned", |b| {
        b.iter(|| {
            local.block_on(&rt, async {
                let resp = membraned.child_request().send().promise.await.unwrap();
                let _c: thing::Client = resp.get().unwrap().get_thing().unwrap();
            });
        });
    });
    child.finish();
}

criterion_group!(benches, bench_membrane);
criterion_main!(benches);
