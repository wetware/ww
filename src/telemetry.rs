//! Opt-in, local OTLP/HTTP JSON export for finite Cell lifecycle spans.
//!
//! The exporter never renders untrusted Display/Debug values. It shares the
//! structured-log visitor and admits a fixed number of bounded records. Network
//! work runs on a dedicated thread; runtime callbacks only use nonblocking sends.
use std::{
    fmt::Write,
    net::IpAddr,
    sync::{
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering::Relaxed},
        mpsc, Arc, Mutex, OnceLock,
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use serde_json::{json, Map, Value};
use tracing::{span, Event, Subscriber};
use tracing_subscriber::{layer::Context, registry::LookupSpan, Layer};

const QUEUE_CAPACITY: usize = 256;
const ACTIVE_CAPACITY: usize = 256;
const MAX_SPAN_BYTES: usize = 16_384;
const MAX_FIELDS_BYTES: usize = 4096;
const MAX_EVENTS: usize = 16;
const MAX_LINKS: usize = 8;
const BATCH_SIZE: usize = 32;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(1);
const DRAIN_TIMEOUT: Duration = Duration::from_millis(1500);
const JOIN_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Default)]
struct Counters {
    sampled_out: AtomicU64,
    admitted: AtomicU64,
    active: AtomicUsize,
    queue_dropped: AtomicU64,
    limit_dropped: AtomicU64,
    record_dropped: AtomicU64,
    export_dropped: AtomicU64,
    shutdown_dropped: AtomicU64,
    exported: AtomicU64,
    bytes: AtomicU64,
}

static COUNTERS: Counters = Counters {
    sampled_out: AtomicU64::new(0),
    admitted: AtomicU64::new(0),
    active: AtomicUsize::new(0),
    queue_dropped: AtomicU64::new(0),
    limit_dropped: AtomicU64::new(0),
    record_dropped: AtomicU64::new(0),
    export_dropped: AtomicU64::new(0),
    shutdown_dropped: AtomicU64::new(0),
    exported: AtomicU64::new(0),
    bytes: AtomicU64::new(0),
};

/// Append exporter counters with fixed, bounded reason labels.
pub fn render_metrics(out: &mut String) {
    for (name, help, value) in [
        (
            "sampled_out",
            "Cell roots excluded by sampling.",
            COUNTERS.sampled_out.load(Relaxed),
        ),
        (
            "admitted",
            "Spans admitted before subsequent queue or delivery loss.",
            COUNTERS.admitted.load(Relaxed),
        ),
        (
            "exported",
            "Spans accepted by the collector; does not establish Tempo delivery.",
            COUNTERS.exported.load(Relaxed),
        ),
        (
            "request_bytes",
            "Attempted OTLP HTTP JSON body bytes, excluding network overhead.",
            COUNTERS.bytes.load(Relaxed),
        ),
    ] {
        let _ = writeln!(
            out,
            "# HELP ww_telemetry_{name}_total {help}\n# TYPE ww_telemetry_{name}_total counter\nww_telemetry_{name}_total {value}"
        );
    }
    let _ = writeln!(
        out,
        "# HELP ww_telemetry_active_spans Active sampled spans retained by the local exporter.\n# TYPE ww_telemetry_active_spans gauge\nww_telemetry_active_spans {}",
        COUNTERS.active.load(Relaxed)
    );
    let _ = writeln!(out, "# HELP ww_telemetry_dropped_total Lost spans, or event and field records for reason record.\n# TYPE ww_telemetry_dropped_total counter");
    for (reason, counter) in [
        ("queue", &COUNTERS.queue_dropped),
        ("limit", &COUNTERS.limit_dropped),
        ("record", &COUNTERS.record_dropped),
        ("export", &COUNTERS.export_dropped),
        ("shutdown", &COUNTERS.shutdown_dropped),
    ] {
        let _ = writeln!(
            out,
            "ww_telemetry_dropped_total{{reason=\"{reason}\"}} {}",
            counter.load(Relaxed)
        );
    }
}

/// Explicit local-only configuration. No credentials, DNS, proxies or redirects.
#[derive(Clone, Debug)]
pub struct Config {
    endpoint: reqwest::Url,
    sample_ratio: f64,
    spans_per_second: u32,
}

impl Config {
    pub fn new(endpoint: &str, sample_ratio: f64, spans_per_second: u32) -> anyhow::Result<Self> {
        let endpoint =
            reqwest::Url::parse(endpoint).map_err(|_| anyhow::anyhow!("invalid OTLP endpoint"))?;
        let local = endpoint
            .host_str()
            .and_then(|host| host.trim_matches(['[', ']']).parse::<IpAddr>().ok())
            .is_some_and(|ip| ip.is_loopback());
        anyhow::ensure!(endpoint.scheme() == "http" && local && endpoint.username().is_empty() && endpoint.password().is_none() && endpoint.query().is_none() && endpoint.fragment().is_none() && endpoint.path() == "/v1/traces", "OTLP endpoint must be an HTTP loopback IP with path /v1/traces and no credentials or query");
        anyhow::ensure!(
            sample_ratio.is_finite() && (0.0..=1.0).contains(&sample_ratio),
            "trace sample ratio must be between 0 and 1"
        );
        anyhow::ensure!(
            (1..=10_000).contains(&spans_per_second),
            "trace span limit must be between 1 and 10000"
        );
        Ok(Self {
            endpoint,
            sample_ratio,
            spans_per_second,
        })
    }

    /// Absence of WW_OTLP_ENDPOINT disables all exporter work.
    pub fn from_env() -> anyhow::Result<Option<Self>> {
        let Some(endpoint) = std::env::var_os("WW_OTLP_ENDPOINT") else {
            return Ok(None);
        };
        let endpoint = endpoint
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("invalid OTLP endpoint"))?;
        let ratio = std::env::var("WW_TRACE_SAMPLE_RATIO")
            .unwrap_or_else(|_| "0.01".into())
            .parse()
            .map_err(|_| anyhow::anyhow!("invalid trace sample ratio"))?;
        let limit = std::env::var("WW_TRACE_MAX_SPANS_PER_SECOND")
            .unwrap_or_else(|_| "100".into())
            .parse()
            .map_err(|_| anyhow::anyhow!("invalid trace span limit"))?;
        Self::new(endpoint, ratio, limit).map(Some)
    }
}

#[derive(Clone)]
pub(crate) struct TraceContext {
    pub(crate) trace_id: String,
    pub(crate) span_id: String,
}

struct SpanData {
    context: TraceContext,
    parent: Option<String>,
    name: String,
    start: SystemTime,
    elapsed: Instant,
    fields: Map<String, Value>,
    events: Vec<Value>,
    links: Vec<Value>,
    bytes: usize,
    dropped_events: u32,
    dropped_links: u32,
}

struct Shared {
    sender: tokio::sync::mpsc::Sender<String>,
    stop: AtomicBool,
    shutdown_deadline: OnceLock<tokio::time::Instant>,
    rate: Mutex<(Instant, u32)>,
    active: AtomicUsize,
    config: Config,
}

/// The layer exports only spans rooted in a finite `cell` span.
#[derive(Clone)]
pub struct TraceLayer(Arc<Shared>);

/// Dropping the guard requests a bounded drain. Delivery is best effort.
pub struct TelemetryGuard {
    shared: Arc<Shared>,
    done: mpsc::Receiver<()>,
}

impl Drop for TelemetryGuard {
    fn drop(&mut self) {
        let _ = self
            .shared
            .shutdown_deadline
            .set(tokio::time::Instant::now() + DRAIN_TIMEOUT);
        self.shared
            .stop
            .store(true, std::sync::atomic::Ordering::Release);
        // Never wait for thread join or a collector without a deadline.
        let _ = self.done.recv_timeout(JOIN_TIMEOUT);
    }
}

/// Start a dedicated exporter thread. No Tokio runtime is required by the caller.
pub fn start(config: Config) -> anyhow::Result<(TraceLayer, TelemetryGuard)> {
    let client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .http1_only()
        .timeout(REQUEST_TIMEOUT)
        .pool_max_idle_per_host(0)
        .build()?;
    let (sender, receiver) = tokio::sync::mpsc::channel(QUEUE_CAPACITY);
    let shared = Arc::new(Shared {
        sender,
        stop: AtomicBool::new(false),
        shutdown_deadline: OnceLock::new(),
        rate: Mutex::new((Instant::now(), 0)),
        active: AtomicUsize::new(0),
        config,
    });
    let (done_tx, done) = mpsc::sync_channel(1);
    let worker_shared = shared.clone();
    std::thread::Builder::new()
        .name("ww-otlp".into())
        .spawn(move || {
            if let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                runtime.block_on(export_loop(client, worker_shared, receiver));
            }
            // Runtime construction failure closes the receiver. Subsequent
            // submissions report queue loss without affecting Cell execution.
            let _ = done_tx.send(());
        })?;
    Ok((TraceLayer(shared.clone()), TelemetryGuard { shared, done }))
}

fn nanos(time: SystemTime) -> String {
    time.duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
        .min(u64::MAX as u128)
        .to_string()
}

fn safe_fields(fields: impl tracing_subscriber::field::RecordFields) -> Map<String, Value> {
    let mut visitor = crate::logging::Fields::default();
    fields.record(&mut visitor);
    let values = visitor.finish();
    if serde_json::to_vec(&values).is_ok_and(|bytes| bytes.len() <= MAX_FIELDS_BYTES) {
        values
    } else {
        COUNTERS.record_dropped.fetch_add(1, Relaxed);
        Map::from_iter([("fields_truncated".into(), true.into())])
    }
}

fn attributes(fields: &Map<String, Value>) -> Vec<Value> {
    fields
        .iter()
        .map(|(key, value)| {
            let value = match value {
                Value::Bool(value) => json!({"boolValue": value}),
                Value::Number(value) if value.as_i64().is_some() => {
                    json!({"intValue": value.to_string()})
                }
                // OTLP intValue is signed. Preserve approved u64 values losslessly.
                Value::Number(value) => json!({"stringValue": value.to_string()}),
                Value::String(value) => json!({"stringValue": value}),
                _ => json!({"stringValue": value.to_string()}),
            };
            json!({"key": key, "value": value})
        })
        .collect()
}

impl<S> Layer<S> for TraceLayer
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_new_span(&self, attrs: &span::Attributes<'_>, id: &span::Id, ctx: Context<'_, S>) {
        if self.0.stop.load(Relaxed) {
            return;
        }
        let parent = attrs.parent().and_then(|id| ctx.span(id)).or_else(|| {
            attrs
                .is_contextual()
                .then(|| ctx.lookup_current())
                .flatten()
        });
        let inherited = parent.and_then(|span| span.extensions().get::<TraceContext>().cloned());
        let root = attrs.metadata().name() == "cell";
        if !root && inherited.is_none() {
            return;
        }
        if root && rand::random::<f64>() >= self.0.config.sample_ratio {
            COUNTERS.sampled_out.fetch_add(1, Relaxed);
            return;
        }
        // try_lock avoids waiting behind another telemetry callback.
        let Ok(mut rate) = self.0.rate.try_lock() else {
            COUNTERS.limit_dropped.fetch_add(1, Relaxed);
            return;
        };
        if rate.0.elapsed() >= Duration::from_secs(1) {
            *rate = (Instant::now(), 0);
        }
        if rate.1 >= self.0.config.spans_per_second
            || self.0.active.load(Relaxed) >= ACTIVE_CAPACITY
        {
            COUNTERS.limit_dropped.fetch_add(1, Relaxed);
            return;
        }
        rate.1 += 1;
        self.0.active.fetch_add(1, Relaxed);
        drop(rate);
        COUNTERS.active.fetch_add(1, Relaxed);
        COUNTERS.admitted.fetch_add(1, Relaxed);
        let context = TraceContext {
            trace_id: if root {
                format!("{:032x}", rand::random::<u128>().max(1))
            } else {
                inherited.as_ref().unwrap().trace_id.clone()
            },
            span_id: format!("{:016x}", rand::random::<u64>().max(1)),
        };
        let fields = safe_fields(attrs);
        let bytes = serde_json::to_vec(&fields).unwrap().len();
        let data = SpanData {
            context: context.clone(),
            parent: if root {
                None
            } else {
                inherited.map(|parent| parent.span_id)
            },
            name: crate::logging::bounded_metadata(attrs.metadata().name()),
            start: SystemTime::now(),
            elapsed: Instant::now(),
            fields,
            events: Vec::new(),
            links: Vec::new(),
            bytes,
            dropped_events: 0,
            dropped_links: 0,
        };
        if let Some(span) = ctx.span(id) {
            let mut extensions = span.extensions_mut();
            extensions.insert(context);
            extensions.insert(data);
        }
    }

    fn on_record(&self, id: &span::Id, values: &span::Record<'_>, ctx: Context<'_, S>) {
        if let Some(span) = ctx.span(id) {
            if let Some(data) = span.extensions_mut().get_mut::<SpanData>() {
                // Reuse the same allowlist, including late-recorded fields.
                let update = safe_fields(values);
                let mut merged = data.fields.clone();
                merged.extend(update);
                let old_size = serde_json::to_vec(&data.fields).unwrap().len();
                let new_size = serde_json::to_vec(&merged).unwrap().len();
                if merged.len() <= 33 && new_size <= MAX_FIELDS_BYTES {
                    data.fields = merged;
                    data.bytes = data.bytes - old_size + new_size;
                } else {
                    COUNTERS.record_dropped.fetch_add(1, Relaxed);
                }
            }
        }
    }

    fn on_event(&self, event: &Event<'_>, ctx: Context<'_, S>) {
        let Some(span) = ctx.event_span(event) else {
            return;
        };
        let mut extensions = span.extensions_mut();
        let Some(data) = extensions.get_mut::<SpanData>() else {
            return;
        };
        if data.events.len() == MAX_EVENTS {
            data.dropped_events = data.dropped_events.saturating_add(1);
            COUNTERS.record_dropped.fetch_add(1, Relaxed);
            return;
        }
        let fields = safe_fields(event);
        let code = fields.get("event_code").and_then(Value::as_u64);
        let name = code
            .and_then(rpc::observability::contract)
            .map_or("unclassified event (text redacted)", |c| c.message);
        let event = json!({"timeUnixNano": nanos(SystemTime::now()), "name": name, "attributes": attributes(&fields)});
        let bytes = event.to_string().len();
        if data.bytes + bytes > MAX_SPAN_BYTES / 2 {
            data.dropped_events = data.dropped_events.saturating_add(1);
            COUNTERS.record_dropped.fetch_add(1, Relaxed);
            return;
        }
        data.bytes += bytes;
        data.events.push(event);
    }

    fn on_follows_from(&self, id: &span::Id, follows: &span::Id, ctx: Context<'_, S>) {
        let Some(context) = ctx
            .span(follows)
            .and_then(|span| span.extensions().get::<TraceContext>().cloned())
        else {
            return;
        };
        if let Some(span) = ctx.span(id) {
            if let Some(data) = span.extensions_mut().get_mut::<SpanData>() {
                if data.links.len() < MAX_LINKS {
                    data.links
                        .push(json!({"traceId": context.trace_id, "spanId": context.span_id}));
                } else {
                    data.dropped_links = data.dropped_links.saturating_add(1);
                    COUNTERS.record_dropped.fetch_add(1, Relaxed);
                }
            }
        }
    }

    fn on_close(&self, id: span::Id, ctx: Context<'_, S>) {
        let Some(span) = ctx.span(&id) else {
            return;
        };
        let Some(data) = span.extensions_mut().remove::<SpanData>() else {
            return;
        };
        self.0.active.fetch_sub(1, Relaxed);
        COUNTERS.active.fetch_sub(1, Relaxed);
        let mut record = json!({"traceId": data.context.trace_id, "spanId": data.context.span_id, "name": data.name,
            "kind": 1, "startTimeUnixNano": nanos(data.start), "endTimeUnixNano": nanos(data.start + data.elapsed.elapsed()),
            "attributes": attributes(&data.fields), "events": data.events, "links": data.links,
            "droppedEventsCount": data.dropped_events, "droppedLinksCount": data.dropped_links});
        if let Some(parent) = data.parent {
            record["parentSpanId"] = parent.into();
        }
        let record = record.to_string();
        if record.len() > MAX_SPAN_BYTES {
            COUNTERS.record_dropped.fetch_add(1, Relaxed);
        } else if self.0.stop.load(Relaxed) {
            COUNTERS.shutdown_dropped.fetch_add(1, Relaxed);
        } else if self.0.sender.try_send(record).is_err() {
            COUNTERS.queue_dropped.fetch_add(1, Relaxed);
        }
    }
}

async fn export_loop(
    client: reqwest::Client,
    shared: Arc<Shared>,
    mut receiver: tokio::sync::mpsc::Receiver<String>,
) {
    let mut drain_deadline = None;
    loop {
        if shared.stop.load(std::sync::atomic::Ordering::Acquire) && drain_deadline.is_none() {
            receiver.close();
            drain_deadline = shared.shutdown_deadline.get().copied();
        }
        if drain_deadline.is_some_and(|deadline| tokio::time::Instant::now() >= deadline) {
            COUNTERS
                .shutdown_dropped
                .fetch_add(receiver.len() as u64, Relaxed);
            return;
        }
        let first = tokio::select! {
            value = receiver.recv() => value,
            _ = tokio::time::sleep(Duration::from_millis(20)) => continue,
        };
        let Some(first) = first else {
            return;
        };
        let mut batch = vec![first];
        while batch.len() < BATCH_SIZE {
            match receiver.try_recv() {
                Ok(span) => batch.push(span),
                Err(_) => break,
            }
        }
        let count = batch.len() as u64;
        let resource = json!({"attributes": [
            {"key": "service.name", "value": {"stringValue": "wetware"}},
            {"key": "service.version", "value": {"stringValue": crate::VERSION}},
            {"key": "ww.git_commit", "value": {"stringValue": crate::GIT_COMMIT}},
        ]});
        let body = format!("{{\"resourceSpans\":[{{\"resource\":{resource},\"scopeSpans\":[{{\"scope\":{{\"name\":\"wetware.cell\"}},\"spans\":[{}]}}]}}]}}", batch.join(","));
        COUNTERS.bytes.fetch_add(body.len() as u64, Relaxed);
        let deadline =
            drain_deadline.unwrap_or_else(|| tokio::time::Instant::now() + REQUEST_TIMEOUT);
        // An in-flight request may finish within its existing one-second bound.
        // The drain deadline starts at guard drop, not after this request ends.
        let response = tokio::time::timeout_at(
            deadline,
            send(&client, &shared.config.endpoint, body, count),
        )
        .await;
        let rejected = response.ok().and_then(Result::ok).unwrap_or(count);
        COUNTERS.exported.fetch_add(count - rejected, Relaxed);
        COUNTERS.export_dropped.fetch_add(rejected, Relaxed);
    }
}

async fn send(
    client: &reqwest::Client,
    endpoint: &reqwest::Url,
    body: String,
    count: u64,
) -> anyhow::Result<u64> {
    let mut response = client
        .post(endpoint.clone())
        .header("content-type", "application/json")
        .body(body)
        .send()
        .await?;
    anyhow::ensure!(
        response.status() == reqwest::StatusCode::OK,
        "OTLP request failed"
    );
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        anyhow::ensure!(
            body.len() + chunk.len() <= MAX_FIELDS_BYTES,
            "OTLP response too large"
        );
        body.extend_from_slice(&chunk);
    }
    let value: Value = serde_json::from_slice(&body)?;
    anyhow::ensure!(value.is_object(), "invalid OTLP response");
    let rejected = value.pointer("/partialSuccess/rejectedSpans");
    Ok(match rejected {
        None => 0,
        Some(Value::String(value)) => value.parse::<u64>()?.min(count),
        Some(Value::Number(value)) => value
            .as_u64()
            .ok_or_else(|| anyhow::anyhow!("invalid rejected span count"))?
            .min(count),
        _ => anyhow::bail!("invalid rejected span count"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write as IoWrite};
    use tracing::Instrument;
    use tracing_subscriber::layer::SubscriberExt;

    fn layer(ratio: f64, limit: u32) -> (TraceLayer, tokio::sync::mpsc::Receiver<String>) {
        let (sender, receiver) = tokio::sync::mpsc::channel(QUEUE_CAPACITY);
        let shared = Shared {
            sender,
            stop: AtomicBool::new(false),
            shutdown_deadline: OnceLock::new(),
            rate: Mutex::new((Instant::now(), 0)),
            active: AtomicUsize::new(0),
            config: Config::new("http://127.0.0.1:4318/v1/traces", ratio, limit).unwrap(),
        };
        (TraceLayer(Arc::new(shared)), receiver)
    }

    fn drain(receiver: &mut tokio::sync::mpsc::Receiver<String>) -> Vec<Value> {
        let mut spans = Vec::new();
        while let Ok(span) = receiver.try_recv() {
            spans.push(serde_json::from_str(&span).unwrap());
        }
        spans
    }

    #[test]
    fn only_explicit_local_configuration_is_accepted() {
        for endpoint in [
            "https://127.0.0.1/v1/traces",
            "http://localhost/v1/traces",
            "http://192.0.2.1/v1/traces",
            "http://secret@127.0.0.1/v1/traces",
            "http://127.0.0.1/v1/traces?token=secret",
            "http://127.0.0.1/wrong",
        ] {
            assert!(Config::new(endpoint, 0.01, 100).is_err(), "{endpoint}");
        }
        assert!(Config::new("http://[::1]:4318/v1/traces", 0.01, 100).is_ok());
        assert!(Config::new("http://127.0.0.1/v1/traces", f64::NAN, 100).is_err());
        assert!(Config::new("http://127.0.0.1/v1/traces", 1., 10_001).is_err());
    }

    #[test]
    fn zero_sampling_and_unsampled_parents_do_not_export_children() {
        let (layer, mut receiver) = layer(0., 100);
        let subscriber = tracing_subscriber::registry().with(layer);
        tracing::subscriber::with_default(subscriber, || {
            let span = tracing::info_span!("cell", owner_id = 1);
            let _guard = span.enter();
            let child = tracing::info_span!("child");
            child.in_scope(|| tracing::warn!(event_code = 300u64));
        });
        assert!(drain(&mut receiver).is_empty());
    }

    #[test]
    fn active_records_and_completed_queue_are_bounded_without_a_consumer() {
        let (layer, mut receiver) = layer(1., 10_000);
        let shared = layer.0.clone();
        let subscriber = tracing_subscriber::registry().with(layer);
        tracing::subscriber::with_default(subscriber, || {
            let spans: Vec<_> = (0..ACTIVE_CAPACITY + 100)
                .map(|owner_id| tracing::info_span!("cell", owner_id))
                .collect();
            assert_eq!(shared.active.load(Relaxed), ACTIVE_CAPACITY);
            drop(spans);
            assert_eq!(shared.active.load(Relaxed), 0);
            for _ in 0..1000 {
                drop(tracing::info_span!("cell"));
            }
            assert_eq!(receiver.len(), QUEUE_CAPACITY);
        });
        assert_eq!(drain(&mut receiver).len(), QUEUE_CAPACITY);
    }

    #[test]
    fn span_rate_limit_bounds_admission() {
        let (layer, mut receiver) = layer(1., 2);
        tracing::subscriber::with_default(tracing_subscriber::registry().with(layer), || {
            for _ in 0..20 {
                drop(tracing::info_span!("cell"));
            }
        });
        assert_eq!(drain(&mut receiver).len(), 2);
    }

    #[test]
    fn interleaved_tasks_explicit_parents_links_and_redaction_remain_correct() {
        let (layer, mut receiver) = layer(1., 100);
        let subscriber = tracing_subscriber::registry().with(layer);
        tracing::subscriber::with_default(subscriber, || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(async {
                let first = tracing::info_span!("cell", owner_id = 11u64, secret = "do-not-export");
                let second = tracing::info_span!("cell", owner_id = 22u64);
                second.follows_from(&first);
                let unrelated = tracing::info_span!(parent: None, "unrelated");
                let a = async {
                    tracing::warn!(event_code = 300u64, count = 11u64, "do-not-export");
                    tokio::task::yield_now().await;
                    tracing::warn!(parent: &second, event_code = 300u64, count = 22u64);
                    tracing::warn!(parent: None, count = 999u64);
                    unrelated.in_scope(|| tracing::warn!(count = 888u64));
                    let child = tracing::info_span!("child", count = tracing::field::Empty);
                    child.record("count", 44u64);
                    child.in_scope(|| tracing::warn!(count = 44u64, secret = "do-not-export"));
                }
                .instrument(first.clone());
                let b = async {
                    tokio::task::yield_now().await;
                    tracing::warn!(event_code = 300u64, count = 22u64);
                }
                .instrument(second.clone());
                tokio::join!(a, b);
            });
        });
        let spans = drain(&mut receiver);
        assert_eq!(spans.len(), 3);
        assert!(!serde_json::to_string(&spans)
            .unwrap()
            .contains("do-not-export"));
        let root = |owner: &str| {
            spans
                .iter()
                .find(|span| {
                    span["attributes"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|a| a["key"] == "owner_id" && a["value"]["intValue"] == owner)
                })
                .unwrap()
        };
        let first = root("11");
        let second = root("22");
        assert_ne!(first["traceId"], second["traceId"]);
        assert_eq!(first["events"].as_array().unwrap().len(), 1);
        assert_eq!(second["events"].as_array().unwrap().len(), 2);
        assert_eq!(second["links"][0]["traceId"], first["traceId"]);
        assert_eq!(second["links"][0]["spanId"], first["spanId"]);
        let child = spans.iter().find(|span| span["name"] == "child").unwrap();
        assert_eq!(child["traceId"], first["traceId"]);
        assert_eq!(child["parentSpanId"], first["spanId"]);
        for span in &spans {
            for event in span["events"].as_array().unwrap() {
                for attribute in event["attributes"].as_array().unwrap() {
                    assert_ne!(attribute["value"]["intValue"], "999");
                    assert_ne!(attribute["value"]["intValue"], "888");
                }
            }
        }
    }

    #[test]
    fn oversized_fields_and_event_volume_are_bounded() {
        let (layer, mut receiver) = layer(1., 100);
        tracing::subscriber::with_default(tracing_subscriber::registry().with(layer), || {
            let span =
                tracing::info_span!("cell", count = u64::MAX, secret = tracing::field::Empty);
            span.record("secret", "never-export".repeat(100_000));
            span.in_scope(|| {
                for _ in 0..1000 {
                    tracing::warn!(event_code = 300u64, error = "never-export");
                }
            });
        });
        let spans = drain(&mut receiver);
        assert_eq!(spans.len(), 1);
        assert!(spans[0].to_string().len() <= MAX_SPAN_BYTES);
        assert_eq!(spans[0]["droppedEventsCount"], 984);
        assert!(!spans[0].to_string().contains("never-export"));
        assert!(spans[0]["attributes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a["key"] == "count"
                && a["value"]["stringValue"].as_str() == Some("18446744073709551615")));
    }

    #[test]
    fn stalled_collector_does_not_block_callbacks_or_shutdown() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let (accepted_tx, accepted) = mpsc::sync_channel(1);
        let (release, released) = mpsc::sync_channel(1);
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            accepted_tx.send(()).unwrap();
            let _ = released.recv_timeout(Duration::from_secs(5));
            drop(stream);
        });
        let (layer, guard) =
            start(Config::new(&format!("http://{address}/v1/traces"), 1., 10_000).unwrap())
                .unwrap();
        let subscriber = tracing_subscriber::registry().with(layer);
        tracing::subscriber::with_default(subscriber, || {
            drop(tracing::info_span!("cell"));
            accepted.recv_timeout(Duration::from_secs(2)).unwrap();
            let start = Instant::now();
            for _ in 0..2000 {
                drop(tracing::info_span!("cell"));
            }
            assert!(
                start.elapsed() < Duration::from_secs(1),
                "callbacks waited for collector"
            );
            let start = Instant::now();
            drop(guard);
            assert!(start.elapsed() < JOIN_TIMEOUT + Duration::from_millis(200));
        });
        let _ = release.send(());
        server.join().unwrap();
    }

    #[test]
    fn shutdown_allows_a_healthy_in_flight_request_to_finish() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!("http://{}/v1/traces", listener.local_addr().unwrap());
        let (accepted_tx, accepted) = mpsc::sync_channel(1);
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut buffer = [0u8; 4096];
            let _ = stream.read(&mut buffer).unwrap();
            accepted_tx.send(()).unwrap();
            std::thread::sleep(Duration::from_millis(100));
            stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}").unwrap();
        });
        let before = COUNTERS.exported.load(Relaxed);
        let (layer, guard) = start(Config::new(&endpoint, 1., 100).unwrap()).unwrap();
        tracing::subscriber::with_default(tracing_subscriber::registry().with(layer), || {
            drop(tracing::info_span!("cell"));
            accepted.recv_timeout(Duration::from_secs(2)).unwrap();
            drop(guard);
        });
        server.join().unwrap();
        assert!(COUNTERS.exported.load(Relaxed) > before);
    }

    #[test]
    fn partial_success_and_failed_transport_are_counted() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = reqwest::Url::parse(&format!(
            "http://{}/v1/traces",
            listener.local_addr().unwrap()
        ))
        .unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut buffer = [0u8; 4096];
            let _ = stream.read(&mut buffer).unwrap();
            let body = r#"{"partialSuccess":{"rejectedSpans":"2","errorMessage":"secret"}}"#;
            write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
        });
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let client = reqwest::Client::builder()
                .no_proxy()
                .timeout(REQUEST_TIMEOUT)
                .build()
                .unwrap();
            assert_eq!(send(&client, &endpoint, "{}".into(), 3).await.unwrap(), 2);
            server.join().unwrap();
            assert!(send(&client, &endpoint, "{}".into(), 3).await.is_err());
        });
    }
    #[test]
    #[ignore = "requires the local collector at 127.0.0.1:14318"]
    fn local_collector_accepts_maximum_u64_as_string_attribute() {
        let before = COUNTERS.exported.load(Relaxed);
        let (layer, guard) =
            start(Config::new("http://127.0.0.1:14318/v1/traces", 1., 100).unwrap()).unwrap();
        tracing::subscriber::with_default(tracing_subscriber::registry().with(layer), || {
            let span = tracing::info_span!("cell", count = u64::MAX);
            span.in_scope(|| tracing::warn!(event_code = 300u64, elapsed_ms = u64::MAX));
        });
        let deadline = Instant::now() + Duration::from_secs(3);
        while COUNTERS.exported.load(Relaxed) == before && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        drop(guard);
        assert_eq!(COUNTERS.exported.load(Relaxed), before + 1);
    }
}
