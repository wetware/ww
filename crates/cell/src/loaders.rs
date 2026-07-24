//! Loader implementations for resolving bytecode from various sources.
//!
//! This module provides loaders for IPFS paths, host filesystem paths,
//! embedded (compile-time) WASM blobs, and a chain loader that tries
//! multiple loaders in sequence.

use anyhow::{Context, Result};
use async_trait::async_trait;
use std::collections::HashMap;

use crate::Loader;
use ipfs::{is_ipfs_path, HttpClient};
use std::path::Path;

/// IPFS loader that resolves bytecode via Kubo's `/api/v0/cat`.
///
/// Handles IPFS-family paths: `/ipfs/...`, `/ipns/...`, `/ipld/...`.
pub struct IpfsLoader {
    client: HttpClient,
}

impl IpfsLoader {
    pub fn new(client: HttpClient) -> Self {
        Self { client }
    }
}

#[async_trait]
impl Loader for IpfsLoader {
    async fn load(&self, path: &str) -> Result<Vec<u8>> {
        if !is_ipfs_path(path) {
            return Err(anyhow::anyhow!("Not an IPFS path: {path}"));
        }
        self.client.cat(path).await
    }
}

/// Loader that reads directly from whatever host path is provided.
///
/// This is a fallback used when no IPFS or mounted prefix handles the request.
pub struct HostPathLoader;

#[async_trait]
impl Loader for HostPathLoader {
    async fn load(&self, name: &str) -> Result<Vec<u8>> {
        use std::fs;

        let path = Path::new(name);
        if !path.exists() || !path.is_file() {
            return Err(anyhow::anyhow!("File not found: {name}"));
        }

        fs::read(path).with_context(|| format!("Failed to read file: {}", path.display()))
    }
}

/// Loader backed by WASM blobs embedded at compile time via `include_bytes!()`.
///
/// Paths are matched by suffix: a request for `some/prefix/bin/main.wasm` will
/// match an entry registered as `kernel/bin/main.wasm` if the request path ends
/// with that key. This allows the loader to work regardless of how the image
/// root path is constructed (absolute, relative, or sentinel).
#[derive(Default)]
pub struct EmbeddedLoader {
    entries: HashMap<&'static str, &'static [u8]>,
}

impl EmbeddedLoader {
    /// Create an empty embedded loader. Use [`insert`] to register blobs.
    pub fn new() -> Self {
        Self {
            entries: HashMap::new(),
        }
    }

    /// Register an embedded WASM blob under the given path suffix.
    ///
    /// The path should be the FHS-relative path within the image,
    /// e.g. `"kernel/bin/main.wasm"` or `"status/bin/status.wasm"`.
    pub fn insert(mut self, suffix: &'static str, bytes: &'static [u8]) -> Self {
        self.entries.insert(suffix, bytes);
        self
    }
}

#[async_trait]
impl Loader for EmbeddedLoader {
    async fn load(&self, path: &str) -> Result<Vec<u8>> {
        // Try exact match first, then suffix match.
        if let Some(bytes) = self.entries.get(path) {
            return Ok(bytes.to_vec());
        }

        for (suffix, bytes) in &self.entries {
            if path.ends_with(suffix) {
                return Ok(bytes.to_vec());
            }
        }

        Err(anyhow::anyhow!("No embedded resource matches: {path}"))
    }
}

/// Chain loader that tries multiple loaders in sequence
///
/// Attempts each loader in order and returns the first successful result,
/// or accumulates all errors if all loaders fail.
pub struct ChainLoader {
    loaders: Vec<Box<dyn Loader>>,
}

impl ChainLoader {
    /// Create a new chain loader with the given loaders
    pub fn new(loaders: Vec<Box<dyn Loader>>) -> Self {
        Self { loaders }
    }
}

#[async_trait]
impl Loader for ChainLoader {
    async fn load(&self, path: &str) -> Result<Vec<u8>> {
        let mut errors = Vec::new();
        for (i, loader) in self.loaders.iter().enumerate() {
            match loader.load(path).await {
                Ok(data) => return Ok(data),
                Err(e) => {
                    errors.push(format!("Loader {i}: {e}"));
                }
            }
        }
        Err(anyhow::anyhow!(
            "All loaders failed for '{}': {}",
            path,
            errors.join("; ")
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[tokio::test]
    async fn test_host_path_loader_reads_file() {
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        tmp.write_all(b"wasm-bytes").unwrap();

        let loader = HostPathLoader;
        let data = loader.load(tmp.path().to_str().unwrap()).await.unwrap();
        assert_eq!(data, b"wasm-bytes");
    }

    #[tokio::test]
    async fn test_host_path_loader_missing_file() {
        let loader = HostPathLoader;
        let err = loader.load("/no/such/file.wasm").await;
        assert!(err.is_err());
        assert!(err.unwrap_err().to_string().contains("File not found"));
    }

    #[tokio::test]
    async fn test_host_path_loader_rejects_directory() {
        let dir = tempfile::tempdir().unwrap();
        let loader = HostPathLoader;
        let err = loader.load(dir.path().to_str().unwrap()).await;
        assert!(err.is_err());
    }

    #[tokio::test]
    async fn test_chain_loader_first_success_wins() {
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        tmp.write_all(b"data").unwrap();
        let path = tmp.path().to_str().unwrap().to_string();

        let chain = ChainLoader::new(vec![Box::new(HostPathLoader)]);
        let data = chain.load(&path).await.unwrap();
        assert_eq!(data, b"data");
    }

    #[tokio::test]
    async fn test_chain_loader_falls_through() {
        // First loader fails (bad path), second succeeds
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        tmp.write_all(b"fallback").unwrap();
        let good_path = tmp.path().to_str().unwrap().to_string();

        struct FailLoader;
        #[async_trait]
        impl Loader for FailLoader {
            async fn load(&self, _path: &str) -> Result<Vec<u8>> {
                Err(anyhow::anyhow!("always fails"))
            }
        }

        // FailLoader first, then HostPathLoader
        let chain = ChainLoader::new(vec![Box::new(FailLoader), Box::new(HostPathLoader)]);
        let data = chain.load(&good_path).await.unwrap();
        assert_eq!(data, b"fallback");
    }

    #[tokio::test]
    async fn test_chain_loader_all_fail() {
        struct FailLoader;
        #[async_trait]
        impl Loader for FailLoader {
            async fn load(&self, _path: &str) -> Result<Vec<u8>> {
                Err(anyhow::anyhow!("nope"))
            }
        }

        let chain = ChainLoader::new(vec![Box::new(FailLoader), Box::new(FailLoader)]);
        let err = chain.load("anything").await;
        assert!(err.is_err());
        let msg = err.unwrap_err().to_string();
        assert!(msg.contains("All loaders failed"));
        assert!(msg.contains("Loader 0"));
        assert!(msg.contains("Loader 1"));
    }

    #[tokio::test]
    async fn test_chain_loader_empty_chain_fails() {
        let chain = ChainLoader::new(vec![]);
        let err = chain.load("any").await;
        assert!(err.is_err());
    }

    // --- EmbeddedLoader tests ---

    #[tokio::test]
    async fn test_embedded_loader_exact_match() {
        let loader = EmbeddedLoader::new().insert("kernel/bin/main.wasm", b"kernel-bytes");
        let data = loader.load("kernel/bin/main.wasm").await.unwrap();
        assert_eq!(data, b"kernel-bytes");
    }

    #[tokio::test]
    async fn test_embedded_loader_suffix_match() {
        let loader = EmbeddedLoader::new().insert("kernel/bin/main.wasm", b"kernel-bytes");
        // Simulate an absolute or prefixed path that ends with the registered suffix
        let data = loader
            .load("/home/user/.ww/images/kernel/bin/main.wasm")
            .await
            .unwrap();
        assert_eq!(data, b"kernel-bytes");
    }

    #[tokio::test]
    async fn test_embedded_loader_no_match() {
        let loader = EmbeddedLoader::new().insert("kernel/bin/main.wasm", b"kernel-bytes");
        let err = loader.load("shell/bin/main.wasm").await;
        assert!(err.is_err());
        assert!(err
            .unwrap_err()
            .to_string()
            .contains("No embedded resource"));
    }

    #[tokio::test]
    async fn test_embedded_loader_empty() {
        let loader = EmbeddedLoader::new();
        let err = loader.load("anything").await;
        assert!(err.is_err());
    }

    #[tokio::test]
    async fn test_chain_host_overrides_embedded() {
        // HostPath should win over Embedded when a local file exists.
        // Write a temp file whose path ends with a known static suffix.
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("bin/main.wasm");
        std::fs::create_dir_all(file_path.parent().unwrap()).unwrap();
        std::fs::write(&file_path, b"local-bytes").unwrap();

        // Embedded has the same suffix registered.
        let embedded = EmbeddedLoader::new().insert("bin/main.wasm", b"embedded-bytes");
        let chain = ChainLoader::new(vec![Box::new(HostPathLoader), Box::new(embedded)]);
        let data = chain.load(file_path.to_str().unwrap()).await.unwrap();
        assert_eq!(data, b"local-bytes", "HostPath should override Embedded");
    }

    #[tokio::test]
    async fn test_chain_embedded_fallback() {
        // When HostPath misses, Embedded should provide the bytes
        let embedded =
            EmbeddedLoader::new().insert("nonexistent/bin/main.wasm", b"embedded-fallback");
        let chain = ChainLoader::new(vec![Box::new(HostPathLoader), Box::new(embedded)]);
        let data = chain.load("nonexistent/bin/main.wasm").await.unwrap();
        assert_eq!(data, b"embedded-fallback");
    }

    /// Compile-time check: EmbeddedLoader must be Send + Sync for use in ChainLoader.
    fn _assert_send_sync() {
        fn is_send_sync<T: Send + Sync>() {}
        is_send_sync::<EmbeddedLoader>();
    }
}
