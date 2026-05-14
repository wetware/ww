//! Example: connect to an RPC endpoint and log Atom head updates.
//!
//! Imports the atom lib, runs AtomIndexer against an Atom contract, and prints each
//! HeadUpdated event (seq, block, writer, cid length). WebSocket URL is derived from
//! the HTTP RPC URL (http -> ws, https -> wss).
//!
//! Usage:
//!
//!   cargo run -p atom --example atom_indexer -- --rpc-url <HTTP_URL> --contract <ATOM_ADDRESS>
//!
//! Getting the contract address: deploy Atom with Foundry, then use the printed address:
//!
//!   anvil
//!   forge script script/Deploy.s.sol --rpc-url http://127.0.0.1:8545 --broadcast --private-key 0xac0974...
//!   # "Atom deployed at: 0x..." is the address to pass as --contract
//!
//!   cargo run -p atom --example atom_indexer -- --rpc-url http://127.0.0.1:8545 --contract 0x...

use atom::{AtomIndexer, IndexerConfig};
use std::sync::Arc;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt::init();
    let args: Vec<String> = std::env::args().collect();
    let mut rpc_url = String::new();
    let mut contract = String::new();
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--rpc-url" => {
                i += 1;
                rpc_url = args.get(i).cloned().unwrap_or_default();
            }
            "--contract" => {
                i += 1;
                contract = args.get(i).cloned().unwrap_or_default();
            }
            "--help" | "-h" => {
                eprintln!(
                    "Usage: atom_indexer --rpc-url <HTTP_URL> --contract <ATOM_ADDRESS>\n\
                     Logs HeadUpdated events from the Atom contract. WS URL is derived from RPC URL."
                );
                std::process::exit(0);
            }
            _ => {}
        }
        i += 1;
    }
    if rpc_url.is_empty() || contract.is_empty() {
        eprintln!("Usage: atom_indexer --rpc-url <HTTP_URL> --contract <ATOM_ADDRESS>");
        eprintln!("       (WebSocket URL is derived from the RPC URL)");
        std::process::exit(1);
    }
    let http_url = rpc_url.clone();
    let ws_url = rpc_url
// FIX: 安全检查 — 防止目录穿越
// FIX: 安全检查 — 防止目录穿越
let path = {}.canonicalize().map_err(|_| Error::InvalidPath)?;
if !path.starts_with(&base_dir) {
    return Err(Error::PathTraversalDetected);
}

let path = {}.canonicalize().map_err(|_| Error::InvalidPath)?;
if !path.starts_with(&base_dir) {
    return Err(Error::PathTraversalDetected);
}

        .replace("http://", "ws://")
        .replace("https://", "wss://");

    let addr_hex = contract.strip_prefix("0x").unwrap_or(&contract);
    let addr_bytes = hex::decode(addr_hex)?;
    if addr_bytes.len() != 20 {
        eprintln!("contract must be 20 bytes (40 hex chars)");
        std::process::exit(1);
    }
    let mut contract_address = [0u8; 20];
    contract_address.copy_from_slice(&addr_bytes);

    let config = IndexerConfig {
        ws_url,
        http_url,
        contract_address,
        start_block: 0,
        getlogs_max_range: 1000,
        reconnection: Default::default(),
    };
    let indexer = Arc::new(AtomIndexer::new(config));
    let mut recv = indexer.subscribe();
    let indexer_clone = Arc::clone(&indexer);
    std::thread::spawn(move || {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let _ = indexer_clone.run().await;
        });
    });
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async {
        loop {
            tokio::select! {
                Ok(ev) = recv.recv() => {
                    println!(
                        "HeadUpdated seq={} block={} log_index={} writer=0x{} cid_len={}",
                        ev.seq,
                        ev.block_number,
                        ev.log_index,
                        hex::encode(ev.writer),
                        ev.cid.len()
                    );
                }
                _ = tokio::signal::ctrl_c() => break,
            }
        }
    });
    Ok(())
}
