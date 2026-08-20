//! Epoch-scoped capability primitives over Cap'n Proto RPC.
//!
//! - **Epoch** -- a monotonic sequence number anchored to on-chain state
//! - **EpochGuard** -- checks whether a capability's epoch is still current
//! - **MembraneServer** -- server that issues epoch-scoped sessions via `graft()`
//! - **SessionBuilder** -- trait for injecting domain-specific capabilities into sessions

#[allow(unused_parens, clippy::match_single_binding)]
pub mod system_capnp {
    include!(concat!(env!("OUT_DIR"), "/capnp/system_capnp.rs"));
}

#[allow(unused_parens, clippy::match_single_binding)]
pub mod routing_capnp {
    include!(concat!(env!("OUT_DIR"), "/capnp/routing_capnp.rs"));
}

#[allow(
    unused_parens,
    clippy::extra_unused_type_parameters,
    clippy::match_single_binding
)]
pub mod stem_capnp {
    include!(concat!(env!("OUT_DIR"), "/capnp/stem_capnp.rs"));
}

#[allow(
    unused_parens,
    clippy::extra_unused_type_parameters,
    clippy::match_single_binding
)]
pub mod auth_capnp {
    include!(concat!(env!("OUT_DIR"), "/capnp/auth_capnp.rs"));
}

#[allow(
    unused_parens,
    clippy::extra_unused_type_parameters,
    clippy::match_single_binding
)]
pub mod membrane_capnp {
    include!(concat!(env!("OUT_DIR"), "/capnp/membrane_capnp.rs"));
}

#[allow(unused_parens, clippy::match_single_binding)]
pub mod http_capnp {
    include!(concat!(env!("OUT_DIR"), "/capnp/http_capnp.rs"));
}

/// Implement Cap'n Proto promise-pipeline conversion for a generated capability client.
///
/// `capnpc` 0.25 declares capability interfaces as pipelined but does not emit the
/// corresponding `FromTypelessPipeline` implementation for their generated clients.
/// Terminal session interfaces need that implementation for
/// `login_request().send().pipeline.get_session()` to be callable. Invoke this macro in
/// the crate that owns a generated session client until the upstream generator emits it.
#[macro_export]
macro_rules! impl_terminal_session_pipeline {
    ($client:path) => {
        impl ::capnp::capability::FromTypelessPipeline for $client {
            fn new(typeless: ::capnp::any_pointer::Pipeline) -> Self {
                <Self as ::capnp::capability::FromClientHook>::new(typeless.as_cap())
            }
        }
    };
}

impl_terminal_session_pipeline!(membrane_capnp::membrane::Client);
impl_terminal_session_pipeline!(auth_capnp::opaque_session::Client);

#[cfg(test)]
#[allow(clippy::all, dead_code, unreachable_pub)]
mod test_session_capnp {
    include!(concat!(
        env!("OUT_DIR"),
        "/crates/authority/test_session_capnp.rs"
    ));
}

#[cfg(test)]
impl_terminal_session_pipeline!(test_session_capnp::structured_session::Client);

#[cfg(test)]
mod wire_type_id_tests {
    use capnp::traits::HasTypeId;

    #[test]
    fn split_schema_type_ids_are_pinned_for_wire_compat() {
        assert_eq!(
            <crate::auth_capnp::signer::Client as HasTypeId>::TYPE_ID,
            0xafaf_af94_68b6_a274
        );
        assert_eq!(
            <crate::auth_capnp::identity::Client as HasTypeId>::TYPE_ID,
            0xa7c2_00e5_b472_6d89
        );
        assert_eq!(
            <crate::auth_capnp::terminal::Client<capnp::any_pointer::Owned> as HasTypeId>::TYPE_ID,
            0xeae8_840b_2a89_8ba9
        );
        assert_eq!(
            <crate::auth_capnp::opaque_session::Client as HasTypeId>::TYPE_ID,
            0xc11f_8355_d7fc_e6bb
        );
        assert_eq!(
            <crate::auth_capnp::authority::Client as HasTypeId>::TYPE_ID,
            0xd119_09df_3e52_3d41
        );
        assert_eq!(
            <crate::membrane_capnp::export::Reader<'static> as HasTypeId>::TYPE_ID,
            0xbb8d_5590_cb2f_3d2e
        );
        assert_eq!(
            <crate::membrane_capnp::membrane::Client as HasTypeId>::TYPE_ID,
            0xdb52_c251_06bc_2c5e
        );
    }
}

pub mod epoch;
pub mod issuer;
mod kernel_ready;
pub mod membrane;
pub mod terminal;

pub use call_guard::{call_failure_code, stale_epoch_error, CallFailureCode};
pub use epoch::{Epoch, EpochGuard};
pub use issuer::{AuthorityServer, KeyMethodAuthorization, PolicyCompileError};
pub use kernel_ready::{KernelReadyError, KernelReadyGate};
pub use membrane::{get_graft_cap, membrane_client, GraftBuilder, MembraneServer, NoExtension};
pub use terminal::{
    AllowAllPolicy, AuthPolicy, AuthenticatedIdentity, AuthorizationError, FixedSessionPolicy,
    LocalPolicyFuture, SessionGrant, SessionTemplate, TerminalServer, DEFAULT_POLICY_TIMEOUT,
};
