#![cfg(feature = "package")]

use std::{fs, path::Path, process::Command};

fn run(command: &str, arguments: &[(&str, &Path)]) -> serde_json::Value {
    let mut process = Command::new(env!("CARGO_BIN_EXE_ww-release-package"));
    process.arg(command);
    for (flag, path) in arguments {
        process.arg(flag).arg(path);
    }
    let output = process.output().unwrap();
    assert!(
        output.status.success(),
        "{command}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let result: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result.as_object().unwrap().len(), 1);
    assert!(result["root"].as_str().unwrap().starts_with("bafy"));
    result
}

#[test]
fn unsigned_cli_roundtrip_preserves_content_and_empty_directories() {
    let temp = tempfile::tempdir().unwrap();
    let tree = temp.path().join("tree");
    fs::create_dir_all(tree.join("empty")).unwrap();
    fs::write(tree.join("VERSION"), b"0.1.1\n").unwrap();
    fs::write(tree.join("CHECKSUMS.txt"), b"unsigned input receipt\n").unwrap();
    let spool = temp.path().join("spool");
    let first = run("pack-tree", &[("--tree", &tree), ("--spool", &spool)]);
    let car = temp.path().join("candidate.car");
    assert_eq!(
        run("export-car", &[("--spool", &spool), ("--car", &car)]),
        first
    );
    let imported = temp.path().join("imported");
    assert_eq!(
        run("import-car", &[("--car", &car), ("--spool", &imported)]),
        first
    );
    let output = temp.path().join("output");
    assert_eq!(
        run(
            "unpack-tree",
            &[("--spool", &imported), ("--tree", &output)]
        ),
        first
    );
    assert_eq!(fs::read(output.join("VERSION")).unwrap(), b"0.1.1\n");
    assert_eq!(
        fs::read(output.join("CHECKSUMS.txt")).unwrap(),
        b"unsigned input receipt\n"
    );
    assert!(output.join("empty").is_dir());
    let again = temp.path().join("again.car");
    run("export-car", &[("--spool", &imported), ("--car", &again)]);
    assert_eq!(fs::read(car).unwrap(), fs::read(again).unwrap());
}

#[test]
fn unsigned_cli_accepts_relative_destination_paths() {
    let temp = tempfile::tempdir().unwrap();
    fs::create_dir(temp.path().join("tree")).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_ww-release-package"))
        .current_dir(temp.path())
        .args(["pack-tree", "--tree", "tree", "--spool", "spool"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(temp.path().join("spool/package.json").is_file());
}

#[cfg(unix)]
#[test]
fn shell_accepts_signed_root_emitted_by_public_package_cli() {
    use sha2::{Digest, Sha256};
    use std::{os::unix::fs::PermissionsExt, process::Output};
    use ww_release::{
        descriptor::{
            Artifact, DeploymentArtifact, Descriptor, HostArtifact, Provenance, BINDING_PATH,
            COMPATIBILITY_PROFILE,
        },
        package,
    };

    fn checked(command: &mut Command) -> Output {
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "{command:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        output
    }
    fn script(path: &Path, contents: &str) {
        fs::write(path, contents).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
    }

    let temp = tempfile::tempdir().unwrap();
    let repository = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let tree = temp.path().join("tree");
    let host = "bin/ww/linux/x86_64/ww";
    let kernel = "fhs/boot/kernel.wasm";
    let status = "fhs/bin/status.wasm";
    for (path, bytes) in [
        (
            host,
            "#!/bin/sh\nprintf '%s\\n' \"$*\" > \"$TEST_MARKER\"\nexit 42\n",
        ),
        (kernel, "fixture kernel"),
        (status, "fixture status"),
        ("VERSION", "0.1.1\n"),
        ("REVISION", "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb\n"),
    ] {
        let destination = tree.join(path);
        fs::create_dir_all(destination.parent().unwrap()).unwrap();
        fs::write(destination, bytes).unwrap();
    }
    // Use the public producer for the deployment graph and final envelope.
    // The marker payloads exercise transport/authentication, not WASM validity.
    let deployment = package::pack_tree(&tree.join("fhs"), &temp.path().join("fhs-spool"))
        .unwrap()
        .root();
    let artifact = |path: &str| {
        let bytes = fs::read(tree.join(path)).unwrap();
        let sha256: [u8; 32] = Sha256::digest(&bytes).into();
        Artifact {
            path: path.into(),
            root: cid::Cid::new_v1(
                0x55,
                cid::multihash::Multihash::wrap(0x12, &sha256).unwrap(),
            ),
            sha256,
            size: bytes.len() as u64,
        }
    };
    let provenance = Provenance {
        repository: "https://github.com/wetware/ww".into(),
        revision: "b".repeat(40),
        builder: "shell-package-test".into(),
    };
    let descriptor = Descriptor {
        channel: ipfs::ipns::canonical_name(
            libp2p_identity::Keypair::ed25519_from_bytes([42; 32])
                .unwrap()
                .public()
                .to_peer_id(),
        ),
        sequence: 1,
        version: "0.1.1".into(),
        source: provenance.clone(),
        hosts: vec![HostArtifact {
            target: "x86_64-unknown-linux-gnu".into(),
            artifact: artifact(host),
            provenance,
            compatibility: COMPATIBILITY_PROFILE.into(),
        }],
        deployment: DeploymentArtifact {
            root: deployment,
            kernel: artifact(kernel),
            status: artifact(status),
            compatibility: COMPATIBILITY_PROFILE.into(),
        },
    };
    fs::write(
        tree.join("RELEASE-DESCRIPTOR.cbor"),
        descriptor.encode().unwrap(),
    )
    .unwrap();
    fs::write(tree.join(BINDING_PATH), descriptor.binding().unwrap()).unwrap();
    let mut manifest = String::from("# sha256\n");
    for path in [host, kernel, status, "VERSION", "REVISION", BINDING_PATH] {
        manifest.push_str(&format!(
            "{}  {path}\n",
            hex::encode(Sha256::digest(fs::read(tree.join(path)).unwrap()))
        ));
    }
    let manifest_path = tree.join("CHECKSUMS.txt");
    fs::write(&manifest_path, manifest).unwrap();
    let keys = temp.path().join("keys");
    checked(
        Command::new("python3")
            .arg(repository.join("tests/release_auth_fixture.py"))
            .arg(&keys)
            .arg(&manifest_path),
    );
    let keyring = keys.join("trusted-keys");
    let spool = temp.path().join("spool");
    let finalized = checked(
        Command::new(env!("CARGO_BIN_EXE_ww-release-package"))
            .args(["finalize", "--tree"])
            .arg(&tree)
            .arg("--spool")
            .arg(&spool)
            .arg("--trusted-keys")
            .arg(&keyring),
    );
    let result: serde_json::Value = serde_json::from_slice(&finalized.stdout).unwrap();
    let root = result["root"].as_str().unwrap();
    let stubs = temp.path().join("stubs");
    fs::create_dir(&stubs).unwrap();
    script(
        &stubs.join("ipfs"),
        r#"#!/bin/sh
set -eu
printf '%s\n' "$*" >> "$TEST_TRANSPORT_LOG"
case "$1" in
  id) exit 0 ;;
  block)
    [ "$2" = get ] && [ "$3" = "$TEST_RELEASE_CID" ]
    cat "$TEST_ROOT_BLOCK" ;;
  cat)
    prefix=/ipfs/$TEST_RELEASE_CID/
    case "$2" in "$prefix"*) ;; *) exit 91 ;; esac
    cat "$TEST_RELEASE_TREE/${2#"$prefix"}" ;;
  *) exit 92 ;;
esac
"#,
    );
    script(
        &stubs.join("uname"),
        "#!/bin/sh\ncase \"$1\" in -s) echo Linux ;; -m) echo x86_64 ;; *) exit 93 ;; esac\n",
    );
    let home = temp.path().join("home");
    fs::create_dir(&home).unwrap();
    let marker = temp.path().join("executed");
    let transport_log = temp.path().join("transport.log");
    let path = std::env::join_paths(
        std::iter::once(stubs).chain(std::env::split_paths(&std::env::var_os("PATH").unwrap())),
    )
    .unwrap();
    let output = Command::new("sh")
        .arg(repository.join("scripts/install.sh"))
        .args(["--release", root, "--trusted-release-keys"])
        .arg(keyring)
        .env("HOME", &home)
        .env("PATH", path)
        .env("TEST_MARKER", &marker)
        .env("TEST_RELEASE_CID", root)
        .env("TEST_RELEASE_TREE", tree)
        .env("TEST_ROOT_BLOCK", spool.join("blocks").join(root))
        .env("TEST_TRANSPORT_LOG", &transport_log)
        .output()
        .unwrap();
    assert_eq!(
        output.status.code(),
        Some(42),
        "shell did not reach the authenticated marker Host: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(fs::read_to_string(marker)
        .unwrap()
        .starts_with("install --check "));
    assert!(fs::read_to_string(transport_log)
        .unwrap()
        .lines()
        .any(|line| line == format!("block get {root}")));
    assert!(!home.join(".ww").exists());
}
