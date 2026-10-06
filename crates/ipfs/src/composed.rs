//! Block transport for the structural image composer.

use anyhow::{ensure, Context, Result};
use cid::Cid;
use serde::{Deserialize, Serialize};

use crate::{BootClient, HttpClient, KuboApiError, KuboOperationTimeout};

const MAX_IMPORT_RESPONSE_BYTES: usize = 64 * 1024;
const MAX_IMPORT_BYTES: usize = 64 * 1024 * 1024;
const MAX_BLOCK_BYTES: usize = 2 * 1024 * 1024;

impl BootClient {
    /// Read an immutable block under the existing boot retry policy.
    pub async fn block_get(&self, cid: &Cid, max_bytes: usize) -> Result<Vec<u8>> {
        self.retry("IPFS block retrieval", || {
            self.client.block_get(cid, max_bytes)
        })
        .await
    }

    /// Bound one import attempt. The caller owns retries of the composition.
    pub async fn import_composed(&self, root: &Cid, blocks: &[(Cid, Vec<u8>)]) -> Result<()> {
        match self.bounded_timeout() {
            Some(timeout) => {
                tokio::time::timeout(timeout, self.client.import_composed(root, blocks))
                    .await
                    .map_err(|_| KuboOperationTimeout::new("IPFS composed DAG import", timeout))?
            }
            None => self.client.import_composed(root, blocks).await,
        }
    }
}

impl HttpClient {
    /// Fetch a raw immutable block, bounding both advertised and streamed bytes.
    /// The composer verifies the content against the requested CID.
    pub async fn block_get(&self, cid: &Cid, max_bytes: usize) -> Result<Vec<u8>> {
        let response = self
            .http_client
            .post(format!("{}/api/v0/block/get", self.base_url))
            .query(&[("arg", cid.to_string())])
            .send()
            .await
            .with_context(|| format!("Failed to fetch IPFS block {cid}"))?;
        read_response(response, max_bytes, &format!("IPFS block get ({cid})")).await
    }

    /// Store generated directories and recursively retain `root` under one Kubo
    /// GC lock. Reused descendants must already exist locally; their existing
    /// retention belongs to the layer lifecycle, not the composer.
    ///
    /// Kubo v0.33.0 acquires `PinLock` before reading the CAR, commits imported
    /// blocks, recursively pins each root, and flushes pins before unlocking:
    /// https://github.com/ipfs/kubo/blob/v0.33.0/core/commands/dag/import.go
    /// This excludes GC between generated-block writes and retention. It is not
    /// a transaction: failure or cancellation can leave unpinned blocks, or a
    /// completed pin whose response was lost. Neither is reported as success.
    /// Kubo pins offline and reports missing descendants in `PinErrorMsg`, even
    /// in an HTTP 200 response; consume and validate the entire response.
    pub async fn import_composed(&self, root: &Cid, blocks: &[(Cid, Vec<u8>)]) -> Result<()> {
        let car = encode_car(root, blocks)?;
        let form = reqwest::multipart::Form::new().part(
            "file",
            reqwest::multipart::Part::bytes(car).file_name("composed.car"),
        );
        let response = self
            .http_client
            .post(format!("{}/api/v0/dag/import", self.base_url))
            .query(&[
                ("pin-roots", "true"),
                ("stats", "false"),
                ("silent", "false"),
            ])
            .multipart(form)
            .send()
            .await
            .context("Failed to import composed IPFS DAG")?;
        let body = read_response(
            response,
            MAX_IMPORT_RESPONSE_BYTES,
            "IPFS composed DAG import",
        )
        .await?;
        parse_kubo_dag_import_response(&body, root)
    }
}

/// Parses and validates a raw Kubo `/api/v0/dag/import` response body.
///
/// This function is public so external fuzz targets can exercise the same
/// acknowledgement boundary as the production HTTP client.
#[doc(hidden)]
pub fn parse_kubo_dag_import_response(response_body: &[u8], expected_root: &Cid) -> Result<()> {
    ensure!(
        response_body.len() <= MAX_IMPORT_RESPONSE_BYTES,
        "DAG import response exceeds {MAX_IMPORT_RESPONSE_BYTES} bytes"
    );
    let body = std::str::from_utf8(response_body).context("DAG import response is not UTF-8")?;
    let mut confirmed = false;
    for line in body.lines().filter(|line| !line.trim().is_empty()) {
        // Serde's derived struct visitor also accepts positional arrays.
        // Check the wire object shapes first, then deserialize the original
        // bytes so duplicate fields remain visible to the strict structs.
        let shape: serde_json::Value =
            serde_json::from_str(line).context("Malformed DAG import response")?;
        ensure!(
            shape.is_object()
                && shape.get("Root").is_some_and(serde_json::Value::is_object)
                && shape["Root"]
                    .get("Cid")
                    .is_some_and(serde_json::Value::is_object),
            "DAG import Root and Cid must be JSON objects"
        );
        let event: ImportEvent =
            serde_json::from_str(line).context("Malformed DAG import response")?;
        ensure!(!confirmed, "DAG import returned more than one root");
        // Kubo returns canonical CID text. Parsing alone is insufficient:
        // cid::Cid accepts path prefixes and ignores trailing CID bytes.
        ensure!(
            event.root.cid.value == expected_root.to_string(),
            "DAG import returned an unexpected or noncanonical root CID"
        );
        ensure!(
            event.root.pin_error_msg.is_empty(),
            "DAG import failed to pin root: {}",
            event.root.pin_error_msg
        );
        confirmed = true;
    }
    ensure!(confirmed, "DAG import did not confirm the root pin");
    Ok(())
}

async fn read_response(
    mut response: reqwest::Response,
    max_bytes: usize,
    operation: &str,
) -> Result<Vec<u8>> {
    let status = response.status();
    let limit = if status.is_success() {
        max_bytes
    } else {
        MAX_IMPORT_RESPONSE_BYTES
    };
    ensure!(
        response
            .content_length()
            .is_none_or(|length| length <= limit as u64),
        "{operation} response exceeds {limit} bytes"
    );
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .with_context(|| format!("Failed to read {operation} response"))?
    {
        ensure!(
            chunk.len() <= limit.saturating_sub(body.len()),
            "{operation} response exceeds {limit} bytes"
        );
        body.extend_from_slice(&chunk);
    }
    if !status.is_success() {
        return Err(KuboApiError {
            operation: operation.to_owned(),
            status,
            message: String::from_utf8_lossy(&body).into_owned(),
        }
        .into());
    }
    Ok(body)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ImportEvent {
    #[serde(rename = "Root")]
    root: ImportRoot,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ImportRoot {
    #[serde(rename = "Cid")]
    cid: JsonCid,
    #[serde(rename = "PinErrorMsg")]
    pin_error_msg: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct JsonCid {
    #[serde(rename = "/")]
    value: String,
}

/// CARv1 framing; DAG-CBOR and CID serialization use existing dependencies.
/// https://ipld.io/specs/transport/car/carv1/
fn encode_car(root: &Cid, blocks: &[(Cid, Vec<u8>)]) -> Result<Vec<u8>> {
    #[derive(Serialize)]
    struct Header<'a> {
        roots: [&'a Cid; 1],
        version: u64,
    }
    let header = serde_ipld_dagcbor::to_vec(&Header {
        roots: [root],
        version: 1,
    })?;
    let mut size = header.len() + 10;
    for (cid, block) in blocks {
        ensure!(
            block.len() <= MAX_BLOCK_BYTES,
            "Composed block exceeds {MAX_BLOCK_BYTES} bytes"
        );
        size = size
            .checked_add(cid.to_bytes().len())
            .and_then(|size| size.checked_add(block.len()))
            .and_then(|size| size.checked_add(10))
            .context("Composed CAR size overflow")?;
        ensure!(
            size <= MAX_IMPORT_BYTES,
            "Composed CAR exceeds {MAX_IMPORT_BYTES} bytes"
        );
    }
    let mut car = Vec::with_capacity(size);
    write_varint(header.len(), &mut car);
    car.extend_from_slice(&header);
    for (cid, block) in blocks {
        let cid = cid.to_bytes();
        write_varint(cid.len() + block.len(), &mut car);
        car.extend_from_slice(&cid);
        car.extend_from_slice(block);
    }
    Ok(car)
}

fn write_varint(mut value: usize, output: &mut Vec<u8>) {
    while value >= 128 {
        output.push(value as u8 | 0x80);
        value >>= 7;
    }
    output.push(value as u8);
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    fn root() -> Cid {
        "QmUNLLsPACCz1vLxQVkXqqLX5R1X345qqfHbsf67hvA3Nn"
            .parse()
            .unwrap()
    }

    async fn serve(response: String) -> (HttpClient, tokio::task::JoinHandle<Vec<u8>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut buf = [0; 4096];
            loop {
                let n = stream.read(&mut buf).await.unwrap();
                request.extend_from_slice(&buf[..n]);
                if n == 0 {
                    break;
                }
                if let Some(end) = request.windows(4).position(|bytes| bytes == b"\r\n\r\n") {
                    let header = String::from_utf8_lossy(&request[..end]);
                    let length = header
                        .lines()
                        .find_map(|line| {
                            let (key, value) = line.split_once(':')?;
                            key.eq_ignore_ascii_case("content-length")
                                .then(|| value.trim().parse::<usize>().unwrap())
                        })
                        .unwrap_or(0);
                    if request.len() >= end + 4 + length {
                        break;
                    }
                }
            }
            stream.write_all(response.as_bytes()).await.unwrap();
            request
        });
        let http = reqwest::Client::builder().no_proxy().build().unwrap();
        (
            HttpClient::with_http_client(format!("http://{address}"), http),
            task,
        )
    }

    fn ok(body: &str) -> String {
        format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
    }

    fn success() -> String {
        format!(
            r#"{{"Root":{{"Cid":{{"/":"{}"}},"PinErrorMsg":""}}}}"#,
            root()
        )
    }

    #[test]
    fn dag_import_response_parser_accepts_one_complete_canonical_acknowledgement() {
        let body = format!("\n \r\n{}\n\t\r\n", success());
        crate::parse_kubo_dag_import_response(body.as_bytes(), &root()).unwrap();
    }

    #[test]
    fn dag_import_response_parser_enforces_the_semantic_size_bound() {
        let mut body = success().into_bytes();
        body.resize(MAX_IMPORT_RESPONSE_BYTES, b' ');
        crate::parse_kubo_dag_import_response(&body, &root()).unwrap();

        body.push(b' ');
        assert!(crate::parse_kubo_dag_import_response(&body, &root()).is_err());
    }

    #[test]
    fn dag_import_response_parser_rejects_trailing_events_and_garbage() {
        let good = success();
        for body in [
            format!("{good} trailing"),
            format!("{good}\n{good}"),
            format!("{good}\n{{"),
        ] {
            assert!(
                crate::parse_kubo_dag_import_response(body.as_bytes(), &root()).is_err(),
                "accepted {body}"
            );
        }
    }

    #[test]
    fn dag_import_response_parser_rejects_unknown_fields_at_every_level() {
        for (level, body) in [
            (
                "ImportEvent",
                format!(
                    r#"{{"Root":{{"Cid":{{"/":"{}"}},"PinErrorMsg":""}},"Extra":true}}"#,
                    root()
                ),
            ),
            (
                "ImportRoot",
                format!(
                    r#"{{"Root":{{"Cid":{{"/":"{}"}},"PinErrorMsg":"","Extra":true}}}}"#,
                    root()
                ),
            ),
            (
                "JsonCid",
                format!(
                    r#"{{"Root":{{"Cid":{{"/":"{}","Extra":true}},"PinErrorMsg":""}}}}"#,
                    root()
                ),
            ),
        ] {
            assert!(
                crate::parse_kubo_dag_import_response(body.as_bytes(), &root()).is_err(),
                "accepted unknown field in {level}: {body}"
            );
        }
    }

    #[test]
    fn dag_import_response_parser_requires_a_literally_empty_pin_error() {
        for pin_error in [" ", "\t", "\n"] {
            let body = serde_json::json!({
                "Root": {
                    "Cid": { "/": root().to_string() },
                    "PinErrorMsg": pin_error,
                }
            })
            .to_string();
            assert!(
                crate::parse_kubo_dag_import_response(body.as_bytes(), &root()).is_err(),
                "accepted PinErrorMsg {pin_error:?}"
            );
        }
    }

    #[tokio::test]
    async fn block_get_returns_bytes_and_requests_the_exact_cid() {
        let (client, server) = serve(ok("abcd")).await;
        assert_eq!(client.block_get(&root(), 4).await.unwrap(), b"abcd");
        let request = server.await.unwrap();
        assert!(String::from_utf8_lossy(&request)
            .starts_with(&format!("POST /api/v0/block/get?arg={} ", root())));
    }

    #[tokio::test]
    async fn block_get_bounds_content_length_and_chunked_bodies() {
        for response in [
            ok("abcde"),
            "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n3\r\nabc\r\n2\r\nde\r\n0\r\n\r\n".to_owned(),
        ] {
            let (client, server) = serve(response).await;
            assert!(client.block_get(&root(), 4).await.is_err());
            server.await.unwrap();
        }
    }

    #[tokio::test]
    async fn import_requires_matching_success_and_sends_a_car_with_pinning() {
        let (client, server) = serve(ok(&success())).await;
        client
            .import_composed(&root(), &[(root(), vec![10, 2, 8, 1])])
            .await
            .unwrap();
        let request = server.await.unwrap();
        let header = String::from_utf8_lossy(&request);
        assert!(
            header.starts_with("POST /api/v0/dag/import?pin-roots=true&stats=false&silent=false ")
        );
        assert!(header.contains("multipart/form-data"));
        // Independently encoded canonical CAR header: roots [CID], version 1.
        // 56 bytes: map(1), roots key(6), array(1), tag(2), bytes(2),
        // identity prefix(1), CIDv0(34), version key(8), version value(1).
        let mut expected_header = vec![0x38, 0xa2, 0x65];
        expected_header.extend_from_slice(b"roots");
        expected_header.extend_from_slice(&[0x81, 0xd8, 0x2a, 0x58, 0x23, 0]);
        expected_header.extend_from_slice(&root().to_bytes());
        expected_header.extend_from_slice(&[0x67]);
        expected_header.extend_from_slice(b"version");
        expected_header.push(1);
        assert!(request
            .windows(expected_header.len())
            .any(|bytes| bytes == expected_header));
    }

    #[tokio::test]
    async fn import_rejects_incomplete_malformed_or_failed_pin_responses() {
        let good = success();
        for body in [
            "".to_owned(),
            "{}".to_owned(),
            "[]".to_owned(),
            format!(r#"[{{"Cid":{{"/":"{}"}},"PinErrorMsg":""}}]"#, root()),
            format!(r#"{{"Root":[{{"/":"{}"}},""]}}"#, root()),
            format!(r#"{{"Root":{{"Cid":["{}"],"PinErrorMsg":""}}}}"#, root()),
            r#"{"Root":{"Cid":{"/":"bad"},"PinErrorMsg":""}}"#.to_owned(),
            good.replace("\"PinErrorMsg\":\"\"", "\"PinErrorMsg\":\"missing block\""),
            good.replace(
                &root().to_string(),
                "bafkreigh2akiscaildc4txjku4qpgcoqp5maesbk73bzqpqcm25tyqpv2u",
            ),
            format!("{good}\n{good}"),
            format!("{good}\n{{\"Message\":\"write failed\",\"Code\":0,\"Type\":\"error\"}}"),
            format!("{good}\n{{"),
            good.replace(
                "\"PinErrorMsg\":\"\"",
                "\"PinErrorMsg\":\"\",\"PinErrorMsg\":\"\"",
            ),
            good.replace(",\"PinErrorMsg\":\"\"", ""),
        ] {
            let (client, server) = serve(ok(&body)).await;
            assert!(
                client.import_composed(&root(), &[]).await.is_err(),
                "accepted {body}"
            );
            server.await.unwrap();
        }
    }

    #[tokio::test]
    async fn import_rejects_path_and_trailing_byte_cid_aliases() {
        let mut trailing = root().to_bytes();
        trailing.push(0);
        for alias in [
            format!("/ipfs/{}", root()),
            cid::multibase::encode(cid::multibase::Base::Base16Lower, trailing),
        ] {
            // These malformed wire values previously survived parsing and
            // compared equal to the expected CID.
            assert_eq!(alias.parse::<Cid>().unwrap(), root());
            let body = success().replace(&root().to_string(), &alias);
            let (client, server) = serve(ok(&body)).await;
            assert!(
                client.import_composed(&root(), &[]).await.is_err(),
                "accepted {alias}"
            );
            server.await.unwrap();
        }
    }

    #[tokio::test]
    async fn backend_http_errors_and_truncated_bodies_fail() {
        for response in [
            "HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                .to_owned(),
            "HTTP/1.1 200 OK\r\nContent-Length: 999\r\nConnection: close\r\n\r\nabc".to_owned(),
        ] {
            let (client, server) = serve(response.clone()).await;
            assert!(client.block_get(&root(), 1024).await.is_err());
            server.await.unwrap();
            let (client, server) = serve(response).await;
            assert!(client.import_composed(&root(), &[]).await.is_err());
            server.await.unwrap();
        }
    }

    #[tokio::test]
    async fn import_response_size_is_bounded() {
        let body = " ".repeat(MAX_IMPORT_RESPONSE_BYTES + 1);
        let (client, server) = serve(ok(&body)).await;
        assert!(client.import_composed(&root(), &[]).await.is_err());
        server.await.unwrap();
    }

    #[test]
    fn car_rejects_oversized_blocks_and_total_archive() {
        assert!(encode_car(&root(), &[(root(), vec![0; MAX_BLOCK_BYTES + 1])]).is_err());
        let blocks = vec![(root(), vec![0; MAX_BLOCK_BYTES]); MAX_IMPORT_BYTES / MAX_BLOCK_BYTES];
        assert!(encode_car(&root(), &blocks).is_err());
    }

    #[tokio::test]
    async fn boot_import_timeout_does_not_report_success_or_replay() {
        use std::time::Duration;
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let client = BootClient::one_attempt(
            HttpClient::with_http_client(
                format!("http://{address}"),
                reqwest::Client::builder().no_proxy().build().unwrap(),
            ),
            Duration::from_millis(20),
        );
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            assert!(
                tokio::time::timeout(Duration::from_millis(80), listener.accept())
                    .await
                    .is_err()
            );
            drop(stream);
        });
        let error = client.import_composed(&root(), &[]).await.unwrap_err();
        assert!(error.downcast_ref::<KuboOperationTimeout>().is_some());
        server.await.unwrap();
    }
}
