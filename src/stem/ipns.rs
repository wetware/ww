//! IPNS-backed authoritative deployment source.
//!
//! Transport uncertainty never changes authority. Signed EOL is different:
//! `next()` owns that deadline and emits one [`Update::InvalidHead`] when the
//! current binding expires without a valid replacement.

use std::cmp::Ordering;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use async_trait::async_trait;
use chrono::Utc;
use libp2p::identity::PeerId;

use crate::ipns::{Fetch, RecordStore, RoutingClient, SignedRecord};

use super::{Head, InvalidHead, Source as SourceContract, Update};

const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(5 * 60);
const DEFAULT_RETRY_BASE: Duration = Duration::from_secs(1);
const DEFAULT_RETRY_MAX: Duration = Duration::from_secs(60);

#[derive(Clone, Debug)]
pub struct Config {
    pub name: PeerId,
    pub routing_url: String,
    pub state_dir: PathBuf,
    pub poll_interval: Duration,
    pub retry_base: Duration,
    pub retry_max: Duration,
}

impl Config {
    pub fn new(name: PeerId, routing_url: String, state_dir: PathBuf) -> Self {
        Self {
            name,
            routing_url,
            state_dir,
            poll_interval: DEFAULT_POLL_INTERVAL,
            retry_base: DEFAULT_RETRY_BASE,
            retry_max: DEFAULT_RETRY_MAX,
        }
    }

    #[cfg(test)]
    fn with_timing(mut self, poll_interval: Duration, retry: Duration) -> Self {
        self.poll_interval = poll_interval;
        self.retry_base = retry;
        self.retry_max = retry;
        self
    }
}

pub struct Source {
    config: Config,
    routing: RoutingClient,
    store: RecordStore,
    floor: Option<SignedRecord>,
    last_update: Option<Update>,
    authoritative: bool,
    routing_probed: bool,
    next_fetch: tokio::time::Instant,
    retry_delay: Duration,
}

impl Source {
    pub fn new(config: Config) -> Result<Self> {
        let routing = RoutingClient::new(config.routing_url.clone())?;
        let store = RecordStore::follower(&config.state_dir, config.name);
        let floor = store
            .load()?
            .map(|raw| SignedRecord::decode(config.name, raw))
            .transpose()
            .context("validate persisted IPNS follower watermark")?;
        let retry_delay = config.retry_base;
        Ok(Self {
            config,
            routing,
            store,
            floor,
            last_update: None,
            authoritative: false,
            routing_probed: false,
            next_fetch: tokio::time::Instant::now(),
            retry_delay,
        })
    }

    pub fn watermark_path(&self) -> &std::path::Path {
        self.store.path()
    }

    fn record_update(record: &SignedRecord) -> Update {
        match record.deployment_cid() {
            Some(cid) => Update::Head(Head { cid }),
            None => Update::InvalidHead(InvalidHead {
                selected: record.value().to_vec(),
                reason:
                    "IPNS selected a signed value that is not one /ipfs/<cid> deployment binding"
                        .to_string(),
            }),
        }
    }

    fn accept_initial_floor(&mut self) -> Option<Update> {
        let floor = self.floor.as_ref()?;
        if floor.is_expired_at(Utc::now()) {
            tracing::info!(
                ipns_sequence = floor.sequence(),
                eol = %floor.eol(),
                "Persisted IPNS ordering floor is expired; its value will not seed deployment"
            );
            return None;
        }
        let update = Self::record_update(floor);
        self.last_update = Some(update.clone());
        self.authoritative = true;
        self.schedule_poll();
        Some(update)
    }

    fn schedule_poll(&mut self) {
        let ttl = self
            .floor
            .as_ref()
            .map(SignedRecord::ttl)
            .unwrap_or(self.config.poll_interval);
        let delay = ttl
            .min(self.config.poll_interval)
            .max(Duration::from_millis(1));
        self.next_fetch = tokio::time::Instant::now() + delay;
        self.retry_delay = self.config.retry_base;
    }

    fn schedule_retry(&mut self) {
        self.next_fetch = tokio::time::Instant::now() + self.retry_delay;
        self.retry_delay = self
            .retry_delay
            .saturating_mul(2)
            .min(self.config.retry_max);
    }

    fn expiry_delay(&self) -> Option<Duration> {
        if !self.authoritative {
            return None;
        }
        let eol = self.floor.as_ref()?.eol();
        Some((eol - Utc::now()).to_std().unwrap_or(Duration::ZERO))
    }

    fn expire(&mut self) -> Option<Update> {
        if !self.authoritative {
            return None;
        }
        self.authoritative = false;
        let floor = self
            .floor
            .as_ref()
            .expect("authoritative state has a floor");
        if matches!(self.last_update, Some(Update::InvalidHead(_))) {
            return None;
        }
        let update = Update::InvalidHead(InvalidHead {
            selected: floor.value().to_vec(),
            reason: format!("signed IPNS binding expired at {}", floor.eol()),
        });
        self.last_update = Some(update.clone());
        Some(update)
    }

    /// Validate and apply one fetched raw record.
    ///
    /// `Some(update)` is returned only for a meaningful authoritative state
    /// change. Persistence completes before memory changes or update delivery.
    fn observe(&mut self, raw: Vec<u8>) -> Result<Option<Update>> {
        let candidate = SignedRecord::decode(self.config.name, raw)
            .context("reject malformed or cryptographically invalid IPNS observation")?;

        if let Some(floor) = self.floor.as_ref() {
            if candidate.sequence() == floor.sequence() && candidate.value() != floor.value() {
                tracing::error!(
                    ipns_sequence = candidate.sequence(),
                    accepted_value = %String::from_utf8_lossy(floor.value()),
                    conflicting_value = %String::from_utf8_lossy(candidate.value()),
                    "IPNS publisher equivocation; retaining accepted binding"
                );
                return Ok(None);
            }
        }

        if candidate.is_expired_at(Utc::now()) {
            tracing::warn!(
                ipns_sequence = candidate.sequence(),
                eol = %candidate.eol(),
                "Ignoring already-expired network IPNS record"
            );
            return Ok(None);
        }

        if let Some(floor) = self.floor.as_ref() {
            if candidate.raw() == floor.raw() {
                return Ok(None);
            }
            match candidate.compare(floor)? {
                Ordering::Less => {
                    tracing::debug!(
                        observed_sequence = candidate.sequence(),
                        floor_sequence = floor.sequence(),
                        "Ignoring stale IPNS record"
                    );
                    return Ok(None);
                }
                Ordering::Equal => return Ok(None),
                Ordering::Greater => {}
            }
        }

        let update = Self::record_update(&candidate);
        let emit = !self.authoritative || self.last_update.as_ref() != Some(&update);

        // There is no cancellation point from durable persistence through the
        // state update and return to the Source caller.
        self.store.persist(candidate.raw())?;
        self.floor = Some(candidate);
        self.authoritative = true;
        self.last_update = Some(update.clone());
        self.schedule_poll();

        Ok(emit.then_some(update))
    }

    async fn fetch_once(&mut self) -> Result<Option<Update>> {
        if !self.routing_probed {
            self.routing
                .probe()
                .await
                .context("probe Kubo HTTP Routing V1")?;
            self.routing_probed = true;
        }
        match self.routing.get(self.config.name).await? {
            Fetch::Record(raw) => self.observe(raw),
            Fetch::NotFound => Ok(None),
        }
    }
}

#[async_trait]
impl SourceContract for Source {
    async fn current(&mut self) -> Result<Update> {
        if let Some(update) = self.accept_initial_floor() {
            return Ok(update);
        }
        match self.fetch_once().await? {
            Some(update) => Ok(update),
            None => anyhow::bail!(
                "no current valid IPNS deployment binding exists above the durable ordering floor"
            ),
        }
    }

    async fn next(&mut self) -> Result<Update> {
        loop {
            if let Some(delay) = self.expiry_delay() {
                enum Event {
                    Expired,
                    Fetched(Result<Option<Update>>),
                }
                let next_fetch = self.next_fetch;
                let event = tokio::select! {
                    biased;
                    _ = tokio::time::sleep(delay) => Event::Expired,
                    result = async {
                        tokio::time::sleep_until(next_fetch).await;
                        self.fetch_once().await
                    } => Event::Fetched(result),
                };
                match event {
                    Event::Expired => {
                        if let Some(update) = self.expire() {
                            return Ok(update);
                        }
                    }
                    Event::Fetched(Ok(Some(update))) => return Ok(update),
                    Event::Fetched(Ok(None)) => self.schedule_poll(),
                    Event::Fetched(Err(error)) => {
                        tracing::warn!("IPNS source observation failed; current authority remains unchanged: {error:#}");
                        self.schedule_retry();
                    }
                }
            } else {
                tokio::time::sleep_until(self.next_fetch).await;
                match self.fetch_once().await {
                    Ok(Some(update)) => return Ok(update),
                    Ok(None) => self.schedule_poll(),
                    Err(error) => {
                        tracing::warn!("IPNS source observation failed while no binding is authoritative: {error:#}");
                        self.schedule_retry();
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ipns::{RoutingClient, RECORD_TTL};
    use chrono::Duration as ChronoDuration;
    use cid::multihash::Multihash;
    use cid::Cid;
    use libp2p::identity::Keypair;
    use rust_ipns::Record;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn test_cid(byte: u8) -> Cid {
        Cid::new_v1(0x55, Multihash::<64>::wrap(0x00, &[byte]).unwrap())
    }

    fn signed(keypair: &Keypair, cid: Cid, sequence: u64, lifetime: ChronoDuration) -> Vec<u8> {
        Record::new(
            keypair,
            format!("/ipfs/{cid}"),
            Utc::now() + lifetime,
            sequence,
            RECORD_TTL,
        )
        .unwrap()
        .encode()
        .unwrap()
    }

    fn source(keypair: &Keypair, directory: &std::path::Path) -> Source {
        let name = keypair.public().to_peer_id();
        Source::new(Config::new(
            name,
            "http://127.0.0.1:1".into(),
            directory.to_path_buf(),
        ))
        .unwrap()
    }

    #[test]
    fn ordering_policy_and_persist_before_emit() {
        let directory = tempfile::tempdir().unwrap();
        let keypair = Keypair::generate_ed25519();
        let mut source = source(&keypair, directory.path());
        let first = signed(&keypair, test_cid(1), 3, ChronoDuration::hours(1));
        let first_update = source.observe(first.clone()).unwrap().unwrap();
        assert!(matches!(first_update, Update::Head(_)));
        assert_eq!(std::fs::read(source.watermark_path()).unwrap(), first);

        let stale = signed(&keypair, test_cid(2), 2, ChronoDuration::hours(2));
        assert!(source.observe(stale).unwrap().is_none());

        let duplicate = source.floor.as_ref().unwrap().raw().to_vec();
        assert!(source.observe(duplicate).unwrap().is_none());

        let same_value = signed(&keypair, test_cid(1), 4, ChronoDuration::hours(2));
        assert!(source.observe(same_value.clone()).unwrap().is_none());
        assert_eq!(std::fs::read(source.watermark_path()).unwrap(), same_value);

        let changed = signed(&keypair, test_cid(2), 5, ChronoDuration::hours(2));
        assert!(matches!(
            source.observe(changed.clone()).unwrap(),
            Some(Update::Head(_))
        ));
        assert_eq!(std::fs::read(source.watermark_path()).unwrap(), changed);
    }

    #[test]
    fn persistence_failure_cannot_advance_or_emit() {
        let directory = tempfile::tempdir().unwrap();
        let keypair = Keypair::generate_ed25519();
        let mut source = source(&keypair, directory.path());
        std::fs::write(directory.path().join("ipns"), b"not-a-directory").unwrap();
        let candidate = signed(&keypair, test_cid(1), 1, ChronoDuration::hours(1));

        assert!(source.observe(candidate).is_err());
        assert!(source.floor.is_none());
        assert!(source.last_update.is_none());
        assert!(!source.authoritative);
    }

    #[test]
    fn same_sequence_different_value_is_equivocation() {
        let directory = tempfile::tempdir().unwrap();
        let keypair = Keypair::generate_ed25519();
        let mut source = source(&keypair, directory.path());
        let accepted = signed(&keypair, test_cid(1), 7, ChronoDuration::hours(1));
        source.observe(accepted.clone()).unwrap();

        let conflicting = signed(&keypair, test_cid(2), 7, ChronoDuration::hours(2));
        assert!(source.observe(conflicting).unwrap().is_none());
        assert_eq!(std::fs::read(source.watermark_path()).unwrap(), accepted);
    }

    #[test]
    fn wrong_signer_is_rejected_without_changing_authority() {
        let directory = tempfile::tempdir().unwrap();
        let keypair = Keypair::generate_ed25519();
        let attacker = Keypair::generate_ed25519();
        let mut source = source(&keypair, directory.path());
        let accepted = signed(&keypair, test_cid(1), 3, ChronoDuration::hours(1));
        source.observe(accepted.clone()).unwrap();

        let forged = signed(&attacker, test_cid(2), 4, ChronoDuration::hours(2));
        assert!(source.observe(forged).is_err());
        assert_eq!(std::fs::read(source.watermark_path()).unwrap(), accepted);
        assert!(matches!(source.last_update, Some(Update::Head(_))));
    }

    #[test]
    fn same_value_refresh_updates_deadline_without_emitting() {
        let directory = tempfile::tempdir().unwrap();
        let keypair = Keypair::generate_ed25519();
        let mut source = source(&keypair, directory.path());
        let original = signed(&keypair, test_cid(1), 4, ChronoDuration::minutes(30));
        source.observe(original).unwrap();
        let original_eol = source.floor.as_ref().unwrap().eol();

        let refresh = signed(&keypair, test_cid(1), 4, ChronoDuration::hours(2));
        assert!(source.observe(refresh.clone()).unwrap().is_none());
        assert!(source.floor.as_ref().unwrap().eol() > original_eol);
        assert_eq!(std::fs::read(source.watermark_path()).unwrap(), refresh);
    }

    #[test]
    fn later_valid_record_recovers_after_expiry() {
        let directory = tempfile::tempdir().unwrap();
        let keypair = Keypair::generate_ed25519();
        let mut source = source(&keypair, directory.path());
        let original = signed(&keypair, test_cid(1), 1, ChronoDuration::hours(1));
        source.observe(original).unwrap();
        assert!(matches!(source.expire(), Some(Update::InvalidHead(_))));
        assert!(source.expire().is_none(), "expiry must emit only once");

        let recovery = signed(&keypair, test_cid(1), 2, ChronoDuration::hours(2));
        assert!(matches!(
            source.observe(recovery).unwrap(),
            Some(Update::Head(_))
        ));
    }

    #[test]
    fn restart_rejects_downgrade_and_expired_value_does_not_seed() {
        let directory = tempfile::tempdir().unwrap();
        let keypair = Keypair::generate_ed25519();
        let mut original = source(&keypair, directory.path());
        let current = signed(&keypair, test_cid(1), 9, ChronoDuration::hours(1));
        original.observe(current).unwrap();

        let mut restarted = source(&keypair, directory.path());
        let stale = signed(&keypair, test_cid(2), 8, ChronoDuration::hours(2));
        assert!(restarted.observe(stale).unwrap().is_none());
        assert_eq!(restarted.floor.as_ref().unwrap().sequence(), 9);

        let expired_dir = tempfile::tempdir().unwrap();
        let expired = source(&keypair, expired_dir.path());
        let raw = signed(&keypair, test_cid(1), 10, ChronoDuration::seconds(-1));
        expired.store.persist(&raw).unwrap();
        let mut expired = source(&keypair, expired_dir.path());
        assert!(expired.accept_initial_floor().is_none());
        assert_eq!(expired.floor.as_ref().unwrap().sequence(), 10);
        let lower = signed(&keypair, test_cid(2), 9, ChronoDuration::hours(1));
        assert!(expired.observe(lower).unwrap().is_none());
    }

    async fn response(stream: &mut tokio::net::TcpStream, status: &str, body: &[u8]) {
        let header = format!(
            "HTTP/1.1 {status}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            crate::ipns::IPNS_RECORD_MEDIA_TYPE,
            body.len()
        );
        stream.write_all(header.as_bytes()).await.unwrap();
        stream.write_all(body).await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn signed_eol_wins_over_stalled_transport() {
        let directory = tempfile::tempdir().unwrap();
        let keypair = Keypair::generate_ed25519();
        let name = keypair.public().to_peer_id();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut probe, _) = listener.accept().await.unwrap();
            let mut request = [0_u8; 2048];
            let _ = probe.read(&mut request).await.unwrap();
            response(&mut probe, "400 Bad Request", b"").await;
            let (mut stalled, _) = listener.accept().await.unwrap();
            let _ = stalled.read(&mut request).await.unwrap();
            std::future::pending::<()>().await;
        });

        let config = Config::new(name, format!("http://{address}"), directory.path().into())
            .with_timing(Duration::from_millis(1), Duration::from_secs(60));
        let mut source = Source::new(config).unwrap();
        let raw = signed(&keypair, test_cid(1), 1, ChronoDuration::seconds(5));
        source.observe(raw).unwrap();

        let next = tokio::spawn(async move { source.next().await.unwrap() });
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_millis(1)).await;
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(6)).await;
        let update = next.await.unwrap();
        assert!(matches!(update, Update::InvalidHead(_)));
        server.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn transport_outage_before_eol_does_not_revoke_early() {
        let directory = tempfile::tempdir().unwrap();
        let keypair = Keypair::generate_ed25519();
        let name = keypair.public().to_peer_id();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            for status in ["400 Bad Request", "503 Service Unavailable"] {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut request = [0_u8; 2048];
                let _ = stream.read(&mut request).await.unwrap();
                response(&mut stream, status, b"").await;
            }
            std::future::pending::<()>().await;
        });
        let config = Config::new(name, format!("http://{address}"), directory.path().into())
            .with_timing(Duration::from_millis(1), Duration::from_secs(60));
        let mut source = Source::new(config).unwrap();
        source
            .observe(signed(
                &keypair,
                test_cid(1),
                1,
                ChronoDuration::seconds(10),
            ))
            .unwrap();

        let next = tokio::spawn(async move { source.next().await.unwrap() });
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(5)).await;
        tokio::task::yield_now().await;
        assert!(!next.is_finished());
        next.abort();
        server.abort();
    }

    #[tokio::test]
    async fn newer_refresh_replaces_the_active_eol_deadline() {
        let directory = tempfile::tempdir().unwrap();
        let keypair = Keypair::generate_ed25519();
        let name = keypair.public().to_peer_id();
        let refreshed = signed(&keypair, test_cid(1), 1, ChronoDuration::seconds(2));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (refreshed_tx, refreshed_rx) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            for (status, body) in [("400 Bad Request", Vec::new()), ("200 OK", refreshed)] {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut request = [0_u8; 2048];
                let _ = stream.read(&mut request).await.unwrap();
                response(&mut stream, status, &body).await;
            }
            refreshed_tx.send(()).unwrap();
            let (_stalled, _) = listener.accept().await.unwrap();
            std::future::pending::<()>().await;
        });
        let config = Config::new(name, format!("http://{address}"), directory.path().into())
            .with_timing(Duration::from_millis(1), Duration::from_secs(60));
        let mut source = Source::new(config).unwrap();
        source
            .observe(signed(
                &keypair,
                test_cid(1),
                1,
                ChronoDuration::milliseconds(500),
            ))
            .unwrap();

        let next = tokio::spawn(async move { source.next().await.unwrap() });
        refreshed_rx.await.unwrap();
        tokio::task::yield_now().await;
        tokio::time::sleep(Duration::from_millis(700)).await;
        assert!(!next.is_finished(), "the replaced EOL fired");
        next.abort();
        server.abort();
    }

    #[tokio::test]
    async fn next_fetch_loop_recovers_after_expiry() {
        let directory = tempfile::tempdir().unwrap();
        let keypair = Keypair::generate_ed25519();
        let name = keypair.public().to_peer_id();
        let recovery = signed(&keypair, test_cid(2), 2, ChronoDuration::hours(1));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            for (status, body) in [("400 Bad Request", Vec::new()), ("200 OK", recovery)] {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut request = [0_u8; 2048];
                let _ = stream.read(&mut request).await.unwrap();
                response(&mut stream, status, &body).await;
            }
        });
        let config = Config::new(name, format!("http://{address}"), directory.path().into())
            .with_timing(Duration::from_millis(100), Duration::from_millis(10));
        let mut source = Source::new(config).unwrap();
        source
            .observe(signed(
                &keypair,
                test_cid(1),
                1,
                ChronoDuration::milliseconds(50),
            ))
            .unwrap();

        assert!(matches!(
            source.next().await.unwrap(),
            Update::InvalidHead(_)
        ));
        let recovery = tokio::time::timeout(Duration::from_secs(1), source.next())
            .await
            .expect("IPNS recovery timed out")
            .unwrap();
        assert!(matches!(recovery, Update::Head(_)));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn routing_probe_distinguishes_disabled_endpoint() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0_u8; 2048];
            let _ = stream.read(&mut request).await.unwrap();
            response(&mut stream, "404 Not Found", b"not found").await;
        });
        let routing = RoutingClient::new(format!("http://{address}")).unwrap();
        let error = routing.probe().await.unwrap_err();
        assert_eq!(error.kind(), crate::ipns::RoutingErrorKind::Unsupported);
        server.await.unwrap();
    }
}
