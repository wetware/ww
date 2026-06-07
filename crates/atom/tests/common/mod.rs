//! Common helpers for integration tests.
//! Some helpers are only used by specific test binaries; allow dead_code to avoid per-binary warnings.
#![allow(dead_code)]

use anyhow::{Context, Result};
use capnp::capability::Promise;
use capnp_rpc::pry;
use serde_json::{json, Value};
use std::process::{Child, Command, Stdio};
use std::time::Duration;
use tokio::time::sleep;

use atom::auth_capnp;
use atom::system_capnp;
use atom::{EpochGuard, GraftBuilder};
use membrane::http_capnp;
use membrane::routing_capnp;

// ---------------------------------------------------------------------------
// Stub runtime + executor + session builder for epoch-guarded capability tests
// ---------------------------------------------------------------------------

/// Minimal runtime that checks epoch guard on load/shutdown.
/// Used by membrane integration tests to verify epoch-staleness semantics.
pub struct StubRuntime {
    guard: EpochGuard,
}

#[allow(refining_impl_trait)]
impl system_capnp::runtime::Server for StubRuntime {
    fn load(
        self: capnp::capability::Rc<Self>,
        _params: system_capnp::runtime::LoadParams,
        mut results: system_capnp::runtime::LoadResults,
    ) -> Promise<(), capnp::Error> {
        pry!(self.guard.check());
        results
            .get()
            .set_executor(capnp_rpc::new_client(StubExecutor));
        Promise::ok(())
    }

    fn shutdown(
        self: capnp::capability::Rc<Self>,
        _params: system_capnp::runtime::ShutdownParams,
        _results: system_capnp::runtime::ShutdownResults,
    ) -> Promise<(), capnp::Error> {
        pry!(self.guard.check());
        Promise::ok(())
    }
}

/// Minimal executor stub: spawn returns unimplemented (no real WASM in tests).
pub struct StubExecutor;

#[allow(refining_impl_trait)]
impl system_capnp::executor::Server for StubExecutor {
    fn spawn(
        self: capnp::capability::Rc<Self>,
        _params: system_capnp::executor::SpawnParams,
        _results: system_capnp::executor::SpawnResults,
    ) -> Promise<(), capnp::Error> {
        Promise::err(capnp::Error::unimplemented("stub".into()))
    }
}

/// GraftBuilder that populates graft results with a StubRuntime (Export list format).
pub struct StubSessionBuilder;

impl GraftBuilder for StubSessionBuilder {
    fn build(
        &self,
        guard: &EpochGuard,
        mut builder: atom::membrane_capnp::membrane::graft_results::Builder<'_>,
    ) -> std::result::Result<(), capnp::Error> {
        let runtime: system_capnp::runtime::Client = capnp_rpc::new_client(StubRuntime {
            guard: guard.clone(),
        });

        let mut caps = builder.reborrow().init_caps(1);
        let mut entry = caps.reborrow().get(0);
        entry.set_name("runtime");
        entry.reborrow().init_schema(); // Phase 1: default (empty) Schema.Node
        entry.init_cap().set_as_capability(runtime.client.hook);
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Stub servers for all 5 graft capabilities (Identity, Host, Runtime, Routing, HttpClient)
// ---------------------------------------------------------------------------

/// Stub Identity: returns unimplemented for all methods.
pub struct StubIdentity;

#[allow(refining_impl_trait)]
impl auth_capnp::identity::Server for StubIdentity {
    fn signer(
        self: capnp::capability::Rc<Self>,
        _params: auth_capnp::identity::SignerParams,
        _results: auth_capnp::identity::SignerResults,
    ) -> Promise<(), capnp::Error> {
        Promise::err(capnp::Error::unimplemented("stub identity".into()))
    }

    fn verify(
        self: capnp::capability::Rc<Self>,
        _params: auth_capnp::identity::VerifyParams,
        _results: auth_capnp::identity::VerifyResults,
    ) -> Promise<(), capnp::Error> {
        Promise::err(capnp::Error::unimplemented("stub identity".into()))
    }
}

/// Stub Host: returns unimplemented for all methods.
pub struct StubHost;

#[allow(refining_impl_trait)]
impl system_capnp::host::Server for StubHost {
    fn id(
        self: capnp::capability::Rc<Self>,
        _params: system_capnp::host::IdParams,
        _results: system_capnp::host::IdResults,
    ) -> Promise<(), capnp::Error> {
        Promise::err(capnp::Error::unimplemented("stub host".into()))
    }

    fn addrs(
        self: capnp::capability::Rc<Self>,
        _params: system_capnp::host::AddrsParams,
        _results: system_capnp::host::AddrsResults,
    ) -> Promise<(), capnp::Error> {
        Promise::err(capnp::Error::unimplemented("stub host".into()))
    }

    fn peers(
        self: capnp::capability::Rc<Self>,
        _params: system_capnp::host::PeersParams,
        _results: system_capnp::host::PeersResults,
    ) -> Promise<(), capnp::Error> {
        Promise::err(capnp::Error::unimplemented("stub host".into()))
    }

    fn network(
        self: capnp::capability::Rc<Self>,
        _params: system_capnp::host::NetworkParams,
        _results: system_capnp::host::NetworkResults,
    ) -> Promise<(), capnp::Error> {
        Promise::err(capnp::Error::unimplemented("stub host".into()))
    }
}

/// Stub HttpClient: returns unimplemented for all methods.
pub struct StubHttpClient;

#[allow(refining_impl_trait)]
impl http_capnp::http_client::Server for StubHttpClient {
    fn get(
        self: capnp::capability::Rc<Self>,
        _params: http_capnp::http_client::GetParams,
        _results: http_capnp::http_client::GetResults,
    ) -> Promise<(), capnp::Error> {
        Promise::err(capnp::Error::unimplemented("stub http client".into()))
    }
}

/// Stub Routing: returns unimplemented for all methods.
pub struct StubRouting;

#[allow(refining_impl_trait)]
impl routing_capnp::routing::Server for StubRouting {
    fn provide(
        self: capnp::capability::Rc<Self>,
        _params: routing_capnp::routing::ProvideParams,
        _results: routing_capnp::routing::ProvideResults,
    ) -> Promise<(), capnp::Error> {
        Promise::err(capnp::Error::unimplemented("stub routing".into()))
    }

    fn find_providers(
        self: capnp::capability::Rc<Self>,
        _params: routing_capnp::routing::FindProvidersParams,
        _results: routing_capnp::routing::FindProvidersResults,
    ) -> Promise<(), capnp::Error> {
        Promise::err(capnp::Error::unimplemented("stub routing".into()))
    }

    fn hash(
        self: capnp::capability::Rc<Self>,
        _params: routing_capnp::routing::HashParams,
        _results: routing_capnp::routing::HashResults,
    ) -> Promise<(), capnp::Error> {
        Promise::err(capnp::Error::unimplemented("stub routing".into()))
    }
}

/// GraftBuilder that populates ALL 5 graft capabilities with stubs (Export list format).
/// Used to verify that graft() returns every capability field.
pub struct FullStubSessionBuilder;

impl GraftBuilder for FullStubSessionBuilder {
    fn build(
        &self,
        guard: &EpochGuard,
        mut builder: atom::membrane_capnp::membrane::graft_results::Builder<'_>,
    ) -> std::result::Result<(), capnp::Error> {
        let identity: auth_capnp::identity::Client = capnp_rpc::new_client(StubIdentity);
        let host: system_capnp::host::Client = capnp_rpc::new_client(StubHost);
        let runtime: system_capnp::runtime::Client = capnp_rpc::new_client(StubRuntime {
            guard: guard.clone(),
        });
        let routing: routing_capnp::routing::Client = capnp_rpc::new_client(StubRouting);
        let http_client: http_capnp::http_client::Client = capnp_rpc::new_client(StubHttpClient);

        let mut caps = builder.reborrow().init_caps(5);

        let mut e = caps.reborrow().get(0);
        e.set_name("identity");
        e.reborrow().init_schema(); // Phase 1: default (empty) Schema.Node
        e.init_cap().set_as_capability(identity.client.hook);

        let mut e = caps.reborrow().get(1);
        e.set_name("host");
        e.reborrow().init_schema();
        e.init_cap().set_as_capability(host.client.hook);

        let mut e = caps.reborrow().get(2);
        e.set_name("runtime");
        e.reborrow().init_schema();
        e.init_cap().set_as_capability(runtime.client.hook);

        let mut e = caps.reborrow().get(3);
        e.set_name("routing");
        e.reborrow().init_schema();
        e.init_cap().set_as_capability(routing.client.hook);

        let mut e = caps.reborrow().get(4);
        e.set_name("http-client");
        e.reborrow().init_schema();
        e.init_cap().set_as_capability(http_client.client.hook);

        Ok(())
    }
}

/// Reqwest client that does not use system proxy (avoids SCDynamicStore panic in sandbox/CI).
fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .no_proxy()
        .build()
        .expect("reqwest client")
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
        .context("HTTP request")?;
    let resp = resp.error_for_status().context("HTTP status")?;
    let v: Value = resp.json().await.context("parse response")?;
    if let Some(err) = v.get("error") {
        anyhow::bail!("RPC error: {}", err);
    }
    v.get("result")
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("Missing result"))
}

/// Create a snapshot of the current chain state (Anvil). Returns snapshot id for evm_revert.
pub async fn evm_snapshot(http_url: &str) -> Result<String> {
    let client = http_client();
    let result = http_json_rpc(&client, http_url, "evm_snapshot", json!([]), 10).await?;
    let id = result
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("snapshot id not string"))?;
    Ok(id.to_string())
}

/// Revert chain to a previous snapshot (Anvil). Returns true if revert succeeded.
pub async fn evm_revert(http_url: &str, snap: &str) -> Result<bool> {
    let client = http_client();
    let result = http_json_rpc(&client, http_url, "evm_revert", json!([snap]), 11).await?;
    result
        .as_bool()
        .ok_or_else(|| anyhow::anyhow!("evm_revert result not bool"))
}

/// Mine n blocks (Anvil). Loops evm_mine one block at a time.
pub async fn evm_mine(http_url: &str, n: u64) -> Result<()> {
    let client = http_client();
    for _ in 0..n {
        let _ = http_json_rpc(&client, http_url, "evm_mine", json!([]), 12).await?;
    }
    Ok(())
}

/// Current block number via eth_blockNumber.
pub async fn eth_block_number(http_url: &str) -> Result<u64> {
    let client = http_client();
    let result = http_json_rpc(&client, http_url, "eth_blockNumber", json!([]), 1).await?;
    let s = result
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("blockNumber not string"))?;
    let s = s.strip_prefix("0x").unwrap_or(s);
    u64::from_str_radix(s, 16).context("parse block number")
}

/// Get transaction count (nonce) for address via eth_getTransactionCount.
pub async fn eth_get_transaction_count(http_url: &str, address: &str) -> Result<u64> {
    let client = http_client();
    let address = address.strip_prefix("0x").unwrap_or(address);
    let result = http_json_rpc(
        &client,
        http_url,
        "eth_getTransactionCount",
        json!([format!("0x{}", address), "latest"]),
        14,
    )
    .await?;
    let s = result
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("getTransactionCount result not string"))?;
    let s = s.strip_prefix("0x").unwrap_or(s);
    u64::from_str_radix(s, 16).context("parse nonce")
}

/// Call Atom.head() via eth_call and decode. For optional post-revert assertion (e.g. seq == 0).
pub async fn atom_head_http(
    http_url: &str,
    contract_address: &[u8; 20],
) -> Result<atom::CurrentHead> {
    use atom::abi::{decode_head_return, HEAD_SELECTOR};
    let client = http_client();
    let params = json!([{
        "to": format!("0x{}", hex::encode(contract_address)),
        "data": format!("0x{}", hex::encode(HEAD_SELECTOR)),
    }, "latest"]);
    let result = http_json_rpc(&client, http_url, "eth_call", params, 13).await?;
    let s = result
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("eth_call result not string"))?;
    let bytes = hex::decode(s.strip_prefix("0x").unwrap_or(s)).context("decode eth_call")?;
    decode_head_return(&bytes).context("decode head()")
}

/// Returns a skip reason when the Foundry-backed integration tests cannot run.
/// Use at the start of integration tests to skip when not in CI/local dev with Foundry.
pub fn foundry_unavailable_reason() -> Option<String> {
    fn in_path(cmd: &str, args: &[&str]) -> bool {
        Command::new(cmd)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    let mut missing = Vec::new();
    for cmd in ["anvil", "forge", "cast"] {
        if !in_path(cmd, &["--help"]) {
            missing.push(cmd);
        }
    }
    if !missing.is_empty() {
        return Some(format!(
            "{} not in PATH; install Foundry before running these tests",
            missing.join("/")
        ));
    }

    let repo_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("crates/atom should be two levels below repo root");
    let forge_std_script = repo_root.join("contracts/stem/lib/forge-std/src/Script.sol");
    let forge_std_test = repo_root.join("contracts/stem/lib/forge-std/src/Test.sol");
    if !forge_std_script.exists() || !forge_std_test.exists() {
        return Some(
            "contracts/stem/lib/forge-std is not initialized; run `git submodule update --init contracts/stem/lib/forge-std`"
                .to_string(),
        );
    }

    None
}

/// True if Foundry tooling and contract submodules are available.
pub fn foundry_available() -> bool {
    foundry_unavailable_reason().is_none()
}

/// Spawn Anvil on a dynamic port and wait until ready.
pub async fn spawn_anvil() -> Result<(Child, String)> {
    let port = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").context("bind for port")?;
        listener.local_addr()?.port()
    };
    let rpc_url = format!("http://127.0.0.1:{}", port);
    let mut cmd = Command::new("anvil");
    cmd.arg("--port")
        .arg(port.to_string())
        .arg("--host")
        .arg("127.0.0.1");
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    let process = cmd.spawn().context("spawn anvil")?;
    wait_for_rpc(&rpc_url).await?;
    Ok((process, rpc_url))
}

async fn wait_for_rpc(url: &str) -> Result<()> {
    let client = http_client();
    for _ in 0..30 {
        let ok = client
            .post(url)
            .json(
                &serde_json::json!({"jsonrpc":"2.0","method":"eth_blockNumber","params":[],"id":1}),
            )
            .send()
            .await
            .is_ok();
        if ok {
            return Ok(());
        }
        sleep(Duration::from_millis(100)).await;
    }
    anyhow::bail!("RPC not ready");
}

/// Deploy Atom contract via forge script. The Foundry project lives at `contracts/stem/`
/// under `repo_root`. Parses the deployed address from the broadcast artifact.
pub fn deploy_atom(repo_root: &std::path::Path, rpc_url: &str) -> Result<String> {
    let foundry_root = repo_root.join("contracts/stem");
    let out = Command::new("forge")
        .current_dir(&foundry_root)
        .args([
            "script",
            "script/Deploy.s.sol:Deploy",
            "--rpc-url",
            rpc_url,
            "--broadcast",
            "--private-key",
            "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80",
        ])
        .output()
        .context("forge script")?;
    if !out.status.success() {
        anyhow::bail!(
            "forge script failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    // Anvil chain id is 31337
    let artifact_path = foundry_root.join("broadcast/Deploy.s.sol/31337/run-latest.json");
    let bytes = std::fs::read(&artifact_path)
        .with_context(|| format!("read broadcast artifact: {}", artifact_path.display()))?;
    let json: serde_json::Value = serde_json::from_slice(&bytes).context("parse broadcast JSON")?;
    let txs = json
        .get("transactions")
        .and_then(|t| t.as_array())
        .ok_or_else(|| anyhow::anyhow!("broadcast: missing transactions array"))?;
    for tx in txs {
        if tx.get("transactionType").and_then(|t| t.as_str()) == Some("CREATE") {
            let addr = tx
                .get("contractAddress")
                .and_then(|a| a.as_str())
                .ok_or_else(|| anyhow::anyhow!("CREATE tx missing contractAddress"))?;
            return Ok(addr.to_string());
        }
    }
    anyhow::bail!("no CREATE transaction in broadcast artifact");
}

/// Call setHead via cast send. Matches deployed Atom.sol ABI.
/// - signature "setHead(bytes)": args = [cid_hex]; cid_kind ignored.
/// - signature "setHead(uint8,bytes)": args = [cid_kind, cid_hex]; cid_kind required.
///   Prefer set_head_bytes when you have raw bytes so encoding is unambiguous.
pub fn set_head(
    repo_root: &std::path::Path,
    rpc_url: &str,
    contract: &str,
    signature: &str,
    cid_hex: &str,
    cid_kind: Option<u8>,
) -> Result<()> {
    let cid_hex_arg = cid_hex.strip_prefix("0x").unwrap_or(cid_hex);
    set_head_hex(
        repo_root,
        rpc_url,
        contract,
        signature,
        cid_hex_arg,
        cid_kind,
    )
}

/// Anvil default account 0 (used for eth_sendTransaction when node signs).
const ANVIL_DEFAULT_FROM: &str = "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266";

/// First 4 bytes of keccak256("setHead(bytes)"). Matches [Atom.sol](src/Atom.sol) setHead(bytes calldata newCid).
const SET_HEAD_BYTES_SELECTOR: [u8; 4] = [0x43, 0xea, 0xe8, 0x23];

/// Build ABI-encoded calldata for setHead(bytes)(cid_bytes) per Solidity ABI spec (dynamic type: offset then enc(k) pad_right(X)).
/// Length in last 4 bytes of the 32-byte word at offset (bytes 64-67); data at 68+.
fn build_set_head_bytes_calldata(cid_bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(4 + 32 + 32 + 32 + cid_bytes.len().div_ceil(32) * 32);
    out.extend_from_slice(&SET_HEAD_BYTES_SELECTOR);
    out.extend_from_slice(&[0u8; 28]);
    out.extend_from_slice(&32u32.to_be_bytes()); // offset to dynamic bytes = 0x20
    out.extend_from_slice(&[0u8; 28]);
    out.extend_from_slice(&(cid_bytes.len() as u32).to_be_bytes()); // length as 32-byte word (right-aligned at 64-67)
    out.extend_from_slice(cid_bytes);
    let pad = (32 - (cid_bytes.len() % 32)) % 32;
    out.extend_from_slice(&vec![0u8; pad][..]);
    out
}

/// Anvil default account 0 private key (same as forge/cast use for --private-key).
const ANVIL_DEFAULT_PRIVATE_KEY: [u8; 32] = [
    0xac, 0x09, 0x74, 0xbe, 0xc3, 0x9a, 0x17, 0xe3, 0x6b, 0xa4, 0xa6, 0xb4, 0xd2, 0x38, 0xff, 0x94,
    0x4b, 0xac, 0xb4, 0x78, 0xcb, 0xed, 0x5e, 0xfc, 0xae, 0x78, 0x4d, 0x7b, 0xf4, 0xf2, 0xff, 0x80,
];

const ANVIL_CHAIN_ID: u64 = 31337;

/// Build RLP-encoded EIP-155 unsigned payload: [nonce, gas_price, gas_limit, to, value, data, chain_id, 0, 0].
fn rlp_encode_unsigned_legacy(
    nonce: u64,
    gas_price: u64,
    gas_limit: u64,
    to: &[u8; 20],
    value: u64,
    data: &[u8],
    chain_id: u64,
) -> Vec<u8> {
    use rlp::RlpStream;
    let mut s = RlpStream::new();
    s.begin_list(9);
    s.append(&nonce);
    s.append(&gas_price);
    s.append(&gas_limit);
    let to_slice: &[u8] = to;
    s.append(&to_slice);
    s.append(&value);
    s.append(&data);
    s.append(&chain_id);
    s.append(&0u8);
    s.append(&0u8);
    s.out().to_vec()
}

/// Trim leading zero bytes for RLP integer encoding. For value 0 return empty slice (encodes as 0x80).
fn trim_leading_zeros(b: &[u8; 32]) -> &[u8] {
    let mut i = 0;
    while i < 32 && b[i] == 0 {
        i += 1;
    }
    if i == 32 {
        &b[32..] // empty slice so RLP encodes integer 0 as 0x80
    } else {
        &b[i..]
    }
}

/// Send a raw EIP-155 legacy transaction via eth_sendRawTransaction. Signs in-process with Anvil default key.
/// Ensures exact calldata is sent without node/JSON interpretation.
pub async fn send_raw_transaction(http_url: &str, to: &str, calldata: &[u8]) -> Result<()> {
    use k256::ecdsa::SigningKey;
    use rlp::RlpStream;
    use sha3::{Digest, Keccak256};

    let to = to.strip_prefix("0x").unwrap_or(to);
    let to_bytes = hex::decode(to).context("decode to address")?;
    let mut to_arr = [0u8; 20];
    to_arr.copy_from_slice(&to_bytes);

    let nonce = eth_get_transaction_count(http_url, ANVIL_DEFAULT_FROM).await?;
    let gas_price = 20_000_000_000u64; // 20 gwei
    let gas_limit = 0x30d40u64;
    let value = 0u64;

    let unsigned_rlp = rlp_encode_unsigned_legacy(
        nonce,
        gas_price,
        gas_limit,
        &to_arr,
        value,
        calldata,
        ANVIL_CHAIN_ID,
    );

    let signing_key = SigningKey::from_bytes((&ANVIL_DEFAULT_PRIVATE_KEY).into())
        .map_err(|e| anyhow::anyhow!("invalid signing key: {}", e))?;
    let (signature, recovery_id) = signing_key
        .sign_digest_recoverable(Keccak256::new_with_prefix(unsigned_rlp))
        .map_err(|e| anyhow::anyhow!("sign failed: {}", e))?;

    let v: u64 = ANVIL_CHAIN_ID
        .checked_mul(2)
        .and_then(|x| x.checked_add(35))
        .and_then(|x| x.checked_add(u64::from(recovery_id.to_byte())))
        .ok_or_else(|| anyhow::anyhow!("v overflow"))?;

    let sig_bytes = signature.to_bytes();
    let sig_slice: &[u8] = sig_bytes.as_ref();
    let r: [u8; 32] = sig_slice[0..32].try_into().unwrap();
    let s: [u8; 32] = sig_slice[32..64].try_into().unwrap();
    let r_trimmed = trim_leading_zeros(&r);
    let s_trimmed = trim_leading_zeros(&s);

    let mut signed = RlpStream::new();
    signed.begin_list(9);
    signed.append(&nonce);
    signed.append(&gas_price);
    signed.append(&gas_limit);
    let addr_slice: &[u8] = &to_arr;
    signed.append(&addr_slice);
    signed.append(&value);
    signed.append(&calldata);
    signed.append(&v);
    signed.append(&r_trimmed);
    signed.append(&s_trimmed);
    let raw_tx = signed.out().to_vec();

    let client = http_client();
    let params = json!([format!("0x{}", hex::encode(&raw_tx))]);
    let tx_hash_value =
        http_json_rpc(&client, http_url, "eth_sendRawTransaction", params, 21).await?;
    let _tx_hash = tx_hash_value
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("tx hash not string"))?;
    Ok(())
}

/// Send a transaction using eth_sendTransaction (kept for reference; set_head_bytes uses send_raw_transaction). Anvil signs for its default unlocked account (ANVIL_DEFAULT_FROM).
/// Calldata is built in Rust so encoding is exact; no cast subprocess involved.
pub async fn send_transaction(http_url: &str, to: &str, calldata: &[u8]) -> Result<()> {
    let to = to.strip_prefix("0x").unwrap_or(to);
    let data_hex = hex::encode(calldata);
    // Build JSON body as raw string so "data" is sent exactly (no serde_json round-trip).
    let body = format!(
        r#"{{"jsonrpc":"2.0","id":20,"method":"eth_sendTransaction","params":[{{"from":"{}","to":"0x{}","data":"0x{}","value":"0x0","gas":"0x30d40"}}]}}"#,
        ANVIL_DEFAULT_FROM, to, data_hex
    );
    let client = http_client();
    let result = async {
        let resp = client
            .post(http_url)
            .header("Content-Type", "application/json")
            .body(body)
            .send()
            .await
            .context("HTTP request")?;
        let resp = resp.error_for_status().context("HTTP status")?;
        let v: Value = resp.json().await.context("parse response")?;
        if let Some(err) = v.get("error") {
            anyhow::bail!("RPC error: {}", err);
        }
        v.get("result")
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("Missing result"))
    }
    .await;
    let _ = result?;
    Ok(())
}

/// Call setHead with raw CID bytes. Builds calldata in Rust (see build_set_head_bytes_calldata) and sends via eth_sendRawTransaction
/// with in-process EIP-155 signing so encoding is exact. Anvil auto-mines the tx immediately.
pub async fn set_head_bytes(
    _repo_root: &std::path::Path,
    rpc_url: &str,
    contract: &str,
    signature: &str,
    cid_bytes: &[u8],
    _cid_kind: Option<u8>,
) -> Result<()> {
    let calldata = if signature == "setHead(bytes)" {
        build_set_head_bytes_calldata(cid_bytes)
    } else {
        anyhow::bail!("set_head_bytes only supports setHead(bytes) for now");
    };
    send_raw_transaction(rpc_url, contract, &calldata).await?;
    Ok(())
}

/// Same as set_head but takes hex string without 0x (avoids cast mis-parsing).
fn set_head_hex(
    repo_root: &std::path::Path,
    rpc_url: &str,
    contract: &str,
    signature: &str,
    cid_hex_no_prefix: &str,
    cid_kind: Option<u8>,
) -> Result<()> {
    let mut cast_args: Vec<String> = vec!["send".into(), contract.into(), signature.into()];
    if signature == "setHead(uint8,bytes)" {
        let k = cid_kind
            .ok_or_else(|| anyhow::anyhow!("cid_kind required for setHead(uint8,bytes)"))?;
        cast_args.push(k.to_string());
    }
    cast_args.push(cid_hex_no_prefix.into());
    let out = Command::new("cast")
        .current_dir(repo_root)
        .args(&cast_args)
        .args([
            "--rpc-url",
            rpc_url,
            "--private-key",
            "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80",
        ])
        .output()
        .context("cast send")?;
    if !out.status.success() {
        anyhow::bail!("cast send failed: {}", String::from_utf8_lossy(&out.stderr));
    }
    Ok(())
}
