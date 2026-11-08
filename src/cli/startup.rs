//! Capture startup inputs before admission, then consume the same capture.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use anyhow::{ensure, Context, Result};
use sha2::{Digest, Sha256};
use tokio::sync::watch;
use ww::{
    cell::{image, mount::Mount},
    upgrade::{
        attestation::PinnedProcess,
        inputs::{
            EffectiveRunConfig, ImmutableSourceInput, MountInput, NamespaceInput, PreflightInputs,
            PublisherInput, RuntimeCachePolicy, RuntimeSettings, SourceKind,
        },
        startup::{
            InstanceLease, InvocationBinding, StartupContractV1, StartupDisposition, StartupLease,
        },
    },
};

use super::{Commands, RunArgs};

const PROXY_ENVIRONMENT: &[&str] = &[
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "ALL_PROXY",
    "NO_PROXY",
    "http_proxy",
    "https_proxy",
    "all_proxy",
    "no_proxy",
];

fn unsupported_environment_name(name: &str, nonempty: bool) -> bool {
    name.starts_with("LD_")
        || name.starts_with("DYLD_")
        || name == "WW_TEST_PID0_RESULT_RACE"
        || (nonempty && PROXY_ENVIRONMENT.contains(&name))
}

pub(super) struct NormalizedRun {
    pub config: EffectiveRunConfig,
    pub mounts: Vec<Mount>,
    pub baseline: Option<Mount>,
}

pub(super) struct CapturedNamespaces {
    pub configs: Vec<ww::ns::NamespaceConfig>,
    pub inputs: Vec<NamespaceInput>,
    _captures: Vec<ww::ns::CapturedNamespaceConfig>,
}

impl NormalizedRun {
    pub fn namespaces(&self) -> Result<CapturedNamespaces> {
        let captures = Commands::startup_namespaces_captured(
            &self.mounts,
            &self.config.namespace_roots,
            self.authoritative(),
            self.baseline.is_some(),
        )?;
        let configs = captures
            .iter()
            .map(|captured| captured.config.clone())
            .collect();
        let inputs = captures
            .iter()
            .map(|captured| NamespaceInput {
                name: captured.config.name.clone(),
                ipns: captured.config.ipns.clone(),
                bootstrap: captured.config.bootstrap.clone(),
                source_path: captured.path.clone(),
                content_sha256: Sha256::digest(&captured.bytes).into(),
            })
            .collect();
        Ok(CapturedNamespaces {
            configs,
            inputs,
            _captures: captures,
        })
    }

    pub fn authoritative(&self) -> bool {
        self.config.stem.is_some() || self.config.ipns_stem.is_some()
    }

    pub fn identity(&self) -> Result<(ed25519_dalek::SigningKey, &'static str)> {
        if let Some(path) = self
            .config
            .identity_path
            .as_ref()
            .filter(|path| path.exists())
        {
            return Ok((
                ww::keys::load(path.to_str().context("identity path is not UTF-8")?)?,
                "file",
            ));
        }
        ensure!(
            self.config.insecure_ephemeral,
            "persistent startup identity is missing"
        );
        Ok((ww::keys::generate()?, "ephemeral"))
    }
}

fn absolute(path: PathBuf, cwd: &Path, home: &Path) -> Result<PathBuf> {
    let path = if let Ok(rest) = path.strip_prefix("~") {
        home.join(rest)
    } else {
        path
    };
    let path = if path.is_absolute() {
        path
    } else {
        cwd.join(path)
    };
    ensure!(path.to_str().is_some(), "startup paths must be UTF-8");
    Ok(path)
}

/// Capture defaults and environment once. This performs local reads only.
pub(super) fn normalize_run(args: RunArgs) -> Result<NormalizedRun> {
    let cwd = std::env::current_dir().context("read invocation CWD")?;
    let home = dirs::home_dir().context("cannot determine invocation HOME")?;
    let runtime = RuntimeSettings {
        kubo_wait_max_secs: super::kubo_wait_max_secs()?,
        kubo_boot_retry_max_secs: super::kubo_boot_retry_max_secs()?,
        kubo_operation_timeout_secs: super::kubo_boot_operation_timeout_secs()?,
        compile_workers: ww::services::capture_compile_worker_count(),
        tracing: ww::config::TracingSettings::capture(),
        compilation_cache: ww::cell::engine::capture_cache_settings().normalize(&cwd),
        interactive: ww::kernel::is_interactive(),
        oci_image_id: std::env::var("WW_OCI_IMAGE_ID")
            .ok()
            .filter(|value| !value.trim().is_empty()),
        unsupported_environment: {
            let mut names: Vec<_> = std::env::vars_os()
                .filter_map(|(name, value)| {
                    let name = name.to_str()?;
                    unsupported_environment_name(name, !value.is_empty()).then(|| name.to_owned())
                })
                .collect();
            names.sort();
            names
        },
    };
    // SAFETY: geteuid has no pointer arguments.
    normalize_with_context(
        args,
        InvocationContext {
            cwd,
            home,
            uid: unsafe { libc::geteuid() },
            runtime,
        },
    )
}

pub(super) struct InvocationContext {
    pub cwd: PathBuf,
    pub home: PathBuf,
    pub uid: u32,
    pub runtime: RuntimeSettings,
}

impl InvocationContext {
    pub fn service(
        cwd: PathBuf,
        home: PathBuf,
        uid: u32,
        environment: &BTreeMap<String, String>,
    ) -> Result<Self> {
        ensure!(
            cwd.is_absolute() && home.is_absolute(),
            "service HOME and working directory must be absolute"
        );
        ensure!(
            !environment.keys().any(|name| name.starts_with("LD_")
                || name.starts_with("DYLD_")
                || name == "WW_TEST_PID0_RESULT_RACE"),
            "unsupported service loader or debug settings"
        );
        let value = |name: &str, default: u64| -> Result<u64> {
            environment
                .get(name)
                .map(|value| {
                    value
                        .parse()
                        .with_context(|| format!("invalid service setting {name}"))
                })
                .unwrap_or(Ok(default))
        };
        let compile_workers = environment.get("WW_COMPILE_WORKERS")
            .and_then(|value| value.parse::<usize>().ok()).filter(|value| *value > 0)
            .context("managed admission requires positive --executor-threads or WW_EXECUTOR_THREADS and positive WW_COMPILE_WORKERS; automatic CPU defaults are unsupported")?;
        let runtime = RuntimeSettings {
            kubo_wait_max_secs: value(
                super::KUBO_WAIT_MAX_SECS_ENV,
                super::KUBO_WAIT_DEFAULT_SECS,
            )?,
            kubo_boot_retry_max_secs: value(
                super::KUBO_BOOT_RETRY_MAX_SECS_ENV,
                super::KUBO_BOOT_RETRY_MAX_SECS_DEFAULT,
            )?,
            kubo_operation_timeout_secs: value(
                super::KUBO_BOOT_OPERATION_TIMEOUT_SECS_ENV,
                super::KUBO_BOOT_OPERATION_TIMEOUT_DEFAULT_SECS,
            )?,
            compile_workers,
            tracing: ww::config::TracingSettings::from_environment(environment, false),
            compilation_cache: ww::cell::engine::CapturedCacheSettings::from_values(
                environment.get("WW_CWASM_DIR").map(PathBuf::from),
                environment
                    .get("WW_CWASM_CACHE_MAX_BYTES")
                    .map(String::as_str),
            )
            .normalize(&cwd),
            interactive: environment.contains_key("WW_TTY"),
            oci_image_id: environment
                .get("WW_OCI_IMAGE_ID")
                .filter(|value| !value.trim().is_empty())
                .cloned(),
            unsupported_environment: environment
                .iter()
                .filter(|(name, value)| unsupported_environment_name(name, !value.is_empty()))
                .map(|(name, _)| name.clone())
                .collect(),
        };
        ensure!(
            runtime.unsupported_environment.is_empty(),
            "guarded service rejects proxy, loader, or debug environment settings"
        );
        ensure!(
            runtime.kubo_operation_timeout_secs > 0,
            "service operation timeout must be positive"
        );
        Ok(Self {
            cwd,
            home,
            uid,
            runtime,
        })
    }
}

pub(super) fn normalize_with_context(
    args: RunArgs,
    context: InvocationContext,
) -> Result<NormalizedRun> {
    let InvocationContext {
        cwd,
        home,
        uid,
        runtime,
    } = context;
    let home = absolute(home, &cwd, &cwd)?;
    let authoritative = args.stem.is_some() || args.ipns_stem.is_some();
    if let Some(stem) = &args.stem {
        super::parse_contract_address(stem)?;
    }
    let ipns_stem = args
        .ipns_stem
        .as_deref()
        .map(ww::ipns::parse_name)
        .transpose()?
        .map(|name| name.to_string());
    let baseline = Commands::installed_baseline_mount(args.fhs_root, Some(&home))?;
    let mut mounts = ww::cell::mount::parse_args(&args.mounts)?;
    ww::cell::mount::freeze_local_paths(&mut mounts, &cwd, Some(&home))?;
    let mut namespace_roots = args
        .namespace_root
        .into_iter()
        .map(|path| absolute(path, &cwd, &home))
        .collect::<Result<Vec<_>>>()?;
    if mounts.is_empty() && namespace_roots.is_empty() && !authoritative {
        namespace_roots.push(home.join(".ww"));
    }
    let listen = if args.listen.is_empty() {
        super::daemon_cmd::default_listen()
            .iter()
            .map(|value| value.to_string())
            .collect()
    } else {
        args.listen.iter().map(ToString::to_string).collect()
    };
    let identity_path = Some(absolute(
        args.identity
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".ww/identity")),
        &cwd,
        &home,
    )?);
    let runtime_cache_policy = match args.runtime_cache_policy.as_str() {
        "shared" => RuntimeCachePolicy::Shared,
        "isolated" => RuntimeCachePolicy::Isolated,
        _ => anyhow::bail!("invalid --runtime-cache-policy (expected shared or isolated)"),
    };
    let config = EffectiveRunConfig {
        cwd: cwd.clone(),
        home,
        uid,
        mounts: mounts
            .iter()
            .map(|mount| MountInput {
                source: mount.source.clone(),
                target: mount.target.to_string(),
            })
            .collect(),
        fhs_root: baseline.as_ref().map(|mount| PathBuf::from(&mount.source)),
        namespace_roots,
        listen,
        wasm_debug: args.wasm_debug,
        identity_path,
        insecure_ephemeral: args.insecure_ephemeral,
        stem: args.stem,
        ipns_stem,
        ipns_routing_url: args.ipns_routing_url,
        rpc_url: args.rpc_url,
        ws_url: args.ws_url,
        confirmation_depth: args.confirmation_depth,
        executor_threads: if args.executor_threads == 0 {
            std::thread::available_parallelism()
                .map(usize::from)
                .unwrap_or(1)
        } else {
            args.executor_threads
        },
        http_listen: args
            .http_listen
            .as_deref()
            .map(str::parse)
            .transpose()
            .context("invalid --http-listen address")?,
        http_dial: args.http_dial,
        runtime_cache_policy,
        http_admin: super::admin_listen_addr(args.with_http_admin)
            .as_deref()
            .map(str::parse)
            .transpose()
            .context("invalid --with-http-admin address")?,
        ipfs_url: args.ipfs_url,
        runtime,
    };
    Ok(NormalizedRun {
        config,
        mounts,
        baseline,
    })
}

/// Parse the manager's actual argv and consumed environment. Clap's ambient
/// environment lookup is disabled; the caller's shell cannot fill missing values.
pub(super) fn normalize_service_run(
    argv: &[String],
    environment: &BTreeMap<String, String>,
    context: InvocationContext,
) -> Result<NormalizedRun> {
    use clap::{Args, FromArgMatches};
    ensure!(
        argv.len() >= 2 && argv[1] == "run",
        "managed invocation must execute ww run directly"
    );
    ensure!(
        Path::new(&argv[0]) == context.home.join(".ww/bin/ww"),
        "managed invocation does not execute the installed Host directly"
    );
    let tail = &argv[2..];
    let mut arguments = vec!["ww-run".to_owned()];
    for (variable, flag) in [
        ("WW_FHS_ROOT", "--fhs-root"),
        ("WW_LISTEN", "--listen"),
        ("WW_IDENTITY", "--identity"),
        ("IPFS_ROUTING_API", "--ipns-routing-url"),
        ("WW_EXECUTOR_THREADS", "--executor-threads"),
        ("WW_RUNTIME_CACHE_POLICY", "--runtime-cache-policy"),
        ("WW_HTTP_ADMIN", "--with-http-admin"),
        ("IPFS_API", "--ipfs-url"),
    ] {
        if !tail.iter().any(|argument| {
            argument == flag
                || argument
                    .strip_prefix(flag)
                    .is_some_and(|suffix| suffix.starts_with('='))
        }) {
            if let Some(value) = environment.get(variable) {
                arguments.extend([flag.to_owned(), value.clone()]);
            }
        }
    }
    arguments.extend_from_slice(tail);
    let matches = RunArgs::augment_args(clap::Command::new("ww-run"))
        .mut_args(|argument| argument.env(None::<&str>))
        .try_get_matches_from(arguments)
        .context("parse effective service invocation")?;
    let args = RunArgs::from_arg_matches(&matches)?;
    ensure!(args.executor_threads > 0, "managed admission requires positive --executor-threads or WW_EXECUTOR_THREADS and positive WW_COMPILE_WORKERS; automatic CPU defaults are unsupported");
    normalize_with_context(args, context)
}

/// Explicit admission preparation may retain captured sources in Kubo. The
/// resulting transport references belong to this single capture and key read.
pub(super) async fn capture_for_admission(
    run: NormalizedRun,
    deadline: Instant,
) -> Result<PreflightInputs> {
    capture_for_contract(run, None, deadline).await
}

pub(super) async fn capture_for_upgrade(
    home: &Path,
    contract: &StartupContractV1,
    deadline: Instant,
    standalone: Option<RunArgs>,
) -> Result<PreflightInputs> {
    let run = match &contract.invocation {
        InvocationBinding::Standalone => normalize_run(
            standalone.context("standalone upgrade requires the exact admitted run options")?,
        )?,
        InvocationBinding::Managed { .. } => {
            ensure!(
                standalone.is_none(),
                "managed upgrade uses the manager's exact invocation"
            );
            let (run, actual) = super::startup_cmd::service_run(home, deadline)?;
            ensure!(
                actual == contract.invocation,
                "managed startup definition differs from the admitted contract"
            );
            run
        }
    };
    ensure!(
        run.config.home == home,
        "upgrade capture HOME differs from the installation"
    );
    capture_for_contract(run, Some(&contract.startup), deadline).await
}

async fn capture_for_contract(
    run: NormalizedRun,
    expected: Option<&ww::upgrade::service::StartupIdentity>,
    deadline: Instant,
) -> Result<PreflightInputs> {
    ensure!(
        run.config.runtime.unsupported_environment.is_empty(),
        "guarded capture rejects proxy, loader, or debug environment settings"
    );
    ensure!(
        Instant::now() < deadline,
        "startup capture deadline expired"
    );
    ensure!(
        !run.config.insecure_ephemeral,
        "startup admission requires persistent identity"
    );
    let namespaces = run.namespaces()?;
    let (key, source) = run.identity()?;
    ensure!(
        source == "file",
        "startup admission requires persistent identity"
    );
    let (owner, resources) = cache::HostFsOwner::start(
        tokio::runtime::Handle::current(),
        cache::HostLimits::default(),
    )?;
    let (scope, scope_owner, finalizer, completion) = resources.try_filesystem_scope()?;
    let client = ww::ipfs::BootClient::one_attempt(
        ww::ipfs::HttpClient::new(run.config.ipfs_url.clone()),
        deadline
            .saturating_duration_since(Instant::now())
            .min(Duration::from_secs(30)),
    );
    let (cancel_tx, mut cancel) = watch::channel(false);
    let capture = async {
        let transaction =
            resources.try_foreground_transaction(resources.try_logical_io()?, &[&scope])?;
        let captured = capture_startup(
            &run,
            &namespaces.configs,
            &namespaces.inputs,
            &key,
            &client,
            &transaction,
            &mut cancel,
        )
        .await?;
        if let Some(expected) = expected {
            ensure!(
                &captured.inputs.startup_identity()? == expected,
                "captured upgrade inputs differ from the admitted startup contract"
            );
        }
        let (inputs, _) = captured.publish(&client, &mut cancel).await?;
        Ok(inputs)
    };
    tokio::pin!(capture);
    let result: Result<PreflightInputs> = tokio::select! {
        result = &mut capture => result,
        _ = tokio::time::sleep_until(deadline.into()) => {
            let _ = cancel_tx.send(true);
            let _ = capture.await;
            Err(anyhow::anyhow!("startup capture deadline expired; acknowledged or uncertain source pins remain retained"))
        },
    };
    drop(scope_owner);
    drop(finalizer);
    let cleanup = completion.wait().await;
    let shutdown = owner.shutdown().await;
    cleanup.context("startup capture filesystem cleanup")?;
    shutdown.context("startup capture filesystem shutdown")?;
    result
}

/// Locks remain held until input comparison. The instance lease then remains
/// held for the full Host lifetime, including startup failures and shutdown.
pub(super) struct GuardedStartup {
    mutation: Option<ww::installation_state::MutationLock>,
    instance: InstanceLease,
    startup: StartupLease,
    contract: StartupContractV1,
    process: PinnedProcess,
    permit: Option<ww::upgrade::StartupPermit>,
}

fn installation_home(executable: &Path) -> Option<&Path> {
    let parent = executable.parent()?;
    let bin = if parent.file_name()? == ".ww-host-upgrade" {
        parent.parent()?
    } else {
        parent
    };
    let root = bin.parent()?;
    ((executable.file_name()? == "ww"
        || executable.file_name()? == "ww (deleted)"
        || parent.file_name()? == ".ww-host-upgrade")
        && bin.file_name()? == "bin"
        && root.file_name()? == ".ww")
        .then(|| root.parent())
        .flatten()
}

fn classify_guarded_home(executable: &Path, argv0: &Path, cwd: &Path) -> Result<Option<PathBuf>> {
    let invocation = if argv0.is_absolute() {
        argv0.to_owned()
    } else {
        cwd.join(argv0)
    };
    let home = installation_home(executable).or_else(|| installation_home(&invocation));
    let Some(home) = home else {
        return Ok(None);
    };
    let installed = home.join(".ww/bin/ww");
    ensure!(
        executable == installed,
        "installed Host launch uses a stale, deleted, reserved, or aliased executable path"
    );
    if let Some(invoked_home) = installation_home(&invocation) {
        ensure!(
            invoked_home == home && invocation == installed,
            "installed Host invocation uses an unsupported executable alias"
        );
    }
    Ok(Some(home.to_owned()))
}

impl GuardedStartup {
    pub fn open(config: &EffectiveRunConfig) -> Result<Option<Self>> {
        let executable = std::env::current_exe().context("identify executing Host path")?;
        let argv0 = std::env::args_os()
            .next()
            .context("missing Host executable invocation")?;
        let Some(home) = classify_guarded_home(&executable, Path::new(&argv0), &config.cwd)? else {
            let installed = config.home.join(".ww/bin/ww");
            if std::fs::symlink_metadata(&installed)
                .is_ok_and(|metadata| metadata.file_type().is_symlink())
            {
                ensure!(
                    std::fs::canonicalize(&installed).ok().as_ref() != Some(&executable),
                    "installed Host symlink aliases are unsupported"
                );
            }
            return Ok(None);
        };
        ensure!(
            home == config.home,
            "installed Host HOME differs from its installation"
        );
        ensure!(
            !config.insecure_ephemeral,
            "guarded startup requires persistent identity"
        );
        ensure!(
            config.runtime.unsupported_environment.is_empty(),
            "guarded startup rejects proxy, loader, or debug environment settings"
        );
        ensure!(
            std::env::var_os("WW_TEST_PID0_RESULT_RACE").is_none(),
            "guarded startup rejects debug PID0 race controls"
        );
        ensure!(
            !std::env::vars_os().any(|(key, _)| key
                .to_str()
                .is_some_and(|name| name.starts_with("LD_") || name.starts_with("DYLD_"))),
            "guarded startup rejects loader injection settings"
        );
        let mutation = match ww::installation_state::MutationLock::acquire(&home) {
            Ok(lock) => Some(lock),
            Err(error)
                if error
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|error| error.kind() == std::io::ErrorKind::WouldBlock) =>
            {
                None
            }
            Err(error) => return Err(error),
        };
        let instance = InstanceLease::shared(&home)?;
        let startup = StartupLease::shared(&home)?;
        let state = startup
            .read_state()?
            .context("required startup state is missing; admit the installed Host")?;
        let contract = startup
            .read_contract()?
            .context("required startup contract is missing")?;
        if let InvocationBinding::Managed { service } = &contract.invocation {
            #[cfg(target_os = "linux")]
            ww::upgrade::native_linux::validate_managed_start(
                &home,
                std::process::id(),
                service,
                Instant::now() + Duration::from_secs(30),
            )?;
            #[cfg(not(target_os = "linux"))]
            {
                let _ = service;
                anyhow::bail!(
                    "managed startup admission requires qualified service-manager observations"
                );
            }
        }
        // Busy mutation exclusion is not launch authority. Only the complete
        // durable owner journal can authorize the transaction child path.
        let permit = if mutation.is_some()
            && matches!(state.disposition, StartupDisposition::Committed { .. })
            && ww::upgrade::validate_steady_start(&startup, &state, &contract).is_ok()
        {
            None
        } else {
            let permit = ww::upgrade::validate_transaction_start(
                &startup,
                &contract.invocation,
                Instant::now() + Duration::from_secs(30),
            )?;
            ensure!(
                permit.contract() == &contract,
                "transaction launch contract changed"
            );
            Some(permit)
        };
        let process = PinnedProcess::capture(
            std::process::id(),
            &contract.artifact,
            Instant::now() + Duration::from_secs(30),
        )?;
        Ok(Some(Self {
            mutation,
            instance,
            startup,
            contract,
            process,
            permit,
        }))
    }

    pub fn admit(self, inputs: &PreflightInputs) -> Result<std::sync::Arc<AdmittedStartup>> {
        ensure!(
            inputs.startup_identity()? == self.contract.startup,
            "captured startup inputs differ from the admitted contract; run startup admit"
        );
        self.process
            .revalidate(Instant::now() + Duration::from_secs(30))?;
        if let Some(expected) = &self.permit {
            let current = ww::upgrade::validate_transaction_start(
                &self.startup,
                &self.contract.invocation,
                Instant::now() + Duration::from_secs(30),
            )?;
            ensure!(
                &current == expected,
                "transaction launch permit changed during source capture"
            );
        }
        if let InvocationBinding::Managed { service } = &self.contract.invocation {
            #[cfg(target_os = "linux")]
            ww::upgrade::native_linux::validate_managed_start(
                self.instance.home(),
                std::process::id(),
                service,
                Instant::now() + Duration::from_secs(30),
            )?;
            #[cfg(not(target_os = "linux"))]
            {
                let _ = service;
                anyhow::bail!(
                    "managed startup admission requires qualified service-manager observations"
                );
            }
        }
        let Self {
            mutation,
            instance,
            startup,
            contract,
            process,
            permit,
        } = self;
        drop(startup);
        drop(mutation);
        Ok(std::sync::Arc::new(AdmittedStartup {
            _instance: instance,
            contract,
            process,
            permit,
            launch_nonce: rand::random(),
        }))
    }
}

pub(super) struct AdmittedStartup {
    _instance: InstanceLease,
    contract: StartupContractV1,
    process: PinnedProcess,
    permit: Option<ww::upgrade::StartupPermit>,
    launch_nonce: [u8; 32],
}

pub(super) struct ReadinessService {
    listener: std::os::unix::net::UnixListener,
    path: PathBuf,
    socket_identity: (u64, u64),
    startup: std::sync::Arc<AdmittedStartup>,
    identity: ww::kernel::IdentityState,
    ready: std::sync::Arc<authority::KernelReadyGate>,
    routes: Option<ww::dispatcher::server::RouteRegistry>,
}

impl ReadinessService {
    pub fn bind(
        startup: std::sync::Arc<AdmittedStartup>,
        identity: ww::kernel::IdentityState,
        ready: std::sync::Arc<authority::KernelReadyGate>,
        routes: Option<ww::dispatcher::server::RouteRegistry>,
    ) -> Result<Self> {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let path = ww::upgrade::inputs::evidence_socket_path(
            startup._instance.home(),
            startup.process.pid(),
            startup.process.start_token(),
        )?;
        let parent = path.parent().context("readiness socket parent")?;
        let metadata = std::fs::symlink_metadata(parent)?;
        // SAFETY: geteuid has no pointer arguments.
        ensure!(
            metadata.is_dir()
                && metadata.uid() == unsafe { libc::geteuid() }
                && metadata.mode() & 0o077 == 0,
            "readiness socket directory is not private and owned"
        );
        // This name includes this exact process birth. An existing entry can
        // only be a conflicting listener or tampering; never unlink it to pass.
        ensure!(
            !path.try_exists()?,
            "readiness socket already exists for this process birth"
        );
        let listener = std::os::unix::net::UnixListener::bind(&path)?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
        listener.set_nonblocking(true)?;
        let socket = std::fs::symlink_metadata(&path)?;
        Ok(Self {
            listener,
            path,
            socket_identity: (socket.dev(), socket.ino()),
            startup,
            identity,
            ready,
            routes,
        })
    }

    async fn respond(&self, mut stream: tokio::net::UnixStream) -> Result<()> {
        use ww::upgrade::inputs::*;
        // Accepted streams are created with CLOEXEC by Tokio/Mio. The
        // independently connected peer is required to have the same owner.
        // SAFETY: geteuid has no pointer arguments.
        ensure!(
            stream.peer_cred()?.uid() == unsafe { libc::geteuid() },
            "readiness peer has another owner"
        );
        let challenge: ReadinessChallenge =
            read_frame(&mut stream, MAX_NATIVE_RESPONSE_BYTES).await?;
        ensure!(
            challenge.version == NATIVE_PROTOCOL_VERSION && challenge.nonce != [0; 32],
            "invalid readiness challenge"
        );
        ensure!(
            challenge
                .launch_nonce
                .is_none_or(|nonce| nonce == self.startup.launch_nonce),
            "readiness launch nonce changed"
        );
        self.startup
            .process
            .revalidate(Instant::now() + Duration::from_secs(2))?;
        let current = self.identity.ready_snapshot(&self.ready, self.routes.as_ref());
        let response = ReadinessResponse {
            version: NATIVE_PROTOCOL_VERSION,
            nonce: challenge.nonce,
            launch_nonce: self.startup.launch_nonce,
            transaction_id: self
                .startup
                .permit
                .as_ref()
                .map(|permit| permit.payload().transaction_id),
            permit_digest: self
                .startup
                .permit
                .as_ref()
                .map(|permit| permit.payload().digest())
                .transpose()?,
            contract: self.startup.contract.identity.clone(),
            artifact: self.startup.contract.artifact.clone(),
            startup: self.startup.contract.startup.clone(),
            serving_ready: current.is_some(),
            epoch_seq: current.as_ref().map(|identity| identity.epoch_seq).unwrap_or(0),
            effective_root: current.as_ref().map(|identity| identity.effective_root.to_string()).unwrap_or_default(),
            kernel_source: current.as_ref().map(|identity| identity.content.source.clone()).unwrap_or_default(),
            kernel_runtime: current.as_ref().map(|identity| identity.content.cid.clone()).unwrap_or_default(),
            kernel_source_cid: current.and_then(|identity| identity.content.source_cid),
        };
        write_frame(&mut stream, &response, MAX_NATIVE_RESPONSE_BYTES).await
    }
}

impl ww::services::Service for ReadinessService {
    fn run(self, mut shutdown: watch::Receiver<()>) -> Result<()> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        runtime.block_on(async {
            let listener = tokio::net::UnixListener::from_std(self.listener.try_clone()?)?;
            loop {
                tokio::select! {
                    _ = shutdown.changed() => return Ok(()),
                    accepted = listener.accept() => {
                        let (stream, _) = accepted?;
                        // A malformed request or a not-ready generation closes
                        // only that connection. Neither condition stops Host.
                        let _ = tokio::time::timeout(Duration::from_secs(2), self.respond(stream)).await;
                    }
                }
            }
        })
    }
}

impl Drop for ReadinessService {
    fn drop(&mut self) {
        use std::os::unix::fs::{FileTypeExt, MetadataExt};
        if std::fs::symlink_metadata(&self.path).is_ok_and(|metadata| {
            metadata.file_type().is_socket()
                && (metadata.dev(), metadata.ino()) == self.socket_identity
        }) {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

pub(super) struct CapturedStartup {
    pub inputs: PreflightInputs,
    mounts: image::CapturedMounts,
}

impl CapturedStartup {
    pub async fn publish(
        self,
        client: &ww::ipfs::BootClient,
        cancel: &mut watch::Receiver<bool>,
    ) -> Result<(PreflightInputs, image::ResolvedMounts)> {
        let resolved = self.mounts.publish_with_cancel(client, cancel).await?;
        let mut inputs = self.inputs;
        inputs.sources = resolved
            .mounts
            .iter()
            .map(|mount| ImmutableSourceInput {
                cid: mount.source().cid().to_string(),
                target: mount.target().to_string(),
                kind: match mount.source().kind() {
                    image::SourceKind::File => SourceKind::File,
                    image::SourceKind::Directory => SourceKind::Directory,
                },
                dag_size: mount.source().dag_size(),
            })
            .collect();
        inputs.validate()?;
        Ok((inputs, resolved))
    }
}

/// Resolve namespace names once and capture all sources without import, pin,
/// publication, cache initialization, listeners or exporters.
pub(super) async fn capture_startup(
    run: &NormalizedRun,
    namespaces: &[ww::ns::NamespaceConfig],
    namespace_inputs: &[NamespaceInput],
    key: &ed25519_dalek::SigningKey,
    client: &ww::ipfs::BootClient,
    transaction: &cache::ForegroundTransaction,
    cancel: &mut watch::Receiver<bool>,
) -> Result<CapturedStartup> {
    ensure!(!*cancel.borrow(), "startup capture cancelled");
    let peer = ww::keys::to_libp2p(key)?.public().to_peer_id();
    let publisher = namespaces
        .iter()
        .find(|config| config.name == "ww")
        .filter(|config| ww::ipns::parse_name(&config.ipns).ok() == Some(peer))
        .map(|config| config.bootstrap_ipfs_path())
        .transpose()?
        .flatten()
        .map(|bootstrap| PublisherInput {
            name: peer.to_string(),
            bootstrap,
            routing_url: run.config.ipns_routing_url.clone(),
            state_directory: run.config.home.join(".ww"),
        });
    let status = ww::metrics::RuntimeStatus::starting();
    let resolution = async {
        let mut resolved = Vec::new();
        for config in namespaces {
            config.validate()?;
        }
        for config in namespaces {
            if config.name == "ww" && publisher.is_some() {
                // This signer publishes after admission. Require its current
                // name to select the configured bootstrap without a fallback;
                // startup must not publish merely to make a check pass.
                let path = client
                    .name_resolve(&format!("/ipns/{}", config.ipns))
                    .await
                    .context(
                        "host-owned ww namespace must already resolve before startup admission",
                    )?;
                let cid = ww::ipfs::cid_identity::parse_cid(
                    path.strip_prefix("/ipfs/")
                        .context("host-owned namespace did not resolve to IPFS")?,
                )?;
                resolved.push((config.name.clone(), format!("/ipfs/{cid}")));
            } else {
                resolved.extend(
                    ww::ns::resolve_namespaces(std::slice::from_ref(config), client, &status)
                        .await?,
                );
            }
        }
        Ok::<_, anyhow::Error>(resolved)
    };
    let resolved = tokio::select! {
        biased;
        _ = cancel.changed() => anyhow::bail!("startup capture cancelled"),
        resolved = resolution => resolved?,
    };
    for config in namespaces {
        ensure!(
            config.ipfs_path()?.is_none() || resolved.iter().any(|(name, _)| name == &config.name),
            "namespace {} is unavailable; startup capture cannot admit a partial image",
            config.name
        );
    }
    if let Some(publisher) = &publisher {
        ensure!(resolved.iter().any(|(name, path)| name == "ww" && path == &publisher.bootstrap),
            "host-owned ww namespace does not resolve to its configured bootstrap; startup capture cannot publish to make admission pass");
    }
    let mut mounts: Vec<_> = run.baseline.clone().into_iter().collect();
    mounts.extend(resolved.into_iter().map(|(_, source)| Mount {
        source,
        target: image::GuestTarget::root(),
    }));
    mounts.extend(run.mounts.clone());
    let mounts = image::capture_frozen_mounts_with_cancel(
        &mounts,
        client,
        transaction,
        run.authoritative(),
        cancel,
    )
    .await?;
    let inputs = PreflightInputs {
        version: 1,
        config: run.config.clone(),
        namespaces: namespace_inputs.to_vec(),
        identity_fingerprint: Sha256::digest(key.verifying_key().as_bytes()).into(),
        operation_digests: mounts.operation_digests().to_vec(),
        has_baseline: run.baseline.is_some(),
        sources: Vec::new(),
        publisher,
    };
    Ok(CapturedStartup { inputs, mounts })
}

async fn read_frame<T: serde::de::DeserializeOwned>(
    stream: &mut tokio::net::UnixStream,
    maximum: usize,
) -> Result<T> {
    use tokio::io::AsyncReadExt;
    let length = stream.read_u32().await? as usize;
    ensure!(
        length > 0 && length <= maximum,
        "native protocol frame exceeds bound"
    );
    let mut bytes = vec![0; length];
    stream.read_exact(&mut bytes).await?;
    serde_json::from_slice(&bytes).context("invalid native protocol frame")
}

async fn write_frame(
    stream: &mut tokio::net::UnixStream,
    value: &impl serde::Serialize,
    maximum: usize,
) -> Result<()> {
    use tokio::io::AsyncWriteExt;
    let bytes = serde_json::to_vec(value)?;
    ensure!(
        bytes.len() <= maximum,
        "native protocol response exceeds bound"
    );
    stream.write_u32(bytes.len() as u32).await?;
    stream.write_all(&bytes).await?;
    stream.flush().await?;
    Ok(())
}

fn inherited_private_stream(fd: i32) -> Result<tokio::net::UnixStream> {
    use std::os::fd::{AsRawFd, FromRawFd};
    ensure!(
        fd >= 3,
        "native protocol requires a private inherited descriptor"
    );
    #[cfg(target_os = "linux")]
    {
        let mut count = 0usize;
        for entry in std::fs::read_dir("/proc/self/fd")? {
            count += 1;
            ensure!(
                count <= 4096,
                "native preflight inherited descriptor bound exceeded"
            );
            let entry = entry?;
            if let Some(open) = entry
                .file_name()
                .to_str()
                .and_then(|name| name.parse::<i32>().ok())
                .filter(|open| *open >= 3)
            {
                // SAFETY: fcntl accepts descriptor integers and changes only
                // per-descriptor exec flags. EBADF can race directory closure.
                let flags = unsafe { libc::fcntl(open, libc::F_GETFD) };
                if flags >= 0 {
                    ensure!(
                        unsafe { libc::fcntl(open, libc::F_SETFD, flags | libc::FD_CLOEXEC) } == 0,
                        "seal inherited preflight descriptor"
                    );
                }
            }
        }
    }
    // SAFETY: fcntl only inspects and updates the supplied descriptor's flags.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    ensure!(flags >= 0, "native protocol descriptor is unavailable");
    // SAFETY: flags came from this descriptor and CLOEXEC preserves other flags.
    ensure!(
        unsafe { libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) } == 0,
        "set native protocol close-on-exec"
    );
    // SAFETY: the explicit protocol argument transfers this inherited FD to us.
    let stream = unsafe { std::os::unix::net::UnixStream::from_raw_fd(fd) };
    let mut kind: libc::c_int = 0;
    let mut size = std::mem::size_of_val(&kind) as libc::socklen_t;
    // SAFETY: kind and size are live, correctly sized output buffers.
    ensure!(
        unsafe {
            libc::getsockopt(
                stream.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_TYPE,
                (&mut kind as *mut libc::c_int).cast(),
                &mut size,
            )
        } == 0
            && kind == libc::SOCK_STREAM,
        "native protocol descriptor is not a stream socket"
    );
    // UnixStream::local_addr checks the address family returned by getsockname.
    stream
        .local_addr()
        .context("native protocol requires a Unix socket")?;
    stream.set_nonblocking(true)?;
    let stream = tokio::net::UnixStream::from_std(stream)?;
    // SAFETY: geteuid has no pointer arguments.
    ensure!(
        stream.peer_cred()?.uid() == unsafe { libc::geteuid() },
        "native protocol peer has another owner"
    );
    Ok(stream)
}

/// A private preflight child performs the production static preparation path.
/// The child neither starts guests nor claims independent release authority.
pub(super) async fn native_preflight(fd: i32) -> Result<()> {
    use tokio::io::AsyncReadExt;
    use ww::upgrade::inputs::*;
    let mut stream = inherited_private_stream(fd)?;
    let request: PreflightRequest = tokio::time::timeout(
        Duration::from_secs(5),
        read_frame(&mut stream, MAX_PREFLIGHT_REQUEST_BYTES),
    )
    .await
    .context("preflight request deadline")??;
    ensure!(
        request.version == NATIVE_PROTOCOL_VERSION && request.nonce != [0; 32],
        "unsupported preflight request"
    );
    ensure!(
        (1..=300_000).contains(&request.timeout_millis),
        "invalid preflight deadline"
    );
    let deadline = Instant::now() + Duration::from_millis(request.timeout_millis);
    let process = PinnedProcess::capture(std::process::id(), &request.artifact, deadline)?;
    let (capture_digest, prepared) = if let Some(inputs) = &request.inputs {
        inputs.validate()?;
        let prepared =
            tokio::time::timeout_at(deadline.into(), prepare_preflight(inputs, deadline))
                .await
                .context("preflight preparation deadline expired")??;
        (Some(inputs.capture_digest()?), Some(prepared))
    } else {
        (None, None)
    };
    process.revalidate(deadline)?;
    let response = PreflightResponse {
        version: NATIVE_PROTOCOL_VERSION,
        nonce: request.nonce,
        artifact: request.artifact,
        guard_capable: true,
        capture_digest,
        prepared,
    };
    tokio::time::timeout_at(deadline.into(), async {
        write_frame(&mut stream, &response, MAX_NATIVE_RESPONSE_BYTES).await?;
        // Keep the exact child alive while the parent independently verifies
        // the held executable and process evidence. EOF is never success.
        ensure!(
            stream.read_u8().await? == 0x06,
            "invalid preflight acknowledgement"
        );
        Ok::<_, anyhow::Error>(())
    })
    .await
    .context("preflight acknowledgement deadline expired")??;
    Ok(())
}

async fn prepare_preflight(
    inputs: &PreflightInputs,
    deadline: Instant,
) -> Result<ww::upgrade::inputs::PreparedHead> {
    use ww::stem::Source as _;
    let (owner, resources) = cache::HostFsOwner::start(
        tokio::runtime::Handle::current(),
        cache::HostLimits::default(),
    )?;
    let (scope, scope_owner, finalizer, completion) = resources.try_filesystem_scope()?;
    let client = ww::ipfs::BootClient::one_attempt(
        ww::ipfs::HttpClient::new(inputs.config.ipfs_url.clone()),
        deadline
            .saturating_duration_since(Instant::now())
            .min(Duration::from_secs(30)),
    );
    let (_cancel_tx, mut cancel) = watch::channel(false);
    let result: Result<_> = async {
        let head = if let Some(stem) = &inputs.config.stem {
            let mut source = ww::stem::atom::Source::new(ww::stem::atom::Config::new(
                inputs.config.rpc_url.clone(),
                super::parse_contract_address(stem)?,
                inputs.config.confirmation_depth,
            ))?;
            Some(source.current().await?)
        } else if let Some(name) = &inputs.config.ipns_stem {
            Some(
                ww::stem::ipns::Source::snapshot(ww::stem::ipns::Config::new(
                    ww::ipns::parse_name(name)?,
                    inputs.config.ipns_routing_url.clone(),
                    inputs.config.home.join(".ww"),
                ))
                .await?,
            )
        } else {
            None
        };
        let head = match head {
            Some(ww::stem::Update::Head(head)) => Some(head.cid),
            Some(ww::stem::Update::InvalidHead(_)) => {
                anyhow::bail!("preflight authoritative head is invalid")
            }
            None => None,
        };
        let mounts = inputs
            .sources
            .iter()
            .map(|source| {
                Ok(Mount {
                    source: format!("/ipfs/{}", source.cid),
                    target: image::GuestTarget::new(&source.target)?,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let transaction =
            resources.try_foreground_transaction(resources.try_logical_io()?, &[&scope])?;
        let captured = image::capture_frozen_mounts_with_cancel(
            &mounts,
            &client,
            &transaction,
            head.is_some(),
            &mut cancel,
        )
        .await?;
        let resolved = captured.publish_with_cancel(&client, &mut cancel).await?;
        for (actual, expected) in resolved.mounts.iter().zip(&inputs.sources) {
            let kind_matches = matches!(
                (actual.source().kind(), expected.kind),
                (image::SourceKind::File, SourceKind::File)
                    | (image::SourceKind::Directory, SourceKind::Directory)
            );
            ensure!(
                actual.source().cid().to_string() == expected.cid
                    && actual.source().dag_size() == expected.dag_size
                    && kind_matches,
                "preflight immutable source inspection differs from captured transport"
            );
        }
        let mut frozen = resolved.mounts;
        let baseline = inputs.has_baseline.then(|| frozen.remove(0));
        if let Some(head) = head {
            client.pin_add_once(&head.to_string()).await?;
        }
        let effective =
            ww::image::compose_root(head, baseline.as_ref(), &frozen, &client, &mut cancel).await?;
        ww::cell::engine::initialize_captured_cache(
            inputs.config.runtime.compilation_cache.clone(),
        )?;
        let (runtime, _clock) = ww::cell::engine::runtime_engine()?;
        let kernel = ww::image::prepare_kernel(
            effective,
            &client,
            runtime,
            &ww::kernel::Compilation::default(),
            &mut cancel,
        )
        .await?;
        let identity = kernel.identity();
        Ok(ww::upgrade::inputs::PreparedHead {
            effective_root: effective.to_string(),
            kernel_source: identity.source,
            kernel_runtime: kernel.runtime_cid().to_string(),
            kernel_source_cid: kernel.source_file().to_string(),
            kernel_wasm_blake3: identity.wasm_blake3,
            kernel_size: identity.size as u64,
            authoritative_head: head.map(|head| head.to_string()),
        })
    }
    .await;
    drop(scope_owner);
    drop(finalizer);
    completion
        .wait()
        .await
        .context("preflight filesystem cleanup")?;
    owner
        .shutdown()
        .await
        .context("preflight filesystem shutdown")?;
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn private_protocol_rejects_oversize_and_unknown_fields() {
        use tokio::io::AsyncWriteExt;
        use ww::upgrade::inputs::ReadinessChallenge;
        let (mut writer, mut reader) = tokio::net::UnixStream::pair().unwrap();
        writer.write_u32(1025).await.unwrap();
        assert!(read_frame::<ReadinessChallenge>(&mut reader, 1024)
            .await
            .unwrap_err()
            .to_string()
            .contains("bound"));
        let (mut writer, mut reader) = tokio::net::UnixStream::pair().unwrap();
        let value = serde_json::json!({ "version":1, "nonce":vec![1;32], "launch_nonce":null, "unexpected":true });
        write_frame(&mut writer, &value, 1024).await.unwrap();
        assert!(read_frame::<ReadinessChallenge>(&mut reader, 1024)
            .await
            .is_err());
        let (mut writer, mut reader) = tokio::net::UnixStream::pair().unwrap();
        let challenge = ReadinessChallenge {
            version: 1,
            nonce: [7; 32],
            launch_nonce: Some([9; 32]),
        };
        write_frame(&mut writer, &challenge, 1024).await.unwrap();
        let decoded: ReadinessChallenge = read_frame(&mut reader, 1024).await.unwrap();
        assert_eq!(decoded.nonce, challenge.nonce);
        assert_eq!(decoded.launch_nonce, challenge.launch_nonce);
    }

    fn service_run(
        root: &Path,
        mut environment: BTreeMap<String, String>,
        extra: &[&str],
    ) -> NormalizedRun {
        environment
            .entry("WW_EXECUTOR_THREADS".into())
            .or_insert("1".into());
        environment
            .entry("WW_COMPILE_WORKERS".into())
            .or_insert("1".into());
        let canonical = root.canonicalize().unwrap();
        let root = canonical.as_path();
        std::fs::create_dir_all(root.join(".ww/fhs")).unwrap();
        std::fs::create_dir_all(root.join("sources")).unwrap();
        let mut argv = vec![
            root.join(".ww/bin/ww").to_str().unwrap().to_owned(),
            "run".into(),
            "./sources".into(),
        ];
        argv.extend(extra.iter().map(|value| value.to_string()));
        let context =
            InvocationContext::service(root.to_owned(), root.to_owned(), 1000, &environment)
                .unwrap();
        normalize_service_run(&argv, &environment, context).unwrap()
    }

    #[test]
    fn service_normalization_uses_observed_environment_and_explicit_cli_values() {
        let root = tempfile::tempdir().unwrap();
        let environment = BTreeMap::from([
            ("WW_EXECUTOR_THREADS".into(), "7".into()),
            ("WW_COMPILE_WORKERS".into(), "3".into()),
            ("WW_HTTP_ADMIN".into(), "off".into()),
            ("WW_CWASM_DIR".into(), "cache".into()),
            ("IPFS_API".into(), "http://127.0.0.1:5999".into()),
        ]);
        let run = service_run(root.path(), environment, &["--executor-threads", "2"]);
        assert_eq!(run.config.executor_threads, 2);
        assert_eq!(run.config.runtime.compile_workers, 3);
        assert_eq!(run.config.ipfs_url, "http://127.0.0.1:5999");
        assert!(run.config.http_admin.is_none());
        assert!(!run.config.runtime.interactive);
        assert_eq!(
            run.mounts[0].source,
            root.path()
                .canonicalize()
                .unwrap()
                .join("sources")
                .to_str()
                .unwrap()
        );
        assert_eq!(
            run.config.identity_path,
            Some(root.path().canonicalize().unwrap().join(".ww/identity"))
        );
        run.config.runtime.compilation_cache.validate().unwrap();
    }

    #[test]
    fn managed_layout_classification_does_not_depend_on_selected_inputs() {
        assert_eq!(
            installation_home(Path::new("/home/operator/.ww/bin/ww")),
            Some(Path::new("/home/operator"))
        );
        assert_eq!(installation_home(Path::new("/tmp/ww")), None);
        for stale in [
            "/home/operator/.ww/bin/ww (deleted)",
            "/home/operator/.ww/bin/.ww-host-upgrade/previous",
        ] {
            assert_eq!(
                installation_home(Path::new(stale)),
                Some(Path::new("/home/operator"))
            );
            assert!(
                classify_guarded_home(Path::new(stale), Path::new("ww"), Path::new("/tmp"))
                    .is_err()
            );
        }
        assert!(classify_guarded_home(
            Path::new("/tmp/ww"),
            Path::new("/home/operator/.ww/bin/ww"),
            Path::new("/tmp")
        )
        .is_err());
        let error = InvocationContext::service(
            PathBuf::from("/home/operator"),
            PathBuf::from("/home/operator"),
            1000,
            &BTreeMap::from([("LD_PRELOAD".into(), "/tmp/inject.so".into())]),
        )
        .err()
        .unwrap();
        assert!(error.to_string().contains("loader"));
        let error = InvocationContext::service(
            PathBuf::from("/home/operator"),
            PathBuf::from("/home/operator"),
            1000,
            &BTreeMap::from([
                ("WW_COMPILE_WORKERS".into(), "1".into()),
                (
                    "HTTPS_PROXY".into(),
                    "http://private:credential@proxy.invalid".into(),
                ),
            ]),
        )
        .err()
        .unwrap();
        assert!(error.to_string().contains("proxy"));
        assert!(!error.to_string().contains("credential"));
        let error = InvocationContext::service(
            PathBuf::from("/home/operator"),
            PathBuf::from("/home/operator"),
            1000,
            &BTreeMap::new(),
        )
        .err()
        .unwrap();
        assert!(error.to_string().contains("WW_COMPILE_WORKERS"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn replaced_installed_process_path_never_becomes_unmanaged() {
        let root = tempfile::tempdir().unwrap();
        let bin = root.path().join(".ww/bin");
        std::fs::create_dir_all(&bin).unwrap();
        let installed = bin.join("ww");
        let replacement = bin.join("replacement");
        std::fs::copy("/bin/sleep", &installed).unwrap();
        std::fs::copy("/bin/sleep", &replacement).unwrap();
        let mut child = std::process::Command::new(&installed)
            .arg("10")
            .spawn()
            .unwrap();
        std::fs::rename(replacement, &installed).unwrap();
        let observed = std::fs::read_link(format!("/proc/{}/exe", child.id())).unwrap();
        let result = classify_guarded_home(&observed, &installed, root.path());
        let _ = child.kill();
        child.wait().unwrap();
        assert!(observed
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .ends_with(" (deleted)"));
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn publisher_mismatch_refuses_after_one_read_without_pin_or_publication() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let root = tempfile::tempdir().unwrap();
        let run = service_run(root.path(), BTreeMap::new(), &[]);
        let key = ed25519_dalek::SigningKey::from_bytes(&[7; 32]);
        let peer = ww::keys::to_libp2p(&key).unwrap().public().to_peer_id();
        let expected = ww::kernel::runtime_cid(b"configured bootstrap");
        let actual = ww::kernel::runtime_cid(b"different current publication");
        let namespace = ww::ns::NamespaceConfig {
            name: "ww".into(),
            ipns: peer.to_string(),
            bootstrap: format!("/ipfs/{expected}"),
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = vec![0; 8192];
            let count = stream.read(&mut request).await.unwrap();
            let body = serde_json::json!({"Path":format!("/ipfs/{actual}")}).to_string();
            stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
            String::from_utf8(request[..count].to_vec()).unwrap()
        });
        let (owner, resources) = cache::HostFsOwner::start(
            tokio::runtime::Handle::current(),
            cache::HostLimits::default(),
        )
        .unwrap();
        let (scope, scope_owner, finalizer, completion) = resources.try_filesystem_scope().unwrap();
        let transaction = resources
            .try_foreground_transaction(resources.try_logical_io().unwrap(), &[&scope])
            .unwrap();
        let client = ww::ipfs::BootClient::one_attempt(
            ww::ipfs::HttpClient::new(format!("http://{address}")),
            Duration::from_secs(2),
        );
        let (_tx, mut cancel) = watch::channel(false);
        let error = capture_startup(
            &run,
            &[namespace],
            &[],
            &key,
            &client,
            &transaction,
            &mut cancel,
        )
        .await
        .err()
        .unwrap();
        assert!(error.to_string().contains("does not resolve"), "{error:#}");
        let request = server.await.unwrap();
        assert!(
            request.starts_with("POST /api/v0/name/resolve?"),
            "{request}"
        );
        assert!(!request.contains("pin/add") && !request.contains("name/publish"));
        drop(transaction);
        drop(scope_owner);
        drop(finalizer);
        completion.wait().await.unwrap();
        owner.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn local_capture_needs_no_kubo_effects_and_binds_bytes_and_runtime() {
        let root = tempfile::tempdir().unwrap();
        let run = service_run(root.path(), BTreeMap::new(), &[]);
        let payload = root.path().join("sources/payload");
        std::fs::write(&payload, b"first capture").unwrap();
        let key = ed25519_dalek::SigningKey::from_bytes(&[7; 32]);
        let (owner, resources) = cache::HostFsOwner::start(
            tokio::runtime::Handle::current(),
            cache::HostLimits::default(),
        )
        .unwrap();
        let (scope, scope_owner, finalizer, completion) = resources.try_filesystem_scope().unwrap();
        let transaction = resources
            .try_foreground_transaction(resources.try_logical_io().unwrap(), &[&scope])
            .unwrap();
        // An unreachable endpoint makes any unexpected Kubo operation fail.
        let client = ww::ipfs::BootClient::one_attempt(
            ww::ipfs::HttpClient::new("http://127.0.0.1:1".into()),
            Duration::from_millis(50),
        );
        let (_tx, mut cancel) = watch::channel(false);
        let first = capture_startup(&run, &[], &[], &key, &client, &transaction, &mut cancel)
            .await
            .unwrap();
        let first_identity = first.inputs.startup_identity().unwrap();
        let mut changed_config = first.inputs.clone();
        changed_config.config.runtime.compile_workers += 1;
        let changed_identity = changed_config.startup_identity().unwrap();
        assert_ne!(changed_identity.configuration, first_identity.configuration);
        assert_eq!(
            changed_identity.frozen_sources,
            first_identity.frozen_sources
        );
        std::fs::write(&payload, b"replacement after capture").unwrap();
        let second = capture_startup(&run, &[], &[], &key, &client, &transaction, &mut cancel)
            .await
            .unwrap();
        assert_ne!(
            first_identity.frozen_sources,
            second.inputs.startup_identity().unwrap().frozen_sources
        );
        assert_eq!(first_identity, first.inputs.startup_identity().unwrap());
        assert_eq!(
            first_identity.fhs_baseline,
            second.inputs.startup_identity().unwrap().fhs_baseline
        );
        assert!(first.inputs.sources.is_empty());
        assert!(first.inputs.validate().is_err());
        drop(first);
        drop(second);
        drop(transaction);
        drop(scope_owner);
        drop(finalizer);
        completion.wait().await.unwrap();
        owner.shutdown().await.unwrap();
    }

    #[test]
    fn configuration_debug_redacts_urls_and_serialization_errors_refuse() {
        use std::os::unix::ffi::OsStringExt;
        let root = tempfile::tempdir().unwrap();
        let mut run = service_run(root.path(), BTreeMap::new(), &[]);
        run.config.rpc_url = "https://token:secret@example.invalid".into();
        assert!(!format!("{:?}", run.config).contains("secret"));
        let mut inputs = PreflightInputs {
            version: 1,
            config: run.config,
            namespaces: vec![],
            identity_fingerprint: [1; 32],
            operation_digests: vec![],
            has_baseline: false,
            sources: vec![],
            publisher: None,
        };
        assert!(!format!("{inputs:?}").contains("secret"));
        inputs.config.cwd = PathBuf::from(std::ffi::OsString::from_vec(vec![b'/', 0xff]));
        assert!(inputs.capture_digest().is_err());
    }
}
