//! Reorg-naive integration test.
//!
//! Proves that our current event-handling/indexing is NOT reorg-safe (by design).
//! A naive observer sees a HeadUpdated event and advances its state; we then revert
//! the chain (evm_revert) so that event is no longer on the canonical chain. The
//! naive observer does NOT undo its state, so applied_head disagrees with canonical
//! head/logs. This mismatch is EXPECTED until reorg safety is implemented; the test
//! protects against mistakenly assuming we are reorg-safe.

mod common;

use anyhow::{Context, Result};
use atom::abi::{
    decode_head_return, decode_log_to_observed, CurrentHead, HEAD_SELECTOR, HEAD_UPDATED_TOPIC0,
};
use common::{deploy_atom, evm_revert, evm_snapshot, set_head, spawn_anvil};
use serde_json::{json, Value};
use std::path::Path;

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
        .context("HTTP request")?;
    let v: Value = resp.json().await.context("parse response")?;
    if let Some(err) = v.get("error") {
        anyhow::bail!("RPC error: {}", err);
    }
    v.get("result")
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("Missing result"))
}

async fn eth_get_logs(
    client: &reqwest::Client,
    http_url: &str,
    contract_address: &[u8; 20],
    from_block: u64,
    to_block: u64,
) -> Result<Vec<Value>> {
    // Address-only filter (Anvil may reject topic filter); we filter for HeadUpdated client-side.
    let filter = json!({
        "address": format!("0x{}", hex::encode(contract_address)),
        "fromBlock": format!("0x{:x}", from_block),
        "toBlock": format!("0x{:x}", to_block),
    });
    let result = http_json_rpc(client, http_url, "eth_getLogs", json!([filter]), 12).await?;
    let arr = result
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("getLogs not array"))?;
    let logs: Vec<Value> = arr
        .iter()
        .filter(|log| {
            log.get("topics")
                .and_then(|t| t.as_array())
                .and_then(|t| t.first())
                .and_then(|t| t.as_str())
                .and_then(|s| hex::decode(s.strip_prefix("0x").unwrap_or(s)).ok())
                .map(|b| b.len() >= 4 && b[..4] == HEAD_UPDATED_TOPIC0)
                .unwrap_or(false)
        })
        .cloned()
        .collect();
    Ok(logs)
}

async fn atom_head(
    client: &reqwest::Client,
    http_url: &str,
    contract_address: &[u8; 20],
) -> Result<CurrentHead> {
    let params = json!([{
        "to": format!("0x{}", hex::encode(contract_address)),
        "data": format!("0x{}", hex::encode(HEAD_SELECTOR)),
    }, "latest"]);
    let result = http_json_rpc(client, http_url, "eth_call", params, 13).await?;
    let s = result
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("eth_call result not string"))?;
    let bytes = hex::decode(s.strip_prefix("0x").unwrap_or(s)).context("decode eth_call")?;
    decode_head_return(&bytes).context("decode head()")
}

#[tokio::test]
async fn test_reorg_naive_observer_mismatch() {
    if let Some(reason) = common::foundry_unavailable_reason() {
        eprintln!("skipping test_reorg_naive_observer_mismatch: {reason}");
        return;
    }
    let _ = tracing_subscriber::fmt().with_test_writer().try_init();

    let repo_root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .unwrap();
    let (mut anvil_process, rpc_url) = spawn_anvil().await.expect("spawn anvil");
    let contract_addr = deploy_atom(repo_root, &rpc_url).expect("deploy Atom");
    let addr_bytes =
        hex::decode(contract_addr.strip_prefix("0x").unwrap_or(&contract_addr)).expect("hex");
    let mut contract_address = [0u8; 20];
    contract_address.copy_from_slice(&addr_bytes);

    let client = reqwest::Client::builder()
        .no_proxy()
        .build()
        .expect("reqwest client");

    // Snapshot state after deploy (seq 0, initial cid). We will revert to this later.
    let snapshot_id = evm_snapshot(&rpc_url).await.expect("evm_snapshot");

    // Emit HeadUpdated: setHead("cid-1")
    let cid_hex = "0x6369642d31"; // "cid-1" in hex
    set_head(
        repo_root,
        &rpc_url,
        &contract_addr,
        "setHead(bytes)",
        cid_hex,
        None,
    )
    .expect("setHead");

    // Naive observation: poll eth_getLogs, decode, update in-memory applied_head (no reorg safety).
    let from_block = 1u64;
    let to_block = 10u64; // enough for one tx
    let logs = eth_get_logs(&client, &rpc_url, &contract_address, from_block, to_block)
        .await
        .expect("eth_getLogs");
    let observed: Vec<_> = logs
        .iter()
        .filter_map(|log| decode_log_to_observed(log).ok())
        .collect();
    assert!(
        !observed.is_empty(),
        "expected at least one HeadUpdated log"
    );
    let observed_event = &observed[0];
    let applied_head = CurrentHead {
        seq: observed_event.seq,
        cid: observed_event.cid.clone(),
    };
    let observed_tx_hash = observed_event.tx_hash;
    assert_eq!(applied_head.seq, 1);
    assert_eq!(applied_head.cid.as_slice(), b"cid-1");

    // Revert chain so the setHead block is no longer canonical (simulates reorg).
    let reverted = evm_revert(&rpc_url, &snapshot_id)
        .await
        .expect("evm_revert");
    assert!(reverted, "evm_revert should return true");

    // Post-revert canonical checks: eth_getLogs should not contain our tx; stem.head() should be initial.
    let logs_after = eth_get_logs(&client, &rpc_url, &contract_address, from_block, to_block)
        .await
        .expect("eth_getLogs after revert");
    let has_our_tx = logs_after.iter().any(|log| {
        log.get("transactionHash")
            .and_then(|h| h.as_str())
            .and_then(|h| hex::decode(h.strip_prefix("0x").unwrap_or(h)).ok())
            .as_ref()
            .map(|b| b.as_slice() == observed_tx_hash)
            .unwrap_or(false)
    });
    assert!(
        !has_our_tx,
        "canonical logs must not contain the reverted setHead tx"
    );

    let canonical_head = atom_head(&client, &rpc_url, &contract_address)
        .await
        .expect("atom head()");
    assert_eq!(
        canonical_head.seq, 0,
        "canonical chain must no longer reflect the reverted setHead"
    );
    // After evm_revert, canonical head is back to initial state (seq 0); cid may be initial or empty depending on node.

    // Expected limitation: naive observer still has the orphaned applied_head (no rollback).
    assert_eq!(applied_head.seq, 1);
    assert_eq!(applied_head.cid.as_slice(), b"cid-1");
    assert_ne!(applied_head.seq, canonical_head.seq);
    assert_ne!(applied_head.cid, canonical_head.cid);

    let _ = anvil_process.kill();
}
