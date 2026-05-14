use anyhow::{bail, Context, Result};

/// List configured namespaces from ~/.ww/etc/ns/.
#[allow(clippy::unused_async)]
pub(super) async fn ns_list() -> Result<()> {
    let home = dirs::home_dir().context("Cannot determine home directory")?;
    let ns_dir = home.join(".ww/etc/ns");
    let configs = ww::ns::list_configs(&ns_dir)?;
    if configs.is_empty() {
        println!("No namespaces configured.");
        println!("  Add one: ww ns add <name> --ipns <key>");
        return Ok(());
    }
    println!("{:<12} {:<24} {:<24} PATH", "NAME", "IPNS", "BOOTSTRAP");
    println!("{:<12} {:<24} {:<24} ----", "----", "----", "---------");
    for config in &configs {
        let ipns = if config.ipns.is_empty() {
            "-".to_string()
        } else {
            config.ipns.clone()
        };
        let bootstrap = if config.bootstrap.is_empty() {
            "-".to_string()
        } else {
            config.bootstrap.clone()
        };
        let path = config.ipfs_path().unwrap_or_else(|| "-".to_string());
        println!("{:<12} {:<24} {:<24} {path}", config.name, ipns, bootstrap);
    }
    Ok(())
}

/// Add or update a namespace config.
#[allow(clippy::unused_async)]
pub(super) async fn ns_add(
    name: String,
    ipns: Option<String>,
    bootstrap: Option<String>,
) -> Result<()> {
    ww::ns::validate_name(&name)?;
    let home = dirs::home_dir().context("Cannot determine home directory")?;
    let ns_dir = home.join(".ww/etc/ns");
    std::fs::create_dir_all(&ns_dir)?;
    let ns_path = ns_dir.join(&name);

    // Read existing config if present, then overlay provided values.
    let mut config = if ns_path.exists() {
        let content = std::fs::read_to_string(&ns_path)?;
        ww::ns::NamespaceConfig::parse(&name, &content)
    } else {
        ww::ns::NamespaceConfig {
            name: name.clone(),
            ipns: String::new(),
            bootstrap: String::new(),
        }
    };

    if let Some(key) = ipns {
        config.ipns = key;
    }
    if let Some(cid) = bootstrap {
        config.bootstrap = cid;
    }

    if config.ipns.is_empty() && config.bootstrap.is_empty() {
        bail!("At least one of --ipns or --bootstrap is required");
    }

    config.write_to(&ns_path)?;
    println!("Namespace '{name}' configured at {}", ns_path.display());
    Ok(())
}

/// Remove a namespace config.
#[allow(clippy::unused_async)]
pub(super) async fn ns_remove(name: String) -> Result<()> {
    ww::ns::validate_name(&name)?;
    let home = dirs::home_dir().context("Cannot determine home directory")?;
    let ns_path = home.join(".ww/etc/ns").join(&name);
    if ns_path.exists() {
        std::fs::remove_file(&ns_path)?;
        println!("Namespace '{name}' removed.");
    } else {
        println!("Namespace '{name}' not found.");
    }
    Ok(())
}

/// Resolve a namespace to its current IPFS CID.
pub(super) async fn ns_resolve(name: String) -> Result<()> {
    ww::ns::validate_name(&name)?;
    let home = dirs::home_dir().context("Cannot determine home directory")?;
    let ns_path = home.join(".ww/etc/ns").join(&name);
    if !ns_path.exists() {
        bail!("Namespace '{name}' not configured. Run: ww ns add {name} --ipns <key>");
    }
    let content = std::fs::read_to_string(&ns_path)?;
    let config = ww::ns::NamespaceConfig::parse(&name, &content);

    // Try IPNS resolution
    if !config.ipns.is_empty() {
        let ipfs_client = ww::ipfs::HttpClient::new("http://localhost:5001".into());
        let ipns_path = format!("/ipns/{}", config.ipns);
        match ipfs_client.name_resolve(&ipns_path).await {
            Ok(resolved) => {
                println!("{resolved}");
                return Ok(());
            }
            Err(e) => {
                eprintln!("IPNS resolution failed: {e}");
                eprintln!("Falling back to bootstrap CID...");
            }
        }
    }

    // Fall back to bootstrap
    if !config.bootstrap.is_empty() {
        let path = if config.bootstrap.starts_with("/ipfs/") {
            config.bootstrap.clone()
        } else {
            format!("/ipfs/{}", config.bootstrap)
        };
        println!("{path}");
    } else {
        bail!("Namespace '{name}' has no IPNS name or bootstrap CID");
    }

    Ok(())
}
