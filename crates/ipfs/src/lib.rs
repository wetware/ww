//! IPFS client API for interacting with IPFS nodes.
//!
//! Provides a thin HTTP wrapper around Kubo's `/api/v0/*` endpoints. This is
//! an internal helper — guests do not receive an IPFS capability. Content
//! is served to guests through the WASI virtual filesystem (`CidTree`).

use std::path::Path;

use anyhow::{Context, Result};
use async_trait::async_trait;
use tokio::io::{AsyncWrite, AsyncWriteExt};

/// HTTP client for talking to a Kubo node's `/api/v0/*` endpoints.
#[derive(Clone)]
pub struct HttpClient {
    pub(crate) http_client: reqwest::Client,
    pub(crate) base_url: String,
}

impl HttpClient {
    /// Create a new IPFS client with the given HTTP API endpoint URL.
    pub fn new(ipfs_url: String) -> Self {
        Self {
            http_client: reqwest::Client::new(),
            base_url: ipfs_url.trim_end_matches('/').to_string(),
        }
    }

    /// The Kubo HTTP API base URL, for diagnostics/logging.
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    async fn cat_response(&self, path: &str) -> Result<reqwest::Response> {
        let url = format!("{}/api/v0/cat?arg={}", self.base_url, path);
        let response = self
            .http_client
            .post(&url)
            .send()
            .await
            .with_context(|| format!("Failed to connect to IPFS node at {}", self.base_url))?;

        if !response.status().is_success() {
            return Err(anyhow::anyhow!(
                "Failed to retrieve file from IPFS: {} (path: {})",
                response.status(),
                path
            ));
        }

        Ok(response)
    }

    /// Fetch file content from an IPFS path (calls `/api/v0/cat`).
    ///
    /// Accepts IPFS-family paths: `/ipfs/<cid>`, `/ipns/...`, `/ipld/...`.
    pub async fn cat(&self, path: &str) -> Result<Vec<u8>> {
        let response = self.cat_response(path).await?;

        response
            .bytes()
            .await
            .with_context(|| format!("Failed to read IPFS content from {path}"))
            .map(|b| b.to_vec())
    }

    /// Stream file content from an IPFS path into an [`AsyncWrite`] sink.
    ///
    /// Returns the total number of bytes written.
    pub async fn cat_to_writer<W>(&self, path: &str, dst: &mut W) -> Result<u64>
    where
        W: AsyncWrite + Unpin,
    {
        let mut response = self.cat_response(path).await?;
        let mut total = 0u64;

        while let Some(chunk) = response
            .chunk()
            .await
            .with_context(|| format!("Failed to read streaming chunk from {path}"))?
        {
            dst.write_all(&chunk)
                .await
                .with_context(|| format!("Failed writing streaming chunk from {path}"))?;
            total += chunk.len() as u64;
        }

        dst.flush()
            .await
            .with_context(|| format!("Failed to flush streaming sink for {path}"))?;
        Ok(total)
    }

    /// Stream file content from an IPFS path directly to `dst`.
    ///
    /// Accepts IPFS-family paths: `/ipfs/<cid>`, `/ipns/...`, `/ipld/...`.
    pub async fn cat_to_path(&self, path: &str, dst: &Path) -> Result<()> {
        if let Some(parent) = dst.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .with_context(|| format!("Failed to create parent dir {}", parent.display()))?;
        }

        let mut out = tokio::fs::File::create(dst)
            .await
            .with_context(|| format!("Failed to create destination file {}", dst.display()))?;
        self.cat_to_writer(path, &mut out).await?;
        Ok(())
    }

    /// Fetch an IPFS directory and extract it to a local path.
    ///
    /// Uses kubo's `/api/v0/get?arg=<path>&archive=true` which returns the
    /// directory contents as a TAR archive. The top-level CID directory in
    /// the TAR is stripped so files land directly under `dst`.
    pub async fn get_dir(&self, ipfs_path: &str, dst: &Path) -> Result<()> {
        let url = format!(
            "{}/api/v0/get?arg={}&archive=true",
            self.base_url, ipfs_path
        );
        let response =
            self.http_client.post(&url).send().await.with_context(|| {
                format!("Failed to fetch IPFS directory from {}", self.base_url)
            })?;

        if !response.status().is_success() {
            anyhow::bail!(
                "Failed to fetch IPFS directory: {} (path: {})",
                response.status(),
                ipfs_path
            );
        }

        let bytes = response
            .bytes()
            .await
            .with_context(|| format!("Failed to read IPFS archive from {ipfs_path}"))?;

        let cursor = std::io::Cursor::new(bytes);
        let mut archive = tar::Archive::new(cursor);

        // Kubo wraps the directory in a top-level entry named after the CID.
        // Strip that prefix so files land directly under dst.
        for entry in archive.entries()? {
            let mut entry = entry?;
            let path = entry.path()?.into_owned();

            let stripped: std::path::PathBuf = path.components().skip(1).collect();
            if stripped.as_os_str().is_empty() {
                continue;
            }

            let target = dst.join(&stripped);
            if entry.header().entry_type().is_dir() {
                std::fs::create_dir_all(&target)?;
            } else {
                if let Some(parent) = target.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                let mut file = std::fs::File::create(&target)?;
                std::io::copy(&mut entry, &mut file)?;
            }
        }

        Ok(())
    }

    /// Resolve an IPNS name to a CID path.
    ///
    /// Calls `/api/v0/name/resolve` on the IPFS node. The name should be a
    /// peer ID or IPNS key name (e.g., "k51qzi5uqu5d..." or "self").
    ///
    /// Returns the resolved path (e.g., "/ipfs/QmHash...").
    pub async fn name_resolve(&self, name: &str) -> anyhow::Result<String> {
        let url = format!(
            "{}/api/v0/name/resolve?arg={}&nocache=true",
            self.base_url, name
        );
        let response = self
            .http_client
            .post(&url)
            .send()
            .await
            .context("IPNS name resolve request failed")?;
        let status = response.status();
        let body: serde_json::Value = response
            .json()
            .await
            .context("Failed to parse name resolve response")?;
        if !status.is_success() {
            let msg = body["Message"].as_str().unwrap_or("unknown error");
            anyhow::bail!("IPNS name resolve failed ({}): {}", status, msg);
        }
        body["Path"]
            .as_str()
            .map(|s| s.to_string())
            .ok_or_else(|| anyhow::anyhow!("name resolve response missing Path field"))
    }

    /// Publish a CID under this node's IPNS key.
    ///
    /// Calls `/api/v0/name/publish` on the IPFS node. The path should be an
    /// IPFS path (e.g., "/ipfs/QmHash...").
    ///
    /// The `key` parameter selects which IPNS key to publish under
    /// (default: "self" for the node's identity key).
    pub async fn name_publish(&self, path: &str, key: &str) -> anyhow::Result<String> {
        let url = format!(
            "{}/api/v0/name/publish?arg={}&key={}",
            self.base_url, path, key
        );
        let response = self
            .http_client
            .post(&url)
            .send()
            .await
            .context("IPNS name publish request failed")?;
        let status = response.status();
        let body: serde_json::Value = response
            .json()
            .await
            .context("Failed to parse name publish response")?;
        if !status.is_success() {
            let msg = body["Message"].as_str().unwrap_or("unknown error");
            anyhow::bail!("IPNS name publish failed ({}): {}", status, msg);
        }
        body["Name"]
            .as_str()
            .map(|s| s.to_string())
            .ok_or_else(|| anyhow::anyhow!("name publish response missing Name field"))
    }

    /// List IPNS key names on the local Kubo node.
    pub async fn key_list(&self) -> anyhow::Result<Vec<String>> {
        let url = format!("{}/api/v0/key/list", self.base_url);
        let response = self
            .http_client
            .post(&url)
            .send()
            .await
            .context("IPFS key list request failed")?;
        let status = response.status();
        let body: serde_json::Value = response
            .json()
            .await
            .context("Failed to parse key list response")?;
        if !status.is_success() {
            let msg = body["Message"].as_str().unwrap_or("unknown error");
            anyhow::bail!("IPFS key list failed ({}): {}", status, msg);
        }
        let keys = body["Keys"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|k| k["Name"].as_str().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default();
        Ok(keys)
    }

    /// Generate a new Ed25519 IPNS key. Returns the key's peer ID.
    pub async fn key_gen(&self, name: &str) -> anyhow::Result<String> {
        let url = format!("{}/api/v0/key/gen?arg={}&type=ed25519", self.base_url, name);
        let response = self
            .http_client
            .post(&url)
            .send()
            .await
            .context("IPFS key gen request failed")?;
        let status = response.status();
        let body: serde_json::Value = response
            .json()
            .await
            .context("Failed to parse key gen response")?;
        if !status.is_success() {
            let msg = body["Message"].as_str().unwrap_or("unknown error");
            anyhow::bail!("IPFS key gen failed ({}): {}", status, msg);
        }
        body["Id"]
            .as_str()
            .map(|s| s.to_string())
            .ok_or_else(|| anyhow::anyhow!("key gen response missing Id field"))
    }
}

/// Directory listing entry from Kubo's `/api/v0/ls` endpoint.
#[derive(Debug, Clone)]
pub struct LsEntry {
    pub name: String,
    pub hash: String,
    pub size: u64,
    pub entry_type: u32, // 1 = directory, 2 = file
}

impl HttpClient {
    /// List directory entries at an IPFS path.
    ///
    /// Calls Kubo's `/api/v0/ls?arg=<path>` and parses the JSON response.
    pub async fn ls(&self, path: &str) -> Result<Vec<LsEntry>> {
        let url = format!("{}/api/v0/ls?arg={}", self.base_url, path);
        let response = self
            .http_client
            .post(&url)
            .send()
            .await
            .with_context(|| format!("Failed to ls IPFS path {path}"))?;

        if !response.status().is_success() {
            anyhow::bail!("IPFS ls failed: {} (path: {})", response.status(), path);
        }

        let body: serde_json::Value = response
            .json()
            .await
            .with_context(|| format!("Failed to parse IPFS ls response for {path}"))?;

        let links = body["Objects"]
            .as_array()
            .and_then(|objs| objs.first())
            .and_then(|obj| obj["Links"].as_array())
            .unwrap_or(&Vec::new())
            .clone();

        let entries = links
            .iter()
            .map(|link| LsEntry {
                name: link["Name"].as_str().unwrap_or("").to_string(),
                hash: link["Hash"].as_str().unwrap_or("").to_string(),
                size: link["Size"].as_u64().unwrap_or(0),
                entry_type: link["Type"].as_u64().unwrap_or(0) as u32,
            })
            .collect();

        Ok(entries)
    }

    /// Pin a CID on the IPFS node.
    ///
    /// Calls Kubo's `/api/v0/pin/add?arg=<cid>`.
    pub async fn pin_add(&self, cid: &str) -> Result<()> {
        let url = format!("{}/api/v0/pin/add?arg={}", self.base_url, cid);
        let response = self
            .http_client
            .post(&url)
            .send()
            .await
            .with_context(|| format!("Failed to pin {cid}"))?;

        if !response.status().is_success() {
            anyhow::bail!("IPFS pin add failed: {} (cid: {})", response.status(), cid);
        }

        Ok(())
    }

    /// Unpin a CID from the IPFS node.
    ///
    /// Calls Kubo's `/api/v0/pin/rm?arg=<cid>`.
    pub async fn pin_rm(&self, cid: &str) -> Result<()> {
        let url = format!("{}/api/v0/pin/rm?arg={}", self.base_url, cid);
        let response = self
            .http_client
            .post(&url)
            .send()
            .await
            .with_context(|| format!("Failed to unpin {cid}"))?;

        if !response.status().is_success() {
            anyhow::bail!("IPFS pin rm failed: {} (cid: {})", response.status(), cid);
        }

        Ok(())
    }

    /// Add a directory tree to IPFS and return the root CID.
    ///
    /// Adds all files in the directory to IPFS and returns
    /// the CID of the root directory. Skips common build artifacts.
    pub async fn add_dir(&self, dir_path: &Path) -> Result<String> {
        use std::collections::VecDeque;
        use std::fs;

        let url = format!(
            "{}/api/v0/add?wrap-with-directory=true&progress=false",
            self.base_url
        );
        let mut form = reqwest::multipart::Form::new();

        // Collect all files first to get directory structure right
        let mut files_to_add: Vec<(String, Vec<u8>)> = Vec::new();

        // Use iterative approach to collect all files
        let mut queue = VecDeque::new();
        queue.push_back((dir_path.to_path_buf(), String::new()));

        while let Some((current_dir, prefix)) = queue.pop_front() {
            let entries = fs::read_dir(&current_dir)
                .with_context(|| format!("Failed to read directory: {}", current_dir.display()))?;

            let mut dir_entries: Vec<_> = entries.collect::<std::result::Result<Vec<_>, _>>()?;
            // Sort for consistent ordering
            dir_entries.sort_by_key(|e| e.file_name());

            for entry in dir_entries {
                let path = entry.path();
                let file_name = entry.file_name();
                let file_name_str = file_name.to_string_lossy().to_string();

                // Skip Cargo build artifacts and version control
                if file_name_str == "target"
                    || file_name_str == ".git"
                    || file_name_str == ".gitignore"
                    || file_name_str == "Cargo.lock"
                {
                    continue;
                }

                let rel_path = if prefix.is_empty() {
                    file_name_str.clone()
                } else {
                    format!("{}/{}", prefix, file_name_str)
                };

                if path.is_dir() {
                    // Queue subdirectories for processing
                    queue.push_back((path, rel_path));
                } else if path.is_file() {
                    // Read regular files only. Non-regular entries (UDS
                    // sockets, FIFOs, dangling symlinks, etc.) would fail
                    // `fs::read` with platform-specific errors and aren't
                    // meaningful as IPFS-added content. The admin UDS
                    // endpoint at `~/.ww/run/<peer-id>.sock` is the
                    // concrete case that motivated this guard.
                    let bytes = fs::read(&path)
                        .with_context(|| format!("Failed to read file: {}", path.display()))?;
                    files_to_add.push((rel_path, bytes));
                }
                // (else: silently skip non-regular non-directory entries)
            }
        }

        // Add files to form in sorted order for consistent structure
        for (path, bytes) in files_to_add {
            let part = reqwest::multipart::Part::bytes(bytes);
            form = form.part("file".to_string(), part.file_name(path));
        }

        let response = self
            .http_client
            .post(&url)
            .multipart(form)
            .send()
            .await
            .context("Failed to add directory to IPFS")?;

        if !response.status().is_success() {
            anyhow::bail!("IPFS add failed: {}", response.status());
        }

        let body = response.text().await?;

        // Parse all lines and find the wrapped root directory
        // With wrap-with-directory=true, the last entry is the wrapping directory
        for line in body.lines().rev() {
            if let Ok(json) = serde_json::from_str::<serde_json::Value>(line) {
                if let Some(hash) = json.get("Hash").and_then(|h| h.as_str()) {
                    return Ok(hash.to_string());
                }
            }
        }

        anyhow::bail!("Failed to extract CID from IPFS response")
    }
}

// ── Routing / Node Identity API ────────────────────────────────────

/// Node identity returned by Kubo's `/api/v0/id` endpoint.
///
/// Used by the Wetware swarm to bootstrap the in-process Kademlia client
/// against the local Kubo node (Amino DHT).
#[derive(Debug, Clone)]
pub struct KuboInfo {
    /// Kubo's libp2p peer ID as a base58-encoded string (e.g. `"12D3KooW..."`).
    pub peer_id: String,
    /// Kubo's swarm listen addresses (may include `/p2p/<peer-id>` suffix).
    pub swarm_addrs: Vec<String>,
}

impl HttpClient {
    /// Fetch the local Kubo node's identity for Kad bootstrap.
    ///
    /// Calls `POST /api/v0/id` and returns the peer ID and swarm addresses.
    /// Returns an error if Kubo is not reachable.
    pub async fn kubo_info(&self) -> Result<KuboInfo> {
        let url = format!("{}/api/v0/id", self.base_url);
        let response = self
            .http_client
            .post(&url)
            .send()
            .await
            .with_context(|| format!("kubo id failed: {}", self.base_url))?;

        if !response.status().is_success() {
            anyhow::bail!("kubo id failed: {}", response.status());
        }

        let body: serde_json::Value = response
            .json()
            .await
            .context("kubo id: failed to parse JSON response")?;

        let peer_id = body["ID"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("kubo id: missing ID field"))?
            .to_string();

        let swarm_addrs = body["Addresses"]
            .as_array()
            .unwrap_or(&Vec::new())
            .iter()
            .filter_map(|v| v.as_str().map(|s| s.to_string()))
            .collect();

        Ok(KuboInfo {
            peer_id,
            swarm_addrs,
        })
    }

    /// Fetch Kubo's connected swarm peers.
    ///
    /// Calls `POST /api/v0/swarm/peers` and returns `(PeerId, Multiaddr)` pairs.
    /// Used to populate the in-process Kad client's routing table so iterative
    /// queries have enough peers to converge.
    pub async fn swarm_peers(&self) -> Result<Vec<(String, String)>> {
        let url = format!("{}/api/v0/swarm/peers", self.base_url);
        let response = self
            .http_client
            .post(&url)
            .send()
            .await
            .with_context(|| format!("kubo swarm/peers failed: {}", self.base_url))?;

        if !response.status().is_success() {
            anyhow::bail!("kubo swarm/peers failed: {}", response.status());
        }

        let body: serde_json::Value = response
            .json()
            .await
            .context("kubo swarm/peers: failed to parse JSON response")?;

        let peers = body["Peers"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|entry| {
                        let peer_id = entry["Peer"].as_str()?.to_string();
                        let addr = entry["Addr"].as_str()?.to_string();
                        Some((peer_id, addr))
                    })
                    .collect()
            })
            .unwrap_or_default();

        Ok(peers)
    }

    /// Add raw bytes to IPFS, returning the CID.
    ///
    /// Calls Kubo's `POST /api/v0/add`.
    pub async fn add_bytes(&self, data: &[u8]) -> Result<String> {
        let url = format!("{}/api/v0/add", self.base_url);
        let part = reqwest::multipart::Part::bytes(data.to_vec()).file_name("data");
        let form = reqwest::multipart::Form::new().part("file", part);

        let response = self
            .http_client
            .post(&url)
            .multipart(form)
            .send()
            .await
            .context("ipfs add failed")?;

        if !response.status().is_success() {
            let body = response.text().await.unwrap_or_default();
            anyhow::bail!("ipfs add failed: {body}");
        }

        let body: serde_json::Value = response.json().await?;
        body.get("Hash")
            .and_then(|h| h.as_str())
            .map(|s| s.to_string())
            .ok_or_else(|| anyhow::anyhow!("ipfs add: missing Hash in response"))
    }

    /// Find providers of a CID via Kubo's routing API.
    ///
    /// Calls `POST /api/v0/routing/findprovs?arg=<cid>&num-providers=<n>`.
    /// Returns `(peer_id, Vec<multiaddr>)` pairs for each provider found.
    /// The response is NDJSON; we collect entries with `Type == 4` (provider).
    pub async fn find_providers(
        &self,
        cid: &str,
        num_providers: usize,
    ) -> Result<Vec<(String, Vec<String>)>> {
        let url = format!(
            "{}/api/v0/routing/findprovs?arg={}&num-providers={}",
            self.base_url, cid, num_providers
        );
        let response = self
            .http_client
            .post(&url)
            .send()
            .await
            .with_context(|| format!("kubo routing/findprovs failed: {}", self.base_url))?;

        if !response.status().is_success() {
            anyhow::bail!("kubo routing/findprovs failed: {}", response.status());
        }

        let body = response
            .text()
            .await
            .context("kubo routing/findprovs: failed to read response")?;

        let mut providers = Vec::new();
        for line in body.lines() {
            let entry: serde_json::Value = match serde_json::from_str(line) {
                Ok(v) => v,
                Err(_) => continue,
            };
            // Type 4 = provider response in Kubo's routing API
            if entry.get("Type").and_then(|t| t.as_u64()) != Some(4) {
                continue;
            }
            if let Some(responses) = entry.get("Responses").and_then(|r| r.as_array()) {
                for resp in responses {
                    let peer_id = match resp.get("ID").and_then(|id| id.as_str()) {
                        Some(id) => id.to_string(),
                        None => continue,
                    };
                    let addrs: Vec<String> = resp
                        .get("Addrs")
                        .and_then(|a| a.as_array())
                        .map(|arr| {
                            arr.iter()
                                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                                .collect()
                        })
                        .unwrap_or_default();
                    providers.push((peer_id, addrs));
                }
            }
        }

        Ok(providers)
    }
}

// ── Pinner impl for cache crate ────────────────────────────────────

#[async_trait]
impl cache::Pinner for HttpClient {
    async fn pin(&self, cid: &cid::Cid) -> Result<()> {
        self.pin_add(&cid.to_string()).await
    }

    async fn unpin(&self, cid: &cid::Cid) -> Result<()> {
        self.pin_rm(&cid.to_string()).await
    }

    async fn fetch(&self, cid: &cid::Cid) -> Result<Vec<u8>> {
        self.cat(&format!("/ipfs/{cid}")).await
    }

    async fn fetch_path(&self, cid: &cid::Cid, subpath: &str) -> Result<Vec<u8>> {
        let path = if subpath.is_empty() {
            format!("/ipfs/{cid}")
        } else {
            format!("/ipfs/{cid}/{subpath}")
        };
        self.cat(&path).await
    }

    async fn fetch_to_path(&self, cid: &cid::Cid, dst: &Path) -> Result<()> {
        self.cat_to_path(&format!("/ipfs/{cid}"), dst).await
    }

    async fn fetch_path_to_path(&self, cid: &cid::Cid, subpath: &str, dst: &Path) -> Result<()> {
        let path = if subpath.is_empty() {
            format!("/ipfs/{cid}")
        } else {
            format!("/ipfs/{cid}/{subpath}")
        };
        self.cat_to_path(&path, dst).await
    }

    async fn size(&self, cid: &cid::Cid) -> Result<u64> {
        let entries = self.ls(&format!("/ipfs/{cid}")).await?;
        Ok(entries.iter().map(|e| e.size).sum())
    }
}

/// Check if a path is a valid IPFS-family path (IPFS, IPNS, or IPLD)
///
/// This centralizes IPFS path validation similar to Go's `path.NewPath(str)`.
/// Returns true if the path starts with a valid IPFS namespace prefix.
pub fn is_ipfs_path(path: &str) -> bool {
    path.starts_with("/ipfs/") || path.starts_with("/ipns/") || path.starts_with("/ipld/")
}

// ── MFS (Mutable File System) API ──────────────────────────────────

/// MFS directory entry from `/api/v0/files/ls`.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct MfsEntry {
    #[serde(rename = "Name")]
    pub name: String,
    #[serde(rename = "Hash")]
    pub hash: String,
    #[serde(rename = "Size")]
    pub size: u64,
    /// 0 = file, 1 = directory
    #[serde(rename = "Type")]
    pub entry_type: u32,
}

/// MFS stat result from `/api/v0/files/stat`.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct MfsStat {
    #[serde(rename = "Hash")]
    pub hash: String,
    #[serde(rename = "Size")]
    pub size: u64,
    #[serde(rename = "Type")]
    pub entry_type: String,
}

/// Borrowing reference to the MFS API.
pub struct MFS<'a> {
    client: &'a HttpClient,
}

impl HttpClient {
    /// Get a borrowing reference to the MFS API.
    pub fn mfs(&self) -> MFS<'_> {
        MFS { client: self }
    }
}

impl MFS<'_> {
    /// Create a directory in MFS.
    pub async fn files_mkdir(&self, path: &str, parents: bool) -> Result<()> {
        let url = format!(
            "{}/api/v0/files/mkdir?arg={}&parents={}",
            self.client.base_url, path, parents
        );
        let response = self
            .client
            .http_client
            .post(&url)
            .send()
            .await
            .with_context(|| format!("MFS mkdir failed for {path}"))?;

        if !response.status().is_success() {
            let body = response.text().await.unwrap_or_default();
            anyhow::bail!("MFS mkdir failed for {path}: {body}");
        }
        Ok(())
    }

    /// Copy a file or directory in MFS. Source can be `/ipfs/<cid>`.
    pub async fn files_cp(&self, src: &str, dst: &str) -> Result<()> {
        let url = format!(
            "{}/api/v0/files/cp?arg={}&arg={}",
            self.client.base_url, src, dst
        );
        let response = self
            .client
            .http_client
            .post(&url)
            .send()
            .await
            .with_context(|| format!("MFS cp failed: {src} -> {dst}"))?;

        if !response.status().is_success() {
            let body = response.text().await.unwrap_or_default();
            anyhow::bail!("MFS cp failed ({src} -> {dst}): {body}");
        }
        Ok(())
    }

    /// List entries in an MFS directory.
    pub async fn files_ls(&self, path: &str) -> Result<Vec<MfsEntry>> {
        let url = format!(
            "{}/api/v0/files/ls?arg={}&long=true",
            self.client.base_url, path
        );
        let response = self
            .client
            .http_client
            .post(&url)
            .send()
            .await
            .with_context(|| format!("MFS ls failed for {path}"))?;

        if !response.status().is_success() {
            let body = response.text().await.unwrap_or_default();
            anyhow::bail!("MFS ls failed for {path}: {body}");
        }

        let body: serde_json::Value = response
            .json()
            .await
            .with_context(|| format!("Failed to parse MFS ls response for {path}"))?;

        let entries: Vec<MfsEntry> = body
            .get("Entries")
            .and_then(|e| e.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| serde_json::from_value(v.clone()).ok())
                    .collect()
            })
            .unwrap_or_default();

        Ok(entries)
    }

    /// Stat an MFS path, optionally computing its hash.
    pub async fn files_stat(&self, path: &str, hash: bool) -> Result<MfsStat> {
        let url = format!(
            "{}/api/v0/files/stat?arg={}&hash={}",
            self.client.base_url, path, hash
        );
        let response = self
            .client
            .http_client
            .post(&url)
            .send()
            .await
            .with_context(|| format!("MFS stat failed for {path}"))?;

        if !response.status().is_success() {
            let body = response.text().await.unwrap_or_default();
            anyhow::bail!("MFS stat failed for {path}: {body}");
        }

        response
            .json()
            .await
            .with_context(|| format!("Failed to parse MFS stat response for {path}"))
    }

    /// Remove an MFS path.
    pub async fn files_rm(&self, path: &str, recursive: bool) -> Result<()> {
        let url = format!(
            "{}/api/v0/files/rm?arg={}&recursive={}",
            self.client.base_url, path, recursive
        );
        let response = self
            .client
            .http_client
            .post(&url)
            .send()
            .await
            .with_context(|| format!("MFS rm failed for {path}"))?;

        if !response.status().is_success() {
            let body = response.text().await.unwrap_or_default();
            anyhow::bail!("MFS rm failed for {path}: {body}");
        }
        Ok(())
    }
}
