//! Select the daemon provisioned by Host CI or the developer's Kubo API.

use ww::ipfs::HttpClient;

pub async fn client() -> Option<HttpClient> {
    let url = match std::env::var("WW_TEST_KUBO_ADDR") {
        Ok(addr) => format!("http://{addr}"),
        Err(std::env::VarError::NotPresent) => {
            std::env::var("IPFS_API_URL").unwrap_or_else(|_| "http://127.0.0.1:5001".to_string())
        }
        Err(error) => panic!("invalid WW_TEST_KUBO_ADDR: {error}"),
    };
    let client = HttpClient::new(url);
    match client.kubo_info().await {
        Ok(_) => Some(client),
        Err(error) if std::env::var_os("WW_TEST_REQUIRE_KUBO").is_some() => {
            panic!("required Kubo at {}: {error:#}", client.base_url())
        }
        Err(error) => {
            eprintln!(
                "skipping optional Kubo test at {}: {error:#}",
                client.base_url()
            );
            None
        }
    }
}

/// Drive the real test entry point in a fresh process with an unusable endpoint.
pub fn assert_required_kubo_fails(test_name: &str) {
    let output = std::process::Command::new(std::env::current_exe().expect("test binary"))
        .args(["--exact", test_name, "--nocapture"])
        .env("WW_TEST_REQUIRE_KUBO", "1")
        .env("WW_TEST_KUBO_ADDR", "127.0.0.1:0")
        .env_remove("IPFS_API_URL")
        .output()
        .expect("run missing-Kubo control");
    let report = String::from_utf8_lossy(&output.stderr);
    assert!(
        !output.status.success(),
        "{test_name} passed without Kubo: {report}"
    );
    assert!(
        report.contains("required Kubo at http://127.0.0.1:0:"),
        "{report}"
    );
}
