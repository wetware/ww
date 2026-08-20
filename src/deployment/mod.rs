//! Host-owned deployment lifecycle.
//!
//! Deployment allocates local epochs, revokes authority, prepares roots,
//! coordinates kernel teardown, activates `CidTree`, and launches one
//! [`crate::kernel::Generation`] at a time. The CLI retains process and
//! operator policy.

use std::collections::HashSet;
use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use authority::{Epoch, EpochGuard};
use rand::Rng;
use tokio::sync::{mpsc, oneshot, watch};
use tracing::{info, warn};

use crate::cell::image::dag_merge;
use crate::cell::vfs::CidTree;
use crate::kernel;
use crate::services::{ExecutorPool, SpawnRequest};
use crate::stem::{Head, InvalidHead, Source, Update};

const OPERATION_TIMEOUT: Duration = Duration::from_secs(30);
const RETRY_BASE_DELAY: Duration = Duration::from_secs(1);
const RETRY_MAX_DELAY: Duration = Duration::from_secs(60);
const RETRY_JITTER_MAX: Duration = Duration::from_millis(500);
const SOURCE_QUEUE_CAPACITY: usize = 16;
const KERNEL_TEARDOWN_TIMEOUT: Duration = Duration::from_secs(30);
const SPECULATIVE_RETENTION: Duration = Duration::from_secs(5 * 60);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FailureClass {
    Transient,
    Permanent,
    Unknown,
}

#[derive(Debug)]
struct OperationTimeout {
    operation: &'static str,
    timeout: Duration,
}

impl std::fmt::Display for OperationTimeout {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "deployment operation {} made no progress within {}s",
            self.operation,
            self.timeout.as_secs_f64()
        )
    }
}

impl std::error::Error for OperationTimeout {}

#[derive(Debug)]
struct PermanentPreparationError(String);

impl std::fmt::Display for PermanentPreparationError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for PermanentPreparationError {}

fn classify_failure(error: &anyhow::Error) -> FailureClass {
    if error
        .chain()
        .any(|cause| cause.is::<PermanentPreparationError>())
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

async fn bounded<T>(operation: &'static str, future: impl Future<Output = Result<T>>) -> Result<T> {
    tokio::time::timeout(OPERATION_TIMEOUT, future)
        .await
        .map_err(|_| OperationTimeout {
            operation,
            timeout: OPERATION_TIMEOUT,
        })?
}

fn retry_delay(current: &mut Duration) -> Duration {
    let jitter = rand::rng().random_range(0..RETRY_JITTER_MAX.as_millis() as u64);
    let delay = *current + Duration::from_millis(jitter);
    *current = (*current * 2).min(RETRY_MAX_DELAY);
    delay
}

#[derive(Debug, Default)]
struct PinSet {
    cids: Vec<String>,
}

impl PinSet {
    fn insert(&mut self, cid: String) {
        if !self.cids.contains(&cid) {
            self.cids.push(cid);
        }
    }

    fn contains(&self, cid: &str) -> bool {
        self.cids.iter().any(|owned| owned == cid)
    }

    fn is_empty(&self) -> bool {
        self.cids.is_empty()
    }
}

/// Immutable deployment content with pin ownership but no authority or epoch binding.
#[derive(Debug)]
pub struct PreparedRoot {
    head: Option<Head>,
    effective: String,
    pins: PinSet,
}

impl PreparedRoot {
    pub fn effective(&self) -> &str {
        &self.effective
    }

    fn head_bytes(&self) -> Vec<u8> {
        self.head.as_ref().map_or_else(Vec::new, Head::bytes)
    }
}

async fn prepare_root(
    head: Option<Head>,
    frozen_layers: &[String],
    ipfs_client: &crate::ipfs::HttpClient,
    cid_tree: Option<&Arc<CidTree>>,
    pins: &mut PinSet,
    cancel: &mut watch::Receiver<bool>,
) -> Result<PreparedRoot> {
    if *cancel.borrow() {
        anyhow::bail!("deployment preparation cancelled");
    }
    let mut layers = Vec::with_capacity(frozen_layers.len() + usize::from(head.is_some()));
    if let Some(head) = &head {
        let cid = head.cid.to_string();
        if !pins.contains(&cid) {
            bounded("head pin", ipfs_client.pin_add(&cid))
                .await
                .context("pinning deployment head")?;
            pins.insert(cid.clone());
        }
        layers.push(cid);
    }
    if *cancel.borrow() {
        anyhow::bail!("deployment preparation cancelled");
    }
    layers.extend_from_slice(frozen_layers);
    if layers.is_empty() {
        return Err(PermanentPreparationError(
            "deployment has neither a Stem head nor configured root layers".to_owned(),
        )
        .into());
    }

    let boot_client = crate::ipfs::BootClient::one_attempt(ipfs_client.clone(), OPERATION_TIMEOUT);
    let effective = bounded(
        "effective-root merge",
        dag_merge(&layers, &boot_client, cancel),
    )
    .await
    .context("composing effective deployment root")?;
    pins.insert(effective.clone());

    if *cancel.borrow() {
        anyhow::bail!("deployment preparation cancelled");
    }

    if let Some(tree) = cid_tree {
        bounded("effective-root prewarm", tree.pre_warm(&effective))
            .await
            .context("pre-warming effective deployment root")?;
    }
    Ok(PreparedRoot {
        head,
        effective,
        pins: std::mem::take(pins),
    })
}

struct PreparationOutput {
    result: Result<PreparedRoot>,
    pins: PinSet,
}

async fn prepare_root_owned(
    head: Head,
    frozen_layers: Vec<String>,
    ipfs_client: crate::ipfs::HttpClient,
    cid_tree: Arc<CidTree>,
    mut pins: PinSet,
    mut cancel: watch::Receiver<bool>,
) -> PreparationOutput {
    let result = prepare_root(
        Some(head),
        &frozen_layers,
        &ipfs_client,
        Some(&cid_tree),
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
    pub source: Option<Box<dyn Source>>,
    /// Latest observed Atom `HeadUpdated` CID bytes. These values are advisory only.
    pub candidates: Option<watch::Receiver<Option<Vec<u8>>>>,
    pub frozen_layers: Vec<String>,
    pub ipfs_client: crate::ipfs::HttpClient,
    pub staging_dir: PathBuf,
}

/// Inputs shared by all kernel generations in one host process.
pub struct KernelLauncher<'a> {
    executor_pool: &'a ExecutorPool,
    artifact: kernel::Artifact,
    readiness_gate: Arc<authority::KernelReadyGate>,
    bootstrap: kernel::Bootstrap,
    runtime_inputs: kernel::RuntimeInputs,
    stdio: kernel::Stdio,
}

impl<'a> KernelLauncher<'a> {
    pub fn new(
        executor_pool: &'a ExecutorPool,
        artifact: kernel::Artifact,
        readiness_gate: Arc<authority::KernelReadyGate>,
        bootstrap: kernel::Bootstrap,
        runtime_inputs: kernel::RuntimeInputs,
        stdio: kernel::Stdio,
    ) -> Self {
        Self {
            executor_pool,
            artifact,
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
    ) -> Result<RunningGeneration> {
        let root = epoch
            .root
            .as_ref()
            .context("cannot launch a kernel for an unrooted deployment epoch")?;
        let (terminate_tx, terminate_rx) = watch::channel(());
        let result_epoch_rx = epoch_rx.clone();
        let generation = kernel::Generation::new(
            self.artifact.clone(),
            kernel::Root::new(format!("/ipfs/{root}"), cid_tree),
            EpochGuard {
                issued_seq: epoch.seq,
                receiver: epoch_rx,
            },
            self.readiness_gate.clone(),
            self.bootstrap.clone(),
            self.runtime_inputs.clone(),
            self.stdio,
            terminate_rx,
        );
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
            intended_seq,
            terminate_tx,
            result_rx,
        })
    }
}

struct RunningGeneration {
    intended_seq: u64,
    terminate_tx: watch::Sender<()>,
    result_rx: oneshot::Receiver<Result<kernel::Outcome>>,
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
    epoch_tx: watch::Sender<Epoch>,
    epoch_rx: watch::Receiver<Epoch>,
    epoch_seq: u64,
    frozen_layers: Vec<String>,
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
        for layer in &config.frozen_layers {
            layer.parse::<cid::Cid>().map_err(|error| {
                PermanentPreparationError(format!(
                    "malformed configured deployment layer {layer}: {error}"
                ))
            })?;
        }

        let mut frozen_pins = PinSet::default();
        let mut pin_delay = RETRY_BASE_DELAY;
        for layer in &config.frozen_layers {
            loop {
                match bounded("frozen-layer pin", config.ipfs_client.pin_add(layer)).await {
                    Ok(()) => {
                        frozen_pins.insert(layer.clone());
                        pin_delay = RETRY_BASE_DELAY;
                        break;
                    }
                    Err(error) if classify_failure(&error) == FailureClass::Transient => {
                        let retry = retry_delay(&mut pin_delay);
                        warn!(%layer, retry_ms = retry.as_millis() as u64, "Frozen layer pin failed; retry scheduled: {error:#}");
                        tokio::time::sleep(retry).await;
                    }
                    Err(error) => return Err(error.context("pinning configured deployment layer")),
                }
            }
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
            epoch_tx,
            epoch_rx,
            epoch_seq: 0,
            target,
            frozen_layers: config.frozen_layers,
            frozen_pins,
            retained_pins: Vec::new(),
            ipfs_client: config.ipfs_client,
            staging_dir: config.staging_dir,
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
            self.frozen_layers.clone(),
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
        let candidate = bytes.and_then(|bytes| match cid::Cid::read_bytes(bytes.as_slice()) {
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
        let epoch = self.current_epoch();
        let running = match launcher.launch(epoch, self.epoch_rx.clone(), self.cid_tree.clone()) {
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
                Source(Option<SourceMessage>),
                Kernel(Result<Result<kernel::Outcome>, oneshot::error::RecvError>),
                Candidate(Result<(), watch::error::RecvError>),
                Prepared(Result<PreparationOutput, tokio::task::JoinError>),
                Released(Result<PinSet, tokio::task::JoinError>),
                Expired,
            }

            let source_rx = self.source_rx.as_mut();
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
                result = async { preparation_task.expect("preparation task is present").await }, if preparation_task.is_some() => LiveEvent::Prepared(result),
                result = async { release_task.expect("release task is present").await }, if release_task.is_some() => LiveEvent::Released(result),
                changed = async { candidate_rx.expect("candidate receiver is present").changed().await }, if candidate_rx.is_some() => LiveEvent::Candidate(changed),
                _ = async { tokio::time::sleep_until(expires_at.expect("expiry is present")).await }, if expires_at.is_some() => LiveEvent::Expired,
            };

            match event {
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
        mut running: RunningGeneration,
        updates: Vec<Update>,
        completed_result: Option<Result<Result<kernel::Outcome>, oneshot::error::RecvError>>,
    ) -> Result<Outcome> {
        let old_epoch = running.intended_seq;
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
        let _ = running.terminate_tx.send(());
        let teardown_timer = tokio::time::sleep(KERNEL_TEARDOWN_TIMEOUT);
        tokio::pin!(teardown_timer);
        let mut stopped = completed_result.map(|_| GenerationStopped(()));
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
                    Stopped,
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
                    result = &mut running.result_rx, if stopped.is_none() => {
                        let _ = result;
                        ReleaseEvent::Stopped
                    }
                    _ = &mut teardown_timer, if stopped.is_none() => ReleaseEvent::TimedOut,
                    update = next_source_update(source_rx) => ReleaseEvent::Update(update),
                    result = release => ReleaseEvent::Released(result),
                };
                match event {
                    ReleaseEvent::Stopped => stopped = Some(GenerationStopped(())),
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
                    Stopped,
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
                    result = &mut running.result_rx, if stopped.is_none() => {
                        let _ = result;
                        StopEvent::Stopped
                    }
                    _ = &mut teardown_timer, if stopped.is_none() => StopEvent::TimedOut,
                    update = next_source_update(source_rx) => StopEvent::Update(update),
                    result = preparation => StopEvent::Prepared(result),
                };
                match event {
                    StopEvent::Stopped => stopped = Some(GenerationStopped(())),
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

            // `frozen_layers` never changes after bootstrap, so the head CID is
            // the only variable preparation input within one `Deployment`.

            if let Target::Invalid(invalid) = &target {
                warn!(
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
                        result = &mut running.result_rx, if stopped.is_none() => {
                            let _ = result;
                            stopped = Some(GenerationStopped(()));
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
            let frozen_layers = self.frozen_layers.clone();
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
                        result = &mut running.result_rx, if stopped.is_none() => {
                            let _ = result;
                            stopped = Some(GenerationStopped(()));
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
                    &frozen_layers,
                    &ipfs_client,
                    Some(&cid_tree),
                    &mut attempt,
                    &mut cancel_rx,
                );
                tokio::pin!(preparation);
                loop {
                    tokio::select! {
                        biased;
                        result = &mut running.result_rx, if stopped.is_none() => {
                            let _ = result;
                            stopped = Some(GenerationStopped(()));
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
                            seq = self.epoch_seq,
                            retry_ms = delay.as_millis() as u64,
                            "Transient deployment preparation failure; retry scheduled: {error:#}"
                        );
                        let retry_timer = tokio::time::sleep(delay);
                        tokio::pin!(retry_timer);
                        loop {
                            tokio::select! {
                                biased;
                                result = &mut running.result_rx, if stopped.is_none() => {
                                    let _ = result;
                                    stopped = Some(GenerationStopped(()));
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
                            result = &mut running.result_rx => {
                                let _ = result;
                                stopped = Some(GenerationStopped(()));
                            }
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
                        seq = self.epoch_seq,
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
            pins,
        } = prepared;
        self.cid_tree.swap_root(effective.clone());
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

    fn protected_cids(&self, extra: Option<&PinSet>) -> HashSet<String> {
        self.frozen_pins
            .cids
            .iter()
            .chain(self.active_pins.cids.iter())
            .chain(extra.into_iter().flat_map(|pins| pins.cids.iter()))
            .cloned()
            .collect()
    }

    async fn release_retained(&mut self) {
        let protected = self.protected_cids(None);
        let mut handled = HashSet::new();
        for pins in &mut self.retained_pins {
            release_pin_set(pins, &self.ipfs_client, &protected, &mut handled).await;
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
    epoch_tx: watch::Sender<Epoch>,
    epoch_rx: watch::Receiver<Epoch>,
    epoch_seq: u64,
    target: Target,
    frozen_layers: Vec<String>,
    frozen_pins: PinSet,
    retained_pins: Vec<PinSet>,
    ipfs_client: crate::ipfs::HttpClient,
    staging_dir: PathBuf,
    source_rx: Option<mpsc::Receiver<SourceMessage>>,
    source_task: Option<tokio::task::JoinHandle<()>>,
    candidate_rx: Option<watch::Receiver<Option<Vec<u8>>>>,
}

impl BootstrapState {
    async fn prepare(mut self) -> Result<Deployment> {
        let tree = Arc::new(CidTree::new(
            String::new(),
            self.ipfs_client.clone(),
            self.staging_dir.clone(),
        ));
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
                    &self.frozen_layers,
                    &self.ipfs_client,
                    Some(&tree),
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
                    tree.swap_root(prepared.effective.clone());
                    self.epoch_tx.send_replace(Epoch {
                        seq: self.epoch_seq,
                        head: prepared.head_bytes(),
                        root: Some(prepared.effective.clone()),
                    });
                    info!(seq = self.epoch_seq, root = %prepared.effective, "Boot deployment root activated");
                    return Ok(Deployment {
                        epoch_tx: self.epoch_tx,
                        epoch_rx: self.epoch_rx,
                        epoch_seq: self.epoch_seq,
                        frozen_layers: self.frozen_layers,
                        frozen_pins: self.frozen_pins,
                        active_pins: std::mem::take(&mut prepared.pins),
                        retained_pins: self.retained_pins,
                        ipfs_client: self.ipfs_client,
                        cid_tree: tree,
                        source_rx: self.source_rx,
                        source_task: self.source_task,
                        candidate_rx: self.candidate_rx,
                        candidate_initialized: false,
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
        let protected: HashSet<String> = self
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

async fn release_pin_set(
    pins: &mut PinSet,
    ipfs_client: &crate::ipfs::HttpClient,
    protected: &HashSet<String>,
    handled: &mut HashSet<String>,
) {
    let mut retained = Vec::new();
    for cid in pins.cids.drain(..) {
        if protected.contains(&cid) || !handled.insert(cid.clone()) {
            continue;
        }
        match bounded("pin removal", ipfs_client.pin_rm(&cid)).await {
            Ok(()) => info!(%cid, "Released deployment pin"),
            Err(error) => {
                warn!(%cid, "Deployment pin release deferred: {error:#}");
                retained.push(cid);
            }
        }
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

    const ROOT: &str = "bafybeigdyrzt5sfp7udm7hu76uh7y26nf3efuylqabf3oclgtqy55fbzdi";

    async fn read_request(stream: &mut tokio::net::TcpStream) -> String {
        let mut request = vec![0_u8; 8192];
        let read = stream.read(&mut request).await.unwrap();
        String::from_utf8_lossy(&request[..read]).into_owned()
    }

    async fn respond(stream: &mut tokio::net::TcpStream, body: serde_json::Value) {
        let body = body.to_string();
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        stream.write_all(response.as_bytes()).await.unwrap();
    }

    async fn single_root_kubo() -> (crate::ipfs::HttpClient, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            for expected in ["/api/v0/pin/add", "/api/v0/pin/add", "/api/v0/ls"] {
                let (mut stream, _) = listener.accept().await.unwrap();
                let request = read_request(&mut stream).await;
                assert!(
                    request.lines().next().unwrap().contains(expected),
                    "{request}"
                );
                let body = if expected.ends_with("/ls") {
                    serde_json::json!({"Objects": [{"Links": []}]})
                } else {
                    serde_json::json!({})
                };
                respond(&mut stream, body).await;
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
                let body = if path.starts_with("/api/v0/ls") {
                    serde_json::json!({"Objects": [{"Links": []}]})
                } else {
                    serde_json::json!({})
                };
                respond(&mut stream, body).await;
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
            "old-root".to_owned(),
            client.clone(),
            staging_dir,
        ));
        let (epoch_tx, epoch_rx) = watch::channel(Epoch {
            seq: 0,
            head: Vec::new(),
            root: Some("old-root".to_owned()),
        });
        let deployment = Deployment {
            epoch_tx,
            epoch_rx: epoch_rx.clone(),
            epoch_seq: 0,
            frozen_layers: Vec::new(),
            frozen_pins: PinSet::default(),
            active_pins: PinSet::default(),
            retained_pins: Vec::new(),
            ipfs_client: client,
            cid_tree: tree.clone(),
            source_rx: Some(source_rx),
            source_task: None,
            candidate_rx: Some(candidate_rx),
            candidate_initialized: false,
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
                intended_seq: 0,
                terminate_tx,
                result_rx,
            },
            result_tx,
            terminate_rx,
        )
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
            for expected in ["/api/v0/pin/add", "/api/v0/pin/add", "/api/v0/ls"] {
                let (mut stream, _) = listener.accept().await.unwrap();
                let request = read_request(&mut stream).await;
                assert!(
                    request.lines().next().unwrap().contains(expected),
                    "{request}"
                );
                let body = if expected.ends_with("/ls") {
                    serde_json::json!({"Objects": [{"Links": []}]})
                } else {
                    serde_json::json!({})
                };
                respond(&mut stream, body).await;
            }
        });
        let (epoch_tx, epoch_rx) = watch::channel(Epoch::zero());
        let staging = tempfile::tempdir().unwrap();
        let deployment = Deployment::bootstrap(
            Config {
                source: None,
                candidates: None,
                frozen_layers: vec![ROOT.to_owned()],
                ipfs_client: crate::ipfs::HttpClient::new(format!("http://{address}")),
                staging_dir: staging.path().to_owned(),
            },
            epoch_tx,
            epoch_rx,
        )
        .await
        .unwrap();

        let epoch = deployment.current_epoch();
        assert_eq!(epoch.seq, 0);
        assert!(epoch.head.is_empty());
        assert_eq!(epoch.root.as_deref(), Some(ROOT));
        assert!(deployment.source_rx.is_none());
        assert_eq!(deployment.cid_tree.root_cid().as_ref(), ROOT);
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
                source: Some(Box::new(source)),
                candidates: None,
                frozen_layers: Vec::new(),
                ipfs_client,
                staging_dir: staging.path().to_owned(),
            },
            epoch_tx,
            epoch_rx,
        )
        .await
        .unwrap();

        let epoch = deployment.current_epoch();
        assert_eq!(epoch.seq, 0);
        assert_eq!(epoch.head, head.bytes());
        assert_eq!(epoch.root.as_deref(), Some(ROOT));
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
                source: Some(Box::new(source)),
                candidates: None,
                frozen_layers: Vec::new(),
                ipfs_client,
                staging_dir: staging.path().to_owned(),
            },
            epoch_tx,
            epoch_rx,
        )
        .await
        .unwrap();

        let epoch = deployment.current_epoch();
        assert_eq!(epoch.seq, 1);
        assert_eq!(epoch.head, valid.bytes());
        assert_eq!(epoch.root.as_deref(), Some(ROOT));
        server.await.unwrap();
    }

    #[test]
    fn local_epoch_increment_is_checked() {
        let client = crate::ipfs::HttpClient::new("http://127.0.0.1:1".to_owned());
        let staging = tempfile::tempdir().unwrap();
        let tree = Arc::new(CidTree::new(
            "old-root".to_owned(),
            client.clone(),
            staging.path().to_owned(),
        ));
        let initial = Epoch {
            seq: u64::MAX,
            head: Vec::new(),
            root: Some("old-root".to_owned()),
        };
        let (epoch_tx, epoch_rx) = watch::channel(initial.clone());
        let mut deployment = Deployment {
            epoch_tx,
            epoch_rx,
            epoch_seq: u64::MAX,
            frozen_layers: Vec::new(),
            frozen_pins: PinSet::default(),
            active_pins: PinSet::default(),
            retained_pins: Vec::new(),
            ipfs_client: client,
            cid_tree: tree,
            source_rx: None,
            source_task: None,
            candidate_rx: None,
            candidate_initialized: false,
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
            "old-root".to_owned(),
            client.clone(),
            staging.path().to_owned(),
        ));
        let (epoch_tx, epoch_rx) = watch::channel(Epoch {
            seq: 3,
            head: Vec::new(),
            root: None,
        });
        let mut deployment = Deployment {
            epoch_tx,
            epoch_rx,
            epoch_seq: 3,
            frozen_layers: Vec::new(),
            frozen_pins: PinSet::default(),
            active_pins: PinSet::default(),
            retained_pins: Vec::new(),
            ipfs_client: client,
            cid_tree: tree.clone(),
            source_rx: None,
            source_task: None,
            candidate_rx: None,
            candidate_initialized: false,
            speculation: None,
        };
        let prepared = PreparedRoot {
            head: None,
            effective: "superseded-root".to_owned(),
            pins: PinSet::default(),
        };

        let result = deployment.activate(2, None, prepared, GenerationStopped(()));

        assert!(result.is_err());
        assert_eq!(tree.root_cid().as_ref(), "old-root");
        assert_eq!(deployment.current_epoch().root, None);
    }

    #[tokio::test]
    async fn preparation_does_not_select_the_candidate_root() {
        let (client, server) = single_root_kubo().await;
        let staging = tempfile::tempdir().unwrap();
        let tree = Arc::new(CidTree::new(
            "old-root".to_owned(),
            client.clone(),
            staging.path().to_owned(),
        ));
        let mut pins = PinSet::default();
        let (_cancel, mut cancel_rx) = watch::channel(false);

        let prepared = prepare_root(
            Some(Head {
                cid: ROOT.parse().unwrap(),
            }),
            &[],
            &client,
            Some(&tree),
            &mut pins,
            &mut cancel_rx,
        )
        .await
        .unwrap();

        assert_eq!(prepared.effective(), ROOT);
        assert_eq!(tree.root_cid().as_ref(), "old-root");
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
        for expected in ["/api/v0/pin/add", "/api/v0/pin/add", "/api/v0/ls"] {
            let request = next_request(&mut requests).await;
            assert!(request.starts_with(expected), "{request}");
        }
        assert_eq!(epoch_observer.borrow().seq, 0);
        assert_eq!(epoch_observer.borrow().root.as_deref(), Some("old-root"));
        assert_eq!(tree.root_cid().as_ref(), "old-root");
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
            assert_eq!(tree.root_cid().as_ref(), "old-root");
            assert!(terminate_rx.has_changed().unwrap());

            result_tx.send(Ok(kernel::Outcome::Terminated)).unwrap();
            transition.await.unwrap()
        };

        assert!(matches!(outcome, Outcome::Replaced { new_epoch: 1, .. }));
        assert_eq!(tree.root_cid().as_ref(), ROOT);
        assert_eq!(deployment.current_epoch().root.as_deref(), Some(ROOT));
        assert_eq!(calls.load(Ordering::SeqCst), 3);
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
        for expected in ["/api/v0/pin/add", "/api/v0/pin/add", "/api/v0/ls"] {
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
            assert_eq!(epoch_observer.borrow().root.as_deref(), Some("old-root"));
            assert_eq!(tree.root_cid().as_ref(), "old-root");
            assert!(!terminate_rx.has_changed().unwrap());
            assert!(ready_gate.is_ready());

            result_tx.send(Ok(kernel::Outcome::Exited(0))).unwrap();
            transition.await.unwrap()
        };

        assert!(matches!(outcome, Outcome::Authoritative { epoch: 0, .. }));
        assert_eq!(calls.load(Ordering::SeqCst), 4);

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
        assert_eq!(calls.load(Ordering::SeqCst), 4);

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
            for index in 1..=3 {
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
                let body = if path.starts_with("/api/v0/ls") {
                    serde_json::json!({"Objects": [{"Links": []}]})
                } else {
                    serde_json::json!({})
                };
                respond(&mut stream, body).await;
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
            assert_eq!(tree.root_cid().as_ref(), "old-root");
            assert!(terminate_rx.has_changed().unwrap());

            release_first_tx.send(()).unwrap();
            for expected in ["/api/v0/pin/add", "/api/v0/ls"] {
                let request = tokio::select! {
                    _ = &mut transition => panic!("replacement activated before teardown"),
                    request = next_request(&mut requests) => request,
                };
                assert!(request.starts_with(expected), "{request}");
            }
            assert_eq!(tree.root_cid().as_ref(), "old-root");
            result_tx.send(Ok(kernel::Outcome::Terminated)).unwrap();
            transition.await.unwrap()
        };

        assert!(matches!(outcome, Outcome::Replaced { new_epoch: 1, .. }));
        assert_eq!(tree.root_cid().as_ref(), ROOT);
        assert_eq!(calls.load(Ordering::SeqCst), 3);
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
            assert_eq!(epoch_observer.borrow().root.as_deref(), Some("old-root"));
            assert_eq!(tree.root_cid().as_ref(), "old-root");
            assert!(!terminate_rx.has_changed().unwrap());
            assert_eq!(calls.load(Ordering::SeqCst), 0);

            source_tx
                .send(SourceMessage::Update(Update::Head(head)))
                .await
                .unwrap();
            for expected in ["/api/v0/pin/add", "/api/v0/pin/add", "/api/v0/ls"] {
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
        assert_eq!(tree.root_cid().as_ref(), ROOT);
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
            cid: "bafkreibm6jg3ux5qugqkmfqt5uj5rxszb4sa4e3u7jj4c5ukv5s4xvcc7a"
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
            for _ in 0..3 {
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
            assert_eq!(tree.root_cid().as_ref(), "old-root");
            assert!(terminate_rx.has_changed().unwrap());

            let unpin = tokio::select! {
                _ = &mut transition => panic!("replacement activated before teardown"),
                request = next_request(&mut requests) => request,
            };
            assert!(unpin.starts_with("/api/v0/pin/rm"), "{unpin}");
            assert!(unpin.contains(&speculative.cid.to_string()), "{unpin}");
            for expected in ["/api/v0/pin/add", "/api/v0/pin/add", "/api/v0/ls"] {
                let request = tokio::select! {
                    _ = &mut transition => panic!("replacement activated before teardown"),
                    request = next_request(&mut requests) => request,
                };
                assert!(request.starts_with(expected), "{request}");
                assert!(request.contains(ROOT), "{request}");
            }
            result_tx.send(Ok(kernel::Outcome::Terminated)).unwrap();
            transition.await.unwrap()
        };

        assert!(matches!(outcome, Outcome::Replaced { new_epoch: 1, .. }));
        assert_eq!(tree.root_cid().as_ref(), ROOT);
        assert_eq!(calls.load(Ordering::SeqCst), 7);
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
            for index in 1..=6 {
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
                    let body = if path.starts_with("/api/v0/ls") {
                        serde_json::json!({"Objects": [{"Links": []}]})
                    } else {
                        serde_json::json!({})
                    };
                    respond(&mut stream, body).await;
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
            for expected in ["/api/v0/pin/add", "/api/v0/pin/add", "/api/v0/pin/rm"] {
                let request = tokio::select! {
                    _ = &mut transition => panic!("speculative failure changed kernel outcome"),
                    request = next_request(&mut requests) => request,
                };
                assert!(request.starts_with(expected), "{request}");
            }
            assert_eq!(epoch_observer.borrow().seq, 0);
            assert_eq!(epoch_observer.borrow().root.as_deref(), Some("old-root"));
            assert_eq!(tree.root_cid().as_ref(), "old-root");
            assert!(!terminate_rx.has_changed().unwrap());

            source_tx
                .send(SourceMessage::Update(Update::Head(head)))
                .await
                .unwrap();
            for expected in ["/api/v0/pin/add", "/api/v0/pin/add", "/api/v0/ls"] {
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
        assert_eq!(tree.root_cid().as_ref(), ROOT);
        assert_eq!(calls.load(Ordering::SeqCst), 6);
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
            for index in 1..=6 {
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
                let body = if path.starts_with("/api/v0/ls") {
                    serde_json::json!({"Objects": [{"Links": []}]})
                } else {
                    serde_json::json!({})
                };
                respond(&mut stream, body).await;
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
            cid: "bafkreibm6jg3ux5qugqkmfqt5uj5rxszb4sa4e3u7jj4c5ukv5s4xvcc7a"
                .parse()
                .unwrap(),
        };
        let intermediate = Head {
            cid: "bafkreif2pall7dybz7vecqka3zo24nq2j4tztjwc5c3f4vmrf6sz4d3asa"
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
            for expected in ["/api/v0/pin/add", "/api/v0/pin/add", "/api/v0/ls"] {
                let request = tokio::select! {
                    _ = &mut transition => panic!("candidate changed kernel outcome"),
                    request = next_request(&mut requests) => request,
                };
                assert!(request.starts_with(expected), "{request}");
                assert!(request.contains(ROOT), "{request}");
            }
            tokio::select! {
                _ = &mut transition => panic!("candidate changed kernel outcome"),
                _ = tokio::time::sleep(Duration::from_millis(20)) => {}
            }
            assert_eq!(epoch_observer.borrow().seq, 0);
            assert_eq!(epoch_observer.borrow().root.as_deref(), Some("old-root"));
            assert_eq!(tree.root_cid().as_ref(), "old-root");
            assert!(!terminate_rx.has_changed().unwrap());

            result_tx.send(Ok(kernel::Outcome::Exited(0))).unwrap();
            transition.await.unwrap()
        };

        assert!(matches!(outcome, Outcome::Authoritative { epoch: 0, .. }));
        let newest_unpin = next_request(&mut requests).await;
        assert!(newest_unpin.starts_with("/api/v0/pin/rm"), "{newest_unpin}");
        assert!(newest_unpin.contains(ROOT), "{newest_unpin}");
        assert_eq!(calls.load(Ordering::SeqCst), 6);
        server.await.unwrap();
    }

    #[tokio::test]
    async fn old_generation_stops_before_cid_tree_root_swap() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (prepared_tx, prepared_rx) = oneshot::channel();
        let server = tokio::spawn(async move {
            for expected in ["/api/v0/pin/add", "/api/v0/pin/add", "/api/v0/ls"] {
                let (mut stream, _) = listener.accept().await.unwrap();
                let request = read_request(&mut stream).await;
                assert!(
                    request.lines().next().unwrap().contains(expected),
                    "{request}"
                );
                let body = if expected.ends_with("/ls") {
                    serde_json::json!({"Objects": [{"Links": []}]})
                } else {
                    serde_json::json!({})
                };
                respond(&mut stream, body).await;
            }
            prepared_tx.send(()).unwrap();
        });
        let client = crate::ipfs::HttpClient::new(format!("http://{address}"));
        let staging = tempfile::tempdir().unwrap();
        let tree = Arc::new(CidTree::new(
            "old-root".to_owned(),
            client.clone(),
            staging.path().to_owned(),
        ));
        let (epoch_tx, epoch_rx) = watch::channel(Epoch {
            seq: 0,
            head: Vec::new(),
            root: Some("old-root".to_owned()),
        });
        let (_source_tx, source_rx) = mpsc::channel(1);
        let mut deployment = Deployment {
            epoch_tx,
            epoch_rx,
            epoch_seq: 0,
            frozen_layers: Vec::new(),
            frozen_pins: PinSet::default(),
            active_pins: PinSet::default(),
            retained_pins: Vec::new(),
            ipfs_client: client,
            cid_tree: tree.clone(),
            source_rx: Some(source_rx),
            source_task: None,
            candidate_rx: None,
            candidate_initialized: false,
            speculation: None,
        };
        let (terminate_tx, terminate_rx) = watch::channel(());
        let (result_tx, result_rx) = oneshot::channel();
        let running = RunningGeneration {
            intended_seq: 0,
            terminate_tx,
            result_rx,
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
            assert_eq!(tree.root_cid().as_ref(), "old-root");

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
        assert_eq!(tree.root_cid().as_ref(), ROOT);
        assert_eq!(deployment.current_epoch().root.as_deref(), Some(ROOT));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn invalid_replacement_revokes_then_later_valid_update_recovers() {
        let (client, server) = single_root_kubo().await;
        let staging = tempfile::tempdir().unwrap();
        let tree = Arc::new(CidTree::new(
            "old-root".to_owned(),
            client.clone(),
            staging.path().to_owned(),
        ));
        let (epoch_tx, epoch_rx) = watch::channel(Epoch {
            seq: 0,
            head: Vec::new(),
            root: Some("old-root".to_owned()),
        });
        let mut epoch_observer = epoch_rx.clone();
        let (source_tx, source_rx) = mpsc::channel(2);
        let mut deployment = Deployment {
            epoch_tx,
            epoch_rx,
            epoch_seq: 0,
            frozen_layers: Vec::new(),
            frozen_pins: PinSet::default(),
            active_pins: PinSet::default(),
            retained_pins: Vec::new(),
            ipfs_client: client,
            cid_tree: tree.clone(),
            source_rx: Some(source_rx),
            source_task: None,
            candidate_rx: None,
            candidate_initialized: false,
            speculation: None,
        };
        let (terminate_tx, terminate_rx) = watch::channel(());
        let (result_tx, result_rx) = oneshot::channel();
        let running = RunningGeneration {
            intended_seq: 0,
            terminate_tx,
            result_rx,
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
            assert_eq!(tree.root_cid().as_ref(), "old-root");
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
        assert_eq!(tree.root_cid().as_ref(), ROOT);
        assert_eq!(deployment.current_epoch().root.as_deref(), Some(ROOT));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn source_error_does_not_advance_the_epoch() {
        let client = crate::ipfs::HttpClient::new("http://127.0.0.1:1".to_owned());
        let staging = tempfile::tempdir().unwrap();
        let tree = Arc::new(CidTree::new(
            "old-root".to_owned(),
            client.clone(),
            staging.path().to_owned(),
        ));
        let (epoch_tx, epoch_rx) = watch::channel(Epoch {
            seq: 0,
            head: Vec::new(),
            root: Some("old-root".to_owned()),
        });
        let (source_tx, source_rx) = mpsc::channel(1);
        let mut deployment = Deployment {
            epoch_tx,
            epoch_rx,
            epoch_seq: 0,
            frozen_layers: Vec::new(),
            frozen_pins: PinSet::default(),
            active_pins: PinSet::default(),
            retained_pins: Vec::new(),
            ipfs_client: client,
            cid_tree: tree,
            source_rx: Some(source_rx),
            source_task: None,
            candidate_rx: None,
            candidate_initialized: false,
            speculation: None,
        };
        let (terminate_tx, _terminate_rx) = watch::channel(());
        let (result_tx, result_rx) = oneshot::channel();
        let running = RunningGeneration {
            intended_seq: 0,
            terminate_tx,
            result_rx,
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
                cids: vec!["shared".to_owned()],
            },
            PinSet {
                cids: vec!["shared".to_owned()],
            },
        ];
        let mut handled = HashSet::new();
        for pins in &mut retained {
            release_pin_set(pins, &client, &HashSet::new(), &mut handled).await;
        }
        retained.retain(|pins| !pins.is_empty());

        assert_eq!(retained.len(), 1);
        assert_eq!(retained[0].cids, ["shared"]);
        server.await.unwrap();

        release_pin_set(
            &mut retained[0],
            &client,
            &HashSet::from(["shared".to_owned()]),
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
            "old-root".to_owned(),
            client.clone(),
            staging.path().to_owned(),
        ));
        let (epoch_tx, epoch_rx) = watch::channel(Epoch {
            seq: 0,
            head: Vec::new(),
            root: Some("old-root".to_owned()),
        });
        let mut deployment = Deployment {
            epoch_tx,
            epoch_rx,
            epoch_seq: 0,
            frozen_layers: Vec::new(),
            frozen_pins: PinSet::default(),
            active_pins: PinSet::default(),
            retained_pins: Vec::new(),
            ipfs_client: client,
            cid_tree: tree,
            source_rx: None,
            source_task: None,
            candidate_rx: None,
            candidate_initialized: false,
            speculation: Some(Speculation::Ready {
                prepared: PreparedRoot {
                    head: Some(Head {
                        cid: ROOT.parse().unwrap(),
                    }),
                    effective: ROOT.to_owned(),
                    pins: PinSet {
                        cids: vec![ROOT.to_owned()],
                    },
                },
                expires_at: tokio::time::Instant::now() + SPECULATIVE_RETENTION,
            }),
        };

        deployment.shutdown_speculation().await;

        assert!(deployment.speculation.is_none());
        assert_eq!(deployment.retained_pins.len(), 1);
        assert_eq!(deployment.retained_pins[0].cids, [ROOT]);
        server.await.unwrap();
    }

    #[tokio::test]
    async fn superseded_attempt_does_not_unpin_a_retained_cid() {
        let client = crate::ipfs::HttpClient::new("http://127.0.0.1:1".to_owned());
        let staging = tempfile::tempdir().unwrap();
        let tree = Arc::new(CidTree::new(
            "old-root".to_owned(),
            client.clone(),
            staging.path().to_owned(),
        ));
        let (epoch_tx, epoch_rx) = watch::channel(Epoch::zero());
        let mut deployment = Deployment {
            epoch_tx,
            epoch_rx,
            epoch_seq: 0,
            frozen_layers: Vec::new(),
            frozen_pins: PinSet::default(),
            active_pins: PinSet::default(),
            retained_pins: vec![PinSet {
                cids: vec!["shared".to_owned()],
            }],
            ipfs_client: client,
            cid_tree: tree,
            source_rx: None,
            source_task: None,
            candidate_rx: None,
            candidate_initialized: false,
            speculation: None,
        };
        let mut attempt = PinSet {
            cids: vec!["shared".to_owned()],
        };

        deployment.release_attempt(&mut attempt).await;

        assert!(attempt.is_empty());
        assert_eq!(deployment.retained_pins[0].cids, ["shared"]);
    }
}
