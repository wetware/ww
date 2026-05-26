use anyhow::Result;

/// Check the development environment.
#[allow(clippy::unused_async)]
pub(super) async fn doctor() -> Result<()> {
    let mut all_required_ok = true;

    // Required: Rust toolchain
    let rustc = std::process::Command::new("rustc")
        .arg("--version")
        .output();
    match rustc {
        Ok(out) if out.status.success() => {
            let ver = String::from_utf8_lossy(&out.stdout).trim().to_string();
            println!("  Rust toolchain .............. OK ({ver})");
        }
        _ => {
            println!("  Rust toolchain .............. MISSING");
            println!("    Fix: curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh");
            all_required_ok = false;
        }
    }

    // Required: wasm32-wasip2 target
    let targets = std::process::Command::new("rustup")
        .args(["target", "list", "--installed"])
        .output();
    match targets {
        Ok(out) if out.status.success() => {
            let list = String::from_utf8_lossy(&out.stdout);
            if list.contains("wasm32-wasip2") {
                println!("  wasm32-wasip2 target ........ OK");
            } else {
                println!("  wasm32-wasip2 target ........ MISSING");
                println!("    Fix: rustup target add wasm32-wasip2");
                all_required_ok = false;
            }
        }
        _ => {
            println!("  wasm32-wasip2 target ........ UNKNOWN (rustup not found)");
            all_required_ok = false;
        }
    }

    // Required: Cargo
    let cargo = std::process::Command::new("cargo")
        .arg("--version")
        .output();
    match cargo {
        Ok(out) if out.status.success() => {
            let ver = String::from_utf8_lossy(&out.stdout).trim().to_string();
            println!("  Cargo ....................... OK ({ver})");
        }
        _ => {
            println!("  Cargo ....................... MISSING");
            all_required_ok = false;
        }
    }

    // Optional: Kubo
    let ipfs = std::process::Command::new("ipfs").arg("version").output();
    match ipfs {
        Ok(out) if out.status.success() => {
            let ver = String::from_utf8_lossy(&out.stdout).trim().to_string();
            println!("  Kubo (IPFS) ................. OK ({ver})");
        }
        _ => {
            println!("  Kubo (IPFS) ................. NOT FOUND (optional)");
        }
    }

    // Optional: Ollama
    let ollama = std::process::Command::new("ollama")
        .arg("--version")
        .output();
    match ollama {
        Ok(out) if out.status.success() => {
            let ver = String::from_utf8_lossy(&out.stdout).trim().to_string();
            println!("  Ollama ...................... OK ({ver})");
        }
        _ => {
            println!("  Ollama ...................... NOT FOUND (optional)");
        }
    }

    // --- Install state checks ---
    println!();
    println!("Install state:");

    let home = dirs::home_dir();
    let ww_dir = home.as_ref().map(|h| h.join(".ww"));

    // ~/.ww directory
    match &ww_dir {
        Some(d) if d.exists() => {
            println!("  ~/.ww ....................... OK");
        }
        _ => {
            println!("  ~/.ww ....................... NOT FOUND (run: ww perform install)");
        }
    }

    // Identity
    match &ww_dir {
        Some(d) if d.join("identity").exists() => {
            println!("  ~/.ww/identity .............. OK");
        }
        _ => {
            println!("  ~/.ww/identity .............. NOT FOUND");
        }
    }

    // Daemon registered
    if let Some(ref h) = home {
        let plist = h.join("Library/LaunchAgents/io.wetware.ww.plist");
        let systemd = h.join(".config/systemd/user/ww.service");
        if plist.exists() || systemd.exists() {
            // Check if daemon is actually running.
            let running = if cfg!(target_os = "macos") {
                std::process::Command::new("launchctl")
                    .args(["list", "io.wetware.ww"])
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .status()
                    .map(|s| s.success())
                    .unwrap_or(false)
            } else {
                std::process::Command::new("systemctl")
                    .args(["--user", "is-active", "--quiet", "ww"])
                    .status()
                    .map(|s| s.success())
                    .unwrap_or(false)
            };
            if running {
                println!("  Background daemon ........... RUNNING");
            } else {
                println!("  Background daemon ........... REGISTERED (not running)");
                if cfg!(target_os = "macos") {
                    println!("    Start: launchctl load {}", plist.display());
                } else {
                    println!("    Start: systemctl --user start ww");
                }
            }
        } else {
            println!("  Background daemon ........... NOT REGISTERED (run: ww perform install)");
        }
    }

    // Claude Code MCP
    let claude_check = std::process::Command::new("claude")
        .args(["mcp", "list"])
        .output();
    match claude_check {
        Ok(out) if out.status.success() => {
            let list = String::from_utf8_lossy(&out.stdout);
            if list.contains("wetware") {
                println!("  Claude Code MCP ............. CONFIGURED");
            } else {
                println!("  Claude Code MCP ............. NOT CONFIGURED");
                println!("    Fix: claude mcp add wetware -- ww shell --mcp");
            }
        }
        _ => {
            println!("  Claude Code MCP ............. UNKNOWN (claude CLI not found)");
        }
    }

    // --- Namespace checks ---
    println!();
    println!("Namespaces:");

    // Check ~/.ww/etc/ns/ exists and has entries
    let ns_dir = ww_dir.as_ref().map(|d| d.join("etc/ns"));
    match &ns_dir {
        Some(d) if d.is_dir() => match ww::ns::list_configs(d) {
            Ok(configs) if configs.is_empty() => {
                println!("  ~/.ww/etc/ns/ ............... EMPTY (no namespaces configured)");
                println!("    Fix: ww perform install (or: ww ns add ww --ipns <key>)");
            }
            Ok(configs) => {
                for config in &configs {
                    let source = if !config.ipns.is_empty() {
                        format!("ipns={}", &config.ipns[..config.ipns.len().min(20)])
                    } else if !config.bootstrap.is_empty() {
                        format!(
                            "bootstrap={}",
                            &config.bootstrap[..config.bootstrap.len().min(20)]
                        )
                    } else {
                        "unconfigured".to_string()
                    };
                    println!("  ns/{} {:<22} OK ({})", config.name, ".", source);
                }
            }
            Err(_) => {
                println!("  ~/.ww/etc/ns/ ............... ERROR (cannot read)");
            }
        },
        _ => {
            println!("  ~/.ww/etc/ns/ ............... NOT FOUND (run: ww perform install)");
        }
    }

    // Check if Kubo is reachable (daemon running, not just installed)
    let kubo_reachable = std::process::Command::new("ipfs")
        .args(["id", "-f", "<id>"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if kubo_reachable {
        println!("  Kubo daemon ................. REACHABLE");
    } else {
        println!("  Kubo daemon ................. NOT REACHABLE");
        println!("    Start with: ipfs daemon &");
        println!("    (Namespace resolution uses embedded fallback without Kubo)");
    }

    if all_required_ok {
        println!("\nAll required checks passed.");
        Ok(())
    } else {
        println!("\nSome required checks failed. Fix the issues above and re-run.");
        std::process::exit(1);
    }
}
