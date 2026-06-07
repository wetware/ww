//! Integration test: indexer can observe a reorg'd event but finalizer filters it (canonical cross-check).

mod common;

use atom::{AtomIndexer, FinalizerBuilder, IndexerConfig};
use common::{
    atom_head_http, deploy_atom, eth_block_number, evm_mine, evm_revert, evm_snapshot,
    set_head_bytes, spawn_anvil,
};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::timeout;
use tracing_subscriber::EnvFilter;

/// Ensures indexer task and Anvil process are cleaned up even on panic (best effort for CI).
struct CleanupGuard {
    task: Option<tokio::task::JoinHandle<()>>,
    process: Option<std::process::Child>,
}

impl Drop for CleanupGuard {
    fn drop(&mut self) {
        if let Some(t) = self.task.take() {
            t.abort();
        }
        if let Some(p) = self.process.as_mut() {
            let _ = p.kill();
        }
    }
}

#[tokio::test]
async fn test_reorg_indexer_false_positive_finalizer_filters() {
    if let Some(reason) = common::foundry_unavailable_reason() {
        eprintln!("skipping test_reorg_indexer_false_positive_finalizer_filters: {reason}");
        return;
    }
    let _ = tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env().add_directive("atom=debug".parse().unwrap()))
        .with_test_writer()
        .try_init();

    // CARGO_MANIFEST_DIR is crates/atom, so ancestors().nth(2) is repo root (script/, broadcast/).
    let repo_root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .unwrap();
    let (anvil_process, rpc_url) = spawn_anvil().await.expect("spawn anvil");
    let contract_addr = deploy_atom(repo_root, &rpc_url).expect("deploy Atom");
    let addr_bytes =
        hex::decode(contract_addr.strip_prefix("0x").unwrap_or(&contract_addr)).expect("hex");
    let mut contract_address = [0u8; 20];
    contract_address.copy_from_slice(&addr_bytes);

    let current_block = eth_block_number(&rpc_url).await.expect("eth_block_number");
    let ws_url = rpc_url
        .replace("http://", "ws://")
        .replace("https://", "wss://");
    let config = IndexerConfig {
        ws_url: ws_url.clone(),
        http_url: rpc_url.clone(),
        contract_address,
        start_block: current_block,
        getlogs_max_range: 1000,
        reconnection: Default::default(),
    };
    let indexer = Arc::new(AtomIndexer::new(config));
    let mut recv = indexer.subscribe();
    let indexer_clone = Arc::clone(&indexer);
    let indexer_task = tokio::spawn(async move {
        let _ = indexer_clone.run().await;
    });
    let _guard = CleanupGuard {
        task: Some(indexer_task),
        process: Some(anvil_process),
    };

    let mut finalizer = FinalizerBuilder::new()
        .confirmation_depth(2)
        .http_url(&rpc_url)
        .contract_address(contract_address)
        .build()
        .expect("finalizer build");

    let snap = evm_snapshot(&rpc_url).await.expect("evm_snapshot");

    // setHead("cid-reorg") after indexer is running (indexer sees event via subscription/backfill of new blocks).
    const CID_REORG_BYTES: &[u8] = b"cid-reorg";
    set_head_bytes(
        repo_root,
        &rpc_url,
        &contract_addr,
        "setHead(bytes)",
        CID_REORG_BYTES,
        None,
    )
    .await
    .expect("setHead");

    // Wait until an observed event matches (seq, cid); ignore unrelated/replayed events.
    let ev = match timeout(Duration::from_secs(10), async {
        loop {
            let e = recv
                .recv()
                .await
                .map_err(|_| anyhow::anyhow!("recv closed"))?;
            if e.seq == 1 && e.cid.as_slice() == CID_REORG_BYTES {
                return Ok::<_, anyhow::Error>(e);
            }
        }
    })
    .await
    {
        Ok(Ok(e)) => e,
        Ok(Err(e)) => panic!(
            "indexer recv failed: {} (contract {}, expected seq 1, expected cid {:?})",
            e, contract_addr, CID_REORG_BYTES
        ),
        Err(_) => panic!(
            "indexer did not observe event: contract {}, expected seq 1, expected cid {:?}",
            contract_addr, CID_REORG_BYTES
        ),
    };

    finalizer.feed(ev);

    let reverted = evm_revert(&rpc_url, &snap).await.expect("evm_revert");
    assert!(reverted, "evm_revert should return true");

    evm_mine(&rpc_url, 3).await.expect("evm_mine 3");

    let tip = finalizer.current_tip().await.expect("current_tip");
    let finalized = finalizer.drain_eligible(tip).await.expect("drain_eligible");
    assert!(
        finalized.is_empty(),
        "finalizer must not emit reorg'd event: got {} finalized",
        finalized.len()
    );

    let head = atom_head_http(&rpc_url, &contract_address)
        .await
        .expect("stem head()");
    assert_eq!(head.seq, 0, "canonical head must be seq 0 after revert");
}
