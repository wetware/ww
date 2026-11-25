//! Integration tests for IPFS operations.
//!
//! Host CI supplies `WW_TEST_KUBO_ADDR` and requires a reachable daemon with
//! `WW_TEST_REQUIRE_KUBO=1`. Local runs also accept `IPFS_API_URL`.

#[path = "support/kubo.rs"]
mod kubo;

use tempfile::TempDir;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Non-proxy reqwest client (same pattern as atom test helpers).
fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .no_proxy()
        .build()
        .expect("reqwest client")
}

/// Add raw bytes to IPFS, returning the CID. Uses `/api/v0/add`.
async fn ipfs_add_bytes(client: &ww::ipfs::HttpClient, data: &[u8]) -> anyhow::Result<String> {
    let url = format!("{}/api/v0/add?pin=false", client.base_url());
    let part = reqwest::multipart::Part::bytes(data.to_vec()).file_name("data");
    let form = reqwest::multipart::Form::new().part("file", part);
    let resp = http_client()
        .post(&url)
        .multipart(form)
        .send()
        .await?
        .error_for_status()?;
    let body: serde_json::Value = resp.json().await?;
    body.get("Hash")
        .and_then(|h| h.as_str())
        .map(String::from)
        .ok_or_else(|| anyhow::anyhow!("missing Hash in IPFS add response"))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Round-trip: add bytes via the API, then retrieve them through `HttpClient::cat()`.
#[tokio::test]
async fn test_add_and_cat_roundtrip() {
    let Some(client) = kubo::client().await else {
        return;
    };

    let payload = b"hello from wetware integration tests";
    let cid = ipfs_add_bytes(&client, payload).await.expect("ipfs add");

    let fetched = client.cat(&format!("/ipfs/{cid}")).await.expect("ipfs cat");

    assert_eq!(fetched, payload);
}

/// Round-trip: add bytes via `HttpClient::add_bytes()`, retrieve via `cat()`.
#[tokio::test]
async fn test_add_bytes_and_cat_roundtrip() {
    let Some(client) = kubo::client().await else {
        return;
    };

    let payload = b"add_bytes integration test data";

    let cid = client.add_bytes(payload).await.expect("add_bytes");
    assert!(!cid.is_empty(), "CID should not be empty");

    let fetched = client.cat(&format!("/ipfs/{cid}")).await.expect("ipfs cat");

    assert_eq!(fetched, payload);
}

/// Add a directory tree, then retrieve it with `get_dir` and verify contents.
#[tokio::test]
async fn test_add_dir_and_get_dir_roundtrip() {
    let Some(client) = kubo::client().await else {
        return;
    };

    // Build a small temp directory to upload.
    let src = TempDir::new().unwrap();
    std::fs::write(src.path().join("hello.txt"), b"hello").unwrap();
    std::fs::create_dir_all(src.path().join("sub")).unwrap();
    std::fs::write(src.path().join("sub/world.txt"), b"world").unwrap();

    let root_cid = client.add_dir(src.path()).await.expect("add_dir");

    // Retrieve into a fresh temp dir.
    let dst = TempDir::new().unwrap();
    client
        .get_dir(&format!("/ipfs/{root_cid}"), dst.path())
        .await
        .expect("get_dir");

    assert_eq!(
        std::fs::read_to_string(dst.path().join("hello.txt")).unwrap(),
        "hello"
    );
    assert_eq!(
        std::fs::read_to_string(dst.path().join("sub/world.txt")).unwrap(),
        "world"
    );
}

/// Verify `ls` returns the expected entries for a known directory.
#[tokio::test]
async fn test_ls_directory() {
    let Some(client) = kubo::client().await else {
        return;
    };

    let src = TempDir::new().unwrap();
    std::fs::write(src.path().join("a.txt"), b"aaa").unwrap();
    std::fs::write(src.path().join("b.txt"), b"bbb").unwrap();

    let root_cid = client.add_dir(src.path()).await.expect("add_dir");

    let entries = client.ls(&format!("/ipfs/{root_cid}")).await.expect("ls");

    let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
    assert!(names.contains(&"a.txt"), "expected a.txt in {names:?}");
    assert!(names.contains(&"b.txt"), "expected b.txt in {names:?}");
}

/// Pin and unpin a CID without error.
#[tokio::test]
async fn test_pin_add_and_rm() {
    let Some(client) = kubo::client().await else {
        return;
    };

    let cid = ipfs_add_bytes(&client, b"pin me").await.expect("ipfs add");

    client.pin_add(&cid).await.expect("pin_add");
    client.pin_rm(&cid).await.expect("pin_rm");
}

/// MFS round-trip: mkdir, cp from /ipfs, ls, stat, rm.
#[tokio::test]
async fn test_mfs_operations() {
    let Some(client) = kubo::client().await else {
        return;
    };

    let cid = ipfs_add_bytes(&client, b"mfs test data")
        .await
        .expect("ipfs add");
    let mfs = client.mfs();

    let dir = "/ww-test-mfs";
    let file_path = format!("{dir}/data.bin");

    // Clean up from any previous failed run.
    let _ = mfs.files_rm(dir, true).await;

    mfs.files_mkdir(dir, true).await.expect("mkdir");
    mfs.files_cp(&format!("/ipfs/{cid}"), &file_path)
        .await
        .expect("cp");

    let entries = mfs.files_ls(dir).await.expect("ls");
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].name, "data.bin");

    let stat = mfs.files_stat(&file_path, true).await.expect("stat");
    assert_eq!(stat.hash, cid);

    mfs.files_rm(dir, true).await.expect("rm");
}

#[test]
fn required_kubo_cannot_skip_ipfs_operations() {
    kubo::assert_required_kubo_fails("test_add_and_cat_roundtrip");
}
