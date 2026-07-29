//! HTTP dispatch primitives shared between rpc (route registration via
//! `HttpListenerImpl`) and the bin's axum-driven `WagiService`.
//!
//! These are pure data types + a constructor — no axum, no Service trait. The
//! axum runner lives in the bin (`src/dispatcher/server.rs`) and consumes
//! these types.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use tokio::sync::{mpsc, oneshot};

/// An HTTP request to be dispatched to a WASM cell.
pub struct CgiRequest {
    pub method: String,
    pub path: String,
    pub query: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    pub response_tx: oneshot::Sender<CgiResponse>,
}

/// An HTTP response from a WASM cell.
pub struct CgiResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

/// Sender half of the request channel. Stored in the route registry.
/// `Send + Sync` because `mpsc::Sender` is `Send + Sync`.
pub type RequestSender = mpsc::Sender<CgiRequest>;

/// Process-unique identity for one route registration.
///
/// A path may be replaced while cleanup for its previous registration is
/// still pending. Cleanup therefore compares this identity before removing a
/// route instead of removing by path alone.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RegistrationId(u64);

impl RegistrationId {
    pub(crate) fn next() -> Self {
        use std::sync::atomic::{AtomicU64, Ordering};

        static NEXT_ID: AtomicU64 = AtomicU64::new(1);
        let id = NEXT_ID
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |id| id.checked_add(1))
            .expect("HTTP registration identity space exhausted");
        Self(id)
    }
}

/// One live route and the identity of the registration that owns it.
#[derive(Clone)]
pub struct RouteEntry {
    registration_id: RegistrationId,
    sender: RequestSender,
    epoch_guard: authority::EpochGuard,
    registration_scope: Option<tokio::sync::watch::Receiver<()>>,
}

impl RouteEntry {
    pub(crate) fn new(
        registration_id: RegistrationId,
        sender: RequestSender,
        epoch_guard: authority::EpochGuard,
        registration_scope: Option<tokio::sync::watch::Receiver<()>>,
    ) -> Self {
        Self {
            registration_id,
            sender,
            epoch_guard,
            registration_scope,
        }
    }

    pub fn registration_id(&self) -> RegistrationId {
        self.registration_id
    }

    pub fn sender(&self) -> RequestSender {
        self.sender.clone()
    }

    /// Whether this registration's issuing epoch is still current.
    pub fn is_live(&self) -> bool {
        self.epoch_guard.check().is_ok()
            && self
                .registration_scope
                .as_ref()
                .is_none_or(|scope| scope.has_changed().is_ok())
    }
}

/// Shared route registry: path prefix → owned route registration.
pub type RouteRegistry = Arc<RwLock<HashMap<String, RouteEntry>>>;

/// Create a new empty route registry.
pub fn new_registry() -> RouteRegistry {
    Arc::new(RwLock::new(HashMap::new()))
}

/// Count registrations whose issuing epoch is still current.
///
/// Readiness and dispatch both derive liveness from this same route-table
/// state; there is no separately maintained counter to reconcile.
pub fn live_route_count(registry: &RouteRegistry) -> Result<usize, &'static str> {
    let routes = registry
        .read()
        .map_err(|_| "route registry lock poisoned")?;
    Ok(routes.values().filter(|entry| entry.is_live()).count())
}

/// Extract server name and port from Host header.
pub fn extract_server_info(headers: &[(String, String)]) -> (String, u16) {
    for (name, value) in headers {
        if name.eq_ignore_ascii_case("host") {
            if let Some(colon) = value.rfind(':') {
                let host = &value[..colon];
                let port = value[colon + 1..].parse().unwrap_or(80);
                return (host.to_string(), port);
            }
            return (value.clone(), 80);
        }
    }
    ("localhost".to_string(), 80)
}
