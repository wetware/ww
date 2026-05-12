//! `ww shell` — thin REPL client connecting to a local daemon via UDS.
//!
//! Today, only the local-UDS path is implemented:
//!
//! ```text
//! ww shell                          # connect to ~/.ww/run/<peer-id>.sock
//! ww shell <multiaddr>              # NOT IMPLEMENTED (forward-stable CLI surface)
//! ww shell --discover               # NOT IMPLEMENTED (mDNS browse, forward-stable)
//! ww shell <multiaddr> --discover   # NOT IMPLEMENTED (multiaddr precedence rule)
//! ```
//!
//! FS permissions on `~/.ww/run/` are the auth boundary for the UDS path —
//! matching the convention of `docker.sock`, `~/.ipfs/api`, `~/.podman/podman.sock`.
//! Remote shell support (libp2p with Noise + Terminal auth) is a follow-up.

use anyhow::{Context, Result};
use libp2p::Multiaddr;
use std::path::PathBuf;
use tokio::net::UnixStream;
use tokio_util::compat::TokioAsyncReadCompatExt;

use ww::rpc::vat_dial;
use ww::shell_capnp;

/// Discover a local daemon by scanning `~/.ww/run/` (and `/var/run/ww/`) for
/// `<peer-id>.sock` files. Returns the path to the chosen socket.
///
/// - 0 daemons found → error with a hint.
/// - 1 daemon found → return its socket path.
/// - >1 daemons found → prompt the user to choose.
fn discover_socket() -> Result<PathBuf> {
    let nodes = ww::discovery::list_local_nodes();
    match nodes.len() {
        0 => anyhow::bail!("no local wetware daemons found\n  Start one with: ww run ."),
        1 => {
            let node = &nodes[0];
            eprintln!(
                "Connecting to {} ({})...",
                node.peer_id,
                node.socket_path.display()
            );
            Ok(node.socket_path.clone())
        }
        _ => {
            eprintln!("Multiple wetware daemons found:\n");
            for (i, node) in nodes.iter().enumerate() {
                eprintln!(
                    "  [{}] {} ({})",
                    i + 1,
                    node.peer_id,
                    node.socket_path.display()
                );
            }
            eprintln!();
            eprint!("Select daemon [1-{}]: ", nodes.len());
            let mut input = String::new();
            std::io::stdin()
                .read_line(&mut input)
                .context("failed to read selection")?;
            let choice: usize = input.trim().parse::<usize>().context("invalid selection")?;
            if choice == 0 || choice > nodes.len() {
                anyhow::bail!("selection out of range");
            }
            let node = &nodes[choice - 1];
            eprintln!(
                "Connecting to {} ({})...",
                node.peer_id,
                node.socket_path.display()
            );
            Ok(node.socket_path.clone())
        }
    }
}

/// Shell prompt: dim `/` then `❯`. Rustyline 15 strips ANSI escapes
/// natively in `tty::width`, so no `\x01..\x02` zero-width markers needed.
const PROMPT: &str = "\x1b[2m/\x1b[0m ❯ ";

/// Run the interactive shell client.
///
/// `addr` and `discover` are the forward-stable CLI surface for remote
/// shell access (libp2p multiaddr / mDNS LAN browse). Both currently
/// exit with `Error: NOT IMPLEMENTED` — they exist so future remote
/// support doesn't break the invocation syntax.
///
/// **Caller contract:** must be invoked on a tokio `LocalSet` (the capnp
/// `RpcSystem` is `!Send`). `cli/main.rs` wraps this with
/// `LocalSet::run_until(...)` already.
pub async fn run_shell(addr: Option<Multiaddr>, discover: bool) -> Result<()> {
    // Forward-stable CLI surface; the server-side path isn't built yet.
    // If both ADDR and --discover are given, ADDR takes precedence (per
    // the priority rule in --help) — but both branches end the same way
    // today, so we don't need to distinguish.
    if addr.is_some() || discover {
        eprintln!("Error: NOT IMPLEMENTED");
        std::process::exit(1);
    }

    // 1. Discover the local daemon's socket.
    let socket_path = discover_socket()?;

    // 2. Connect over UDS. No Noise, no Yamux, no protocol negotiation —
    //    the kernel completes the connect synchronously; we own the read
    //    + write halves of the stream as a single duplex.
    let stream = UnixStream::connect(&socket_path)
        .await
        .with_context(|| format!("connect to {}", socket_path.display()))?;

    // 3. Bootstrap Cap'n Proto RPC via the paved-path helper. The helper
    //    spawns the RpcSystem driver before returning, so the Bootstrap
    //    roundtrip flows immediately and the cell is observably live by
    //    the time the first eval call fires. `.compat()` adapts the
    //    tokio AsyncRead+AsyncWrite UnixStream into the futures::io
    //    traits the helper expects.
    let vat_dial::VatDial {
        bootstrap: shell,
        driver,
    } = vat_dial::connect::<_, shell_capnp::shell::Client>(stream.compat());
    // Surface the eventual RpcSystem outcome for debugging session drops.
    tokio::task::spawn_local(async move {
        if let Ok(Err(e)) = driver.await {
            tracing::debug!("Shell RPC session ended: {e}");
        }
    });

    eprintln!("{}", glia::banner());
    eprintln!("AI agents:  ipfs cat /ipns/releases.wetware.run/.agents/prompt.md");

    // 4. REPL loop. rustyline is blocking, so run it on its own thread
    //    and bridge to the async eval loop via an mpsc channel. Eval
    //    output flows back through an `ExternalPrinter` so it interleaves
    //    with the live prompt instead of smashing into the next prompt
    //    line (rustyline draws the next prompt the moment the line is
    //    sent, before the async side has the eval result in hand).
    use rustyline::ExternalPrinter as _;
    let (line_tx, mut line_rx) = tokio::sync::mpsc::channel::<String>(1);
    let (printer_tx, printer_rx) = tokio::sync::oneshot::channel();
    std::thread::spawn(move || {
        let mut rl = rustyline::DefaultEditor::new().expect("failed to create editor");
        let printer = rl
            .create_external_printer()
            .expect("failed to create external printer");
        let _ = printer_tx.send(printer);
        loop {
            match rl.readline(PROMPT) {
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

    let mut printer = printer_rx
        .await
        .context("rustyline thread failed to initialize external printer")?;

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
                    let out = if is_error {
                        format!("error: {text}\n")
                    } else {
                        format!("{text}\n")
                    };
                    let _ = printer.print(out);
                }
            }
            Ok(Err(e)) => {
                let _ = printer.print(format!("RPC error: {e}\n"));
                break;
            }
            Err(_) => {
                let _ = printer.print("eval timeout (30s)\n".to_string());
            }
        }
    }

    Ok(())
}
