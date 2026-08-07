/// Build the private PID0 portion of the native-host/kernel ABI material.
///
/// Keeping this input assembly separate makes the two private ABI files
/// independently testable while `build.rs` remains the single fingerprint
/// producer.
pub fn private_pid0_abi_material(
    kernel_abi_version: &str,
    kernel_runtime_wit: &[u8],
    pid0_export_membrane_cap: &str,
) -> Result<String, String> {
    let source = std::str::from_utf8(kernel_runtime_wit)
        .map_err(|error| format!("private kernel runtime WIT is not UTF-8: {error}"))?;
    let mut resolve = wit_parser::Resolve::default();
    let package = resolve
        .push_str("kernel.wit", source)
        .map_err(|error| format!("parse private kernel runtime WIT: {error:#}"))?;
    let mut printer = wit_component::WitPrinter::default();
    printer.emit_docs(false);
    printer
        .print(&resolve, package, &[])
        .map_err(|error| format!("canonicalize private kernel runtime WIT: {error:#}"))?;
    let canonical_wit = String::from(printer.output);

    Ok(format!(
        "kernel-abi={kernel_abi_version}\n\
         kernel-runtime-wit={}\n\
         pid0-export-membrane-cap={pid0_export_membrane_cap}\n",
        blake3::hash(canonical_wit.as_bytes()).to_hex(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn private_wit_changes_kernel_abi_material() {
        let original = private_pid0_abi_material(
            "2",
            b"package test:kernel; interface readiness { kernel-ready: func(); } world pid0 { import readiness; }",
            "cap-name",
        )
        .unwrap();
        let changed = private_pid0_abi_material(
            "2",
            b"package test:kernel; interface readiness { kernel-ready: func(value: u64); } world pid0 { import readiness; }",
            "cap-name",
        )
        .unwrap();
        assert_ne!(original, changed);
    }

    #[test]
    fn private_membrane_handoff_name_changes_kernel_abi_material() {
        let wit = b"package test:kernel; world pid0 {}";
        let original = private_pid0_abi_material("2", wit, "cap-name").unwrap();
        let changed = private_pid0_abi_material("2", wit, "renamed-cap").unwrap();
        assert_ne!(original, changed);
    }

    #[test]
    fn private_wit_comments_and_formatting_do_not_change_abi_material() {
        let compact = b"package test:kernel; interface readiness { kernel-ready: func(); } world pid0 { import readiness; }";
        let documented = br#"
            package test:kernel;

            /// Private readiness interface.
            interface readiness {
                /// Commit readiness.
                kernel-ready: func();
            }

            world pid0 {
                import readiness;
            }
        "#;
        assert_eq!(
            private_pid0_abi_material("2", compact, "cap-name").unwrap(),
            private_pid0_abi_material("2", documented, "cap-name").unwrap()
        );
    }

    #[test]
    fn private_wit_rejects_invalid_utf8() {
        let error = private_pid0_abi_material("2", &[0xff], "cap-name").unwrap_err();
        assert!(error.contains("not UTF-8"), "unexpected error: {error}");
    }

    #[test]
    fn private_wit_rejects_malformed_source() {
        let error = private_pid0_abi_material("2", b"world {", "cap-name").unwrap_err();
        assert!(
            error.contains("parse private kernel runtime WIT"),
            "unexpected error: {error}"
        );
    }
}
