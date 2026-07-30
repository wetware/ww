use schema_id::catalog::{
    generate_repository_catalog, load_overlay, repository_root_from_manifest, to_pretty_json,
};

fn generated() -> schema_id::catalog::Catalog {
    let root = repository_root_from_manifest();
    generate_repository_catalog(&root, &root.join("capnp/capability-policy.json"))
        .expect("repository catalog")
}

#[test]
fn repository_catalog_is_deterministic_and_schema_derived() {
    let first = generated();
    let second = generated();
    assert_eq!(
        to_pretty_json(&first).unwrap(),
        to_pretty_json(&second).unwrap()
    );

    let host = first
        .capabilities
        .iter()
        .find(|entry| entry.catalog_id == "ww.host")
        .expect("host entry");
    assert_eq!(host.interface_id, "0x9ea70c8c9aefb70c");
    assert_eq!(host.schema_path, "capnp/system.capnp");
    assert_eq!(
        host.methods
            .iter()
            .map(|method| (method.name.as_str(), method.ordinal))
            .collect::<Vec<_>>(),
        vec![("id", 0), ("addrs", 1), ("peers", 2), ("network", 3)]
    );

    let routing = first
        .capabilities
        .iter()
        .find(|entry| entry.catalog_id == "ww.routing")
        .expect("routing entry");
    assert_eq!(
        routing
            .methods
            .last()
            .map(|method| (&*method.name, method.ordinal)),
        Some(("publish", 7))
    );
}

#[test]
fn policy_overlay_rejects_duplicate_ids_names_and_unknown_interfaces() {
    let root = repository_root_from_manifest();
    let overlay_path = root.join("capnp/capability-policy.json");
    let baseline = load_overlay(&overlay_path).expect("overlay");

    let mut duplicate_id = baseline.clone();
    let mut duplicate = duplicate_id.capabilities[0].clone();
    duplicate.conventional_name = "unique-test-name".to_owned();
    duplicate_id.capabilities.push(duplicate);
    let error = generate_with_overlay(&root, duplicate_id).unwrap_err();
    assert!(error.contains("duplicate catalogId"), "{error}");

    let mut duplicate_name = baseline.clone();
    let mut duplicate = duplicate_name.capabilities[0].clone();
    duplicate.catalog_id = "ww.unique-test-id".to_owned();
    duplicate_name.capabilities.push(duplicate);
    let error = generate_with_overlay(&root, duplicate_name).unwrap_err();
    assert!(error.contains("duplicate conventionalName"), "{error}");

    let mut unknown = baseline;
    unknown.capabilities[0].interface_id = "0x8000000000000001".to_owned();
    let error = generate_with_overlay(&root, unknown).unwrap_err();
    assert!(error.contains("unknown interface ID"), "{error}");
}

#[test]
fn policy_overlay_rejects_manual_method_metadata() {
    let root = repository_root_from_manifest();
    let overlay_path = root.join("capnp/capability-policy.json");
    let mut overlay: serde_json::Value =
        serde_json::from_slice(&std::fs::read(overlay_path).expect("overlay bytes"))
            .expect("overlay JSON");
    overlay["capabilities"][0]["methods"] = serde_json::json!([
        {
            "name": "fabricated",
            "ordinal": 0
        }
    ]);

    let temp = tempfile::tempdir().expect("temp");
    let invalid_path = temp.path().join("invalid-overlay.json");
    std::fs::write(
        &invalid_path,
        serde_json::to_vec(&overlay).expect("invalid overlay JSON"),
    )
    .expect("write invalid overlay");

    let error = load_overlay(&invalid_path).unwrap_err();
    assert!(error.contains("unknown field `methods`"), "{error}");
}

#[test]
fn sensitive_classification_and_no_live_reference_fields_are_stable() {
    let catalog = generated();
    let critical = catalog
        .capabilities
        .iter()
        .filter(|entry| entry.sensitivity == "critical")
        .map(|entry| entry.conventional_name.as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        critical,
        vec![
            "authority",
            "host",
            "identity",
            "membrane",
            "routing",
            "runtime",
            "vat-listener"
        ]
    );

    let value = serde_json::to_value(catalog).expect("catalog JSON");
    fn check_keys(value: &serde_json::Value) {
        match value {
            serde_json::Value::Object(map) => {
                for (key, child) in map {
                    assert!(
                        !matches!(
                            key.as_str(),
                            "cap" | "client" | "liveReference" | "bootstrapReference"
                        ),
                        "catalog must not contain a live reference field: {key}"
                    );
                    check_keys(child);
                }
            }
            serde_json::Value::Array(values) => {
                for child in values {
                    check_keys(child);
                }
            }
            _ => {}
        }
    }
    check_keys(&value);
}

fn generate_with_overlay(
    root: &std::path::Path,
    overlay: schema_id::catalog::PolicyOverlay,
) -> Result<schema_id::catalog::Catalog, String> {
    let temp = tempfile::tempdir().expect("temp");
    let overlay_path = temp.path().join("overlay.json");
    let json = serde_json::to_vec(&overlay).expect("overlay JSON");
    std::fs::write(&overlay_path, json).expect("write overlay");
    generate_repository_catalog(root, &overlay_path)
}
