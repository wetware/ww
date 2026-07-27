//! Namespace resolution for the `etc/ns/` convention.
//!
//! Each file in `etc/ns/` defines a named namespace that maps to an IPFS
//! directory tree. On boot, these namespaces are resolved and mounted as
//! FHS layers.
//!
//! ## File format (`etc/ns/<name>`)
//!
//! ```text
//! # Wetware namespace config
//! ipns=k51qzi5uqu5d...
//! bootstrap=/ipfs/bafyrei...
//! ```
//!
//! - `ipns`: IPNS name for live resolution (updates on publish)
//! - `bootstrap`: fallback CID from build time (works offline after first pin)
//!
//! Lines starting with `#` are comments. Unknown keys are ignored (forward compat).

use anyhow::{bail, Context, Result};
use cid::Cid;
use std::collections::HashMap;
use std::path::Path;

/// A parsed namespace config from `etc/ns/<name>`.
#[derive(Debug, Clone)]
pub struct NamespaceConfig {
    /// Namespace name (filename in `etc/ns/`).
    pub name: String,
    /// IPNS name for live resolution. May be empty.
    pub ipns: String,
    /// Bootstrap IPFS path (e.g. `/ipfs/bafyrei...`). May be empty.
    pub bootstrap: String,
}

impl NamespaceConfig {
    /// Parse a namespace config file.
    pub fn parse(name: &str, content: &str) -> Self {
        let mut fields = HashMap::new();
        for line in content.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if let Some((key, value)) = line.split_once('=') {
                fields.insert(key.trim().to_string(), value.trim().to_string());
            }
        }
        NamespaceConfig {
            name: name.to_string(),
            ipns: fields.remove("ipns").unwrap_or_default(),
            bootstrap: fields.remove("bootstrap").unwrap_or_default(),
        }
    }

    /// Write this config to a file.
    pub fn write_to(&self, path: &Path) -> Result<()> {
        self.validate()?;
        let content = format!(
            "# Wetware namespace: {}\nipns={}\nbootstrap={}\n",
            self.name, self.ipns, self.bootstrap
        );
        std::fs::write(path, content)
            .with_context(|| format!("write namespace config: {}", path.display()))
    }

    /// Validate the namespace configuration before it is used or persisted.
    ///
    /// A bootstrap is deliberately limited to one CID, rather than an
    /// arbitrary IPFS path. It is a stable offline fallback for the namespace,
    /// not a second mount syntax; accepting a subpath here would make a typo
    /// look like a recoverable Kubo failure later in boot.
    pub fn validate(&self) -> Result<()> {
        validate_name(&self.name)?;
        let _ = self.bootstrap_ipfs_path()?;
        Ok(())
    }

    /// Return the bootstrap as a canonical `/ipfs/<cid>` path.
    pub fn bootstrap_ipfs_path(&self) -> Result<Option<String>> {
        if self.bootstrap.is_empty() {
            return Ok(None);
        }

        let value = self
            .bootstrap
            .strip_prefix("/ipfs/")
            .unwrap_or(&self.bootstrap);
        if value.is_empty() {
            bail!("Namespace '{}' has an empty bootstrap CID", self.name);
        }
        if value.contains('/') {
            bail!(
                "Namespace '{}' bootstrap must be a CID, not an IPFS subpath: {}",
                self.name,
                self.bootstrap
            );
        }
        let cid = value
            .parse::<Cid>()
            .with_context(|| format!("Namespace '{}' has an invalid bootstrap CID", self.name))?;
        Ok(Some(format!("/ipfs/{cid}")))
    }

    /// The best IPFS path to use for mounting.
    /// Prefers IPNS (for live updates), falls back to a validated bootstrap CID.
    pub fn ipfs_path(&self) -> Result<Option<String>> {
        if !self.ipns.is_empty() {
            Ok(Some(format!("/ipns/{}", self.ipns)))
        } else {
            self.bootstrap_ipfs_path()
        }
    }
}

/// Scan `etc/ns/` directories from a list of local mount paths.
///
/// Looks for `<mount>/etc/ns/*` in each root mount. Returns all found
/// namespace configs. Later mounts override earlier ones (same name).
pub fn scan_namespace_configs(mount_paths: &[&Path]) -> Result<Vec<NamespaceConfig>> {
    let mut configs: HashMap<String, NamespaceConfig> = HashMap::new();

    for base in mount_paths {
        let ns_dir = base.join("etc/ns");
        if !ns_dir.is_dir() {
            continue;
        }
        let entries = std::fs::read_dir(&ns_dir)
            .with_context(|| format!("read namespace dir: {}", ns_dir.display()))?;
        for entry in entries {
            let entry = entry?;
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                let content = std::fs::read_to_string(&path)
                    .with_context(|| format!("read namespace config: {}", path.display()))?;
                let config = NamespaceConfig::parse(name, &content);
                config.validate()?;
                configs.insert(name.to_string(), config);
            }
        }
    }

    let mut result: Vec<_> = configs.into_values().collect();
    result.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(result)
}

/// Resolve namespace configs into IPFS mount paths.
///
/// For each namespace, tries IPNS resolution first (via the provided resolver),
/// then falls back to the bootstrap CID. Returns a list of (name, ipfs_path) pairs.
pub async fn resolve_namespaces(
    configs: &[NamespaceConfig],
    ipfs_client: &crate::ipfs::BootClient,
    runtime_status: &crate::metrics::RuntimeStatus,
) -> Vec<(String, String)> {
    // Namespace fallback must stay bounded even when the generic boot retry
    // budget is intentionally unbounded. Within this window, BootClient still
    // retries transport and gateway failures; a no-progress stall selects the
    // configured bootstrap rather than holding the whole image indefinitely.
    let resolve_timeout = ipfs_client
        .operation_timeout()
        .unwrap_or(std::time::Duration::from_secs(90));
    let mut resolved = Vec::new();

    for config in configs {
        // Try IPNS resolution first
        if !config.ipns.is_empty() {
            let ipns_path = format!("/ipns/{}", config.ipns);
            let result =
                match tokio::time::timeout(resolve_timeout, ipfs_client.name_resolve(&ipns_path))
                    .await
                {
                    Ok(result) => result,
                    Err(_) => {
                        tracing::warn!(
                            ns = %config.name,
                            ipns = %config.ipns,
                            timeout_secs = resolve_timeout.as_secs(),
                            "IPNS resolution made no progress, trying bootstrap CID"
                        );
                        Err(anyhow::anyhow!("IPNS resolution timed out"))
                    }
                };
            match result {
                Ok(resolved_path) => {
                    tracing::info!(
                        ns = %config.name,
                        ipns = %config.ipns,
                        resolved = %resolved_path,
                        "Namespace resolved via IPNS"
                    );
                    resolved.push((config.name.clone(), resolved_path));
                    continue;
                }
                Err(e) => {
                    runtime_status.mark_degraded(format!(
                        "namespace {} IPNS resolution failed; using bootstrap if available",
                        config.name
                    ));
                    tracing::warn!(
                        ns = %config.name,
                        ipns = %config.ipns,
                        err = %e,
                        "IPNS resolution failed, trying bootstrap CID"
                    );
                }
            }
        }

        // Fall back to bootstrap CID
        if let Some(path) = config
            .bootstrap_ipfs_path()
            .expect("namespace configs are validated before resolution")
        {
            if path.starts_with("/ipfs/") {
                tracing::info!(
                    ns = %config.name,
                    path = %path,
                    "Namespace using bootstrap CID"
                );
                resolved.push((config.name.clone(), path));
            }
        } else {
            runtime_status.mark_degraded(format!(
                "namespace {} could not be resolved and has no bootstrap CID",
                config.name
            ));
            tracing::warn!(
                ns = %config.name,
                "Namespace has no IPNS name or bootstrap CID, skipping"
            );
        }
    }

    resolved
}

/// Validate that a namespace name is safe for use as a filename.
/// Rejects names containing `/`, `\`, `..`, or starting with `.`.
pub fn validate_name(name: &str) -> Result<()> {
    if name.is_empty() {
        bail!("Namespace name cannot be empty");
    }
    if name.contains('/') || name.contains('\\') || name.contains("..") || name.starts_with('.') {
        bail!("Invalid namespace name '{name}': must not contain /, \\, .., or start with .");
    }
    Ok(())
}

/// List namespace configs from a directory (e.g. `~/.ww/etc/ns/`).
pub fn list_configs(ns_dir: &Path) -> Result<Vec<NamespaceConfig>> {
    if !ns_dir.is_dir() {
        return Ok(Vec::new());
    }
    let mut configs = Vec::new();
    let entries = std::fs::read_dir(ns_dir)
        .with_context(|| format!("read namespace dir: {}", ns_dir.display()))?;
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
            let content = std::fs::read_to_string(&path)
                .with_context(|| format!("read namespace config: {}", path.display()))?;
            let config = NamespaceConfig::parse(name, &content);
            config.validate()?;
            configs.push(config);
        }
    }
    Ok(configs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    async fn read_request(stream: &mut tokio::net::TcpStream) {
        let mut request = [0u8; 4096];
        let _ = stream.read(&mut request).await.unwrap();
    }

    async fn accept_identity_then_stall(listener: TcpListener) {
        let (mut identity, _) = listener.accept().await.unwrap();
        read_request(&mut identity).await;
        identity
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Length: 28\r\nConnection: close\r\n\r\n{\"ID\":\"test\",\"Addresses\":[]}",
            )
            .await
            .unwrap();
        drop(identity);

        let (mut stream, _) = listener.accept().await.unwrap();
        read_request(&mut stream).await;
        std::future::pending::<()>().await;
    }

    #[tokio::test]
    async fn stalled_ipns_resolution_uses_bootstrap_and_marks_degraded() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(accept_identity_then_stall(listener));
        let config = NamespaceConfig {
            name: "ww".into(),
            ipns: "k51-test".into(),
            bootstrap: format!("/ipfs/{}", test_cid()),
        };
        let runtime_status = crate::metrics::RuntimeStatus::starting();
        let raw_client = crate::ipfs::HttpClient::new(format!("http://{address}"));

        raw_client
            .kubo_info()
            .await
            .expect("the fake Kubo identity endpoint must succeed first");
        let client = crate::ipfs::BootClient::new(raw_client, 1, 1);

        let resolved = resolve_namespaces(&[config], &client, &runtime_status).await;

        assert_eq!(
            resolved,
            vec![("ww".into(), format!("/ipfs/{}", test_cid()))]
        );
        assert!(runtime_status.is_degraded());
        server.abort();
    }

    #[tokio::test]
    async fn transient_ipns_failure_retries_without_degrading() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut first, _) = listener.accept().await.unwrap();
            read_request(&mut first).await;
            first
                .write_all(b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 18\r\nConnection: close\r\n\r\n{\"Message\":\"busy\"}")
                .await
                .unwrap();

            let (mut second, _) = listener.accept().await.unwrap();
            read_request(&mut second).await;
            second
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Length: 26\r\nConnection: close\r\n\r\n{\"Path\":\"/ipfs/recovered\"}",
                )
                .await
                .unwrap();
        });
        let config = NamespaceConfig {
            name: "ww".into(),
            ipns: "k51-test".into(),
            bootstrap: "/ipfs/bootstrap".into(),
        };
        let runtime_status = crate::metrics::RuntimeStatus::starting();
        let client = crate::ipfs::BootClient::new(
            crate::ipfs::HttpClient::new(format!("http://{address}")),
            2,
            1,
        );

        let resolved = resolve_namespaces(&[config], &client, &runtime_status).await;

        assert_eq!(resolved, vec![("ww".into(), "/ipfs/recovered".into())]);
        assert!(!runtime_status.is_degraded());
        server.await.unwrap();
    }

    fn test_cid() -> String {
        let multihash =
            cid::multihash::Multihash::<64>::wrap(0x00, b"namespace").expect("identity multihash");
        Cid::new_v1(0x55, multihash).to_string()
    }

    #[test]
    fn parse_full_config() {
        let content = "# Wetware namespace: ww\nipns=k51qzi5uqu5d\nbootstrap=/ipfs/bafyrei123\n";
        let config = NamespaceConfig::parse("ww", content);
        assert_eq!(config.name, "ww");
        assert_eq!(config.ipns, "k51qzi5uqu5d");
        assert_eq!(config.bootstrap, "/ipfs/bafyrei123");
    }

    #[test]
    fn parse_comments_and_blanks() {
        let content = "\n# comment\n\nipns = abc \n\n# another comment\nbootstrap = /ipfs/xyz\n";
        let config = NamespaceConfig::parse("test", content);
        assert_eq!(config.ipns, "abc");
        assert_eq!(config.bootstrap, "/ipfs/xyz");
    }

    #[test]
    fn parse_missing_fields() {
        let config = NamespaceConfig::parse("empty", "");
        assert_eq!(config.ipns, "");
        assert_eq!(config.bootstrap, "");
    }

    #[test]
    fn parse_unknown_keys_ignored() {
        let content = "ipns=abc\nfuture_field=42\nbootstrap=/ipfs/xyz\n";
        let config = NamespaceConfig::parse("ww", content);
        assert_eq!(config.ipns, "abc");
        assert_eq!(config.bootstrap, "/ipfs/xyz");
    }

    #[test]
    fn ipfs_path_prefers_ipns() {
        let config = NamespaceConfig {
            name: "ww".into(),
            ipns: "k51abc".into(),
            bootstrap: format!("/ipfs/{}", test_cid()),
        };
        assert_eq!(config.ipfs_path().unwrap(), Some("/ipns/k51abc".into()));
    }

    #[test]
    fn ipfs_path_falls_back_to_bootstrap() {
        let cid = test_cid();
        let config = NamespaceConfig {
            name: "ww".into(),
            ipns: String::new(),
            bootstrap: format!("/ipfs/{cid}"),
        };
        assert_eq!(config.ipfs_path().unwrap(), Some(format!("/ipfs/{cid}")));
    }

    #[test]
    fn ipfs_path_adds_prefix_to_bare_cid() {
        let cid = test_cid();
        let config = NamespaceConfig {
            name: "ww".into(),
            ipns: String::new(),
            bootstrap: cid.clone(),
        };
        assert_eq!(config.ipfs_path().unwrap(), Some(format!("/ipfs/{cid}")));
    }

    #[test]
    fn ipfs_path_none_when_empty() {
        let config = NamespaceConfig {
            name: "ww".into(),
            ipns: String::new(),
            bootstrap: String::new(),
        };
        assert_eq!(config.ipfs_path().unwrap(), None);
    }

    #[test]
    fn write_and_reparse_roundtrip() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let cid = test_cid();
        let config = NamespaceConfig {
            name: "myns".into(),
            ipns: "k51abc".into(),
            bootstrap: format!("/ipfs/{cid}"),
        };
        config.write_to(tmp.path()).unwrap();
        let content = std::fs::read_to_string(tmp.path()).unwrap();
        let reparsed = NamespaceConfig::parse("myns", &content);
        assert_eq!(reparsed.ipns, "k51abc");
        assert_eq!(reparsed.bootstrap, format!("/ipfs/{cid}"));
    }

    #[test]
    fn scan_namespace_configs_from_tempdir() {
        let tmp = tempfile::TempDir::new().unwrap();
        let ns_dir = tmp.path().join("etc/ns");
        std::fs::create_dir_all(&ns_dir).unwrap();
        let cid = test_cid();
        std::fs::write(
            ns_dir.join("ww"),
            format!("ipns=k51abc\nbootstrap=/ipfs/{cid}\n"),
        )
        .unwrap();
        std::fs::write(ns_dir.join("org"), format!("bootstrap={cid}\n")).unwrap();

        let paths = [tmp.path()];
        let refs: Vec<&Path> = paths.to_vec();
        let configs = scan_namespace_configs(&refs).unwrap();
        assert_eq!(configs.len(), 2);
    }

    #[test]
    fn bootstrap_rejects_invalid_cid_before_kubo_resolution() {
        let config = NamespaceConfig {
            name: "ww".into(),
            ipns: "k51abc".into(),
            bootstrap: "/ipfs/not-a-cid".into(),
        };
        let err = config.validate().unwrap_err();
        assert!(err.to_string().contains("invalid bootstrap CID"));
    }

    #[test]
    fn bootstrap_rejects_ipfs_subpath() {
        let config = NamespaceConfig {
            name: "ww".into(),
            ipns: String::new(),
            bootstrap: format!("/ipfs/{}/etc/ns", test_cid()),
        };
        let err = config.validate().unwrap_err();
        assert!(err
            .to_string()
            .contains("must be a CID, not an IPFS subpath"));
    }

    #[test]
    fn scan_skips_missing_ns_dir() {
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = [tmp.path()];
        let refs: Vec<&Path> = paths.to_vec();
        let configs = scan_namespace_configs(&refs).unwrap();
        assert!(configs.is_empty());
    }

    #[test]
    fn validate_name_accepts_simple_names() {
        assert!(validate_name("ww").is_ok());
        assert!(validate_name("myorg").is_ok());
        assert!(validate_name("my-ns").is_ok());
    }

    #[test]
    fn validate_name_rejects_traversal() {
        assert!(validate_name("../etc").is_err());
        assert!(validate_name("foo/bar").is_err());
        assert!(validate_name(".hidden").is_err());
        assert!(validate_name("").is_err());
        assert!(validate_name("foo\\bar").is_err());
    }

    #[test]
    fn list_configs_empty_on_missing_dir() {
        let configs = list_configs(Path::new("/nonexistent/path")).unwrap();
        assert!(configs.is_empty());
    }
}
