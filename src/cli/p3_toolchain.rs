use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use anyhow::{bail, Context, Result};

pub(super) const NIGHTLY: &str = "nightly-2026-08-30";
const SDK_VERSION: &str = "34.0";
const LLVM_VERSION: &str = "23.1.0";
const COMPONENT_LD_VERSION: &str = "wasm-component-ld 0.5.30";
const WASM_TOOLS_VERSION: &str = "wasm-tools 1.258.0";

pub(super) struct Toolchain {
    pub(super) component_ld: PathBuf,
    pub(super) wasm_ld: PathBuf,
    pub(super) p3_lib: PathBuf,
    wasm_tools: PathBuf,
}

impl Toolchain {
    pub(super) fn discover_and_validate() -> Result<Self> {
        let sdk = std::env::var_os("WASI_SDK_PATH")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .context("WASI_SDK_PATH must name an extracted WASI SDK 34.0 directory")?;
        let component_ld = sdk.join("bin/wasm-component-ld");
        let wasm_ld = sdk.join("bin/wasm-ld");
        let p3_lib = sdk.join("share/wasi-sysroot/experimental-coop-threads/lib/wasm32-wasip3");

        let sdk_version = std::fs::read_to_string(sdk.join("VERSION"))
            .with_context(|| format!("read {}/VERSION", sdk.display()))?;
        if sdk_version.lines().next() != Some(SDK_VERSION) {
            bail!("WASI SDK must be version {SDK_VERSION}");
        }
        if !p3_lib.is_dir() {
            bail!(
                "WASI SDK P3 library directory is missing: {}",
                p3_lib.display()
            );
        }

        let component_version = command_stdout(Command::new(&component_ld).arg("--version"))
            .with_context(|| format!("run {}", component_ld.display()))?;
        if component_version.trim() != COMPONENT_LD_VERSION {
            bail!("{} must be {COMPONENT_LD_VERSION}", component_ld.display());
        }

        let linker_version = command_stdout(Command::new(&wasm_ld).arg("--version"))
            .with_context(|| format!("run {}", wasm_ld.display()))?;
        if !linker_version.starts_with(&format!("LLD {LLVM_VERSION} ")) {
            bail!("{} must use LLD {LLVM_VERSION}", wasm_ld.display());
        }

        let wasm_tools = std::env::var_os("WASM_TOOLS")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("wasm-tools"));
        let wasm_tools_version = command_stdout(Command::new(&wasm_tools).arg("--version"))
            .with_context(|| format!("run {}", wasm_tools.display()))?;
        if !valid_wasm_tools_version(wasm_tools_version.trim()) {
            bail!(
                "{} must report {WASM_TOOLS_VERSION} (reported: {})",
                wasm_tools.display(),
                wasm_tools_version.trim()
            );
        }

        let installed = command_stdout(Command::new("rustup").args([
            "component",
            "list",
            "--toolchain",
            NIGHTLY,
            "--installed",
        ]))
        .with_context(|| format!("inspect {NIGHTLY}"))?;
        if !installed.lines().any(|line| line == "rust-src") {
            bail!("{NIGHTLY} must have the rust-src component");
        }

        let rustc = command_stdout(
            Command::new("rustc")
                .arg(format!("+{NIGHTLY}"))
                .args(["--version", "--verbose"]),
        )
        .with_context(|| format!("inspect {NIGHTLY}"))?;
        if !rustc
            .lines()
            .any(|line| line == format!("LLVM version: {LLVM_VERSION}"))
        {
            bail!("{NIGHTLY} must use LLVM {LLVM_VERSION}");
        }

        Ok(Self {
            component_ld,
            wasm_ld,
            p3_lib,
            wasm_tools,
        })
    }

    pub(super) fn encoded_rustflags(&self) -> String {
        format!(
            "-Clink-arg=--wasm-ld-path={}\u{1f}-Lnative={}",
            self.wasm_ld.display(),
            self.p3_lib.display()
        )
    }

    pub(super) fn validate_component(&self, artifact: &Path) -> Result<()> {
        let validation = Command::new(&self.wasm_tools)
            .args(["validate", "--features", "all"])
            .arg(artifact)
            .output()
            .with_context(|| format!("run {} validate", self.wasm_tools.display()))?;
        require_success(validation, "wasm-tools validate")?;

        let wit = command_stdout(
            Command::new(&self.wasm_tools)
                .args(["component", "wit"])
                .arg(artifact),
        )
        .with_context(|| format!("inspect imports in {}", artifact.display()))?;
        validate_component_wit(&wit)
    }
}

fn command_stdout(command: &mut Command) -> Result<String> {
    let output = command.output().context("execute command")?;
    require_success(output, "command")
}

fn require_success(output: Output, description: &str) -> Result<String> {
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("{description} failed: {}", stderr.trim());
    }
    String::from_utf8(output.stdout).context("command output is not UTF-8")
}

fn valid_wasm_tools_version(version: &str) -> bool {
    if version == WASM_TOOLS_VERSION {
        return true;
    }

    let Some(metadata) = version
        .strip_prefix(WASM_TOOLS_VERSION)
        .and_then(|rest| rest.strip_prefix(" ("))
        .and_then(|rest| rest.strip_suffix(')'))
    else {
        return false;
    };
    let Some((commit, date)) = metadata.split_once(' ') else {
        return false;
    };

    commit.len() == 9
        && commit
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
        && date.len() == 10
        && date.bytes().enumerate().all(|(index, byte)| {
            if matches!(index, 4 | 7) {
                byte == b'-'
            } else {
                byte.is_ascii_digit()
            }
        })
}

fn validate_component_wit(wit: &str) -> Result<()> {
    if wit
        .lines()
        .any(|line| line.contains("wasi:") && line.contains("@0.2."))
    {
        bail!("component contains a WASI 0.2 reference");
    }

    for imported in wit.lines().filter_map(|line| {
        line.trim()
            .strip_prefix("import ")
            .and_then(|line| line.strip_suffix(';'))
    }) {
        if imported.starts_with("wasi:sockets/") {
            bail!("component unexpectedly imports WASI sockets: {imported}");
        }
        if imported.starts_with("wasi:")
            && !imported
                .rsplit_once('@')
                .is_some_and(|(_, version)| version.starts_with("0.3."))
        {
            bail!("component has a non-P3 WASI import: {imported}");
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{valid_wasm_tools_version, validate_component_wit};

    #[test]
    fn accepts_pinned_wasm_tools_version_formats() {
        assert!(valid_wasm_tools_version("wasm-tools 1.258.0"));
        assert!(valid_wasm_tools_version(
            "wasm-tools 1.258.0 (5c6d31c78 2026-08-24)"
        ));
        assert!(!valid_wasm_tools_version("wasm-tools 1.258.1"));
        assert!(!valid_wasm_tools_version(
            "wasm-tools 1.258.0 (untrusted metadata)"
        ));
    }

    #[test]
    fn rejects_p2_and_socket_imports() {
        let p2 = "world root {\n  import wasi:cli/environment@0.2.6;\n}";
        let sockets = "world root {\n  import wasi:sockets/types@0.3.0;\n}";
        let p3 = "world root {\n  import wasi:cli/environment@0.3.0;\n}";

        assert!(validate_component_wit(p2).is_err());
        assert!(validate_component_wit(sockets).is_err());
        assert!(validate_component_wit(p3).is_ok());
    }
}
