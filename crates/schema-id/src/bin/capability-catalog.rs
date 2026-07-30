use schema_id::catalog::{
    check_catalog, generate_repository_catalog, repository_root_from_manifest, write_catalog,
};

fn main() {
    if let Err(error) = run() {
        eprintln!("capability-catalog: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let command = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "check".to_owned());
    if !matches!(command.as_str(), "check" | "generate") {
        return Err("usage: capability-catalog [check|generate]".to_owned());
    }
    let root = repository_root_from_manifest();
    let overlay = root.join("capnp/capability-policy.json");
    let artifact = root.join("doc/generated/capability-catalog.json");
    let catalog = generate_repository_catalog(&root, &overlay)?;
    if command == "generate" {
        write_catalog(&artifact, &catalog)?;
        println!("generated {}", artifact.display());
    } else {
        check_catalog(&artifact, &catalog)?;
        println!("catalog is current: {}", artifact.display());
    }
    Ok(())
}
