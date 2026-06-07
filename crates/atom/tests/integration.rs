//! Integration test: Anvil + deploy Atom + setHead + AtomIndexer.

mod common;

use atom::{AtomIndexer, IndexerConfig};
use common::{deploy_atom, set_head, spawn_anvil};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::timeout;
use tracing_subscriber::EnvFilter;

#[tokio::test]
async fn test_indexer_against_anvil() {
    if let Some(reason) = common::foundry_unavailable_reason() {
        eprintln!("skipping test_indexer_against_anvil: {reason}");
        return;
    }
    let _ = tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env().add_directive("atom=debug".parse().unwrap()))
        .with_test_writer()
        .try_init();

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

    set_head(
        repo_root,
        &rpc_url,
        &contract_addr,
        "setHead(bytes)",
        "0x697066732f2f6669727374",
        None,
    )
    .expect("setHead 1");
    set_head(
        repo_root,
        &rpc_url,
        &contract_addr,
        "setHead(bytes)",
        "0x69706c642f2f7365636f6e64",
        None,
    )
    .expect("setHead 2");
    set_head(
        repo_root,
        &rpc_url,
        &contract_addr,
        "setHead(bytes)",
        "0x626c6f622f2f7468697264",
        None,
    )
    .expect("setHead 3");

    let ws_url = rpc_url
        .replace("http://", "ws://")
        .replace("https://", "wss://");
    let config = IndexerConfig {
        ws_url: ws_url.clone(),
        http_url: rpc_url.clone(),
        contract_address,
        start_block: 0,
        getlogs_max_range: 1000,
        reconnection: Default::default(),
    };
    let indexer = Arc::new(AtomIndexer::new(config));
    let mut recv = indexer.subscribe();
    let indexer_clone = Arc::clone(&indexer);
    let task = tokio::spawn(async move {
        let _ = indexer_clone.run().await;
    });

    let mut events = Vec::new();
    let _ = timeout(Duration::from_secs(15), async {
        while events.len() < 3 {
            if let Ok(ev) = recv.recv().await {
                events.push(ev);
            }
        }
    })
    .await;

    task.abort();
    let _ = anvil_process.kill();

    assert!(
        events.len() >= 3,
        "expected at least 3 events, got {}",
        events.len()
    );
    for (i, ev) in events.iter().take(3).enumerate() {
        assert_eq!(ev.seq, (i + 1) as u64);
    }
    for w in events.windows(2) {
        let (a, b) = (&w[0], &w[1]);
        assert!(
            (a.block_number, a.log_index) < (b.block_number, b.log_index),
            "ordering: ({},{}) < ({},{})",
            a.block_number,
            a.log_index,
            b.block_number,
            b.log_index
        );
    }
}
