//! `ww shell` CLI surface.

use anyhow::{bail, Context, Result};
use auth::SigningDomain;
use capnp::capability::FromClientHook;
use capnp_rpc::{new_client, pry};
use libp2p::multiaddr::Protocol;
use libp2p::{Multiaddr, PeerId, StreamProtocol};
use libp2p_core::SignedEnvelope;
use rustyline::error::ReadlineError;
use std::collections::HashMap;
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};

const CAPNP_PROTOCOL: StreamProtocol = StreamProtocol::new("/ww/0.1.0");
const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(3);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const RPC_TIMEOUT: Duration = Duration::from_secs(30);
const TEST_DISCOVERY_ENV: &str = "WW_TEST_MDNS_CANDIDATES";

#[cfg(has_wasm_std_shell_bin_shell_wasm)]
const EMBEDDED_SHELL: &[u8] = include_bytes!("../../std/shell/bin/shell.wasm");
#[cfg(not(has_wasm_std_shell_bin_shell_wasm))]
const EMBEDDED_SHELL: &[u8] = &[];

#[derive(Clone, Debug)]
struct Candidate {
    peer_id: Option<PeerId>,
    addrs: Vec<Multiaddr>,
}

struct LocalSigner {
    keypair: libp2p::identity::Keypair,
}

impl LocalSigner {
    fn from_signing_key(sk: &ed25519_dalek::SigningKey) -> Result<Self> {
        let keypair = ww::keys::to_libp2p(sk)?;
        Ok(Self { keypair })
    }
}

#[allow(refining_impl_trait)]
impl ww::stem_capnp::signer::Server for LocalSigner {
    fn sign(
        self: capnp::capability::Rc<Self>,
        params: ww::stem_capnp::signer::SignParams,
        mut results: ww::stem_capnp::signer::SignResults,
    ) -> capnp::capability::Promise<(), capnp::Error> {
        let p = pry!(params.get());
        let nonce = p.get_nonce();
        let epoch_seq = p.get_epoch_seq();
        let domain = SigningDomain::terminal_membrane();

        let mut payload = Vec::with_capacity(16);
        payload.extend_from_slice(&nonce.to_be_bytes());
        payload.extend_from_slice(&epoch_seq.to_be_bytes());

        let envelope = pry!(SignedEnvelope::new(
            &self.keypair,
            domain.as_str().to_string(),
            domain.payload_type().to_vec(),
            payload,
        )
        .map_err(|e| capnp::Error::failed(format!("signing failed: {e}"))));

        results.get().set_sig(&envelope.into_protobuf_encoding());
        capnp::capability::Promise::ok(())
    }
}

/// Run the interactive shell client.
///
/// - `ww shell <addr>` dials explicit multiaddr.
/// - `ww shell` discovers local mDNS candidates and connects when unambiguous.
pub async fn run_shell(addr: Option<Multiaddr>, select: Option<String>) -> Result<()> {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async move { run_shell_local(addr, select).await })
        .await
}

async fn run_shell_local(addr: Option<Multiaddr>, select: Option<String>) -> Result<()> {
    let (signing_key, preferred_peer_id) = load_shell_identity()?;

    let target = if let Some(addr) = addr {
        let peer = peer_id_from_addr(&addr);
        let addrs = vec![addr];
        candidate_from_parts(peer, addrs)?
    } else {
        let candidates = if let Some(candidates) = discovery_candidates_override()? {
            candidates
        } else {
            discover_mdns_candidates(&signing_key).await?
        };
        choose_candidate(
            candidates,
            Some(preferred_peer_id),
            select.as_deref(),
            stdin_is_interactive_tty(),
        )?
    };

    let shell = dial_shell(&target, &signing_key).await?;
    run_repl(&shell).await
}

async fn dial_shell(
    target: &Candidate,
    signing_key: &ed25519_dalek::SigningKey,
) -> Result<ww::shell_capnp::shell::Client> {
    let keypair = ww::keys::to_libp2p(signing_key)?;
    let mut client = ww::host::ClientSwarm::new(keypair)?;
    let mut stream_control = client.stream_control();

    let (connected_tx, connected_rx) = oneshot::channel();

    if let Some(peer_id) = target.peer_id {
        // Seed known addresses and initiate dial.
        for addr in &target.addrs {
            client.add_peer_addr(peer_id, addr.clone());
        }
    } else if let Some(addr) = target.addrs.first() {
        client
            .dial(addr.clone())
            .map_err(|e| anyhow::anyhow!("failed to dial {addr}: {e}"))?;
    } else {
        bail!("no dial addresses provided");
    }

    tokio::task::spawn_local(client.run(Some(connected_tx), None));

    let connected_peer = tokio::time::timeout(CONNECT_TIMEOUT, connected_rx)
        .await
        .context("timed out waiting for libp2p connection")?
        .context("connection notification channel dropped")?;

    let remote_peer = target.peer_id.unwrap_or(connected_peer);

    let stream = tokio::time::timeout(
        CONNECT_TIMEOUT,
        stream_control.open_stream(remote_peer, CAPNP_PROTOCOL),
    )
    .await
    .context("timed out opening shell stream")?
    .map_err(|e| anyhow::anyhow!("failed to open shell stream: {e}"))?;

    let ww::rpc::vat_dial::VatDial {
        bootstrap: terminal,
        driver: _driver,
    } = ww::rpc::vat_dial::connect::<
        _,
        ww::stem_capnp::terminal::Client<ww::stem_capnp::membrane::Owned>,
    >(stream);

    let signer_client: ww::stem_capnp::signer::Client =
        new_client(LocalSigner::from_signing_key(signing_key)?);

    let mut login_req = terminal.login_request();
    login_req.get().set_signer(signer_client);
    let login_resp = tokio::time::timeout(RPC_TIMEOUT, login_req.send().promise)
        .await
        .context("terminal login timed out")??;

    let membrane = login_resp
        .get()?
        .get_session()
        .context("terminal login returned no session")?;

    let graft_resp = tokio::time::timeout(RPC_TIMEOUT, membrane.graft_request().send().promise)
        .await
        .context("graft request timed out")??;
    let caps = graft_resp.get()?.get_caps()?;
    let runtime: ww::system_capnp::runtime::Client = get_graft_cap(&caps, "runtime")?;

    let shell_wasm = load_shell_wasm()?;

    let mut load_req = runtime.load_request();
    load_req.get().set_wasm(&shell_wasm);
    let load_resp = tokio::time::timeout(RPC_TIMEOUT, load_req.send().promise)
        .await
        .context("runtime.load timed out")??;
    let executor = load_resp.get()?.get_executor()?;

    let spawn_resp = tokio::time::timeout(RPC_TIMEOUT, executor.spawn_request().send().promise)
        .await
        .context("executor.spawn timed out")??;
    let process = spawn_resp.get()?.get_process()?;

    let bootstrap_resp =
        tokio::time::timeout(RPC_TIMEOUT, process.bootstrap_request().send().promise)
            .await
            .context("process.bootstrap timed out")??;
    let bootstrap = bootstrap_resp.get()?;
    let shell: ww::shell_capnp::shell::Client = bootstrap.get_cap().get_as_capability()?;

    wait_shell_ready(&shell).await?;
    Ok(shell)
}

async fn run_repl(shell: &ww::shell_capnp::shell::Client) -> Result<()> {
    let mut rl = rustyline::DefaultEditor::new().context("failed to initialize line editor")?;

    loop {
        match rl.readline("ww> ") {
            Ok(line) => {
                let input = line.trim();
                if input.is_empty() {
                    continue;
                }
                if input == ":q" || input == ":quit" || input == ":exit" {
                    break;
                }
                let _ = rl.add_history_entry(input);

                let (result, is_error) = shell_eval(shell, input).await?;
                if is_error {
                    eprintln!("{result}");
                } else {
                    println!("{result}");
                }
            }
            Err(ReadlineError::Interrupted) => {
                eprintln!("^C");
                continue;
            }
            Err(ReadlineError::Eof) => break,
            Err(e) => return Err(anyhow::anyhow!("readline error: {e}")),
        }
    }

    Ok(())
}

async fn shell_eval(shell: &ww::shell_capnp::shell::Client, text: &str) -> Result<(String, bool)> {
    let mut req = shell.eval_request();
    req.get().set_text(text);
    let resp = tokio::time::timeout(RPC_TIMEOUT, req.send().promise)
        .await
        .context("shell eval timed out")??;
    let result = resp.get()?;
    let text = result
        .get_result()?
        .to_str()
        .unwrap_or("(invalid UTF-8)")
        .to_string();
    Ok((text, result.get_is_error()))
}

async fn wait_shell_ready(shell: &ww::shell_capnp::shell::Client) -> Result<()> {
    for _ in 0..60 {
        let (result, is_error) = shell_eval(shell, "nil").await?;
        if !is_error || !result.contains("not ready") {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    bail!("shell did not become ready")
}

fn get_graft_cap<T: FromClientHook>(
    caps: &capnp::struct_list::Reader<'_, ww::stem_capnp::export::Owned>,
    name: &str,
) -> Result<T, capnp::Error> {
    for i in 0..caps.len() {
        let entry = caps.get(i);
        let n = entry
            .get_name()?
            .to_str()
            .map_err(|e| capnp::Error::failed(e.to_string()))?;
        if n == name {
            return entry.get_cap().get_as_capability();
        }
    }

    Err(capnp::Error::failed(format!(
        "capability '{name}' not found in graft response"
    )))
}

fn load_shell_wasm() -> Result<Vec<u8>> {
    if !EMBEDDED_SHELL.is_empty() {
        return Ok(EMBEDDED_SHELL.to_vec());
    }

    let path = Path::new("std/shell/bin/shell.wasm");
    if path.exists() {
        return std::fs::read(path).context("failed to read std/shell/bin/shell.wasm");
    }

    bail!(
        "shell WASM not found (embedded shell unavailable). Build it with `make shell` or install a release with embedded shell."
    )
}

fn shell_identity_path() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("WW_IDENTITY") {
        return Ok(PathBuf::from(path));
    }

    let home = dirs::home_dir().context("cannot determine home directory")?;
    Ok(home.join(".ww/identity"))
}

fn load_shell_identity() -> Result<(ed25519_dalek::SigningKey, PeerId)> {
    let path = shell_identity_path()?;
    if !path.exists() {
        bail!(
            "Identity file not found: {}\n\
             `ww shell` requires a persistent identity to authenticate.\n\
             Create one with: ww keygen > ~/.ww/identity",
            path.display()
        );
    }

    let sk = ww::keys::load(path.to_str().context("identity path is non-UTF-8")?)?;
    let peer_id = ww::keys::to_libp2p(&sk)?.public().to_peer_id();
    Ok((sk, peer_id))
}

async fn discover_mdns_candidates(
    signing_key: &ed25519_dalek::SigningKey,
) -> Result<Vec<Candidate>> {
    let keypair = ww::keys::to_libp2p(signing_key)?;
    let client = ww::host::ClientSwarm::new(keypair)?;

    let (_connected_tx, _connected_rx) = oneshot::channel::<PeerId>();
    let (discovered_tx, mut discovered_rx) = mpsc::unbounded_channel::<(PeerId, Multiaddr)>();

    tokio::task::spawn_local(client.run(None, Some(discovered_tx)));

    let deadline = tokio::time::Instant::now() + DISCOVERY_TIMEOUT;
    let mut candidates: HashMap<PeerId, Vec<Multiaddr>> = HashMap::new();

    loop {
        tokio::select! {
            _ = tokio::time::sleep_until(deadline) => break,
            maybe = discovered_rx.recv() => {
                match maybe {
                    Some((peer_id, addr)) => {
                        let entry = candidates.entry(peer_id).or_default();
                        if !entry.iter().any(|a| a == &addr) {
                            entry.push(addr);
                        }
                    }
                    None => break,
                }
            }
        }
    }

    Ok(candidates
        .into_iter()
        .map(|(peer_id, addrs)| Candidate {
            peer_id: Some(peer_id),
            addrs,
        })
        .collect())
}

fn discovery_candidates_override() -> Result<Option<Vec<Candidate>>> {
    #[cfg(debug_assertions)]
    {
        match std::env::var(TEST_DISCOVERY_ENV) {
            Ok(raw) => Ok(Some(parse_discovery_candidates_json(&raw)?)),
            Err(std::env::VarError::NotPresent) => Ok(None),
            Err(std::env::VarError::NotUnicode(_)) => {
                bail!("{TEST_DISCOVERY_ENV} must be valid UTF-8 JSON")
            }
        }
    }
    #[cfg(not(debug_assertions))]
    {
        Ok(None)
    }
}

fn parse_discovery_candidates_json(raw: &str) -> Result<Vec<Candidate>> {
    #[derive(serde::Deserialize)]
    struct RawCandidate {
        peer_id: String,
        addrs: Vec<String>,
    }

    let raw_candidates: Vec<RawCandidate> = serde_json::from_str(raw)
        .map_err(|e| anyhow::anyhow!("invalid {TEST_DISCOVERY_ENV} JSON: {e}"))?;

    let mut candidates = Vec::with_capacity(raw_candidates.len());
    for (index, entry) in raw_candidates.into_iter().enumerate() {
        let peer_id: PeerId = entry.peer_id.parse().map_err(|e| {
            anyhow::anyhow!("invalid peer_id at {TEST_DISCOVERY_ENV}[{index}]: {e}")
        })?;

        let mut addrs = Vec::with_capacity(entry.addrs.len());
        for (addr_index, addr) in entry.addrs.into_iter().enumerate() {
            let parsed: Multiaddr = addr.parse().map_err(|e| {
                anyhow::anyhow!(
                    "invalid multiaddr at {TEST_DISCOVERY_ENV}[{index}].addrs[{addr_index}]: {e}"
                )
            })?;
            addrs.push(parsed);
        }

        candidates.push(Candidate {
            peer_id: Some(peer_id),
            addrs,
        });
    }

    Ok(candidates)
}

fn choose_candidate(
    candidates: Vec<Candidate>,
    preferred: Option<PeerId>,
    select: Option<&str>,
    interactive_tty: bool,
) -> Result<Candidate> {
    if candidates.is_empty() {
        bail!(
            "No wetware hosts discovered via mDNS.\n\
             Try `ww shell <multiaddr>` to connect explicitly."
        );
    }

    if let Some(selector) = select {
        let selected = choose_candidate_by_selector(&candidates, selector)?;
        return ensure_candidate_addr(selected);
    }

    if candidates.len() == 1 {
        return ensure_candidate_addr(candidates.into_iter().next().unwrap());
    }

    if let Some(preferred_peer) = preferred {
        let mut matches: Vec<Candidate> = candidates
            .iter()
            .filter(|c| c.peer_id == Some(preferred_peer))
            .cloned()
            .collect();

        if matches.len() == 1 {
            return ensure_candidate_addr(matches.remove(0));
        }
    }

    if interactive_tty {
        return select_candidate_interactive(&candidates);
    }

    let listing = format_candidates(&candidates);
    bail!(
        "Multiple wetware hosts discovered via mDNS; refusing to guess.\n\
         If this is a script/non-interactive session, pass one of:\n\
         - `ww shell --select <index|peer-id>`\n\
         Use an explicit multiaddr: `ww shell <multiaddr>`\n\
         Discovered hosts:{listing}\n\
         TODO tracked in https://github.com/wetware/ww/issues/479"
    )
}

fn stdin_is_interactive_tty() -> bool {
    io::stdin().is_terminal() && io::stdout().is_terminal()
}

fn format_candidates(candidates: &[Candidate]) -> String {
    let mut listing = String::new();
    for (index, c) in candidates.iter().enumerate() {
        let addrs = c
            .addrs
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(", ");
        if let Some(peer_id) = c.peer_id {
            listing.push_str(&format!("\n  [{}] {} [{}]", index + 1, peer_id, addrs));
        } else {
            listing.push_str(&format!("\n  [{}] <unknown-peer> [{}]", index + 1, addrs));
        }
    }
    listing
}

fn choose_candidate_by_selector(candidates: &[Candidate], selector: &str) -> Result<Candidate> {
    let selector = selector.trim();
    if selector.is_empty() {
        bail!("empty selector: expected index (1..N) or peer id");
    }

    if let Ok(index) = selector.parse::<usize>() {
        if index == 0 || index > candidates.len() {
            bail!(
                "selector index {} out of range; expected 1..{}",
                index,
                candidates.len()
            );
        }
        return Ok(candidates[index - 1].clone());
    }

    let peer_id: PeerId = selector
        .parse()
        .map_err(|_| anyhow::anyhow!("invalid selector '{selector}': expected index or peer id"))?;

    let mut matches = candidates
        .iter()
        .filter(|c| c.peer_id == Some(peer_id))
        .cloned()
        .collect::<Vec<_>>();

    if matches.is_empty() {
        bail!("selector peer id {peer_id} not found in discovered candidates");
    }
    if matches.len() > 1 {
        bail!("selector peer id {peer_id} matched multiple candidates");
    }
    Ok(matches.remove(0))
}

fn select_candidate_interactive(candidates: &[Candidate]) -> Result<Candidate> {
    println!("Multiple wetware hosts discovered via mDNS.");
    println!("Select a host by index or peer id:");
    print!("{}", format_candidates(candidates));
    println!();

    for _ in 0..5 {
        eprint!("selection> ");
        io::stderr().flush().context("failed to flush stderr")?;

        let mut line = String::new();
        io::stdin()
            .read_line(&mut line)
            .context("failed to read selection from stdin")?;
        let selector = line.trim();

        if selector.eq_ignore_ascii_case("q")
            || selector.eq_ignore_ascii_case("quit")
            || selector.eq_ignore_ascii_case("exit")
        {
            bail!("selection canceled");
        }

        match choose_candidate_by_selector(candidates, selector) {
            Ok(candidate) => return ensure_candidate_addr(candidate),
            Err(err) => eprintln!("Invalid selection: {err}"),
        }
    }

    bail!("too many invalid selections; aborted")
}

fn ensure_candidate_addr(candidate: Candidate) -> Result<Candidate> {
    if candidate.addrs.is_empty() {
        if let Some(peer_id) = candidate.peer_id {
            bail!("discovered peer {} has no dialable addresses", peer_id);
        }
        bail!("candidate has no dialable addresses");
    }
    Ok(candidate)
}

fn peer_id_from_addr(addr: &Multiaddr) -> Option<PeerId> {
    for protocol in addr.iter() {
        if let Protocol::P2p(peer_id) = protocol {
            return Some(peer_id);
        }
    }
    None
}

fn candidate_from_parts(peer: Option<PeerId>, addrs: Vec<Multiaddr>) -> Result<Candidate> {
    if addrs.is_empty() {
        bail!("no dial addresses provided")
    }
    Ok(Candidate {
        peer_id: peer,
        addrs,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn maddr(s: &str) -> Multiaddr {
        s.parse().unwrap()
    }

    #[test]
    fn choose_prefers_matching_identity_when_multiple() {
        let p1: PeerId = "12D3KooWJ3qM19qUUj8JdT9kPEg6VZLoes6eexfUYd6Xn7SPrf8n"
            .parse()
            .unwrap();
        let p2: PeerId = "12D3KooWQdQnZYK7hX8Q2Yb8qXWQYvdr4jRWk6TUhSxvVmF5vU3P"
            .parse()
            .unwrap();

        let chosen = choose_candidate(
            vec![
                Candidate {
                    peer_id: Some(p1),
                    addrs: vec![maddr("/ip4/10.0.0.1/tcp/2025")],
                },
                Candidate {
                    peer_id: Some(p2),
                    addrs: vec![maddr("/ip4/10.0.0.2/tcp/2025")],
                },
            ],
            Some(p2),
            None,
            false,
        )
        .unwrap();

        assert_eq!(chosen.peer_id, Some(p2));
    }

    #[test]
    fn choose_errors_on_multiple_without_preference_match() {
        let p1: PeerId = "12D3KooWJ3qM19qUUj8JdT9kPEg6VZLoes6eexfUYd6Xn7SPrf8n"
            .parse()
            .unwrap();
        let p2: PeerId = "12D3KooWQdQnZYK7hX8Q2Yb8qXWQYvdr4jRWk6TUhSxvVmF5vU3P"
            .parse()
            .unwrap();
        let p3: PeerId = "12D3KooWJfUGS8thH9bC4x6hFQ3mFAH3RT6N8gW2H8RyV8Xxwy9A"
            .parse()
            .unwrap();

        let err = choose_candidate(
            vec![
                Candidate {
                    peer_id: Some(p1),
                    addrs: vec![maddr("/ip4/10.0.0.1/tcp/2025")],
                },
                Candidate {
                    peer_id: Some(p2),
                    addrs: vec![maddr("/ip4/10.0.0.2/tcp/2025")],
                },
            ],
            Some(p3),
            None,
            false,
        )
        .unwrap_err()
        .to_string();

        assert!(err.contains("Multiple wetware hosts discovered"), "{err}");
        assert!(err.contains("--select <index|peer-id>"), "{err}");
    }

    #[test]
    fn candidate_from_parts_allows_addr_without_peer_id() {
        let c = candidate_from_parts(None, vec![maddr("/ip4/127.0.0.1/tcp/2025")]).unwrap();
        assert_eq!(c.peer_id, None);
        assert_eq!(c.addrs.len(), 1);
    }

    #[test]
    fn choose_candidate_errors_when_empty() {
        let err = choose_candidate(vec![], None, None, false)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("No wetware hosts discovered via mDNS"),
            "{err}"
        );
    }

    #[test]
    fn choose_candidate_respects_numeric_selector() {
        let p1: PeerId = "12D3KooWJ3qM19qUUj8JdT9kPEg6VZLoes6eexfUYd6Xn7SPrf8n"
            .parse()
            .unwrap();
        let p2: PeerId = "12D3KooWQdQnZYK7hX8Q2Yb8qXWQYvdr4jRWk6TUhSxvVmF5vU3P"
            .parse()
            .unwrap();

        let chosen = choose_candidate(
            vec![
                Candidate {
                    peer_id: Some(p1),
                    addrs: vec![maddr("/ip4/10.0.0.1/tcp/2025")],
                },
                Candidate {
                    peer_id: Some(p2),
                    addrs: vec![maddr("/ip4/10.0.0.2/tcp/2025")],
                },
            ],
            None,
            Some("2"),
            false,
        )
        .unwrap();

        assert_eq!(chosen.peer_id, Some(p2));
    }

    #[test]
    fn choose_candidate_respects_peer_id_selector() {
        let p1: PeerId = "12D3KooWJ3qM19qUUj8JdT9kPEg6VZLoes6eexfUYd6Xn7SPrf8n"
            .parse()
            .unwrap();
        let p2: PeerId = "12D3KooWQdQnZYK7hX8Q2Yb8qXWQYvdr4jRWk6TUhSxvVmF5vU3P"
            .parse()
            .unwrap();

        let chosen = choose_candidate(
            vec![
                Candidate {
                    peer_id: Some(p1),
                    addrs: vec![maddr("/ip4/10.0.0.1/tcp/2025")],
                },
                Candidate {
                    peer_id: Some(p2),
                    addrs: vec![maddr("/ip4/10.0.0.2/tcp/2025")],
                },
            ],
            None,
            Some("12D3KooWQdQnZYK7hX8Q2Yb8qXWQYvdr4jRWk6TUhSxvVmF5vU3P"),
            false,
        )
        .unwrap();

        assert_eq!(chosen.peer_id, Some(p2));
    }

    #[test]
    fn choose_candidate_rejects_invalid_selector() {
        let p1: PeerId = "12D3KooWJ3qM19qUUj8JdT9kPEg6VZLoes6eexfUYd6Xn7SPrf8n"
            .parse()
            .unwrap();
        let p2: PeerId = "12D3KooWQdQnZYK7hX8Q2Yb8qXWQYvdr4jRWk6TUhSxvVmF5vU3P"
            .parse()
            .unwrap();

        let err = choose_candidate(
            vec![
                Candidate {
                    peer_id: Some(p1),
                    addrs: vec![maddr("/ip4/10.0.0.1/tcp/2025")],
                },
                Candidate {
                    peer_id: Some(p2),
                    addrs: vec![maddr("/ip4/10.0.0.2/tcp/2025")],
                },
            ],
            None,
            Some("99"),
            false,
        )
        .unwrap_err()
        .to_string();

        assert!(err.contains("out of range"), "{err}");
    }

    #[test]
    fn peer_id_from_addr_extracts_when_present() {
        let peer_id: PeerId = "12D3KooWJ3qM19qUUj8JdT9kPEg6VZLoes6eexfUYd6Xn7SPrf8n"
            .parse()
            .unwrap();
        let addr = maddr(&format!("/ip4/127.0.0.1/tcp/2025/p2p/{peer_id}"));
        assert_eq!(peer_id_from_addr(&addr), Some(peer_id));
    }

    #[test]
    fn peer_id_from_addr_returns_none_without_p2p() {
        let addr = maddr("/ip4/127.0.0.1/tcp/2025");
        assert_eq!(peer_id_from_addr(&addr), None);
    }

    #[test]
    fn parse_discovery_candidates_json_parses_valid_input() {
        let input = r#"[
            {
                "peer_id": "12D3KooWJ3qM19qUUj8JdT9kPEg6VZLoes6eexfUYd6Xn7SPrf8n",
                "addrs": ["/ip4/127.0.0.1/tcp/2025"]
            }
        ]"#;

        let parsed = parse_discovery_candidates_json(input).unwrap();
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].addrs.len(), 1);
        assert_eq!(
            parsed[0].addrs[0].to_string(),
            "/ip4/127.0.0.1/tcp/2025".to_string()
        );
    }

    #[test]
    fn parse_discovery_candidates_json_rejects_invalid_multiaddr() {
        let input = r#"[
            {
                "peer_id": "12D3KooWJ3qM19qUUj8JdT9kPEg6VZLoes6eexfUYd6Xn7SPrf8n",
                "addrs": ["not-a-multiaddr"]
            }
        ]"#;

        let err = parse_discovery_candidates_json(input)
            .unwrap_err()
            .to_string();
        assert!(err.contains("invalid multiaddr"), "{err}");
    }
}
