//! Build/publication contract checks. The ignored test also needs the pinned
//! WASI P3 toolchain and an explicitly selected, disposable Kubo API endpoint.

use std::collections::BTreeMap;
use std::path::Path;
use std::process::Output;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const ROOT_CID: &str = "bafybeihsp2xyejzajvqbbuwz2kep2bgbzho2waqi6pp4dfdlaf7jsqqkaq";

fn ww() -> tokio::process::Command {
    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_ww"));
    command.env("NO_PROXY", "127.0.0.1,localhost");
    command.kill_on_drop(true);
    command
}

fn success(output: &Output) {
    assert!(
        output.status.success(),
        "ww failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn write(root: &Path, name: &str, bytes: &[u8]) {
    let path = root.join(name);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, bytes).unwrap();
}

fn snapshot(root: &Path) -> BTreeMap<String, Option<Vec<u8>>> {
    walkdir::WalkDir::new(root)
        .min_depth(1)
        .into_iter()
        .map(|entry| {
            let entry = entry.unwrap();
            let name = entry.path().strip_prefix(root).unwrap().to_str().unwrap();
            let bytes = entry
                .file_type()
                .is_file()
                .then(|| std::fs::read(entry.path()).unwrap());
            (name.to_owned(), bytes)
        })
        .collect()
}

async fn read_request(stream: &mut TcpStream) -> Vec<u8> {
    let mut request = Vec::new();
    loop {
        let mut chunk = [0; 4096];
        let count = stream.read(&mut chunk).await.unwrap();
        assert_ne!(count, 0, "truncated HTTP request");
        request.extend_from_slice(&chunk[..count]);
        if let Some(end) = find(&request, b"\r\n\r\n") {
            let headers = String::from_utf8_lossy(&request[..end]);
            let size: usize = headers
                .lines()
                .find_map(|line| {
                    line.to_ascii_lowercase()
                        .strip_prefix("content-length:")
                        .map(|size| size.trim().parse().unwrap())
                })
                .expect("multipart request needs Content-Length");
            if request.len() == end + 4 + size {
                return request;
            }
        }
    }
}

fn find(bytes: &[u8], needle: &[u8]) -> Option<usize> {
    bytes.windows(needle.len()).position(|part| part == needle)
}

fn multipart_files(request: &[u8]) -> BTreeMap<String, Vec<u8>> {
    let header_end = find(request, b"\r\n\r\n").unwrap();
    let headers = String::from_utf8_lossy(&request[..header_end]);
    assert!(headers.starts_with("POST /api/v0/add?"));
    assert!(headers.contains("wrap-with-directory=true"));
    let boundary = headers
        .lines()
        .find_map(|line| line.split_once("boundary=").map(|(_, value)| value))
        .unwrap();
    let delimiter = format!("\r\n--{boundary}");
    let mut body = &request[header_end + 4..];
    assert!(body.starts_with(format!("--{boundary}\r\n").as_bytes()));
    body = &body[boundary.len() + 4..];
    let mut files = BTreeMap::new();
    loop {
        let end = find(body, b"\r\n\r\n").unwrap();
        let headers = String::from_utf8_lossy(&body[..end]);
        let name = headers
            .split_once("filename=\"")
            .unwrap()
            .1
            .split('"')
            .next()
            .unwrap();
        let contents = &body[end + 4..];
        let next = find(contents, delimiter.as_bytes()).unwrap();
        assert!(files
            .insert(name.to_owned(), contents[..next].to_vec())
            .is_none());
        body = &contents[next + delimiter.len()..];
        if body.starts_with(b"--") {
            break;
        }
        assert!(body.starts_with(b"\r\n"));
        body = &body[2..];
    }
    files
}

async fn mock_push(root: &Path, status: &str, response: String) -> (Output, Vec<u8>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let status = status.to_owned();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let request = read_request(&mut stream).await;
        stream.write_all(format!("HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}", response.len()).as_bytes()).await.unwrap();
        request
    });
    let output = tokio::time::timeout(
        Duration::from_secs(20),
        ww().arg("push")
            .arg(root)
            .args(["--ipfs-url", &url])
            .output(),
    )
    .await
    .expect("push timeout")
    .unwrap();
    let request = tokio::time::timeout(Duration::from_secs(2), server)
        .await
        .expect("push made no upload request")
        .unwrap();
    (output, request)
}

#[tokio::test]
async fn push_uploads_named_components_and_supporting_files_without_mutating_image() {
    let image = tempfile::tempdir().unwrap();
    // Neither component name matches the directory basename or "main".
    write(image.path(), "bin/weather_probe.wasm", b"\0asm\xffweather");
    write(image.path(), "bin/metrics-worker.wasm", b"\0asm\x80metrics");
    write(image.path(), "etc/application.json", b"{\"interval\":7}");
    write(
        image.path(),
        "src/lib.rs",
        b"// project source remains publishable",
    );
    write(image.path(), "target/stale.wasm", b"excluded");
    write(image.path(), ".git/config", b"excluded");
    write(image.path(), ".gitignore", b"excluded");
    write(image.path(), "Cargo.lock", b"excluded");
    let before = snapshot(image.path());
    let response = format!("{{\"Name\":\"\",\"Hash\":\"{ROOT_CID}\",\"Size\":\"100\"}}\n");
    let (output, request) = mock_push(image.path(), "200 OK", response).await;
    success(&output);
    assert!(String::from_utf8_lossy(&output.stdout).contains(ROOT_CID));
    assert_eq!(
        multipart_files(&request),
        BTreeMap::from([
            (
                "bin/weather_probe.wasm".into(),
                b"\0asm\xffweather".to_vec()
            ),
            (
                "bin/metrics-worker.wasm".into(),
                b"\0asm\x80metrics".to_vec()
            ),
            ("etc/application.json".into(), b"{\"interval\":7}".to_vec()),
            (
                "src/lib.rs".into(),
                b"// project source remains publishable".to_vec()
            ),
        ])
    );
    assert_eq!(snapshot(image.path()), before);
    assert!(!image.path().join("boot").exists());
    assert!(!image.path().join("bin/main.wasm").exists());
}

#[tokio::test]
async fn push_rejects_missing_components_before_contacting_kubo() {
    for directory_named_wasm in [false, true] {
        let image = tempfile::tempdir().unwrap();
        write(image.path(), "boot/main.wasm", b"obsolete layout");
        if directory_named_wasm {
            std::fs::create_dir_all(image.path().join("bin/pretend.wasm")).unwrap();
        }
        let before = snapshot(image.path());
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let output = tokio::time::timeout(
            Duration::from_secs(10),
            ww().arg("push")
                .arg(image.path())
                .args(["--ipfs-url", &url])
                .output(),
        )
        .await
        .expect("invalid image must fail before contacting Kubo")
        .unwrap();
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("bin"));
        assert!(
            tokio::time::timeout(Duration::from_millis(50), listener.accept())
                .await
                .is_err()
        );
        assert_eq!(snapshot(image.path()), before);
    }
}

#[tokio::test]
async fn push_rejects_removed_atom_options_before_contacting_kubo() {
    let image = tempfile::tempdir().unwrap();
    write(image.path(), "bin/probe.wasm", b"component");
    for (flag, value) in [
        ("--stem", "0x1111111111111111111111111111111111111111"),
        ("--rpc-url", "http://127.0.0.1:8545"),
        ("--private-key", "0x01"),
    ] {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let output = tokio::time::timeout(
            Duration::from_secs(10),
            ww().arg("push")
                .arg(image.path())
                .args(["--ipfs-url", &url, flag, value])
                .output(),
        )
        .await
        .expect("removed options must fail before contacting Kubo")
        .unwrap();
        assert_eq!(output.status.code(), Some(2));
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("unexpected argument") && stderr.contains(flag),
            "{stderr}"
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(50), listener.accept())
                .await
                .is_err()
        );
    }
}

#[tokio::test]
async fn push_propagates_kubo_failure_without_reporting_publication() {
    let image = tempfile::tempdir().unwrap();
    write(image.path(), "bin/probe.wasm", b"component");
    let before = snapshot(image.path());
    let (output, request) = mock_push(
        image.path(),
        "500 Internal Server Error",
        "{\"Message\":\"storage unavailable\"}".into(),
    )
    .await;
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("storage unavailable"));
    assert!(!String::from_utf8_lossy(&output.stdout).contains("CID:"));
    assert_eq!(multipart_files(&request)["bin/probe.wasm"], b"component");
    assert_eq!(snapshot(image.path()), before);
}

/// Run with an isolated Kubo daemon and the pinned SDK/tooling:
/// IPFS_API_URL=http://127.0.0.1:PORT cargo test --test cli_image_contract \
///   generated_project_builds_and_publishes_the_same_image -- --ignored --exact
#[tokio::test]
#[ignore = "requires WASI SDK 34.0, wasm-tools 1.258.0, and an isolated IPFS_API_URL"]
async fn generated_project_builds_and_publishes_the_same_image() {
    let api = std::env::var("IPFS_API_URL").expect("set IPFS_API_URL to an isolated Kubo daemon");
    std::env::var("WASI_SDK_PATH").expect("set WASI_SDK_PATH");
    let examples = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples");
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let name = format!("imagecontract{unique}");
    // Keep TempDir's cleanup owner while init creates the project itself.
    let project = tempfile::Builder::new()
        .prefix(&name)
        .rand_bytes(0)
        .tempdir_in(&examples)
        .unwrap();
    std::fs::remove_dir(project.path()).unwrap();
    success(
        &ww()
            .current_dir(&examples)
            .args(["init", &name])
            .output()
            .await
            .unwrap(),
    );
    success(
        &ww()
            .current_dir(project.path())
            .arg("build")
            .output()
            .await
            .unwrap(),
    );
    let component_path = format!("bin/{name}.wasm");
    assert!(project.path().join(&component_path).is_file());
    assert!(!project.path().join("boot").exists());
    let component = std::fs::read(project.path().join(&component_path)).unwrap();
    // A stale target artifact must not affect Cargo-message selection.
    write(
        project.path(),
        "target/wasm32-wasip3/release/stale.wasm",
        b"not a component",
    );
    for argument in [Path::new(&name), project.path()] {
        success(
            &ww()
                .current_dir(&examples)
                .arg("build")
                .arg(argument)
                .output()
                .await
                .unwrap(),
        );
        assert_eq!(
            std::fs::read(project.path().join(&component_path)).unwrap(),
            component
        );
        assert!(!project.path().join("bin/main.wasm").exists());
        assert_eq!(
            std::fs::read_dir(project.path().join("bin"))
                .unwrap()
                .count(),
            1
        );
    }
    write(project.path(), "etc/application.json", b"{\"interval\":7}");
    let mut expected = BTreeMap::new();
    for entry in walkdir::WalkDir::new(project.path())
        .into_iter()
        .filter_entry(|entry| {
            !matches!(
                entry.file_name().to_str(),
                Some("target" | ".git" | ".gitignore" | "Cargo.lock")
            )
        })
    {
        let entry = entry.unwrap();
        if entry.file_type().is_file() {
            expected.insert(
                entry
                    .path()
                    .strip_prefix(project.path())
                    .unwrap()
                    .to_str()
                    .unwrap()
                    .to_owned(),
                std::fs::read(entry.path()).unwrap(),
            );
        }
    }
    let output = ww()
        .current_dir(project.path())
        .args(["push", "--ipfs-url", &api])
        .output()
        .await
        .unwrap();
    success(&output);
    let stdout = String::from_utf8(output.stdout).unwrap();
    let root = stdout
        .lines()
        .find_map(|line| line.strip_prefix("CID: "))
        .expect("push reports directory CID");
    let client = ww::ipfs::HttpClient::new(api.clone());
    let mut actual = BTreeMap::new();
    let mut pending = vec![(root.to_owned(), String::new())];
    let mut component_cid = None;
    while let Some((directory, prefix)) = pending.pop() {
        for entry in client.ls(&format!("/ipfs/{directory}")).await.unwrap() {
            let path = format!("{prefix}{}", entry.name);
            if entry.entry_type == 1 {
                pending.push((entry.hash, format!("{path}/")));
            } else {
                if path == component_path {
                    component_cid = Some(entry.hash.clone());
                }
                actual.insert(
                    path,
                    client.cat(&format!("/ipfs/{}", entry.hash)).await.unwrap(),
                );
            }
        }
    }
    assert_eq!(
        actual, expected,
        "published paths and bytes must match the build tree under existing import exclusions"
    );
    assert_ne!(root, component_cid.expect("published executable CID"));
    let pins: serde_json::Value = reqwest::Client::builder()
        .no_proxy()
        .build()
        .unwrap()
        .post(format!("{api}/api/v0/pin/ls"))
        .query(&[("arg", root), ("type", "recursive")])
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(pins["Keys"][root]["Type"], "recursive");
    // Verify push's pin before direct import can independently pin the root.
    assert_eq!(root, client.add_dir(project.path()).await.unwrap());
}
