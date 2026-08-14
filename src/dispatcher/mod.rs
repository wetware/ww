//! Generic request dispatcher for wetware.
//!
//! Handles routing external requests to WASI cell processes.
//! The dispatcher (`HttpServer`) is generic over a `ProtocolAdapter`.
//!
//! Two execution modes:
//! - **Per-request spawn** (Mode A): each request spawns a fresh WASI process.
//!   Simple, boring, ~1-5ms overhead. Good for library consumers.
//! - **Long-lived worker** (Mode B): process stays alive, stdin/stdout swapped
//!   between requests via SwappableReader/Writer. ~1.6us overhead.
//!
//! ```text
//!  Protocol Client ──transport──► ProtocolAdapter ──► HttpServer ──► Cell process
//!                                (framing)           (dispatch)     (WASI guest)
//! ```
//!
//! // NOTE: interested in feedback from embedded apps and other
//! // resource-constrained environments where execution mode matters.

pub mod server;
pub use rpc::wagi;

use std::fmt;

use async_trait::async_trait;

// =========================================================================
// ProtocolAdapter trait
// =========================================================================

/// A protocol adapter that handles framing, decoding, and encoding
/// for a specific wire protocol.
///
/// The dispatcher (HttpServer) is generic over this trait. Adding a new
/// protocol = implementing this trait. The dispatcher doesn't change.
///
#[async_trait]
pub trait ProtocolAdapter {
    /// Protocol-specific request type.
    type Request: Send + 'static;
    /// Protocol-specific response type.
    type Response: Send + 'static;
    /// Protocol-specific error type.
    type Error: fmt::Display + Send + 'static;

    /// Read the next request from the transport.
    ///
    /// Handles protocol framing and lifecycle methods internally.
    /// Returns `Ok(None)` on clean EOF (client disconnected).
    /// Returns `Err` on I/O or parse failure.
    ///
    async fn next_request(&mut self) -> Result<Option<Self::Request>, Self::Error>;

    /// Extract the body bytes to write to the cell process's stdin.
    /// Called once per request. The process reads these bytes then sees EOF.
    fn request_body(req: &Self::Request) -> Vec<u8>;

    /// Build a response from the cell process's stdout output.
    fn build_response(req: &Self::Request, stdout: Vec<u8>, exit_code: i32) -> Self::Response;

    /// Build an error response (spawn failure, crash, etc.).
    fn build_error_response(req: &Self::Request, err: &dyn fmt::Display) -> Self::Response;

    /// Write the response back to the transport.
    async fn send_response(&mut self, resp: Self::Response) -> Result<(), Self::Error>;
}

// =========================================================================
// HttpServer — the dispatcher
// =========================================================================

/// Maximum response size from a cell process (16 MiB).
/// Prevents OOM from buggy/malicious cells writing unlimited data to stdout.
const MAX_RESPONSE_BYTES: usize = 16 * 1024 * 1024;

/// Host-side request dispatcher. Routes protocol requests to WASI cell processes.
///
/// Mode A (per-request spawn): each handle() call spawns a fresh process.
/// Mode B (long-lived worker): not yet implemented — will use SwappableReader/Writer.
pub struct HttpServer {
    /// Cap'n Proto client for the executor.
    executor: crate::system_capnp::executor::Client,
}

impl HttpServer {
    /// Create a new HttpServer with an executor.
    pub fn new(executor: crate::system_capnp::executor::Client) -> Self {
        Self { executor }
    }

    /// Handle one request: spawn process, pipe stdin/stdout, wait for exit.
    ///
    /// This is Mode A (per-request spawn). Each call creates a fresh WASI
    /// process, writes the request body to stdin, reads the response from
    /// stdout, waits for exit.
    pub async fn handle(&self, body: Vec<u8>) -> Result<(Vec<u8>, i32), capnp::Error> {
        // Spawn a fresh process
        let spawn_resp = self.executor.spawn_request().send().promise.await?;
        let process = spawn_resp.get()?.get_process()?;

        // Write request body to stdin, then close
        let stdin_resp = process.stdin_request().send().promise.await?;
        let stdin = stdin_resp.get()?.get_stream()?;
        let mut write_req = stdin.write_request();
        write_req.get().set_data(&body);
        write_req.send().promise.await?;
        stdin.close_request().send().promise.await?;

        // Read response from stdout until EOF (bounded by MAX_RESPONSE_BYTES)
        let stdout_resp = process.stdout_request().send().promise.await?;
        let stdout = stdout_resp.get()?.get_stream()?;
        let mut response = Vec::new();
        loop {
            let mut read_req = stdout.read_request();
            read_req.get().set_max_bytes(64 * 1024);
            let read_resp = read_req.send().promise.await?;
            let chunk = read_resp.get()?.get_data()?;
            if chunk.is_empty() {
                break; // EOF
            }
            response.extend_from_slice(chunk);
            if response.len() > MAX_RESPONSE_BYTES {
                // Kill the process and return error
                let _ = process.wait_request().send().promise.await;
                return Err(capnp::Error::failed(format!(
                    "cell response exceeded {} bytes",
                    MAX_RESPONSE_BYTES
                )));
            }
        }

        // Always call wait() to collect exit code and clean up resources
        let wait_resp = process.wait_request().send().promise.await?;
        let exit_code = wait_resp.get()?.get_exit_code();

        Ok((response, exit_code))
    }

    /// Run the dispatcher loop: read requests from the adapter, route to
    /// cell processes, send responses back.
    ///
    /// Processes requests serially. Returns when the adapter signals EOF
    /// (next_request returns None) or encounters an error.
    pub async fn run<P: ProtocolAdapter>(&self, adapter: &mut P) -> Result<(), P::Error> {
        while let Some(req) = adapter.next_request().await? {
            let body = P::request_body(&req);
            let resp = match self.handle(body).await {
                Ok((stdout, exit_code)) => P::build_response(&req, stdout, exit_code),
                Err(err) => P::build_error_response(&req, &err),
            };
            adapter.send_response(resp).await?;
        }
        Ok(())
    }
}

// =========================================================================
// Tests
// =========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // Verify the trait is object-safe by checking it compiles as a type parameter
    fn _assert_adapter_compiles<P: ProtocolAdapter>(_p: &P) {}

    // Verify HttpServer can be constructed
    #[test]
    fn http_server_struct_exists() {
        // Can't construct without a real capnp client, but verify the type exists
        let _: fn(crate::system_capnp::executor::Client) -> HttpServer = HttpServer::new;
    }
}
