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
