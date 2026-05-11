//! `ww shell` — thin REPL client that dials a shell cell over libp2p.
//!
//! Discovers local nodes via lockfiles in `~/.ww/run/`, or dials a
//! remote node via an explicit multiaddr.
//!
//! Accepts an optional positional argument:
//!   - `/ip4/.../tcp/.../p2p/...` — dial directly (multiaddr)
//!   - `/dnsaddr/...` — resolve via DNS TXT records, then dial
//!   - *(omitted)* — discover via lockfiles in `~/.ww/run/`

use anyhow::{Context, Result};
use libp2p::Multiaddr;
use std::path::PathBuf;

use ww::rpc::vat_dial;
use ww::shell_capnp;

/// Discover a local node from lockfiles in `~/.ww/run/`.
///
/// If exactly one node is running, returns its multiaddr.
/// If multiple are running, prompts the user to choose.
fn discover_from_lockfiles() -> Result<Multiaddr> {
    let nodes = ww::discovery::list_local_nodes();

    match nodes.len() {
        0 => anyhow::bail!("no local wetware nodes found\n  Start one with: ww run ."),
        1 => {
            let node = &nodes[0];
            let addr = pick_best_addr(&node.addrs)
                .with_context(|| format!("node {} has no addresses", node.peer_id))?;
            let full = format!("{addr}/p2p/{}", node.peer_id);
            eprintln!("Connecting to {}...", node.peer_id);
            full.parse::<Multiaddr>()
                .with_context(|| format!("invalid multiaddr: {full}"))
        }
        _ => {
            eprintln!("Multiple wetware nodes found:\n");
            for (i, node) in nodes.iter().enumerate() {
                let addr_summary = node
                    .addrs
                    .first()
                    .map(|a| a.as_str())
                    .unwrap_or("(no addrs)");
                eprintln!("  [{}] {} ({})", i + 1, node.peer_id, addr_summary);
            }
            eprintln!();

            eprint!("Select node [1-{}]: ", nodes.len());
            let mut input = String::new();
            std::io::stdin()
                .read_line(&mut input)
                .context("failed to read selection")?;

            let choice: usize = input.trim().parse::<usize>().context("invalid selection")?;
            if choice == 0 || choice > nodes.len() {
                anyhow::bail!("selection out of range");
            }

            let node = &nodes[choice - 1];
            let addr = pick_best_addr(&node.addrs)
                .with_context(|| format!("node {} has no addresses", node.peer_id))?;
            let full = format!("{addr}/p2p/{}", node.peer_id);
            eprintln!("Connecting to {}...", node.peer_id);
            full.parse::<Multiaddr>()
                .with_context(|| format!("invalid multiaddr: {full}"))
        }
    }
}

/// Pick the best address from a list — prefer loopback, then any.
fn pick_best_addr(addrs: &[String]) -> Option<String> {
    addrs
        .iter()
        .find(|a| a.contains("/ip4/127.") || a.contains("/ip6/::1/"))
        .or_else(|| addrs.first())
        .cloned()
}

/// Run the interactive shell client.
pub async fn run_shell(addr: Option<Multiaddr>, identity: Option<PathBuf>) -> Result<()> {
    // 1. Resolve target address.
    let addr = match addr {
        Some(a) => a,
        None => discover_from_lockfiles()?,
    };

    // 2. Load identity key.
    //
    // The shell is a client, not a node. It uses an ephemeral key by
    // default so it never collides with the local daemon's identity
    // (libp2p refuses to dial yourself). Only load a real identity when
    // the user passes --identity explicitly.
    let keypair = if let Some(path) = identity {
        let path_str = path.to_str().context("identity path is non-UTF-8")?;
        let sk = ww::keys::load(path_str)?;
        ww::keys::to_libp2p(&sk)?
    } else {
        let sk = ww::keys::generate()?;
        ww::keys::to_libp2p(&sk)?
    };

    // 3. Build client swarm and extract peer ID from the address.
    let mut client = ww::host::ClientSwarm::new(keypair)?;
    let mut stream_control = client.stream_control();

    let peer_id_from_addr = addr.iter().find_map(|proto| match proto {
        libp2p::multiaddr::Protocol::P2p(id) => Some(id),
        _ => None,
    });

    let (connected_tx, connected_rx) = tokio::sync::oneshot::channel();

    if let Some(peer_id) = peer_id_from_addr {
        let transport_addr: Multiaddr = addr
            .iter()
            .filter(|p| !matches!(p, libp2p::multiaddr::Protocol::P2p(_)))
            .collect();
        client.add_peer_addr(peer_id, transport_addr);
    } else {
        client
            .dial(addr.clone())
            .map_err(|e| anyhow::anyhow!("failed to dial {addr}: {e}"))?;
    }

    // 4. Spawn swarm event loop.
    tokio::task::spawn_local(client.run(Some(connected_tx)));

    // 5. Resolve peer ID.
    let peer_id = if let Some(id) = peer_id_from_addr {
        id
    } else {
        eprintln!("Resolving {addr}...");
        tokio::time::timeout(std::time::Duration::from_secs(30), connected_rx)
            .await
            .map_err(|_| anyhow::anyhow!("connection timeout (30s) — is the address correct?"))?
            .map_err(|_| anyhow::anyhow!("swarm event loop ended before connection"))?
    };

    // 6. Compute the shell protocol from schema bytes.
    let schema_bytes = include_bytes!(concat!(env!("OUT_DIR"), "/shell_schema.bin"));
    let protocol_cid = ww::rpc::schema_cid(schema_bytes);
    let stream_protocol = ww::rpc::schema_protocol(&protocol_cid)?;

    // 7. Dial the shell protocol.
    eprintln!("Connecting to {peer_id}...");
    let stream = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        stream_control.open_stream(peer_id, stream_protocol),
    )
    .await
    .map_err(|_| anyhow::anyhow!("connection timeout after 30s"))?
    .map_err(|e| anyhow::anyhow!("failed to open stream: {e}"))?;

    // 8. Bootstrap Cap'n Proto RPC via the paved-path helper, which spawns
    //    the RpcSystem driver before returning. The driver flushes the
    //    Bootstrap message and receives the remote Return on its own; the
    //    REPL loop's first user-typed `eval` call observes whether the
    //    handshake succeeded via that call's own 30s timeout.
    let vat_dial::VatDial {
        bootstrap: shell,
        driver,
    } = vat_dial::connect::<_, shell_capnp::shell::Client>(stream);
    // Surface the eventual RpcSystem outcome for debugging session drops.
    tokio::task::spawn_local(async move {
        if let Ok(Err(e)) = driver.await {
            tracing::debug!("Shell RPC session ended: {e}");
        }
    });

    eprintln!("{}", glia::banner());
    eprintln!("Connected to {peer_id}");
    eprintln!("AI agents:  ipfs cat /ipns/releases.wetware.run/.agents/prompt.md");

    // 9. REPL loop.
    let (line_tx, mut line_rx) = tokio::sync::mpsc::channel::<String>(1);

    std::thread::spawn(move || {
        let mut rl = rustyline::DefaultEditor::new().expect("failed to create editor");
        loop {
            match rl.readline("/ > ") {
                Ok(line) => {
                    if !line.trim().is_empty() {
                        let _ = rl.add_history_entry(&line);
                    }
                    if line_tx.blocking_send(line).is_err() {
                        break;
                    }
                }
                Err(rustyline::error::ReadlineError::Interrupted) => continue,
                Err(rustyline::error::ReadlineError::Eof) => break,
                Err(e) => {
                    eprintln!("readline error: {e}");
                    break;
                }
            }
        }
    });

    while let Some(line) = line_rx.recv().await {
        if line.trim().is_empty() {
            continue;
        }

        let mut req = shell.eval_request();
        req.get().set_text(&line);

        match tokio::time::timeout(std::time::Duration::from_secs(30), req.send().promise).await {
            Ok(Ok(response)) => {
                let result: shell_capnp::shell::eval_results::Reader<'_> = response.get()?;
                let text = result.get_result()?.to_str().unwrap_or("(invalid UTF-8)");
                let is_error = result.get_is_error();

                if text == "exit" && !is_error {
                    break;
                }

                if !text.is_empty() {
                    if is_error {
                        eprintln!("error: {text}");
                    } else {
                        println!("{text}");
                    }
                }
            }
            Ok(Err(e)) => {
                eprintln!("RPC error: {e}");
                break;
            }
            Err(_) => {
                eprintln!("eval timeout (30s)");
            }
        }
    }

    Ok(())
}
