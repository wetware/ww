use super::*;

fn source(root: &Path) -> PathBuf {
    let tree = root.join("input");
    fs::create_dir_all(tree.join("nested/empty")).unwrap();
    fs::write(tree.join("nested/payload"), vec![42; CHUNK_BYTES + 17]).unwrap();
    fs::write(tree.join("CHECKSUMS.txt"), b"unsigned receipt\n").unwrap();
    tree
}

fn car_bytes(root: Cid, blocks: &[(Cid, Vec<u8>)]) -> Vec<u8> {
    // Independent, canonical single-root CARv1 fixture framing.
    let cid = root.to_bytes();
    let mut header = b"\xa2\x65roots\x81\xd8\x2a\x58".to_vec();
    header.push((cid.len() + 1) as u8);
    header.push(0);
    header.extend_from_slice(&cid);
    header.extend_from_slice(b"\x67version\x01");
    let mut result = Vec::new();
    varint(header.len() as u64, &mut result);
    result.extend_from_slice(&header);
    for (cid, bytes) in blocks {
        let cid = cid.to_bytes();
        varint((cid.len() + bytes.len()) as u64, &mut result);
        result.extend_from_slice(&cid);
        result.extend_from_slice(bytes);
    }
    result
}

fn expect_bad_car(root: &Path, bytes: &[u8]) {
    let input = root.join("input.car");
    let spool = root.join("rejected-spool");
    fs::write(&input, bytes).unwrap();
    assert!(read_car(&input, &spool).is_err());
    assert!(!spool.exists(), "failed import left partial candidate data");
}

#[test]
fn unsigned_tree_transport_preserves_large_files_empty_directories_and_receipts() {
    let temp = tempfile::tempdir().unwrap();
    let tree = source(temp.path());
    let spool = temp.path().join("packed");
    let package = pack_tree(&tree, &spool).unwrap();
    let car = temp.path().join("input.car");
    write_car(&package, &car).unwrap();
    let imported = read_car(&car, &temp.path().join("imported")).unwrap();
    assert_eq!(imported.root(), package.root());
    let output = temp.path().join("output");
    unpack_tree(&imported, &output).unwrap();
    for name in ["nested/payload", "CHECKSUMS.txt"] {
        assert_eq!(
            fs::read(tree.join(name)).unwrap(),
            fs::read(output.join(name)).unwrap()
        );
    }
    assert!(output.join("nested/empty").is_dir());
    let repacked = pack_tree(&output, &temp.path().join("repacked")).unwrap();
    assert_eq!(repacked.root(), package.root());
    let again = temp.path().join("again.car");
    write_car(&repacked, &again).unwrap();
    assert_eq!(fs::read(car).unwrap(), fs::read(again).unwrap());
}

#[test]
fn signed_car_preserves_the_qualified_root_descriptor_and_signature() {
    let temp = tempfile::tempdir().unwrap();
    let (tree, spool, key, descriptor) = fixture(temp.path());
    let package = finalize(&tree, &spool, &keyring(&key)).unwrap();
    let car = temp.path().join("signed.car");
    write_car(&package, &car).unwrap();
    let imported = read_car(&car, &temp.path().join("imported")).unwrap();
    assert_eq!(
        imported.root().to_string(),
        "bafybeifyyh6o4ng7sumde5mq5xtx47vi336xriqk6guja4hbd6hwlmbzne"
    );
    assert_eq!(
        imported
            .authenticate(&keyring(&key))
            .unwrap()
            .descriptor_cid(),
        descriptor.cid().unwrap()
    );
    assert_eq!(
        imported.file_bytes("CHECKSUMS.txt.sig", 64).unwrap(),
        fs::read(tree.join("CHECKSUMS.txt.sig")).unwrap()
    );
    let again = temp.path().join("again.car");
    write_car(&imported, &again).unwrap();
    assert_eq!(fs::read(car).unwrap(), fs::read(again).unwrap());
    let output = temp.path().join("unsigned-output");
    assert!(unpack_tree(&imported, &output).is_err());
    assert!(!output.exists());
}

#[test]
fn pack_refuses_reserved_names_case_collisions_and_long_paths_without_partial_spool() {
    for reserved in [
        DESCRIPTOR_FILE,
        BINDING_PATH,
        DESCRIPTOR_LINK_NAME,
        "CHECKSUMS.txt.sig",
        "release-descriptor.CBOR",
    ] {
        let temp = tempfile::tempdir().unwrap();
        let tree = source(temp.path());
        fs::write(tree.join(reserved), b"must not silently disappear").unwrap();
        let spool = temp.path().join("spool");
        assert!(pack_tree(&tree, &spool).is_err(), "accepted {reserved}");
        assert!(!spool.exists());
        assert!(tree.join(reserved).exists());
    }
    let temp = tempfile::tempdir().unwrap();
    let tree = source(temp.path());
    let long = tree
        .join("a".repeat(200))
        .join("b".repeat(200))
        .join("c".repeat(200));
    fs::create_dir_all(&long).unwrap();
    assert!(pack_tree(&tree, &temp.path().join("spool")).is_err());
    assert!(!temp.path().join("spool").exists());
}

#[test]
fn fresh_destination_contract_preserves_existing_files_and_refuses_symlinks() {
    let temp = tempfile::tempdir().unwrap();
    let tree = source(temp.path());
    let existing = temp.path().join("existing");
    fs::create_dir(&existing).unwrap();
    fs::write(existing.join("sentinel"), b"preserve").unwrap();
    assert!(pack_tree(&tree, &existing).is_err());
    assert!(pack_tree(&tree, &tree.join("spool")).is_err());
    assert!(!tree.join("spool").exists());
    let package = pack_tree(&tree, &temp.path().join("spool")).unwrap();
    assert!(unpack_tree(&package, &existing).is_err());
    let car = temp.path().join("existing.car");
    fs::write(&car, b"preserve").unwrap();
    assert!(write_car(&package, &car).is_err());
    assert_eq!(fs::read(car).unwrap(), b"preserve");
    assert_eq!(fs::read(existing.join("sentinel")).unwrap(), b"preserve");
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(existing.join("sentinel"), tree.join("alias")).unwrap();
        assert!(pack_tree(&tree, &temp.path().join("rejected")).is_err());
        assert!(!temp.path().join("rejected").exists());
    }
}

#[test]
fn car_import_rejects_duplicate_unreachable_missing_corrupt_and_unsupported_blocks() {
    let temp = tempfile::tempdir().unwrap();
    let spool = temp.path().join("source");
    let mut builder = Builder::new(&spool).unwrap();
    let leaf = builder.block(0x55, b"payload").unwrap();
    let root = builder
        .directory(BTreeMap::from([("file".into(), leaf)]))
        .unwrap()
        .cid;
    let blocks = vec![
        (root, read_block(&spool, &root).unwrap()),
        (leaf.cid, b"payload".to_vec()),
    ];
    let input = temp.path().join("valid.car");
    fs::write(&input, car_bytes(root, &blocks)).unwrap();
    assert_eq!(
        read_car(&input, &temp.path().join("valid")).unwrap().root(),
        root
    );
    let mut duplicate = blocks.clone();
    duplicate.push(blocks[1].clone());
    expect_bad_car(temp.path(), &car_bytes(root, &duplicate));
    let extra = builder.block(0x55, b"unreachable").unwrap().cid;
    let mut unreachable = blocks.clone();
    unreachable.push((extra, b"unreachable".to_vec()));
    expect_bad_car(temp.path(), &car_bytes(root, &unreachable));
    expect_bad_car(temp.path(), &car_bytes(root, &blocks[..1]));
    expect_bad_car(temp.path(), &car_bytes(root, &[]));
    let mut corrupt = blocks.clone();
    corrupt[1].1[0] ^= 1;
    expect_bad_car(temp.path(), &car_bytes(root, &corrupt));
    for codec in [0x99, 0x55] {
        let hash = if codec == 0x55 {
            cid::multihash::Multihash::wrap(0, b"payload").unwrap()
        } else {
            *leaf.cid.hash()
        };
        let cid = Cid::new_v1(codec, hash);
        expect_bad_car(temp.path(), &car_bytes(cid, &[(cid, b"payload".to_vec())]));
    }
    let valid = car_bytes(root, &blocks);
    expect_bad_car(temp.path(), &valid[..valid.len() - 1]);
    let mut trailing = valid.clone();
    trailing.push(0);
    expect_bad_car(temp.path(), &trailing);
    let mut overlong = valid.clone();
    overlong[0] |= 128;
    overlong.insert(1, 0);
    expect_bad_car(temp.path(), &overlong);
}

fn malformed_directory(root: &Path, entries: Vec<PbLink>) -> Package {
    let mut builder = Builder::new(root).unwrap();
    let root = builder
        .block(
            0x70,
            &PbNode {
                data: Some(Bytes::from_static(&[8, 1])),
                links: entries,
            }
            .into_bytes(),
        )
        .unwrap()
        .cid;
    builder.package(root).unwrap()
}

#[test]
fn materialization_rejects_unsafe_casefold_duplicate_paths_and_wrong_sizes() {
    for names in [
        vec!["../escape"],
        vec!["/absolute"],
        vec!["a/b"],
        vec!["."],
        vec![".."],
        vec!["é"],
        vec!["A", "a"],
        vec!["same", "same"],
        vec!["CHECKSUMS.txt.sig"],
    ] {
        let temp = tempfile::tempdir().unwrap();
        let spool = temp.path().join("spool");
        let leaf = Builder::new(&spool)
            .unwrap()
            .block(0x55, b"payload")
            .unwrap();
        let entries = names
            .iter()
            .map(|name| PbLink {
                cid: leaf.cid,
                name: Some((*name).into()),
                size: Some(leaf.size),
            })
            .collect();
        let package = malformed_directory(&spool, entries);
        let output = temp.path().join("output");
        assert!(
            unpack_tree(&package, &output).is_err(),
            "accepted {names:?}"
        );
        assert!(!output.exists());
    }
    let temp = tempfile::tempdir().unwrap();
    let spool = temp.path().join("spool");
    let leaf = Builder::new(&spool)
        .unwrap()
        .block(0x55, b"payload")
        .unwrap();
    for size in [None, Some(0), Some(8)] {
        let package = malformed_directory(
            &spool,
            vec![PbLink {
                cid: leaf.cid,
                name: Some("file".into()),
                size,
            }],
        );
        let output = temp.path().join("output");
        assert!(unpack_tree(&package, &output).is_err());
        assert!(!output.exists());
    }
}

#[test]
fn global_materialization_budgets_reject_shared_dag_amplification_before_output() {
    for kind in ["files", "directories", "bytes", "work"] {
        let temp = tempfile::tempdir().unwrap();
        let spool = temp.path().join("spool");
        let mut builder = Builder::new(&spool).unwrap();
        let root = match kind {
            "files" | "directories" => {
                let leaf = if kind == "files" {
                    builder.block(0x55, &[]).unwrap()
                } else {
                    builder.directory(BTreeMap::new()).unwrap()
                };
                let shared = builder
                    .directory((0..600).map(|i| (format!("entry{i:04}"), leaf)).collect())
                    .unwrap();
                builder
                    .directory(BTreeMap::from([("a".into(), shared), ("b".into(), shared)]))
                    .unwrap()
            }
            _ => {
                let payload = if kind == "bytes" {
                    vec![42; CHUNK_BYTES]
                } else {
                    Vec::new()
                };
                let mut file = builder.block(0x55, &payload).unwrap();
                for _ in 0..if kind == "bytes" { 3 } else { 4 } {
                    file = builder.file_parent(&[file; FANOUT]).unwrap();
                }
                builder
                    .directory(
                        (0..if kind == "bytes" { 3 } else { 2 })
                            .map(|index| (format!("file{index}"), file))
                            .collect(),
                    )
                    .unwrap()
            }
        };
        let package = builder.package(root.cid).unwrap();
        let output = temp.path().join("output");
        assert!(unpack_tree(&package, &output).is_err(), "unbounded {kind}");
        assert!(
            !output.exists(),
            "{kind} amplification wrote partial output"
        );
    }
}

#[test]
fn original_mixed_zip64_fixture_is_not_a_car_and_creates_no_spool() {
    let temp = tempfile::tempdir().unwrap();
    let bytes = include_bytes!("../../tests/fixtures/mixed-zip64.zip");
    assert_eq!(bytes.len(), 272);
    expect_bad_car(temp.path(), bytes);
}

#[test]
fn materialization_rejects_long_paths_deep_trees_and_invalid_unixfs_file_sizes() {
    for kind in ["path", "depth", "file-size"] {
        let temp = tempfile::tempdir().unwrap();
        let spool = temp.path().join("spool");
        let mut builder = Builder::new(&spool).unwrap();
        let mut child = builder.block(0x55, b"payload").unwrap();
        if kind == "file-size" {
            let node = PbNode {
                data: Some(Bytes::from_static(&[8, 2, 24, 8, 32, 8])),
                links: vec![PbLink {
                    cid: child.cid,
                    name: Some(String::new()),
                    size: Some(child.size),
                }],
            };
            child = builder.block(0x70, &node.into_bytes()).unwrap();
        }
        let (name, depth) = match kind {
            "path" => ("a".repeat(200), 3),
            "depth" => ("a".into(), 65),
            _ => ("file".into(), 1),
        };
        for _ in 0..depth {
            child = builder
                .directory(BTreeMap::from([(name.clone(), child)]))
                .unwrap();
        }
        let package = Package {
            spool,
            root: child.cid,
        };
        let output = temp.path().join("output");
        assert!(unpack_tree(&package, &output).is_err(), "accepted {kind}");
        assert!(!output.exists());
    }
}

#[test]
fn export_omits_unreachable_spool_files_and_refuses_corrupted_content() {
    let temp = tempfile::tempdir().unwrap();
    let tree = source(temp.path());
    let spool = temp.path().join("spool");
    let package = pack_tree(&tree, &spool).unwrap();
    let first = temp.path().join("first.car");
    write_car(&package, &first).unwrap();
    fs::write(spool.join("blocks/unrelated"), b"must not enter CAR").unwrap();
    let second = temp.path().join("second.car");
    write_car(&package, &second).unwrap();
    assert_eq!(fs::read(first).unwrap(), fs::read(second).unwrap());
    fs::write(
        spool.join("blocks").join(package.root().to_string()),
        b"changed",
    )
    .unwrap();
    let rejected = temp.path().join("rejected.car");
    assert!(write_car(&package, &rejected).is_err());
    assert!(!rejected.exists());
}
