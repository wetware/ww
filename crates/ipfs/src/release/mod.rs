//! Durable release retention and publication ordering, independent of deployment caches.
//!
//! [`Owner`] is the authoritative local release ordering journal for one channel.
//! Complete retention precedes local commitment. The same atomic journal commit
//! selects the current release and saves its immutable announcement request.
//! Only then may [`Publisher`] deliver that request to eventual discovery.
//!
//! Delivery acknowledgement is evidence of transport acceptance, not remote CAS,
//! delayed-write fencing, or proof that any consumer discovers the latest release.
//! Errors and cancellation cannot undo a local commit. A newer local commit may
//! supersede an uncertain delivery; an older remote write may still arrive later.
//! Consumers need authenticated release freshness watermarks. A new consumer
//! cannot prove latest discovery through IPNS alone.
//!
//! Retirement never unpins. The journal does not audit retained backend storage.
//! See `doc/release-publication.md` for the approved divergence from #737.

mod graph;
mod journal;

use anyhow::{ensure, Context, Result};
use async_trait::async_trait;
use cid::Cid;
use serde::{Deserialize, Serialize};
use std::{path::Path, time::Duration};

/// Maximum release records. Retired records remain ownership evidence.
pub const MAX_RELEASES: usize = 256;
/// Maximum acknowledged/pending stages across the entire owner.
pub const MAX_STAGES: usize = 4096;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Publication {
    pub generation: u64,
    pub root: Cid,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PublicationIntent {
    pub previous: Option<Publication>,
    pub next: Publication,
}

/// Authenticated discovery adapter. `prepare` must validate the exact release,
/// its channel, ordering, and compatibility before returning a bounded immutable
/// request. It MUST NOT announce anything. The owner journals that request and
/// commits local authority before calling `publish`.
///
/// `publish` may deliver the request more than once. Success acknowledges delivery
/// only. Errors/cancellation leave delivery uncertain, including after a newer
/// local release commits. No remote comparison, CAS, or delayed-effect fence is
/// assumed. The owner never invokes an obsolete request after supersession.
#[async_trait]
pub trait Publisher: Send + Sync {
    fn domain(&self) -> &str;
    async fn prepare(&self, intent: &PublicationIntent) -> Result<Vec<u8>>;
    async fn publish(&self, intent: &PublicationIntent, announcement: &[u8]) -> Result<()>;
}

/// Bounded immutable delivery request stored in the existing owner journal.
pub const MAX_ANNOUNCEMENT_BYTES: usize = 16 * 1024;

/// Retention must atomically exclude GC from import through complete recursive
/// graph validation, pinning, and durable pin acknowledgement. Empty blocks
/// reconcile a previously journaled import. Errors can leave retained data.
/// No implementation may delete another owner's pins. This trait has no unpin.
#[async_trait]
pub trait Retention: Send + Sync {
    fn domain(&self) -> &str;
    async fn import(&self, root: &Cid, blocks: &[(Cid, Vec<u8>)]) -> Result<()>;
}

/// Bounded adapter for a provisioned release Kubo retention domain.
///
/// `domain` must identify one stable Kubo repository and publication channel.
/// Operators must route every publisher for that channel through the same
/// journal directory. Other clients must not remove its pins. The domain is an
/// operator binding, not an independently authenticated Kubo identity.
pub struct KuboRetention {
    client: crate::HttpClient,
    domain: String,
    timeout: Duration,
}
impl KuboRetention {
    pub fn new(client: crate::HttpClient, domain: String, timeout: Duration) -> Result<Self> {
        validate_domain(&domain)?;
        ensure!(!timeout.is_zero(), "retention timeout must be positive");
        Ok(Self {
            client,
            domain,
            timeout,
        })
    }
}
#[async_trait]
impl Retention for KuboRetention {
    fn domain(&self) -> &str {
        &self.domain
    }
    async fn import(&self, root: &Cid, blocks: &[(Cid, Vec<u8>)]) -> Result<()> {
        tokio::time::timeout(self.timeout, self.client.import_composed(root, blocks))
            .await
            .context("release retention timed out; acknowledgement unknown")?
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReleaseState {
    Candidate,
    Retained,
    Published,
    Retired,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Release {
    pub publication: Publication,
    pub previous: Option<Publication>,
    pub state: ReleaseState,
    /// Roots acknowledged by this owner; pins remain even after retirement.
    pub stages: Vec<Cid>,
    /// A validated import whose acknowledgement is not durably recorded yet.
    pub pending_stage: Option<Cid>,
    /// Delivery has not been acknowledged. May remain true after retirement;
    /// a superseded request can still be in flight remotely.
    pub publishing: bool,
    /// Immutable authenticated request selected at the local commit point.
    pub announcement: Option<Vec<u8>>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct State {
    version: u32,
    domain: String,
    current: Option<Publication>,
    releases: Vec<Release>,
}

/// One locked owner per private journal directory. Async methods borrow the
/// owner exclusively; the OS lock serializes separate processes and instances.
/// Durable writes are small synchronous commits with no cancellation points.
/// A failed durable write poisons the handle; reopen before further operations.
pub struct Owner {
    journal: journal::Journal,
    state: State,
    poisoned: bool,
}

impl Owner {
    /// Open an existing private directory on a local filesystem supporting
    /// advisory locks, atomic rename, file sync, and directory sync. The caller
    /// must durably provision the directory and must not replace it while used.
    pub fn open(directory: &Path, domain: &str) -> Result<Self> {
        validate_domain(domain)?;
        let mut journal = journal::Journal::open(directory)?;
        let state = match journal.read()? {
            Some(state) => state,
            None => {
                let state = State {
                    version: 2,
                    domain: domain.to_owned(),
                    current: None,
                    releases: Vec::new(),
                };
                journal.write(&state)?;
                state
            }
        };
        state.validate()?;
        ensure!(
            state.domain == domain,
            "release retention domain differs from journal"
        );
        journal.mark_initialized()?;
        Ok(Self {
            journal,
            state,
            poisoned: false,
        })
    }

    fn ready(&self) -> Result<()> {
        ensure!(
            !self.poisoned,
            "journal write failed; reopen owner to reconcile"
        );
        Ok(())
    }
    fn index(&self, generation: u64) -> Result<usize> {
        self.ready()?;
        self.state
            .releases
            .iter()
            .position(|r| r.publication.generation == generation)
            .context("unknown release generation")
    }
    fn commit(&mut self, next: State) -> Result<()> {
        self.ready()?;
        next.validate()?;
        self.poisoned = true;
        self.journal.write(&next)?;
        self.state = next;
        self.poisoned = false;
        Ok(())
    }
    pub fn current(&self) -> Result<Option<Publication>> {
        self.ready()?;
        Ok(self.state.current.clone())
    }
    pub fn releases(&self) -> Result<&[Release]> {
        self.ready()?;
        Ok(&self.state.releases)
    }

    /// Reserve one candidate against the durable current publication. Exact
    /// retries return the same generation; competing/stale candidates fail.
    pub fn begin(&mut self, root: Cid, expected: Option<Publication>) -> Result<u64> {
        self.ready()?;
        graph::validate_cid(&root)?;
        ensure!(expected == self.state.current, "stale expected publication");
        if let Some(active) = self
            .state
            .releases
            .iter()
            .find(|r| matches!(r.state, ReleaseState::Candidate | ReleaseState::Retained))
        {
            ensure!(
                active.publication.root == root && active.previous == expected,
                "competing candidate already active"
            );
            return Ok(active.publication.generation);
        }
        ensure!(
            self.state.releases.len() < MAX_RELEASES,
            "release record limit reached; coordinated migration required"
        );
        let generation = self
            .state
            .releases
            .last()
            .map_or(1, |r| r.publication.generation + 1);
        let mut next = self.state.clone();
        next.releases.push(Release {
            publication: Publication { generation, root },
            previous: expected,
            state: ReleaseState::Candidate,
            stages: Vec::new(),
            pending_stage: None,
            publishing: false,
            announcement: None,
        });
        self.commit(next)?;
        Ok(generation)
    }

    /// Validate one bounded stage before any backend effect. Reused descendants
    /// must be roots already acknowledged by this owner. The final release root
    /// is retained the same way, linking these retained stages.
    pub async fn retain_stage<R: Retention + ?Sized>(
        &mut self,
        generation: u64,
        root: Cid,
        blocks: &[(Cid, Vec<u8>)],
        backend: &R,
    ) -> Result<()> {
        let index = self.index(generation)?;
        ensure!(
            backend.domain() == self.state.domain,
            "retention domain mismatch"
        );
        let release = &self.state.releases[index];
        ensure!(
            release.state == ReleaseState::Candidate,
            "release is not a candidate"
        );
        ensure!(
            release.pending_stage.is_none_or(|pending| pending == root),
            "another stage needs reconciliation"
        );
        let retained = self
            .state
            .releases
            .iter()
            .flat_map(|r| r.stages.iter().copied())
            .collect();
        graph::validate_stage(&root, blocks, &retained)?;
        if release.stages.contains(&root) {
            return Ok(());
        }
        if release.pending_stage.is_none() {
            ensure!(
                self.state.stage_count() < MAX_STAGES,
                "release stage limit reached"
            );
            let mut next = self.state.clone();
            next.releases[index].pending_stage = Some(root);
            self.commit(next)?;
        }
        backend.import(&root, blocks).await?;
        self.acknowledge_stage(index, root)
    }

    /// Retry the recorded root offline under the backend GC lock. If the
    /// previous import was incomplete, resupply the same stage with retain_stage.
    pub async fn reconcile_retention<R: Retention + ?Sized>(
        &mut self,
        generation: u64,
        backend: &R,
    ) -> Result<()> {
        let index = self.index(generation)?;
        ensure!(
            backend.domain() == self.state.domain,
            "retention domain mismatch"
        );
        let release = &self.state.releases[index];
        ensure!(
            release.state == ReleaseState::Candidate,
            "release is not a candidate"
        );
        if let Some(root) = release.pending_stage {
            backend.import(&root, &[]).await?;
            self.acknowledge_stage(index, root)?;
        }
        Ok(())
    }

    fn acknowledge_stage(&mut self, index: usize, root: Cid) -> Result<()> {
        let mut next = self.state.clone();
        let release = &mut next.releases[index];
        ensure!(
            release.pending_stage == Some(root),
            "retention acknowledgement does not match pending stage"
        );
        release.pending_stage = None;
        release.stages.push(root);
        self.commit(next)
    }

    pub fn finish_retention(&mut self, generation: u64) -> Result<()> {
        let index = self.index(generation)?;
        let release = &self.state.releases[index];
        if release.state == ReleaseState::Retained {
            return Ok(());
        }
        ensure!(
            release.state == ReleaseState::Candidate,
            "release is not a candidate"
        );
        ensure!(
            release.pending_stage.is_none() && release.stages.contains(&release.publication.root),
            "complete release root has not been acknowledged"
        );
        let mut next = self.state.clone();
        next.releases[index].state = ReleaseState::Retained;
        self.commit(next)
    }

    /// Commit local release authority after complete retention and authentication.
    /// A successful return means the exact request and current release survived
    /// file sync, atomic rename, and directory sync. No remote write occurs here.
    pub async fn commit_publication<P: Publisher + ?Sized>(
        &mut self,
        generation: u64,
        publisher: &P,
    ) -> Result<()> {
        let index = self.index(generation)?;
        ensure!(
            publisher.domain() == self.state.domain,
            "publication domain mismatch"
        );
        let release = &self.state.releases[index];
        if release.state == ReleaseState::Published {
            ensure!(
                self.state.current.as_ref() == Some(&release.publication),
                "publication is no longer current"
            );
            return Ok(());
        }
        ensure!(
            release.state == ReleaseState::Retained,
            "release must be retained before publication"
        );
        ensure!(
            release.previous == self.state.current,
            "stale publication candidate"
        );
        let intent = PublicationIntent {
            previous: release.previous.clone(),
            next: release.publication.clone(),
        };
        let announcement = publisher.prepare(&intent).await?;
        ensure!(
            !announcement.is_empty() && announcement.len() <= MAX_ANNOUNCEMENT_BYTES,
            "invalid announcement size"
        );
        let mut next = self.state.clone();
        for release in &mut next.releases {
            if release.state == ReleaseState::Published {
                release.state = ReleaseState::Retired;
            }
        }
        next.releases[index].announcement = Some(announcement);
        next.releases[index].publishing = true;
        next.releases[index].state = ReleaseState::Published;
        next.current = Some(intent.next);
        self.commit(next)
    }

    /// Commit if needed, then deliver the current immutable request. Every call
    /// can reannounce current state, including after an earlier acknowledgement.
    /// Transport acknowledgement never proves eventual discoverability.
    pub async fn publish<P: Publisher + ?Sized>(
        &mut self,
        generation: u64,
        publisher: &P,
    ) -> Result<()> {
        self.commit_publication(generation, publisher).await?;
        let index = self.index(generation)?;
        let release = &self.state.releases[index];
        let intent = PublicationIntent {
            previous: release.previous.clone(),
            next: release.publication.clone(),
        };
        publisher
            .publish(
                &intent,
                release
                    .announcement
                    .as_deref()
                    .context("missing committed announcement")?,
            )
            .await?;
        if self.state.releases[index].publishing {
            let mut next = self.state.clone();
            next.releases[index].publishing = false;
            self.commit(next)?;
        }
        Ok(())
    }

    /// Retire an inactive candidate. Current local authority cannot be retired.
    /// Superseded announcements may remain uncertain. All pins remain retained.
    pub fn retire(&mut self, generation: u64) -> Result<()> {
        let index = self.index(generation)?;
        let release = &self.state.releases[index];
        ensure!(
            release.state != ReleaseState::Published,
            "cannot retire current publication"
        );
        if release.state == ReleaseState::Retired {
            return Ok(());
        }
        let mut next = self.state.clone();
        next.releases[index].state = ReleaseState::Retired;
        self.commit(next)
    }

    /// CID pins are not exclusive ownership claims. This owner never unpins.
    pub fn prune(&self, generation: u64) -> Result<()> {
        self.index(generation)?;
        anyhow::bail!("pruning refused: exclusive or coordinated pin ownership is not proven")
    }
}

fn validate_domain(domain: &str) -> Result<()> {
    ensure!(
        !domain.is_empty() && domain.len() <= 256 && !domain.chars().any(char::is_control),
        "invalid release retention domain"
    );
    Ok(())
}

impl State {
    fn stage_count(&self) -> usize {
        self.releases
            .iter()
            .map(|r| r.stages.len() + usize::from(r.pending_stage.is_some()))
            .sum()
    }
    fn validate(&self) -> Result<()> {
        ensure!(self.version == 2, "unknown release journal version");
        validate_domain(&self.domain)?;
        ensure!(
            self.releases.len() <= MAX_RELEASES && self.stage_count() <= MAX_STAGES,
            "release journal exceeds bounds"
        );
        let mut active = 0;
        let mut published = None;
        for (index, release) in self.releases.iter().enumerate() {
            ensure!(
                release.publication.generation == index as u64 + 1,
                "invalid release generation sequence"
            );
            graph::validate_cid(&release.publication.root)?;
            let mut stages = std::collections::BTreeSet::new();
            for root in &release.stages {
                graph::validate_cid(root)?;
                ensure!(stages.insert(*root), "duplicate retained stage");
            }
            if let Some(root) = release.pending_stage {
                graph::validate_cid(&root)?;
                ensure!(
                    !stages.contains(&root),
                    "pending stage already acknowledged"
                );
                ensure!(
                    matches!(
                        release.state,
                        ReleaseState::Candidate | ReleaseState::Retired
                    ),
                    "pending stage on completed release"
                );
            }
            if let Some(previous) = &release.previous {
                ensure!(
                    previous.generation < release.publication.generation,
                    "invalid previous generation"
                );
                ensure!(
                    self.releases.iter().any(|r| r.publication == *previous
                        && matches!(r.state, ReleaseState::Published | ReleaseState::Retired)
                        && r.stages.contains(&r.publication.root)),
                    "unknown previous publication"
                );
            }
            if matches!(
                release.state,
                ReleaseState::Retained | ReleaseState::Published
            ) {
                ensure!(
                    stages.contains(&release.publication.root),
                    "release root missing retention acknowledgement"
                );
            }
            ensure!(
                !release.publishing || release.announcement.is_some(),
                "delivery pending without committed announcement"
            );
            if let Some(announcement) = &release.announcement {
                ensure!(
                    !announcement.is_empty() && announcement.len() <= MAX_ANNOUNCEMENT_BYTES,
                    "invalid announcement size"
                );
                ensure!(
                    matches!(
                        release.state,
                        ReleaseState::Published | ReleaseState::Retired
                    ) && stages.contains(&release.publication.root),
                    "announcement without retained local commitment"
                );
            }
            ensure!(
                release.state != ReleaseState::Published || release.announcement.is_some(),
                "current publication has no announcement"
            );
            if matches!(
                release.state,
                ReleaseState::Candidate | ReleaseState::Retained
            ) {
                active += 1;
                ensure!(release.previous == self.current, "stale active candidate");
            }
            if release.state == ReleaseState::Published {
                ensure!(published.is_none(), "multiple current publications");
                published = Some(release.publication.clone());
            }
        }
        ensure!(active <= 1, "multiple active candidates");
        ensure!(
            published == self.current,
            "current publication does not match journal"
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests;
