//! Membrane integration tests against a toy capnp interface.
//!
//! In-crate (not `tests/`) because the generated interface's `TYPE_ID` and the
//! test schema module are crate-private. Ported from the approved feasibility
//! spike; adapted to the `Policy` trait API. The real-cap end-to-end path is
//! additionally covered by the M1a spike in `crates/rpc`.

use std::future::Future;
use std::rc::Rc;

use capnp::any_pointer;
use capnp::capability::{FromClientHook, Rc as CapRc};
use capnp::traits::HasTypeId;
use capnp::Error;

use capnp_rpc::rpc_twoparty_capnp::Side;
use capnp_rpc::twoparty::VatNetwork;
use capnp_rpc::RpcSystem;
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

use crate::test_thing_capnp::thing;
use crate::{attenuate, membrane, membrane_state_of, Allowlist, Policy, DENIED_MARKER};

const PING: u16 = 0;
const FORBIDDEN: u16 = 1;
const CHILD: u16 = 2;
const ECHO: u16 = 3;

fn thing_id() -> u64 {
    thing::Client::TYPE_ID
}

// ---------------------------------------------------------------------------
// Test server
// ---------------------------------------------------------------------------

struct ThingImpl {
    depth: u32,
}

impl thing::Server for ThingImpl {
    fn ping(
        self: CapRc<Self>,
        _params: thing::PingParams,
        mut results: thing::PingResults,
    ) -> impl Future<Output = Result<(), Error>> + 'static {
        results.get().set_msg(format!("pong-{}", self.depth));
        std::future::ready(Ok(()))
    }

    fn forbidden(
        self: CapRc<Self>,
        _params: thing::ForbiddenParams,
        mut results: thing::ForbiddenResults,
    ) -> impl Future<Output = Result<(), Error>> + 'static {
        results.get().set_msg("TOP SECRET");
        std::future::ready(Ok(()))
    }

    fn child(
        self: CapRc<Self>,
        _params: thing::ChildParams,
        mut results: thing::ChildResults,
    ) -> impl Future<Output = Result<(), Error>> + 'static {
        results.get().set_thing(capnp_rpc::new_client(ThingImpl {
            depth: self.depth + 1,
        }));
        std::future::ready(Ok(()))
    }

    /// Calls `forbidden` on the capability handed in as a parameter and echoes
    /// the result. The backend sees whatever cap actually arrives: if a
    /// membrane of ours was unwrapped on reentry, this call reaches a bare cap
    /// and succeeds; if not, it hits the membrane and fails closed.
    async fn echo(
        self: CapRc<Self>,
        params: thing::EchoParams,
        mut results: thing::EchoResults,
    ) -> Result<(), Error> {
        let arg: thing::Client = params.get()?.get_thing()?;
        let response = arg.forbidden_request().send().promise.await?;
        let msg = response.get()?.get_msg()?.to_string()?;
        results.get().set_msg(&msg);
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn policy() -> Rc<dyn Policy> {
    // ping + child + echo allowed; forbidden deliberately absent.
    Rc::new(
        Allowlist::new()
            .allow(thing_id(), PING)
            .allow(thing_id(), CHILD)
            .allow(thing_id(), ECHO),
    )
}

fn membraned_local_thing() -> thing::Client {
    let raw: thing::Client = capnp_rpc::new_client(ThingImpl { depth: 0 });
    membrane(raw, policy())
}

fn assert_denied<T>(result: Result<T, Error>, context: &str) {
    match result {
        Ok(_) => panic!("{context}: expected membrane denial, but call succeeded"),
        Err(e) => assert!(
            e.to_string().contains(DENIED_MARKER),
            "{context}: expected membrane denial, got unrelated error: {e}"
        ),
    }
}

async fn expect_ping(client: &thing::Client, expected: &str, context: &str) {
    let response = client
        .ping_request()
        .send()
        .promise
        .await
        .unwrap_or_else(|e| panic!("{context}: ping failed: {e}"));
    let msg = response.get().unwrap().get_msg().unwrap();
    assert_eq!(msg.to_str().unwrap(), expected, "{context}");
}

// ---------------------------------------------------------------------------
// Stage 1: allow / deny / cast-bypass, in process
// ---------------------------------------------------------------------------

#[tokio::test]
async fn allowed_method_succeeds_through_membrane() {
    let m = membraned_local_thing();
    expect_ping(&m, "pong-0", "in-process allowed").await;
}

#[tokio::test]
async fn denied_method_fails_closed() {
    let m = membraned_local_thing();
    let r = m.forbidden_request().send().promise.await;
    assert_denied(r, "in-process denied");
}

/// Negative control: with `forbidden` allowed, the same path succeeds —
/// proving denials elsewhere come from the policy check, not broken forwarding.
#[tokio::test]
async fn permissive_policy_forwards_forbidden() {
    let raw: thing::Client = capnp_rpc::new_client(ThingImpl { depth: 0 });
    let m = membrane(
        raw,
        Rc::new(Allowlist::new().allow(thing_id(), FORBIDDEN)) as Rc<dyn Policy>,
    );
    let response = m.forbidden_request().send().promise.await.unwrap();
    assert_eq!(
        response.get().unwrap().get_msg().unwrap().to_str().unwrap(),
        "TOP SECRET"
    );
}

/// THE CAST-BYPASS TEST: building typed clients directly from the membrane's
/// hook (three ways) must not evade the filter, because filtering happens in
/// the hook every typed client shares.
#[tokio::test]
async fn cast_bypass_still_denied() {
    let m = membraned_local_thing();

    // Route A: rip the hook out and build a fresh typed client from it.
    let recast: thing::Client = FromClientHook::new(m.client.clone().hook);
    assert_denied(
        recast.forbidden_request().send().promise.await,
        "cast-bypass route A (FromClientHook::new)",
    );
    expect_ping(&recast, "pong-0", "cast-bypass route A ping").await;

    // Route B: fully untyped new_call with the real ids by hand.
    let untyped = capnp::capability::Client::new(m.client.clone().hook);
    let req =
        untyped.new_call::<any_pointer::Owned, any_pointer::Owned>(thing_id(), FORBIDDEN, None);
    assert_denied(
        req.send().promise.await,
        "cast-bypass route B (untyped new_call)",
    );

    // Route C: cast_to round trip.
    let cast: thing::Client = m.clone().cast_to::<thing::Client>();
    assert_denied(
        cast.forbidden_request().send().promise.await,
        "cast-bypass route C (cast_to)",
    );
}

// ---------------------------------------------------------------------------
// Stage 2: recursive membrane preservation, in process
// ---------------------------------------------------------------------------

#[tokio::test]
async fn returned_capability_is_rewrapped() {
    let m = membraned_local_thing();
    let response = m.child_request().send().promise.await.unwrap();
    let child: thing::Client = response.get().unwrap().get_thing().unwrap();

    expect_ping(&child, "pong-1", "child allowed").await;
    assert_denied(
        child.forbidden_request().send().promise.await,
        "child denied",
    );

    // Depth 2: grandchild is membraned too.
    let response2 = child.child_request().send().promise.await.unwrap();
    let grandchild: thing::Client = response2.get().unwrap().get_thing().unwrap();
    expect_ping(&grandchild, "pong-2", "grandchild allowed").await;
    assert_denied(
        grandchild.forbidden_request().send().promise.await,
        "grandchild denied",
    );
}

#[tokio::test]
async fn pipelined_capability_is_rewrapped() {
    let m = membraned_local_thing();
    let rp = m.child_request().send();
    let pipelined: thing::Client = rp.pipeline.get_thing();

    // Pipelined calls issued before the original resolves.
    let denied = pipelined.forbidden_request().send().promise;
    let allowed = pipelined.ping_request().send().promise;

    let (original, denied, allowed) = futures::join!(rp.promise, denied, allowed);
    original.unwrap();
    assert_denied(denied, "pipelined denied");
    assert_eq!(
        allowed
            .unwrap()
            .get()
            .unwrap()
            .get_msg()
            .unwrap()
            .to_str()
            .unwrap(),
        "pong-1",
        "pipelined allowed"
    );
}

// ---------------------------------------------------------------------------
// Stage 3: dual-membrane reentry (params)
// ---------------------------------------------------------------------------

/// A membraned cap of ours, handed back in as a call parameter, must be
/// unwrapped before it reaches the backend — restoring the bare cap the
/// backend originally exported. Proof: `echo` calls `forbidden` on the param.
/// `forbidden` is denied *through the membrane*, so if the backend received the
/// still-membraned cap the call would fail closed. It succeeds with the
/// backend's real answer, which is only possible if reentry unwrapped it.
#[tokio::test]
async fn membraned_param_is_unwrapped_on_reentry() {
    let m = membraned_local_thing();

    // A membraned child of ours (round-trips through the response rewrap path).
    let response = m.child_request().send().promise.await.unwrap();
    let child: thing::Client = response.get().unwrap().get_thing().unwrap();
    // Sanity: the child really is membraned (forbidden denied when called direct).
    assert_denied(
        child.forbidden_request().send().promise.await,
        "reentry precondition: child forbidden denied directly",
    );

    // Hand the membraned child back in as a parameter.
    let mut req = m.echo_request();
    req.get().set_thing(child);
    let echoed = req.send().promise.await.unwrap();
    assert_eq!(
        echoed.get().unwrap().get_msg().unwrap().to_str().unwrap(),
        "TOP SECRET",
        "backend must see the unwrapped bare cap, so its forbidden() succeeds",
    );
}

/// A capability protected by boundary A must remain protected when passed as a
/// parameter through independent boundary B. The process-global registry must
/// not authorize B to strip A's membrane.
#[tokio::test]
async fn foreign_boundary_param_remains_membraned() {
    let boundary_a = membraned_local_thing();

    let raw_b: thing::Client = capnp_rpc::new_client(ThingImpl { depth: 100 });
    let boundary_b = membrane(
        raw_b,
        Rc::new(Allowlist::new().allow(thing_id(), ECHO)) as Rc<dyn Policy>,
    );

    let mut request = boundary_b.echo_request();
    request.get().set_thing(boundary_a);
    assert_denied(
        request.send().promise.await,
        "foreign boundary must not unwrap another boundary's capability",
    );
}

// ---------------------------------------------------------------------------
// Stage 4: collapse-on-wrap (allowlist intersection, single layer)
// ---------------------------------------------------------------------------

/// Attenuating an already-membraned cap with two static allowlists must fold
/// to ONE membrane whose allowlist is the intersection: methods only the inner
/// or only the outer allowed are both denied, and the collapsed hook wraps the
/// bare cap directly (no nested membrane).
#[tokio::test]
async fn allowlist_collapse_intersects_and_flattens() {
    let raw: thing::Client = capnp_rpc::new_client(ThingImpl { depth: 0 });
    // inner allows {ping, child}; outer allows {child, forbidden}.
    let inner = membrane(
        raw,
        Rc::new(
            Allowlist::new()
                .allow(thing_id(), PING)
                .allow(thing_id(), CHILD),
        ) as Rc<dyn Policy>,
    );
    let outer = attenuate(
        inner,
        Rc::new(
            Allowlist::new()
                .allow(thing_id(), CHILD)
                .allow(thing_id(), FORBIDDEN),
        ) as Rc<dyn Policy>,
    );

    // Intersection is {child}: allowed.
    outer.child_request().send().promise.await.unwrap();
    // ping was only in inner -> denied.
    assert_denied(
        outer.ping_request().send().promise.await,
        "collapse: ping denied (not in outer allowlist)",
    );
    // forbidden was only in outer -> denied.
    assert_denied(
        outer.forbidden_request().send().promise.await,
        "collapse: forbidden denied (not in inner allowlist)",
    );

    // Single layer: the collapsed membrane wraps the bare cap, not a membrane.
    let hook = outer.client.clone().hook;
    let state = membrane_state_of(&*hook).expect("outer is a membrane");
    assert!(
        membrane_state_of(&*state.inner).is_none(),
        "collapse: inner must be the bare cap, not a nested membrane",
    );
}

#[tokio::test]
async fn independent_boundary_does_not_collapse_foreign_lineage() {
    let raw: thing::Client = capnp_rpc::new_client(ThingImpl { depth: 0 });
    let boundary_a = membrane(
        raw,
        Rc::new(
            Allowlist::new()
                .allow(thing_id(), PING)
                .allow(thing_id(), CHILD),
        ) as Rc<dyn Policy>,
    );
    let boundary_b = membrane(
        boundary_a,
        Rc::new(
            Allowlist::new()
                .allow(thing_id(), CHILD)
                .allow(thing_id(), FORBIDDEN),
        ) as Rc<dyn Policy>,
    );

    let state_b = membrane_state_of(&*boundary_b.client.clone().hook).expect("boundary B membrane");
    let state_a = membrane_state_of(&*state_b.inner).expect("boundary A remains nested");
    assert!(
        !Rc::ptr_eq(&state_a.lineage, &state_b.lineage),
        "independent membranes must receive distinct boundary lineages"
    );

    boundary_b.child_request().send().promise.await.unwrap();
    assert_denied(
        boundary_b.ping_request().send().promise.await,
        "boundary B still enforces its own policy",
    );
    assert_denied(
        boundary_b.forbidden_request().send().promise.await,
        "boundary A remains enforced under boundary B",
    );
}

/// A stateful outer policy (rate limit) must NOT collapse: it stacks so its
/// per-call counter survives. The inner allowlist still filters underneath.
#[tokio::test]
async fn stateful_policy_stacks_not_collapses() {
    use crate::RateLimit;
    use std::time::Duration;

    let raw: thing::Client = capnp_rpc::new_client(ThingImpl { depth: 0 });
    let inner = membrane(
        raw,
        Rc::new(Allowlist::new().allow(thing_id(), PING)) as Rc<dyn Policy>,
    );
    // Rate-limit the already-membraned cap: 1 call per long window.
    let outer = attenuate(
        inner,
        Rc::new(RateLimit::new(
            Box::new(Allowlist::new().allow(thing_id(), PING)),
            1,
            Duration::from_secs(3600),
        )) as Rc<dyn Policy>,
    );

    // Stacked, not collapsed: outer wraps a membrane, not the bare cap.
    let hook = outer.client.clone().hook;
    let state = membrane_state_of(&*hook).expect("outer is a membrane");
    assert!(
        membrane_state_of(&*state.inner).is_some(),
        "stateful policy must stack: inner should still be a membrane",
    );

    // First ping passes the rate limit; second is denied by the counter.
    expect_ping(&outer, "pong-0", "rate-limit first call").await;
    assert_denied(
        outer.ping_request().send().promise.await,
        "rate-limit second call denied",
    );
}

// ---------------------------------------------------------------------------
// Across a real capnp-rpc twoparty connection
// ---------------------------------------------------------------------------

fn setup_rpc(bootstrap: thing::Client) -> thing::Client {
    let (client_stream, server_stream) = tokio::io::duplex(8 * 1024);

    let (sr, sw) = tokio::io::split(server_stream);
    let server_network = VatNetwork::new(
        sr.compat(),
        sw.compat_write(),
        Side::Server,
        Default::default(),
    );
    let server_rpc = RpcSystem::new(Box::new(server_network), Some(bootstrap.client));
    tokio::task::spawn_local(async move {
        let _ = server_rpc.await;
    });

    let (cr, cw) = tokio::io::split(client_stream);
    let client_network = VatNetwork::new(
        cr.compat(),
        cw.compat_write(),
        Side::Client,
        Default::default(),
    );
    let mut client_rpc = RpcSystem::new(Box::new(client_network), None);
    let remote: thing::Client = client_rpc.bootstrap(Side::Server);
    tokio::task::spawn_local(async move {
        let _ = client_rpc.await;
    });

    remote
}

/// Membrane on the CLIENT side, around an imported remote cap. Exercises
/// new_call interception + response rewrap + pipeline rewrap over the wire.
#[tokio::test]
async fn rpc_client_side_membrane() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let remote = setup_rpc(capnp_rpc::new_client(ThingImpl { depth: 0 }));
            let m = membrane(remote, policy());

            expect_ping(&m, "pong-0", "rpc client-side allowed").await;
            assert_denied(
                m.forbidden_request().send().promise.await,
                "rpc client-side denied",
            );

            let response = m.child_request().send().promise.await.unwrap();
            let child: thing::Client = response.get().unwrap().get_thing().unwrap();
            expect_ping(&child, "pong-1", "rpc client-side child allowed").await;
            assert_denied(
                child.forbidden_request().send().promise.await,
                "rpc client-side child denied",
            );

            let rp = m.child_request().send();
            let pipelined: thing::Client = rp.pipeline.get_thing();
            let denied = pipelined.forbidden_request().send().promise;
            let allowed = pipelined.ping_request().send().promise;
            let (original, denied, allowed) = futures::join!(rp.promise, denied, allowed);
            original.unwrap();
            assert_denied(denied, "rpc client-side pipelined denied");
            assert_eq!(
                allowed
                    .unwrap()
                    .get()
                    .unwrap()
                    .get_msg()
                    .unwrap()
                    .to_str()
                    .unwrap(),
                "pong-1"
            );
        })
        .await;
}

/// Membrane on the SERVER side, before export. Exercises the call()
/// interception path and MembraneResults rewrap: caps placed in results get
/// wrapped before entering the RPC answer.
#[tokio::test]
async fn rpc_server_side_membrane() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let raw: thing::Client = capnp_rpc::new_client(ThingImpl { depth: 0 });
            let bootstrap = membrane(raw, policy());
            let remote = setup_rpc(bootstrap);

            expect_ping(&remote, "pong-0", "rpc server-side allowed").await;
            assert_denied(
                remote.forbidden_request().send().promise.await,
                "rpc server-side denied",
            );

            let response = remote.child_request().send().promise.await.unwrap();
            let child: thing::Client = response.get().unwrap().get_thing().unwrap();
            expect_ping(&child, "pong-1", "rpc server-side child allowed").await;
            assert_denied(
                child.forbidden_request().send().promise.await,
                "rpc server-side child denied",
            );

            let rp = remote.child_request().send();
            let pipelined: thing::Client = rp.pipeline.get_thing();
            let denied = pipelined.forbidden_request().send().promise;
            let allowed = pipelined.ping_request().send().promise;
            let (original, denied, allowed) = futures::join!(rp.promise, denied, allowed);
            original.unwrap();
            assert_denied(denied, "rpc server-side pipelined denied");
            assert_eq!(
                allowed
                    .unwrap()
                    .get()
                    .unwrap()
                    .get_msg()
                    .unwrap()
                    .to_str()
                    .unwrap(),
                "pong-1"
            );
        })
        .await;
}

#[tokio::test]
async fn rpc_foreign_boundary_param_remains_membraned() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let remote_a = setup_rpc(capnp_rpc::new_client(ThingImpl { depth: 0 }));
            let boundary_a = membrane(remote_a, policy());

            let remote_b = setup_rpc(capnp_rpc::new_client(ThingImpl { depth: 100 }));
            let boundary_b = membrane(
                remote_b,
                Rc::new(Allowlist::new().allow(thing_id(), ECHO)) as Rc<dyn Policy>,
            );

            let mut request = boundary_b.echo_request();
            request.get().set_thing(boundary_a);
            assert_denied(
                request.send().promise.await,
                "RPC foreign boundary must remain guarded",
            );
        })
        .await;
}
