//! Composer v1 interoperability against isolated Kubo 0.33.0 repositories.
//! Run with WW_TEST_REQUIRE_KUBO=1 and the pinned `ipfs` binary on PATH.

use std::collections::BTreeMap;
use std::path::Path;
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;

struct Kubo {
    child: Child,
    repo: tempfile::TempDir,
    api: String,
}

impl Drop for Kubo {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Kubo {
    fn command(repo: &Path) -> Command {
        let mut command = Command::new("ipfs");
        command.env("IPFS_PATH", repo);
        command
    }

    fn checked(output: Output) -> Vec<u8> {
        assert!(
            output.status.success(),
            "Kubo command failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        output.stdout
    }

    fn start() -> Option<Self> {
        if std::env::var_os("WW_TEST_REQUIRE_KUBO").is_none() {
            eprintln!("skipping composer interoperability; set WW_TEST_REQUIRE_KUBO=1");
            return None;
        }
        let version = Self::checked(
            Command::new("ipfs")
                .args(["version", "--number"])
                .output()
                .expect("WW_TEST_REQUIRE_KUBO=1 requires ipfs on PATH"),
        );
        assert_eq!(String::from_utf8(version).unwrap().trim(), "0.33.0");
        let repo = tempfile::tempdir().unwrap();
        let run =
            |args: &[&str]| Self::checked(Self::command(repo.path()).args(args).output().unwrap());
        run(&["init", "--profile=test"]);
        run(&["config", "Addresses.API", "/ip4/127.0.0.1/tcp/0"]);
        run(&["config", "Addresses.Gateway", "/ip4/127.0.0.1/tcp/0"]);
        run(&["config", "--json", "Addresses.Swarm", "[]"]);
        let child = Self::command(repo.path())
            .args(["daemon", "--offline", "--enable-gc"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let mut kubo = Self {
            child,
            repo,
            api: String::new(),
        };
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if let Ok(api) = std::fs::read_to_string(kubo.repo.path().join("api")) {
                kubo.api = api.trim().to_owned();
                if kubo
                    .command_api()
                    .arg("id")
                    .output()
                    .unwrap()
                    .status
                    .success()
                {
                    return Some(kubo);
                }
            }
            assert!(Instant::now() < deadline, "isolated Kubo startup timed out");
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn command_api(&self) -> Command {
        let mut command = Self::command(self.repo.path());
        command.arg(format!("--api={}", self.api));
        command
    }

    fn bytes(&self, args: &[&str]) -> Vec<u8> {
        Self::checked(self.command_api().args(args).output().unwrap())
    }

    fn cli(&self, args: &[&str]) -> String {
        String::from_utf8(self.bytes(args))
            .unwrap()
            .trim()
            .to_owned()
    }

    fn client(&self) -> ipfs::BootClient {
        let parts: Vec<_> = self.api.split('/').collect();
        let url = format!("http://{}:{}", parts[2], parts[4]);
        ipfs::BootClient::new(ipfs::HttpClient::new(url), 0, 1)
    }

    fn add(&self, files: &[(&str, &str)]) -> String {
        let dir = tempfile::tempdir().unwrap();
        for (name, content) in files {
            let path = dir.path().join(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, content).unwrap();
        }
        self.cli(&[
            "add",
            "-Qr",
            "--cid-version=1",
            "--raw-leaves=true",
            dir.path().to_str().unwrap(),
        ])
    }

    fn cid_at(&self, root: &str, path: &str) -> String {
        self.cli(&["resolve", &format!("/ipfs/{root}/{path}")])
            .trim_start_matches("/ipfs/")
            .to_owned()
    }

    async fn tree(&self, root: &str) -> BTreeMap<String, String> {
        let mut output = BTreeMap::new();
        let mut pending = vec![(root.to_owned(), String::new())];
        while let Some((cid, prefix)) = pending.pop() {
            for entry in self.client().ls(&format!("/ipfs/{cid}")).await.unwrap() {
                let path = format!("{prefix}{}", entry.name);
                if entry.entry_type == 1 {
                    output.insert(format!("{path}/"), "directory".to_owned());
                    pending.push((entry.hash, format!("{path}/")));
                } else {
                    output.insert(path, entry.hash);
                }
            }
        }
        output
    }
}

async fn compose(kubo: &Kubo, layers: &[String]) -> anyhow::Result<String> {
    let (_cancel_tx, mut cancel) = tokio::sync::watch::channel(false);
    cell::image::dag_merge(layers, &kubo.client(), &mut cancel).await
}

#[tokio::test]
async fn visible_semantics_match_mfs_and_survive_gc() {
    let Some(kubo) = Kubo::start() else { return };
    let base = kubo.add(&[
        ("keep/deep/file", "unchanged"),
        ("shared/keep", "keep"),
        ("shared/change", "old"),
        ("was-file", "file"),
        ("was-directory/old", "directory"),
        ("replace", "old"),
    ]);
    let overlay = kubo.add(&[
        ("shared/change", "new"),
        ("shared/added", "added"),
        ("was-file/child", "now directory"),
        ("was-directory", "now file"),
        ("replace", "new"),
        ("雪 e\u{301}é &?+#%", "exact UTF-8"),
    ]);
    let layers = [base.clone(), overlay.clone()];
    let root = compose(&kubo, &layers).await.unwrap();
    assert_eq!(root, compose(&kubo, &layers).await.unwrap());
    assert_eq!(kubo.cid_at(&root, "keep"), kubo.cid_at(&base, "keep"));
    assert_eq!(
        kubo.cid_at(&root, "was-file"),
        kubo.cid_at(&overlay, "was-file")
    );
    assert_eq!(
        kubo.cid_at(&root, "shared/keep"),
        kubo.cid_at(&base, "shared/keep")
    );
    assert_ne!(kubo.cid_at(&root, "shared"), kubo.cid_at(&base, "shared"));
    assert_eq!(
        kubo.cli(&["cat", &format!("/ipfs/{root}/shared/change")]),
        "new"
    );

    // Reference the historical visible operations, without requiring root identity.
    kubo.cli(&["files", "cp", &format!("/ipfs/{base}"), "/reference"]);
    for name in ["shared/change", "was-file", "was-directory", "replace"] {
        kubo.cli(&["files", "rm", "-r", &format!("/reference/{name}")]);
    }
    for name in [
        "shared/change",
        "shared/added",
        "was-file",
        "was-directory",
        "replace",
        "雪 e\u{301}é &?+#%",
    ] {
        kubo.cli(&[
            "files",
            "cp",
            &format!("/ipfs/{overlay}/{name}"),
            &format!("/reference/{name}"),
        ]);
    }
    let reference = kubo.cli(&["files", "stat", "--hash", "/reference"]);
    assert_eq!(kubo.tree(&root).await, kubo.tree(&reference).await);
    kubo.cli(&["files", "rm", "-r", "/reference"]);
    kubo.cli(&["pin", "rm", &base, &overlay]);
    kubo.cli(&["repo", "gc"]);
    kubo.cli(&["pin", "ls", "--type=recursive", &root]);
    assert_eq!(
        kubo.cli(&["cat", &format!("/ipfs/{root}/keep/deep/file")]),
        "unchanged"
    );
    assert_eq!(kubo.tree(&root).await.len(), 12);
}

#[tokio::test]
async fn directory_metadata_uses_overlay_wholesale_on_recursive_merge() {
    let Some(kubo) = Kubo::start() else { return };
    let base = kubo.add(&[("base", "base")]);
    let overlay = kubo.add(&[("overlay", "overlay")]);
    kubo.cli(&["files", "cp", &format!("/ipfs/{base}"), "/metadata"]);
    kubo.cli(&["files", "chmod", "750", "/metadata"]);
    kubo.cli(&[
        "files",
        "touch",
        "--mtime=123",
        "--mtime-nsecs=456",
        "/metadata",
    ]);
    let base = kubo.cli(&["files", "stat", "--hash", "/metadata"]);
    kubo.cli(&["pin", "add", &base]);
    // No overlay fields means no inherited mode or mtime.
    let overlay_node: Value = serde_json::from_str(&kubo.cli(&["dag", "get", &overlay])).unwrap();
    let root = compose(&kubo, &[base.clone(), overlay.clone()])
        .await
        .unwrap();
    let merged_node: Value = serde_json::from_str(&kubo.cli(&["dag", "get", &root])).unwrap();
    assert_eq!(merged_node["Data"], overlay_node["Data"]);
    assert_eq!(kubo.cid_at(&root, "base"), kubo.cid_at(&base, "base"));
    assert_eq!(
        kubo.cid_at(&root, "overlay"),
        kubo.cid_at(&overlay, "overlay")
    );

    // Both explicit overlay fields replace the earlier directory's fields.
    kubo.cli(&["files", "cp", &format!("/ipfs/{overlay}"), "/overlay"]);
    kubo.cli(&["files", "chmod", "700", "/overlay"]);
    kubo.cli(&[
        "files",
        "touch",
        "--mtime=789",
        "--mtime-nsecs=1",
        "/overlay",
    ]);
    let overlay = kubo.cli(&["files", "stat", "--hash", "/overlay"]);
    kubo.cli(&["pin", "add", &overlay]);
    let expected: Value = serde_json::from_str(&kubo.cli(&["dag", "get", &overlay])).unwrap();
    let layers = [base.clone(), overlay];
    let root = compose(&kubo, &layers).await.unwrap();
    let actual: Value = serde_json::from_str(&kubo.cli(&["dag", "get", &root])).unwrap();
    assert_eq!(actual["Data"], expected["Data"]);
    assert_eq!(compose(&kubo, &layers).await.unwrap(), root);
    assert_eq!(kubo.cid_at(&root, "base"), kubo.cid_at(&base, "base"));
    assert_eq!(
        compose(&kubo, std::slice::from_ref(&base)).await.unwrap(),
        base
    );
}

#[tokio::test]
async fn actual_kubo_hamt_fixture_fails_closed_at_root_and_nested() {
    let Some(kubo) = Kubo::start() else { return };
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/composer");
    let manifest: Value =
        serde_json::from_slice(&std::fs::read(fixture.join("hamt.json")).unwrap()).unwrap();
    let hamt = manifest["cid"].as_str().unwrap();
    kubo.cli(&["dag", "import", fixture.join("hamt.car").to_str().unwrap()]);
    assert_eq!(
        kubo.bytes(&["block", "get", hamt]),
        include_bytes!("fixtures/composer/hamt-root.dagpb")
    );
    assert_eq!(kubo.tree(hamt).await.len(), 4000);
    let error = compose(&kubo, &[hamt.to_owned()]).await.unwrap_err();
    assert!(format!("{error:#}").contains("HAMT"), "{error:#}");
    kubo.cli(&["files", "mkdir", "/nested"]);
    kubo.cli(&["files", "cp", &format!("/ipfs/{hamt}"), "/nested/large"]);
    let nested = kubo.cli(&["files", "stat", "--hash", "/nested"]);
    kubo.cli(&["pin", "add", &nested]);
    let ordinary = kubo.add(&[("ordinary", "file")]);
    let error = compose(&kubo, &[ordinary, nested]).await.unwrap_err();
    assert!(format!("{error:#}").contains("HAMT"), "{error:#}");
}

#[cfg(unix)]
#[tokio::test]
async fn cidv0_chunked_files_and_symlinks_reuse_their_original_dags() {
    let Some(kubo) = Kubo::start() else { return };
    let directory = tempfile::tempdir().unwrap();
    let content: Vec<u8> = (0..300_000).map(|index| (index % 251) as u8).collect();
    std::fs::write(directory.path().join("chunked"), &content).unwrap();
    std::os::unix::fs::symlink("chunked", directory.path().join("alias")).unwrap();
    let base = kubo.cli(&[
        "add",
        "-Qr",
        "--cid-version=0",
        "--raw-leaves=false",
        "--chunker=size-65536",
        directory.path().to_str().unwrap(),
    ]);
    assert!(base.starts_with("Qm"), "fixture must use CIDv0");
    // Kubo `resolve` promotes output CIDs to the starting path's CID version.
    // Inspect the serialized link to assert exact CIDv0 reference reuse.
    let link_cid = |root: &str, name: &str| {
        let node =
            ipld_dagpb::PbNode::from_bytes(bytes::Bytes::from(kubo.bytes(&["block", "get", root])))
                .unwrap();
        node.links
            .into_iter()
            .find(|link| link.name.as_deref() == Some(name))
            .unwrap()
            .cid
            .to_string()
    };
    let file = link_cid(&base, "chunked");
    let file_node: Value = serde_json::from_str(&kubo.cli(&["dag", "get", &file])).unwrap();
    assert!(file_node["Links"].as_array().unwrap().len() > 1);
    let symlink = link_cid(&base, "alias");
    let overlay = kubo.add(&[("added", "overlay")]);
    let root = compose(&kubo, &[base, overlay]).await.unwrap();
    assert_eq!(link_cid(&root, "chunked"), file);
    assert_eq!(link_cid(&root, "alias"), symlink);
    assert_eq!(
        kubo.bytes(&["cat", &format!("/ipfs/{root}/chunked")]),
        content
    );
    let destination = tempfile::tempdir().unwrap();
    let extracted = destination.path().join("image");
    kubo.cli(&["get", "--output", extracted.to_str().unwrap(), &root]);
    assert_eq!(
        std::fs::read_link(extracted.join("alias")).unwrap(),
        Path::new("chunked")
    );
    assert_eq!(std::fs::read(extracted.join("alias")).unwrap(), content);
}

#[tokio::test]
async fn import_rejects_http_success_when_kubo_cannot_pin_a_missing_descendant() {
    use cid::{multihash::Multihash, Cid};
    use ipld_dagpb::{PbLink, PbNode};
    use sha2::{Digest, Sha256};

    let Some(kubo) = Kubo::start() else { return };
    let hash = Sha256::digest(b"deliberately absent raw block");
    let missing = Cid::new_v1(0x55, Multihash::wrap(0x12, hash.as_slice()).unwrap());
    let block = PbNode {
        links: vec![PbLink {
            cid: missing,
            name: Some("missing".to_owned()),
            size: Some(29),
        }],
        data: Some(bytes::Bytes::from_static(&[0x08, 0x01])),
    }
    .into_bytes();
    let hash = Sha256::digest(&block);
    let root = Cid::new_v1(0x70, Multihash::wrap(0x12, hash.as_slice()).unwrap());
    let error = kubo
        .client()
        .client()
        .import_composed(&root, &[(root, block)])
        .await
        .unwrap_err();
    // This message comes from PinErrorMsg parsing after a successful HTTP status.
    assert!(
        format!("{error:#}").contains("DAG import failed to pin root"),
        "{error:#}"
    );
    assert!(error.downcast_ref::<ipfs::KuboApiError>().is_none());
    let pinned = kubo
        .command_api()
        .args(["pin", "ls", "--type=recursive", &root.to_string()])
        .output()
        .unwrap();
    assert!(
        !pinned.status.success(),
        "incomplete root must not be pinned"
    );
}

#[tokio::test]
async fn kubo_mtime_absent_and_positive_fraction_boundaries_remain_readable() {
    let Some(kubo) = Kubo::start() else { return };
    let base = kubo.add(&[("file", "content")]);
    for (index, (fraction, expected)) in [
        (None, vec![0x08, 0x01, 0x42, 0x02, 0x08, 0x7b]),
        (
            Some(1),
            vec![0x08, 0x01, 0x42, 0x07, 0x08, 0x7b, 0x15, 0x01, 0, 0, 0],
        ),
        (
            Some(999_999_999),
            vec![
                0x08, 0x01, 0x42, 0x07, 0x08, 0x7b, 0x15, 0xff, 0xc9, 0x9a, 0x3b,
            ],
        ),
    ]
    .into_iter()
    .enumerate()
    {
        let path = format!("/fraction-{index}");
        kubo.cli(&["files", "cp", &format!("/ipfs/{base}"), &path]);
        let mut args = vec![
            "files".to_owned(),
            "touch".to_owned(),
            "--mtime=123".to_owned(),
        ];
        if let Some(fraction) = fraction {
            args.push(format!("--mtime-nsecs={fraction}"));
        }
        args.push(path.clone());
        kubo.cli(&args.iter().map(String::as_str).collect::<Vec<_>>());
        let root = kubo.cli(&["files", "stat", "--hash", &path]);
        kubo.cli(&["pin", "add", &root]);
        let node = ipld_dagpb::PbNode::from_bytes(bytes::Bytes::from(
            kubo.bytes(&["block", "get", &root]),
        ))
        .unwrap();
        assert_eq!(
            node.data.unwrap().as_ref(),
            expected,
            "fraction={fraction:?}"
        );
        assert_eq!(
            compose(&kubo, std::slice::from_ref(&root)).await.unwrap(),
            root
        );
        assert_eq!(kubo.cli(&["cat", &format!("/ipfs/{root}/file")]), "content");
    }
}

#[tokio::test]
async fn legacy_dagpb_raw_with_explicit_filesize_remains_readable() {
    use cid::{multihash::Multihash, Cid};
    use ipld_dagpb::{PbLink, PbNode};
    use sha2::{Digest, Sha256};

    let Some(kubo) = Kubo::start() else { return };
    let address = |block: &[u8]| {
        Cid::new_v1(
            0x70,
            Multihash::wrap(0x12, Sha256::digest(block).as_slice()).unwrap(),
        )
    };
    // UnixFS Raw, Data="x", filesize=1. This is distinct from a raw-codec block.
    let raw_block = PbNode {
        links: vec![],
        data: Some(bytes::Bytes::from_static(&[
            0x08, 0x00, 0x12, 0x01, b'x', 0x18, 0x01,
        ])),
    }
    .into_bytes();
    let raw_cid = address(&raw_block);
    let directory = PbNode {
        links: vec![PbLink {
            cid: raw_cid,
            name: Some("legacy".to_owned()),
            size: Some(raw_block.len() as u64),
        }],
        data: Some(bytes::Bytes::from_static(&[0x08, 0x01])),
    }
    .into_bytes();
    let base = address(&directory);
    kubo.client()
        .import_composed(&base, &[(raw_cid, raw_block), (base, directory)])
        .await
        .unwrap();
    assert_eq!(kubo.bytes(&["cat", &format!("/ipfs/{base}/legacy")]), b"x");
    let overlay = kubo.add(&[("opaque", "raw-codec payload")]);
    let root = compose(&kubo, &[base.to_string(), overlay.clone()])
        .await
        .unwrap();
    assert_eq!(kubo.cid_at(&root, "legacy"), raw_cid.to_string());
    assert_eq!(kubo.bytes(&["cat", &format!("/ipfs/{root}/legacy")]), b"x");
    assert_eq!(
        kubo.cid_at(&root, "opaque"),
        kubo.cid_at(&overlay, "opaque")
    );
    assert_eq!(
        kubo.cli(&["cat", &format!("/ipfs/{root}/opaque")]),
        "raw-codec payload"
    );
}

#[tokio::test]
async fn file_linking_to_directory_is_unreadable_in_kubo_and_rejected_by_composer() {
    use cid::{multihash::Multihash, Cid};
    use ipld_dagpb::{PbLink, PbNode};
    use sha2::{Digest, Sha256};

    let Some(kubo) = Kubo::start() else { return };
    let address = |codec, block: &[u8]| {
        Cid::new_v1(
            codec,
            Multihash::wrap(0x12, Sha256::digest(block).as_slice()).unwrap(),
        )
    };
    let child = PbNode {
        links: vec![],
        data: Some(bytes::Bytes::from_static(&[0x08, 0x01])),
    }
    .into_bytes();
    let child_cid = address(0x70, &child);
    let file = PbNode {
        links: vec![PbLink {
            cid: child_cid,
            name: None,
            size: Some(child.len() as u64),
        }],
        // File, filesize=0, blocksizes=[0].
        data: Some(bytes::Bytes::from_static(&[
            0x08, 0x02, 0x18, 0x00, 0x20, 0x00,
        ])),
    }
    .into_bytes();
    let file_cid = address(0x70, &file);
    let file_tsize = file.len() as u64 + child.len() as u64;
    let root_block = PbNode {
        links: vec![PbLink {
            cid: file_cid,
            name: Some("invalid".to_owned()),
            size: Some(file_tsize),
        }],
        data: Some(bytes::Bytes::from_static(&[0x08, 0x01])),
    }
    .into_bytes();
    let root = address(0x70, &root_block);
    kubo.client()
        .import_composed(
            &root,
            &[(child_cid, child), (file_cid, file), (root, root_block)],
        )
        .await
        .unwrap();

    let cat = kubo
        .command_api()
        .args(["cat", &format!("/ipfs/{root}/invalid")])
        .output()
        .unwrap();
    assert!(
        !cat.status.success(),
        "Kubo unexpectedly read File -> Directory"
    );
    let error = compose(&kubo, &[root.to_string()]).await.unwrap_err();
    assert!(format!("{error:#}").contains("file child"), "{error:#}");
}

#[tokio::test]
async fn incorrect_file_blocksizes_disagree_with_kubo_cat_and_are_rejected() {
    use cid::{multihash::Multihash, Cid};
    use ipld_dagpb::{PbLink, PbNode};
    use sha2::{Digest, Sha256};

    let Some(kubo) = Kubo::start() else { return };
    let address = |codec, block: &[u8]| {
        Cid::new_v1(
            codec,
            Multihash::wrap(0x12, Sha256::digest(block).as_slice()).unwrap(),
        )
    };
    let chunk = b"x".to_vec();
    let chunk_cid = address(0x55, &chunk);
    let file = PbNode {
        links: vec![PbLink {
            cid: chunk_cid,
            name: None,
            size: Some(1),
        }],
        // File, filesize=7, blocksizes=[7]. The linked raw block is one byte.
        data: Some(bytes::Bytes::from_static(&[
            0x08, 0x02, 0x18, 0x07, 0x20, 0x07,
        ])),
    }
    .into_bytes();
    let file_cid = address(0x70, &file);
    let file_tsize = file.len() as u64 + 1;
    let root_block = PbNode {
        links: vec![PbLink {
            cid: file_cid,
            name: Some("invalid".to_owned()),
            size: Some(file_tsize),
        }],
        data: Some(bytes::Bytes::from_static(&[0x08, 0x01])),
    }
    .into_bytes();
    let root = address(0x70, &root_block);
    kubo.client()
        .import_composed(
            &root,
            &[(chunk_cid, chunk), (file_cid, file), (root, root_block)],
        )
        .await
        .unwrap();

    let entries = kubo.client().ls(&format!("/ipfs/{root}")).await.unwrap();
    assert_eq!(entries[0].size, 7);
    assert_eq!(kubo.bytes(&["cat", &format!("/ipfs/{root}/invalid")]), b"x");
    let error = compose(&kubo, &[root.to_string()]).await.unwrap_err();
    assert!(format!("{error:#}").contains("blocksize"), "{error:#}");
}
