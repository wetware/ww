//! `ww shell` CLI surface.
//!
//! The UDS admin path has been removed. Remote shell transport/auth
//! replacement is tracked separately.

use anyhow::{bail, Result};
use libp2p::Multiaddr;

/// Run the interactive shell client.
///
/// `addr` and `discover` are the forward-stable CLI surface for remote
/// shell access (libp2p multiaddr / mDNS LAN browse).
pub async fn run_shell(addr: Option<Multiaddr>, discover: bool) -> Result<()> {
    let hint = if addr.is_some() || discover {
        "remote shell is not implemented yet"
    } else {
        "local shell is temporarily unavailable while transport/auth is being reworked"
    };
    bail!("ww shell: NOT IMPLEMENTED ({hint})")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn shell_without_args_reports_local_unavailable() {
        let err = run_shell(None, false).await.unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("local shell is temporarily unavailable"),
            "unexpected error: {msg}"
        );
    }

    #[tokio::test]
    async fn shell_with_addr_reports_remote_unimplemented() {
        let addr: Multiaddr = "/ip4/127.0.0.1/tcp/2025".parse().unwrap();
        let err = run_shell(Some(addr), false).await.unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("remote shell is not implemented yet"),
            "unexpected error: {msg}"
        );
    }
}
