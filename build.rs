use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    let manifest_dir = env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR not set");
    let manifest_path = Path::new(&manifest_dir);
    let target_dir = resolve_target_dir(manifest_path);

    // Embed the source revision in every host binary. CI supplies the exact
    // workflow revision; local builds fall back to the checked-out commit.
    // Keeping this in build.rs makes `/version` useful outside containers too.
    println!("cargo:rerun-if-env-changed=WW_BUILD_GIT_SHA");
    emit_git_rerun_paths(manifest_path);
    let git_sha = env::var("WW_BUILD_GIT_SHA")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| {
            Command::new("git")
                .args(["rev-parse", "--verify", "HEAD"])
                .current_dir(manifest_path)
                .output()
                .ok()
                .filter(|output| output.status.success())
                .and_then(|output| String::from_utf8(output.stdout).ok())
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
        })
        .unwrap_or_else(|| "unknown".to_string());
    println!("cargo:rustc-env=WW_BUILD_GIT_SHA={git_sha}");

    emit_kernel_abi_fingerprint(manifest_path);

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

    // Compile shell schema so the ww shell CLI gets typed access.
    let shell_schema = manifest_path.join("capnp/shell.capnp");
    if shell_schema.exists() {
        capnpc::CompilerCommand::new()
            .src_prefix(manifest_path.join("capnp"))
            .file(&shell_schema)
            .run()
            .expect("failed to compile shell.capnp");
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

fn emit_kernel_abi_fingerprint(manifest_path: &Path) {
    const KERNEL_ABI_VERSION: &str = "2";
    const KERNEL_RUNTIME_WIT: &str = "std/kernel/wit/kernel.wit";
    const PID0_EXPORT_MEMBRANE_ABI: &str = "std/kernel/abi/pid0_export_membrane_cap.rs";
    const SCHEMA_ROOTS: &[&str] = &[
        "system.capnp",
        "routing.capnp",
        "auth.capnp",
        "membrane.capnp",
        "stem.capnp",
        "http.capnp",
    ];
    let capnp_dir = manifest_path.join("capnp");
    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR not set"));
    let raw_request = out_dir.join("kernel_abi_schema_request.bin");
    let mut compiler = capnpc::CompilerCommand::new();
    compiler
        .src_prefix(&capnp_dir)
        .crate_provides("capnp", [0xa93fc509624c72d9]);
    for schema in SCHEMA_ROOTS {
        compiler.file(capnp_dir.join(schema));
    }
    compiler
        .raw_code_generator_request_path(&raw_request)
        .run()
        .expect("failed to compile schemas for kernel ABI fingerprint");

    // Fingerprint every generated schema node, not a hand-maintained subset.
    // This covers interface method ordinals plus referenced structs/enums such
    // as membrane exports, process handles, and HTTP request data.
    let request_data = fs::read(&raw_request).expect("read kernel ABI schema request");
    let message = capnp::serialize::read_message(
        &mut request_data.as_slice(),
        capnp::message::ReaderOptions::new(),
    )
    .expect("decode kernel ABI schema request");
    let request: capnp::schema_capnp::code_generator_request::Reader = message
        .get_root()
        .expect("read kernel ABI code generator request");
    let mut schema_ids: Vec<u64> = request
        .get_nodes()
        .expect("read kernel ABI schema nodes")
        .iter()
        .map(|node| node.get_id())
        .collect();
    schema_ids.sort_unstable();
    schema_ids.dedup();
    let schema_names: Vec<String> = schema_ids
        .iter()
        .map(|type_id| format!("NODE_{type_id:016X}"))
        .collect();
    let requested_schemas: Vec<(&str, u64)> = schema_names
        .iter()
        .zip(schema_ids.iter().copied())
        .map(|(name, type_id)| (name.as_str(), type_id))
        .collect();
    let mut schemas = schema_id::extract_schemas(&raw_request, &requested_schemas)
        .expect("extract schemas for kernel ABI fingerprint");
    schemas.sort_by_key(|schema| schema.type_id);

    let lock_path = manifest_path.join("Cargo.lock");
    let lock = fs::read_to_string(&lock_path).expect("read Cargo.lock for kernel ABI fingerprint");
    let capnp_rpc_source = lock
        .split("[[package]]")
        .find(|package| {
            package
                .lines()
                .any(|line| line.trim() == "name = \"capnp-rpc\"")
        })
        .and_then(|package| {
            package.lines().find_map(|line| {
                line.trim()
                    .strip_prefix("source = \"")
                    .and_then(|value| value.strip_suffix('"'))
            })
        })
        .filter(|source| {
            source.starts_with("git+https://github.com/wetware/capnproto-rust?")
                && source.rsplit_once('#').is_some_and(|(_, revision)| {
                    revision.len() == 40 && revision.bytes().all(|byte| byte.is_ascii_hexdigit())
                })
        })
        .expect("patched capnp-rpc source missing from Cargo.lock");

    let mut material = format!("kernel-abi={KERNEL_ABI_VERSION}\n");
    let kernel_runtime_wit_path = manifest_path.join(KERNEL_RUNTIME_WIT);
    let kernel_runtime_wit =
        fs::read(&kernel_runtime_wit_path).expect("read private kernel runtime WIT");
    material.push_str(&format!(
        "kernel-runtime-wit={}\n",
        blake3::hash(&kernel_runtime_wit).to_hex()
    ));
    let pid0_export_membrane_abi_path = manifest_path.join(PID0_EXPORT_MEMBRANE_ABI);
    let pid0_export_membrane_abi = fs::read(&pid0_export_membrane_abi_path)
        .expect("read PID0 export membrane private ABI definition");
    material.push_str(&format!(
        "pid0-export-membrane-abi={}\n",
        blake3::hash(&pid0_export_membrane_abi).to_hex()
    ));
    for schema in SCHEMA_ROOTS {
        material.push_str(&format!("schema-root={schema}\n"));
    }
    for schema in schemas {
        material.push_str(&format!("schema-{:016x}={}\n", schema.type_id, schema.cid));
    }
    material.push_str(&format!("capnp-rpc={capnp_rpc_source}\n"));
    let fingerprint = blake3::hash(material.as_bytes()).to_hex();

    println!("cargo:rustc-env=WW_KERNEL_ABI={KERNEL_ABI_VERSION}");
    println!("cargo:rustc-env=WW_KERNEL_ABI_FPR={fingerprint}");
    println!("cargo:rerun-if-changed={}", lock_path.display());
    println!(
        "cargo:rerun-if-changed={}",
        kernel_runtime_wit_path.display()
    );
    println!(
        "cargo:rerun-if-changed={}",
        pid0_export_membrane_abi_path.display()
    );
    for schema in SCHEMA_ROOTS {
        println!(
            "cargo:rerun-if-changed={}",
            capnp_dir.join(schema).display()
        );
    }
}

fn emit_git_rerun_paths(manifest_path: &Path) {
    let git_path = |name: &str| {
        Command::new("git")
            .args(["rev-parse", "--git-path", name])
            .current_dir(manifest_path)
            .output()
            .ok()
            .filter(|output| output.status.success())
            .and_then(|output| String::from_utf8(output.stdout).ok())
            .map(|path| PathBuf::from(path.trim()))
            .map(|path| {
                if path.is_absolute() {
                    path
                } else {
                    manifest_path.join(path)
                }
            })
    };

    if let Some(head) = git_path("HEAD") {
        println!("cargo:rerun-if-changed={}", head.display());
    }
    let symbolic_ref = Command::new("git")
        .args(["symbolic-ref", "-q", "HEAD"])
        .current_dir(manifest_path)
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());
    if let Some(symbolic_ref) = symbolic_ref {
        if let Some(reference) = git_path(&symbolic_ref) {
            println!("cargo:rerun-if-changed={}", reference.display());
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
