//! Scoped installation ownership. Private state never enters the FHS.
use anyhow::{ensure, Context, Result};
use clap::Args;
use serde::{Deserialize, Serialize};
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use ww::installation_state::{MutationLock, Receipt};
use ww::upgrade::storage as durable;

use super::upgrade_cmd::{self, ReleaseArgs, Scope};

const SOFTWARE: [&str; 3] = ["bin/ww", "fhs/boot/kernel.wasm", "fhs/bin/status.wasm"];

#[derive(Args)]
pub(super) struct InstallArgs {
    #[command(flatten)]
    release: ReleaseArgs,
    /// Authenticated bootstrap handoff; ordinary users select --release instead.
    #[arg(long, hide = true, conflicts_with = "release", requires_all = ["manifest", "signature"])]
    payload: Option<PathBuf>,
    #[arg(long, hide = true, requires = "payload")]
    manifest: Option<PathBuf>,
    #[arg(long, hide = true, requires = "payload")]
    signature: Option<PathBuf>,
    /// An untrusted locator. Future upgrades independently authenticate it.
    #[arg(long, hide = true, requires = "payload", conflicts_with = "release")]
    bootstrap_release: Option<String>,
    /// Validate selected software and destinations without installing.
    #[arg(long)]
    check: bool,
    /// Print one versioned result object.
    #[arg(long)]
    pub(super) json: bool,
}

#[derive(Args)]
pub(super) struct UpdateArgs {
    #[command(flatten)]
    release: ReleaseArgs,
    #[arg(long)]
    check: bool,
    #[arg(long)]
    pub(super) json: bool,
}

#[derive(Serialize)]
struct Report {
    version: u8,
    operation: &'static str,
    changed_paths: Vec<PathBuf>,
    target: PathBuf,
    release: Option<String>,
    fhs: Option<FhsDigests>,
    previous_fhs: Option<FhsDigests>,
    activation: bool,
    check: bool,
    recovery: Option<&'static str>,
    warnings: Vec<String>,
    service: Option<super::daemon_cmd::RemovalReport>,
}

#[derive(Debug, PartialEq, Serialize)]
struct FhsDigests {
    kernel_blake3: String,
    status_blake3: String,
}

impl From<&Receipt> for FhsDigests {
    fn from(receipt: &Receipt) -> Self {
        Self {
            kernel_blake3: receipt.hashes[1].clone(),
            status_blake3: receipt.hashes[2].clone(),
        }
    }
}

struct ImageRecovery {
    release: Option<String>,
    fhs: FhsDigests,
    previous_fhs: FhsDigests,
    action: &'static str,
}

impl Report {
    fn print(self, json: bool) -> Result<()> {
        if json {
            println!("{}", serde_json::to_string(&self)?);
        } else {
            eprintln!(
                "{}{}: {}",
                self.operation,
                if self.check { " preflight" } else { "" },
                self.target.display()
            );
            for path in &self.changed_paths {
                eprintln!("  {}", path.display());
            }
            if let Some(release) = &self.release {
                eprintln!("Release: {release}");
            }
            if let Some(fhs) = &self.previous_fhs {
                eprintln!("Previous kernel BLAKE3: {}", fhs.kernel_blake3);
                eprintln!("Previous status BLAKE3: {}", fhs.status_blake3);
            }
            if let Some(fhs) = &self.fhs {
                eprintln!("Kernel BLAKE3: {}", fhs.kernel_blake3);
                eprintln!("Status BLAKE3: {}", fhs.status_blake3);
            }
            if let Some(recovery) = self.recovery {
                eprintln!("Recovery: {recovery}");
            }
            for warning in &self.warnings {
                eprintln!("{warning}");
            }
        }
        Ok(())
    }
}

fn incompatible(path: &Path) -> anyhow::Error {
    anyhow::anyhow!("unrecognized or altered installation file {}; preserve it and restore the recorded artifact before retrying", path.display())
}

fn parents(home: &Path, path: &Path) -> Result<()> {
    ensure!(path.starts_with(home), "installation path escapes HOME");
    let mut parent = path.parent();
    while let Some(p) = parent {
        if durable::exists(p)? {
            durable::directory(p)?;
        }
        if p == home {
            break;
        }
        parent = p.parent();
    }
    Ok(())
}

fn read_regular(path: &Path) -> Result<Option<Vec<u8>>> {
    use std::io::Read;
    if !durable::exists(path)? {
        return Ok(None);
    }
    let file = durable::open_read(path)?;
    ensure!(
        file.metadata()?.len() <= 1024 * 1024 * 1024,
        "oversized installation file {}",
        path.display()
    );
    let mut bytes = Vec::new();
    file.take(1024 * 1024 * 1024 + 1).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() <= 1024 * 1024 * 1024,
        "oversized installation file"
    );
    Ok(Some(bytes))
}

pub(super) fn persist_identity(path: &Path, key: &ed25519_dalek::SigningKey) -> Result<()> {
    use std::io::Write;
    let absolute = if path.is_absolute() {
        path.to_owned()
    } else {
        std::env::current_dir()?.join(path)
    };
    let path = absolute.as_path();
    let parent = path.parent().context("identity parent")?;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(parent)?;
    let mut temp = tempfile::NamedTempFile::new_in(parent)?;
    temp.write_all(ww::keys::encode(key).as_bytes())?;
    temp.as_file().sync_all()?;
    temp.persist_noclobber(path).with_context(|| {
        format!(
            "create identity {}; existing identity was preserved",
            path.display()
        )
    })?;
    std::fs::File::open(parent)?
        .sync_all()
        .context("sync identity parent")
}

fn digest(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex().to_string()
}

fn recognized(state: &Receipt, index: usize, bytes: &[u8]) -> bool {
    !bytes.is_empty()
        && (state.hashes[index] == digest(bytes)
            || state
                .previous
                .as_ref()
                .is_some_and(|old| old[index] == digest(bytes)))
}

fn atomic_write(path: &Path, bytes: &[u8], executable: bool) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;
    let parent = path.parent().context("installation file parent")?;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(parent)?;
    durable::directory(parent)?;
    if durable::exists(path)? {
        durable::open_read(path)?;
    }
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    file.write_all(bytes)?;
    file.as_file()
        .set_permissions(std::fs::Permissions::from_mode(if executable {
            0o755
        } else {
            0o600
        }))?;
    file.as_file().sync_all()?;
    durable::rename(file.path(), path)?;
    Ok(())
}

fn migrate(root: &Path, state: &Receipt) -> Result<Receipt> {
    let mut next = state.clone();
    if next.version == 1 {
        ensure!(next.complete, "incomplete version-1 installation; recover the exact recorded package before migration");
        let backup = root.join("installation-v1.json");
        if !durable::exists(&backup)? {
            durable::write_record(&backup, &state.encode()?)?;
        }
        next.version = 2;
    }
    Ok(next)
}

pub(super) fn installed_fhs(root: &Path) -> Result<()> {
    let home = root.parent().context("installation HOME")?;
    parents(home, &root.join("installation.json"))?;
    ensure!(
        !durable::exists(&root.join("image-update.json"))?,
        "pending FHS update; run ww image update to recover"
    );
    let state = Receipt::load(root)?;
    for (index, name) in SOFTWARE.iter().enumerate().skip(1) {
        let path = root.join(name);
        parents(home, &path)?;
        let bytes = read_regular(&path)?.with_context(|| {
            format!(
                "missing {}; rerun ww install with the recorded package",
                path.display()
            )
        })?;
        ensure!(
            state.hashes[index] == digest(&bytes),
            "{}",
            incompatible(&path)
        );
    }
    Ok(())
}

/// Hold through source capture. The running Host releases this guard after it
/// owns a frozen baseline; FHS updates never change that captured generation.
pub(super) fn startup_guard(
    baseline: Option<&ww::cell::mount::Mount>,
) -> Result<Option<MutationLock>> {
    let Some(home) = dirs::home_dir() else {
        return Ok(None);
    };
    startup_guard_at(&home, baseline)
}

fn startup_guard_at(
    home: &Path,
    baseline: Option<&ww::cell::mount::Mount>,
) -> Result<Option<MutationLock>> {
    let root = home.join(".ww");
    let Some(baseline) = baseline else {
        return Ok(None);
    };
    let selected = Path::new(&baseline.source)
        .canonicalize()
        .context("resolve selected FHS")?;
    let managed = match root.join("fhs").canonicalize() {
        Ok(path) => path,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("resolve managed FHS"),
    };
    if selected != managed {
        return Ok(None);
    }
    if !durable::exists(&root.join("installation.json"))?
        && !durable::exists(&root.join("image-update.json"))?
        && !durable::exists(&root.join("host-upgrade.json"))?
    {
        return Ok(None);
    }
    let lock = MutationLock::acquire(home)?;
    lock.reject_pending()?;
    installed_fhs(&root)?;
    Ok(Some(lock))
}

#[cfg(target_os = "linux")]
fn verify_guard_capability(
    bytes: &[u8],
    generation: &ww::upgrade::provenance::AuthenticatedGeneration,
) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;
    use std::time::{Duration, Instant};
    // This is a protocol observation budget, not a service restart default.
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut temporary = tempfile::NamedTempFile::new()?;
    temporary.write_all(bytes)?;
    temporary
        .as_file()
        .set_permissions(std::fs::Permissions::from_mode(0o700))?;
    temporary.as_file().sync_all()?;
    let temporary = temporary.into_temp_path();
    let executable = ww::upgrade::attestation::VerifiedExecutable::open(
        &temporary,
        generation.artifact(),
        deadline,
    )?;
    ww::upgrade::native_linux::verify_guard_capability(&executable, deadline)
}

#[cfg(not(target_os = "linux"))]
fn verify_guard_capability(
    _: &[u8],
    _: &ww::upgrade::provenance::AuthenticatedGeneration,
) -> Result<()> {
    anyhow::bail!("guarded native installation is unavailable: exact running-executable evidence is not qualified on this platform")
}

pub(super) async fn install(args: InstallArgs) -> Result<()> {
    let home = dirs::home_dir().context("cannot determine HOME")?;
    let lock = MutationLock::acquire(&home)?;
    lock.reject_pending()?;
    let root = home.join(".ww");
    let selected = if args.release.release.is_some() {
        Some(upgrade_cmd::select_release(&args.release, Scope::Both).await?)
    } else if let Some(root) = &args.bootstrap_release {
        // The shell's manifest/locator handoff does not carry the complete
        // signed descriptor proof. Fetch immutable metadata through the same
        // independent verifier before any guarded installation writes.
        let mut release = args.release.clone();
        release.release = Some(root.clone());
        Some(upgrade_cmd::select_release(&release, Scope::Metadata).await?)
    } else {
        None
    };
    let state = Receipt::read(&root)?;
    let guarded_metadata = ww::upgrade::startup::metadata_present(&home)?;
    ensure!(!guarded_metadata || selected.is_some(), "guarded installation requires its exact authenticated release; use ww install --release <RECORDED_ROOT>");
    let guarded = selected.is_some() && (state.is_none() || guarded_metadata);
    ensure!(selected.is_some() || args.payload.is_some() || state.is_some(), "automatic release discovery awaits trusted channel provisioning; use ww install --release <CID> or the authenticated shell bootstrap");
    let current = std::env::current_exe()?.canonicalize()?;
    let native = if args.release.release.is_some() {
        selected
            .as_ref()
            .and_then(|s| s.host.clone())
            .context("selected Host bytes missing")?
    } else {
        std::fs::read(&current)?
    };
    let (kernel, status) = if args.release.release.is_some() {
        let s = selected.as_ref().context("selected release missing")?;
        (
            s.kernel.clone().context("selected kernel missing")?,
            s.status.clone().context("selected status missing")?,
        )
    } else if let Some(payload) = &args.payload {
        let kernel = ww::packaging::read_payload(&payload.join("boot/kernel.wasm"))?;
        let status = ww::packaging::read_payload(&payload.join("bin/status.wasm"))?;
        (kernel, status)
    } else if state.is_some() {
        installed_fhs(&root)?;
        (
            std::fs::read(root.join(SOFTWARE[1]))?,
            std::fs::read(root.join(SOFTWARE[2]))?,
        )
    } else {
        (
            super::EMBEDDED_KERNEL.to_vec(),
            super::EMBEDDED_STATUS.to_vec(),
        )
    };
    let bytes = [native, kernel, status];
    if let Some(selected) = &selected {
        selected.selection.verify_host(&bytes[0])?;
        selected.selection.verify_deployment(&bytes[1], &bytes[2])?;
    }
    if args.payload.is_some() {
        let keys = ww::release::auth::TrustedKeys::from_bytes(&upgrade_cmd::read_bounded(
            &args.release.trusted_release_keys,
            122,
        )?)?;
        let manifest = upgrade_cmd::read_bounded(
            args.manifest
                .as_ref()
                .context("bootstrap manifest required")?,
            ww::release::auth::MAX_MANIFEST_BYTES,
        )?;
        let signature = upgrade_cmd::read_bounded(
            args.signature
                .as_ref()
                .context("bootstrap signature required")?,
            64,
        )?;
        let authenticated = keys.authenticate(&manifest, &signature)?;
        let platform_path = ww::release::descriptor::platform_path(upgrade_cmd::platform()?)?;
        for (path, bytes) in [platform_path, "fhs/boot/kernel.wasm", "fhs/bin/status.wasm"]
            .iter()
            .zip(&bytes)
        {
            authenticated.verify_artifact(path, bytes)?;
        }
    }
    if let Some(payload) = &args.payload {
        ww::packaging::verify_fhs(payload, &bytes[1], &bytes[2])?;
    } else {
        ww::packaging::validate_payload(&bytes[1], &bytes[2])?;
    }
    for (index, payload) in bytes.iter().enumerate() {
        let path = root.join(SOFTWARE[index]);
        parents(&home, &path)?;
        if let Some(state) = &state {
            ensure!(
                state.hashes[index] == digest(payload),
                "{} differs from the recorded artifact; install made no changes. Use {}",
                path.display(),
                if index == 0 {
                    "ww upgrade --release <CID>"
                } else {
                    "ww image update --release <CID>"
                }
            );
        }
        if let Some(existing) = read_regular(&path)? {
            let same_running_exe = index == 0
                && path.canonicalize().is_ok_and(|p| p == current)
                && existing == *payload;
            ensure!(
                same_running_exe
                    || state
                        .as_ref()
                        .is_some_and(|s| recognized(s, index, &existing)),
                "{}",
                incompatible(&path)
            );
        }
    }
    let identity = root.join("identity");
    parents(&home, &identity)?;
    let key = if read_regular(&identity)?.is_some() {
        Some(
            ww::keys::load(identity.to_str().context("identity path is non-UTF-8")?)
                .context("existing identity is malformed; refusing to replace it")?,
        )
    } else {
        None
    };
    ensure!(
        state.is_none()
            || key.is_some()
            || (guarded && state.as_ref().is_some_and(|s| !s.complete)),
        "managed installation identity is missing; restore the original identity from backup"
    );
    if let Some(s) = &state {
        if !s.complete && s.version == 1 {
            // Exact v1 recovery preserves all five legacy ownership slots.
            for (index, path) in [(3, root.join("etc/ns/ww")), (4, service_path(&home))] {
                parents(&home, &path)?;
                let existing = read_regular(&path)?.with_context(|| {
                    format!(
                        "incomplete version-1 installation: restore recorded {} before retrying",
                        path.display()
                    )
                })?;
                ensure!(
                    digest(&existing) == s.hashes[index],
                    "incomplete version-1 installation: {} is not the recorded generation",
                    path.display()
                );
            }
        }
    }
    if guarded && !state.as_ref().is_some_and(|state| state.complete) {
        verify_guard_capability(
            &bytes[0],
            &selected
                .as_ref()
                .context("guarded proof missing")?
                .generation,
        )?;
    }
    let changed_paths = SOFTWARE
        .iter()
        .zip(&bytes)
        .filter_map(|(name, bytes)| {
            let path = root.join(name);
            match read_regular(&path) {
                Ok(Some(old)) if old == *bytes => None,
                _ => Some(path),
            }
        })
        .chain(key.is_none().then_some(identity.clone()))
        .collect();
    let mut release = selected
        .as_ref()
        .map(|s| s.root.to_string())
        .or(args.bootstrap_release)
        .map(|value| upgrade_cmd::release_cid(&value).map(|cid| cid.to_string()))
        .transpose()?;
    if args.check && guarded && state.as_ref().is_some_and(|receipt| receipt.complete) {
        let lease = ww::upgrade::startup::StartupLease::shared(&home)?;
        let startup = ww::upgrade::validate_owned_installation(&lock, &lease)?;
        ensure!(
            matches!(
                startup.disposition,
                ww::upgrade::startup::StartupDisposition::RequiredUnbound
                    | ww::upgrade::startup::StartupDisposition::Committed { .. }
            ),
            "finish guarded recovery before checking installation"
        );
        ensure!(
            selected
                .as_ref()
                .context("guarded proof missing")?
                .generation
                .artifact()
                == &startup.native.artifact,
            "installation contains another authenticated native artifact"
        );
        release = Some(startup.native.transport_root);
    }
    if !args.check {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&root)?;
        let mut next = state.clone().unwrap_or(Receipt {
            version: 2,
            hashes: std::array::from_fn(|_| digest(b"")),
            previous: None,
            complete: true,
        });
        // A v1 incomplete receipt is only finalized after all five recorded
        // identities have been established; no new package is selected.
        if next.version == 1 && next.complete {
            next = migrate(&root, &next)?;
        }
        // A guarded retry must reproduce the first intent byte-for-byte.
        // Recomputing previous from an incomplete receipt changes that intent.
        if next.complete {
            next.previous = Some(next.hashes.clone());
        }
        for (index, payload) in bytes.iter().enumerate() {
            next.hashes[index] = digest(payload);
        }
        next.complete = false;
        if guarded {
            for path in SOFTWARE {
                std::fs::DirBuilder::new()
                    .recursive(true)
                    .mode(0o700)
                    .create(root.join(path).parent().context("software parent")?)?;
            }
            let selected = selected.as_ref().context("guarded release proof missing")?;
            let mut engine = ww::upgrade::Engine::from_lock(lock)?;
            let repeated = if let Some(receipt) = state.as_ref().filter(|state| state.complete) {
                engine.verify_complete_installation(&selected.generation, receipt)?
            } else {
                false
            };
            if repeated {
                release = Some(engine.startup_state()?.native.transport_root);
            } else {
                let initialization =
                    engine.prepare_initialization(&selected.generation, &next, &selected.trust)?;
                if key.is_none() {
                    ensure!(
                        initialization.may_create_identity()?,
                        "recorded installation identity is missing; restore the original identity"
                    );
                    persist_identity(&identity, &ww::keys::generate()?)?;
                }
                for index in 0..3 {
                    let path = root.join(SOFTWARE[index]);
                    if read_regular(&path)?.as_deref() != Some(bytes[index].as_slice()) {
                        atomic_write(&path, &bytes[index], index == 0)?;
                    }
                }
                // The incomplete receipt remains the startup fence until both
                // FHS slots and the native/proof/locator tuple are durable.
                for index in 1..3 {
                    ensure!(
                        read_regular(&root.join(SOFTWARE[index]))?.as_deref()
                            == Some(bytes[index].as_slice()),
                        "installed FHS differs from the authenticated initialization"
                    );
                }
                next.complete = true;
                next.previous = None;
                initialization.finish(&next)?;
            }
        } else {
            if key.is_none() {
                persist_identity(&identity, &ww::keys::generate()?)?;
            }
            durable::write_record(&root.join("installation.json"), &next.encode()?)?;
            for index in 0..3 {
                let path = root.join(SOFTWARE[index]);
                if read_regular(&path)?.as_deref() != Some(bytes[index].as_slice()) {
                    atomic_write(&path, &bytes[index], index == 0)?;
                }
            }
            next.complete = true;
            next.previous = None;
            durable::write_record(&root.join("installation.json"), &next.encode()?)?;
            if next.version == 1 {
                let migrated = migrate(&root, &next)?;
                durable::write_record(&root.join("installation.json"), &migrated.encode()?)?;
            }
            if let Some(release) = &release {
                durable::write_record(
                    &root.join("installed-release.json"),
                    &serde_json::to_vec(&upgrade_cmd::InstalledRelease {
                        version: 1,
                        release: release.clone(),
                    })?,
                )?;
            }
        }
    }
    Report { version: 1, operation: "install", changed_paths, target: root, release, fhs: Some(FhsDigests {kernel_blake3: digest(&bytes[1]), status_blake3: digest(&bytes[2])}), previous_fhs: state.as_ref().map(FhsDigests::from), activation: false, check: args.check, recovery: None,
        warnings: vec!["Host, baseline FHS, and identity only. Register a daemon separately with ww daemon install.".into()], service: None }.print(args.json)
}

fn service_path(home: &Path) -> PathBuf {
    home.join(if cfg!(target_os = "macos") {
        "Library/LaunchAgents/io.wetware.ww.plist"
    } else {
        ".config/systemd/user/ww.service"
    })
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ImageJournal {
    version: u8,
    before: Vec<u8>,
    after: Vec<u8>,
    release: String,
}

fn image_store(root: &Path, hash: &str) -> Result<PathBuf> {
    ensure!(
        hash.len() == 64
            && hash
                .bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()),
        "invalid retained FHS digest"
    );
    Ok(root.join(".fhs-recovery").join(hash))
}

fn image_generation(root: &Path, state: &Receipt) -> Result<[Vec<u8>; 2]> {
    let mut result = [Vec::new(), Vec::new()];
    for index in 1..3 {
        let path = image_store(root, &state.hashes[index])?;
        parents(root.parent().context("HOME")?, &path)?;
        let bytes = read_regular(&path)?.context("retained FHS artifact is missing")?;
        ensure!(
            digest(&bytes) == state.hashes[index],
            "retained FHS artifact is corrupt"
        );
        result[index - 1] = bytes;
    }
    Ok(result)
}

fn recover_image(root: &Path, check: bool) -> Result<Option<ImageRecovery>> {
    let Some(bytes) = durable::read_record(&root.join("image-update.json"))? else {
        return Ok(None);
    };
    let journal: ImageJournal =
        serde_json::from_slice(&bytes).context("invalid FHS recovery journal")?;
    ensure!(journal.version == 1, "unsupported FHS recovery journal");
    let before = Receipt::decode(&journal.before)?;
    let after = Receipt::decode(&journal.after)?;
    ensure!(
        before.complete
            && after.complete
            && before.hashes[0] == after.hashes[0]
            && before.hashes[3..] == after.hashes[3..],
        "invalid FHS recovery scope"
    );
    let current = Receipt::read(root)?.context("FHS recovery ownership receipt is missing")?;
    ensure!(current.encode()? == before.encode()? || current.encode()? == after.encode()?, "installation receipt changed outside the recorded FHS transaction; preserve recovery evidence");
    let (state, payload, action, release) = match image_generation(root, &after) {
        Ok(payload) => (&after, payload, "completed recorded FHS update", Some(journal.release.clone())),
        Err(_) => (&before, image_generation(root, &before).context("neither recorded FHS generation is recoverable; preserve journal and restore retained artifacts")?, "restored previous FHS baseline", None),
    };
    for (index, name) in SOFTWARE.iter().enumerate().skip(1) {
        let path = root.join(name);
        parents(root.parent().context("HOME")?, &path)?;
        if let Some(existing) = read_regular(&path)? {
            let hash = digest(&existing);
            ensure!(
                hash == before.hashes[index] || hash == after.hashes[index],
                "{}",
                incompatible(&path)
            );
        }
    }
    if !check {
        for index in 1..3 {
            atomic_write(&root.join(SOFTWARE[index]), &payload[index - 1], false)?;
        }
        durable::write_record(&root.join("installation.json"), &state.encode()?)?;
        durable::write_record(&root.join("image-update-retained.json"), &bytes)?;
        durable::remove(&root.join("image-update.json"))?;
    }
    Ok(Some(ImageRecovery {
        release,
        fhs: state.into(),
        previous_fhs: (&before).into(),
        action,
    }))
}

pub(super) async fn image_update(args: UpdateArgs) -> Result<()> {
    let home = dirs::home_dir().context("cannot determine HOME")?;
    let lock = MutationLock::acquire(&home)?;
    let root = home.join(".ww");
    parents(&home, &root.join("image-update.json"))?;
    ensure!(
        !durable::exists(&root.join("host-upgrade.json"))?,
        "pending Host upgrade; recover it with ww upgrade first"
    );
    if let Some(recovered) = recover_image(&root, args.check)? {
        return Report { version:1, operation:"image update", changed_paths: if args.check { Vec::new() } else { SOFTWARE[1..].iter().map(|p| root.join(p)).collect() }, target:root.join("fhs"),release:recovered.release,fhs:Some(recovered.fhs),previous_fhs:Some(recovered.previous_fhs),activation:false,check:args.check,recovery:Some(recovered.action),warnings:vec!["Recovery handled only the recorded generation. Invoke image update again to request another release.".into()], service:None }.print(args.json);
    }
    lock.reject_pending()?;
    installed_fhs(&root)?;
    let before = Receipt::load(&root)?;
    let installed = read_regular(&root.join(SOFTWARE[0]))?.context("installed Host is missing")?;
    ensure!(
        digest(&installed) == before.hashes[0],
        "installed Host differs from its ownership receipt"
    );
    ensure!(std::fs::read(std::env::current_exe()?)? == installed, "image update must run with the installed Host so baseline validation uses its production engine");
    let selected = upgrade_cmd::select_release(&args.release, Scope::Fhs).await?;
    let kernel = selected.kernel.context("selected kernel missing")?;
    let status = selected.status.context("selected status missing")?;
    ww::packaging::validate_payload(&kernel, &status)?;
    let payload = [kernel, status];
    let mut after = before.clone();
    for index in 1..3 {
        after.hashes[index] = digest(&payload[index - 1]);
    }
    after.version = 2;
    after.previous = None;
    let changed = before.hashes[1..3] != after.hashes[1..3];
    let mut warnings = vec!["Baseline only; the Host was not restarted. Activation requires an explicit later start or restart.".into()];
    if root.join("etc/ns").is_dir() {
        for entry in std::fs::read_dir(root.join("etc/ns"))? {
            let path = entry?.path();
            warnings.push(format!("Preserved namespace {} can shadow updated baseline paths and retain its configured publication target.", path.display()));
        }
    }
    if let Some(external) = std::env::var_os("WW_FHS_ROOT") {
        if Path::new(&external) != root.join("fhs") {
            warnings.push(format!(
                "WW_FHS_ROOT selects {}; this operation targets only {}.",
                Path::new(&external).display(),
                root.join("fhs").display()
            ));
        }
    }
    if !args.check && changed {
        let _ = migrate(&root, &before)?;
        let store = root.join(".fhs-recovery");
        parents(&home, &store.join("artifact"))?;
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&store)?;
        durable::directory(&store)?;
        for index in 1..3 {
            atomic_write(
                &image_store(&root, &before.hashes[index])?,
                &read_regular(&root.join(SOFTWARE[index]))?.context("missing installed FHS")?,
                false,
            )?;
            atomic_write(
                &image_store(&root, &after.hashes[index])?,
                &payload[index - 1],
                false,
            )?;
        }
        let journal = ImageJournal {
            version: 1,
            before: before.encode()?,
            after: after.encode()?,
            release: selected.root.to_string(),
        };
        durable::write_record(
            &root.join("image-update.json"),
            &serde_json::to_vec(&journal)?,
        )?;
        recover_image(&root, false)?;
    }
    Report {
        version: 1,
        operation: "image update",
        changed_paths: if changed {
            SOFTWARE[1..].iter().map(|p| root.join(p)).collect()
        } else {
            Vec::new()
        },
        target: root.join("fhs"),
        release: Some(selected.root.to_string()),
        fhs: Some((&after).into()),
        previous_fhs: Some((&before).into()),
        activation: false,
        check: args.check,
        recovery: None,
        warnings,
        service: None,
    }
    .print(args.json)
}

pub(super) async fn uninstall(check: bool, json: bool) -> Result<()> {
    let home = dirs::home_dir().context("cannot determine HOME")?;
    let lock = MutationLock::acquire(&home)?;
    uninstall_with(&lock, check, |lock, check| {
        let service = super::daemon_cmd::preflight_removal(lock)?;
        super::daemon_cmd::remove_locked(lock, service, check)
    })?
    .print(json)
}

fn uninstall_with(
    lock: &MutationLock,
    check: bool,
    remove_service: impl FnOnce(&MutationLock, bool) -> Result<super::daemon_cmd::RemovalReport>,
) -> Result<Report> {
    lock.reject_pending()?;
    let home = lock.home();
    let root = home.join(".ww");
    parents(home, &root.join("installation.json"))?;
    let state = match Receipt::read(&root)? {
        Some(state) => state,
        None => Receipt::decode(&durable::read_record(&root.join("uninstalled-software.json"))?
            .context("uninstall requires managed ownership; external/package-manager software is not owned")?)?,
    };
    ensure!(
        state.complete,
        "recover the incomplete installation before removing software"
    );
    let mut paths = Vec::new();
    for (index, name) in SOFTWARE.iter().enumerate() {
        let path = root.join(name);
        parents(home, &path)?;
        if let Some(bytes) = read_regular(&path)? {
            ensure!(
                digest(&bytes) == state.hashes[index],
                "{}",
                incompatible(&path)
            );
            paths.push(path);
        }
    }
    // Validate all metadata before changing lifecycle state or software.
    for name in [
        "installation.json",
        "installed-release.json",
        "uninstalled-software.json",
    ] {
        durable::read_record(&root.join(name))?;
    }
    // The shared native owner validates guarded provenance and blocks starts
    // before lifecycle commands can stop the owned service.
    let removal = ww::upgrade::removal::prepare(lock, check)?;
    let service = remove_service(lock, check)?;
    let _quiescent = if check {
        None
    } else {
        removal
            .map(ww::upgrade::removal::Removal::quiesce)
            .transpose()?
    };
    if !check {
        let _ = migrate(&root, &state)?;
        durable::write_record(&root.join("uninstalled-software.json"), &state.encode()?)?;
        for index in (1..3).rev() {
            let path = root.join(SOFTWARE[index]);
            if let Some(bytes) = read_regular(&path)? {
                ensure!(
                    digest(&bytes) == state.hashes[index],
                    "{}",
                    incompatible(&path)
                );
                durable::remove(&path)?;
            }
        }
        durable::remove(&root.join("installed-release.json"))?;
        durable::remove(&root.join("installation.json"))?;
        // The tombstone above is usable ownership for a retry after metadata
        // cleanup. Delete the runnable retry entrypoint only as the last step.
        let native = root.join(SOFTWARE[0]);
        if let Some(bytes) = read_regular(&native)? {
            ensure!(
                digest(&bytes) == state.hashes[0],
                "{}",
                incompatible(&native)
            );
            durable::remove(&native)?;
        }
    }
    Ok(Report { version:1,operation:"uninstall",changed_paths:paths,target:root,release:None,fhs:None,previous_fhs:None,activation:false,check,recovery:None,warnings:vec!["Identity, namespaces, keys, application data, logs, signed state, and recovery evidence were preserved.".into()],service:Some(service) })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> (tempfile::TempDir, Receipt, Receipt) {
        let home = tempfile::tempdir().unwrap();
        let root = home.path().join(".ww");
        std::fs::create_dir_all(root.join(".fhs-recovery")).unwrap();
        let mut before = Receipt {
            version: 2,
            hashes: std::array::from_fn(|_| digest(b"legacy")),
            previous: None,
            complete: true,
        };
        let mut after = before.clone();
        for (index, (old, new)) in [
            (b"old kernel".as_slice(), b"new kernel".as_slice()),
            (b"old status".as_slice(), b"new status".as_slice()),
        ]
        .iter()
        .enumerate()
        {
            before.hashes[index + 1] = digest(old);
            after.hashes[index + 1] = digest(new);
            atomic_write(&root.join(SOFTWARE[index + 1]), old, false).unwrap();
            atomic_write(&image_store(&root, &digest(old)).unwrap(), old, false).unwrap();
            atomic_write(&image_store(&root, &digest(new)).unwrap(), new, false).unwrap();
        }
        atomic_write(&root.join("identity"), b"private identity sentinel", false).unwrap();
        atomic_write(
            &root.join("etc/ns/ww"),
            b"operator namespace sentinel",
            false,
        )
        .unwrap();
        atomic_write(
            &root.join("ipns-watermark"),
            b"authoritative state sentinel",
            false,
        )
        .unwrap();
        let journal = ImageJournal {
            version: 1,
            before: before.encode().unwrap(),
            after: after.encode().unwrap(),
            release: "authenticated-release-sentinel".into(),
        };
        durable::write_record(&root.join("installation.json"), &before.encode().unwrap()).unwrap();
        durable::write_record(
            &root.join("image-update.json"),
            &serde_json::to_vec(&journal).unwrap(),
        )
        .unwrap();
        (home, before, after)
    }

    #[test]
    fn every_partial_fhs_generation_recovers_without_touching_private_state() {
        for written in 0..4 {
            for receipt_committed in [false, true] {
                let (home, _, after) = fixture();
                let root = home.path().join(".ww");
                for (index, name) in SOFTWARE.iter().enumerate().skip(1) {
                    if written & (1 << (index - 1)) != 0 {
                        let new = read_regular(&image_store(&root, &after.hashes[index]).unwrap())
                            .unwrap()
                            .unwrap();
                        atomic_write(&root.join(name), &new, false).unwrap();
                    }
                }
                if receipt_committed {
                    durable::write_record(
                        &root.join("installation.json"),
                        &after.encode().unwrap(),
                    )
                    .unwrap();
                }
                assert!(installed_fhs(&root).is_err());
                let recovered = recover_image(&root, false).unwrap().unwrap();
                assert_eq!(recovered.action, "completed recorded FHS update");
                assert_eq!(
                    recovered.release.as_deref(),
                    Some("authenticated-release-sentinel")
                );
                assert_eq!(recovered.fhs, (&after).into());
                installed_fhs(&root).unwrap();
                assert_eq!(Receipt::load(&root).unwrap().hashes, after.hashes);
                assert_eq!(
                    std::fs::read(root.join("identity")).unwrap(),
                    b"private identity sentinel"
                );
                assert_eq!(
                    std::fs::read(root.join("etc/ns/ww")).unwrap(),
                    b"operator namespace sentinel"
                );
                assert_eq!(
                    std::fs::read(root.join("ipns-watermark")).unwrap(),
                    b"authoritative state sentinel"
                );
                assert!(root.join("image-update-retained.json").is_file());
                assert!(recover_image(&root, false).unwrap().is_none());
            }
        }
    }

    #[test]
    fn missing_target_restores_the_complete_previous_baseline() {
        let (home, before, after) = fixture();
        let root = home.path().join(".ww");
        durable::remove(&image_store(&root, &after.hashes[1]).unwrap()).unwrap();
        let new_status = read_regular(&image_store(&root, &after.hashes[2]).unwrap())
            .unwrap()
            .unwrap();
        atomic_write(&root.join(SOFTWARE[2]), &new_status, false).unwrap();
        let recovered = recover_image(&root, false).unwrap().unwrap();
        assert_eq!(recovered.action, "restored previous FHS baseline");
        assert_eq!(recovered.release, None);
        assert_eq!(recovered.fhs, (&before).into());
        assert_eq!(Receipt::load(&root).unwrap().hashes, before.hashes);
        installed_fhs(&root).unwrap();
    }

    #[test]
    fn unavailable_generations_leave_managed_startup_closed() {
        let (home, before, after) = fixture();
        let root = home.path().join(".ww");
        for state in [&before, &after] {
            durable::remove(&image_store(&root, &state.hashes[1]).unwrap()).unwrap();
        }
        assert!(recover_image(&root, false).is_err());
        assert!(root.join("image-update.json").is_file());
        assert!(installed_fhs(&root).is_err());
    }

    #[test]
    fn recovery_preflight_and_unknown_collisions_never_replace_files() {
        let (home, _, _) = fixture();
        let root = home.path().join(".ww");
        let old = std::fs::read(root.join(SOFTWARE[1])).unwrap();
        recover_image(&root, true).unwrap();
        assert_eq!(std::fs::read(root.join(SOFTWARE[1])).unwrap(), old);
        assert!(root.join("image-update.json").exists());
        atomic_write(&root.join(SOFTWARE[2]), b"operator changed status", false).unwrap();
        assert!(recover_image(&root, false).is_err());
        assert_eq!(std::fs::read(root.join(SOFTWARE[1])).unwrap(), old);
        assert_eq!(
            std::fs::read(root.join(SOFTWARE[2])).unwrap(),
            b"operator changed status"
        );
    }

    #[test]
    fn recovery_rejects_receipt_changes_outside_its_scope() {
        let (home, mut before, _) = fixture();
        let root = home.path().join(".ww");
        before.hashes[0] = digest(b"different native Host");
        durable::write_record(&root.join("installation.json"), &before.encode().unwrap()).unwrap();
        assert!(recover_image(&root, false).is_err());
        assert_eq!(Receipt::load(&root).unwrap().hashes, before.hashes);
    }

    #[test]
    fn startup_guard_excludes_updates_until_capture_finishes() {
        let (home, _, _) = fixture();
        let root = home.path().join(".ww");
        let baseline = ww::cell::mount::Mount {
            source: root.join("fhs").to_str().unwrap().into(),
            target: ww::cell::image::GuestTarget::root(),
        };
        assert!(startup_guard_at(home.path(), Some(&baseline)).is_err());
        let mut alias = baseline.clone();
        alias.source = root.join("fhs/../fhs").to_str().unwrap().into();
        assert!(startup_guard_at(home.path(), Some(&alias)).is_err());
        recover_image(&root, false).unwrap();
        let guard = startup_guard_at(home.path(), Some(&baseline))
            .unwrap()
            .unwrap();
        assert!(MutationLock::acquire(home.path()).is_err());
        assert!(startup_guard_at(home.path(), Some(&alias)).is_err());
        drop(guard);
        assert!(MutationLock::acquire(home.path()).is_ok());
    }

    #[test]
    fn managed_startup_rejects_pending_recovery_without_receipt() {
        let (home, _, _) = fixture();
        let root = home.path().join(".ww");
        durable::remove(&root.join("installation.json")).unwrap();
        let baseline = ww::cell::mount::Mount {
            source: root.join("fhs").to_str().unwrap().into(),
            target: ww::cell::image::GuestTarget::root(),
        };
        assert!(startup_guard_at(home.path(), Some(&baseline)).is_err());
        assert!(recover_image(&root, false).is_err());
        durable::remove(&root.join("image-update.json")).unwrap();
        durable::write_record(&root.join("host-upgrade.json"), b"pending").unwrap();
        assert!(startup_guard_at(home.path(), Some(&baseline)).is_err());
    }

    #[test]
    fn external_baseline_does_not_require_a_managed_fhs() {
        let (home, _, _) = fixture();
        let root = home.path().join(".ww");
        let external = home.path().join("external-fhs");
        std::fs::rename(root.join("fhs"), &external).unwrap();
        let baseline = ww::cell::mount::Mount {
            source: external.to_str().unwrap().into(),
            target: ww::cell::image::GuestTarget::root(),
        };
        assert!(startup_guard_at(home.path(), Some(&baseline))
            .unwrap()
            .is_none());
    }

    fn absent_service(
        lock: &MutationLock,
        check: bool,
    ) -> Result<super::super::daemon_cmd::RemovalReport> {
        Ok(super::super::daemon_cmd::RemovalReport {
            changed: false,
            checked: check,
            path: service_path(lock.home()),
            backup: None,
            registered: false,
            running: false,
            enabled: false,
        })
    }

    #[test]
    fn software_removal_preserves_state_unknown_files_and_retry_evidence() {
        let (home, _, _) = fixture();
        let root = home.path().join(".ww");
        recover_image(&root, false).unwrap();
        atomic_write(&root.join(SOFTWARE[0]), b"legacy", true).unwrap();
        atomic_write(
            &root.join("fhs/custom.wasm"),
            b"unknown operator content",
            false,
        )
        .unwrap();
        let lock = MutationLock::acquire(home.path()).unwrap();
        uninstall_with(&lock, true, absent_service).unwrap();
        assert!(root.join(SOFTWARE[0]).exists());
        uninstall_with(&lock, false, absent_service).unwrap();
        for path in SOFTWARE {
            assert!(!root.join(path).exists());
        }
        assert!(!root.join("installation.json").exists());
        assert!(root.join("uninstalled-software.json").exists());
        assert_eq!(
            std::fs::read(root.join("fhs/custom.wasm")).unwrap(),
            b"unknown operator content"
        );
        assert_eq!(
            std::fs::read(root.join("identity")).unwrap(),
            b"private identity sentinel"
        );
        assert_eq!(
            std::fs::read(root.join("etc/ns/ww")).unwrap(),
            b"operator namespace sentinel"
        );
        assert_eq!(
            std::fs::read(root.join("ipns-watermark")).unwrap(),
            b"authoritative state sentinel"
        );
        assert!(uninstall_with(&lock, false, absent_service)
            .unwrap()
            .changed_paths
            .is_empty());
    }

    #[test]
    fn removal_failure_retains_the_executable_for_retry() {
        use std::os::unix::fs::PermissionsExt;
        if unsafe { libc::geteuid() } == 0 {
            return;
        }
        let (home, _, _) = fixture();
        let root = home.path().join(".ww");
        recover_image(&root, false).unwrap();
        atomic_write(&root.join(SOFTWARE[0]), b"legacy", true).unwrap();
        let lock = MutationLock::acquire(home.path()).unwrap();
        let boot = root.join("fhs/boot");
        std::fs::set_permissions(&boot, std::fs::Permissions::from_mode(0o500)).unwrap();
        let result = uninstall_with(&lock, false, absent_service);
        std::fs::set_permissions(&boot, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert!(result.is_err());
        assert!(!root.join(SOFTWARE[2]).exists());
        assert_eq!(std::fs::read(root.join(SOFTWARE[0])).unwrap(), b"legacy");
        uninstall_with(&lock, false, absent_service).unwrap();
        assert!(!root.join(SOFTWARE[0]).exists());
    }

    #[test]
    fn removal_validates_locator_before_service_effects() {
        let (home, _, _) = fixture();
        let root = home.path().join(".ww");
        recover_image(&root, false).unwrap();
        atomic_write(&root.join(SOFTWARE[0]), b"legacy", true).unwrap();
        std::os::unix::fs::symlink(root.join("identity"), root.join("installed-release.json"))
            .unwrap();
        let lock = MutationLock::acquire(home.path()).unwrap();
        assert!(uninstall_with(&lock, false, |_, _| panic!(
            "invalid locator must refuse before lifecycle mutation"
        ))
        .is_err());
        assert!(root.join(SOFTWARE[0]).exists());
    }

    #[test]
    fn receipt_migration_retains_legacy_provenance_and_original_bytes() {
        let (home, mut before, _) = fixture();
        let root = home.path().join(".ww");
        before.version = 1;
        let original = before.encode().unwrap();
        let scoped = migrate(&root, &before).unwrap();
        assert_eq!(scoped.version, 2);
        assert_eq!(scoped.hashes, before.hashes);
        assert_eq!(
            std::fs::read(root.join("installation-v1.json")).unwrap(),
            original
        );
        let json: serde_json::Value = serde_json::from_slice(&scoped.encode().unwrap()).unwrap();
        assert_eq!(json["legacy"]["namespace"], before.hashes[3]);
        assert_eq!(json["fhs"]["kernel"], before.hashes[1]);
        before.complete = false;
        assert!(migrate(&root, &before).is_err());
    }

    #[test]
    fn recovery_refuses_symlinked_store_and_identity_creation_never_overwrites() {
        let (home, _, _) = fixture();
        let root = home.path().join(".ww");
        let moved = root.join("retained-store");
        std::fs::rename(root.join(".fhs-recovery"), &moved).unwrap();
        std::os::unix::fs::symlink(&moved, root.join(".fhs-recovery")).unwrap();
        assert!(recover_image(&root, false).is_err());
        let identity = root.join("identity");
        assert!(persist_identity(&identity, &ww::keys::generate().unwrap()).is_err());
        assert_eq!(
            std::fs::read(identity).unwrap(),
            b"private identity sentinel"
        );
    }
}
