//! Host-owned deployment lifecycle.
//!
//! Deployment allocates local epochs, revokes authority, prepares roots,
//! coordinates kernel teardown, activates `CidTree`, and launches one
//! [`crate::kernel::Generation`] at a time. The CLI retains process and
//! operator policy.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use authority::{Epoch, EpochGuard};
use cid::Cid;
use rand::Rng;
use tokio::sync::{mpsc, oneshot, watch};
use tracing::{info, warn};

use crate::cell::image::{CompositionError, FrozenMount};
use crate::cell::vfs::CidTree;
use crate::image::{bounded, OperationTimeout, PermanentPreparationError, OPERATION_TIMEOUT};
use crate::ipfs::cid_identity::decode_cid;
use crate::kernel;
use crate::services::{ExecutorPool, SpawnRequest};
use crate::stem::{Head, InvalidHead, Source, Update};
use cell::engine::RuntimeEngine;

mod retained_pinner;
pub use retained_pinner::RetainedPinner;

const RETRY_BASE_DELAY: Duration = Duration::from_secs(1);
const RETRY_MAX_DELAY: Duration = Duration::from_secs(60);
const RETRY_JITTER_MAX: Duration = Duration::from_millis(500);
const SOURCE_QUEUE_CAPACITY: usize = 16;
const KERNEL_TEARDOWN_TIMEOUT: Duration = Duration::from_secs(30);
const SPECULATIVE_RETENTION: Duration = Duration::from_secs(5 * 60);
const MAX_PIN_RELEASES: usize = 128;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FailureClass {
    Transient,
    Permanent,
    Unknown,
}

fn classify_failure(error: &anyhow::Error) -> FailureClass {
    if let Some(selection) = error.downcast_ref::<cell::image::KernelFileError>() {
        return match selection.kind() {
            cell::image::KernelFileFailure::MissingPath
            | cell::image::KernelFileFailure::InvalidSource => FailureClass::Permanent,
            cell::image::KernelFileFailure::Unavailable => FailureClass::Transient,
            cell::image::KernelFileFailure::Cancelled | cell::image::KernelFileFailure::Unknown => {
                FailureClass::Unknown
            }
        };
    }
    if let Some(validation) = error.downcast_ref::<cell::proc::KernelValidationError>() {
        return match validation.kind() {
            cell::proc::KernelValidationFailure::SizeLimit
            | cell::proc::KernelValidationFailure::InvalidComponent
            | cell::proc::KernelValidationFailure::Compatibility => FailureClass::Permanent,
            cell::proc::KernelValidationFailure::Compilation => FailureClass::Unknown,
        };
    }
    if let Some(admission) = error
        .chain()
        .find_map(|cause| cause.downcast_ref::<cache::AdmissionError>())
    {
        return match admission.kind() {
            cache::AdmissionKind::Closed | cache::AdmissionKind::CleanupPoisoned => {
                FailureClass::Permanent
            }
            _ => FailureClass::Transient,
        };
    }
    if error
        .chain()
        .any(|cause| cause.is::<PermanentPreparationError>() || cause.is::<CompositionError>())
    {
        FailureClass::Permanent
    } else if error.chain().any(|cause| {
        cause
            .downcast_ref::<reqwest::Error>()
            .is_some_and(|error| error.is_connect() || error.is_timeout() || error.is_request())
            || cause
                .downcast_ref::<crate::ipfs::KuboApiError>()
                .is_some_and(crate::ipfs::KuboApiError::is_server_error)
            || cause.is::<OperationTimeout>()
            || cause.is::<crate::ipfs::KuboOperationTimeout>()
    }) {
        FailureClass::Transient
    } else {
        FailureClass::Unknown
    }
}

fn retry_delay(current: &mut Duration) -> Duration {
    let jitter = rand::rng().random_range(0..RETRY_JITTER_MAX.as_millis() as u64);
    let delay = *current + Duration::from_millis(jitter);
    *current = (*current * 2).min(RETRY_MAX_DELAY);
    delay
}

#[derive(Debug, Default)]
struct PinSet {
    cids: Vec<Cid>,
}

impl PinSet {
    fn insert(&mut self, cid: Cid) {
        if !self.cids.contains(&cid) {
            self.cids.push(cid);
        }
    }

    fn contains(&self, cid: &Cid) -> bool {
        self.cids.contains(cid)
    }

    fn is_empty(&self) -> bool {
        self.cids.is_empty()
    }
}

/// Immutable deployment content with pin ownership but no authority or epoch binding.
#[derive(Debug)]
pub struct PreparedRoot {
    head: Option<Head>,
    effective: Cid,
    kernel: kernel::PreparedKernel,
    pins: PinSet,
}

impl PreparedRoot {
    pub fn effective(&self) -> &Cid {
        &self.effective
    }

    fn head_bytes(&self) -> Vec<u8> {
        self.head.as_ref().map_or_else(Vec::new, Head::bytes)
    }
}

/// Bootstrap keeps the cache tree absent until composition yields a valid root.
enum Prewarm<'a> {
    Existing(&'a Arc<CidTree>),
    Bootstrap {
        tree: &'a mut Option<Arc<CidTree>>,
        staging_dir: &'a Path,
        filesystem_host: &'a Arc<cache::HostResources>,
        filesystem_scope: &'a cache::FsScope,
    },
}

#[allow(clippy::too_many_arguments)]
async fn prepare_root(
    head: Option<Head>,
    installed_baseline: Option<&FrozenMount>,
    runtime_engine: &RuntimeEngine,
    compilation: &kernel::Compilation,
    frozen_mounts: &[FrozenMount],
    ipfs_client: &crate::ipfs::HttpClient,
    prewarm: Prewarm<'_>,
    pins: &mut PinSet,
    cancel: &mut watch::Receiver<bool>,
) -> Result<PreparedRoot> {
    if *cancel.borrow() {
        anyhow::bail!("deployment preparation cancelled");
    }
    if let Some(head) = &head {
        let cid = head.cid;
        if !pins.contains(&cid) {
            bounded("head pin", ipfs_client.pin_add(&cid.to_string()))
                .await
                .context("pinning deployment head")?;
            pins.insert(cid);
        }
    }
    if *cancel.borrow() {
        anyhow::bail!("deployment preparation cancelled");
    }
    let boot_client = crate::ipfs::BootClient::one_attempt(ipfs_client.clone(), OPERATION_TIMEOUT);
    let effective = crate::image::compose_root(
        head.as_ref().map(|head| head.cid),
        installed_baseline,
        frozen_mounts,
        &boot_client,
        cancel,
    )
    .await?;
    pins.insert(effective);

    if *cancel.borrow() {
        anyhow::bail!("deployment preparation cancelled");
    }

    let tree = match prewarm {
        Prewarm::Existing(tree) => tree,
        Prewarm::Bootstrap {
            tree,
            staging_dir,
            filesystem_host,
            filesystem_scope,
        } => tree.get_or_insert_with(|| {
            Arc::new(CidTree::new(
                effective,
                ipfs_client.clone(),
                staging_dir.to_owned(),
                Arc::clone(filesystem_host),
                filesystem_scope.clone(),
            ))
        }),
    };
    bounded("effective-root prewarm", tree.pre_warm(&effective))
        .await
        .context("pre-warming effective deployment root")?;
    let kernel = crate::image::prepare_kernel(
        effective,
        &boot_client,
        runtime_engine.clone(),
        compilation,
        cancel,
    )
    .await?;
    Ok(PreparedRoot {
        head,
        effective,
        kernel,
        pins: std::mem::take(pins),
    })
}

struct PreparationOutput {
    result: Result<PreparedRoot>,
    pins: PinSet,
}

#[allow(clippy::too_many_arguments)]
async fn prepare_root_owned(
    head: Head,
    installed_baseline: Option<FrozenMount>,
    runtime_engine: RuntimeEngine,
    compilation: kernel::Compilation,
    frozen_mounts: Vec<FrozenMount>,
    ipfs_client: crate::ipfs::HttpClient,
    cid_tree: Arc<CidTree>,
    mut pins: PinSet,
    mut cancel: watch::Receiver<bool>,
) -> PreparationOutput {
    let result = prepare_root(
        Some(head),
        installed_baseline.as_ref(),
        &runtime_engine,
        &compilation,
        &frozen_mounts,
        &ipfs_client,
        Prewarm::Existing(&cid_tree),
        &mut pins,
        &mut cancel,
    )
    .await;
    PreparationOutput { result, pins }
}

#[derive(Clone)]
struct Candidate {
    head: Head,
    expires_at: tokio::time::Instant,
}

struct SpeculativeTask {
    candidate: Candidate,
    cancel: watch::Sender<bool>,
    task: tokio::task::JoinHandle<PreparationOutput>,
}

enum Speculation {
    InFlight(SpeculativeTask),
    Ready {
        prepared: PreparedRoot,
        expires_at: tokio::time::Instant,
    },
    Stopping {
        task: SpeculativeTask,
        next: Option<Candidate>,
    },
    Releasing {
        task: tokio::task::JoinHandle<PinSet>,
        next: Option<Candidate>,
    },
}

enum SourceMessage {
    Update(Update),
    Error(anyhow::Error),
}

fn spawn_source_follower(
    mut source: Box<dyn Source>,
) -> (mpsc::Receiver<SourceMessage>, tokio::task::JoinHandle<()>) {
    let (tx, rx) = mpsc::channel(SOURCE_QUEUE_CAPACITY);
    let task = tokio::spawn(async move {
        let mut delay = RETRY_BASE_DELAY;
        loop {
            match source.next().await {
                Ok(update) => {
                    delay = RETRY_BASE_DELAY;
                    if tx.send(SourceMessage::Update(update)).await.is_err() {
                        return;
                    }
                }
                Err(error) => {
                    if tx.send(SourceMessage::Error(error)).await.is_err() {
                        return;
                    }
                    tokio::time::sleep(retry_delay(&mut delay)).await;
                }
            }
        }
    });
    (rx, task)
}

async fn current_with_retry(source: &mut dyn Source) -> Update {
    let mut delay = RETRY_BASE_DELAY;
    loop {
        match source.current().await {
            Ok(update) => return update,
            Err(error) => {
                let retry = retry_delay(&mut delay);
                warn!(
                    retry_ms = retry.as_millis() as u64,
                    "Stem current-state query failed: {error:#}"
                );
                tokio::time::sleep(retry).await;
            }
        }
    }
}

async fn next_source_update(source_rx: &mut mpsc::Receiver<SourceMessage>) -> Result<Update> {
    loop {
        match source_rx
            .recv()
            .await
            .context("Stem source follower stopped")?
        {
            SourceMessage::Update(update) => return Ok(update),
            SourceMessage::Error(error) => {
                warn!("Stem source unavailable; deployment authority unchanged: {error:#}");
            }
        }
    }
}

/// Inputs used to establish deployment epoch zero.
pub struct Config {
    /// Frozen installed FHS root, below Stem, namespaces, and CLI operations.
    pub installed_baseline: Option<FrozenMount>,
    /// The same engine used by the ExecutorPool and every generation.
    pub runtime_engine: RuntimeEngine,
    pub source: Option<Box<dyn Source>>,
    /// Latest observed Atom `HeadUpdated` CID bytes. These values are advisory only.
    pub candidates: Option<watch::Receiver<Option<Vec<u8>>>>,
    /// Ordered, inspected sources retained before deployment preparation.
    pub frozen_mounts: Vec<FrozenMount>,
    /// Acknowledged source pins, including immutable subpath parent roots.
    /// These are shared native pins, not independent reference-counted claims.
    pub retained_source_roots: Vec<Cid>,
    pub ipfs_client: crate::ipfs::HttpClient,
    pub staging_dir: PathBuf,
    pub filesystem_host: Arc<cache::HostResources>,
    pub filesystem_scope: cache::FsScope,
}

/// Inputs shared by all kernel generations in one host process.
pub struct KernelLauncher<'a> {
    executor_pool: &'a ExecutorPool,
    identity_state: kernel::IdentityState,
    readiness_gate: Arc<authority::KernelReadyGate>,
    bootstrap: kernel::Bootstrap,
    runtime_inputs: kernel::RuntimeInputs,
    stdio: kernel::Stdio,
}

impl<'a> KernelLauncher<'a> {
    pub fn new(
        executor_pool: &'a ExecutorPool,
        identity_state: kernel::IdentityState,
        readiness_gate: Arc<authority::KernelReadyGate>,
        bootstrap: kernel::Bootstrap,
        runtime_inputs: kernel::RuntimeInputs,
        stdio: kernel::Stdio,
    ) -> Self {
        Self {
            executor_pool,
            identity_state,
            readiness_gate,
            bootstrap,
            runtime_inputs,
            stdio,
        }
    }

    fn launch(
        &self,
        epoch: Epoch,
        epoch_rx: watch::Receiver<Epoch>,
        cid_tree: Arc<CidTree>,
        prepared: kernel::PreparedKernel,
    ) -> Result<RunningGeneration> {
        let root = epoch
            .root
            .as_ref()
            .context("cannot launch a kernel for an unrooted deployment epoch")?;
        let (terminate_tx, terminate_rx) = watch::channel(());
        let (constructed_tx, constructed_rx) = oneshot::channel();
        let (admitted_tx, admitted_rx) = oneshot::channel();
        let result_epoch_rx = epoch_rx.clone();
        let generation = kernel::Generation::new(
            prepared,
            kernel::Root::new(*root, cid_tree),
            EpochGuard {
                issued_seq: epoch.seq,
                receiver: epoch_rx,
            },
            self.identity_state.clone(),
            self.readiness_gate.clone(),
            self.bootstrap.clone(),
            self.runtime_inputs.clone(),
            self.stdio,
            terminate_rx,
            kernel::LaunchAdmission {
                constructed: constructed_tx,
                admitted: admitted_rx,
            },
        )?;
        let filesystem_completion = generation.filesystem_completion();
        let intended_seq = epoch.seq;
        let (result_tx, result_rx) = oneshot::channel();
        self.executor_pool
            .spawn(SpawnRequest {
                name: "kernel".into(),
                factory: Box::new(move |_shutdown| {
                    Box::pin(async move {
                        let result = generation.run().await;
                        if result_tx.send(result).is_ok() {
                            let live_seq = result_epoch_rx.borrow().seq;
                            notify_pid0_result_ready(live_seq).await;
                        }
                    })
                }),
                result_tx: None,
            })
            .map_err(|_| anyhow::anyhow!("executor pool rejected kernel spawn"))?;
        Ok(RunningGeneration {
            admission: Some((constructed_rx, admitted_tx)),
            intended_seq,
            terminate_tx,
            result_rx,
            filesystem_completion,
        })
    }
}

struct RunningGeneration {
    admission: Option<(oneshot::Receiver<()>, oneshot::Sender<()>)>,
    intended_seq: u64,
    terminate_tx: watch::Sender<()>,
    result_rx: oneshot::Receiver<Result<kernel::Outcome>>,
    filesystem_completion: cache::FsCompletion,
}

impl RunningGeneration {
    async fn stopped(
        &mut self,
        completed_result: Option<Result<Result<kernel::Outcome>, oneshot::error::RecvError>>,
    ) -> Result<GenerationStopped> {
        if completed_result.is_none() {
            let _ = (&mut self.result_rx).await;
        }
        self.filesystem_completion
            .wait()
            .await
            .context("kernel generation filesystem cleanup failed")?;
        Ok(GenerationStopped(()))
    }
}

struct GenerationStopped(());

/// Deployment-domain result for one kernel generation.
pub enum Outcome {
    Authoritative {
        epoch: u64,
        result: Result<kernel::Outcome>,
    },
    Replaced {
        old_epoch: u64,
        new_epoch: u64,
        teardown_elapsed: Duration,
    },
    TeardownTimedOut {
        epoch: u64,
        timeout: Duration,
    },
}

enum Target {
    Static,
    Head(Head),
    Invalid(InvalidHead),
}

impl Target {
    fn from_update(update: Update) -> Self {
        match update {
            Update::Head(head) => Self::Head(head),
            Update::InvalidHead(invalid) => Self::Invalid(invalid),
        }
    }

    fn head_bytes(&self) -> Vec<u8> {
        match self {
            Self::Static => Vec::new(),
            Self::Head(head) => head.bytes(),
            Self::Invalid(invalid) => invalid.selected.clone(),
        }
    }

    fn head(&self) -> Option<&Head> {
        match self {
            Self::Head(head) => Some(head),
            Self::Static | Self::Invalid(_) => None,
        }
    }
}

/// The single owner of deployment transitions for one host process.
pub struct Deployment {
    compilation: kernel::Compilation,
    runtime_engine: RuntimeEngine,
    installed_baseline: Option<FrozenMount>,
    active_kernel: kernel::PreparedKernel,
    epoch_tx: watch::Sender<Epoch>,
    epoch_rx: watch::Receiver<Epoch>,
    epoch_seq: u64,
    frozen_mounts: Vec<FrozenMount>,
    frozen_pins: PinSet,
    active_pins: PinSet,
    retained_pins: Vec<PinSet>,
    ipfs_client: crate::ipfs::HttpClient,
    cid_tree: Arc<CidTree>,
    source_rx: Option<mpsc::Receiver<SourceMessage>>,
    source_task: Option<tokio::task::JoinHandle<()>>,
    candidate_rx: Option<watch::Receiver<Option<Vec<u8>>>>,
    candidate_initialized: bool,
    speculation: Option<Speculation>,
}

impl Deployment {
    /// Establish epoch zero and prepare its effective root.
    pub async fn bootstrap(
        config: Config,
        epoch_tx: watch::Sender<Epoch>,
        epoch_rx: watch::Receiver<Epoch>,
    ) -> Result<Self> {
        let frozen_mounts = config.frozen_mounts;
        let mut frozen_pins = PinSet::default();
        for cid in config.retained_source_roots {
            frozen_pins.insert(cid);
        }
        // Resolution has already acknowledged retention. Repeating pin/add
        // would not create another owner and must not authorize later unpinning.
        for mount in config.installed_baseline.iter().chain(frozen_mounts.iter()) {
            if !frozen_pins.contains(&mount.source().cid()) {
                return Err(PermanentPreparationError(format!(
                    "frozen mount source {} has no acknowledged retention",
                    mount.source().cid()
                ))
                .into());
            }
        }
        if frozen_mounts.len()
            + usize::from(config.source.is_some())
            + usize::from(config.installed_baseline.is_some())
            > 128
        {
            return Err(CompositionError::ResourceLimit(
                "deployment source operation limit exceeded",
            )
            .into());
        }

        let (target, source_rx, source_task) = match config.source {
            Some(mut source) => {
                let current = current_with_retry(source.as_mut()).await;
                let (rx, task) = spawn_source_follower(source);
                (Target::from_update(current), Some(rx), Some(task))
            }
            None => (Target::Static, None, None),
        };
        epoch_tx.send_replace(Epoch {
            seq: 0,
            head: target.head_bytes(),
            root: None,
        });

        let state = BootstrapState {
            installed_baseline: config.installed_baseline,
            runtime_engine: config.runtime_engine,
            compilation: kernel::Compilation::default(),
            epoch_tx,
            epoch_rx,
            epoch_seq: 0,
            target,
            frozen_mounts,
            frozen_pins,
            retained_pins: Vec::new(),
            ipfs_client: config.ipfs_client,
            staging_dir: config.staging_dir,
            filesystem_host: config.filesystem_host,
            filesystem_scope: config.filesystem_scope,
            source_rx,
            source_task,
            candidate_rx: config.candidates,
        };
        state.prepare().await
    }

    pub fn cid_tree(&self) -> Arc<CidTree> {
        self.cid_tree.clone()
    }

    pub fn current_epoch(&self) -> Epoch {
        self.epoch_rx.borrow().clone()
    }

    fn candidate_is_active(&self, candidate: &Candidate) -> bool {
        let epoch = self.epoch_rx.borrow();
        epoch.root.is_some() && epoch.head == candidate.head.bytes()
    }

    fn start_speculation(&mut self, candidate: Candidate) {
        if candidate.expires_at <= tokio::time::Instant::now()
            || self.candidate_is_active(&candidate)
        {
            return;
        }
        let (cancel, cancel_rx) = watch::channel(false);
        let task = tokio::spawn(prepare_root_owned(
            candidate.head.clone(),
            self.installed_baseline.clone(),
            self.runtime_engine.clone(),
            self.compilation.clone(),
            self.frozen_mounts.clone(),
            self.ipfs_client.clone(),
            self.cid_tree.clone(),
            PinSet::default(),
            cancel_rx,
        ));
        self.speculation = Some(Speculation::InFlight(SpeculativeTask {
            candidate,
            cancel,
            task,
        }));
    }

    fn start_speculative_release(&mut self, pins: PinSet, next: Option<Candidate>) {
        if pins.is_empty() {
            self.speculation = None;
            if let Some(next) = next {
                self.start_speculation(next);
            }
            return;
        }
        let mut protected = self.protected_cids(None);
        protected.extend(
            self.retained_pins
                .iter()
                .flat_map(|pins| pins.cids.iter().cloned()),
        );
        let ipfs_client = self.ipfs_client.clone();
        let task = tokio::spawn(async move {
            let mut pins = pins;
            release_pin_set(&mut pins, &ipfs_client, &protected, &mut HashSet::new()).await;
            pins
        });
        self.speculation = Some(Speculation::Releasing { task, next });
    }

    fn set_speculative_candidate(&mut self, candidate: Option<Candidate>) {
        let state = self.speculation.take();
        self.speculation = match state {
            None => {
                if let Some(candidate) = candidate {
                    self.start_speculation(candidate);
                }
                return;
            }
            Some(Speculation::InFlight(mut task)) => {
                if candidate
                    .as_ref()
                    .is_some_and(|candidate| candidate.head == task.candidate.head)
                {
                    task.candidate.expires_at = candidate.expect("candidate is present").expires_at;
                    Some(Speculation::InFlight(task))
                } else {
                    let _ = task.cancel.send(true);
                    Some(Speculation::Stopping {
                        task,
                        next: candidate,
                    })
                }
            }
            Some(Speculation::Ready {
                prepared,
                expires_at: _,
            }) => {
                if candidate
                    .as_ref()
                    .is_some_and(|candidate| prepared.head.as_ref() == Some(&candidate.head))
                {
                    Some(Speculation::Ready {
                        prepared,
                        expires_at: candidate.expect("candidate is present").expires_at,
                    })
                } else {
                    let pins = prepared.pins;
                    self.start_speculative_release(pins, candidate);
                    return;
                }
            }
            Some(Speculation::Stopping { task, next: _ }) => Some(Speculation::Stopping {
                task,
                next: candidate,
            }),
            Some(Speculation::Releasing { task, next: _ }) => Some(Speculation::Releasing {
                task,
                next: candidate,
            }),
        };
    }

    fn observe_candidate(&mut self, bytes: Option<Vec<u8>>) {
        let candidate = bytes.and_then(|bytes| match decode_cid(&bytes) {
            Ok(cid) => Some(Candidate {
                head: Head { cid },
                expires_at: tokio::time::Instant::now() + SPECULATIVE_RETENTION,
            }),
            Err(error) => {
                tracing::debug!(%error, "Ignoring malformed advisory Atom head");
                None
            }
        });
        let candidate = candidate.filter(|candidate| !self.candidate_is_active(candidate));
        self.set_speculative_candidate(candidate);
    }

    fn finish_speculative_preparation(
        &mut self,
        candidate: Candidate,
        output: PreparationOutput,
        keep: bool,
        next: Option<Candidate>,
    ) {
        if !keep {
            let pins = match output.result {
                Ok(prepared) => prepared.pins,
                Err(error) => {
                    tracing::debug!(%error, "Discarded obsolete speculative preparation");
                    output.pins
                }
            };
            self.start_speculative_release(pins, next);
            return;
        }

        match output.result {
            Ok(prepared) if candidate.expires_at > tokio::time::Instant::now() => {
                self.speculation = Some(Speculation::Ready {
                    prepared,
                    expires_at: candidate.expires_at,
                });
            }
            Ok(prepared) => self.start_speculative_release(prepared.pins, None),
            Err(error) => {
                if classify_failure(&error) == FailureClass::Transient {
                    warn!(%error, "Transient deployment preparation failure during speculation; candidate abandoned");
                } else {
                    tracing::debug!(%error, "Speculative deployment preparation failed");
                }
                self.start_speculative_release(output.pins, None);
            }
        }
    }

    fn finish_speculative_release(&mut self, pins: PinSet, next: Option<Candidate>) {
        if !pins.is_empty() {
            self.retained_pins.push(pins);
        }
        self.speculation = None;
        if let Some(next) = next {
            self.start_speculation(next);
        }
    }

    /// Launch and supervise the current rooted deployment generation.
    pub async fn run_generation(&mut self, launcher: &KernelLauncher<'_>) -> Result<Outcome> {
        if let Some(outcome) = self.replace_queued_before_launch().await? {
            return Ok(outcome);
        }
        // The final empty queue observation and enqueue have no intervening
        // await. Generation checks the accepted epoch/root again after async
        // construction before publishing identity or executing guest code.
        let epoch = self.current_epoch();
        let running = match launcher.launch(
            epoch,
            self.epoch_rx.clone(),
            self.cid_tree.clone(),
            self.active_kernel.clone(),
        ) {
            Ok(running) => running,
            Err(error) => {
                self.shutdown_speculation().await;
                return Err(error);
            }
        };
        let outcome = self.await_generation(running).await;
        if outcome.is_err() {
            self.shutdown_speculation().await;
        }
        outcome
    }

    async fn replace_queued_before_launch(&mut self) -> Result<Option<Outcome>> {
        let updates = self.drain_source_updates()?;
        if updates.is_empty() {
            return Ok(None);
        }
        self.replace_generation(None, updates, None).await.map(Some)
    }

    /// Stop advisory work and release every speculative pin before host shutdown.
    pub async fn shutdown(&mut self) {
        self.candidate_rx = None;
        self.shutdown_speculation().await;
        self.release_retained().await;
        if let Some(task) = self.source_task.take() {
            task.abort();
        }
    }

    async fn shutdown_speculation(&mut self) {
        let Some(state) = self.speculation.take() else {
            return;
        };
        match state {
            Speculation::InFlight(task) | Speculation::Stopping { task, .. } => {
                let _ = task.cancel.send(true);
                match task.task.await {
                    Ok(output) => {
                        let mut pins = match output.result {
                            Ok(prepared) => prepared.pins,
                            Err(error) => {
                                tracing::debug!(%error, "Speculative preparation stopped during shutdown");
                                output.pins
                            }
                        };
                        self.release_attempt(&mut pins).await;
                    }
                    Err(error) => {
                        warn!(%error, "Speculative preparation task failed during shutdown")
                    }
                }
            }
            Speculation::Ready { prepared, .. } => {
                self.release_attempt(&mut { prepared.pins }).await;
            }
            Speculation::Releasing { task, .. } => match task.await {
                Ok(pins) if !pins.is_empty() => self.retained_pins.push(pins),
                Ok(_) => {}
                Err(error) => warn!(%error, "Speculative pin release task failed during shutdown"),
            },
        }
    }

    async fn await_generation(&mut self, mut running: RunningGeneration) -> Result<Outcome> {
        loop {
            if !self.candidate_initialized {
                let candidate = self
                    .candidate_rx
                    .as_mut()
                    .map(|receiver| receiver.borrow_and_update().clone());
                if let Some(candidate) = candidate {
                    self.observe_candidate(candidate);
                }
                self.candidate_initialized = true;
            }

            enum LiveEvent {
                Constructed,
                Source(Option<SourceMessage>),
                Kernel(Result<Result<kernel::Outcome>, oneshot::error::RecvError>),
                Candidate(Result<(), watch::error::RecvError>),
                Prepared(Result<PreparationOutput, tokio::task::JoinError>),
                Released(Result<PinSet, tokio::task::JoinError>),
                Expired,
            }

            let source_rx = self.source_rx.as_mut();
            let admission_rx = running.admission.as_mut().map(|(rx, _)| rx);
            let candidate_rx = self.candidate_rx.as_mut();
            let (preparation_task, release_task, expires_at) = match self.speculation.as_mut() {
                Some(Speculation::InFlight(task)) => {
                    (Some(&mut task.task), None, Some(task.candidate.expires_at))
                }
                Some(Speculation::Stopping { task, .. }) => (Some(&mut task.task), None, None),
                Some(Speculation::Ready { expires_at, .. }) => (None, None, Some(*expires_at)),
                Some(Speculation::Releasing { task, .. }) => (None, Some(task), None),
                None => (None, None, None),
            };
            let event = tokio::select! {
                biased;
                message = async { source_rx.expect("source receiver is present").recv().await }, if source_rx.is_some() => LiveEvent::Source(message),
                result = &mut running.result_rx => LiveEvent::Kernel(result),
                _ = async { admission_rx.expect("admission receiver is present").await }, if admission_rx.is_some() => LiveEvent::Constructed,
                result = async { preparation_task.expect("preparation task is present").await }, if preparation_task.is_some() => LiveEvent::Prepared(result),
                result = async { release_task.expect("release task is present").await }, if release_task.is_some() => LiveEvent::Released(result),
                changed = async { candidate_rx.expect("candidate receiver is present").changed().await }, if candidate_rx.is_some() => LiveEvent::Candidate(changed),
                _ = async { tokio::time::sleep_until(expires_at.expect("expiry is present")).await }, if expires_at.is_some() => LiveEvent::Expired,
            };

            match event {
                LiveEvent::Constructed => {
                    let (_, admitted) = running.admission.take().expect("admission is present");
                    // This is launch admission's linearization point: consume
                    // every queued authoritative update before approving guest
                    // execution. No await separates the drain from approval.
                    let updates = self.drain_source_updates()?;
                    if !updates.is_empty() {
                        return self.replace(running, updates, None).await;
                    }
                    let _ = admitted.send(());
                }
                LiveEvent::Source(Some(SourceMessage::Update(update))) => {
                    return self.replace(running, vec![update], None).await;
                }
                LiveEvent::Source(Some(SourceMessage::Error(error))) => {
                    warn!("Stem source unavailable; current deployment remains authoritative: {error:#}");
                }
                LiveEvent::Source(None) => anyhow::bail!("Stem source follower stopped"),
                LiveEvent::Kernel(result) => {
                    let updates = self.drain_source_updates()?;
                    if !updates.is_empty() {
                        return self.replace(running, updates, Some(result)).await;
                    }
                    self.shutdown_speculation().await;
                    return Ok(Outcome::Authoritative {
                        epoch: running.intended_seq,
                        result: result.context("kernel result channel dropped")?,
                    });
                }
                LiveEvent::Candidate(Ok(())) => {
                    let candidate = self
                        .candidate_rx
                        .as_mut()
                        .expect("candidate receiver is present")
                        .borrow_and_update()
                        .clone();
                    self.observe_candidate(candidate);
                }
                LiveEvent::Candidate(Err(_)) => {
                    tracing::debug!("Atom advisory candidate feed stopped; disabling speculation");
                    self.candidate_rx = None;
                    self.set_speculative_candidate(None);
                }
                LiveEvent::Prepared(result) => {
                    let state = self
                        .speculation
                        .take()
                        .expect("preparation state is present");
                    let (candidate, keep, next) = match state {
                        Speculation::InFlight(task) => (task.candidate, true, None),
                        Speculation::Stopping { task, next } => (task.candidate, false, next),
                        Speculation::Ready { .. } | Speculation::Releasing { .. } => {
                            unreachable!("preparation event without a preparation task")
                        }
                    };
                    match result {
                        Ok(output) => {
                            self.finish_speculative_preparation(candidate, output, keep, next)
                        }
                        Err(error) => {
                            warn!(%error, "Speculative preparation task failed");
                            self.speculation = None;
                            if let Some(next) = next {
                                self.start_speculation(next);
                            }
                        }
                    }
                }
                LiveEvent::Released(result) => {
                    let state = self.speculation.take().expect("release state is present");
                    let next = match state {
                        Speculation::Releasing { next, .. } => next,
                        Speculation::InFlight(_)
                        | Speculation::Ready { .. }
                        | Speculation::Stopping { .. } => {
                            unreachable!("release event without a release task")
                        }
                    };
                    match result {
                        Ok(pins) => self.finish_speculative_release(pins, next),
                        Err(error) => {
                            warn!(%error, "Speculative pin release task failed");
                            self.speculation = None;
                            if let Some(next) = next {
                                self.start_speculation(next);
                            }
                        }
                    }
                }
                LiveEvent::Expired => self.set_speculative_candidate(None),
            }
        }
    }

    fn accept_update(&mut self, update: Update) -> Result<Target> {
        self.epoch_seq = self
            .epoch_seq
            .checked_add(1)
            .context("deployment epoch counter exhausted")?;
        let target = Target::from_update(update);
        self.epoch_tx.send_replace(Epoch {
            seq: self.epoch_seq,
            head: target.head_bytes(),
            root: None,
        });
        info!(
            event_code = rpc::observability::EventCode::DeploymentEpochAdvanced as u16,
            seq = self.epoch_seq,
            "Advancing deployment epoch; authority revoked"
        );
        Ok(target)
    }

    fn drain_source_updates(&mut self) -> Result<Vec<Update>> {
        let Some(source_rx) = self.source_rx.as_mut() else {
            return Ok(Vec::new());
        };
        let mut updates = Vec::new();
        loop {
            match source_rx.try_recv() {
                Ok(SourceMessage::Update(update)) => updates.push(update),
                Ok(SourceMessage::Error(error)) => {
                    warn!("Stem source unavailable; deployment authority unchanged: {error:#}");
                }
                Err(mpsc::error::TryRecvError::Empty) => return Ok(updates),
                Err(mpsc::error::TryRecvError::Disconnected) => {
                    anyhow::bail!("Stem source follower stopped")
                }
            }
        }
    }

    async fn next_update(&mut self) -> Result<Update> {
        let source_rx = self
            .source_rx
            .as_mut()
            .context("dynamic deployment lost its Stem source")?;
        next_source_update(source_rx).await
    }

    async fn replace(
        &mut self,
        running: RunningGeneration,
        updates: Vec<Update>,
        completed_result: Option<Result<Result<kernel::Outcome>, oneshot::error::RecvError>>,
    ) -> Result<Outcome> {
        self.replace_generation(Some(running), updates, completed_result)
            .await
    }

    async fn replace_generation(
        &mut self,
        mut running: Option<RunningGeneration>,
        updates: Vec<Update>,
        completed_result: Option<Result<Result<kernel::Outcome>, oneshot::error::RecvError>>,
    ) -> Result<Outcome> {
        let old_epoch = running
            .as_ref()
            .map_or(self.epoch_seq, |running| running.intended_seq);
        let started = Instant::now();
        let mut updates = updates.into_iter();
        let mut target = self.accept_update(
            updates
                .next()
                .context("replacement requires an authoritative update")?,
        )?;
        for update in updates {
            target = self.accept_update(update)?;
        }
        wait_at_pid0_epoch_race_barrier(self.epoch_seq).await;
        if let Some(running) = &running {
            let _ = running.terminate_tx.send(());
        }
        let teardown_timer = tokio::time::sleep(KERNEL_TEARDOWN_TIMEOUT);
        tokio::pin!(teardown_timer);
        let stopping = async {
            match running.as_mut() {
                Some(running) => running.stopped(completed_result).await,
                // Admission has not enqueued a generation. The preceding
                // replacement already established its teardown barrier.
                None => Ok(GenerationStopped(())),
            }
        };
        tokio::pin!(stopping);
        let mut stopped = None;
        let mut attempt = PinSet::default();
        let mut attempt_head: Option<cid::Cid> = None;
        let mut retry = RETRY_BASE_DELAY;
        match self.speculation.as_mut() {
            Some(Speculation::Stopping { task, next }) => {
                let _ = task.cancel.send(true);
                *next = None;
            }
            Some(Speculation::Releasing { next, .. }) => *next = None,
            Some(Speculation::InFlight(_) | Speculation::Ready { .. }) | None => {}
        }

        'target: loop {
            while matches!(self.speculation, Some(Speculation::Releasing { .. })) {
                enum ReleaseEvent {
                    Stopped(Result<GenerationStopped>),
                    TimedOut,
                    Update(Result<Update>),
                    Released(Result<PinSet, tokio::task::JoinError>),
                }
                let source_rx = self
                    .source_rx
                    .as_mut()
                    .context("dynamic deployment lost its Stem source")?;
                let release = match self.speculation.as_mut() {
                    Some(Speculation::Releasing { task, .. }) => task,
                    _ => unreachable!("release event without a release task"),
                };
                let event = tokio::select! {
                    biased;
                    result = &mut stopping, if stopped.is_none() => ReleaseEvent::Stopped(result),
                    _ = &mut teardown_timer, if stopped.is_none() => ReleaseEvent::TimedOut,
                    update = next_source_update(source_rx) => ReleaseEvent::Update(update),
                    result = release => ReleaseEvent::Released(result),
                };
                match event {
                    ReleaseEvent::Stopped(result) => stopped = Some(result?),
                    ReleaseEvent::Update(update) => target = self.accept_update(update?)?,
                    ReleaseEvent::Released(result) => {
                        let state = self.speculation.take();
                        debug_assert!(matches!(state, Some(Speculation::Releasing { .. })));
                        match result {
                            Ok(pins) if !pins.is_empty() => self.retained_pins.push(pins),
                            Ok(_) => {}
                            Err(error) => {
                                warn!(%error, "Speculative pin release task failed during authoritative transition")
                            }
                        }
                    }
                    ReleaseEvent::TimedOut => {
                        self.shutdown_speculation().await;
                        return Ok(Outcome::TeardownTimedOut {
                            epoch: old_epoch,
                            timeout: KERNEL_TEARDOWN_TIMEOUT,
                        });
                    }
                }
            }

            let target_matches_speculative_task =
                self.speculation
                    .as_ref()
                    .is_some_and(|speculation| match speculation {
                        Speculation::InFlight(task) => target
                            .head()
                            .is_some_and(|head| head == &task.candidate.head),
                        Speculation::Ready { .. }
                        | Speculation::Stopping { .. }
                        | Speculation::Releasing { .. } => false,
                    });
            if matches!(self.speculation, Some(Speculation::InFlight(_)))
                && !target_matches_speculative_task
            {
                let state = self.speculation.take();
                let Some(Speculation::InFlight(task)) = state else {
                    unreachable!("in-flight speculation state changed synchronously")
                };
                let _ = task.cancel.send(true);
                self.speculation = Some(Speculation::Stopping { task, next: None });
            }
            if matches!(self.speculation, Some(Speculation::Stopping { .. })) {
                enum StopEvent {
                    Stopped(Result<GenerationStopped>),
                    TimedOut,
                    Update(Result<Update>),
                    Prepared(Result<PreparationOutput, tokio::task::JoinError>),
                }
                let source_rx = self
                    .source_rx
                    .as_mut()
                    .context("dynamic deployment lost its Stem source")?;
                let preparation = match self.speculation.as_mut() {
                    Some(Speculation::Stopping { task, .. }) => &mut task.task,
                    _ => unreachable!("preparation event without a stopping task"),
                };
                let event = tokio::select! {
                    biased;
                    result = &mut stopping, if stopped.is_none() => StopEvent::Stopped(result),
                    _ = &mut teardown_timer, if stopped.is_none() => StopEvent::TimedOut,
                    update = next_source_update(source_rx) => StopEvent::Update(update),
                    result = preparation => StopEvent::Prepared(result),
                };
                match event {
                    StopEvent::Stopped(result) => stopped = Some(result?),
                    StopEvent::Update(update) => {
                        target = self.accept_update(update?)?;
                    }
                    StopEvent::Prepared(result) => {
                        let state = self.speculation.take();
                        debug_assert!(matches!(state, Some(Speculation::Stopping { .. })));
                        match result {
                            Ok(output) => {
                                let mut pins = match output.result {
                                    Ok(prepared) => prepared.pins,
                                    Err(error) => {
                                        tracing::debug!(%error, "Discarded mismatched speculative preparation");
                                        output.pins
                                    }
                                };
                                self.release_attempt(&mut pins).await;
                            }
                            Err(error) => {
                                warn!(%error, "Speculative preparation task failed during authoritative transition")
                            }
                        }
                    }
                    StopEvent::TimedOut => {
                        self.shutdown_speculation().await;
                        return Ok(Outcome::TeardownTimedOut {
                            epoch: old_epoch,
                            timeout: KERNEL_TEARDOWN_TIMEOUT,
                        });
                    }
                }
                continue 'target;
            }

            if let Some(Speculation::Ready { prepared, .. }) = self.speculation.as_ref() {
                if prepared.head.as_ref() != target.head() {
                    let state = self.speculation.take();
                    let Some(Speculation::Ready { prepared, .. }) = state else {
                        unreachable!("ready speculation state changed synchronously")
                    };
                    let mut pins = prepared.pins;
                    self.release_attempt(&mut pins).await;
                }
            }

            // `frozen_mounts` never changes after bootstrap, so the head CID is
            // the only variable preparation input within one `Deployment`.

            if let Target::Invalid(invalid) = &target {
                warn!(
                    event_code = rpc::observability::EventCode::InvalidDeploymentHead as u16,
                    seq = self.epoch_seq,
                    selected = %hex::encode(&invalid.selected),
                    reason = %invalid.reason,
                    "Authoritative Stem update selected an invalid deployment head"
                );
                self.release_attempt(&mut attempt).await;
                attempt_head = None;
                loop {
                    tokio::select! {
                        biased;
                        result = &mut stopping, if stopped.is_none() => {
                            stopped = Some(result?);
                        }
                        _ = &mut teardown_timer, if stopped.is_none() => {
                            return Ok(Outcome::TeardownTimedOut {
                                epoch: old_epoch,
                                timeout: KERNEL_TEARDOWN_TIMEOUT,
                            });
                        }
                        update = self.next_update() => {
                            target = self.accept_update(update?)?;
                            continue 'target;
                        }
                    }
                }
            }

            let head = target.head().cloned();
            let accepted_epoch = self.epoch_seq;
            if attempt_head.as_ref() != head.as_ref().map(|head| &head.cid) {
                self.release_attempt(&mut attempt).await;
                attempt_head = head.as_ref().map(|head| head.cid);
            }

            enum PreparationStep {
                Prepared(Result<PreparedRoot>),
                SpeculativeFailed { error: anyhow::Error, pins: PinSet },
                Update(Update),
                TimedOut,
            }
            let installed_baseline = self.installed_baseline.clone();
            let runtime_engine = self.runtime_engine.clone();
            let compilation = self.compilation.clone();
            let frozen_mounts = self.frozen_mounts.clone();
            let ipfs_client = self.ipfs_client.clone();
            let cid_tree = self.cid_tree.clone();
            let step = if matches!(
                self.speculation.as_ref(),
                Some(Speculation::Ready { prepared, .. })
                    if prepared.head.as_ref() == target.head()
            ) {
                let state = self.speculation.take();
                let Some(Speculation::Ready { prepared, .. }) = state else {
                    unreachable!("ready speculation state changed synchronously")
                };
                PreparationStep::Prepared(Ok(prepared))
            } else if target_matches_speculative_task {
                let result = loop {
                    let source_rx = self
                        .source_rx
                        .as_mut()
                        .context("dynamic deployment lost its Stem source")?;
                    let preparation = match self.speculation.as_mut() {
                        Some(Speculation::InFlight(task)) => &mut task.task,
                        _ => unreachable!("promotion event without an in-flight task"),
                    };
                    tokio::select! {
                        biased;
                        result = &mut stopping, if stopped.is_none() => {
                            stopped = Some(result?);
                        }
                        _ = &mut teardown_timer, if stopped.is_none() => {
                            break None;
                        }
                        message = next_source_update(source_rx) => {
                            target = self.accept_update(message?)?;
                            continue 'target;
                        }
                        result = preparation => break Some(result),
                    }
                };
                match result {
                    None => {
                        self.shutdown_speculation().await;
                        return Ok(Outcome::TeardownTimedOut {
                            epoch: old_epoch,
                            timeout: KERNEL_TEARDOWN_TIMEOUT,
                        });
                    }
                    Some(Ok(output)) => {
                        let state = self.speculation.take();
                        debug_assert!(matches!(state, Some(Speculation::InFlight(_))));
                        match output.result {
                            Ok(prepared) => PreparationStep::Prepared(Ok(prepared)),
                            Err(error) => PreparationStep::SpeculativeFailed {
                                error,
                                pins: output.pins,
                            },
                        }
                    }
                    Some(Err(error)) => {
                        self.speculation = None;
                        warn!(%error, "Speculative preparation task failed during promotion");
                        continue 'target;
                    }
                }
            } else {
                let (_cancel, mut cancel_rx) = watch::channel(false);
                let preparation = prepare_root(
                    head,
                    installed_baseline.as_ref(),
                    &runtime_engine,
                    &compilation,
                    &frozen_mounts,
                    &ipfs_client,
                    Prewarm::Existing(&cid_tree),
                    &mut attempt,
                    &mut cancel_rx,
                );
                tokio::pin!(preparation);
                loop {
                    tokio::select! {
                        biased;
                        result = &mut stopping, if stopped.is_none() => {
                            stopped = Some(result?);
                        }
                        _ = &mut teardown_timer, if stopped.is_none() => {
                            break PreparationStep::TimedOut;
                        }
                        message = self.next_update() => {
                            break PreparationStep::Update(message?);
                        }
                        result = &mut preparation => {
                            break PreparationStep::Prepared(result);
                        }
                    }
                }
            };

            match step {
                PreparationStep::TimedOut => {
                    return Ok(Outcome::TeardownTimedOut {
                        epoch: old_epoch,
                        timeout: KERNEL_TEARDOWN_TIMEOUT,
                    });
                }
                PreparationStep::Update(update) => {
                    target = self.accept_update(update)?;
                    retry = RETRY_BASE_DELAY;
                    continue 'target;
                }
                PreparationStep::SpeculativeFailed { error, mut pins } => {
                    tracing::debug!(%error, "Promoted speculative preparation failed; starting authoritative preparation");
                    self.release_attempt(&mut pins).await;
                    continue 'target;
                }
                PreparationStep::Prepared(Err(error)) => match classify_failure(&error) {
                    FailureClass::Transient => {
                        let delay = retry_delay(&mut retry);
                        warn!(
                            event_code = rpc::observability::EventCode::DeploymentRetry as u16,
                            seq = self.epoch_seq,
                            retry_ms = delay.as_millis() as u64,
                            error = error.as_ref() as &dyn std::error::Error,
                            "Transient deployment preparation failure; retry scheduled"
                        );
                        let retry_timer = tokio::time::sleep(delay);
                        tokio::pin!(retry_timer);
                        loop {
                            tokio::select! {
                                biased;
                                result = &mut stopping, if stopped.is_none() => {
                                    stopped = Some(result?);
                                }
                                _ = &mut teardown_timer, if stopped.is_none() => {
                                    return Ok(Outcome::TeardownTimedOut {
                                        epoch: old_epoch,
                                        timeout: KERNEL_TEARDOWN_TIMEOUT,
                                    });
                                }
                                update = self.next_update() => {
                                    target = self.accept_update(update?)?;
                                    retry = RETRY_BASE_DELAY;
                                    continue 'target;
                                }
                                _ = &mut retry_timer => continue 'target,
                            }
                        }
                    }
                    FailureClass::Permanent => {
                        target = Target::Invalid(InvalidHead {
                            selected: target.head_bytes(),
                            reason: format!("deployment root is unusable: {error:#}"),
                        });
                        continue 'target;
                    }
                    FailureClass::Unknown => {
                        return Err(error.context("preparing authoritative deployment root"));
                    }
                },
                PreparationStep::Prepared(Ok(prepared)) => {
                    while stopped.is_none() {
                        tokio::select! {
                            biased;
                            result = &mut stopping => stopped = Some(result?),
                            _ = &mut teardown_timer => {
                                self.release_attempt(&mut { prepared.pins }).await;
                                return Ok(Outcome::TeardownTimedOut {
                                    epoch: old_epoch,
                                    timeout: KERNEL_TEARDOWN_TIMEOUT,
                                });
                            }
                            update = self.next_update() => {
                                let mut pins = prepared.pins;
                                self.release_attempt(&mut pins).await;
                                target = self.accept_update(update?)?;
                                retry = RETRY_BASE_DELAY;
                                continue 'target;
                            }
                        }
                    }

                    let updates = self.drain_source_updates()?;
                    if !updates.is_empty() {
                        let mut pins = prepared.pins;
                        self.release_attempt(&mut pins).await;
                        for update in updates {
                            target = self.accept_update(update)?;
                        }
                        retry = RETRY_BASE_DELAY;
                        continue 'target;
                    }

                    let prepared = self
                        .activate(
                            accepted_epoch,
                            target.head(),
                            prepared,
                            stopped.take().expect("generation stopped"),
                        )
                        .err();
                    if let Some(prepared) = prepared {
                        let mut prepared = *prepared;
                        self.release_attempt(&mut prepared.pins).await;
                        let updates = self.drain_source_updates()?;
                        if !updates.is_empty() {
                            for update in updates {
                                target = self.accept_update(update)?;
                            }
                            continue 'target;
                        }
                        anyhow::bail!(
                            "prepared deployment epoch was superseded without a queued Stem update"
                        );
                    }
                    self.release_retained().await;
                    info!(
                        event_code = rpc::observability::EventCode::DeploymentRootActivated as u16,
                        seq = self.epoch_seq,
                        root = %self.cid_tree.root_cid(),
                        teardown_ms = started.elapsed().as_millis() as u64,
                        "Deployment root activated"
                    );
                    return Ok(Outcome::Replaced {
                        old_epoch,
                        new_epoch: self.epoch_seq,
                        teardown_elapsed: started.elapsed(),
                    });
                }
            }
        }
    }

    fn activate(
        &mut self,
        accepted_epoch: u64,
        accepted_head: Option<&Head>,
        prepared: PreparedRoot,
        _stopped: GenerationStopped,
    ) -> std::result::Result<(), Box<PreparedRoot>> {
        if accepted_epoch != self.epoch_seq || prepared.head.as_ref() != accepted_head {
            return Err(Box::new(prepared));
        }
        let PreparedRoot {
            head,
            effective,
            kernel,
            pins,
        } = prepared;
        self.active_kernel = kernel;
        self.cid_tree.swap_root(effective);
        self.epoch_tx.send_replace(Epoch {
            seq: self.epoch_seq,
            head: head.as_ref().map_or_else(Vec::new, Head::bytes),
            root: Some(effective),
        });
        self.retained_pins
            .push(std::mem::replace(&mut self.active_pins, pins));
        self.cid_tree.cleanup_stubs();
        Ok(())
    }

    async fn release_attempt(&mut self, attempt: &mut PinSet) {
        if attempt.is_empty() {
            return;
        }
        self.retained_pins.push(std::mem::take(attempt));
        self.release_retained().await;
    }

    fn protected_cids(&self, extra: Option<&PinSet>) -> HashSet<Cid> {
        let prepared = match self.speculation.as_ref() {
            Some(Speculation::Ready { prepared, .. }) => Some(&prepared.pins),
            _ => None,
        };
        self.frozen_pins
            .cids
            .iter()
            .chain(self.active_pins.cids.iter())
            .chain(prepared.into_iter().flat_map(|pins| pins.cids.iter()))
            .chain(extra.into_iter().flat_map(|pins| pins.cids.iter()))
            .cloned()
            .collect()
    }

    async fn release_retained(&mut self) {
        let protected = self.protected_cids(None);
        let mut handled = HashSet::new();
        let mut budget = PinReleaseBudget::new();
        for pins in &mut self.retained_pins {
            release_pin_set_with_budget(
                pins,
                &self.ipfs_client,
                &protected,
                &mut handled,
                &mut budget,
            )
            .await;
        }
        self.retained_pins.retain(|pins| !pins.is_empty());
    }
}

impl Drop for Deployment {
    fn drop(&mut self) {
        if let Some(task) = self.source_task.take() {
            task.abort();
        }
        if let Some(speculation) = self.speculation.take() {
            match speculation {
                Speculation::InFlight(task) | Speculation::Stopping { task, .. } => {
                    let _ = task.cancel.send(true);
                    task.task.abort();
                }
                Speculation::Releasing { task, .. } => task.abort(),
                Speculation::Ready { .. } => {
                    warn!("Deployment dropped before speculative pins were released")
                }
            }
        }
    }
}

struct BootstrapState {
    installed_baseline: Option<FrozenMount>,
    runtime_engine: RuntimeEngine,
    compilation: kernel::Compilation,
    epoch_tx: watch::Sender<Epoch>,
    epoch_rx: watch::Receiver<Epoch>,
    epoch_seq: u64,
    target: Target,
    frozen_mounts: Vec<FrozenMount>,
    frozen_pins: PinSet,
    retained_pins: Vec<PinSet>,
    ipfs_client: crate::ipfs::HttpClient,
    staging_dir: PathBuf,
    filesystem_host: Arc<cache::HostResources>,
    filesystem_scope: cache::FsScope,
    source_rx: Option<mpsc::Receiver<SourceMessage>>,
    source_task: Option<tokio::task::JoinHandle<()>>,
    candidate_rx: Option<watch::Receiver<Option<Vec<u8>>>>,
}

impl BootstrapState {
    async fn prepare(mut self) -> Result<Deployment> {
        let mut tree = None;
        let mut attempt = PinSet::default();
        let mut retry = RETRY_BASE_DELAY;
        loop {
            if let Target::Invalid(invalid) = &self.target {
                warn!(
                    selected = %hex::encode(&invalid.selected),
                    reason = %invalid.reason,
                    "Boot Stem selected an invalid deployment head; host remains unrooted"
                );
                self.release_attempt(&mut attempt).await;
                self.target = Target::from_update(self.next_update().await?);
                self.advance_epoch()?;
                continue;
            }

            enum Step {
                Prepared(Result<PreparedRoot>),
                Update(Update),
            }
            let head = self.target.head().cloned();
            let step = {
                let (_cancel, mut cancel_rx) = watch::channel(false);
                let preparation = prepare_root(
                    head,
                    self.installed_baseline.as_ref(),
                    &self.runtime_engine,
                    &self.compilation,
                    &self.frozen_mounts,
                    &self.ipfs_client,
                    Prewarm::Bootstrap {
                        tree: &mut tree,
                        staging_dir: &self.staging_dir,
                        filesystem_host: &self.filesystem_host,
                        filesystem_scope: &self.filesystem_scope,
                    },
                    &mut attempt,
                    &mut cancel_rx,
                );
                tokio::pin!(preparation);
                match self.source_rx.as_mut() {
                    Some(source_rx) => loop {
                        tokio::select! {
                            result = &mut preparation => break Step::Prepared(result),
                            message = source_rx.recv() => match message.context("Stem source follower stopped")? {
                                SourceMessage::Update(update) => break Step::Update(update),
                                SourceMessage::Error(error) => warn!("Stem source unavailable during boot; current baseline remains authoritative: {error:#}"),
                            }
                        }
                    },
                    None => Step::Prepared(preparation.await),
                }
            };

            match step {
                Step::Update(update) => {
                    self.release_attempt(&mut attempt).await;
                    self.target = Target::from_update(update);
                    self.advance_epoch()?;
                    retry = RETRY_BASE_DELAY;
                }
                Step::Prepared(Err(error))
                    if classify_failure(&error) == FailureClass::Transient =>
                {
                    let delay = retry_delay(&mut retry);
                    warn!(
                        retry_ms = delay.as_millis() as u64,
                        "Transient boot deployment preparation failure; retry scheduled: {error:#}"
                    );
                    tokio::time::sleep(delay).await;
                }
                Step::Prepared(Err(error)) => {
                    if self.source_rx.is_some()
                        && classify_failure(&error) == FailureClass::Permanent
                    {
                        self.target = Target::Invalid(InvalidHead {
                            selected: self.target.head_bytes(),
                            reason: format!("deployment root is unusable: {error:#}"),
                        });
                    } else {
                        return Err(error.context("preparing boot deployment root"));
                    }
                }
                Step::Prepared(Ok(mut prepared)) => {
                    let updates = self.drain_updates()?;
                    if !updates.is_empty() {
                        self.release_attempt(&mut prepared.pins).await;
                        for update in updates {
                            self.target = Target::from_update(update);
                            self.advance_epoch()?;
                        }
                        retry = RETRY_BASE_DELAY;
                        continue;
                    }
                    let tree = tree.expect("successful preparation initializes a valid CidTree");
                    tree.swap_root(prepared.effective);
                    self.epoch_tx.send_replace(Epoch {
                        seq: self.epoch_seq,
                        head: prepared.head_bytes(),
                        root: Some(prepared.effective),
                    });
                    info!(
                        event_code = rpc::observability::EventCode::DeploymentRootActivated as u16,
                        seq = self.epoch_seq,
                        "Boot deployment root activated"
                    );
                    return Ok(Deployment {
                        installed_baseline: self.installed_baseline,
                        runtime_engine: self.runtime_engine,
                        active_kernel: prepared.kernel,
                        epoch_tx: self.epoch_tx,
                        epoch_rx: self.epoch_rx,
                        epoch_seq: self.epoch_seq,
                        frozen_mounts: self.frozen_mounts,
                        frozen_pins: self.frozen_pins,
                        active_pins: std::mem::take(&mut prepared.pins),
                        retained_pins: self.retained_pins,
                        ipfs_client: self.ipfs_client,
                        cid_tree: tree,
                        source_rx: self.source_rx,
                        source_task: self.source_task,
                        candidate_rx: self.candidate_rx,
                        candidate_initialized: false,
                        compilation: self.compilation,
                        speculation: None,
                    });
                }
            }
        }
    }

    fn advance_epoch(&mut self) -> Result<()> {
        self.epoch_seq = self
            .epoch_seq
            .checked_add(1)
            .context("deployment epoch counter exhausted")?;
        self.epoch_tx.send_replace(Epoch {
            seq: self.epoch_seq,
            head: self.target.head_bytes(),
            root: None,
        });
        info!(
            event_code = rpc::observability::EventCode::DeploymentEpochAdvanced as u16,
            seq = self.epoch_seq,
            "Advancing deployment epoch during boot"
        );
        Ok(())
    }

    async fn next_update(&mut self) -> Result<Update> {
        let source_rx = self.source_rx.as_mut().context("Stem source is absent")?;
        loop {
            match source_rx
                .recv()
                .await
                .context("Stem source follower stopped")?
            {
                SourceMessage::Update(update) => return Ok(update),
                SourceMessage::Error(error) => {
                    warn!("Stem source unavailable during boot: {error:#}")
                }
            }
        }
    }

    fn drain_updates(&mut self) -> Result<Vec<Update>> {
        let Some(source_rx) = self.source_rx.as_mut() else {
            return Ok(Vec::new());
        };
        let mut updates = Vec::new();
        loop {
            match source_rx.try_recv() {
                Ok(SourceMessage::Update(update)) => updates.push(update),
                Ok(SourceMessage::Error(error)) => {
                    warn!("Stem source unavailable during boot: {error:#}")
                }
                Err(mpsc::error::TryRecvError::Empty) => return Ok(updates),
                Err(mpsc::error::TryRecvError::Disconnected) => {
                    anyhow::bail!("Stem source follower stopped")
                }
            }
        }
    }

    async fn release_attempt(&mut self, attempt: &mut PinSet) {
        let protected: HashSet<Cid> = self
            .frozen_pins
            .cids
            .iter()
            .chain(self.retained_pins.iter().flat_map(|pins| pins.cids.iter()))
            .cloned()
            .collect();
        release_pin_set(attempt, &self.ipfs_client, &protected, &mut HashSet::new()).await;
        if !attempt.is_empty() {
            self.retained_pins.push(std::mem::take(attempt));
        }
    }
}

struct PinReleaseBudget {
    deadline: tokio::time::Instant,
    remaining: usize,
}

impl PinReleaseBudget {
    fn new() -> Self {
        Self {
            deadline: tokio::time::Instant::now() + OPERATION_TIMEOUT,
            remaining: MAX_PIN_RELEASES,
        }
    }
}

async fn release_pin_set(
    pins: &mut PinSet,
    ipfs_client: &crate::ipfs::HttpClient,
    protected: &HashSet<Cid>,
    handled: &mut HashSet<Cid>,
) {
    release_pin_set_with_budget(
        pins,
        ipfs_client,
        protected,
        handled,
        &mut PinReleaseBudget::new(),
    )
    .await;
}

async fn release_pin_set_with_budget(
    pins: &mut PinSet,
    ipfs_client: &crate::ipfs::HttpClient,
    protected: &HashSet<Cid>,
    handled: &mut HashSet<Cid>,
    budget: &mut PinReleaseBudget,
) {
    let mut retained = Vec::new();
    let mut attempts = 0;
    for cid in pins.cids.drain(..) {
        if protected.contains(&cid) || !handled.insert(cid) {
            continue;
        }
        if budget.remaining == 0 || tokio::time::Instant::now() >= budget.deadline {
            retained.push(cid);
            continue;
        }
        attempts += 1;
        budget.remaining -= 1;
        let removal =
            tokio::time::timeout_at(budget.deadline, ipfs_client.pin_rm(&cid.to_string()))
                .await
                .map_err(|_| anyhow::anyhow!("deployment pin cleanup deadline reached"))
                .and_then(|result| result);
        match removal {
            Ok(()) => info!(%cid, "Released deployment pin"),
            Err(error) => {
                warn!(%cid, "Deployment pin release deferred: {error:#}");
                retained.push(cid);
            }
        }
    }
    if !retained.is_empty() {
        warn!(
            roots = retained.len(),
            attempts, "Deployment pins remain retained after bounded cleanup"
        );
    }
    pins.cids = retained;
}

#[cfg(debug_assertions)]
fn pid0_result_race_barrier(epoch: u64) -> Option<std::net::SocketAddr> {
    let value = std::env::var("WW_TEST_PID0_RESULT_RACE").ok()?;
    let (configured, address) = value.split_once('@')?;
    (configured.parse::<u64>().ok()? == epoch).then(|| address.parse().ok())?
}

#[cfg(debug_assertions)]
async fn wait_at_pid0_epoch_race_barrier(epoch: u64) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let Some(address) = pid0_result_race_barrier(epoch) else {
        return;
    };
    let mut stream = tokio::net::TcpStream::connect(address)
        .await
        .expect("connect PID0 epoch race test barrier");
    stream
        .write_all(b"E")
        .await
        .expect("signal PID0 epoch observation");
    let mut release = [0_u8; 1];
    stream
        .read_exact(&mut release)
        .await
        .expect("wait for PID0 epoch race test release");
}

#[cfg(not(debug_assertions))]
async fn wait_at_pid0_epoch_race_barrier(_epoch: u64) {}

#[cfg(debug_assertions)]
async fn notify_pid0_result_ready(epoch: u64) {
    use tokio::io::AsyncWriteExt;

    let Some(address) = pid0_result_race_barrier(epoch) else {
        return;
    };
    let mut stream = tokio::net::TcpStream::connect(address)
        .await
        .expect("connect PID0 result-ready test barrier");
    stream
        .write_all(b"R")
        .await
        .expect("signal PID0 result readiness");
}

#[cfg(not(debug_assertions))]
async fn notify_pid0_result_ready(_epoch: u64) {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    const PREPARATION_REQUESTS: [&str; 9] = [
        "/api/v0/pin/add",
        "/api/v0/block/get",
        "/api/v0/block/get",
        "/api/v0/dag/import",
        "/api/v0/ls",
        "/api/v0/block/get",
        "/api/v0/block/get",
        "/api/v0/block/get",
        "/api/v0/block/get",
    ];
    const ROOT: &str = "bafybeidpy7qr6ny6gz7tjqnj4kfnr6inigfle3boepq6afg6uzuc5h4tf4";

    async fn frozen_root(cid: &str) -> FrozenMount {
        let (client, _, _requests, shutdown, server) = recording_kubo().await;
        let client = crate::ipfs::BootClient::one_attempt(client, OPERATION_TIMEOUT);
        let (_cancel, mut cancel) = watch::channel(false);
        let source = crate::cell::image::inspect_source(cid.parse().unwrap(), &client, &mut cancel)
            .await
            .unwrap();
        shutdown.send(()).unwrap();
        server.await.unwrap();
        FrozenMount::new(source, crate::cell::image::GuestTarget::root()).unwrap()
    }

    fn test_filesystem_host() -> Arc<cache::HostResources> {
        crate::services::test_filesystem_resources()
    }

    fn test_filesystem_scope() -> cache::FsScope {
        crate::services::test_filesystem_scope()
    }

    #[test]
    fn deterministic_composer_errors_preserve_later_head_recovery() {
        use crate::cell::image::GuestTarget;
        for error in [
            CompositionError::TargetObstruction {
                target: GuestTarget::new("/etc/app/config").unwrap(),
                ancestor: GuestTarget::new("/etc").unwrap(),
            },
            CompositionError::InvalidDag("malformed source".into()),
            CompositionError::UnsupportedProfile("unsupported source".into()),
            CompositionError::ResourceLimit("source limit"),
        ] {
            assert_eq!(
                classify_failure(&anyhow::Error::new(error).context("preparing root")),
                FailureClass::Permanent
            );
        }
        assert_eq!(
            classify_failure(&anyhow::anyhow!("unclassified error")),
            FailureClass::Unknown
        );
        assert_eq!(
            classify_failure(&anyhow::Error::new(OperationTimeout {
                operation: "test",
                timeout: OPERATION_TIMEOUT,
            })),
            FailureClass::Transient
        );
    }

    #[test]
    fn deterministic_kernel_validation_errors_preserve_later_head_recovery() {
        let (runtime, _clock) = cell::engine::runtime_engine().unwrap();
        for bytes in [
            b"invalid wasm".to_vec(),
            wat::parse_str("(component)").unwrap(),
            vec![0; cell::sched::MAX_COMPONENT_BYTES + 1],
        ] {
            let error = match cell::proc::ValidatedKernel::compile(&runtime, &bytes) {
                Ok(_) => panic!("invalid kernel accepted"),
                Err(error) => anyhow::Error::new(error).context("preparing selected kernel"),
            };
            assert_eq!(classify_failure(&error), FailureClass::Permanent);
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn filesystem_admission_failure_classification_preserves_retry_policy() {
        let (host_owner, host) = cache::HostFsOwner::start(
            tokio::runtime::Handle::current(),
            cache::HostLimits::new(2, 2, 1).unwrap(),
        )
        .unwrap();
        let (scope, owner, finalizer, completion) = host.try_filesystem_scope().unwrap();
        let exhausted = match host.try_filesystem_scope() {
            Ok(_) => panic!("cleanup-root overload unexpectedly admitted"),
            Err(error) => anyhow::Error::new(error),
        };
        assert_eq!(classify_failure(&exhausted), FailureClass::Transient);

        host_owner.close_admission();
        let closed = anyhow::Error::new(host.try_logical_io().unwrap_err());
        assert_eq!(classify_failure(&closed), FailureClass::Permanent);

        drop(scope);
        drop(owner);
        assert_eq!(finalizer.run().await, Ok(()));
        assert_eq!(completion.wait().await, Ok(()));
        host_owner.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn pin_aliases_share_ownership_and_protection() {
        let root = crate::ipfs::cid_identity::parse_cid(ROOT).unwrap();
        let alias = root
            .to_string_of_base(cid::multibase::Base::Base58Btc)
            .unwrap();
        let mut pins = PinSet::default();
        pins.insert(root);
        pins.insert(crate::ipfs::cid_identity::parse_cid(&alias).unwrap());
        assert_eq!(pins.cids, [root]);
        let client = crate::ipfs::HttpClient::new("http://127.0.0.1:1".to_owned());
        release_pin_set(
            &mut pins,
            &client,
            &HashSet::from([root]),
            &mut HashSet::new(),
        )
        .await;
        assert!(
            pins.is_empty(),
            "an alias owner must be protected without attempting an unpin"
        );
    }

    #[tokio::test]
    async fn cleanup_defers_roots_beyond_the_attempt_limit() {
        let (client, calls, _requests, shutdown, server) = recording_kubo().await;
        let mut pins = PinSet::default();
        for index in 0..=MAX_PIN_RELEASES {
            let digest = [index as u8; 32];
            pins.insert(Cid::new_v1(
                0x55,
                cid::multihash::Multihash::wrap(0x12, &digest).unwrap(),
            ));
        }
        release_pin_set(&mut pins, &client, &HashSet::new(), &mut HashSet::new()).await;
        assert_eq!(calls.load(Ordering::SeqCst), MAX_PIN_RELEASES);
        assert_eq!(pins.cids.len(), 1);
        release_pin_set(&mut pins, &client, &HashSet::new(), &mut HashSet::new()).await;
        assert!(pins.is_empty());
        assert_eq!(calls.load(Ordering::SeqCst), MAX_PIN_RELEASES + 1);
        shutdown.send(()).unwrap();
        server.await.unwrap();
    }

    #[tokio::test]
    async fn cleanup_budget_is_shared_across_retained_pin_sets() {
        let (client, calls, _requests, shutdown, server) = recording_kubo().await;
        let staging = tempfile::tempdir().unwrap();
        let (_source_tx, source_rx) = mpsc::channel(2);
        let (_candidate_tx, candidate_rx) = watch::channel(None);
        let (mut deployment, _, _) =
            test_deployment(client, staging.path().to_owned(), source_rx, candidate_rx);
        for index in 0..=MAX_PIN_RELEASES {
            let digest = [index as u8; 32];
            deployment.retained_pins.push(PinSet {
                cids: vec![Cid::new_v1(
                    0x55,
                    cid::multihash::Multihash::wrap(0x12, &digest).unwrap(),
                )],
            });
        }
        deployment.release_retained().await;
        assert_eq!(calls.load(Ordering::SeqCst), MAX_PIN_RELEASES);
        assert_eq!(deployment.retained_pins.len(), 1);
        deployment.release_retained().await;
        assert!(deployment.retained_pins.is_empty());
        assert_eq!(calls.load(Ordering::SeqCst), MAX_PIN_RELEASES + 1);
        shutdown.send(()).unwrap();
        server.await.unwrap();
    }

    #[tokio::test]
    async fn unretained_frozen_mount_fails_before_any_pin() {
        let mount = frozen_root(ROOT).await;
        let (client, calls, _requests, shutdown, server) = recording_kubo().await;
        let staging = tempfile::tempdir().unwrap();
        let (epoch_tx, epoch_rx) = watch::channel(Epoch::zero());
        let observer = epoch_rx.clone();
        let result = Deployment::bootstrap(
            Config {
                installed_baseline: None,
                runtime_engine: kernel::test_support::runtime(),
                source: None,
                candidates: None,
                frozen_mounts: vec![mount],
                retained_source_roots: Vec::new(),
                ipfs_client: client,
                staging_dir: staging.path().to_owned(),
                filesystem_host: test_filesystem_host(),
                filesystem_scope: test_filesystem_scope(),
            },
            epoch_tx,
            epoch_rx,
        )
        .await;
        assert!(result.is_err());
        assert_eq!(observer.borrow().root, None);
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        shutdown.send(()).unwrap();
        server.await.unwrap();
    }

    #[tokio::test]
    async fn malformed_advisory_bytes_never_start_preparation() {
        let (client, calls, _requests, shutdown, server) = recording_kubo().await;
        let staging = tempfile::tempdir().unwrap();
        let (_source_tx, source_rx) = mpsc::channel(2);
        let (_candidate_tx, candidate_rx) = watch::channel(None);
        let (mut deployment, tree, observer) =
            test_deployment(client, staging.path().to_owned(), source_rx, candidate_rx);
        let canonical = ROOT.parse::<cid::Cid>().unwrap().to_bytes();
        let mut trailing = canonical.clone();
        trailing.push(0);
        let mut nonminimal = vec![0x81, 0];
        nonminimal.extend_from_slice(&canonical[1..]);
        for bytes in [
            trailing,
            nonminimal,
            canonical[..canonical.len() - 1].to_vec(),
            vec![0xff; 11],
        ] {
            deployment.observe_candidate(Some(bytes));
            assert!(
                deployment.speculation.is_none(),
                "malformed bytes started speculative preparation"
            );
            assert_eq!(calls.load(Ordering::SeqCst), 0);
        }
        assert_eq!(observer.borrow().seq, 0);
        assert_eq!(tree.root_cid().as_ref(), &OLD_ROOT.parse::<Cid>().unwrap());
        shutdown.send(()).unwrap();
        server.await.unwrap();
    }

    #[tokio::test]
    async fn bootstrap_tree_is_absent_until_valid_composition() {
        let staging = tempfile::tempdir().unwrap();
        let client = crate::ipfs::HttpClient::new("http://127.0.0.1:1".to_owned());
        let mut tree = None;
        let mut pins = PinSet::default();
        let (_cancel_tx, mut cancel) = watch::channel(false);
        let result = prepare_root(
            None,
            None,
            &kernel::test_support::runtime(),
            &kernel::Compilation::default(),
            &[],
            &client,
            Prewarm::Bootstrap {
                tree: &mut tree,
                staging_dir: staging.path(),
                filesystem_host: &test_filesystem_host(),
                filesystem_scope: &test_filesystem_scope(),
            },
            &mut pins,
            &mut cancel,
        )
        .await;
        assert!(result.is_err());
        assert!(tree.is_none());
        assert!(pins.is_empty());

        let (client, server) = single_root_kubo().await;
        let prepared = prepare_root(
            Some(Head {
                cid: ROOT.parse().unwrap(),
            }),
            None,
            &kernel::test_support::runtime(),
            &kernel::Compilation::default(),
            &[],
            &client,
            Prewarm::Bootstrap {
                tree: &mut tree,
                staging_dir: staging.path(),
                filesystem_host: &test_filesystem_host(),
                filesystem_scope: &test_filesystem_scope(),
            },
            &mut pins,
            &mut cancel,
        )
        .await
        .unwrap();
        assert_eq!(
            tree.unwrap().root_cid().as_ref(),
            &ROOT.parse::<Cid>().unwrap()
        );
        assert_eq!(*prepared.effective(), ROOT.parse::<cid::Cid>().unwrap());
        assert_eq!(prepared.pins.cids, [ROOT.parse::<cid::Cid>().unwrap()]);
        assert!(pins.is_empty());
        server.await.unwrap();
    }

    const OLD_ROOT: &str = "bafkreibm6jg3ux5qugqkmfqt5uj5rxszb4sa4e3u7jj4c5ukv5s4xvcc7a";
    const SHARED_ROOT: &str = "bafkreif2pall7dybz7vecqka3zo24nq2j4tztjwc5c3f4vmrf6sz4d3asa";

    #[tokio::test]
    async fn bootstrap_prewarm_failure_keeps_attempt_pins_for_retry() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            for (index, expected) in [
                "/api/v0/pin/add",
                "/api/v0/block/get",
                "/api/v0/block/get",
                "/api/v0/dag/import",
                "/api/v0/ls",
                "/api/v0/block/get",
                "/api/v0/block/get",
                "/api/v0/dag/import",
                "/api/v0/ls",
                "/api/v0/block/get",
                "/api/v0/block/get",
                "/api/v0/block/get",
                "/api/v0/block/get",
            ]
            .into_iter()
            .enumerate()
            {
                let (mut stream, _) = listener.accept().await.unwrap();
                let request = read_request(&mut stream).await;
                assert!(
                    request.lines().next().unwrap().contains(expected),
                    "{request}"
                );
                if index == 4 {
                    stream.write_all(b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 4\r\nConnection: close\r\n\r\nbusy").await.unwrap();
                } else {
                    respond(&mut stream, &request).await;
                }
            }
        });
        let client = crate::ipfs::HttpClient::new(format!("http://{address}"));
        let staging = tempfile::tempdir().unwrap();
        let mut tree = None;
        let mut pins = PinSet::default();
        let (_cancel, mut cancel) = watch::channel(false);
        let head = Head {
            cid: ROOT.parse().unwrap(),
        };
        let error = prepare_root(
            Some(head.clone()),
            None,
            &kernel::test_support::runtime(),
            &kernel::Compilation::default(),
            &[],
            &client,
            Prewarm::Bootstrap {
                tree: &mut tree,
                staging_dir: staging.path(),
                filesystem_host: &test_filesystem_host(),
                filesystem_scope: &test_filesystem_scope(),
            },
            &mut pins,
            &mut cancel,
        )
        .await
        .unwrap_err();
        assert_eq!(classify_failure(&error), FailureClass::Transient);
        assert_eq!(pins.cids, [head.cid]);
        assert_eq!(
            tree.as_ref().unwrap().root_cid().as_ref(),
            &ROOT.parse::<Cid>().unwrap()
        );
        let prepared = prepare_root(
            Some(head.clone()),
            None,
            &kernel::test_support::runtime(),
            &kernel::Compilation::default(),
            &[],
            &client,
            Prewarm::Bootstrap {
                tree: &mut tree,
                staging_dir: staging.path(),
                filesystem_host: &test_filesystem_host(),
                filesystem_scope: &test_filesystem_scope(),
            },
            &mut pins,
            &mut cancel,
        )
        .await
        .unwrap();
        assert_eq!(prepared.pins.cids, [head.cid]);
        assert!(pins.is_empty());
        server.await.unwrap();
    }

    #[tokio::test]
    async fn bootstrap_supersession_releases_distinct_head_and_effective_pins() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (request_tx, mut requests) = mpsc::unbounded_channel();
        let next_root = DIRECTORY_BLOCKS[1].0;
        let old_head = COMPOSED_DIRECTORY_BLOCKS[0].0;
        let frozen = COMPOSED_DIRECTORY_BLOCKS[1].0;
        let old_effective = COMPOSED_DIRECTORY_BLOCKS[2].0;
        assert_ne!(old_head, old_effective);
        assert_ne!(old_head, frozen);
        assert_ne!(old_effective, frozen);
        let server = tokio::spawn(async move {
            let mut stalled = None;
            for (index, (expected, expected_cid)) in [
                ("/api/v0/pin/add", Some(old_head)),
                ("/api/v0/block/get", Some(old_head)),
                ("/api/v0/block/get", Some(KERNEL_BLOCKS[0].0)),
                ("/api/v0/block/get", Some(frozen)),
                ("/api/v0/dag/import", None),
                ("/api/v0/ls", Some(old_effective)),
                ("/api/v0/pin/rm", Some(old_head)),
                ("/api/v0/pin/rm", Some(old_effective)),
                ("/api/v0/pin/add", Some(next_root)),
                ("/api/v0/block/get", Some(next_root)),
                ("/api/v0/block/get", Some(KERNEL_BLOCKS[0].0)),
                ("/api/v0/block/get", Some(frozen)),
                ("/api/v0/dag/import", None),
                ("/api/v0/ls", Some(frozen)),
                ("/api/v0/block/get", Some(frozen)),
                ("/api/v0/block/get", Some(KERNEL_BLOCKS[0].0)),
                ("/api/v0/block/get", Some(KERNEL_BLOCKS[1].0)),
                ("/api/v0/block/get", Some(KERNEL_BLOCKS[1].0)),
            ]
            .into_iter()
            .enumerate()
            {
                let (mut stream, _) = listener.accept().await.unwrap();
                let request = read_request(&mut stream).await;
                let path = request
                    .lines()
                    .next()
                    .unwrap()
                    .split_whitespace()
                    .nth(1)
                    .unwrap()
                    .to_owned();
                assert!(path.starts_with(expected), "{path}");
                if let Some(expected_cid) = expected_cid {
                    assert!(path.contains(expected_cid), "wrong CID for {path}");
                }
                if index == 5 {
                    stalled = Some(stream);
                    request_tx.send(path).unwrap();
                    continue;
                }
                respond(&mut stream, &request).await;
                request_tx.send(path).unwrap();
            }
            drop(stalled);
        });
        let initial = Head {
            cid: old_head.parse().unwrap(),
        };
        let (epoch_tx, epoch_rx) = watch::channel(Epoch {
            seq: 0,
            head: initial.bytes(),
            root: None,
        });
        let observer = epoch_rx.clone();
        let (source_tx, source_rx) = mpsc::channel(2);
        let staging = tempfile::tempdir().unwrap();
        let state = BootstrapState {
            installed_baseline: None,
            runtime_engine: kernel::test_support::runtime(),
            compilation: kernel::Compilation::default(),
            epoch_tx,
            epoch_rx,
            epoch_seq: 0,
            target: Target::Head(initial),
            frozen_mounts: vec![frozen_root(frozen).await],
            frozen_pins: PinSet {
                cids: vec![frozen.parse().unwrap()],
            },
            retained_pins: Vec::new(),
            ipfs_client: crate::ipfs::HttpClient::new(format!("http://{address}")),
            staging_dir: staging.path().to_owned(),
            filesystem_host: test_filesystem_host(),
            filesystem_scope: test_filesystem_scope(),
            source_rx: Some(source_rx),
            source_task: None,
            candidate_rx: None,
        };
        let bootstrap = tokio::spawn(state.prepare());
        for _ in 0..6 {
            next_request(&mut requests).await;
        }
        assert_eq!(
            observer.borrow().root,
            None,
            "prewarm must precede rooted epoch publication"
        );
        let next = Head {
            cid: next_root.parse().unwrap(),
        };
        source_tx
            .send(SourceMessage::Update(Update::Head(next.clone())))
            .await
            .unwrap();
        let removed_head = next_request(&mut requests).await;
        assert!(removed_head.contains(old_head), "{removed_head}");
        let removed_effective = next_request(&mut requests).await;
        assert!(
            removed_effective.contains(old_effective),
            "{removed_effective}"
        );
        assert!(!removed_head.contains(frozen), "{removed_head}");
        assert!(!removed_effective.contains(frozen), "{removed_effective}");
        let deployment = tokio::time::timeout(Duration::from_secs(2), bootstrap)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(deployment.current_epoch().seq, 1);
        assert_eq!(deployment.current_epoch().head, next.bytes());
        assert_eq!(
            deployment.current_epoch().root,
            Some(frozen.parse().unwrap())
        );
        assert_eq!(
            deployment.cid_tree.root_cid().as_ref(),
            &frozen.parse::<Cid>().unwrap()
        );
        assert_eq!(
            deployment.active_pins.cids,
            [next.cid, frozen.parse().unwrap()]
        );
        assert_eq!(deployment.frozen_pins.cids, [frozen.parse().unwrap()]);
        server.await.unwrap();
    }

    // SHA-256 UnixFS fixtures include a statically valid P3 boot kernel.
    const DIRECTORY_BLOCKS: [(&str, &[u8]); 3] = [
        kernel::test_support::BLOCKS[0],
        kernel::test_support::BLOCKS[1],
        kernel::test_support::BLOCKS[2],
    ];
    const COMPOSED_DIRECTORY_BLOCKS: [(&str, &[u8]); 3] = [
        kernel::test_support::BLOCKS[3],
        kernel::test_support::BLOCKS[4],
        kernel::test_support::BLOCKS[5],
    ];
    const KERNEL_BLOCKS: [(&str, &[u8]); 2] = [
        kernel::test_support::BLOCKS[6],
        kernel::test_support::BLOCKS[7],
    ];

    struct KuboRequest {
        text: String,
        bytes: Vec<u8>,
    }

    impl std::ops::Deref for KuboRequest {
        type Target = str;
        fn deref(&self) -> &str {
            &self.text
        }
    }

    impl std::fmt::Display for KuboRequest {
        fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str(&self.text)
        }
    }

    async fn read_request(stream: &mut tokio::net::TcpStream) -> KuboRequest {
        let mut request = Vec::new();
        loop {
            let mut buffer = [0_u8; 8192];
            let read = stream.read(&mut buffer).await.unwrap();
            assert_ne!(read, 0, "request ended before complete headers/body");
            request.extend_from_slice(&buffer[..read]);
            if let Some(end) = request.windows(4).position(|part| part == b"\r\n\r\n") {
                let header = String::from_utf8_lossy(&request[..end]);
                let length = header
                    .lines()
                    .find_map(|line| {
                        let (key, value) = line.split_once(':')?;
                        key.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().unwrap())
                    })
                    .unwrap_or(0);
                if request.len() >= end + 4 + length {
                    break;
                }
            }
        }
        KuboRequest {
            text: String::from_utf8_lossy(&request).into_owned(),
            bytes: request,
        }
    }

    async fn respond(stream: &mut tokio::net::TcpStream, request: &KuboRequest) {
        let path = request
            .lines()
            .next()
            .unwrap()
            .split_whitespace()
            .nth(1)
            .unwrap();
        let body = if path.starts_with("/api/v0/block/get") {
            kernel::test_support::BLOCKS
                .iter()
                .find(|(cid, _)| path.contains(cid))
                .expect("known fixture block")
                .1
                .to_vec()
        } else if path.starts_with("/api/v0/dag/import") {
            let root = kernel::test_support::BLOCKS
                .iter()
                .find_map(|(root, _)| {
                    let cid = root.parse::<cid::Cid>().unwrap().to_bytes();
                    request
                        .bytes
                        .windows(cid.len())
                        .any(|part| part == cid)
                        .then_some(*root)
                })
                .expect("CAR header must contain a known fixture root");
            serde_json::json!({"Root":{"Cid":{"/":root},"PinErrorMsg":""}})
                .to_string()
                .into_bytes()
        } else if path.starts_with("/api/v0/ls") {
            serde_json::json!({"Objects":[{"Links":[]}]})
                .to_string()
                .into_bytes()
        } else {
            assert!(
                path.starts_with("/api/v0/pin/"),
                "unexpected endpoint {path}"
            );
            b"{}".to_vec()
        };
        let header = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        stream.write_all(header.as_bytes()).await.unwrap();
        stream.write_all(&body).await.unwrap();
    }

    async fn single_root_kubo() -> (crate::ipfs::HttpClient, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            for expected in PREPARATION_REQUESTS {
                let (mut stream, _) = listener.accept().await.unwrap();
                let request = read_request(&mut stream).await;
                assert!(
                    request.lines().next().unwrap().contains(expected),
                    "{request}"
                );
                respond(&mut stream, &request).await;
            }
        });
        (
            crate::ipfs::HttpClient::new(format!("http://{address}")),
            server,
        )
    }

    async fn recording_kubo() -> (
        crate::ipfs::HttpClient,
        Arc<AtomicUsize>,
        mpsc::UnboundedReceiver<String>,
        oneshot::Sender<()>,
        tokio::task::JoinHandle<()>,
    ) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let server_calls = calls.clone();
        let (request_tx, request_rx) = mpsc::unbounded_channel();
        let (shutdown_tx, mut shutdown_rx) = oneshot::channel();
        let server = tokio::spawn(async move {
            loop {
                let accepted = tokio::select! {
                    accepted = listener.accept() => accepted,
                    _ = &mut shutdown_rx => return,
                };
                let (mut stream, _) = accepted.unwrap();
                let request = read_request(&mut stream).await;
                let path = request
                    .lines()
                    .next()
                    .and_then(|line| line.split_whitespace().nth(1))
                    .unwrap()
                    .to_owned();
                server_calls.fetch_add(1, Ordering::SeqCst);
                request_tx.send(path.clone()).unwrap();
                respond(&mut stream, &request).await;
            }
        });
        (
            crate::ipfs::HttpClient::new(format!("http://{address}")),
            calls,
            request_rx,
            shutdown_tx,
            server,
        )
    }

    fn test_deployment(
        client: crate::ipfs::HttpClient,
        staging_dir: PathBuf,
        source_rx: mpsc::Receiver<SourceMessage>,
        candidate_rx: watch::Receiver<Option<Vec<u8>>>,
    ) -> (Deployment, Arc<CidTree>, watch::Receiver<Epoch>) {
        let tree = Arc::new(CidTree::new(
            OLD_ROOT.parse().unwrap(),
            client.clone(),
            staging_dir,
            test_filesystem_host(),
            test_filesystem_scope(),
        ));
        let (epoch_tx, epoch_rx) = watch::channel(Epoch {
            seq: 0,
            head: Vec::new(),
            root: Some(OLD_ROOT.parse().unwrap()),
        });
        let deployment = Deployment {
            installed_baseline: None,
            runtime_engine: kernel::test_support::runtime(),
            active_kernel: kernel::test_support::prepared(ROOT),
            epoch_tx,
            epoch_rx: epoch_rx.clone(),
            epoch_seq: 0,
            frozen_mounts: Vec::new(),
            frozen_pins: PinSet::default(),
            active_pins: PinSet::default(),
            retained_pins: Vec::new(),
            ipfs_client: client,
            cid_tree: tree.clone(),
            source_rx: Some(source_rx),
            source_task: None,
            candidate_rx: Some(candidate_rx),
            candidate_initialized: false,
            compilation: kernel::Compilation::default(),
            speculation: None,
        };
        (deployment, tree, epoch_rx)
    }

    fn test_running_generation() -> (
        RunningGeneration,
        oneshot::Sender<Result<kernel::Outcome>>,
        watch::Receiver<()>,
    ) {
        let (terminate_tx, terminate_rx) = watch::channel(());
        let (result_tx, result_rx) = oneshot::channel();
        (
            RunningGeneration {
                admission: None,
                intended_seq: 0,
                terminate_tx,
                result_rx,
                filesystem_completion: completed_filesystem_scope(),
            },
            result_tx,
            terminate_rx,
        )
    }

    #[tokio::test]
    async fn queued_source_update_wins_over_completed_construction_admission() {
        let (client, _calls, _requests, shutdown, server) = recording_kubo().await;
        let staging = tempfile::tempdir().unwrap();
        let (source_tx, source_rx) = mpsc::channel(2);
        let (_candidate_tx, candidate_rx) = watch::channel(None);
        let (mut deployment, _tree, epoch_rx) =
            test_deployment(client, staging.path().to_owned(), source_rx, candidate_rx);
        let (mut running, result_tx, mut terminate_rx) = test_running_generation();
        let (constructed_tx, constructed_rx) = oneshot::channel();
        let (admitted_tx, admitted_rx) = oneshot::channel();
        running.admission = Some((constructed_rx, admitted_tx));
        constructed_tx.send(()).unwrap();
        source_tx
            .send(SourceMessage::Update(Update::Head(Head {
                cid: ROOT.parse().unwrap(),
            })))
            .await
            .unwrap();
        let worker = tokio::spawn(async move {
            terminate_rx.changed().await.unwrap();
            result_tx.send(Ok(kernel::Outcome::Terminated)).unwrap();
        });
        let outcome = deployment.await_generation(running).await.unwrap();
        worker.await.unwrap();
        assert!(matches!(outcome, Outcome::Replaced { new_epoch: 1, .. }));
        assert!(
            admitted_rx.await.is_err(),
            "obsolete construction must not receive approval"
        );
        assert_eq!(epoch_rx.borrow().seq, 1);
        shutdown.send(()).unwrap();
        server.await.unwrap();
    }

    #[tokio::test]
    async fn queued_updates_supersede_an_activated_root_before_launch() {
        let (client, calls, _requests, shutdown, server) = recording_kubo().await;
        let staging = tempfile::tempdir().unwrap();
        let (source_tx, source_rx) = mpsc::channel(2);
        let (_candidate_tx, candidate_rx) = watch::channel(None);
        let (mut deployment, tree, epoch_rx) =
            test_deployment(client, staging.path().to_owned(), source_rx, candidate_rx);
        let head = Head {
            cid: ROOT.parse().unwrap(),
        };
        // Model updates queued while cleanup follows activation. No generation
        // has been enqueued; the stale activated root must not reach launch.
        source_tx
            .send(SourceMessage::Update(Update::InvalidHead(InvalidHead {
                selected: vec![1],
                reason: "superseded invalid head".into(),
            })))
            .await
            .unwrap();
        source_tx
            .send(SourceMessage::Update(Update::Head(head.clone())))
            .await
            .unwrap();
        let outcome = deployment
            .replace_queued_before_launch()
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(
            outcome,
            Outcome::Replaced {
                old_epoch: 0,
                new_epoch: 2,
                ..
            }
        ));
        assert_eq!(epoch_rx.borrow().seq, 2);
        assert_eq!(epoch_rx.borrow().head, head.bytes());
        assert_eq!(epoch_rx.borrow().root, Some(head.cid));
        assert_eq!(*tree.root_cid(), head.cid);
        assert_eq!(calls.load(Ordering::SeqCst), 9);
        assert!(deployment
            .replace_queued_before_launch()
            .await
            .unwrap()
            .is_none());
        shutdown.send(()).unwrap();
        server.await.unwrap();
    }

    fn completed_filesystem_scope() -> cache::FsCompletion {
        let (scope, owner, finalizer, completion) =
            test_filesystem_host().try_filesystem_scope().unwrap();
        drop(scope);
        drop(owner);
        drop(finalizer);
        completion
    }

    async fn next_request(requests: &mut mpsc::UnboundedReceiver<String>) -> String {
        tokio::time::timeout(Duration::from_secs(2), requests.recv())
            .await
            .expect("Kubo request timed out")
            .expect("Kubo request server stopped")
    }

    struct ScriptedSource {
        current: Option<Update>,
        next: VecDeque<Result<Update>>,
    }

    #[async_trait::async_trait]
    impl Source for ScriptedSource {
        async fn current(&mut self) -> Result<Update> {
            self.current
                .take()
                .context("scripted Source current() called more than once")
        }

        async fn next(&mut self) -> Result<Update> {
            match self.next.pop_front() {
                Some(result) => result,
                None => std::future::pending().await,
            }
        }
    }

    #[tokio::test]
    async fn static_bootstrap_publishes_rooted_epoch_zero_without_a_source() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            for expected in &PREPARATION_REQUESTS[1..] {
                let (mut stream, _) = listener.accept().await.unwrap();
                let request = read_request(&mut stream).await;
                assert!(
                    request.lines().next().unwrap().contains(expected),
                    "{request}"
                );
                respond(&mut stream, &request).await;
            }
        });
        let (epoch_tx, epoch_rx) = watch::channel(Epoch::zero());
        let staging = tempfile::tempdir().unwrap();
        let deployment = Deployment::bootstrap(
            Config {
                installed_baseline: None,
                runtime_engine: kernel::test_support::runtime(),
                source: None,
                candidates: None,
                frozen_mounts: vec![frozen_root(ROOT).await],
                retained_source_roots: vec![ROOT.parse().unwrap()],
                ipfs_client: crate::ipfs::HttpClient::new(format!("http://{address}")),
                staging_dir: staging.path().to_owned(),
                filesystem_host: test_filesystem_host(),
                filesystem_scope: test_filesystem_scope(),
            },
            epoch_tx,
            epoch_rx,
        )
        .await
        .unwrap();

        let epoch = deployment.current_epoch();
        assert_eq!(epoch.seq, 0);
        assert!(epoch.head.is_empty());
        assert_eq!(epoch.root, Some(ROOT.parse().unwrap()));
        assert!(deployment.source_rx.is_none());
        assert_eq!(
            deployment.cid_tree.root_cid().as_ref(),
            &ROOT.parse::<Cid>().unwrap()
        );
        server.await.unwrap();
    }

    #[tokio::test]
    async fn source_baseline_always_uses_local_epoch_zero() {
        let (ipfs_client, server) = single_root_kubo().await;
        let head = Head {
            cid: ROOT.parse().unwrap(),
        };
        let source = ScriptedSource {
            current: Some(Update::Head(head.clone())),
            next: VecDeque::new(),
        };
        let (epoch_tx, epoch_rx) = watch::channel(Epoch::zero());
        let staging = tempfile::tempdir().unwrap();

        let deployment = Deployment::bootstrap(
            Config {
                installed_baseline: None,
                runtime_engine: kernel::test_support::runtime(),
                source: Some(Box::new(source)),
                candidates: None,
                frozen_mounts: Vec::new(),
                retained_source_roots: Vec::new(),
                ipfs_client,
                staging_dir: staging.path().to_owned(),
                filesystem_host: test_filesystem_host(),
                filesystem_scope: test_filesystem_scope(),
            },
            epoch_tx,
            epoch_rx,
        )
        .await
        .unwrap();

        let epoch = deployment.current_epoch();
        assert_eq!(epoch.seq, 0);
        assert_eq!(epoch.head, head.bytes());
        assert_eq!(epoch.root, Some(ROOT.parse().unwrap()));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn later_valid_update_recovers_an_invalid_boot_head() {
        let (ipfs_client, server) = single_root_kubo().await;
        let valid = Head {
            cid: ROOT.parse().unwrap(),
        };
        let source = ScriptedSource {
            current: Some(Update::InvalidHead(InvalidHead {
                selected: b"invalid".to_vec(),
                reason: "selected bytes are not a CID".to_owned(),
            })),
            next: VecDeque::from([Ok(Update::Head(valid.clone()))]),
        };
        let (epoch_tx, epoch_rx) = watch::channel(Epoch::zero());
        let staging = tempfile::tempdir().unwrap();

        let deployment = Deployment::bootstrap(
            Config {
                installed_baseline: None,
                runtime_engine: kernel::test_support::runtime(),
                source: Some(Box::new(source)),
                candidates: None,
                frozen_mounts: Vec::new(),
                retained_source_roots: Vec::new(),
                ipfs_client,
                staging_dir: staging.path().to_owned(),
                filesystem_host: test_filesystem_host(),
                filesystem_scope: test_filesystem_scope(),
            },
            epoch_tx,
            epoch_rx,
        )
        .await
        .unwrap();

        let epoch = deployment.current_epoch();
        assert_eq!(epoch.seq, 1);
        assert_eq!(epoch.head, valid.bytes());
        assert_eq!(epoch.root, Some(ROOT.parse().unwrap()));
        server.await.unwrap();
    }

    #[test]
    fn local_epoch_increment_is_checked() {
        let client = crate::ipfs::HttpClient::new("http://127.0.0.1:1".to_owned());
        let staging = tempfile::tempdir().unwrap();
        let tree = Arc::new(CidTree::new(
            OLD_ROOT.parse().unwrap(),
            client.clone(),
            staging.path().to_owned(),
            test_filesystem_host(),
            test_filesystem_scope(),
        ));
        let initial = Epoch {
            seq: u64::MAX,
            head: Vec::new(),
            root: Some(OLD_ROOT.parse().unwrap()),
        };
        let (epoch_tx, epoch_rx) = watch::channel(initial.clone());
        let mut deployment = Deployment {
            installed_baseline: None,
            runtime_engine: kernel::test_support::runtime(),
            active_kernel: kernel::test_support::prepared(ROOT),
            epoch_tx,
            epoch_rx,
            epoch_seq: u64::MAX,
            frozen_mounts: Vec::new(),
            frozen_pins: PinSet::default(),
            active_pins: PinSet::default(),
            retained_pins: Vec::new(),
            ipfs_client: client,
            cid_tree: tree,
            source_rx: None,
            source_task: None,
            candidate_rx: None,
            candidate_initialized: false,
            compilation: kernel::Compilation::default(),
            speculation: None,
        };

        let result = deployment.accept_update(Update::InvalidHead(InvalidHead {
            selected: Vec::new(),
            reason: "overflow test".to_owned(),
        }));
        assert!(result.is_err());
        let error = result.err().unwrap();

        assert!(error.to_string().contains("counter exhausted"));
        let current = deployment.current_epoch();
        assert_eq!(current.seq, initial.seq);
        assert_eq!(current.head, initial.head);
        assert_eq!(current.root, initial.root);
    }

    #[test]
    fn superseded_prepared_root_never_becomes_selected() {
        let client = crate::ipfs::HttpClient::new("http://127.0.0.1:1".to_owned());
        let staging = tempfile::tempdir().unwrap();
        let tree = Arc::new(CidTree::new(
            OLD_ROOT.parse().unwrap(),
            client.clone(),
            staging.path().to_owned(),
            test_filesystem_host(),
            test_filesystem_scope(),
        ));
        let (epoch_tx, epoch_rx) = watch::channel(Epoch {
            seq: 3,
            head: Vec::new(),
            root: None,
        });
        let mut deployment = Deployment {
            installed_baseline: None,
            runtime_engine: kernel::test_support::runtime(),
            active_kernel: kernel::test_support::prepared(ROOT),
            epoch_tx,
            epoch_rx,
            epoch_seq: 3,
            frozen_mounts: Vec::new(),
            frozen_pins: PinSet::default(),
            active_pins: PinSet::default(),
            retained_pins: Vec::new(),
            ipfs_client: client,
            cid_tree: tree.clone(),
            source_rx: None,
            source_task: None,
            candidate_rx: None,
            candidate_initialized: false,
            compilation: kernel::Compilation::default(),
            speculation: None,
        };
        let prepared = PreparedRoot {
            head: None,
            effective: DIRECTORY_BLOCKS[2].0.parse().unwrap(),
            kernel: kernel::test_support::prepared(DIRECTORY_BLOCKS[2].0),
            pins: PinSet::default(),
        };

        let result = deployment.activate(2, None, prepared, GenerationStopped(()));

        assert!(result.is_err());
        assert_eq!(tree.root_cid().as_ref(), &OLD_ROOT.parse::<Cid>().unwrap());
        assert_eq!(deployment.current_epoch().root, None);
    }

    #[tokio::test]
    async fn preparation_does_not_select_the_candidate_root() {
        let (client, server) = single_root_kubo().await;
        let staging = tempfile::tempdir().unwrap();
        let tree = Arc::new(CidTree::new(
            OLD_ROOT.parse().unwrap(),
            client.clone(),
            staging.path().to_owned(),
            test_filesystem_host(),
            test_filesystem_scope(),
        ));
        let mut pins = PinSet::default();
        let (_cancel, mut cancel_rx) = watch::channel(false);

        let prepared = prepare_root(
            Some(Head {
                cid: ROOT.parse().unwrap(),
            }),
            None,
            &kernel::test_support::runtime(),
            &kernel::Compilation::default(),
            &[],
            &client,
            Prewarm::Existing(&tree),
            &mut pins,
            &mut cancel_rx,
        )
        .await
        .unwrap();

        assert_eq!(*prepared.effective(), ROOT.parse::<Cid>().unwrap());
        assert_eq!(tree.root_cid().as_ref(), &OLD_ROOT.parse::<Cid>().unwrap());
        server.await.unwrap();
    }

    // Catches divergent composition, kernel selection, and validation between
    // standalone image preparation and deployment's actual preparation path.
    #[tokio::test]
    async fn standalone_and_boot_preparation_agree_on_frozen_roots() {
        let (client, _, _requests, shutdown, server) = recording_kubo().await;
        let staging = tempfile::tempdir().unwrap();
        let tree = Arc::new(CidTree::new(
            OLD_ROOT.parse().unwrap(),
            client.clone(),
            staging.path().to_owned(),
            test_filesystem_host(),
            test_filesystem_scope(),
        ));
        let boot_client = crate::ipfs::BootClient::one_attempt(client.clone(), OPERATION_TIMEOUT);
        let runtime = kernel::test_support::runtime();
        let compilation = kernel::Compilation::default();
        let (_cancel, mut cancel) = watch::channel(false);
        for (root, expected_failure) in [
            (ROOT, None),
            (kernel::test_support::EMPTY_ROOT, Some("missing")),
            (kernel::test_support::INVALID_ROOT, Some("invalid")),
        ] {
            let root = root.parse::<Cid>().unwrap();
            let boot = prepare_root(
                Some(Head { cid: root }),
                None,
                &runtime,
                &compilation,
                &[],
                &client,
                Prewarm::Existing(&tree),
                &mut PinSet::default(),
                &mut cancel,
            )
            .await;
            let standalone = async {
                let effective =
                    crate::image::compose_root(Some(root), None, &[], &boot_client, &mut cancel)
                        .await?;
                crate::image::prepare_kernel(
                    effective,
                    &boot_client,
                    runtime.clone(),
                    &compilation,
                    &mut cancel,
                )
                .await
            }
            .await;
            match expected_failure {
                None => {
                    let boot = boot.unwrap();
                    let standalone = standalone.unwrap();
                    assert_eq!(standalone.effective_root(), root);
                    assert_eq!(standalone.effective_root(), boot.effective);
                    assert_eq!(standalone.source_file(), boot.kernel.source_file());
                    assert_eq!(standalone.runtime_cid(), boot.kernel.runtime_cid());
                    assert_eq!(
                        standalone.source_file(),
                        KERNEL_BLOCKS[1].0.parse::<Cid>().unwrap()
                    );
                }
                Some(kind) => {
                    let boot = boot.unwrap_err();
                    let standalone = standalone.unwrap_err();
                    assert_eq!(classify_failure(&boot), FailureClass::Permanent);
                    assert_eq!(classify_failure(&standalone), FailureClass::Permanent);
                    if kind == "missing" {
                        for error in [&boot, &standalone] {
                            assert_eq!(
                                error
                                    .downcast_ref::<cell::image::KernelFileError>()
                                    .unwrap()
                                    .kind(),
                                cell::image::KernelFileFailure::MissingPath
                            );
                        }
                    } else {
                        for error in [&boot, &standalone] {
                            assert!(error
                                .downcast_ref::<cell::proc::KernelValidationError>()
                                .is_some());
                        }
                    }
                }
            }
        }
        assert_eq!(*tree.root_cid(), OLD_ROOT.parse::<Cid>().unwrap());
        shutdown.send(()).unwrap();
        server.await.unwrap();
    }

    // Small deterministic UnixFS fixtures let the two production entry points
    // consume identical bytes. Backend faults are injected below HTTP; this is
    // not a substitute for the real Kubo pin/import/GC integration fixtures.
    #[derive(Clone, Default)]
    struct PreparationBlocks(std::collections::HashMap<Cid, Vec<u8>>);

    impl PreparationBlocks {
        fn varint(mut value: u64, bytes: &mut Vec<u8>) {
            while value >= 128 {
                bytes.push(value as u8 | 0x80);
                value >>= 7;
            }
            bytes.push(value as u8);
        }

        fn field(tag: u8, value: &[u8], bytes: &mut Vec<u8>) {
            bytes.push(tag);
            Self::varint(value.len() as u64, bytes);
            bytes.extend_from_slice(value);
        }

        fn block(&mut self, codec: u64, bytes: Vec<u8>) -> (Cid, u64) {
            use sha2::{Digest, Sha256};
            let cid = Cid::new_v1(
                codec,
                cid::multihash::Multihash::wrap(0x12, &Sha256::digest(&bytes)).unwrap(),
            );
            let size = bytes.len() as u64;
            self.0.insert(cid, bytes);
            (cid, size)
        }

        fn pb(&mut self, metadata: &[u8], links: &[(&str, (Cid, u64))]) -> (Cid, u64) {
            let mut bytes = Vec::new();
            let mut descendants = 0;
            for (name, (cid, size)) in links {
                let mut link = Vec::new();
                Self::field(10, &cid.to_bytes(), &mut link);
                Self::field(18, name.as_bytes(), &mut link);
                link.push(24);
                Self::varint(*size, &mut link);
                Self::field(18, &link, &mut bytes);
                descendants += size;
            }
            Self::field(10, metadata, &mut bytes);
            let (cid, size) = self.block(0x70, bytes);
            (cid, size + descendants)
        }

        fn directory(&mut self, links: &[(&str, (Cid, u64))]) -> (Cid, u64) {
            self.pb(&[8, 1], links)
        }

        fn root(&mut self, kernel: (Cid, u64)) -> Cid {
            let boot = self.directory(&[("kernel.wasm", kernel)]);
            self.directory(&[("boot", boot)]).0
        }

        fn kernel(bytes: Vec<u8>) -> (Self, Cid, Cid) {
            let mut blocks = Self::default();
            let file = blocks.block(0x55, bytes);
            let root = blocks.root(file);
            (blocks, root, file.0)
        }

        async fn serve(
            self,
            root: Cid,
        ) -> (
            crate::ipfs::HttpClient,
            oneshot::Sender<()>,
            tokio::task::JoinHandle<()>,
        ) {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let (shutdown, mut stop) = oneshot::channel();
            let task = tokio::spawn(async move {
                loop {
                    let accepted = tokio::select! {
                        result = listener.accept() => result.unwrap(),
                        _ = &mut stop => return,
                    };
                    let (mut stream, _) = accepted;
                    let request = read_request(&mut stream).await;
                    let path = request
                        .lines()
                        .next()
                        .unwrap()
                        .split_whitespace()
                        .nth(1)
                        .unwrap();
                    let (status, body) = if path.starts_with("/api/v0/block/get") {
                        let value = path
                            .split("arg=")
                            .nth(1)
                            .unwrap()
                            .split('&')
                            .next()
                            .unwrap();
                        let cid = value.parse::<Cid>().unwrap();
                        match self.0.get(&cid) {
                            Some(bytes) => ("200 OK", bytes.clone()),
                            None => (
                                "500 Internal Server Error",
                                br#"{"Message":"block unavailable","Code":0,"Type":"error"}"#
                                    .to_vec(),
                            ),
                        }
                    } else if path.starts_with("/api/v0/dag/import") {
                        assert!(request
                            .bytes
                            .windows(root.to_bytes().len())
                            .any(|part| part == root.to_bytes()));
                        ("200 OK", serde_json::json!({"Root":{"Cid":{"/":root.to_string()},"PinErrorMsg":""}}).to_string().into_bytes())
                    } else if path.starts_with("/api/v0/ls") {
                        ("200 OK", br#"{"Objects":[{"Links":[]}]}"#.to_vec())
                    } else {
                        assert!(
                            path.starts_with("/api/v0/pin/add"),
                            "unexpected endpoint {path}"
                        );
                        ("200 OK", b"{}".to_vec())
                    };
                    let header = format!(
                        "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    stream.write_all(header.as_bytes()).await.unwrap();
                    stream.write_all(&body).await.unwrap();
                }
            });
            (
                crate::ipfs::HttpClient::new(format!("http://{address}")),
                shutdown,
                task,
            )
        }
    }

    // Match the accepted production fixture's import declarations. These WAT
    // variants change only the member/type being challenged, and a trapping
    // start proves both preparation paths stop before guest instantiation.
    fn preparation_component(transport: &str, readiness: &str, command: &str) -> Vec<u8> {
        wat::parse_str(format!(
            r#"(component
            {transport}
            {readiness}
            (core module $module
                (func $start unreachable)
                (start $start)
                (func (export "run") unreachable))
            (core instance $instance (instantiate $module))
            {command})"#
        ))
        .unwrap()
    }

    const PREPARATION_TRANSPORT: &str = r#"
        (import "wetware:transport/connection@0.2.0" (instance
            (type $error (variant (case "failed" string)))
            (export "transport-error" (type $transport-error (eq $error)))
            (export "open" (func (param "outgoing" (stream u8))
                (result (tuple (stream u8) (future (result (error $transport-error)))))))))"#;
    const PREPARATION_READINESS: &str = r#"
        (import "wetware:kernel-runtime/readiness@1.0.0" (instance
            (type $error (enum "stale-generation"))
            (export "ready-error" (type $ready-error (eq $error)))
            (export "kernel-ready" (func (result (result (error $ready-error)))))))"#;
    const PREPARATION_COMMAND: &str = r#"
        (func $run async (result (result))
            (canon lift (core func $instance "run") async))
        (instance (export (interface "wasi:cli/run@0.3.0"))
            (export "run" (func $run)))"#;

    #[derive(Clone, Copy, Debug)]
    enum PreparationExpectation {
        Valid(Cid),
        InvalidDag(&'static str),
        InvalidFile(&'static str),
        Unavailable,
        InvalidComponent,
        Compatibility(&'static str),
    }

    #[tokio::test]
    async fn standalone_and_boot_agree_on_dag_faults_and_p3_contracts() {
        use PreparationExpectation::*;
        let valid = preparation_component(
            PREPARATION_TRANSPORT,
            PREPARATION_READINESS,
            PREPARATION_COMMAND,
        );
        let (valid_blocks, valid_root, valid_file) = PreparationBlocks::kernel(valid.clone());
        let mut cases = vec![(
            "trapping-start",
            valid_blocks.clone(),
            valid_root,
            Valid(valid_file),
        )];
        for (name, transport, readiness, command, member) in [
            (
                "missing-open",
                r#"(import "wetware:transport/connection@0.2.0" (instance))"#,
                PREPARATION_READINESS,
                PREPARATION_COMMAND,
                "open",
            ),
            (
                "missing-ready",
                PREPARATION_TRANSPORT,
                r#"(import "wetware:kernel-runtime/readiness@1.0.0" (instance))"#,
                PREPARATION_COMMAND,
                "kernel-ready",
            ),
            (
                "wrong-open-type",
                r#"(import "wetware:transport/connection@0.2.0" (instance (export "open" (func))))"#,
                PREPARATION_READINESS,
                PREPARATION_COMMAND,
                "open",
            ),
            (
                "wrong-ready-type",
                PREPARATION_TRANSPORT,
                r#"(import "wetware:kernel-runtime/readiness@1.0.0" (instance (export "kernel-ready" (func))))"#,
                PREPARATION_COMMAND,
                "kernel-ready",
            ),
            (
                "missing-command",
                PREPARATION_TRANSPORT,
                PREPARATION_READINESS,
                "",
                "wasi:cli/run",
            ),
            (
                "wrong-command-result",
                PREPARATION_TRANSPORT,
                PREPARATION_READINESS,
                r#"(func $run async (result u32) (canon lift (core func $instance "run") async)) (instance (export (interface "wasi:cli/run@0.3.0")) (export "run" (func $run)))"#,
                "signature",
            ),
            (
                "wrong-command-type",
                PREPARATION_TRANSPORT,
                PREPARATION_READINESS,
                r#"(func $run (canon lift (core func $instance "run"))) (instance (export (interface "wasi:cli/run@0.3.0")) (export "run" (func $run)))"#,
                "run",
            ),
        ] {
            let bytes = preparation_component(transport, readiness, command);
            let (blocks, root, _) = PreparationBlocks::kernel(bytes);
            cases.push((name, blocks, root, Compatibility(member)));
        }
        for (name, bytes) in [
            ("invalid-component", b"not wasm".to_vec()),
            ("core-module", wat::parse_str("(module)").unwrap()),
        ] {
            let (blocks, root, _) = PreparationBlocks::kernel(bytes);
            cases.push((name, blocks, root, InvalidComponent));
        }
        let mut corrupted = valid_blocks.clone();
        corrupted.0.get_mut(&valid_file).unwrap()[0] ^= 1;
        cases.push(("corrupt-raw", corrupted, valid_root, InvalidFile("CID")));
        let mut corrupted = valid_blocks.clone();
        corrupted.0.get_mut(&valid_root).unwrap()[0] ^= 1;
        cases.push(("corrupt-dagpb", corrupted, valid_root, InvalidDag("CID")));
        for (name, missing) in [
            ("missing-file-block", valid_file),
            ("missing-directory-block", valid_root),
        ] {
            let mut missing_blocks = valid_blocks.clone();
            missing_blocks.0.remove(&missing);
            cases.push((name, missing_blocks, valid_root, Unavailable));
        }
        for (name, metadata) in [("hamt", vec![8, 5]), ("malformed-unixfs", vec![0xff])] {
            let mut blocks = PreparationBlocks::default();
            let root = blocks.pb(&metadata, &[]).0;
            cases.push((
                name,
                blocks,
                root,
                InvalidDag(if name == "hamt" { "HAMT" } else { "malformed" }),
            ));
        }
        let mut blocks = PreparationBlocks::default();
        let root = blocks.block(0x70, vec![0x12, 0xff]).0;
        cases.push(("malformed-dagpb", blocks, root, InvalidDag("malformed")));
        for name in [
            "directory-kernel",
            "symlink-kernel",
            "symlink-ancestor",
            "file-ancestor",
        ] {
            let mut blocks = PreparationBlocks::default();
            let entry = match name {
                "directory-kernel" => blocks.directory(&[]),
                "file-ancestor" => blocks.block(0x55, b"file".to_vec()),
                _ => blocks.pb(&[8, 4, 18, 1, b'x'], &[]),
            };
            let root = if name.ends_with("ancestor") {
                blocks.directory(&[("boot", entry)]).0
            } else {
                blocks.root(entry)
            };
            cases.push((
                name,
                blocks,
                root,
                InvalidFile(if name.ends_with("ancestor") {
                    "ancestor"
                } else if name == "symlink-kernel" {
                    "symlink"
                } else {
                    "not a regular file"
                }),
            ));
        }
        let mut blocks = PreparationBlocks::default();
        let chunk = blocks.block(0x55, vec![0; 1024 * 1024]);
        let mut metadata = vec![8, 2, 24];
        PreparationBlocks::varint(9 * chunk.1, &mut metadata);
        for _ in 0..9 {
            metadata.push(32);
            PreparationBlocks::varint(chunk.1, &mut metadata);
        }
        let file = blocks.pb(&metadata, &vec![("", chunk); 9]);
        let root = blocks.root(file);
        cases.push((
            "repeated-chunks-over-logical-limit",
            blocks,
            root,
            InvalidFile("8 MiB"),
        ));

        let runtime = kernel::test_support::runtime();
        let compilation = kernel::Compilation::default();
        for (name, blocks, root, expected) in cases {
            let (client, shutdown, server) = blocks.serve(root).await;
            let boot_client =
                crate::ipfs::BootClient::one_attempt(client.clone(), OPERATION_TIMEOUT);
            let staging = tempfile::tempdir().unwrap();
            let tree = Arc::new(CidTree::new(
                OLD_ROOT.parse().unwrap(),
                client.clone(),
                staging.path().into(),
                test_filesystem_host(),
                test_filesystem_scope(),
            ));
            let (_cancel, mut cancel) = watch::channel(false);
            let boot = prepare_root(
                Some(Head { cid: root }),
                None,
                &runtime,
                &compilation,
                &[],
                &client,
                Prewarm::Existing(&tree),
                &mut PinSet::default(),
                &mut cancel,
            )
            .await
            .map(|prepared| prepared.kernel);
            let standalone = async {
                let effective =
                    crate::image::compose_root(Some(root), None, &[], &boot_client, &mut cancel)
                        .await?;
                crate::image::prepare_kernel(
                    effective,
                    &boot_client,
                    runtime.clone(),
                    &compilation,
                    &mut cancel,
                )
                .await
            }
            .await;
            match expected {
                Valid(file) => {
                    let boot = boot.unwrap_or_else(|error| panic!("{name} boot: {error:#}"));
                    let standalone =
                        standalone.unwrap_or_else(|error| panic!("{name} check: {error:#}"));
                    assert_eq!(boot.effective_root(), root, "{name}");
                    assert_eq!(standalone.effective_root(), root, "{name}");
                    assert_eq!(boot.source_file(), file, "{name}");
                    assert_eq!(standalone.source_file(), file, "{name}");
                    assert_eq!(boot.runtime_cid(), kernel::runtime_cid(&valid), "{name}");
                    assert_eq!(standalone.runtime_cid(), boot.runtime_cid(), "{name}");
                }
                _ => {
                    for (entrypoint, result) in [("boot", boot), ("check", standalone)] {
                        let error = result.expect_err(name);
                        let diagnostic = format!("{error:#}");
                        let context = format!("{name} {entrypoint}: {diagnostic}");
                        assert_eq!(
                            classify_failure(&error),
                            if matches!(expected, Unavailable) {
                                FailureClass::Transient
                            } else {
                                FailureClass::Permanent
                            },
                            "{context}"
                        );
                        match expected {
                            InvalidDag(message) => {
                                assert!(
                                    error.downcast_ref::<CompositionError>().is_some(),
                                    "{context}"
                                );
                                assert!(diagnostic.contains(message), "{context}");
                            }
                            InvalidFile(message) => {
                                assert_eq!(
                                    error
                                        .downcast_ref::<cell::image::KernelFileError>()
                                        .unwrap()
                                        .kind(),
                                    cell::image::KernelFileFailure::InvalidSource,
                                    "{context}"
                                );
                                assert!(diagnostic.contains(message), "{context}");
                            }
                            InvalidComponent => assert_eq!(
                                error
                                    .downcast_ref::<cell::proc::KernelValidationError>()
                                    .unwrap()
                                    .kind(),
                                cell::proc::KernelValidationFailure::InvalidComponent,
                                "{context}"
                            ),
                            Compatibility(message) => {
                                assert_eq!(
                                    error
                                        .downcast_ref::<cell::proc::KernelValidationError>()
                                        .unwrap()
                                        .kind(),
                                    cell::proc::KernelValidationFailure::Compatibility,
                                    "{context}"
                                );
                                assert!(diagnostic.contains(message), "{context}");
                            }
                            Unavailable => {}
                            Valid(_) => unreachable!(),
                        }
                    }
                }
            }
            assert_eq!(
                *tree.root_cid(),
                OLD_ROOT.parse::<Cid>().unwrap(),
                "{name}: preparation activated the root"
            );
            shutdown.send(()).unwrap();
            server.await.unwrap();
            eprintln!("preparation-equivalence {name}: {expected:?}");
        }
    }

    #[tokio::test]
    async fn standalone_and_boot_reject_the_same_obstructed_or_excessive_layers() {
        use crate::cell::image::GuestTarget;

        let baseline = frozen_root(ROOT).await;
        let (client, _, _requests, shutdown, server) = recording_kubo().await;
        let boot_client = crate::ipfs::BootClient::one_attempt(client.clone(), OPERATION_TIMEOUT);
        let (_cancel, mut cancel) = watch::channel(false);
        let source = crate::cell::image::inspect_source(
            KERNEL_BLOCKS[1].0.parse().unwrap(),
            &boot_client,
            &mut cancel,
        )
        .await
        .unwrap();
        let obstructed = vec![
            FrozenMount::new(source.clone(), GuestTarget::new("/boot").unwrap()).unwrap(),
            FrozenMount::new(source, GuestTarget::new("/boot/kernel.wasm").unwrap()).unwrap(),
        ];
        let staging = tempfile::tempdir().unwrap();
        let tree = Arc::new(CidTree::new(
            OLD_ROOT.parse().unwrap(),
            client.clone(),
            staging.path().to_owned(),
            test_filesystem_host(),
            test_filesystem_scope(),
        ));
        let runtime = kernel::test_support::runtime();
        let compilation = kernel::Compilation::default();
        for (head, baseline, mounts, expected) in [
            (None, None, vec![], "empty"),
            (
                Some(KERNEL_BLOCKS[1].0.parse().unwrap()),
                Some(&baseline),
                vec![],
                "source",
            ),
            (None, Some(&baseline), obstructed, "obstruction"),
            (
                None,
                None,
                vec![baseline.clone(); cell::mount::MAX_MOUNT_OPERATIONS + 1],
                "limit",
            ),
        ] {
            let boot = prepare_root(
                head.map(|cid| Head { cid }),
                baseline,
                &runtime,
                &compilation,
                &mounts,
                &client,
                Prewarm::Existing(&tree),
                &mut PinSet::default(),
                &mut cancel,
            )
            .await
            .unwrap_err();
            let standalone =
                crate::image::compose_root(head, baseline, &mounts, &boot_client, &mut cancel)
                    .await
                    .unwrap_err();
            for error in [&boot, &standalone] {
                assert_eq!(
                    classify_failure(error),
                    FailureClass::Permanent,
                    "{expected}: {error:#}"
                );
                match expected {
                    "empty" => assert!(error.downcast_ref::<PermanentPreparationError>().is_some()),
                    "obstruction" => assert!(matches!(
                        error.downcast_ref::<CompositionError>(),
                        Some(CompositionError::TargetObstruction { .. })
                    )),
                    "limit" => assert!(matches!(
                        error.downcast_ref::<CompositionError>(),
                        Some(CompositionError::ResourceLimit(_))
                    )),
                    "source" => assert!(error.downcast_ref::<CompositionError>().is_some()),
                    _ => unreachable!(),
                }
            }
        }
        assert_eq!(*tree.root_cid(), OLD_ROOT.parse::<Cid>().unwrap());
        shutdown.send(()).unwrap();
        server.await.unwrap();
    }

    #[tokio::test]
    async fn standalone_preparation_cancellation_preserves_native_worker() {
        let (client, _, mut requests, shutdown, server) = recording_kubo().await;
        let client = crate::ipfs::BootClient::one_attempt(client, OPERATION_TIMEOUT);
        let compilation = kernel::Compilation::default();
        let (release, native) = compilation.hold_for_test().await;
        let (cancel, mut receiver) = watch::channel(false);
        let preparation = tokio::spawn(async move {
            crate::image::prepare_kernel(
                ROOT.parse().unwrap(),
                &client,
                kernel::test_support::runtime(),
                &compilation,
                &mut receiver,
            )
            .await
        });
        for _ in 0..4 {
            assert!(next_request(&mut requests)
                .await
                .starts_with("/api/v0/block/get"));
        }
        tokio::task::yield_now().await;
        assert!(!preparation.is_finished());
        cancel.send_replace(true);
        let error = tokio::time::timeout(Duration::from_secs(2), preparation)
            .await
            .unwrap()
            .unwrap()
            .unwrap_err();
        assert!(format!("{error:#}").contains("cancelled"), "{error:#}");
        assert!(!native.is_finished());
        release.send(()).unwrap();
        native.await.unwrap();
        shutdown.send(()).unwrap();
        server.await.unwrap();
    }

    #[tokio::test]
    async fn installed_baseline_is_below_head_and_frozen_operations() {
        let baseline = frozen_root(COMPOSED_DIRECTORY_BLOCKS[0].0).await;
        let head = Head {
            cid: COMPOSED_DIRECTORY_BLOCKS[1].0.parse().unwrap(),
        };
        let (client, _, _requests, shutdown, server) = recording_kubo().await;
        let staging = tempfile::tempdir().unwrap();
        let tree = Arc::new(CidTree::new(
            OLD_ROOT.parse().unwrap(),
            client.clone(),
            staging.path().to_owned(),
            test_filesystem_host(),
            test_filesystem_scope(),
        ));
        let runtime = kernel::test_support::runtime();
        let compilation = kernel::Compilation::default();
        let (_cancel, mut cancel) = watch::channel(false);
        for (mounts, expected) in [
            (Vec::new(), COMPOSED_DIRECTORY_BLOCKS[2].0),
            (vec![baseline.clone()], COMPOSED_DIRECTORY_BLOCKS[0].0),
        ] {
            let prepared = prepare_root(
                Some(head.clone()),
                Some(&baseline),
                &runtime,
                &compilation,
                &mounts,
                &client,
                Prewarm::Existing(&tree),
                &mut PinSet::default(),
                &mut cancel,
            )
            .await
            .unwrap();
            let boot_client =
                crate::ipfs::BootClient::one_attempt(client.clone(), OPERATION_TIMEOUT);
            let standalone_root = crate::image::compose_root(
                Some(head.cid),
                Some(&baseline),
                &mounts,
                &boot_client,
                &mut cancel,
            )
            .await
            .unwrap();
            let standalone_kernel = crate::image::prepare_kernel(
                standalone_root,
                &boot_client,
                runtime.clone(),
                &compilation,
                &mut cancel,
            )
            .await
            .unwrap();
            assert_eq!(standalone_root, prepared.effective);
            assert_eq!(
                standalone_kernel.source_file(),
                prepared.kernel.source_file()
            );
            assert_eq!(
                standalone_kernel.runtime_cid(),
                prepared.kernel.runtime_cid()
            );
            assert_eq!(prepared.effective, expected.parse::<Cid>().unwrap());
            assert_eq!(prepared.kernel.effective_root(), prepared.effective);
            assert_eq!(
                prepared.kernel.source_file(),
                KERNEL_BLOCKS[1].0.parse::<Cid>().unwrap()
            );
            assert_eq!(
                prepared.kernel.runtime_cid(),
                kernel::runtime_cid(KERNEL_BLOCKS[1].1)
            );
        }
        assert_eq!(*tree.root_cid(), OLD_ROOT.parse::<Cid>().unwrap());
        shutdown.send(()).unwrap();
        server.await.unwrap();
    }

    #[tokio::test]
    async fn head_inherits_missing_kernel_but_invalid_selected_kernel_never_falls_back() {
        let baseline = frozen_root(ROOT).await;
        let (client, _, _requests, shutdown, server) = recording_kubo().await;
        let staging = tempfile::tempdir().unwrap();
        let tree = Arc::new(CidTree::new(
            OLD_ROOT.parse().unwrap(),
            client.clone(),
            staging.path().to_owned(),
            test_filesystem_host(),
            test_filesystem_scope(),
        ));
        let runtime = kernel::test_support::runtime();
        let compilation = kernel::Compilation::default();
        let (_cancel, mut cancel) = watch::channel(false);
        let inherited = prepare_root(
            Some(Head {
                cid: kernel::test_support::EMPTY_ROOT.parse().unwrap(),
            }),
            Some(&baseline),
            &runtime,
            &compilation,
            &[],
            &client,
            Prewarm::Existing(&tree),
            &mut PinSet::default(),
            &mut cancel,
        )
        .await
        .unwrap();
        assert_eq!(inherited.effective, ROOT.parse::<Cid>().unwrap());
        let mut pins = PinSet::default();
        let invalid = prepare_root(
            Some(Head {
                cid: kernel::test_support::INVALID_ROOT.parse().unwrap(),
            }),
            Some(&baseline),
            &runtime,
            &compilation,
            &[],
            &client,
            Prewarm::Existing(&tree),
            &mut pins,
            &mut cancel,
        )
        .await
        .unwrap_err();
        assert_eq!(classify_failure(&invalid), FailureClass::Permanent);
        assert!(invalid
            .downcast_ref::<cell::proc::KernelValidationError>()
            .is_some());
        assert!(pins.contains(&kernel::test_support::INVALID_ROOT.parse().unwrap()));
        assert_eq!(*tree.root_cid(), OLD_ROOT.parse::<Cid>().unwrap());
        shutdown.send(()).unwrap();
        server.await.unwrap();
    }

    #[tokio::test]
    async fn cancelled_preparation_does_not_wait_for_obsolete_native_compilation() {
        let (client, _, mut requests, shutdown, server) = recording_kubo().await;
        let staging = tempfile::tempdir().unwrap();
        let tree = Arc::new(CidTree::new(
            OLD_ROOT.parse().unwrap(),
            client.clone(),
            staging.path().to_owned(),
            test_filesystem_host(),
            test_filesystem_scope(),
        ));
        let compilation = kernel::Compilation::default();
        let (release, native) = compilation.hold_for_test().await;
        let (cancel, receiver) = watch::channel(false);
        let preparation = tokio::spawn(prepare_root_owned(
            Head {
                cid: ROOT.parse().unwrap(),
            },
            None,
            kernel::test_support::runtime(),
            compilation,
            Vec::new(),
            client,
            tree,
            PinSet::default(),
            receiver,
        ));
        for expected in PREPARATION_REQUESTS {
            assert!(next_request(&mut requests).await.starts_with(expected));
        }
        tokio::task::yield_now().await;
        assert!(!preparation.is_finished());
        cancel.send_replace(true);
        let output = tokio::time::timeout(Duration::from_secs(2), preparation)
            .await
            .unwrap()
            .unwrap();
        assert!(output.result.is_err());
        assert!(output.pins.contains(&ROOT.parse().unwrap()));
        assert!(!native.is_finished());
        release.send(()).unwrap();
        native.await.unwrap();
        shutdown.send(()).unwrap();
        server.await.unwrap();
    }

    #[tokio::test]
    async fn ready_speculation_is_authority_neutral_and_reused_after_revocation() {
        let (client, calls, mut requests, shutdown_tx, server) = recording_kubo().await;
        let staging = tempfile::tempdir().unwrap();
        let (_source_tx, source_rx) = mpsc::channel(2);
        let (_candidate_tx, candidate_rx) = watch::channel(None);
        let (mut deployment, tree, mut epoch_observer) =
            test_deployment(client, staging.path().to_owned(), source_rx, candidate_rx);
        let (running, result_tx, terminate_rx) = test_running_generation();
        let head = Head {
            cid: ROOT.parse().unwrap(),
        };

        deployment.observe_candidate(Some(head.bytes()));
        let task = match deployment.speculation.take().unwrap() {
            Speculation::InFlight(task) => task,
            _ => panic!("candidate did not start speculative preparation"),
        };
        let candidate = task.candidate.clone();
        let output = task.task.await.unwrap();
        deployment.finish_speculative_preparation(candidate, output, true, None);
        assert!(matches!(
            deployment.speculation.as_ref(),
            Some(Speculation::Ready { .. })
        ));
        for expected in PREPARATION_REQUESTS {
            let request = next_request(&mut requests).await;
            assert!(request.starts_with(expected), "{request}");
        }
        assert_eq!(epoch_observer.borrow().seq, 0);
        assert_eq!(
            epoch_observer.borrow().root,
            Some(OLD_ROOT.parse().unwrap())
        );
        assert_eq!(tree.root_cid().as_ref(), &OLD_ROOT.parse::<Cid>().unwrap());
        assert!(!terminate_rx.has_changed().unwrap());

        let outcome = {
            let transition = deployment.replace(running, vec![Update::Head(head.clone())], None);
            tokio::pin!(transition);
            tokio::select! {
                _ = &mut transition => panic!("replacement activated before teardown"),
                changed = epoch_observer.changed() => changed.unwrap(),
            }
            assert_eq!(epoch_observer.borrow().seq, 1);
            assert_eq!(epoch_observer.borrow().root, None);
            assert_eq!(tree.root_cid().as_ref(), &OLD_ROOT.parse::<Cid>().unwrap());
            assert!(terminate_rx.has_changed().unwrap());

            result_tx.send(Ok(kernel::Outcome::Terminated)).unwrap();
            transition.await.unwrap()
        };

        assert!(matches!(outcome, Outcome::Replaced { new_epoch: 1, .. }));
        assert_eq!(tree.root_cid().as_ref(), &ROOT.parse::<Cid>().unwrap());
        assert_eq!(deployment.current_epoch().root, Some(ROOT.parse().unwrap()));
        assert_eq!(calls.load(Ordering::SeqCst), 9);
        shutdown_tx.send(()).unwrap();
        server.await.unwrap();
    }

    #[tokio::test]
    async fn ready_speculation_expires_and_releases_pins_without_changing_authority() {
        let (client, calls, mut requests, shutdown_tx, server) = recording_kubo().await;
        let staging = tempfile::tempdir().unwrap();
        let (_source_tx, source_rx) = mpsc::channel(2);
        let (candidate_tx, candidate_rx) = watch::channel(None);
        let (mut deployment, tree, epoch_observer) =
            test_deployment(client, staging.path().to_owned(), source_rx, candidate_rx);
        let ready_gate = authority::KernelReadyGate::new(epoch_observer.clone());
        ready_gate.bind_generation(0);
        ready_gate.kernel_ready().unwrap();
        let (running, result_tx, terminate_rx) = test_running_generation();
        let head = Head {
            cid: ROOT.parse().unwrap(),
        };

        candidate_tx.send_replace(Some(head.bytes()));
        deployment.observe_candidate(Some(head.bytes()));
        let task = match deployment.speculation.take().unwrap() {
            Speculation::InFlight(task) => task,
            _ => panic!("candidate did not start speculative preparation"),
        };
        let candidate = task.candidate.clone();
        let output = task.task.await.unwrap();
        deployment.finish_speculative_preparation(candidate, output, true, None);
        assert!(matches!(
            deployment.speculation.as_ref(),
            Some(Speculation::Ready { .. })
        ));
        for expected in PREPARATION_REQUESTS {
            let request = next_request(&mut requests).await;
            assert!(request.starts_with(expected), "{request}");
        }

        tokio::time::pause();
        let outcome = {
            let transition = deployment.await_generation(running);
            tokio::pin!(transition);
            tokio::select! {
                biased;
                _ = &mut transition => panic!("candidate expiry changed kernel outcome"),
                _ = tokio::task::yield_now() => {}
            }
            tokio::time::advance(SPECULATIVE_RETENTION).await;
            tokio::time::resume();
            let unpin = tokio::select! {
                _ = &mut transition => panic!("candidate expiry changed kernel outcome"),
                request = tokio::time::timeout(Duration::from_secs(2), requests.recv()) => {
                    request
                        .expect("timed out waiting for speculative pin release")
                        .expect("Kubo request server stopped")
                },
            };
            assert!(unpin.starts_with("/api/v0/pin/rm"), "{unpin}");
            assert!(unpin.contains(ROOT), "{unpin}");
            assert_eq!(epoch_observer.borrow().seq, 0);
            assert_eq!(
                epoch_observer.borrow().root,
                Some(OLD_ROOT.parse().unwrap())
            );
            assert_eq!(tree.root_cid().as_ref(), &OLD_ROOT.parse::<Cid>().unwrap());
            assert!(!terminate_rx.has_changed().unwrap());
            assert!(ready_gate.is_ready());

            result_tx.send(Ok(kernel::Outcome::Exited(0))).unwrap();
            transition.await.unwrap()
        };

        assert!(matches!(outcome, Outcome::Authoritative { epoch: 0, .. }));
        assert_eq!(calls.load(Ordering::SeqCst), 10);

        let (running, _result_tx, _terminate_rx) = test_running_generation();
        {
            let transition = deployment.await_generation(running);
            tokio::pin!(transition);
            tokio::select! {
                biased;
                _ = &mut transition => panic!("unchanged advisory changed kernel outcome"),
                _ = tokio::task::yield_now() => {}
            }
        }
        assert!(deployment.speculation.is_none());
        assert_eq!(calls.load(Ordering::SeqCst), 10);

        shutdown_tx.send(()).unwrap();
        server.await.unwrap();
    }

    #[tokio::test]
    async fn matching_in_flight_speculation_continues_during_teardown() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let server_calls = calls.clone();
        let (request_tx, mut requests) = mpsc::unbounded_channel();
        let (release_first_tx, release_first_rx) = oneshot::channel();
        let server = tokio::spawn(async move {
            let mut release_first_rx = Some(release_first_rx);
            for index in 1..=9 {
                let (mut stream, _) = listener.accept().await.unwrap();
                let request = read_request(&mut stream).await;
                let path = request
                    .lines()
                    .next()
                    .and_then(|line| line.split_whitespace().nth(1))
                    .unwrap()
                    .to_owned();
                server_calls.fetch_add(1, Ordering::SeqCst);
                request_tx.send(path.clone()).unwrap();
                if index == 1 {
                    release_first_rx.take().unwrap().await.unwrap();
                }
                respond(&mut stream, &request).await;
            }
        });
        let client = crate::ipfs::HttpClient::new(format!("http://{address}"));
        let staging = tempfile::tempdir().unwrap();
        let (source_tx, source_rx) = mpsc::channel(2);
        let (candidate_tx, candidate_rx) = watch::channel(None);
        let (mut deployment, tree, mut epoch_observer) =
            test_deployment(client, staging.path().to_owned(), source_rx, candidate_rx);
        let (running, result_tx, terminate_rx) = test_running_generation();
        let head = Head {
            cid: ROOT.parse().unwrap(),
        };

        let outcome = {
            let transition = deployment.await_generation(running);
            tokio::pin!(transition);
            candidate_tx.send_replace(Some(head.bytes()));
            let first_pin = tokio::select! {
                _ = &mut transition => panic!("candidate changed kernel outcome"),
                request = next_request(&mut requests) => request,
            };
            assert!(first_pin.starts_with("/api/v0/pin/add"), "{first_pin}");

            source_tx
                .send(SourceMessage::Update(Update::Head(head)))
                .await
                .unwrap();
            tokio::select! {
                _ = &mut transition => panic!("replacement activated before preparation and teardown"),
                changed = epoch_observer.changed() => changed.unwrap(),
            }
            assert_eq!(epoch_observer.borrow().root, None);
            assert_eq!(tree.root_cid().as_ref(), &OLD_ROOT.parse::<Cid>().unwrap());
            assert!(terminate_rx.has_changed().unwrap());

            release_first_tx.send(()).unwrap();
            for expected in &PREPARATION_REQUESTS[1..] {
                let request = tokio::select! {
                    _ = &mut transition => panic!("replacement activated before teardown"),
                    request = next_request(&mut requests) => request,
                };
                assert!(request.starts_with(expected), "{request}");
            }
            assert_eq!(tree.root_cid().as_ref(), &OLD_ROOT.parse::<Cid>().unwrap());
            result_tx.send(Ok(kernel::Outcome::Terminated)).unwrap();
            transition.await.unwrap()
        };

        assert!(matches!(outcome, Outcome::Replaced { new_epoch: 1, .. }));
        assert_eq!(tree.root_cid().as_ref(), &ROOT.parse::<Cid>().unwrap());
        assert_eq!(calls.load(Ordering::SeqCst), 9);
        server.await.unwrap();
    }

    #[tokio::test]
    async fn malformed_and_closed_advisory_feed_do_not_affect_authoritative_following() {
        let (client, calls, mut requests, shutdown_tx, server) = recording_kubo().await;
        let staging = tempfile::tempdir().unwrap();
        let (source_tx, source_rx) = mpsc::channel(2);
        let (candidate_tx, candidate_rx) = watch::channel(None);
        let (mut deployment, tree, epoch_observer) =
            test_deployment(client, staging.path().to_owned(), source_rx, candidate_rx);
        let (running, result_tx, terminate_rx) = test_running_generation();
        let head = Head {
            cid: ROOT.parse().unwrap(),
        };

        let outcome = {
            let transition = deployment.await_generation(running);
            tokio::pin!(transition);
            candidate_tx.send_replace(Some(b"not-a-cid".to_vec()));
            tokio::select! {
                _ = &mut transition => panic!("malformed candidate changed kernel outcome"),
                _ = tokio::time::sleep(Duration::from_millis(20)) => {}
            }
            drop(candidate_tx);
            tokio::select! {
                _ = &mut transition => panic!("closed advisory feed changed kernel outcome"),
                _ = tokio::time::sleep(Duration::from_millis(20)) => {}
            }
            assert_eq!(epoch_observer.borrow().seq, 0);
            assert_eq!(
                epoch_observer.borrow().root,
                Some(OLD_ROOT.parse().unwrap())
            );
            assert_eq!(tree.root_cid().as_ref(), &OLD_ROOT.parse::<Cid>().unwrap());
            assert!(!terminate_rx.has_changed().unwrap());
            assert_eq!(calls.load(Ordering::SeqCst), 0);

            source_tx
                .send(SourceMessage::Update(Update::Head(head)))
                .await
                .unwrap();
            for expected in PREPARATION_REQUESTS {
                let request = tokio::select! {
                    _ = &mut transition => panic!("replacement activated before teardown"),
                    request = next_request(&mut requests) => request,
                };
                assert!(request.starts_with(expected), "{request}");
            }
            result_tx.send(Ok(kernel::Outcome::Terminated)).unwrap();
            transition.await.unwrap()
        };

        assert!(matches!(outcome, Outcome::Replaced { new_epoch: 1, .. }));
        assert_eq!(tree.root_cid().as_ref(), &ROOT.parse::<Cid>().unwrap());
        shutdown_tx.send(()).unwrap();
        server.await.unwrap();
    }

    #[tokio::test]
    async fn authoritative_mismatch_releases_speculation_and_prepares_the_selected_head() {
        let (client, calls, mut requests, shutdown_tx, server) = recording_kubo().await;
        let staging = tempfile::tempdir().unwrap();
        let (source_tx, source_rx) = mpsc::channel(2);
        let (candidate_tx, candidate_rx) = watch::channel(None);
        let (mut deployment, tree, mut epoch_observer) =
            test_deployment(client, staging.path().to_owned(), source_rx, candidate_rx);
        let (running, result_tx, terminate_rx) = test_running_generation();
        let speculative = Head {
            cid: "bafybeiaawn3jm4eczk7dirhcgbvqacymijqipbx4z33rumc2ab6aggvm5u"
                .parse()
                .unwrap(),
        };
        let authoritative = Head {
            cid: ROOT.parse().unwrap(),
        };

        let outcome = {
            let transition = deployment.await_generation(running);
            tokio::pin!(transition);
            candidate_tx.send_replace(Some(speculative.bytes()));
            for _ in 0..9 {
                tokio::select! {
                    _ = &mut transition => panic!("speculation changed kernel outcome"),
                    _ = next_request(&mut requests) => {}
                }
            }
            tokio::select! {
                _ = &mut transition => panic!("speculation changed kernel outcome"),
                _ = tokio::time::sleep(Duration::from_millis(20)) => {}
            }

            source_tx
                .send(SourceMessage::Update(Update::Head(authoritative)))
                .await
                .unwrap();
            tokio::select! {
                _ = &mut transition => panic!("replacement activated before teardown"),
                changed = epoch_observer.changed() => changed.unwrap(),
            }
            assert_eq!(epoch_observer.borrow().root, None);
            assert_eq!(tree.root_cid().as_ref(), &OLD_ROOT.parse::<Cid>().unwrap());
            assert!(terminate_rx.has_changed().unwrap());

            let unpin = tokio::select! {
                _ = &mut transition => panic!("replacement activated before teardown"),
                request = next_request(&mut requests) => request,
            };
            assert!(unpin.starts_with("/api/v0/pin/rm"), "{unpin}");
            assert!(unpin.contains(&speculative.cid.to_string()), "{unpin}");
            for expected in PREPARATION_REQUESTS {
                let request = tokio::select! {
                    _ = &mut transition => panic!("replacement activated before teardown"),
                    request = next_request(&mut requests) => request,
                };
                assert!(request.starts_with(expected), "{request}");
                if expected == "/api/v0/pin/add" || expected == "/api/v0/ls" {
                    assert!(request.contains(ROOT), "{request}");
                }
            }
            result_tx.send(Ok(kernel::Outcome::Terminated)).unwrap();
            transition.await.unwrap()
        };

        assert!(matches!(outcome, Outcome::Replaced { new_epoch: 1, .. }));
        assert_eq!(tree.root_cid().as_ref(), &ROOT.parse::<Cid>().unwrap());
        assert_eq!(calls.load(Ordering::SeqCst), 19);
        shutdown_tx.send(()).unwrap();
        server.await.unwrap();
    }

    #[tokio::test]
    async fn speculative_failure_releases_pins_and_authority_retries_from_scratch() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let server_calls = calls.clone();
        let (request_tx, mut requests) = mpsc::unbounded_channel();
        let server = tokio::spawn(async move {
            for index in 1..=12 {
                let (mut stream, _) = listener.accept().await.unwrap();
                let request = read_request(&mut stream).await;
                let path = request
                    .lines()
                    .next()
                    .and_then(|line| line.split_whitespace().nth(1))
                    .unwrap()
                    .to_owned();
                server_calls.fetch_add(1, Ordering::SeqCst);
                request_tx.send(path.clone()).unwrap();
                if index == 2 {
                    stream
                        .write_all(b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 4\r\nConnection: close\r\n\r\nbusy")
                        .await
                        .unwrap();
                } else {
                    respond(&mut stream, &request).await;
                }
            }
        });
        let client = crate::ipfs::HttpClient::new(format!("http://{address}"));
        let staging = tempfile::tempdir().unwrap();
        let (source_tx, source_rx) = mpsc::channel(2);
        let (candidate_tx, candidate_rx) = watch::channel(None);
        let (mut deployment, tree, epoch_observer) =
            test_deployment(client, staging.path().to_owned(), source_rx, candidate_rx);
        let (running, result_tx, terminate_rx) = test_running_generation();
        let head = Head {
            cid: ROOT.parse().unwrap(),
        };

        let outcome = {
            let transition = deployment.await_generation(running);
            tokio::pin!(transition);
            candidate_tx.send_replace(Some(head.bytes()));
            for expected in ["/api/v0/pin/add", "/api/v0/block/get", "/api/v0/pin/rm"] {
                let request = tokio::select! {
                    _ = &mut transition => panic!("speculative failure changed kernel outcome"),
                    request = next_request(&mut requests) => request,
                };
                assert!(request.starts_with(expected), "{request}");
            }
            assert_eq!(epoch_observer.borrow().seq, 0);
            assert_eq!(
                epoch_observer.borrow().root,
                Some(OLD_ROOT.parse().unwrap())
            );
            assert_eq!(tree.root_cid().as_ref(), &OLD_ROOT.parse::<Cid>().unwrap());
            assert!(!terminate_rx.has_changed().unwrap());

            source_tx
                .send(SourceMessage::Update(Update::Head(head)))
                .await
                .unwrap();
            for expected in PREPARATION_REQUESTS {
                let request = tokio::select! {
                    _ = &mut transition => panic!("replacement activated before teardown"),
                    request = next_request(&mut requests) => request,
                };
                assert!(request.starts_with(expected), "{request}");
            }
            result_tx.send(Ok(kernel::Outcome::Terminated)).unwrap();
            transition.await.unwrap()
        };

        assert!(matches!(outcome, Outcome::Replaced { new_epoch: 1, .. }));
        assert_eq!(tree.root_cid().as_ref(), &ROOT.parse::<Cid>().unwrap());
        assert_eq!(calls.load(Ordering::SeqCst), 12);
        server.await.unwrap();
    }

    #[tokio::test]
    async fn newest_candidate_cancels_and_cleans_old_work_before_starting() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let server_calls = calls.clone();
        let (request_tx, mut requests) = mpsc::unbounded_channel();
        let (release_first_tx, release_first_rx) = oneshot::channel();
        let server = tokio::spawn(async move {
            let mut release_first_rx = Some(release_first_rx);
            for index in 1..=12 {
                let (mut stream, _) = listener.accept().await.unwrap();
                let request = read_request(&mut stream).await;
                let path = request
                    .lines()
                    .next()
                    .and_then(|line| line.split_whitespace().nth(1))
                    .unwrap()
                    .to_owned();
                server_calls.fetch_add(1, Ordering::SeqCst);
                request_tx.send(path.clone()).unwrap();
                if index == 1 {
                    release_first_rx.take().unwrap().await.unwrap();
                }
                respond(&mut stream, &request).await;
            }
        });
        let client = crate::ipfs::HttpClient::new(format!("http://{address}"));
        let staging = tempfile::tempdir().unwrap();
        let (_source_tx, source_rx) = mpsc::channel(2);
        let (candidate_tx, candidate_rx) = watch::channel(None);
        let (mut deployment, tree, epoch_observer) =
            test_deployment(client, staging.path().to_owned(), source_rx, candidate_rx);
        let (running, result_tx, terminate_rx) = test_running_generation();
        let first = Head {
            cid: "bafybeiaawn3jm4eczk7dirhcgbvqacymijqipbx4z33rumc2ab6aggvm5u"
                .parse()
                .unwrap(),
        };
        let intermediate = Head {
            cid: "bafybeiciecrmnoeowo36v5qcgxveqq72i4ap4daubts3g2uxj7ad7lripy"
                .parse()
                .unwrap(),
        };
        let newest = Head {
            cid: ROOT.parse().unwrap(),
        };

        let outcome = {
            let transition = deployment.await_generation(running);
            tokio::pin!(transition);
            candidate_tx.send_replace(Some(first.bytes()));
            let first_pin = tokio::select! {
                _ = &mut transition => panic!("candidate changed kernel outcome"),
                request = next_request(&mut requests) => request,
            };
            assert!(first_pin.starts_with("/api/v0/pin/add"), "{first_pin}");
            assert!(first_pin.contains(&first.cid.to_string()), "{first_pin}");

            candidate_tx.send_replace(Some(intermediate.bytes()));
            candidate_tx.send_replace(Some(newest.bytes()));
            release_first_tx.send(()).unwrap();
            let old_unpin = tokio::select! {
                _ = &mut transition => panic!("candidate changed kernel outcome"),
                request = next_request(&mut requests) => request,
            };
            assert!(old_unpin.starts_with("/api/v0/pin/rm"), "{old_unpin}");
            assert!(old_unpin.contains(&first.cid.to_string()), "{old_unpin}");
            for expected in PREPARATION_REQUESTS {
                let request = tokio::select! {
                    _ = &mut transition => panic!("candidate changed kernel outcome"),
                    request = next_request(&mut requests) => request,
                };
                assert!(request.starts_with(expected), "{request}");
                if expected == "/api/v0/pin/add" || expected == "/api/v0/ls" {
                    assert!(request.contains(ROOT), "{request}");
                }
            }
            tokio::select! {
                _ = &mut transition => panic!("candidate changed kernel outcome"),
                _ = tokio::time::sleep(Duration::from_millis(20)) => {}
            }
            assert_eq!(epoch_observer.borrow().seq, 0);
            assert_eq!(
                epoch_observer.borrow().root,
                Some(OLD_ROOT.parse().unwrap())
            );
            assert_eq!(tree.root_cid().as_ref(), &OLD_ROOT.parse::<Cid>().unwrap());
            assert!(!terminate_rx.has_changed().unwrap());

            result_tx.send(Ok(kernel::Outcome::Exited(0))).unwrap();
            transition.await.unwrap()
        };

        assert!(matches!(outcome, Outcome::Authoritative { epoch: 0, .. }));
        let newest_unpin = next_request(&mut requests).await;
        assert!(newest_unpin.starts_with("/api/v0/pin/rm"), "{newest_unpin}");
        assert!(newest_unpin.contains(ROOT), "{newest_unpin}");
        assert_eq!(calls.load(Ordering::SeqCst), 12);
        server.await.unwrap();
    }

    #[tokio::test]
    async fn old_generation_stops_before_cid_tree_root_swap() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (prepared_tx, prepared_rx) = oneshot::channel();
        let server = tokio::spawn(async move {
            for expected in PREPARATION_REQUESTS {
                let (mut stream, _) = listener.accept().await.unwrap();
                let request = read_request(&mut stream).await;
                assert!(
                    request.lines().next().unwrap().contains(expected),
                    "{request}"
                );
                respond(&mut stream, &request).await;
            }
            prepared_tx.send(()).unwrap();
        });
        let client = crate::ipfs::HttpClient::new(format!("http://{address}"));
        let staging = tempfile::tempdir().unwrap();
        let tree = Arc::new(CidTree::new(
            OLD_ROOT.parse().unwrap(),
            client.clone(),
            staging.path().to_owned(),
            test_filesystem_host(),
            test_filesystem_scope(),
        ));
        let (epoch_tx, epoch_rx) = watch::channel(Epoch {
            seq: 0,
            head: Vec::new(),
            root: Some(OLD_ROOT.parse().unwrap()),
        });
        let (_source_tx, source_rx) = mpsc::channel(1);
        let mut deployment = Deployment {
            installed_baseline: None,
            runtime_engine: kernel::test_support::runtime(),
            active_kernel: kernel::test_support::prepared(ROOT),
            epoch_tx,
            epoch_rx,
            epoch_seq: 0,
            frozen_mounts: Vec::new(),
            frozen_pins: PinSet::default(),
            active_pins: PinSet::default(),
            retained_pins: Vec::new(),
            ipfs_client: client,
            cid_tree: tree.clone(),
            source_rx: Some(source_rx),
            source_task: None,
            candidate_rx: None,
            candidate_initialized: false,
            compilation: kernel::Compilation::default(),
            speculation: None,
        };
        let (terminate_tx, terminate_rx) = watch::channel(());
        let (result_tx, result_rx) = oneshot::channel();
        let running = RunningGeneration {
            admission: None,
            intended_seq: 0,
            terminate_tx,
            result_rx,
            filesystem_completion: completed_filesystem_scope(),
        };
        let head = Head {
            cid: ROOT.parse().unwrap(),
        };
        let outcome = {
            let replacement = deployment.replace(running, vec![Update::Head(head)], None);
            tokio::pin!(replacement);

            tokio::select! {
                _ = &mut replacement => panic!("replacement activated before preparation checkpoint"),
                prepared = prepared_rx => prepared.unwrap(),
            }
            assert!(terminate_rx.has_changed().unwrap());
            assert_eq!(tree.root_cid().as_ref(), &OLD_ROOT.parse::<Cid>().unwrap());

            result_tx.send(Ok(kernel::Outcome::Terminated)).unwrap();
            replacement.await.unwrap()
        };
        assert!(matches!(
            outcome,
            Outcome::Replaced {
                old_epoch: 0,
                new_epoch: 1,
                ..
            }
        ));
        assert_eq!(tree.root_cid().as_ref(), &ROOT.parse::<Cid>().unwrap());
        assert_eq!(deployment.current_epoch().root, Some(ROOT.parse().unwrap()));
        server.await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn lost_generation_result_waits_for_filesystem_completion_before_root_swap() {
        tokio::time::timeout(Duration::from_secs(10), async {
            assert_lost_generation_result_waits_for_filesystem_completion(false).await;
            assert_lost_generation_result_waits_for_filesystem_completion(true).await;
        })
        .await
        .expect("generation filesystem completion regression watchdog expired");
    }

    async fn assert_lost_generation_result_waits_for_filesystem_completion(
        preconsume_result: bool,
    ) {
        let client = crate::ipfs::HttpClient::new("http://127.0.0.1:1".to_owned());
        let staging = tempfile::tempdir().unwrap();
        let stub = staging.path().join("dir-old-generation");
        std::fs::create_dir(&stub).unwrap();
        std::fs::write(stub.join("entry"), b"stale").unwrap();
        let tree = Arc::new(CidTree::new(
            OLD_ROOT.parse().unwrap(),
            client.clone(),
            staging.path().to_owned(),
            test_filesystem_host(),
            test_filesystem_scope(),
        ));
        let (epoch_tx, epoch_rx) = watch::channel(Epoch {
            seq: 0,
            head: Vec::new(),
            root: Some(OLD_ROOT.parse().unwrap()),
        });
        let (_source_tx, source_rx) = mpsc::channel(1);
        let head = Head {
            cid: ROOT.parse().unwrap(),
        };
        let mut deployment = Deployment {
            installed_baseline: None,
            runtime_engine: kernel::test_support::runtime(),
            active_kernel: kernel::test_support::prepared(ROOT),
            epoch_tx,
            epoch_rx,
            epoch_seq: 0,
            frozen_mounts: Vec::new(),
            frozen_pins: PinSet::default(),
            active_pins: PinSet::default(),
            retained_pins: Vec::new(),
            ipfs_client: client,
            cid_tree: tree.clone(),
            source_rx: Some(source_rx),
            source_task: None,
            candidate_rx: None,
            candidate_initialized: false,
            compilation: kernel::Compilation::default(),
            speculation: Some(Speculation::Ready {
                prepared: PreparedRoot {
                    head: Some(head.clone()),
                    effective: head.cid,
                    kernel: kernel::test_support::prepared(&(head.cid).to_string()),
                    pins: PinSet::default(),
                },
                expires_at: tokio::time::Instant::now() + SPECULATIVE_RETENTION,
            }),
        };
        let (filesystem_scope, filesystem_owner, filesystem_finalizer, filesystem_completion) =
            test_filesystem_host().try_filesystem_scope().unwrap();
        let private = tempfile::tempdir().unwrap();
        let private_path = private.path().to_owned();
        let (cleanup_entered_tx, cleanup_entered_rx) = oneshot::channel();
        let (cleanup_release_tx, cleanup_release_rx) = std::sync::mpsc::channel();
        filesystem_scope
            .own_cleanup(cache::FilesystemOperation::ProcessCleanup, move || {
                let _ = cleanup_entered_tx.send(());
                let _ = cleanup_release_rx.recv();
                private.close()
            })
            .unwrap();
        drop(filesystem_scope);
        drop(filesystem_owner);
        let finalizer_task = tokio::spawn(async move { filesystem_finalizer.run().await });

        let (terminate_tx, terminate_rx) = watch::channel(());
        let (result_tx, result_rx) = oneshot::channel();
        let mut running = RunningGeneration {
            admission: None,
            intended_seq: 0,
            terminate_tx,
            result_rx,
            filesystem_completion,
        };
        drop(result_tx);
        let completed_result = if preconsume_result {
            Some((&mut running.result_rx).await)
        } else {
            None
        };
        let outcome = {
            let replacement =
                deployment.replace(running, vec![Update::Head(head)], completed_result);
            tokio::pin!(replacement);

            cleanup_entered_rx.await.unwrap();

            assert!(matches!(
                futures::poll!(&mut replacement),
                std::task::Poll::Pending
            ));
            assert!(terminate_rx.has_changed().unwrap());
            assert_eq!(tree.root_cid().as_ref(), &OLD_ROOT.parse::<Cid>().unwrap());
            assert!(
                stub.exists(),
                "stub cleanup ran before filesystem completion"
            );
            assert!(private_path.exists());

            cleanup_release_tx.send(()).unwrap();
            replacement.await.unwrap()
        };
        finalizer_task.await.unwrap().unwrap();
        assert!(matches!(
            outcome,
            Outcome::Replaced {
                old_epoch: 0,
                new_epoch: 1,
                ..
            }
        ));
        assert_eq!(tree.root_cid().as_ref(), &ROOT.parse::<Cid>().unwrap());
        assert_eq!(deployment.current_epoch().root, Some(ROOT.parse().unwrap()));
        assert!(!stub.exists());
        assert!(!private_path.exists());
    }

    #[tokio::test]
    async fn filesystem_cleanup_failure_denies_generation_replacement() {
        let staging = tempfile::tempdir().unwrap();
        let stub = staging.path().join("dir-old-generation");
        std::fs::create_dir(&stub).unwrap();
        let client = crate::ipfs::HttpClient::new("http://127.0.0.1:1".to_owned());
        let (_source_tx, source_rx) = mpsc::channel(1);
        let (_candidate_tx, candidate_rx) = watch::channel(None);
        let (mut deployment, tree, _epoch_rx) =
            test_deployment(client, staging.path().to_owned(), source_rx, candidate_rx);
        let head = Head {
            cid: ROOT.parse().unwrap(),
        };
        deployment.speculation = Some(Speculation::Ready {
            prepared: PreparedRoot {
                head: Some(head.clone()),
                effective: head.cid,
                kernel: kernel::test_support::prepared(&(head.cid).to_string()),
                pins: PinSet::default(),
            },
            expires_at: tokio::time::Instant::now() + SPECULATIVE_RETENTION,
        });

        let (filesystem_scope, filesystem_owner, filesystem_finalizer, filesystem_completion) =
            test_filesystem_host().try_filesystem_scope().unwrap();
        filesystem_scope
            .own_cleanup(cache::FilesystemOperation::ProcessCleanup, || {
                Err(std::io::Error::other("injected cleanup failure"))
            })
            .unwrap();
        drop(filesystem_scope);
        drop(filesystem_owner);
        let finalizer_task = tokio::spawn(async move { filesystem_finalizer.run().await });

        let (terminate_tx, _terminate_rx) = watch::channel(());
        let (result_tx, result_rx) = oneshot::channel();
        let mut running = RunningGeneration {
            admission: None,
            intended_seq: 0,
            terminate_tx,
            result_rx,
            filesystem_completion,
        };
        drop(result_tx);
        let completed_result = Some((&mut running.result_rx).await);

        let error = match deployment
            .replace(running, vec![Update::Head(head)], completed_result)
            .await
        {
            Ok(_) => panic!("cleanup failure authorized generation replacement"),
            Err(error) => error,
        };
        let finalizer_error = finalizer_task.await.unwrap().unwrap_err();

        assert!(error.to_string().contains("filesystem cleanup failed"));
        assert!(finalizer_error
            .to_string()
            .contains("injected cleanup failure"));
        assert_eq!(tree.root_cid().as_ref(), &OLD_ROOT.parse::<Cid>().unwrap());
        assert!(
            stub.exists(),
            "failed cleanup must not authorize stub sweep"
        );
    }

    #[tokio::test]
    async fn invalid_replacement_revokes_then_later_valid_update_recovers() {
        let (client, server) = single_root_kubo().await;
        let staging = tempfile::tempdir().unwrap();
        let tree = Arc::new(CidTree::new(
            OLD_ROOT.parse().unwrap(),
            client.clone(),
            staging.path().to_owned(),
            test_filesystem_host(),
            test_filesystem_scope(),
        ));
        let (epoch_tx, epoch_rx) = watch::channel(Epoch {
            seq: 0,
            head: Vec::new(),
            root: Some(OLD_ROOT.parse().unwrap()),
        });
        let mut epoch_observer = epoch_rx.clone();
        let (source_tx, source_rx) = mpsc::channel(2);
        let mut deployment = Deployment {
            installed_baseline: None,
            runtime_engine: kernel::test_support::runtime(),
            active_kernel: kernel::test_support::prepared(ROOT),
            epoch_tx,
            epoch_rx,
            epoch_seq: 0,
            frozen_mounts: Vec::new(),
            frozen_pins: PinSet::default(),
            active_pins: PinSet::default(),
            retained_pins: Vec::new(),
            ipfs_client: client,
            cid_tree: tree.clone(),
            source_rx: Some(source_rx),
            source_task: None,
            candidate_rx: None,
            candidate_initialized: false,
            compilation: kernel::Compilation::default(),
            speculation: None,
        };
        let (terminate_tx, terminate_rx) = watch::channel(());
        let (result_tx, result_rx) = oneshot::channel();
        let running = RunningGeneration {
            admission: None,
            intended_seq: 0,
            terminate_tx,
            result_rx,
            filesystem_completion: completed_filesystem_scope(),
        };
        let outcome = {
            let replacement = deployment.replace(
                running,
                vec![Update::InvalidHead(InvalidHead {
                    selected: b"bad-head".to_vec(),
                    reason: "not deployable".to_owned(),
                })],
                None,
            );
            tokio::pin!(replacement);

            tokio::select! {
                _ = &mut replacement => panic!("invalid replacement completed instead of waiting for recovery"),
                changed = epoch_observer.changed() => changed.unwrap(),
            }
            assert_eq!(epoch_observer.borrow().seq, 1);
            assert_eq!(epoch_observer.borrow().root, None);
            assert_eq!(tree.root_cid().as_ref(), &OLD_ROOT.parse::<Cid>().unwrap());
            assert!(terminate_rx.has_changed().unwrap());

            let valid = Head {
                cid: ROOT.parse().unwrap(),
            };
            source_tx
                .send(SourceMessage::Update(Update::Head(valid)))
                .await
                .unwrap();
            result_tx.send(Ok(kernel::Outcome::Terminated)).unwrap();
            replacement.await.unwrap()
        };

        assert!(matches!(
            outcome,
            Outcome::Replaced {
                old_epoch: 0,
                new_epoch: 2,
                ..
            }
        ));
        assert_eq!(tree.root_cid().as_ref(), &ROOT.parse::<Cid>().unwrap());
        assert_eq!(deployment.current_epoch().root, Some(ROOT.parse().unwrap()));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn ipns_eol_revokes_generation_and_real_source_recovers_host() {
        use chrono::Utc;
        use libp2p::identity::Keypair;
        use rust_ipns::Record;

        let state = tempfile::tempdir().unwrap();
        let keypair = Keypair::generate_ed25519();
        let name = keypair.public().to_peer_id();
        let root: cid::Cid = ROOT.parse().unwrap();
        let current = Record::new(
            &keypair,
            format!("/ipfs/{root}"),
            Utc::now() + chrono::Duration::seconds(1),
            1,
            Duration::from_millis(1),
        )
        .unwrap()
        .encode()
        .unwrap();
        crate::ipns::RecordStore::follower(state.path(), name)
            .persist(&current)
            .unwrap();
        let recovery = Record::new(
            &keypair,
            format!("/ipfs/{root}"),
            Utc::now() + chrono::Duration::hours(1),
            2,
            Duration::from_millis(1),
        )
        .unwrap()
        .encode()
        .unwrap();

        let routing_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let routing_address = routing_listener.local_addr().unwrap();
        let routing_server = tokio::spawn(async move {
            for (status, body) in [
                ("400 Bad Request", Vec::new()),
                ("503 Service Unavailable", Vec::new()),
                ("200 OK", recovery),
            ] {
                let (mut stream, _) = routing_listener.accept().await.unwrap();
                let _request = read_request(&mut stream).await;
                let response = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    crate::ipns::IPNS_RECORD_MEDIA_TYPE,
                    body.len()
                );
                stream.write_all(response.as_bytes()).await.unwrap();
                stream.write_all(&body).await.unwrap();
            }
        });
        let source = crate::stem::ipns::Source::new(crate::stem::ipns::Config::new(
            name,
            format!("http://{routing_address}"),
            state.path().to_path_buf(),
        ))
        .unwrap();
        let (ipfs_client, _calls, _requests, shutdown_tx, ipfs_server) = recording_kubo().await;
        let (epoch_tx, epoch_rx) = watch::channel(Epoch::zero());
        let staging = tempfile::tempdir().unwrap();
        let mut deployment = Deployment::bootstrap(
            Config {
                installed_baseline: None,
                runtime_engine: kernel::test_support::runtime(),
                source: Some(Box::new(source)),
                candidates: None,
                frozen_mounts: Vec::new(),
                retained_source_roots: Vec::new(),
                ipfs_client,
                staging_dir: staging.path().to_path_buf(),
                filesystem_host: test_filesystem_host(),
                filesystem_scope: test_filesystem_scope(),
            },
            epoch_tx,
            epoch_rx,
        )
        .await
        .unwrap();
        assert_eq!(deployment.current_epoch().root, Some(ROOT.parse().unwrap()));
        let mut epoch_observer = deployment.epoch_rx.clone();
        epoch_observer.borrow_and_update();

        let (running, result_tx, mut terminate_rx) = test_running_generation();
        let outcome = {
            let transition = deployment.await_generation(running);
            tokio::pin!(transition);
            tokio::select! {
                _ = &mut transition => panic!("IPNS transition completed before revocation"),
                changed = epoch_observer.changed() => changed.unwrap(),
                _ = tokio::time::sleep(Duration::from_secs(3)) => {
                    panic!("IPNS EOL did not revoke deployment")
                }
            }
            assert_eq!(epoch_observer.borrow().seq, 1);
            assert_eq!(epoch_observer.borrow().root, None);
            terminate_rx.changed().await.unwrap();
            result_tx.send(Ok(kernel::Outcome::Terminated)).unwrap();
            tokio::time::timeout(Duration::from_secs(3), transition)
                .await
                .expect("IPNS recovery did not reactivate deployment")
                .unwrap()
        };

        assert!(matches!(
            outcome,
            Outcome::Replaced {
                old_epoch: 0,
                new_epoch: 2,
                ..
            }
        ));
        assert_eq!(deployment.current_epoch().root, Some(ROOT.parse().unwrap()));
        assert!(
            deployment.source_task.is_some(),
            "host follower must remain alive"
        );
        routing_server.await.unwrap();
        shutdown_tx.send(()).unwrap();
        ipfs_server.await.unwrap();
    }

    #[tokio::test]
    async fn source_error_does_not_advance_the_epoch() {
        let client = crate::ipfs::HttpClient::new("http://127.0.0.1:1".to_owned());
        let staging = tempfile::tempdir().unwrap();
        let tree = Arc::new(CidTree::new(
            OLD_ROOT.parse().unwrap(),
            client.clone(),
            staging.path().to_owned(),
            test_filesystem_host(),
            test_filesystem_scope(),
        ));
        let (epoch_tx, epoch_rx) = watch::channel(Epoch {
            seq: 0,
            head: Vec::new(),
            root: Some(OLD_ROOT.parse().unwrap()),
        });
        let (source_tx, source_rx) = mpsc::channel(1);
        let mut deployment = Deployment {
            installed_baseline: None,
            runtime_engine: kernel::test_support::runtime(),
            active_kernel: kernel::test_support::prepared(ROOT),
            epoch_tx,
            epoch_rx,
            epoch_seq: 0,
            frozen_mounts: Vec::new(),
            frozen_pins: PinSet::default(),
            active_pins: PinSet::default(),
            retained_pins: Vec::new(),
            ipfs_client: client,
            cid_tree: tree,
            source_rx: Some(source_rx),
            source_task: None,
            candidate_rx: None,
            candidate_initialized: false,
            compilation: kernel::Compilation::default(),
            speculation: None,
        };
        let (terminate_tx, _terminate_rx) = watch::channel(());
        let (result_tx, result_rx) = oneshot::channel();
        let running = RunningGeneration {
            admission: None,
            intended_seq: 0,
            terminate_tx,
            result_rx,
            filesystem_completion: completed_filesystem_scope(),
        };
        source_tx
            .send(SourceMessage::Error(anyhow::anyhow!("RPC unavailable")))
            .await
            .unwrap();
        result_tx.send(Ok(kernel::Outcome::Exited(0))).unwrap();

        let outcome = deployment.await_generation(running).await.unwrap();

        assert!(matches!(
            outcome,
            Outcome::Authoritative {
                epoch: 0,
                result: Ok(kernel::Outcome::Exited(0))
            }
        ));
        assert_eq!(deployment.current_epoch().seq, 0);
    }

    #[tokio::test]
    async fn failed_unpin_keeps_one_retained_owner() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let _request = read_request(&mut stream).await;
            stream
                .write_all(b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 4\r\nConnection: close\r\n\r\nbusy")
                .await
                .unwrap();
        });
        let client = crate::ipfs::HttpClient::new(format!("http://{address}"));
        let mut retained = vec![
            PinSet {
                cids: vec![SHARED_ROOT.parse().unwrap()],
            },
            PinSet {
                cids: vec![SHARED_ROOT.parse().unwrap()],
            },
        ];
        let mut handled = HashSet::new();
        for pins in &mut retained {
            release_pin_set(pins, &client, &HashSet::new(), &mut handled).await;
        }
        retained.retain(|pins| !pins.is_empty());

        assert_eq!(retained.len(), 1);
        assert_eq!(retained[0].cids, [SHARED_ROOT.parse().unwrap()]);
        server.await.unwrap();

        release_pin_set(
            &mut retained[0],
            &client,
            &HashSet::from([SHARED_ROOT.parse().unwrap()]),
            &mut HashSet::new(),
        )
        .await;
        assert!(retained[0].is_empty());
    }

    #[tokio::test]
    async fn failed_speculative_unpin_is_retained_for_later_cleanup() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let request = read_request(&mut stream).await;
            assert!(request.contains("/api/v0/pin/rm"), "{request}");
            stream
                .write_all(b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 4\r\nConnection: close\r\n\r\nbusy")
                .await
                .unwrap();
        });
        let client = crate::ipfs::HttpClient::new(format!("http://{address}"));
        let staging = tempfile::tempdir().unwrap();
        let tree = Arc::new(CidTree::new(
            OLD_ROOT.parse().unwrap(),
            client.clone(),
            staging.path().to_owned(),
            test_filesystem_host(),
            test_filesystem_scope(),
        ));
        let (epoch_tx, epoch_rx) = watch::channel(Epoch {
            seq: 0,
            head: Vec::new(),
            root: Some(OLD_ROOT.parse().unwrap()),
        });
        let mut deployment = Deployment {
            installed_baseline: None,
            runtime_engine: kernel::test_support::runtime(),
            active_kernel: kernel::test_support::prepared(ROOT),
            epoch_tx,
            epoch_rx,
            epoch_seq: 0,
            frozen_mounts: Vec::new(),
            frozen_pins: PinSet::default(),
            active_pins: PinSet::default(),
            retained_pins: Vec::new(),
            ipfs_client: client,
            cid_tree: tree,
            source_rx: None,
            source_task: None,
            candidate_rx: None,
            candidate_initialized: false,
            compilation: kernel::Compilation::default(),
            speculation: Some(Speculation::Ready {
                prepared: PreparedRoot {
                    head: Some(Head {
                        cid: ROOT.parse().unwrap(),
                    }),
                    effective: ROOT.parse().unwrap(),
                    kernel: kernel::test_support::prepared(ROOT),
                    pins: PinSet {
                        cids: vec![ROOT.parse().unwrap()],
                    },
                },
                expires_at: tokio::time::Instant::now() + SPECULATIVE_RETENTION,
            }),
        };

        deployment.shutdown_speculation().await;

        assert!(deployment.speculation.is_none());
        assert_eq!(deployment.retained_pins.len(), 1);
        assert_eq!(deployment.retained_pins[0].cids, [ROOT.parse().unwrap()]);
        server.await.unwrap();
    }

    #[tokio::test]
    async fn superseded_attempt_does_not_unpin_a_retained_cid() {
        let client = crate::ipfs::HttpClient::new("http://127.0.0.1:1".to_owned());
        let staging = tempfile::tempdir().unwrap();
        let tree = Arc::new(CidTree::new(
            OLD_ROOT.parse().unwrap(),
            client.clone(),
            staging.path().to_owned(),
            test_filesystem_host(),
            test_filesystem_scope(),
        ));
        let (epoch_tx, epoch_rx) = watch::channel(Epoch::zero());
        let mut deployment = Deployment {
            installed_baseline: None,
            runtime_engine: kernel::test_support::runtime(),
            active_kernel: kernel::test_support::prepared(ROOT),
            epoch_tx,
            epoch_rx,
            epoch_seq: 0,
            frozen_mounts: Vec::new(),
            frozen_pins: PinSet::default(),
            active_pins: PinSet::default(),
            retained_pins: vec![PinSet {
                cids: vec![SHARED_ROOT.parse().unwrap()],
            }],
            ipfs_client: client,
            cid_tree: tree,
            source_rx: None,
            source_task: None,
            candidate_rx: None,
            candidate_initialized: false,
            compilation: kernel::Compilation::default(),
            speculation: None,
        };
        let mut attempt = PinSet {
            cids: vec![SHARED_ROOT.parse().unwrap()],
        };

        deployment.release_attempt(&mut attempt).await;

        assert!(attempt.is_empty());
        assert_eq!(
            deployment.retained_pins[0].cids,
            [SHARED_ROOT.parse().unwrap()]
        );
    }
}
