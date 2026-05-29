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

/// Shared route registry: path prefix → request channel sender.
pub type RouteRegistry = Arc<RwLock<HashMap<String, RequestSender>>>;

/// Create a new empty route registry.
pub fn new_registry() -> RouteRegistry {
    Arc::new(RwLock::new(HashMap::new()))
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
