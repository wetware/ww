use std::env;
use std::fs;
use std::path::{Path, PathBuf};

fn main() {
    let manifest_dir = env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR not set");
    let manifest_path = Path::new(&manifest_dir);
    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());
    let target_dir = resolve_target_dir(manifest_path);

    // Compile example schemas so integration tests get typed access.
    let greeter_schema = manifest_path.join("examples/discovery/greeter.capnp");
    if greeter_schema.exists() {
        capnpc::CompilerCommand::new()
            .src_prefix(manifest_path.join("examples/discovery"))
            .file(&greeter_schema)
            .run()
            .expect("failed to compile greeter.capnp");
        println!("cargo:rerun-if-changed={}", greeter_schema.display());
    }

    // Compile shell schema so the ww shell CLI gets typed access + schema bytes.
    let shell_schema = manifest_path.join("capnp/shell.capnp");
    if shell_schema.exists() {
        let raw_request = out_dir.join("shell_request.bin");
        capnpc::CompilerCommand::new()
            .src_prefix(manifest_path.join("capnp"))
            .file(&shell_schema)
            .raw_code_generator_request_path(&raw_request)
            .run()
            .expect("failed to compile shell.capnp");

        // Extract Shell interface schema bytes for protocol CID computation.
        if let Some(shell_id) = find_interface_id(&raw_request, "Shell") {
            let schemas = schema_id::extract_schemas(&raw_request, &[("SHELL", shell_id)])
                .expect("extract Shell schema");
            schema_id::write_schema_bytes(&out_dir.join("shell_schema.bin"), &schemas[0])
                .expect("write shell schema bytes");
        }
        println!("cargo:rerun-if-changed={}", shell_schema.display());
    }
    let cid_file = target_dir.join("default-config.cid");

    // Read CID from the generated .cid file in target directory
    let cid_value = if cid_file.exists() {
        match fs::read_to_string(&cid_file) {
            Ok(content) => {
                let cid = content.trim();
                if cid.is_empty() {
                    String::new()
                } else {
                    format!("/ipfs/{cid}")
                }
            }
            Err(_) => {
                // Failed to read file - use empty CID
                String::new()
            }
        }
    } else {
        // File doesn't exist - this is expected on first build or when IPFS is unavailable
        // Use empty string as default (will be empty CID at runtime)
        // The Makefile will generate this file as part of 'make all' or 'make default-config'
        // Ensure target directory exists for when Makefile creates the file
        let _ = fs::create_dir_all(&target_dir);
        String::new()
    };

    // Set the environment variable for use in Rust code
    println!("cargo:rustc-env=DEFAULT_KERNEL_CID={cid_value}");
    println!("cargo:rerun-if-changed={}", cid_file.display());

    // Read the std namespace CID (same pattern as above).
    // Written by `make publish-std` in CI; absent for local builds.
    let std_cid_file = target_dir.join("std-namespace.cid");
    let std_cid_value = if std_cid_file.exists() {
        match fs::read_to_string(&std_cid_file) {
            Ok(content) => {
                let cid = content.trim();
                if cid.is_empty() {
                    String::new()
                } else {
                    format!("/ipfs/{cid}")
                }
            }
            Err(_) => String::new(),
        }
    } else {
        String::new()
    };
    println!("cargo:rustc-env=WW_STD_CID={std_cid_value}");
    println!("cargo:rerun-if-changed={}", std_cid_file.display());

    // Check for WASM files that will be embedded via include_bytes!() in release builds.
    // In debug mode, emit a warning but don't fail (allows iterating on non-WASM code).
    // In release mode, fail with a clear error message.
    let embedded_wasm = [
        "std/kernel/bin/main.wasm",
        "std/shell/bin/shell.wasm",
        "std/status/bin/status.wasm",
        "examples/echo/bin/echo.wasm",
    ];
    let mut missing = Vec::new();
    for wasm_path in &embedded_wasm {
        let full = manifest_path.join(wasm_path);
        println!("cargo:rerun-if-changed={}", full.display());
        if !full.exists() {
            missing.push(*wasm_path);
        }
    }
    // Declare expected cfg flags so rustc doesn't warn about unexpected cfgs.
    for wasm_path in &embedded_wasm {
        let flag = wasm_path.replace(['/', '.'], "_");
        println!("cargo:rustc-check-cfg=cfg(has_wasm_{flag})");
    }

    // Set a cfg flag for each WASM file that exists, so the CLI can
    // conditionally include_bytes!() only when the files are available.
    // This avoids writing empty stubs to the source tree (which would
    // break tests that check file existence to decide whether to skip).
    for wasm_path in &embedded_wasm {
        let full = manifest_path.join(wasm_path);
        if full.exists() && fs::metadata(&full).map(|m| m.len() > 0).unwrap_or(false) {
            // Convert path to a valid cfg identifier: replace / and . with _
            let flag = wasm_path.replace(['/', '.'], "_");
            println!("cargo:rustc-cfg=has_wasm_{flag}");
        }
    }
    if !missing.is_empty() {
        let profile = env::var("PROFILE").unwrap_or_default();
        let msg = format!(
            "Missing WASM files for embedding:\n{}\n\nRun `make std` to build them.",
            missing
                .iter()
                .map(|p| format!("  {p}"))
                .collect::<Vec<_>>()
                .join("\n")
        );
        if profile == "release" {
            panic!("{msg}");
        } else {
            println!("cargo:warning={msg}");
        }
    }
}

fn resolve_target_dir(manifest_path: &Path) -> PathBuf {
    match env::var("CARGO_TARGET_DIR") {
        Ok(raw) if !raw.trim().is_empty() => {
            let configured = PathBuf::from(raw);
            if configured.is_absolute() {
                configured
            } else {
                manifest_path.join(configured)
            }
        }
        _ => manifest_path.join("target"),
    }
}

/// Scan a raw CodeGeneratorRequest for an interface node with the given
/// display name and return its type ID.
fn find_interface_id(raw_request_path: &Path, name: &str) -> Option<u64> {
    let data = std::fs::read(raw_request_path).ok()?;
    let reader =
        capnp::serialize::read_message(&mut data.as_slice(), capnp::message::ReaderOptions::new())
            .ok()?;
    let request: capnp::schema_capnp::code_generator_request::Reader = reader.get_root().ok()?;
    for node in request.get_nodes().ok()?.iter() {
        if let Ok(n) = node.get_display_name() {
            if (n.to_str().ok()?.ends_with(&format!(":{name}")) || n.to_str().ok()? == name)
                && matches!(
                    node.which(),
                    Ok(capnp::schema_capnp::node::Which::Interface(_))
                )
            {
                return Some(node.get_id());
            }
        }
    }
    None
}
