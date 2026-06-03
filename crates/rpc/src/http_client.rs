//! Epoch-guarded outbound HTTP proxy with domain scoping.
//!
//! The `EpochGuardedHttpProxy` implements the `HttpClient` Cap'n Proto interface,
//! checking the epoch guard and validating the URL host against an allowlist
//! before forwarding the request via `reqwest`.

use capnp::capability::Promise;
use capnp_rpc::pry;
use membrane::EpochGuard;

use membrane::http_capnp;

const MAX_REDIRECTS: usize = 10;

/// Epoch-guarded HTTP proxy that enforces domain scoping.
///
/// Only created when the operator passes `--http-dial` flags.
/// The allowlist supports exact hosts, subdomain globs (`*.example.com`),
/// and `*` for unrestricted access.
pub struct EpochGuardedHttpProxy {
    client: reqwest::Client,
    guard: EpochGuard,
    allowed_hosts: Vec<String>,
}

impl EpochGuardedHttpProxy {
    pub fn new(allowed_hosts: Vec<String>, guard: EpochGuard) -> Self {
        let redirect_allowed_hosts = allowed_hosts.clone();
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(5))
            .redirect(reqwest::redirect::Policy::custom(move |attempt| {
                if attempt.previous().len() > MAX_REDIRECTS {
                    return attempt.error(std::io::Error::other("too many redirects"));
                }

                if let Err(message) =
                    Self::validate_url_authorized(attempt.url(), &redirect_allowed_hosts)
                {
                    return attempt.error(std::io::Error::new(
                        std::io::ErrorKind::PermissionDenied,
                        message,
                    ));
                }

                // Reqwest removes Authorization, Cookie, Proxy-Authorization, and
                // related sensitive headers on cross-host redirects after this
                // policy returns Follow.
                attempt.follow()
            }))
            .build()
            .expect("failed to build reqwest client");
        Self {
            client,
            guard,
            allowed_hosts,
        }
    }
}

impl EpochGuardedHttpProxy {
    /// Validate epoch, parse URL, and enforce domain scoping.
    fn validate_request(&self, url_str: &str) -> Result<reqwest::Url, capnp::Error> {
        self.guard.check()?;

        let parsed = reqwest::Url::parse(url_str)
            .map_err(|e| capnp::Error::failed(format!("invalid URL: {e}")))?;

        Self::validate_url_authorized(&parsed, &self.allowed_hosts)
            .map_err(capnp::Error::failed)?;

        Ok(parsed)
    }

    fn validate_url_authorized(
        parsed: &reqwest::Url,
        allowed_hosts: &[String],
    ) -> Result<(), String> {
        if parsed.scheme() != "http" && parsed.scheme() != "https" {
            return Err(format!("URL scheme {:?} is not allowed", parsed.scheme()));
        }

        let host = parsed
            .host_str()
            .ok_or_else(|| "URL has no host".to_string())?;

        if !Self::host_matches_allowed(host, allowed_hosts) {
            return Err(format!("host {host:?} not in allowlist"));
        }

        Ok(())
    }

    fn host_matches_allowed(host: &str, allowed_hosts: &[String]) -> bool {
        allowed_hosts.iter().any(|pattern| {
            if pattern == "*" {
                true
            } else if let Some(suffix) = pattern.strip_prefix("*.") {
                host == suffix || host.ends_with(&format!(".{suffix}"))
            } else {
                host == pattern
            }
        })
    }

    /// Extract headers from a Cap'n Proto header list into a vec of (name, value) pairs.
    fn extract_headers(
        headers: capnp::struct_list::Reader<'_, http_capnp::header::Owned>,
    ) -> Result<Vec<(String, String)>, capnp::Error> {
        let mut out = Vec::with_capacity(headers.len() as usize);
        for i in 0..headers.len() {
            let h = headers.get(i);
            let name = h
                .get_name()?
                .to_str()
                .map_err(|e| capnp::Error::failed(e.to_string()))?
                .to_string();
            let value = h
                .get_value()?
                .to_str()
                .map_err(|e| capnp::Error::failed(e.to_string()))?
                .to_string();
            out.push((name, value));
        }
        Ok(out)
    }

    /// Execute a request and serialize the response into Cap'n Proto results.
    async fn execute_and_serialize<T>(
        client: reqwest::Client,
        request: reqwest::Request,
        mut results: T,
    ) -> Result<(), capnp::Error>
    where
        T: ResponseBuilder,
    {
        let response = client.execute(request).await.map_err(|e| {
            capnp::Error::failed(format!(
                "HTTP request failed: {}",
                Self::format_reqwest_error(&e)
            ))
        })?;

        let status = response.status().as_u16();
        let resp_headers: Vec<(String, String)> = response
            .headers()
            .iter()
            .map(|(k, v)| (k.as_str().to_string(), v.to_str().unwrap_or("").to_string()))
            .collect();
        let body = response
            .bytes()
            .await
            .map_err(|e| capnp::Error::failed(format!("failed to read body: {e}")))?;

        results.set_response(status, &resp_headers, &body);
        Ok(())
    }

    fn format_reqwest_error(error: &reqwest::Error) -> String {
        let mut message = error.to_string();
        let mut current = std::error::Error::source(error);
        while let Some(source) = current {
            message.push_str(": ");
            message.push_str(&source.to_string());
            current = source.source();
        }
        message
    }
}

/// Trait to abstract over GetResults and PostResults for response serialization.
trait ResponseBuilder {
    fn set_response(&mut self, status: u16, headers: &[(String, String)], body: &[u8]);
}

impl ResponseBuilder for http_capnp::http_client::GetResults {
    fn set_response(&mut self, status: u16, headers: &[(String, String)], body: &[u8]) {
        let mut res = self.get();
        res.set_status(status);
        res.set_body(body);
        let mut header_list = res.init_headers(headers.len() as u32);
        for (i, (name, value)) in headers.iter().enumerate() {
            let mut h = header_list.reborrow().get(i as u32);
            h.set_name(name);
            h.set_value(value);
        }
    }
}

impl ResponseBuilder for http_capnp::http_client::PostResults {
    fn set_response(&mut self, status: u16, headers: &[(String, String)], body: &[u8]) {
        let mut res = self.get();
        res.set_status(status);
        res.set_body(body);
        let mut header_list = res.init_headers(headers.len() as u32);
        for (i, (name, value)) in headers.iter().enumerate() {
            let mut h = header_list.reborrow().get(i as u32);
            h.set_name(name);
            h.set_value(value);
        }
    }
}

#[allow(refining_impl_trait)]
impl http_capnp::http_client::Server for EpochGuardedHttpProxy {
    fn get(
        self: capnp::capability::Rc<Self>,
        params: http_capnp::http_client::GetParams,
        results: http_capnp::http_client::GetResults,
    ) -> Promise<(), capnp::Error> {
        let reader = pry!(params.get());
        let url_str = pry!(pry!(reader.get_url())
            .to_str()
            .map_err(|e| capnp::Error::failed(e.to_string())));

        pry!(self.validate_request(url_str));

        let req_headers = pry!(Self::extract_headers(pry!(reader.get_headers())));
        let mut builder = self.client.get(url_str);
        for (name, value) in &req_headers {
            builder = builder.header(name.as_str(), value.as_str());
        }

        let request = pry!(builder
            .build()
            .map_err(|e| capnp::Error::failed(format!("failed to build request: {e}"))));
        let client = self.client.clone();

        Promise::from_future(
            async move { Self::execute_and_serialize(client, request, results).await },
        )
    }

    fn post(
        self: capnp::capability::Rc<Self>,
        params: http_capnp::http_client::PostParams,
        results: http_capnp::http_client::PostResults,
    ) -> Promise<(), capnp::Error> {
        let reader = pry!(params.get());
        let url_str = pry!(pry!(reader.get_url())
            .to_str()
            .map_err(|e| capnp::Error::failed(e.to_string())));

        pry!(self.validate_request(url_str));

        let req_headers = pry!(Self::extract_headers(pry!(reader.get_headers())));
        let body_bytes = pry!(reader.get_body()).to_vec();

        let mut builder = self.client.post(url_str).body(body_bytes);
        for (name, value) in &req_headers {
            builder = builder.header(name.as_str(), value.as_str());
        }

        let request = pry!(builder
            .build()
            .map_err(|e| capnp::Error::failed(format!("failed to build request: {e}"))));
        let client = self.client.clone();

        Promise::from_future(
            async move { Self::execute_and_serialize(client, request, results).await },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use membrane::epoch::{Epoch, Provenance};
    use std::error::Error;
    use std::sync::Arc;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};
    use tokio::sync::watch;
    use tokio::task::JoinHandle;

    fn test_proxy(allowed_hosts: Vec<String>) -> EpochGuardedHttpProxy {
        let epoch = Epoch {
            seq: 1,
            head: vec![],
            provenance: Provenance::Block(0),
        };
        let (_tx, rx) = watch::channel(epoch);
        let guard = EpochGuard {
            issued_seq: 1,
            receiver: rx,
        };
        EpochGuardedHttpProxy::new(allowed_hosts, guard)
    }

    fn stale_proxy(allowed_hosts: Vec<String>) -> EpochGuardedHttpProxy {
        let epoch = Epoch {
            seq: 2,
            head: vec![],
            provenance: Provenance::Block(0),
        };
        let (_tx, rx) = watch::channel(epoch);
        let guard = EpochGuard {
            issued_seq: 1, // stale — epoch advanced
            receiver: rx,
        };
        EpochGuardedHttpProxy::new(allowed_hosts, guard)
    }

    struct TestResponse {
        status: u16,
        headers: Vec<(&'static str, String)>,
        body: &'static str,
    }

    impl TestResponse {
        fn ok(body: &'static str) -> Self {
            Self {
                status: 200,
                headers: Vec::new(),
                body,
            }
        }

        fn redirect(location: impl Into<String>) -> Self {
            Self {
                status: 302,
                headers: vec![("Location", location.into())],
                body: "",
            }
        }
    }

    struct TestServer {
        base_url: String,
        handle: JoinHandle<()>,
    }

    impl TestServer {
        async fn spawn(
            handler: impl Fn(String) -> TestResponse + Send + Sync + 'static,
        ) -> std::io::Result<Self> {
            let listener = TcpListener::bind("127.0.0.1:0").await?;
            let base_url = format!("http://{}", listener.local_addr()?);
            let handler = Arc::new(handler);
            let handle = tokio::spawn(async move {
                loop {
                    let Ok((stream, _)) = listener.accept().await else {
                        break;
                    };
                    let handler = Arc::clone(&handler);
                    tokio::spawn(async move {
                        let _ = serve_connection(stream, handler).await;
                    });
                }
            });

            Ok(Self { base_url, handle })
        }

        fn url(&self, path: &str) -> String {
            format!("{}{}", self.base_url, path)
        }
    }

    impl Drop for TestServer {
        fn drop(&mut self) {
            self.handle.abort();
        }
    }

    async fn serve_connection<F>(mut stream: TcpStream, handler: Arc<F>) -> std::io::Result<()>
    where
        F: Fn(String) -> TestResponse + Send + Sync + 'static,
    {
        let mut buf = [0_u8; 2048];
        let n = stream.read(&mut buf).await?;
        let request = String::from_utf8_lossy(&buf[..n]);
        let path = request
            .lines()
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
            .unwrap_or("/")
            .to_string();
        let response = handler(path);
        let reason = match response.status {
            200 => "OK",
            302 => "Found",
            _ => "Error",
        };

        let mut raw = format!(
            "HTTP/1.1 {} {}\r\nContent-Length: {}\r\nConnection: close\r\n",
            response.status,
            reason,
            response.body.len()
        );
        for (name, value) in response.headers {
            raw.push_str(name);
            raw.push_str(": ");
            raw.push_str(&value);
            raw.push_str("\r\n");
        }
        raw.push_str("\r\n");
        raw.push_str(response.body);

        stream.write_all(raw.as_bytes()).await
    }

    fn error_chain_contains(err: &(dyn Error + 'static), needle: &str) -> bool {
        let mut current = Some(err);
        while let Some(err) = current {
            if err.to_string().contains(needle) {
                return true;
            }
            current = err.source();
        }
        false
    }

    #[test]
    fn allowlist_permits_listed_host() {
        let proxy = test_proxy(vec!["example.com".into()]);
        assert!(proxy.validate_request("https://example.com/path").is_ok());
    }

    #[tokio::test]
    async fn follows_allowed_redirect() {
        let server = TestServer::spawn(|path| match path.as_str() {
            "/start" => TestResponse::redirect("/final"),
            "/final" => TestResponse::ok("final body"),
            _ => TestResponse {
                status: 404,
                headers: Vec::new(),
                body: "not found",
            },
        })
        .await
        .unwrap();
        let proxy = test_proxy(vec!["127.0.0.1".into()]);

        let response = proxy.client.get(server.url("/start")).send().await.unwrap();

        assert_eq!(response.status(), reqwest::StatusCode::OK);
        assert_eq!(response.text().await.unwrap(), "final body");
    }

    #[tokio::test]
    async fn rejects_redirect_to_disallowed_host() {
        let server = TestServer::spawn(|path| match path.as_str() {
            "/start" => TestResponse::redirect("http://127.0.0.2/blocked"),
            _ => TestResponse::ok("unexpected"),
        })
        .await
        .unwrap();
        let proxy = test_proxy(vec!["127.0.0.1".into()]);

        let err = proxy
            .client
            .get(server.url("/start"))
            .send()
            .await
            .unwrap_err();

        assert!(
            error_chain_contains(&err, "not in allowlist"),
            "unexpected error: {err:?}"
        );
    }

    #[tokio::test]
    async fn rejects_redirects_after_max_depth() {
        let server = TestServer::spawn(|_| TestResponse::redirect("/loop"))
            .await
            .unwrap();
        let proxy = test_proxy(vec!["127.0.0.1".into()]);

        let err = proxy
            .client
            .get(server.url("/loop"))
            .send()
            .await
            .unwrap_err();

        assert!(
            error_chain_contains(&err, "too many redirects"),
            "unexpected error: {err:?}"
        );
    }

    #[tokio::test]
    async fn rejects_redirect_to_non_http_scheme() {
        let server = TestServer::spawn(|path| match path.as_str() {
            "/start" => TestResponse::redirect("ftp://127.0.0.1/blocked"),
            _ => TestResponse::ok("unexpected"),
        })
        .await
        .unwrap();
        let proxy = test_proxy(vec!["127.0.0.1".into()]);

        let err = proxy
            .client
            .get(server.url("/start"))
            .send()
            .await
            .unwrap_err();

        assert!(
            error_chain_contains(&err, "not allowed"),
            "unexpected error: {err:?}"
        );
    }

    #[test]
    fn allowlist_rejects_unlisted_host() {
        let proxy = test_proxy(vec!["example.com".into()]);
        let err = proxy.validate_request("https://evil.com/path").unwrap_err();
        assert!(err.to_string().contains("not in allowlist"));
    }

    #[test]
    fn empty_allowlist_rejects_all() {
        let proxy = test_proxy(vec![]);
        let err = proxy
            .validate_request("https://anything.example.org/x")
            .unwrap_err();
        assert!(err.to_string().contains("not in allowlist"));
    }

    #[test]
    fn star_wildcard_permits_all() {
        let proxy = test_proxy(vec!["*".into()]);
        assert!(proxy
            .validate_request("https://anything.example.org/x")
            .is_ok());
    }

    #[test]
    fn subdomain_wildcard_matches_subdomains() {
        let proxy = test_proxy(vec!["*.example.com".into()]);
        assert!(proxy
            .validate_request("https://api.example.com/path")
            .is_ok());
        assert!(proxy
            .validate_request("https://deep.sub.example.com/path")
            .is_ok());
    }

    #[test]
    fn subdomain_wildcard_matches_bare_domain() {
        let proxy = test_proxy(vec!["*.example.com".into()]);
        assert!(proxy.validate_request("https://example.com/path").is_ok());
    }

    #[test]
    fn subdomain_wildcard_rejects_other_domains() {
        let proxy = test_proxy(vec!["*.example.com".into()]);
        let err = proxy.validate_request("https://evil.com/path").unwrap_err();
        assert!(err.to_string().contains("not in allowlist"));
    }

    #[test]
    fn rejects_invalid_url() {
        let proxy = test_proxy(vec!["*".into()]);
        let err = proxy.validate_request("not a url").unwrap_err();
        assert!(err.to_string().contains("invalid URL"));
    }

    #[test]
    fn rejects_initial_non_http_scheme() {
        let proxy = test_proxy(vec!["*".into()]);
        let err = proxy.validate_request("data:text/plain,hello").unwrap_err();
        assert!(err.to_string().contains("not allowed"));
    }

    #[test]
    fn stale_epoch_rejects_request() {
        let proxy = stale_proxy(vec!["*".into()]);
        let err = proxy.validate_request("https://example.com").unwrap_err();
        assert!(err.to_string().contains("staleEpoch"));
    }

    #[test]
    fn allowlist_does_not_match_subdomains() {
        let proxy = test_proxy(vec!["example.com".into()]);
        let err = proxy
            .validate_request("https://sub.example.com/path")
            .unwrap_err();
        assert!(err.to_string().contains("not in allowlist"));
    }
}
