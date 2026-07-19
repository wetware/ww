//! A schema-agnostic capability membrane built on
//! `capnp::private::capability::ClientHook`.
//!
//! Enforcement lives at the hook level, so a membraned capability cannot be
//! bypassed by casting the client to another type: every typed client, cast,
//! or promise pipeline bottoms out in `ClientHook::new_call`/`call` on the
//! same wrapped hook. The membrane:
//!   * denies calls not permitted by its [`Policy`], at the hook level;
//!   * re-wraps capabilities found in call *results* so everything reached
//!     through a membraned cap is itself membraned (recursive preservation);
//!   * re-wraps promise-pipelined capabilities (otherwise pipelining would be
//!     a membrane escape hatch);
//!   * re-wraps capabilities produced by promise resolution
//!     (`get_resolved` / `when_more_resolved`).
//!
//! Direction handled: caps flowing OUT of the membrane (results, pipelines,
//! resolution). Caps flowing INTO the membrane (request params) pass through
//! unwrapped here; the full dual membrane (reverse-wrap params, unwrap on
//! reentry) is tracked as M3 in the single-authority roadmap.
//!
//! [`Policy`] is a trait, so one membrane mechanism serves allowlists,
//! revocation, rate limits, and auditing. `check` takes `&self` but may hold
//! interior-mutable state; the membrane calls it once per invocation.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::rc::{Rc, Weak};
use std::time::{Duration, Instant};

use capnp::any_pointer;
use capnp::capability::{Promise, RemotePromise, Request, Response};
use capnp::message::{Builder, HeapAllocator};
use capnp::private::capability::{
    ClientHook, ParamsHook, PipelineHook, PipelineOp, RequestHook, ResponseHook, ResultsHook,
};
use capnp::traits::{Imbue, ImbueMut};
use capnp::{Error, MessageSize};
use futures::channel::oneshot;

type CapTable = Vec<Option<Box<dyn ClientHook>>>;

// Toy `Thing` interface for the crate's own integration tests (cast-bypass,
// recursive rewrap, pipelining, twoparty RPC). Test-only; compiled by build.rs.
#[cfg(test)]
#[allow(clippy::all, dead_code, unreachable_pub)]
mod test_thing_capnp {
    include!(concat!(env!("OUT_DIR"), "/test_thing_capnp.rs"));
}

#[cfg(test)]
mod integration_tests;

// ---------------------------------------------------------------------------
// Denial errors
// ---------------------------------------------------------------------------

/// Stable prefix marking an error as a membrane policy denial. Glia maps such
/// errors to `:glia.error/permission-denied` (roadmap D9); the marker plus the
/// `(interface_id, ordinal)` it carries let the mapping route without parsing
/// human-readable prose.
pub const DENIED_MARKER: &str = "ww-membrane/permission-denied";

/// Construct a fail-closed denial error carrying the method key.
pub fn denied_error(interface_id: u64, method_id: u16, reason: &str) -> Error {
    Error::failed(format!(
        "{DENIED_MARKER} interface={interface_id:#x} ordinal={method_id}: {reason}"
    ))
}

/// If `err` is a membrane denial, return the denied `(interface_id, ordinal)`.
///
/// Diagnostic helper for callers that want to route on denials; the marker is
/// the stable contract, the parse is best-effort.
pub fn denied_method_key(err: &Error) -> Option<(u64, u16)> {
    let s = err.to_string();
    let rest = s.split_once(DENIED_MARKER)?.1;
    let iface = rest.split("interface=").nth(1)?.split_whitespace().next()?;
    let ordinal = rest.split("ordinal=").nth(1)?;
    let ordinal = ordinal.split([' ', ':']).next()?;
    let iface = iface.strip_prefix("0x").unwrap_or(iface);
    Some((
        u64::from_str_radix(iface, 16).ok()?,
        ordinal.parse::<u16>().ok()?,
    ))
}

// ---------------------------------------------------------------------------
// Policy
// ---------------------------------------------------------------------------

/// Attenuation policy: consulted on every call the membrane guards.
///
/// `check` takes `&self` but implementations may hold interior-mutable state
/// (a revoked flag, a rate-limit counter) — the membrane calls `check` once
/// per invocation, so stateful policies observe every call. `Err` denies the
/// call, fail-closed; prefer [`denied_error`] so the denial is routable.
pub trait Policy {
    fn check(&self, interface_id: u64, method_id: u16) -> Result<(), Error>;
}

/// Stateless `(interface_id, method_id)` allowlist. The first and simplest
/// [`Policy`]; unknown or unlisted methods fail closed.
#[derive(Default)]
pub struct Allowlist {
    allowed: HashSet<(u64, u16)>,
}

impl Allowlist {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn allow(mut self, interface_id: u64, method_id: u16) -> Self {
        self.allowed.insert((interface_id, method_id));
        self
    }
}

impl Policy for Allowlist {
    fn check(&self, interface_id: u64, method_id: u16) -> Result<(), Error> {
        if self.allowed.contains(&(interface_id, method_id)) {
            Ok(())
        } else {
            Err(denied_error(interface_id, method_id, "not on allowlist"))
        }
    }
}

/// Wraps another policy with a revoke switch. Dropping a granted capability's
/// authority without killing the holder is the classic membrane use; call
/// [`RevocablePolicy::revoke`] and every subsequent call fails closed.
///
/// Hold the `Rc<RevocablePolicy>` on the granter side and hand a
/// `Rc<dyn Policy>` clone to [`membrane`]; both point at the same revoke flag.
pub struct RevocablePolicy {
    inner: Box<dyn Policy>,
    revoked: Cell<bool>,
}

impl RevocablePolicy {
    pub fn new(inner: Box<dyn Policy>) -> Rc<Self> {
        Rc::new(Self {
            inner,
            revoked: Cell::new(false),
        })
    }

    pub fn revoke(&self) {
        self.revoked.set(true);
    }

    pub fn is_revoked(&self) -> bool {
        self.revoked.get()
    }
}

impl Policy for RevocablePolicy {
    fn check(&self, interface_id: u64, method_id: u16) -> Result<(), Error> {
        if self.revoked.get() {
            return Err(denied_error(interface_id, method_id, "capability revoked"));
        }
        self.inner.check(interface_id, method_id)
    }
}

/// Wraps another policy with a fixed-window rate limit. Rate-limit-as-
/// capability: the limit is intrinsic to the reference and travels with it
/// across boundaries, rather than being an endpoint policy check (roadmap
/// D29). Stateful — it counts calls, so it never collapses with another
/// membrane (roadmap D18); it stacks.
pub struct RateLimit {
    inner: Box<dyn Policy>,
    max_per_window: u32,
    window: Duration,
    state: RefCell<RateWindow>,
}

struct RateWindow {
    count: u32,
    started: Instant,
}

impl RateLimit {
    pub fn new(inner: Box<dyn Policy>, max_per_window: u32, window: Duration) -> Self {
        Self {
            inner,
            max_per_window,
            window,
            state: RefCell::new(RateWindow {
                count: 0,
                started: Instant::now(),
            }),
        }
    }
}

impl Policy for RateLimit {
    fn check(&self, interface_id: u64, method_id: u16) -> Result<(), Error> {
        // Composition: the call must also satisfy the wrapped policy.
        self.inner.check(interface_id, method_id)?;

        let mut w = self.state.borrow_mut();
        if w.started.elapsed() >= self.window {
            w.count = 0;
            w.started = Instant::now();
        }
        if w.count >= self.max_per_window {
            return Err(denied_error(interface_id, method_id, "rate limit exceeded"));
        }
        w.count += 1;
        Ok(())
    }
}

/// Wrap an untyped client hook in a membrane governed by `policy`.
pub fn membrane_hook(inner: Box<dyn ClientHook>, policy: Rc<dyn Policy>) -> Box<dyn ClientHook> {
    MembraneHook::wrap(inner, policy)
}

/// Wrap a typed client in a membrane governed by `policy`.
pub fn membrane<C: capnp::capability::FromClientHook>(client: C, policy: Rc<dyn Policy>) -> C {
    C::new(membrane_hook(client.into_client_hook(), policy))
}

// ---------------------------------------------------------------------------
// MembraneHook: the ClientHook wrapper
// ---------------------------------------------------------------------------

struct MembraneState {
    inner: Box<dyn ClientHook>,
    policy: Rc<dyn Policy>,
}

// ---------------------------------------------------------------------------
// Membrane identity (for unwrap-on-reentry, double-wrap avoidance, collapse)
// ---------------------------------------------------------------------------
//
// A capability has no `Any`, so to recognize "this hook is one of my
// membranes" we use two coordinates:
//   * `get_brand()` returns MEMBRANE_BRAND — the address of a data-segment
//     `static`. capnp-rpc connection brands are `self as *const ConnectionState`
//     (heap) and local/broken hooks use 0, so this value can never collide with
//     a real connection brand and therefore never makes the RPC layer mistake a
//     membraned cap for one of its own (which would tunnel through the membrane).
//   * `get_ptr()` returns the per-membrane `Rc<MembraneState>` address, which a
//     process-local registry maps back to the live `MembraneState`.
//
// Together they let [`membrane_state_of`] downcast an arbitrary hook to its
// backing membrane without `Any`.

static MEMBRANE_BRAND_ANCHOR: u8 = 0;

fn membrane_brand() -> usize {
    &MEMBRANE_BRAND_ANCHOR as *const u8 as usize
}

thread_local! {
    static REGISTRY: RefCell<HashMap<usize, Weak<MembraneState>>> =
        RefCell::new(HashMap::new());
}

fn registry_insert(state: &Rc<MembraneState>) {
    let key = Rc::as_ptr(state) as *const () as usize;
    REGISTRY.with(|r| {
        r.borrow_mut().insert(key, Rc::downgrade(state));
    });
}

/// If `hook` is a live membrane of ours, return its backing state (inner cap +
/// policy). Returns `None` for any non-membrane hook.
// Consumed by the dual-membrane reentry/unwrap path (next M3 commit); exercised
// now by the identity tests.
#[allow(dead_code)]
fn membrane_state_of(hook: &dyn ClientHook) -> Option<Rc<MembraneState>> {
    if hook.get_brand() != membrane_brand() {
        return None;
    }
    let key = hook.get_ptr();
    REGISTRY.with(|r| r.borrow().get(&key).and_then(Weak::upgrade))
}

impl Drop for MembraneState {
    fn drop(&mut self) {
        // Deregister by our own address (the registry key is Rc::as_ptr, which
        // is the address of this MembraneState).
        let key = self as *const MembraneState as *const () as usize;
        REGISTRY.with(|r| {
            r.borrow_mut().remove(&key);
        });
    }
}

pub struct MembraneHook {
    state: Rc<MembraneState>,
}

impl MembraneHook {
    pub fn wrap(inner: Box<dyn ClientHook>, policy: Rc<dyn Policy>) -> Box<dyn ClientHook> {
        let state = Rc::new(MembraneState { inner, policy });
        registry_insert(&state);
        Box::new(Self { state })
    }
}

impl ClientHook for MembraneHook {
    fn add_ref(&self) -> Box<dyn ClientHook> {
        Box::new(Self {
            state: self.state.clone(),
        })
    }

    fn new_call(
        &self,
        interface_id: u64,
        method_id: u16,
        size_hint: Option<MessageSize>,
    ) -> Request<any_pointer::Owned, any_pointer::Owned> {
        if let Err(e) = self.state.policy.check(interface_id, method_id) {
            return Request::new(Box::new(BrokenRequest::new(e)));
        }
        let inner_request = self
            .state
            .inner
            .new_call(interface_id, method_id, size_hint);
        Request::new(Box::new(MembraneRequest {
            inner: inner_request.hook,
            policy: self.state.policy.clone(),
        }))
    }

    fn call(
        &self,
        interface_id: u64,
        method_id: u16,
        params: Box<dyn ParamsHook>,
        results: Box<dyn ResultsHook>,
    ) -> Promise<(), Error> {
        if let Err(e) = self.state.policy.check(interface_id, method_id) {
            return Promise::err(e);
        }
        // Interpose on results so that caps placed there by the inner object
        // get membraned before they reach the caller (e.g. the RPC answer).
        // The flush runs when the callee drops the results hook (capnp's own
        // "results done" signal); its outcome is carried out over `rx` so a
        // copy failure surfaces as the call's error instead of a silent empty
        // result (roadmap D7). Drop remains the completion signal, matching
        // capnp-rpc's local `Results`.
        let (tx, rx) = oneshot::channel();
        let wrapped_results =
            Box::new(MembraneResults::new(results, self.state.policy.clone(), tx));
        let inner = self
            .state
            .inner
            .call(interface_id, method_id, params, wrapped_results);
        Promise::from_future(async move {
            inner.await?;
            match rx.await {
                Ok(flush_result) => flush_result,
                // Results hook dropped without sending (e.g. cancellation
                // before any flush) — nothing was promised to the caller.
                Err(oneshot::Canceled) => Ok(()),
            }
        })
    }

    fn get_brand(&self) -> usize {
        // Membrane-unique sentinel (see MEMBRANE_BRAND_ANCHOR): lets us
        // recognize our own membranes for unwrap-on-reentry, and — being a
        // data-segment address — never collides with a capnp-rpc connection
        // brand (`self as *const ConnectionState`, always heap) or the 0 used
        // by local/broken hooks. It therefore does NOT let the RPC layer
        // mistake a membraned cap for one of its own and tunnel through the
        // membrane. Must NOT forward the inner brand.
        membrane_brand()
    }

    fn get_ptr(&self) -> usize {
        // MUST NOT forward the inner pointer: `get_ptr` keys export tables and
        // CapabilityServerSet lookups; forwarding would let the membraned cap
        // alias its unwrapped sibling.
        Rc::as_ptr(&self.state) as *const () as usize
    }

    fn get_resolved(&self) -> Option<Box<dyn ClientHook>> {
        self.state
            .inner
            .get_resolved()
            .map(|h| Self::wrap(h, self.state.policy.clone()))
    }

    fn when_more_resolved(&self) -> Option<Promise<Box<dyn ClientHook>, Error>> {
        let policy = self.state.policy.clone();
        self.state
            .inner
            .when_more_resolved()
            .map(|p| Promise::from_future(async move { Ok(Self::wrap(p.await?, policy)) }))
    }

    fn when_resolved(&self) -> Promise<(), Error> {
        self.state.inner.when_resolved()
    }
}

// ---------------------------------------------------------------------------
// MembraneRequest: wraps RequestHook so responses + pipelines are re-wrapped
// ---------------------------------------------------------------------------

struct MembraneRequest {
    inner: Box<dyn RequestHook>,
    policy: Rc<dyn Policy>,
}

impl RequestHook for MembraneRequest {
    fn get(&mut self) -> any_pointer::Builder<'_> {
        // NOTE: params pass into the membrane unwrapped (see module docs / M3).
        self.inner.get()
    }

    fn get_brand(&self) -> usize {
        0
    }

    fn send(self: Box<Self>) -> RemotePromise<any_pointer::Owned> {
        let Self { inner, policy } = *self;
        let RemotePromise { promise, pipeline } = inner.send();

        // Wrap the pipeline hook so promise-pipelined caps stay inside.
        let pipeline_policy = policy.clone();
        let wrapped_pipeline: Box<dyn PipelineHook> = Box::new(MembranePipeline {
            inner: pipeline.hook,
            policy: pipeline_policy,
        });

        // Wrap the response so caps in the results stay inside.
        let wrapped_promise = Promise::from_future(async move {
            let response = promise.await?;
            let membraned = MembraneResponse::rewrap(response.hook, policy)?;
            Ok(Response::new(Box::new(membraned)))
        });

        RemotePromise {
            promise: wrapped_promise,
            pipeline: any_pointer::Pipeline::new(wrapped_pipeline),
        }
    }

    fn send_streaming(self: Box<Self>) -> Promise<(), Error> {
        // Streaming methods return no caps; policy was checked in new_call().
        self.inner.send_streaming()
    }

    fn tail_send(self: Box<Self>) -> Option<(u32, Promise<(), Error>, Box<dyn PipelineHook>)> {
        // Tail calls would let results bypass the membrane; refuse.
        None
    }
}

// ---------------------------------------------------------------------------
// MembraneResponse: deep-copies the response and wraps its cap table
// ---------------------------------------------------------------------------

struct MembraneResponse {
    message: Builder<HeapAllocator>,
    cap_table: CapTable,
}

impl MembraneResponse {
    /// Copy the inner response into a fresh message whose cap table we own,
    /// then wrap every capability in that table.
    ///
    /// The deep copy is the price of doing this with public APIs only: the
    /// inner response's cap table is private to its hook, but `set_as` on an
    /// imbued `any_pointer::Builder` re-materializes the caps into our table.
    /// Cost O(response size); roadmap D13 tracks the benchmark + tripwire.
    fn rewrap(inner: Box<dyn ResponseHook>, policy: Rc<dyn Policy>) -> capnp::Result<Self> {
        let mut message = Builder::new_default();
        let mut cap_table = CapTable::new();
        {
            let mut root: any_pointer::Builder = message.init_root();
            root.imbue_mut(&mut cap_table);
            root.set_as(inner.get()?)?;
        }
        for slot in cap_table.iter_mut() {
            if let Some(hook) = slot.take() {
                *slot = Some(MembraneHook::wrap(hook, policy.clone()));
            }
        }
        Ok(Self { message, cap_table })
    }
}

impl ResponseHook for MembraneResponse {
    fn get(&self) -> capnp::Result<any_pointer::Reader<'_>> {
        let mut reader: any_pointer::Reader = self.message.get_root_as_reader()?;
        reader.imbue(&self.cap_table);
        Ok(reader)
    }
}

// ---------------------------------------------------------------------------
// MembranePipeline: wraps PipelineHook so pipelined caps are membraned
// ---------------------------------------------------------------------------

struct MembranePipeline {
    inner: Box<dyn PipelineHook>,
    policy: Rc<dyn Policy>,
}

impl PipelineHook for MembranePipeline {
    fn add_ref(&self) -> Box<dyn PipelineHook> {
        Box::new(Self {
            inner: self.inner.add_ref(),
            policy: self.policy.clone(),
        })
    }

    fn get_pipelined_cap(&self, ops: &[PipelineOp]) -> Box<dyn ClientHook> {
        MembraneHook::wrap(self.inner.get_pipelined_cap(ops), self.policy.clone())
    }
}

// ---------------------------------------------------------------------------
// MembraneResults: server-side (call() path) results interposition
// ---------------------------------------------------------------------------

/// Buffers results in our own message + cap table; when the callee finishes and
/// drops this hook (capnp's own "results done" signal), it wraps every cap and
/// copies the buffered payload into the real results hook, then reports the copy
/// outcome over `outcome` so `MembraneHook::call` can surface a copy failure as
/// the call's error rather than a silent empty result (roadmap D7).
struct MembraneResults {
    inner: Box<dyn ResultsHook>,
    message: Builder<HeapAllocator>,
    cap_table: CapTable,
    policy: Rc<dyn Policy>,
    outcome: Option<oneshot::Sender<capnp::Result<()>>>,
}

impl MembraneResults {
    fn new(
        inner: Box<dyn ResultsHook>,
        policy: Rc<dyn Policy>,
        outcome: oneshot::Sender<capnp::Result<()>>,
    ) -> Self {
        Self {
            inner,
            message: Builder::new_default(),
            cap_table: CapTable::new(),
            policy,
            outcome: Some(outcome),
        }
    }

    fn flush(&mut self) -> capnp::Result<()> {
        let mut reader: any_pointer::Reader = self.message.get_root_as_reader()?;
        // Imbue a membrane-wrapped view of our cap table for the copy, so the
        // real results hook captures wrapped caps.
        let wrapped: CapTable = self
            .cap_table
            .iter()
            .map(|slot| {
                slot.as_ref()
                    .map(|h| MembraneHook::wrap(h.add_ref(), self.policy.clone()))
            })
            .collect();
        reader.imbue(&wrapped);
        self.inner.get()?.set_as(reader)
    }
}

impl Drop for MembraneResults {
    fn drop(&mut self) {
        // Drop is capnp's "results done" signal. Flush here, then report the
        // outcome so a copy failure becomes the call's error (D7) rather than
        // a silently empty result. The receiver may already be gone (caller
        // cancelled) — send is best-effort.
        let res = self.flush();
        if let Some(tx) = self.outcome.take() {
            let _ = tx.send(res);
        }
    }
}

impl ResultsHook for MembraneResults {
    fn get(&mut self) -> capnp::Result<any_pointer::Builder<'_>> {
        let mut builder: any_pointer::Builder = self.message.get_root()?;
        builder.imbue_mut(&mut self.cap_table);
        Ok(builder)
    }

    fn set_pipeline(&mut self) -> capnp::Result<()> {
        self.flush()?;
        self.inner.set_pipeline()
    }

    fn allow_cancellation(&self) {
        self.inner.allow_cancellation()
    }

    fn tail_call(self: Box<Self>, _request: Box<dyn RequestHook>) -> Promise<(), Error> {
        Promise::err(Error::unimplemented(
            "membrane: tail_call not supported".into(),
        ))
    }

    fn direct_tail_call(
        self: Box<Self>,
        _request: Box<dyn RequestHook>,
    ) -> (Promise<(), Error>, Box<dyn PipelineHook>) {
        let e = Error::unimplemented("membrane: direct_tail_call not supported".into());
        (
            Promise::err(e.clone()),
            Box::new(BrokenPipeline { error: e }),
        )
    }
}

// ---------------------------------------------------------------------------
// Broken client/request/pipeline (capnp-rpc's `broken` module is pub(crate),
// so we carry our own minimal copies for the deny path).
//
// Provenance: mirrors capnp-rpc broken.rs @ 0.25.1. Upstream draft PR
// capnproto/capnproto-rust#671 exposes a public `new_broken_cap` constructor;
// once released, this block is deleted in favor of it (roadmap D8).
// ---------------------------------------------------------------------------

struct BrokenRequest {
    error: Error,
    message: Builder<HeapAllocator>,
    cap_table: CapTable,
}

impl BrokenRequest {
    fn new(error: Error) -> Self {
        Self {
            error,
            message: Builder::new_default(),
            cap_table: CapTable::new(),
        }
    }
}

impl RequestHook for BrokenRequest {
    fn get(&mut self) -> any_pointer::Builder<'_> {
        let mut result: any_pointer::Builder = self.message.get_root().unwrap();
        result.imbue_mut(&mut self.cap_table);
        result
    }
    fn get_brand(&self) -> usize {
        0
    }
    fn send(self: Box<Self>) -> RemotePromise<any_pointer::Owned> {
        RemotePromise {
            promise: Promise::err(self.error.clone()),
            pipeline: any_pointer::Pipeline::new(Box::new(BrokenPipeline { error: self.error })),
        }
    }
    fn send_streaming(self: Box<Self>) -> Promise<(), Error> {
        Promise::err(self.error)
    }
    fn tail_send(self: Box<Self>) -> Option<(u32, Promise<(), Error>, Box<dyn PipelineHook>)> {
        None
    }
}

struct BrokenPipeline {
    error: Error,
}

impl PipelineHook for BrokenPipeline {
    fn add_ref(&self) -> Box<dyn PipelineHook> {
        Box::new(Self {
            error: self.error.clone(),
        })
    }
    fn get_pipelined_cap(&self, _ops: &[PipelineOp]) -> Box<dyn ClientHook> {
        Box::new(BrokenClient {
            state: Rc::new(self.error.clone()),
        })
    }
}

struct BrokenClient {
    state: Rc<Error>,
}

impl ClientHook for BrokenClient {
    fn add_ref(&self) -> Box<dyn ClientHook> {
        Box::new(Self {
            state: self.state.clone(),
        })
    }
    fn new_call(
        &self,
        _interface_id: u64,
        _method_id: u16,
        _size_hint: Option<MessageSize>,
    ) -> Request<any_pointer::Owned, any_pointer::Owned> {
        Request::new(Box::new(BrokenRequest::new((*self.state).clone())))
    }
    fn call(
        &self,
        _interface_id: u64,
        _method_id: u16,
        _params: Box<dyn ParamsHook>,
        _results: Box<dyn ResultsHook>,
    ) -> Promise<(), Error> {
        Promise::err((*self.state).clone())
    }
    fn get_brand(&self) -> usize {
        0
    }
    fn get_ptr(&self) -> usize {
        Rc::as_ptr(&self.state) as *const () as usize
    }
    fn get_resolved(&self) -> Option<Box<dyn ClientHook>> {
        None
    }
    fn when_more_resolved(&self) -> Option<Promise<Box<dyn ClientHook>, Error>> {
        None
    }
    fn when_resolved(&self) -> Promise<(), Error> {
        Promise::err((*self.state).clone())
    }
}

// ---------------------------------------------------------------------------
// Policy unit tests (schema-free). Membrane integration tests (cast-bypass,
// pipelines, resolution) against a toy schema are ported separately as the
// next M1 commit; the real-cap end-to-end path is already proven by the M1a
// spike in crates/rpc.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    const IFACE: u64 = 0xdead_beef;

    #[test]
    fn allowlist_allows_listed_denies_rest() {
        let p = Allowlist::new().allow(IFACE, 0);
        assert!(p.check(IFACE, 0).is_ok());
        let denied = p.check(IFACE, 1).unwrap_err();
        assert!(denied.to_string().contains(DENIED_MARKER));
    }

    #[test]
    fn denial_error_roundtrips_method_key() {
        let e = denied_error(IFACE, 7, "nope");
        assert_eq!(denied_method_key(&e), Some((IFACE, 7)));
        // A non-denial error yields None.
        assert_eq!(denied_method_key(&Error::failed("unrelated".into())), None);
    }

    #[test]
    fn revocable_denies_after_revoke() {
        let base = Box::new(Allowlist::new().allow(IFACE, 0));
        let rev = RevocablePolicy::new(base);
        assert!(rev.check(IFACE, 0).is_ok());
        rev.revoke();
        let denied = rev.check(IFACE, 0).unwrap_err();
        assert!(denied.to_string().contains(DENIED_MARKER));
        assert!(rev.is_revoked());
    }

    #[test]
    fn rate_limit_denies_after_n_calls() {
        // Flagship "rate-limit-as-capability" demo, at the policy layer (D29).
        // A generous window so timing never flakes the count assertion.
        let base = Box::new(Allowlist::new().allow(IFACE, 0));
        let rl = RateLimit::new(base, 3, Duration::from_secs(3600));
        assert!(rl.check(IFACE, 0).is_ok()); // 1
        assert!(rl.check(IFACE, 0).is_ok()); // 2
        assert!(rl.check(IFACE, 0).is_ok()); // 3
        let denied = rl.check(IFACE, 0).unwrap_err(); // 4 -> denied
        assert!(denied.to_string().contains(DENIED_MARKER));
    }

    #[test]
    fn rate_limit_still_enforces_inner_policy() {
        let base = Box::new(Allowlist::new().allow(IFACE, 0));
        let rl = RateLimit::new(base, 100, Duration::from_secs(3600));
        // Method 1 is not on the inner allowlist -> denied regardless of rate.
        assert!(rl.check(IFACE, 1).is_err());
    }

    #[test]
    fn rate_limit_window_resets() {
        let base = Box::new(Allowlist::new().allow(IFACE, 0));
        let rl = RateLimit::new(base, 1, Duration::from_millis(20));
        assert!(rl.check(IFACE, 0).is_ok());
        assert!(rl.check(IFACE, 0).is_err());
        std::thread::sleep(Duration::from_millis(30));
        assert!(rl.check(IFACE, 0).is_ok(), "window should have reset");
    }

    // -----------------------------------------------------------------------
    // Membrane identity (M3 mini-spike): sentinel brand + registry
    // -----------------------------------------------------------------------

    fn dummy_hook() -> Box<dyn ClientHook> {
        // A non-membrane hook to wrap / to test negative recognition.
        Box::new(BrokenClient {
            state: Rc::new(Error::failed("dummy".into())),
        })
    }

    #[test]
    fn membrane_brand_is_nonzero_and_stable() {
        // Non-zero (0 is local/broken) and stable across calls.
        assert_ne!(membrane_brand(), 0);
        assert_eq!(membrane_brand(), membrane_brand());
    }

    #[test]
    fn membrane_recognizes_own_wrap_but_not_others() {
        let policy: Rc<dyn Policy> = Rc::new(Allowlist::new());
        let wrapped = MembraneHook::wrap(dummy_hook(), policy);

        // Our membrane advertises the sentinel brand and resolves via registry.
        assert_eq!(wrapped.get_brand(), membrane_brand());
        assert!(
            membrane_state_of(&*wrapped).is_some(),
            "a membrane must recognize its own wrap"
        );

        // A bare, non-membrane hook is not recognized.
        let bare = dummy_hook();
        assert!(
            membrane_state_of(&*bare).is_none(),
            "a non-membrane hook must not resolve to a membrane"
        );
    }

    #[test]
    fn membrane_deregisters_on_drop() {
        let policy: Rc<dyn Policy> = Rc::new(Allowlist::new());
        let wrapped = MembraneHook::wrap(dummy_hook(), policy);
        let key = wrapped.get_ptr();
        assert!(REGISTRY.with(|r| r.borrow().contains_key(&key)));
        drop(wrapped);
        assert!(
            REGISTRY.with(|r| !r.borrow().contains_key(&key)),
            "dropping the last membrane ref must deregister it"
        );
    }

    /// D7: a results-copy failure must be reported as the call's error, not
    /// silently swallowed. A `ResultsHook` whose `get()` fails makes the flush
    /// fail; the outcome channel must carry that error.
    #[test]
    fn flush_failure_is_reported_not_swallowed() {
        struct FailingResults;
        impl ResultsHook for FailingResults {
            fn get(&mut self) -> capnp::Result<any_pointer::Builder<'_>> {
                Err(Error::failed("injected results failure".into()))
            }
            fn set_pipeline(&mut self) -> capnp::Result<()> {
                Ok(())
            }
            fn allow_cancellation(&self) {}
            fn tail_call(self: Box<Self>, _r: Box<dyn RequestHook>) -> Promise<(), Error> {
                Promise::err(Error::unimplemented("unused".into()))
            }
            fn direct_tail_call(
                self: Box<Self>,
                _r: Box<dyn RequestHook>,
            ) -> (Promise<(), Error>, Box<dyn PipelineHook>) {
                let e = Error::unimplemented("unused".into());
                (
                    Promise::err(e.clone()),
                    Box::new(BrokenPipeline { error: e }),
                )
            }
        }

        let (tx, mut rx) = oneshot::channel();
        let policy: Rc<dyn Policy> = Rc::new(Allowlist::new());
        let mr = MembraneResults::new(Box::new(FailingResults), policy, tx);
        drop(mr); // callee-done signal → flush runs → inner.get() fails

        match rx.try_recv() {
            Ok(Some(Err(e))) => {
                assert!(e.to_string().contains("injected results failure"));
            }
            other => panic!("expected the flush error to be reported, got {other:?}"),
        }
    }
}
