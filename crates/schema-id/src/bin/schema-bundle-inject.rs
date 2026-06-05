use std::path::PathBuf;

fn main() {
    if let Err(error) = run() {
        eprintln!("{error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let mut args = std::env::args_os().skip(1);
    let wasm_path = args
        .next()
        .map(PathBuf::from)
        .ok_or_else(|| usage("missing WASM path"))?;
    let bundle_path = args
        .next()
        .map(PathBuf::from)
        .ok_or_else(|| usage("missing SchemaBundle path"))?;
    let output_path = args
        .next()
        .map(PathBuf::from)
        .unwrap_or_else(|| wasm_path.clone());
    if args.next().is_some() {
        return Err(usage("too many arguments"));
    }

    let wasm = std::fs::read(&wasm_path)
        .map_err(|e| format!("failed to read {}: {e}", wasm_path.display()))?;
    let bundle = std::fs::read(&bundle_path)
        .map_err(|e| format!("failed to read {}: {e}", bundle_path.display()))?;
    let injected = schema_id::inject_schema_bundle_section(&wasm, &bundle)
        .map_err(|e| format!("failed to inject schema bundle: {e}"))?;
    std::fs::write(&output_path, injected)
        .map_err(|e| format!("failed to write {}: {e}", output_path.display()))?;

    Ok(())
}

fn usage(message: &str) -> String {
    format!("{message}\nusage: schema-bundle-inject <wasm> <schema-bundle-bin> [output-wasm]")
}
