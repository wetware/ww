//! Guest runtime for Wetware WASM cells.
//!
//! # Execution Model
//!
//! Each WASM guest runs inside a single-threaded wasmtime `Store` on a
//! tokio `LocalSet` worker.  The host spawns two tasks on the same
//! `LocalSet`: the WASM guest (via `call_run_async`) and a Cap'n Proto
//! RPC system that bridges the guest's data streams to the host's
//! membrane.
//!
//! Cooperative scheduling works through two mechanisms:
//!
//! 1. **Fuel yield** (`fuel_async_yield_interval`): wasmtime suspends
//!    the guest every 10K instructions (see `sched::YIELD_INTERVAL`),
//!    returning `Poll::Pending` from `call_run_async`.  This gives the
//!    host-side RPC task time to process messages.
//!
//! 2. **WASI poll**: when the guest calls `wasi:io/poll#poll`, wasmtime
//!    makes a host call that yields back to the tokio executor.  The
//!    host-side RPC task can run during this yield.
//!
//! The `poll_loop` function is the guest's cooperative scheduler.  It
//! alternates between polling the capnp RPC system (to process inbound
//! messages) and polling user work (the guest's async entry point).
//!
//! # Why `Waker::noop()`?
//!
//! The poll loop creates a noop waker because WASI poll is the real
//! wakeup mechanism, not the Rust waker.  When `StreamReader::poll_read`
//! returns `Pending` with an empty buffer, it calls `wake_by_ref()` —
//! but this is a no-op.  The actual wakeup happens when `wasi_poll::poll`
//! returns because the reader pollable is ready.  This is correct for
//! single-threaded WASM where there's no cross-task notification needed.
//!
//! # Write-flush invariant
//!
//! The `WRITE_OCCURRED` thread-local tracks whether the RPC system or
//! user work wrote bytes during the current poll cycle.  If writes
//! occurred, the loop polls the RPC system again (to flush outbound
//! messages) and includes the writer pollable in the WASI poll set.
//! If no writes occurred, only the reader and an idle timeout are polled.
//!
//! The idle timeout (100ms) is a safety net for missed wakeups: the
//! host's `AsyncReadStream` background task can race with the
//! foreground pollable check, causing a missed wakeup that would
//! otherwise block the guest indefinitely.

use capnp::capability::FromClientHook;
use capnp_rpc::rpc_twoparty_capnp::Side;
use capnp_rpc::twoparty::VatNetwork;
use capnp_rpc::RpcSystem;
use std::cell::Cell;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll, Waker};

// Tracks whether any data was written during the current poll cycle.
// Single-threaded WASM, so a thread-local Cell<bool> is race-free.
thread_local! {
    static WRITE_OCCURRED: Cell<bool> = const { Cell::new(false) };
}

mod bindings {
    wit_bindgen::generate!({
        path: "../../crates/cell/wit",
        world: "guest-streams",
        with: {
            "wasi:io/error@0.2.9": wasip2::io::error,
            "wasi:io/poll@0.2.9": wasip2::io::poll,
            "wasi:io/streams@0.2.9": wasip2::io::streams,
        },
    });
}

use bindings::wetware::streams::streams::create_connection;
use wasip2::io::poll as wasi_poll;
use wasip2::io::streams::{
    InputStream as WasiInputStream, OutputStream as WasiOutputStream, Pollable as WasiPollable,
    StreamError as WasiStreamError,
};

pub struct StreamReader {
    stream: WasiInputStream,
    buffer: Vec<u8>,
    offset: usize,
}

impl StreamReader {
    pub fn new(stream: WasiInputStream) -> Self {
        Self {
            stream,
            buffer: Vec::new(),
            offset: 0,
        }
    }

    pub fn pollable(&self) -> WasiPollable {
        self.stream.subscribe()
    }
}

impl futures::io::AsyncRead for StreamReader {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut [u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        if self.offset < self.buffer.len() {
            let available = &self.buffer[self.offset..];
            let to_copy = available.len().min(buf.len());
            buf[..to_copy].copy_from_slice(&available[..to_copy]);
            self.offset += to_copy;
            if self.offset >= self.buffer.len() {
                self.buffer.clear();
                self.offset = 0;
            }
            return std::task::Poll::Ready(Ok(to_copy));
        }

        let len = buf.len() as u64;
        match self.stream.read(len) {
            Ok(bytes) => {
                if bytes.is_empty() {
                    cx.waker().wake_by_ref();
                    return std::task::Poll::Pending;
                }
                self.buffer = bytes;
                self.offset = 0;
                let available = &self.buffer[self.offset..];
                let to_copy = available.len().min(buf.len());
                buf[..to_copy].copy_from_slice(&available[..to_copy]);
                self.offset += to_copy;
                std::task::Poll::Ready(Ok(to_copy))
            }
            Err(WasiStreamError::Closed) => std::task::Poll::Ready(Ok(0)),
            Err(err) => std::task::Poll::Ready(Err(std::io::Error::other(format!(
                "stream read error: {:?}",
                err
            )))),
        }
    }
}

pub struct StreamWriter {
    stream: WasiOutputStream,
}

impl StreamWriter {
    pub fn new(stream: WasiOutputStream) -> Self {
        Self { stream }
    }

    pub fn pollable(&self) -> WasiPollable {
        self.stream.subscribe()
    }
}

impl futures::io::AsyncWrite for StreamWriter {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        if buf.is_empty() {
            return std::task::Poll::Ready(Ok(0));
        }
        match self.stream.check_write() {
            Ok(0) => {
                cx.waker().wake_by_ref();
                std::task::Poll::Pending
            }
            Ok(budget) => {
                let to_write = buf.len().min(budget as usize);
                match self.stream.write(&buf[..to_write]) {
                    Ok(_written) => {
                        WRITE_OCCURRED.with(|f| f.set(true));
                        std::task::Poll::Ready(Ok(to_write))
                    }
                    Err(WasiStreamError::Closed) => std::task::Poll::Ready(Ok(0)),
                    Err(err) => std::task::Poll::Ready(Err(std::io::Error::other(format!(
                        "stream write error: {:?}",
                        err
                    )))),
                }
            }
            Err(WasiStreamError::Closed) => std::task::Poll::Ready(Ok(0)),
            Err(err) => std::task::Poll::Ready(Err(std::io::Error::other(format!(
                "stream write error: {:?}",
                err
            )))),
        }
    }

    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        match self.stream.flush() {
            Ok(()) => std::task::Poll::Ready(Ok(())),
            Err(WasiStreamError::Closed) => std::task::Poll::Ready(Ok(())),
            Err(err) => std::task::Poll::Ready(Err(std::io::Error::other(format!(
                "stream flush error: {:?}",
                err
            )))),
        }
    }

    fn poll_close(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        match self.stream.flush() {
            Ok(()) => std::task::Poll::Ready(Ok(())),
            Err(WasiStreamError::Closed) => std::task::Poll::Ready(Ok(())),
            Err(err) => std::task::Poll::Ready(Err(std::io::Error::other(format!(
                "stream close error: {:?}",
                err
            )))),
        }
    }
}

pub struct StreamPollables {
    pub reader: WasiPollable,
    pub writer: WasiPollable,
}

/// Additional pollables to include in the guest's poll set.
///
/// `poll_loop` always waits on the RPC transport internally.
/// `PollSet` holds extra streams the guest wants serviced concurrently
/// (stdin, listeners, extra channels, etc.).
pub struct PollSet {
    pollables: Vec<WasiPollable>,
    /// Keeps source objects (streams, readers, etc.) alive so their child
    /// pollables remain valid for the lifetime of the poll set.
    _keep_alive: Vec<Box<dyn std::any::Any>>,
}

impl PollSet {
    pub fn new() -> Self {
        Self {
            pollables: Vec::new(),
            _keep_alive: Vec::new(),
        }
    }

    pub fn push(&mut self, p: WasiPollable) {
        self.pollables.push(p);
    }

    /// Push a pollable along with the source object it was derived from.
    ///
    /// The source is kept alive for the lifetime of the `PollSet`, preventing
    /// WASI "resource has children" errors when the parent stream would
    /// otherwise be dropped before its child pollable.
    pub fn push_with_source<T: 'static>(&mut self, p: WasiPollable, source: T) {
        self.pollables.push(p);
        self._keep_alive.push(Box::new(source));
    }
}

impl Default for PollSet {
    fn default() -> Self {
        Self::new()
    }
}

/// Safety-net timeout for idle poll cycles.
///
/// When the polling loop has no pending writes and made no progress, it blocks
/// on the reader pollable alone.  The host streams large responses (e.g. 1 MB
/// handler WASM) in chunks via wasmtime's `AsyncReadStream`, whose background
/// task can race with the foreground pollable check — causing a missed wakeup
/// that would block the guest indefinitely.
///
/// Adding a `wasi:clocks/monotonic-clock.subscribe-duration` pollable to the
/// poll set provides a guaranteed wakeup (per the WASI spec, this is the
/// canonical way to add a timeout to a poll).  In the common case the reader
/// fires first and latency is unaffected; if a wakeup is missed, the timeout
/// fires and the loop retries.
///
/// The pollable is created once before each loop and reused across iterations.
/// Because clock pollables are level-triggered (stay ready once elapsed), we
/// refresh only when the timeout actually fires.
const IDLE_POLL_TIMEOUT_NS: u64 = 100_000_000; // 100ms

fn new_idle_timeout() -> WasiPollable {
    wasip2::clocks::monotonic_clock::subscribe_duration(IDLE_POLL_TIMEOUT_NS)
}

pub struct GuestStreams {
    pub reader: StreamReader,
    pub writer: StreamWriter,
    pub pollables: StreamPollables,
}

pub fn connect_streams() -> GuestStreams {
    let connection = create_connection();
    let input_stream = connection.get_input_stream();
    let output_stream = connection.get_output_stream();

    let reader = StreamReader::new(input_stream);
    let writer = StreamWriter::new(output_stream);
    let pollables = StreamPollables {
        reader: reader.pollable(),
        writer: writer.pollable(),
    };

    GuestStreams {
        reader,
        writer,
        pollables,
    }
}

pub struct RpcSession<C> {
    pub rpc_system: RpcSystem<Side>,
    pub client: C,
    pub pollables: StreamPollables,
    pub poll_set: PollSet,
}

impl<C: FromClientHook> RpcSession<C> {
    pub fn connect() -> Self {
        Self::connect_with_export(None)
    }

    /// Connect and export `bootstrap` as this vat's bootstrap capability.
    ///
    /// The host can retrieve the exported cap via `rpc_system.bootstrap(Side::Client)`.
    /// Pass `None` for guests that do not export a capability (equivalent to `connect()`).
    pub fn connect_with_export(bootstrap: Option<capnp::capability::Client>) -> Self {
        let streams = connect_streams();
        let pollables = streams.pollables;
        let network = VatNetwork::new(
            streams.reader,
            streams.writer,
            Side::Client,
            Default::default(),
        );
        let mut rpc_system = RpcSystem::new(Box::new(network), bootstrap);
        let client = rpc_system.bootstrap(Side::Server);
        Self {
            rpc_system,
            client,
            pollables,
            poll_set: PollSet::new(),
        }
    }

    /// Register additional pollables to service concurrently with RPC.
    pub fn with_poll_set(mut self, poll_set: PollSet) -> Self {
        self.poll_set = poll_set;
        self
    }

    /// Clean up resources in WASI-safe order at process exit.
    ///
    /// WASI-P2 enforces that child resources (pollables) are dropped
    /// before their parents (streams).  Pollables are children of the
    /// streams inside `rpc_system`, so we drop them first.
    ///
    /// Cap'n Proto destructors are then leaked (`mem::forget`) because
    /// they try to close handles that the host has already torn down.
    ///
    /// Note: `serve_and_run` uses the same pattern inline (not this method)
    /// because it also owns a `future` that must be dropped between
    /// `poll_set` and `pollables`. See the teardown block in that function.
    pub fn forget(self) {
        // 1. Drop child resources (pollables) before parent streams.
        drop(self.poll_set);
        drop(self.pollables);
        // 2. Leak Cap'n Proto objects to avoid close-after-teardown panics.
        std::mem::forget(self.client);
        std::mem::forget(self.rpc_system);
    }
}

/// Why the poll loop exited without the user future completing.
///
/// The `cycle` field counts how many poll-loop iterations completed before
/// the RPC connection died.  Low values (< 5) mean the connection dropped
/// during bootstrap; high values mean it dropped mid-request.
#[derive(Debug)]
pub enum PollLoopExit {
    /// RPC connection closed cleanly (remote side hung up).
    RpcClosed { cycle: u64 },
    /// RPC connection closed with an error.
    RpcError { cycle: u64, error: capnp::Error },
}

impl PollLoopExit {
    pub fn cycle(&self) -> u64 {
        match self {
            PollLoopExit::RpcClosed { cycle } => *cycle,
            PollLoopExit::RpcError { cycle, .. } => *cycle,
        }
    }
}

impl std::fmt::Display for PollLoopExit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PollLoopExit::RpcClosed { cycle } => {
                write!(
                    f,
                    "RPC connection closed by host (after {cycle} poll cycles)"
                )
            }
            PollLoopExit::RpcError { cycle, error } => {
                write!(f, "RPC connection error after {cycle} poll cycles: {error}")
            }
        }
    }
}

/// Core poll loop: drive RPC alongside user work until one side finishes.
///
/// Each iteration: reset write flag → poll RPC → poll user work → flush
/// RPC writes → block on WASI I/O.
///
/// Returns `Ok(value)` when `poll_work` returns `Poll::Ready(value)`.
/// Returns `Err(PollLoopExit)` if the RPC connection closes first.
fn poll_loop<T>(
    rpc_system: &mut RpcSystem<Side>,
    pollables: &StreamPollables,
    extras: &PollSet,
    mut poll_work: impl FnMut(&mut Context<'_>) -> Poll<T>,
) -> Result<T, PollLoopExit> {
    let mut rpc_done = false;
    let mut rpc_exit: Option<PollLoopExit> = None;
    let mut idle_timeout = new_idle_timeout();
    let mut cycle: u64 = 0;
    loop {
        let mut cx = Context::from_waker(Waker::noop());
        WRITE_OCCURRED.with(|f| f.set(false));

        // ── Phase 1: Drive RPC (process inbound messages) ──
        if !rpc_done {
            if let Poll::Ready(result) = Pin::new(&mut *rpc_system).poll(&mut cx) {
                rpc_done = true;
                rpc_exit = Some(match result {
                    Ok(()) => PollLoopExit::RpcClosed { cycle },
                    Err(e) => PollLoopExit::RpcError { cycle, error: e },
                });
            }
        }

        // ── Phase 2: Drive user work ──
        if let Poll::Ready(val) = poll_work(&mut cx) {
            return Ok(val);
        }

        // ── Phase 3: Flush writes ──
        //
        // User work may have queued outbound RPC messages.  Poll the RPC
        // system again so those bytes reach the StreamWriter before we
        // decide whether to wait on the writer pollable.
        let wrote = WRITE_OCCURRED.with(|f| f.get());
        if !rpc_done && wrote {
            if let Poll::Ready(result) = Pin::new(&mut *rpc_system).poll(&mut cx) {
                rpc_done = true;
                rpc_exit = Some(match result {
                    Ok(()) => PollLoopExit::RpcClosed { cycle },
                    Err(e) => PollLoopExit::RpcError { cycle, error: e },
                });
            }
        }

        if rpc_done {
            return Err(rpc_exit.unwrap_or(PollLoopExit::RpcClosed { cycle }));
        }

        // ── Phase 4: Block on WASI I/O ──
        //
        // Build a poll set from the RPC transport + any extra pollables
        // registered by the guest (stdin, listeners, etc.).
        // If writes occurred, include the writer so the host can drain it.
        // Otherwise, include the idle timeout as a missed-wakeup safety net.
        let mut wasi_set: Vec<&WasiPollable> = Vec::with_capacity(2 + extras.pollables.len() + 1);
        wasi_set.push(&pollables.reader);
        if wrote {
            wasi_set.push(&pollables.writer);
        }
        for p in &extras.pollables {
            wasi_set.push(p);
        }
        if !wrote {
            wasi_set.push(&idle_timeout);
        }
        wasi_poll::poll(&wasi_set);
        if !wrote && idle_timeout.ready() {
            idle_timeout = new_idle_timeout();
        }

        cycle += 1;
    }
}

/// Export a bootstrap capability over WASI stdin/stdout.
///
/// This is for handler processes spawned by `Server.serve()`. The host wires
/// the handler's stdin/stdout to a libp2p stream. This function sets up a
/// Cap'n Proto RPC VatNetwork over stdin/stdout and exports the given
/// bootstrap capability. The remote peer bootstraps it to obtain the service.
///
/// Unlike [`serve`], this function does NOT use the wetware:streams connection.
/// It reads/writes directly from WASI stdin/stdout and drives the RPC system
/// until the connection closes. No host capabilities are available — if the
/// handler needs IPFS/routing, it should use `system::run()` over data_streams
/// instead.
///
/// # Example
///
/// ```no_run
/// let bootstrap: capnp::capability::Client =
///     todo!("construct the service capability exported by this guest");
/// system::serve_stdio(bootstrap);
/// ```
pub fn serve_stdio(bootstrap: capnp::capability::Client) {
    let stdin = wasip2::cli::stdin::get_stdin();
    let stdout = wasip2::cli::stdout::get_stdout();

    let reader = StreamReader::new(stdin);
    let writer = StreamWriter::new(stdout);
    let pollables = StreamPollables {
        reader: reader.pollable(),
        writer: writer.pollable(),
    };

    let network = VatNetwork::new(reader, writer, Side::Server, Default::default());
    let mut rpc_system = RpcSystem::new(Box::new(network), Some(bootstrap));

    // Drive RPC only (no user future) — poll_loop returns Err when RPC closes.
    let empty_extras = PollSet::new();
    if let Err(ref exit @ PollLoopExit::RpcError { .. }) =
        poll_loop(&mut rpc_system, &pollables, &empty_extras, |_| {
            Poll::<()>::Pending
        })
    {
        log::error!("serve_stdio: {exit}");
    }

    // WASI-P2 teardown: leak Cap'n Proto objects to avoid close-after-teardown
    // panics. At process exit the host reclaims all handles; running Cap'n Proto
    // destructors would try to close handles the host already tore down.
    // Pollables are also leaked here (no user future owns them, so no
    // parent-before-child ordering issue unlike serve_and_run / Session::forget).
    // See also: Session::forget() and the serve_and_run teardown block below.
    std::mem::forget(rpc_system);
    std::mem::forget(pollables);
}

/// Run a guest program with an async entry point, exporting a bootstrap capability.
///
/// Like [`run`], but the guest also provides `bootstrap` as its own bootstrap
/// capability on the RPC connection.  The host can retrieve it via
/// `rpc_system.bootstrap(Side::Client)`.
///
/// Use this when the guest needs to export a capability back to the host —
/// for example, a kernel that wraps and attenuates the host's Membrane before
/// re-exporting it to external peers.
///
/// # Example
///
/// ```no_run
/// let bootstrap: capnp::capability::Client =
///     todo!("construct the bootstrap capability exported by this guest");
/// system::serve(bootstrap, |host: capnp::capability::Client| async move {
///     // ... use host capabilities while exporting bootstrap to the host ...
///     let _ = host;
///     Ok::<(), capnp::Error>(())
/// });
/// ```
pub fn serve<C, F, Fut>(bootstrap: capnp::capability::Client, f: F)
where
    C: FromClientHook + Clone,
    F: FnOnce(C) -> Fut,
    Fut: Future<Output = Result<(), capnp::Error>>,
{
    run_with_session(RpcSession::<C>::connect_with_export(Some(bootstrap)), f)
}

/// Run a guest program with an async entry point.
///
/// Sets up the RPC session, bootstraps the host capability, and drives
/// the provided async closure to completion alongside the RPC system.
/// Handles all resource cleanup automatically.
///
/// # Example
///
/// ```no_run
/// system::run(|membrane: capnp::capability::Client| async move {
///     // Cast membrane to the generated Membrane client type in real guests.
///     let _ = membrane;
///     Ok::<(), capnp::Error>(())
/// });
/// ```
pub fn run<C, F, Fut>(f: F)
where
    C: FromClientHook + Clone,
    F: FnOnce(C) -> Fut,
    Fut: Future<Output = Result<(), capnp::Error>>,
{
    run_with_session(RpcSession::<C>::connect(), f)
}

/// Like [`run`], but with additional pollables in the poll set.
///
/// Use when the guest needs to service extra streams (e.g. stdin)
/// concurrently with the RPC connection.
pub fn run_with<C, F, Fut>(poll_set: PollSet, f: F)
where
    C: FromClientHook + Clone,
    F: FnOnce(C) -> Fut,
    Fut: Future<Output = Result<(), capnp::Error>>,
{
    run_with_session(RpcSession::<C>::connect().with_poll_set(poll_set), f)
}

fn run_with_session<C, F, Fut>(mut session: RpcSession<C>, f: F)
where
    C: FromClientHook + Clone,
    F: FnOnce(C) -> Fut,
    Fut: Future<Output = Result<(), capnp::Error>>,
{
    let client = session.client.clone();
    let mut future = Box::pin(f(client));

    match poll_loop(
        &mut session.rpc_system,
        &session.pollables,
        &session.poll_set,
        |cx| future.as_mut().poll(cx),
    ) {
        Ok(Ok(())) => {}
        Ok(Err(e)) => {
            log::error!("guest error: {e}");
        }
        Err(exit) => {
            log::error!("guest aborted: {exit}");
        }
    }

    // WASI-P2 teardown — resource ordering matters.
    //
    // WASI-P2 enforces that child resources (pollables) must be dropped before
    // their parents (streams inside rpc_system). Rust's default drop order
    // (reverse declaration) doesn't guarantee this, so we do it manually.
    //
    // Order:
    //   1. poll_set    — owns references to pollables, must go first
    //   2. future      — may capture streams whose pollables are in poll_set
    //   3. pollables   — children of streams in rpc_system
    //   4. forget client + rpc_system — leak Cap'n Proto objects; their
    //      destructors try to close handles the host has already torn down
    //      at process exit, causing panics
    //
    // This is the same pattern as Session::forget(), but we also own `future`
    // which must be dropped between poll_set and pollables.
    // See also: Session::forget() and the serve_stdio teardown above.
    let poll_set = session.poll_set;
    let pollables = session.pollables;
    drop(poll_set);
    drop(future);
    drop(pollables);
    std::mem::forget(session.client);
    std::mem::forget(session.rpc_system);
}
