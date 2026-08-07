/// Build the private PID0 portion of the native-host/kernel ABI material.
///
/// Keeping this input assembly separate makes the two private ABI files
/// independently testable while `build.rs` remains the single fingerprint
/// producer.
pub fn private_pid0_abi_material(
    kernel_abi_version: &str,
    kernel_runtime_wit: &[u8],
    pid0_export_membrane_abi: &[u8],
) -> String {
    format!(
        "kernel-abi={kernel_abi_version}\n\
         kernel-runtime-wit={}\n\
         pid0-export-membrane-abi={}\n",
        blake3::hash(kernel_runtime_wit).to_hex(),
        blake3::hash(pid0_export_membrane_abi).to_hex()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn private_wit_changes_kernel_abi_material() {
        let original = private_pid0_abi_material("2", b"world pid0 {}", b"cap-name");
        let changed =
            private_pid0_abi_material("2", b"world pid0 { import readiness; }", b"cap-name");
        assert_ne!(original, changed);
    }

    #[test]
    fn private_membrane_handoff_name_changes_kernel_abi_material() {
        let original = private_pid0_abi_material("2", b"world pid0 {}", b"cap-name");
        let changed = private_pid0_abi_material("2", b"world pid0 {}", b"renamed-cap");
        assert_ne!(original, changed);
    }
}
