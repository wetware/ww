//! WAGI (WebAssembly Gateway Interface) adapter.
//!
//! CGI for WASM: host parses HTTP, injects request metadata as environment
//! variables (RFC 3875), pipes the request body to stdin, reads a
//! CGI-formatted response from stdout. Fresh cell per request. Stateless.
//!
//! This module provides standalone functions for CGI env construction and
//! response parsing. WagiAdapter does NOT implement ProtocolAdapter because
//! ProtocolAdapter's request_body() returns only Vec<u8> and WAGI needs
//! env vars too. Phase 2 will either extend the trait or bypass the generic
//! dispatcher.

use std::collections::HashMap;

/// A parsed CGI response from cell stdout.
#[derive(Debug, Clone)]
pub struct WagiResponse {
    pub status_code: u16,
    pub reason: String,
    pub headers: HashMap<String, String>,
    pub body: Vec<u8>,
}

/// Errors that can occur when parsing CGI output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WagiError {
    /// Cell produced no output at all.
    EmptyOutput,
    /// Headers could not be parsed (no \r\n\r\n or \n\n separator found,
    /// or individual header lines are malformed).
    MalformedHeaders(String),
}

impl std::fmt::Display for WagiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WagiError::EmptyOutput => write!(f, "cell produced empty stdout"),
            WagiError::MalformedHeaders(msg) => write!(f, "malformed CGI headers: {msg}"),
        }
    }
}

impl std::error::Error for WagiError {}

/// Construct RFC 3875 CGI environment variables from an HTTP request.
///
/// Returns a `Vec<String>` of `KEY=VALUE` pairs suitable for passing to
/// `ProcBuilder::with_env()` or `Executor.bind()`.
///
/// Header values are converted with `to_string_lossy()` to handle non-UTF8.
pub fn build_cgi_env(
    method: &str,
    path: &str,
    query: &str,
    headers: &[(String, String)],
    server_name: &str,
    server_port: u16,
) -> Vec<String> {
    let mut env = Vec::with_capacity(8 + headers.len());

    env.push(format!("REQUEST_METHOD={method}"));
    env.push(format!("PATH_INFO={path}"));
    env.push(format!("QUERY_STRING={query}"));
    env.push(format!("SERVER_NAME={server_name}"));
    env.push(format!("SERVER_PORT={server_port}"));
    env.push("SERVER_PROTOCOL=HTTP/1.1".to_string());
    env.push("GATEWAY_INTERFACE=CGI/1.1".to_string());

    for (name, value) in headers {
        let upper = name.to_uppercase().replace('-', "_");
        match upper.as_str() {
            "CONTENT_TYPE" => env.push(format!("CONTENT_TYPE={value}")),
            "CONTENT_LENGTH" => env.push(format!("CONTENT_LENGTH={value}")),
            _ => env.push(format!("HTTP_{upper}={value}")),
        }
    }

    env
}

/// Parse a CGI-formatted response from cell stdout.
///
/// Expected format:
/// ```text
/// Status: 200 OK\r\n
/// Content-Type: text/plain\r\n
/// \r\n
/// body bytes here
/// ```
///
/// Missing `Status:` line defaults to 200 OK (per CGI spec).
/// Missing `Content-Type` defaults to `text/plain`.
///
/// Stdout is authoritative: if a valid response is found, it is returned
/// regardless of exit code. Exit code is for observability only.
pub fn parse_cgi_response(stdout: &[u8]) -> Result<WagiResponse, WagiError> {
    if stdout.is_empty() {
        return Err(WagiError::EmptyOutput);
    }

    // Find the header/body separator: \r\n\r\n or \n\n
    let (header_end, body_start) = find_header_boundary(stdout).ok_or_else(|| {
        // No blank line found. This parser currently requires an explicit
        // header/body separator and treats separator-less output as malformed.
        WagiError::MalformedHeaders("no header/body separator found".into())
    })?;

    let header_bytes = &stdout[..header_end];
    let body = stdout[body_start..].to_vec();

    // Parse header lines
    let header_str = String::from_utf8_lossy(header_bytes);
    let mut status_code: u16 = 200;
    let mut reason = "OK".to_string();
    let mut headers = HashMap::new();

    for line in header_str.lines() {
        let line = line.trim_end_matches('\r');
        if line.is_empty() {
            continue;
        }

        if let Some(status_val) = line.strip_prefix("Status:") {
            let status_val = status_val.trim();
            // Parse "NNN Reason" or just "NNN"
            let (code_str, rest) = match status_val.find(' ') {
                Some(idx) => (&status_val[..idx], status_val[idx + 1..].trim()),
                None => (status_val, ""),
            };
            status_code = code_str.parse().map_err(|_| {
                WagiError::MalformedHeaders(format!("invalid status code: {code_str}"))
            })?;
            if !rest.is_empty() {
                reason = rest.to_string();
            }
        } else if let Some(idx) = line.find(':') {
            let key = line[..idx].trim().to_string();
            let val = line[idx + 1..].trim().to_string();
            headers.insert(key, val);
        } else {
            return Err(WagiError::MalformedHeaders(format!(
                "header line missing colon: {line}"
            )));
        }
    }

    // Default Content-Type if not set
    if !headers.contains_key("Content-Type") {
        headers.insert("Content-Type".to_string(), "text/plain".to_string());
    }

    Ok(WagiResponse {
        status_code,
        reason,
        headers,
        body,
    })
}

/// Map a WAGI error to an HTTP status code.
pub fn error_to_status(err: &WagiError) -> u16 {
    match err {
        WagiError::EmptyOutput => 502,
        WagiError::MalformedHeaders(_) => 502,
    }
}

/// Find the header/body boundary in CGI output.
/// Returns (header_end_offset, body_start_offset).
fn find_header_boundary(data: &[u8]) -> Option<(usize, usize)> {
    // Try \r\n\r\n first
    if let Some(pos) = find_bytes(data, b"\r\n\r\n") {
        return Some((pos, pos + 4));
    }
    // Fall back to \n\n
    if let Some(pos) = find_bytes(data, b"\n\n") {
        return Some((pos, pos + 2));
    }
    None
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ===== build_cgi_env tests =====

    #[test]
    fn cgi_env_basic() {
        let env = build_cgi_env("GET", "/counter", "", &[], "localhost", 8080);
        assert!(env.contains(&"REQUEST_METHOD=GET".to_string()));
        assert!(env.contains(&"PATH_INFO=/counter".to_string()));
        assert!(env.contains(&"QUERY_STRING=".to_string()));
        assert!(env.contains(&"SERVER_NAME=localhost".to_string()));
        assert!(env.contains(&"SERVER_PORT=8080".to_string()));
        assert!(env.contains(&"SERVER_PROTOCOL=HTTP/1.1".to_string()));
        assert!(env.contains(&"GATEWAY_INTERFACE=CGI/1.1".to_string()));
    }

    #[test]
    fn cgi_env_with_query_string() {
        let env = build_cgi_env(
            "GET",
            "/search",
            "q=hello&page=1",
            &[],
            "localhost",
            0,
        );
        assert!(env.contains(&"QUERY_STRING=q=hello&page=1".to_string()));
    }

    #[test]
    fn cgi_env_headers_prefixed() {
        let headers = vec![
            ("Accept".to_string(), "text/html".to_string()),
            ("Host".to_string(), "example.com".to_string()),
            ("X-Custom-Header".to_string(), "value".to_string()),
        ];
        let env = build_cgi_env("POST", "/api", "", &headers, "localhost", 8080);
        assert!(env.contains(&"HTTP_ACCEPT=text/html".to_string()));
        assert!(env.contains(&"HTTP_HOST=example.com".to_string()));
        assert!(env.contains(&"HTTP_X_CUSTOM_HEADER=value".to_string()));
    }

    #[test]
    fn cgi_env_content_type_not_prefixed() {
        let headers = vec![
            ("Content-Type".to_string(), "application/json".to_string()),
            ("Content-Length".to_string(), "42".to_string()),
        ];
        let env = build_cgi_env("POST", "/api", "", &headers, "localhost", 8080);
        assert!(env.contains(&"CONTENT_TYPE=application/json".to_string()));
        assert!(env.contains(&"CONTENT_LENGTH=42".to_string()));
        // Should NOT have HTTP_ prefix
        assert!(!env.iter().any(|e| e.starts_with("HTTP_CONTENT_TYPE")));
        assert!(!env.iter().any(|e| e.starts_with("HTTP_CONTENT_LENGTH")));
    }

    // ===== parse_cgi_response tests =====

    #[test]
    fn parse_valid_response() {
        let stdout = b"Status: 200 OK\r\nContent-Type: text/plain\r\n\r\nhello world";
        let resp = parse_cgi_response(stdout).unwrap();
        assert_eq!(resp.status_code, 200);
        assert_eq!(resp.reason, "OK");
        assert_eq!(resp.headers.get("Content-Type").unwrap(), "text/plain");
        assert_eq!(resp.body, b"hello world");
    }

    #[test]
    fn parse_no_status_line_defaults_200() {
        let stdout = b"Content-Type: text/html\r\n\r\n<h1>hi</h1>";
        let resp = parse_cgi_response(stdout).unwrap();
        assert_eq!(resp.status_code, 200);
        assert_eq!(resp.body, b"<h1>hi</h1>");
    }

    #[test]
    fn parse_lf_line_endings() {
        let stdout = b"Status: 404 Not Found\nContent-Type: text/plain\n\nnot here";
        let resp = parse_cgi_response(stdout).unwrap();
        assert_eq!(resp.status_code, 404);
        assert_eq!(resp.reason, "Not Found");
        assert_eq!(resp.body, b"not here");
    }

    #[test]
    fn parse_missing_content_type_defaults() {
        let stdout = b"Status: 200 OK\r\n\r\nbody";
        let resp = parse_cgi_response(stdout).unwrap();
        assert_eq!(resp.headers.get("Content-Type").unwrap(), "text/plain");
    }

    #[test]
    fn parse_empty_stdout_is_error() {
        let result = parse_cgi_response(b"");
        assert_eq!(result.unwrap_err(), WagiError::EmptyOutput);
    }

    #[test]
    fn parse_no_separator_is_error() {
        let stdout = b"just some text with no blank line";
        let result = parse_cgi_response(stdout);
        assert!(matches!(result, Err(WagiError::MalformedHeaders(_))));
    }

    #[test]
    fn parse_405_response() {
        let stdout =
            b"Status: 405 Method Not Allowed\r\nContent-Type: text/plain\r\n\r\nMethod Not Allowed";
        let resp = parse_cgi_response(stdout).unwrap();
        assert_eq!(resp.status_code, 405);
        assert_eq!(resp.reason, "Method Not Allowed");
    }

    #[test]
    fn parse_status_code_only_no_reason() {
        let stdout = b"Status: 204\r\n\r\n";
        let resp = parse_cgi_response(stdout).unwrap();
        assert_eq!(resp.status_code, 204);
        assert_eq!(resp.body, b"");
    }

    #[test]
    fn parse_empty_body() {
        let stdout = b"Status: 204 No Content\r\n\r\n";
        let resp = parse_cgi_response(stdout).unwrap();
        assert_eq!(resp.status_code, 204);
        assert_eq!(resp.body, b"");
    }

    #[test]
    fn parse_binary_body() {
        let mut stdout =
            b"Status: 200 OK\r\nContent-Type: application/octet-stream\r\n\r\n".to_vec();
        stdout.extend_from_slice(&[0x00, 0x01, 0xff, 0xfe]);
        let resp = parse_cgi_response(&stdout).unwrap();
        assert_eq!(resp.body, &[0x00, 0x01, 0xff, 0xfe]);
    }

    #[test]
    fn parse_multiple_headers() {
        let stdout = b"Status: 200 OK\r\nContent-Type: text/plain\r\nX-Request-Id: abc123\r\nCache-Control: no-cache\r\n\r\nok";
        let resp = parse_cgi_response(stdout).unwrap();
        assert_eq!(resp.headers.get("X-Request-Id").unwrap(), "abc123");
        assert_eq!(resp.headers.get("Cache-Control").unwrap(), "no-cache");
    }

    // ===== error_to_status tests =====

    #[test]
    fn error_status_codes() {
        assert_eq!(error_to_status(&WagiError::EmptyOutput), 502);
        assert_eq!(
            error_to_status(&WagiError::MalformedHeaders("test".into())),
            502
        );
    }
}
