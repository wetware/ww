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
use crate::{membrane, Allowlist, Policy, DENIED_MARKER};

const PING: u16 = 0;
const FORBIDDEN: u16 = 1;
const CHILD: u16 = 2;

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
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn policy() -> Rc<dyn Policy> {
    // ping + child allowed; forbidden deliberately absent.
    Rc::new(
        Allowlist::new()
            .allow(thing_id(), PING)
            .allow(thing_id(), CHILD),
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
