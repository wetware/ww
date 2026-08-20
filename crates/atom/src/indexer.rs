//! AtomIndexer: observed-only indexing of Atom HeadUpdated events.
//!
//! Subscribes via WebSocket, backfills via HTTP on startup/reconnect, maintains
//! in-memory cursor and current HEAD. No reorg safety or confirmations in the indexer
//! itself. The host's Atom Stem adapter derives authoritative state by polling
//! `Atom.head()` at finalized chain depth.

use crate::abi::{decode_log_to_observed, CurrentHead, HeadUpdatedObserved, HEAD_UPDATED_TOPIC0};
use crate::config::IndexerConfig;
use crate::cursor::Cursor;
use anyhow::{Context, Result};
use futures_util::{SinkExt, StreamExt};
use rand::Rng;
use serde_json::{json, Value};
use std::sync::Arc;
use tokio::sync::{broadcast, RwLock};
use tokio::time::{sleep, timeout, Duration};
use tokio_tungstenite::{connect_async, tungstenite::Message};

fn build_logs_filter(
    address: &[u8; 20],
    topic0: Option<&[u8; 4]>,
    from_block: Option<u64>,
    to_block: Option<u64>,
) -> Value {
    let mut filter = json!({
        "address": format!("0x{}", hex::encode(address)),
    });
    // Single-topic filter: [topic0] only (some nodes reject [topic0, null, null, null]).
    if let Some(t0) = topic0 {
        filter["topics"] = json!([format!("0x{}", hex::encode(t0))]);
    }
    if let Some(from) = from_block {
        filter["fromBlock"] = Value::String(format!("0x{:x}", from));
    }
    if let Some(to) = to_block {
        filter["toBlock"] = Value::String(format!("0x{:x}", to));
    }
    filter
}

/// Build address-only filter (no topics) for fallback when node rejects topic filter.
fn build_logs_filter_address_only(
    address: &[u8; 20],
    from_block: Option<u64>,
    to_block: Option<u64>,
) -> Value {
    let mut filter = json!({
        "address": format!("0x{}", hex::encode(address)),
    });
    if let Some(from) = from_block {
        filter["fromBlock"] = Value::String(format!("0x{:x}", from));
    }
    if let Some(to) = to_block {
        filter["toBlock"] = Value::String(format!("0x{:x}", to));
    }
    filter
}

async fn http_json_rpc(
    client: &reqwest::Client,
    url: &str,
    method: &str,
    params: Value,
    id: u64,
) -> Result<Value> {
    let body = json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": method,
        "params": params
    });
    let resp = client
        .post(url)
        .json(&body)
        .send()
        .await
        .context("HTTP request failed")?;
    let json: Value = resp.json().await.context("parse response")?;
    if let Some(err) = json.get("error") {
        anyhow::bail!("RPC error: {}", err);
    }
    let result = json
        .get("result")
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("Missing result"))?;
    Ok(result)
}

async fn eth_block_number(client: &reqwest::Client, http_url: &str) -> Result<u64> {
    let result = http_json_rpc(client, http_url, "eth_blockNumber", json!([]), 1).await?;
    let s = result
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("blockNumber not string"))?;
    let s = s.strip_prefix("0x").unwrap_or(s);
    u64::from_str_radix(s, 16).context("parse block number")
}

/// Returns the current chain tip (latest block number) via JSON-RPC eth_blockNumber.
/// Useful for starting an indexer from "now" (live-only, no backfill of older blocks).
pub async fn current_block_number(http_url: &str) -> Result<u64> {
    let client = reqwest::Client::builder()
        .no_proxy()
        .build()
        .expect("reqwest client");
    eth_block_number(&client, http_url).await
}

async fn eth_get_logs(
    client: &reqwest::Client,
    http_url: &str,
    filter: Value,
) -> Result<Vec<Value>> {
    let result = http_json_rpc(client, http_url, "eth_getLogs", json!([filter]), 2).await?;
    let arr = result
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("getLogs not array"))?;
    Ok(arr.clone())
}

/// Atom indexer: follows HeadUpdated logs, backfills via HTTP, maintains current HEAD.
pub struct AtomIndexer {
    config: IndexerConfig,
    event_tx: broadcast::Sender<HeadUpdatedObserved>,
    current_head: Arc<RwLock<Option<CurrentHead>>>,
}

impl AtomIndexer {
    pub fn new(config: IndexerConfig) -> Self {
        let (event_tx, _) = broadcast::channel(256);
        Self {
            config,
            event_tx,
            current_head: Arc::new(RwLock::new(None)),
        }
    }

    /// Subscribe to observed HeadUpdated events (ordered by block_number, log_index).
    pub fn subscribe(&self) -> broadcast::Receiver<HeadUpdatedObserved> {
        self.event_tx.subscribe()
    }

    /// Current HEAD (from head() or latest event). None until first update.
    pub async fn current_head(&self) -> Option<CurrentHead> {
        self.current_head.read().await.clone()
    }

    /// Run the indexer (blocking on the async loop). Call from a spawned task.
    pub async fn run(self: Arc<Self>) -> Result<()> {
        let config = &self.config;
        let http_client = reqwest::Client::builder()
            .no_proxy()
            .build()
            .expect("reqwest client");
        let mut cursor = Cursor::new(config.start_block.saturating_sub(1));
        let reconnection = config.reconnection.clone();

        loop {
            match run_once(Arc::clone(&self), &http_client, &mut cursor, config).await {
                Ok(()) => {
                    sleep(Duration::from_secs(reconnection.initial_backoff_secs)).await;
                }
                Err(e) => {
                    tracing::warn!(reason = %e, "AtomIndexer failed, reconnecting...");
                    let base = std::cmp::min(
                        Duration::from_secs(reconnection.initial_backoff_secs) * 2,
                        Duration::from_secs(reconnection.max_backoff_secs),
                    );
                    let jitter = Duration::from_millis(rand::thread_rng().gen_range(0..500));
                    sleep(base + jitter).await;
                }
            }
        }
    }
}

async fn run_once(
    indexer: Arc<AtomIndexer>,
    http_client: &reqwest::Client,
    cursor: &mut Cursor,
    config: &IndexerConfig,
) -> Result<()> {
    // Subscribe first so WS buffers events while backfill runs (no missed-event gap).
    let ws_url = &config.ws_url;
    let (ws_stream, _) = connect_async(ws_url).await.context("WS connect")?;
    let (mut ws_sender, mut ws_receiver) = ws_stream.split();

    let logs_id = 1u64;
    let filter = build_logs_filter(
        &config.contract_address,
        Some(&HEAD_UPDATED_TOPIC0),
        None,
        None,
    );
    let sub_req = json!({
        "jsonrpc": "2.0",
        "id": logs_id,
        "method": "eth_subscribe",
        "params": ["logs", filter]
    });
    ws_sender
        .send(Message::Text(serde_json::to_string(&sub_req)?))
        .await
        .map_err(|e| anyhow::anyhow!("send subscribe: {}", e))?;

    let (sub_id, needs_client_filter) =
        match timeout(Duration::from_secs(10), ws_receiver.next()).await {
            Ok(Some(Ok(Message::Text(text)))) => {
                let v: Value = serde_json::from_str(&text).context("parse sub response")?;
                if v.get("error").is_some() {
                    let err = v["error"]
                        .get("message")
                        .and_then(|m| m.as_str())
                        .unwrap_or("");
                    if err.contains("data did not match") || err.contains("variant") {
                        tracing::warn!(
                            "RPC does not support logs filter (Anvil?), using client-side filter"
                        );
                        let sub_req_no_filter = json!({
                            "jsonrpc": "2.0",
                            "id": logs_id,
                            "method": "eth_subscribe",
                            "params": ["logs"]
                        });
                        ws_sender
                            .send(Message::Text(serde_json::to_string(&sub_req_no_filter)?))
                            .await
                            .map_err(|e| anyhow::anyhow!("send subscribe: {}", e))?;
                        let text2 = timeout(Duration::from_secs(10), ws_receiver.next())
                            .await
                            .map_err(|_| anyhow::anyhow!("subscribe timeout"))?
                            .ok_or_else(|| anyhow::anyhow!("ws closed"))?
                            .map_err(|e| anyhow::anyhow!("ws: {}", e))?;
                        let msg = match text2 {
                            Message::Text(t) => t,
                            _ => anyhow::bail!("expected text"),
                        };
                        let v2: Value = serde_json::from_str(&msg)?;
                        let id = v2["result"]
                            .as_str()
                            .ok_or_else(|| anyhow::anyhow!("no sub id"))?
                            .to_string();
                        (id, true)
                    } else {
                        anyhow::bail!("subscribe error: {}", err);
                    }
                } else {
                    let id = v["result"]
                        .as_str()
                        .ok_or_else(|| anyhow::anyhow!("no result"))?
                        .to_string();
                    (id, false)
                }
            }
            Ok(Some(Ok(_))) => anyhow::bail!("unexpected message"),
            Ok(Some(Err(e))) => return Err(anyhow::anyhow!("ws: {}", e)),
            Ok(None) => anyhow::bail!("ws closed"),
            Err(_) => anyhow::bail!("subscribe timeout"),
        };
    let _ = sub_id;

    // Backfill after subscribe so the WS stream buffers any events arriving in between.
    let from_block = cursor.last_processed_block + 1;
    let tip = eth_block_number(http_client, &config.http_url).await?;
    if from_block <= tip {
        backfill(
            http_client,
            &config.http_url,
            &config.contract_address,
            from_block,
            tip,
            config.getlogs_max_range,
            &indexer.event_tx,
            &indexer.current_head,
        )
        .await?;
        cursor.last_processed_block = tip;
    }

    while let Some(msg) = ws_receiver.next().await {
        let text = match msg.map_err(|e| anyhow::anyhow!("ws: {}", e))? {
            Message::Text(t) => t,
            Message::Close(_) => break,
            _ => continue,
        };
        let v: Value = serde_json::from_str(&text).context("parse ws message")?;
        if v.get("method").and_then(|m| m.as_str()) != Some("eth_subscription") {
            continue;
        }
        let result = v
            .get("params")
            .and_then(|p| p.get("result"))
            .ok_or_else(|| anyhow::anyhow!("no params.result"))?;
        if needs_client_filter {
            let addr = match result.get("address").and_then(|a| a.as_str()) {
                Some(a) => a,
                None => continue,
            };
            let addr_bytes = match hex::decode(addr.strip_prefix("0x").unwrap_or(addr)) {
                Ok(b) if b.len() == 20 => b,
                _ => continue,
            };
            let mut addr_20 = [0u8; 20];
            addr_20.copy_from_slice(&addr_bytes);
            if addr_20 != config.contract_address {
                continue;
            }
            let topics = result.get("topics").and_then(|t| t.as_array());
            let topic0 = match topics.and_then(|t| t.first()).and_then(|t| t.as_str()) {
                Some(s) => hex::decode(s.strip_prefix("0x").unwrap_or(s)).ok(),
                _ => continue,
            };
            let topic0_4 = match topic0.as_ref().filter(|b| b.len() >= 4) {
                Some(b) => [b[0], b[1], b[2], b[3]],
                _ => continue,
            };
            if topic0_4 != HEAD_UPDATED_TOPIC0 {
                continue;
            }
        }
        let observed = decode_log_to_observed(result).context("decode log")?;
        cursor.last_processed_block = cursor.last_processed_block.max(observed.block_number);
        let _ = indexer.event_tx.send(observed.clone());
        set_current_head_if_newer(
            &indexer.current_head,
            CurrentHead {
                seq: observed.seq,
                cid: observed.cid,
            },
        )
        .await;
    }
    Ok(())
}

fn log_matches_head_updated(log: &Value) -> bool {
    let topics = match log.get("topics").and_then(|t| t.as_array()) {
        Some(t) if !t.is_empty() => t,
        _ => return false,
    };
    let t0 = match topics[0].as_str() {
        Some(s) => s,
        None => return false,
    };
    let bytes = match hex::decode(t0.strip_prefix("0x").unwrap_or(t0)) {
        Ok(b) if b.len() >= 4 => b,
        _ => return false,
    };
    bytes[..4] == HEAD_UPDATED_TOPIC0
}

#[allow(clippy::too_many_arguments)]
async fn backfill(
    client: &reqwest::Client,
    http_url: &str,
    contract_address: &[u8; 20],
    from_block: u64,
    to_block: u64,
    max_range: u64,
    event_tx: &broadcast::Sender<HeadUpdatedObserved>,
    current_head: &Arc<RwLock<Option<CurrentHead>>>,
) -> Result<()> {
    let mut from = from_block;
    while from <= to_block {
        let to = (from + max_range - 1).min(to_block);
        let filter = build_logs_filter(
            contract_address,
            Some(&HEAD_UPDATED_TOPIC0),
            Some(from),
            Some(to),
        );
        let logs = match eth_get_logs(client, http_url, filter).await {
            Ok(l) => l,
            Err(e) => {
                tracing::debug!(reason = %e, "eth_getLogs with topic filter failed, trying address-only");
                let fallback =
                    build_logs_filter_address_only(contract_address, Some(from), Some(to));
                let raw = eth_get_logs(client, http_url, fallback).await?;
                raw.into_iter()
                    .filter(log_matches_head_updated)
                    .collect::<Vec<_>>()
            }
        };
        // If topic filter returned empty, try address-only (some nodes ignore topic filter and return []).
        let logs = if logs.is_empty() {
            let fallback = build_logs_filter_address_only(contract_address, Some(from), Some(to));
            match eth_get_logs(client, http_url, fallback).await {
                Ok(raw) => raw
                    .into_iter()
                    .filter(log_matches_head_updated)
                    .collect::<Vec<_>>(),
                Err(_) => logs,
            }
        } else {
            logs
        };
        let mut observed: Vec<HeadUpdatedObserved> = logs
            .iter()
            .filter_map(|log| {
                decode_log_to_observed(log)
                    .map_err(|e| tracing::debug!(%e, "decode log skipped"))
                    .ok()
            })
            .collect();
        if !logs.is_empty() && observed.is_empty() {
            tracing::warn!(
                raw_count = logs.len(),
                from,
                to,
                "backfill: logs received but none decoded"
            );
        } else if !observed.is_empty() {
            tracing::debug!(count = observed.len(), from, to, "backfill: decoded events");
        }
        observed.sort_by_key(|o| (o.block_number, o.log_index));
        for o in observed {
            let _ = event_tx.send(o.clone());
            set_current_head_if_newer(
                current_head,
                CurrentHead {
                    seq: o.seq,
                    cid: o.cid,
                },
            )
            .await;
        }
        from = to + 1;
    }
    Ok(())
}

async fn set_current_head_if_newer(
    current_head: &Arc<RwLock<Option<CurrentHead>>>,
    new: CurrentHead,
) {
    let mut guard = current_head.write().await;
    let should_set = guard.as_ref().map(|h| new.seq >= h.seq).unwrap_or(true);
    if should_set {
        tracing::info!(seq = new.seq, "current HEAD updated");
        *guard = Some(new);
    }
}
