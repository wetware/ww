//! WagiService: axum HTTP server on a dedicated OS thread.
//!
//! Implements the `Service` trait from `runtime.rs`. Accepts route
//! registrations from `HttpListenerImpl` via a shared registry, then
//! dispatches incoming HTTP requests to WASM cells using the CGI adapter
//! (`dispatcher::wagi`).
//!
//! Architecture: Cap'n Proto clients are `!Send`, so the axum handler
//! can't hold a `Executor` directly. Instead, each route registers
//! a `RequestSender` (an mpsc channel). The axum handler sends requests
//! through the channel, and a local task on the RPC event loop receives
//! them, spawns cells via `Executor`, and sends responses back.

use std::collections::HashMap;

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::response::Response;
use axum::routing::any;
use axum::Router;
use tokio::sync::{oneshot, watch};

pub use rpc::dispatch::{new_registry, CgiRequest, CgiResponse, RequestSender, RouteRegistry};

/// The axum HTTP server running on its own OS thread.
///
/// ```text
/// Host supervisor
///  ├── Thread: SwarmService
///  ├── Thread: EpochService
///  ├── Thread: WagiService  ← this
///  └── Threads: ExecutorPool
/// ```
pub struct WagiService {
    pub listen_addr: std::net::SocketAddr,
    pub registry: RouteRegistry,
}

impl crate::services::Service for WagiService {
    fn run(self, mut shutdown: watch::Receiver<()>) -> anyhow::Result<()> {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        let _span = tracing::info_span!("wagi-http").entered();

        rt.block_on(async move {
            let app = Router::new()
                .route("/{*path}", any(handle_request))
                .route("/", any(handle_request))
                .with_state(self.registry);

            let listener = tokio::net::TcpListener::bind(self.listen_addr).await?;
            let local_addr = listener.local_addr()?;
            tracing::info!(%local_addr, "WAGI HTTP server listening");

            axum::serve(listener, app)
                .with_graceful_shutdown(async move {
                    let _ = shutdown.changed().await;
                    tracing::info!("WAGI HTTP server shutting down");
                })
                .await?;

            Ok(())
        })
    }
}

/// Maximum request body size (16 MiB).
const MAX_REQUEST_BYTES: usize = 16 * 1024 * 1024;

/// Axum handler: match path prefix, send request through channel, await response.
async fn handle_request(State(registry): State<RouteRegistry>, request: Request<Body>) -> Response {
    let path = request.uri().path().to_string();
    let query = request.uri().query().unwrap_or("").to_string();
    let method = request.method().to_string();

    // Find the longest matching prefix in the registry.
    let sender = {
        let routes = match registry.read() {
            Ok(r) => r,
            Err(_) => {
                return error_response(StatusCode::INTERNAL_SERVER_ERROR, "route lock poisoned")
            }
        };
        find_longest_prefix(&routes, &path)
    };

    let sender = match sender {
        Some(s) => s,
        None => return error_response(StatusCode::NOT_FOUND, &format!("no handler for {path}")),
    };

    // Extract headers before consuming the body.
    let headers: Vec<(String, String)> = request
        .headers()
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or("").to_string()))
        .collect();

    // Read the request body.
    let body_bytes = match axum::body::to_bytes(request.into_body(), MAX_REQUEST_BYTES).await {
        Ok(b) => b.to_vec(),
        Err(_) => return error_response(StatusCode::PAYLOAD_TOO_LARGE, "request body too large"),
    };

    // Send request to the RPC event loop and await response.
    let (response_tx, response_rx) = oneshot::channel();
    let cgi_req = CgiRequest {
        method,
        path,
        query,
        headers,
        body: body_bytes,
        response_tx,
    };

    if sender.send(cgi_req).await.is_err() {
        return error_response(StatusCode::SERVICE_UNAVAILABLE, "route handler closed");
    }

    match response_rx.await {
        Ok(resp) => build_http_response(&resp),
        Err(_) => error_response(StatusCode::BAD_GATEWAY, "cell handler dropped response"),
    }
}

/// Find the longest prefix match in the route table.
fn find_longest_prefix(
    routes: &HashMap<String, RequestSender>,
    path: &str,
) -> Option<RequestSender> {
    let mut best: Option<(&str, &RequestSender)> = None;
    for (prefix, sender) in routes {
        if path.starts_with(prefix.as_str()) {
            match best {
                Some((current_best, _)) if prefix.len() <= current_best.len() => {}
                _ => best = Some((prefix.as_str(), sender)),
            }
        }
    }
    best.map(|(_, sender)| sender.clone())
}

/// Build an axum Response from a CgiResponse.
fn build_http_response(cgi: &CgiResponse) -> Response {
    let mut builder = Response::builder().status(cgi.status);
    for (key, value) in &cgi.headers {
        builder = builder.header(key.as_str(), value.as_str());
    }
    builder
        .body(Body::from(cgi.body.clone()))
        .unwrap_or_else(|_| {
            error_response(StatusCode::INTERNAL_SERVER_ERROR, "response build error")
        })
}

/// Build an error response.
fn error_response(status: StatusCode, msg: &str) -> Response {
    Response::builder()
        .status(status)
        .header("content-type", "text/plain")
        .body(Body::from(msg.to_string()))
        .unwrap()
}

pub use rpc::dispatch::extract_server_info;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn find_longest_prefix_empty_returns_none() {
        let routes = HashMap::new();
        assert!(find_longest_prefix(&routes, "/api/v1/prices").is_none());
    }

    #[test]
    fn extract_server_info_parses_host_header() {
        let headers = vec![("Host".to_string(), "example.com:8080".to_string())];
        let (name, port) = extract_server_info(&headers);
        assert_eq!(name, "example.com");
        assert_eq!(port, 8080);
    }

    #[test]
    fn extract_server_info_default_port() {
        let headers = vec![("host".to_string(), "example.com".to_string())];
        let (name, port) = extract_server_info(&headers);
        assert_eq!(name, "example.com");
        assert_eq!(port, 80);
    }

    #[test]
    fn extract_server_info_no_host() {
        let headers: Vec<(String, String)> = vec![];
        let (name, port) = extract_server_info(&headers);
        assert_eq!(name, "localhost");
        assert_eq!(port, 80);
    }

    #[test]
    fn new_registry_is_empty() {
        let reg = new_registry();
        assert!(reg.read().unwrap().is_empty());
    }
}
