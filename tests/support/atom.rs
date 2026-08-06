use std::io::Read;
use std::net::{SocketAddr, TcpListener};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use reqwest::Client;
use serde_json::{json, Value};

const ANVIL_CHAIN_ID: u64 = 31_337;
const ANVIL_DEFAULT_FROM: &str = "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266";
const ANVIL_DEFAULT_PRIVATE_KEY: &str =
    "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";
const SET_HEAD_BYTES_SELECTOR: [u8; 4] = [0x43, 0xea, 0xe8, 0x23];
const RPC_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Clone, Debug)]
pub struct TransactionReceipt {
    pub hash: String,
    pub block_number: u64,
    pub transaction_index: u64,
}

pub struct AtomFixture {
    anvil: Child,
    _anvil_stdout: tempfile::NamedTempFile,
    _anvil_stderr: tempfile::NamedTempFile,
    _foundry_state: tempfile::TempDir,
    client: Client,
    pub rpc_url: String,
    pub ws_url: String,
    pub contract_address: String,
}

impl AtomFixture {
    pub async fn start(repo_root: &Path) -> Self {
        require_tool("anvil");
        require_tool("forge");

        let address = unused_addr();
        let rpc_url = format!("http://{address}");
        let ws_url = format!("ws://{address}");
        let anvil_stdout = tempfile::NamedTempFile::new().expect("create Anvil stdout capture");
        let anvil_stderr = tempfile::NamedTempFile::new().expect("create Anvil stderr capture");
        let anvil = Command::new("anvil")
            .args([
                "--host",
                "127.0.0.1",
                "--port",
                &address.port().to_string(),
                "--chain-id",
                &ANVIL_CHAIN_ID.to_string(),
                "--silent",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::from(
                anvil_stdout
                    .as_file()
                    .try_clone()
                    .expect("clone Anvil stdout"),
            ))
            .stderr(Stdio::from(
                anvil_stderr
                    .as_file()
                    .try_clone()
                    .expect("clone Anvil stderr"),
            ))
            .spawn()
            .expect("spawn isolated Anvil");

        let client = Client::builder()
            .timeout(Duration::from_secs(2))
            .build()
            .expect("build Anvil client");
        wait_for_rpc(&client, &rpc_url, &anvil_stderr).await;

        let foundry_state = tempfile::tempdir().expect("create isolated Foundry state");
        let out_dir = foundry_state.path().join("out");
        let cache_dir = foundry_state.path().join("cache");
        let broadcast_dir = foundry_state.path().join("broadcast");
        let foundry_root = repo_root.join("contracts/stem");
        let output = Command::new("forge")
            .current_dir(&foundry_root)
            .args([
                "script",
                "script/Deploy.s.sol:Deploy",
                "--rpc-url",
                &rpc_url,
                "--broadcast",
                "--private-key",
                ANVIL_DEFAULT_PRIVATE_KEY,
            ])
            .env("FOUNDRY_OUT", &out_dir)
            .env("FOUNDRY_CACHE_PATH", &cache_dir)
            .env("FOUNDRY_BROADCAST", &broadcast_dir)
            .output()
            .expect("run isolated Atom deployment");
        assert!(
            output.status.success(),
            "forge deployment failed\n--- stdout ---\n{}\n--- stderr ---\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );

        let artifact = broadcast_dir
            .join("Deploy.s.sol")
            .join(ANVIL_CHAIN_ID.to_string())
            .join("run-latest.json");
        let deployment: Value = serde_json::from_slice(
            &std::fs::read(&artifact).unwrap_or_else(|error| {
                panic!(
                    "isolated Foundry broadcast artifact missing at {}: {error}; Foundry must honor FOUNDRY_BROADCAST",
                    artifact.display()
                )
            }),
        )
        .expect("parse isolated Foundry deployment artifact");
        let transactions = deployment["transactions"]
            .as_array()
            .expect("deployment transactions array");
        let create_index = transactions
            .iter()
            .position(|transaction| transaction["transactionType"] == "CREATE")
            .expect("Atom deployment CREATE transaction");
        let contract_address = transactions[create_index]["contractAddress"]
            .as_str()
            .expect("deployed Atom contract address")
            .to_string();
        let receipts = deployment["receipts"]
            .as_array()
            .expect("deployment receipts array");
        let receipt = receipts
            .get(create_index)
            .expect("receipt for Atom CREATE transaction");
        assert_receipt_success(receipt, "Atom deployment");

        Self {
            anvil,
            _anvil_stdout: anvil_stdout,
            _anvil_stderr: anvil_stderr,
            _foundry_state: foundry_state,
            client,
            rpc_url,
            ws_url,
            contract_address,
        }
    }

    pub async fn set_head(&self, cid: &cid::Cid) -> TransactionReceipt {
        let calldata = encode_set_head(&cid.to_bytes());
        let transaction = json!({
            "from": ANVIL_DEFAULT_FROM,
            "to": self.contract_address,
            "data": format!("0x{}", hex::encode(calldata)),
            "value": "0x0",
            "gas": "0x30d40"
        });
        let hash = self
            .rpc("eth_sendTransaction", json!([transaction]))
            .await
            .as_str()
            .expect("eth_sendTransaction hash")
            .to_string();
        self.wait_for_receipt(hash).await
    }

    async fn wait_for_receipt(&self, hash: String) -> TransactionReceipt {
        let deadline = Instant::now() + RPC_TIMEOUT;
        loop {
            let receipt = self.rpc("eth_getTransactionReceipt", json!([hash])).await;
            if !receipt.is_null() {
                assert_receipt_success(&receipt, &format!("Atom transaction {hash}"));
                return TransactionReceipt {
                    hash,
                    block_number: parse_hex_u64(
                        receipt["blockNumber"]
                            .as_str()
                            .expect("receipt blockNumber"),
                    ),
                    transaction_index: parse_hex_u64(
                        receipt["transactionIndex"]
                            .as_str()
                            .expect("receipt transactionIndex"),
                    ),
                };
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for Atom transaction receipt {hash}"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    async fn rpc(&self, method: &str, params: Value) -> Value {
        let response = self
            .client
            .post(&self.rpc_url)
            .json(&json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": method,
                "params": params,
            }))
            .send()
            .await
            .unwrap_or_else(|error| panic!("Anvil RPC {method} failed: {error}"))
            .error_for_status()
            .unwrap_or_else(|error| panic!("Anvil RPC {method} HTTP failure: {error}"))
            .json::<Value>()
            .await
            .unwrap_or_else(|error| panic!("parse Anvil RPC {method}: {error}"));
        assert!(
            response.get("error").is_none(),
            "Anvil RPC {method} returned error: {response}"
        );
        response
            .get("result")
            .cloned()
            .unwrap_or_else(|| panic!("Anvil RPC {method} omitted result: {response}"))
    }
}

impl Drop for AtomFixture {
    fn drop(&mut self) {
        if matches!(self.anvil.try_wait(), Ok(None)) {
            let _ = self.anvil.kill();
            let _ = self.anvil.wait();
        }
    }
}

fn require_tool(name: &str) {
    let status = Command::new(name).arg("--version").output();
    assert!(
        status.is_ok_and(|output| output.status.success()),
        "{name} is required for pid0 epoch E2E tests; CI must install the pinned Foundry toolchain"
    );
}

fn unused_addr() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral Anvil port");
    let address = listener.local_addr().expect("read Anvil port");
    drop(listener);
    address
}

async fn wait_for_rpc(client: &Client, rpc_url: &str, stderr: &tempfile::NamedTempFile) {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let ready = client
            .post(rpc_url)
            .json(&json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "eth_chainId",
                "params": [],
            }))
            .send()
            .await
            .is_ok_and(|response| response.status().is_success());
        if ready {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "Anvil did not become ready at {rpc_url}\n{}",
            read_capture(stderr)
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

fn encode_set_head(cid_bytes: &[u8]) -> Vec<u8> {
    let mut encoded = Vec::with_capacity(68 + cid_bytes.len().div_ceil(32) * 32);
    encoded.extend_from_slice(&SET_HEAD_BYTES_SELECTOR);
    encoded.extend_from_slice(&[0; 28]);
    encoded.extend_from_slice(&32_u32.to_be_bytes());
    encoded.extend_from_slice(&[0; 28]);
    encoded.extend_from_slice(&(cid_bytes.len() as u32).to_be_bytes());
    encoded.extend_from_slice(cid_bytes);
    encoded.resize(68 + cid_bytes.len().div_ceil(32) * 32, 0);
    encoded
}

fn assert_receipt_success(receipt: &Value, operation: &str) {
    let status = receipt["status"]
        .as_str()
        .unwrap_or_else(|| panic!("{operation} receipt omitted status: {receipt}"));
    assert_eq!(
        parse_hex_u64(status),
        1,
        "{operation} transaction failed: {receipt}"
    );
}

fn parse_hex_u64(value: &str) -> u64 {
    u64::from_str_radix(value.strip_prefix("0x").unwrap_or(value), 16)
        .unwrap_or_else(|error| panic!("parse hexadecimal integer {value}: {error}"))
}

fn read_capture(file: &tempfile::NamedTempFile) -> String {
    let mut handle = file.reopen().expect("reopen capture");
    let mut output = String::new();
    handle.read_to_string(&mut output).expect("read capture");
    output
}
