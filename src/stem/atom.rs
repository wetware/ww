//! Atom-backed [`super::Source`] implementation.
//!
//! The adapter reads `Atom.head()` at finalized chain depth. Contract events
//! are not part of the correctness path. Chain progress alone can therefore
//! make a head authoritative.

use std::time::Duration;

use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use cid::Cid;

use super::{Head, InvalidHead, Source as SourceContract, Update};

const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(1);

#[derive(Clone, Debug)]
struct Snapshot {
    revision: u64,
    finalized_block: u64,
    update: Update,
}

/// Atom source configuration.
#[derive(Clone, Debug)]
pub struct Config {
    pub http_url: String,
    pub contract_address: [u8; 20],
    pub confirmation_depth: u64,
    pub poll_interval: Duration,
}

impl Config {
    pub fn new(http_url: String, contract_address: [u8; 20], confirmation_depth: u64) -> Self {
        Self {
            http_url,
            contract_address,
            confirmation_depth,
            poll_interval: DEFAULT_POLL_INTERVAL,
        }
    }

    pub fn with_poll_interval(mut self, poll_interval: Duration) -> Self {
        self.poll_interval = poll_interval;
        self
    }
}

/// An authoritative Atom Stem source.
pub struct Source {
    config: Config,
    http: reqwest::Client,
    last_update: Option<Update>,
    last_revision: Option<u64>,
    last_finalized_block: Option<u64>,
}

impl Source {
    pub fn new(config: Config) -> Result<Self> {
        let http = reqwest::Client::builder()
            .no_proxy()
            .build()
            .context("building Atom source HTTP client")?;
        Ok(Self {
            config,
            http,
            last_update: None,
            last_revision: None,
            last_finalized_block: None,
        })
    }

    async fn rpc(
        &self,
        method: &str,
        params: serde_json::Value,
        id: u64,
    ) -> Result<serde_json::Value> {
        let response = self
            .http
            .post(&self.config.http_url)
            .json(&serde_json::json!({
                "jsonrpc": "2.0",
                "id": id,
                "method": method,
                "params": params,
            }))
            .send()
            .await
            .with_context(|| format!("Atom source {method} request failed"))?
            .error_for_status()
            .with_context(|| format!("Atom source {method} returned an HTTP failure"))?;
        let response: serde_json::Value = response
            .json()
            .await
            .with_context(|| format!("Atom source {method} response was malformed"))?;
        if let Some(error) = response.get("error") {
            bail!("Atom source {method} RPC error: {error}");
        }
        response
            .get("result")
            .cloned()
            .with_context(|| format!("Atom source {method} response omitted result"))
    }

    async fn snapshot(&self) -> Result<Snapshot> {
        let tip = self
            .rpc("eth_blockNumber", serde_json::json!([]), 1)
            .await?
            .as_str()
            .context("Atom source eth_blockNumber result was not a string")
            .and_then(parse_hex_u64)?;
        let finalized_block = tip.checked_sub(self.config.confirmation_depth).with_context(|| {
            format!(
                "Atom source finalized depth is unavailable: chain tip {tip} is below confirmation depth {}",
                self.config.confirmation_depth
            )
        })?;
        let block = format!("0x{finalized_block:x}");
        let result = self
            .rpc(
                "eth_call",
                serde_json::json!([{
                    "to": format!("0x{}", hex::encode(self.config.contract_address)),
                    "data": format!("0x{}", hex::encode(::atom::abi::HEAD_SELECTOR)),
                }, block]),
                2,
            )
            .await?;
        let encoded = result
            .as_str()
            .context("Atom source eth_call result was not a string")?;
        let encoded = hex::decode(encoded.strip_prefix("0x").unwrap_or(encoded))
            .context("Atom source eth_call result was not hex")?;
        let head = ::atom::abi::decode_head_return(&encoded)
            .context("Atom source could not decode Atom.head()")?;
        let update = match Cid::read_bytes(head.cid.as_slice()) {
            Ok(cid) => Update::Head(Head { cid }),
            Err(error) => Update::InvalidHead(InvalidHead {
                selected: head.cid,
                reason: format!("Atom selected malformed CID bytes: {error}"),
            }),
        };
        Ok(Snapshot {
            revision: head.seq,
            finalized_block,
            update,
        })
    }

    fn accept(&mut self, snapshot: Snapshot) -> Update {
        tracing::debug!(
            atom_revision = snapshot.revision,
            finalized_block = snapshot.finalized_block,
            "Atom source established authoritative state"
        );
        self.last_revision = Some(snapshot.revision);
        self.last_finalized_block = Some(snapshot.finalized_block);
        self.last_update = Some(snapshot.update.clone());
        snapshot.update
    }
}

#[async_trait]
impl SourceContract for Source {
    async fn current(&mut self) -> Result<Update> {
        let snapshot = self.snapshot().await?;
        Ok(self.accept(snapshot))
    }

    async fn next(&mut self) -> Result<Update> {
        loop {
            tokio::time::sleep(self.config.poll_interval).await;
            let snapshot = self.snapshot().await?;
            if self.last_update.as_ref() == Some(&snapshot.update) {
                self.last_revision = Some(snapshot.revision);
                self.last_finalized_block = Some(snapshot.finalized_block);
                continue;
            }
            // `accept` and the return have no cancellation point between them.
            // If this future is dropped earlier, the next call polls chain state
            // again and cannot strand the update.
            return Ok(self.accept(snapshot));
        }
    }
}

fn parse_hex_u64(value: &str) -> Result<u64> {
    let value = value.strip_prefix("0x").unwrap_or(value);
    u64::from_str_radix(value, 16).context("parsing Atom source block number")
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn encode_head(revision: u64, cid: &[u8]) -> String {
        let padded_len = cid.len().div_ceil(32) * 32;
        let mut encoded = vec![0; 96 + padded_len];
        encoded[24..32].copy_from_slice(&revision.to_be_bytes());
        encoded[63] = 64;
        encoded[88..96].copy_from_slice(&(cid.len() as u64).to_be_bytes());
        encoded[96..96 + cid.len()].copy_from_slice(cid);
        format!("0x{}", hex::encode(encoded))
    }

    async fn request(stream: &mut tokio::net::TcpStream) -> serde_json::Value {
        let mut bytes = vec![0_u8; 8192];
        let read = stream.read(&mut bytes).await.unwrap();
        let text = String::from_utf8_lossy(&bytes[..read]);
        serde_json::from_str(text.split("\r\n\r\n").nth(1).unwrap()).unwrap()
    }

    async fn respond(stream: &mut tokio::net::TcpStream, result: serde_json::Value) {
        let body = serde_json::json!({"jsonrpc": "2.0", "id": 1, "result": result}).to_string();
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        stream.write_all(response.as_bytes()).await.unwrap();
    }

    #[tokio::test]
    async fn chain_depth_advances_without_a_second_contract_event() {
        let old: Cid = "bafkreibm6jg3ux5qugqkmfqt5uj5rxszb4sa4e3u7jj4c5ukv5s4xvcc7a"
            .parse()
            .unwrap();
        let new: Cid = "bafkreif2pall7dybz7vecqka3zo24nq2j4tztjwc5c3f4vmrf6sz4d3asa"
            .parse()
            .unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let old_bytes = old.to_bytes();
        let new_bytes = new.to_bytes();
        let server = tokio::spawn(async move {
            let script = [
                ("eth_blockNumber", serde_json::json!("0x2"), None),
                (
                    "eth_call",
                    serde_json::json!(encode_head(1, &old_bytes)),
                    Some("0x0"),
                ),
                ("eth_blockNumber", serde_json::json!("0x3"), None),
                (
                    "eth_call",
                    serde_json::json!(encode_head(1, &old_bytes)),
                    Some("0x1"),
                ),
                ("eth_blockNumber", serde_json::json!("0x4"), None),
                (
                    "eth_call",
                    // A lower backend revision still represents newer
                    // authoritative chain state after a depth-bounded reorg.
                    serde_json::json!(encode_head(0, &new_bytes)),
                    Some("0x2"),
                ),
            ];
            for (method, result, block) in script {
                let (mut stream, _) = listener.accept().await.unwrap();
                let received = request(&mut stream).await;
                assert_eq!(received["method"], method);
                if let Some(block) = block {
                    assert_eq!(received["params"][1], block);
                }
                respond(&mut stream, result).await;
            }
        });
        let config = Config::new(format!("http://{address}"), [0; 20], 2)
            .with_poll_interval(Duration::from_millis(1));
        let mut source = Source::new(config).unwrap();

        assert_eq!(
            source.current().await.unwrap(),
            Update::Head(Head { cid: old })
        );
        assert_eq!(
            source.next().await.unwrap(),
            Update::Head(Head { cid: new })
        );
        server.await.unwrap();
    }

    #[tokio::test]
    async fn malformed_authoritative_cid_is_an_invalid_head_update() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            for (method, result) in [
                ("eth_blockNumber", serde_json::json!("0x6")),
                ("eth_call", serde_json::json!(encode_head(9, b"not-a-cid"))),
            ] {
                let (mut stream, _) = listener.accept().await.unwrap();
                let received = request(&mut stream).await;
                assert_eq!(received["method"], method);
                if method == "eth_call" {
                    assert_eq!(received["params"][1], "0x0");
                }
                respond(&mut stream, result).await;
            }
        });
        let mut source = Source::new(
            Config::new(format!("http://{address}"), [0; 20], 6)
                .with_poll_interval(Duration::from_millis(1)),
        )
        .unwrap();

        let update = source.current().await.unwrap();
        assert!(matches!(update, Update::InvalidHead(_)));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn tip_below_confirmation_depth_is_not_authoritative() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let received = request(&mut stream).await;
            assert_eq!(received["method"], "eth_blockNumber");
            respond(&mut stream, serde_json::json!("0x5")).await;

            tokio::time::timeout(Duration::from_millis(100), listener.accept())
                .await
                .is_err()
        });
        let mut source = Source::new(
            Config::new(format!("http://{address}"), [0; 20], 6)
                .with_poll_interval(Duration::from_millis(1)),
        )
        .unwrap();

        let error = source.current().await.unwrap_err();
        assert!(
            format!("{error:#}").contains(
                "Atom source finalized depth is unavailable: chain tip 5 is below confirmation depth 6"
            ),
            "unexpected error: {error:#}"
        );
        assert!(
            server.await.unwrap(),
            "Atom source queried a block before the configured depth was available"
        );
    }

    #[tokio::test]
    async fn cancelling_next_does_not_strand_an_authoritative_change() {
        let old: Cid = "bafkreibm6jg3ux5qugqkmfqt5uj5rxszb4sa4e3u7jj4c5ukv5s4xvcc7a"
            .parse()
            .unwrap();
        let new: Cid = "bafkreif2pall7dybz7vecqka3zo24nq2j4tztjwc5c3f4vmrf6sz4d3asa"
            .parse()
            .unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let old_bytes = old.to_bytes();
        let new_bytes = new.to_bytes();
        let (response_ready_tx, response_ready_rx) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let mut response_ready_tx = Some(response_ready_tx);
            for (index, (method, result)) in [
                ("eth_blockNumber", serde_json::json!("0x1")),
                ("eth_call", serde_json::json!(encode_head(1, &old_bytes))),
                ("eth_blockNumber", serde_json::json!("0x2")),
                ("eth_call", serde_json::json!(encode_head(2, &new_bytes))),
                ("eth_blockNumber", serde_json::json!("0x2")),
                ("eth_call", serde_json::json!(encode_head(2, &new_bytes))),
            ]
            .into_iter()
            .enumerate()
            {
                let (mut stream, _) = listener.accept().await.unwrap();
                assert_eq!(request(&mut stream).await["method"], method);
                respond(&mut stream, result).await;
                if index == 3 {
                    response_ready_tx.take().unwrap().send(()).unwrap();
                }
            }
        });
        let mut source = Source::new(
            Config::new(format!("http://{address}"), [0; 20], 0)
                .with_poll_interval(Duration::from_millis(1)),
        )
        .unwrap();
        assert_eq!(
            source.current().await.unwrap(),
            Update::Head(Head { cid: old })
        );

        {
            let next = source.next();
            tokio::pin!(next);
            tokio::select! {
                biased;
                ready = response_ready_rx => ready.unwrap(),
                update = &mut next => panic!("next returned before the cancellation checkpoint: {update:?}"),
            }
        }

        assert_eq!(
            source.next().await.unwrap(),
            Update::Head(Head { cid: new })
        );
        server.await.unwrap();
    }
}
