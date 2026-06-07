//! Integration test: finalizer confirmation depth gates and adopts after K blocks.

mod common;

use atom::{AtomIndexer, FinalizerBuilder, IndexerConfig};
use common::{
    atom_head_http, deploy_atom, eth_block_number, evm_mine, set_head_bytes, spawn_anvil,
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
async fn test_finalizer_confirmation_depth_gates_and_adopts() {
    if let Some(reason) = common::foundry_unavailable_reason() {
        eprintln!("skipping test_finalizer_confirmation_depth_gates_and_adopts: {reason}");
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

    // Sanity: head() right after deploy should be seq=0, cid=b"ipfs-initial"
    let head_after_deploy = atom_head_http(&rpc_url, &contract_address)
        .await
        .expect("head after deploy");
    assert_eq!(head_after_deploy.seq, 0, "seq after deploy");
    assert_eq!(
        head_after_deploy.cid.as_slice(),
        b"ipfs-initial",
        "cid after deploy (got {:?})",
        head_after_deploy.cid
    );

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

    // setHead("cid-1") — raw bytes + eth_sendRawTransaction (in-process EIP-155 signing).
    const CID_1_BYTES: &[u8] = b"cid-1";
    set_head_bytes(
        repo_root,
        &rpc_url,
        &contract_addr,
        "setHead(bytes)",
        CID_1_BYTES,
        None,
    )
    .await
    .expect("setHead");
    let head_after_set = atom_head_http(&rpc_url, &contract_address)
        .await
        .expect("stem head after setHead");
    assert_eq!(head_after_set.seq, 1, "contract head().seq after setHead");
    assert_eq!(
        head_after_set.cid.as_slice(),
        CID_1_BYTES,
        "contract head().cid after setHead (got {:?})",
        head_after_set.cid
    );

    // Wait until an observed event matches (seq, cid); ignore unrelated/replayed events.
    let ev = match timeout(Duration::from_secs(15), async {
        loop {
            let e = recv
                .recv()
                .await
                .map_err(|_| anyhow::anyhow!("recv closed"))?;
            if e.seq == 1 && e.cid.as_slice() == CID_1_BYTES {
                return Ok::<_, anyhow::Error>(e);
            }
        }
    })
    .await
    {
        Ok(Ok(e)) => e,
        Ok(Err(e)) => panic!(
            "indexer recv failed: {} (contract {}, expected seq 1, expected cid {:?})",
            e, contract_addr, CID_1_BYTES
        ),
        Err(_) => panic!(
            "indexer did not observe event: contract {}, expected seq 1, expected cid {:?}",
            contract_addr, CID_1_BYTES
        ),
    };

    let mut finalizer = FinalizerBuilder::new()
        .confirmation_depth(2)
        .http_url(&rpc_url)
        .contract_address(contract_address)
        .build()
        .expect("finalizer build");
    finalizer.feed(ev);

    let tip = finalizer.current_tip().await.expect("current_tip");
    let finalized = finalizer.drain_eligible(tip).await.expect("drain_eligible");
    assert!(
        finalized.is_empty(),
        "expected no finalized events before K blocks"
    );

    evm_mine(&rpc_url, 1).await.expect("evm_mine 1");
    let tip = finalizer.current_tip().await.expect("current_tip");
    let finalized = finalizer.drain_eligible(tip).await.expect("drain_eligible");
    assert!(finalized.is_empty(), "expected still empty after 1 block");

    evm_mine(&rpc_url, 1).await.expect("evm_mine 2");
    let tip = finalizer.current_tip().await.expect("current_tip");
    let finalized = finalizer.drain_eligible(tip).await.expect("drain_eligible");
    assert_eq!(
        finalized.len(),
        1,
        "expected exactly one finalized event after K blocks (tip={})",
        tip
    );
    assert_eq!(finalized[0].seq, 1);
    assert_eq!(
        finalized[0].cid.as_slice(),
        CID_1_BYTES,
        "finalized event cid must match"
    );
}
