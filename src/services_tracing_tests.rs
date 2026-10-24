//! Exercise the actual worker loop with deterministic cross-thread handshakes.

use super::*;
use std::collections::BTreeMap;
use std::task::Poll;
use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id};
use tracing::{Event, Subscriber};
use tracing_subscriber::fmt::writer::{MakeWriter, MutexGuardWriter};
use tracing_subscriber::layer::{Context as LayerContext, SubscriberExt};
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::Layer;

#[derive(Default)]
struct Fields(BTreeMap<String, String>);

impl Visit for Fields {
    fn record_str(&mut self, field: &Field, value: &str) {
        self.0.insert(field.name().into(), value.into());
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.0.insert(field.name().into(), format!("{value:?}"));
    }
}

struct RecordedSpan {
    kind: &'static str,
    fields: Fields,
    parent: Option<Id>,
    enters: usize,
    exits: usize,
    closed: bool,
}

struct RecordedEvent {
    fields: Fields,
    scope: Vec<Id>,
}

#[derive(Default)]
struct Trace {
    spans: HashMap<Id, RecordedSpan>,
    events: Vec<RecordedEvent>,
}

#[derive(Clone)]
struct Capture(Arc<Mutex<Trace>>);

#[derive(Clone, Default)]
struct JsonOutput(Arc<Mutex<Vec<u8>>>);

impl<'a> MakeWriter<'a> for JsonOutput {
    type Writer = MutexGuardWriter<'a, Vec<u8>>;

    fn make_writer(&'a self) -> Self::Writer {
        self.0.make_writer()
    }
}

impl<S: Subscriber + for<'a> LookupSpan<'a>> Layer<S> for Capture {
    fn on_new_span(&self, attrs: &Attributes<'_>, id: &Id, ctx: LayerContext<'_, S>) {
        let span = ctx.span(id).unwrap();
        let mut fields = Fields::default();
        attrs.record(&mut fields);
        self.0.lock().unwrap().spans.insert(
            id.clone(),
            RecordedSpan {
                kind: attrs.metadata().name(),
                fields,
                parent: span.parent().map(|parent| parent.id()),
                enters: 0,
                exits: 0,
                closed: false,
            },
        );
    }

    fn on_event(&self, event: &Event<'_>, ctx: LayerContext<'_, S>) {
        let mut fields = Fields::default();
        event.record(&mut fields);
        let scope = ctx
            .event_scope(event)
            .map(|scope| scope.from_root().map(|span| span.id()).collect())
            .unwrap_or_default();
        self.0
            .lock()
            .unwrap()
            .events
            .push(RecordedEvent { fields, scope });
    }

    fn on_enter(&self, id: &Id, _ctx: LayerContext<'_, S>) {
        self.0.lock().unwrap().spans.get_mut(id).unwrap().enters += 1;
    }

    fn on_exit(&self, id: &Id, _ctx: LayerContext<'_, S>) {
        self.0.lock().unwrap().spans.get_mut(id).unwrap().exits += 1;
    }

    fn on_close(&self, id: Id, _ctx: LayerContext<'_, S>) {
        self.0.lock().unwrap().spans.get_mut(&id).unwrap().closed = true;
    }
}

type Signal = (&'static str, &'static str);

struct DropProbe {
    name: &'static str,
    signals: std::sync::mpsc::Sender<Signal>,
}

impl Drop for DropProbe {
    fn drop(&mut self) {
        tracing::info!(probe = "drop", owner = self.name);
        let _ = self.signals.send((self.name, "drop"));
    }
}

enum Command {
    Resume,
    Finish,
}

/// Report suspension only after the receiver has actually returned Pending.
async fn next_command(
    commands: &mut mpsc::UnboundedReceiver<Command>,
    name: &'static str,
    signals: &std::sync::mpsc::Sender<Signal>,
) -> Command {
    let mut reported = false;
    std::future::poll_fn(|cx| match commands.poll_recv(cx) {
        Poll::Ready(command) => Poll::Ready(command.unwrap()),
        Poll::Pending => {
            if !reported {
                signals.send((name, "suspended")).unwrap();
                reported = true;
            }
            Poll::Pending
        }
    })
    .await
}

fn request(
    name: &'static str,
    signals: std::sync::mpsc::Sender<Signal>,
    unrelated: Option<mpsc::UnboundedReceiver<()>>,
) -> (
    SpawnRequest,
    mpsc::UnboundedSender<Command>,
    tokio::sync::oneshot::Receiver<Result<i32>>,
) {
    let (commands_tx, mut commands) = mpsc::unbounded_channel();
    let (result_tx, result_rx) = tokio::sync::oneshot::channel();
    let request = SpawnRequest {
        name: name.into(),
        factory: Box::new(move |_| {
            tracing::info!(probe = "factory", owner = name);
            if let Some(mut unrelated) = unrelated {
                let signals = signals.clone();
                tokio::task::spawn_local(async move {
                    while unrelated.recv().await.is_some() {
                        tracing::info!(probe = "unrelated", owner = "none");
                        signals.send(("none", "unrelated")).unwrap();
                    }
                });
            }
            let probe = DropProbe { name, signals };
            Box::pin(async move {
                tracing::info!(probe = "start", owner = name);
                loop {
                    match next_command(&mut commands, name, &probe.signals).await {
                        Command::Resume => tracing::info!(probe = "resume", owner = name),
                        Command::Finish => {
                            tracing::info!(probe = "complete", owner = name);
                            break;
                        }
                    }
                }
                drop(probe);
            })
        }),
        result_tx: Some(result_tx),
    };
    (request, commands_tx, result_rx)
}

fn expect_signal(signals: &std::sync::mpsc::Receiver<Signal>, expected: Signal) {
    assert_eq!(
        signals.recv_timeout(Duration::from_secs(10)).unwrap(),
        expected
    );
}

fn exercise_cells(cancel: bool) {
    let trace = Arc::new(Mutex::new(Trace::default()));
    let capture = Capture(trace.clone());
    let json_output = JsonOutput::default();
    let json_writer = json_output.clone();
    let (requests_tx, requests_rx) = mpsc::channel(2);
    let (shutdown_tx, shutdown_rx) = watch::channel(());
    // Match ExecutorPool admission accounting without introducing another worker.
    let counts = Arc::new(vec![AtomicUsize::new(0)]);
    let metrics = ExecutorMetrics::default();
    let worker_counts = counts.clone();
    let worker = std::thread::spawn(move || {
        let subscriber = tracing_subscriber::registry().with(capture).with(
            tracing_subscriber::fmt::layer()
                .json()
                .with_ansi(false)
                .with_current_span(true)
                .with_span_list(false)
                .with_writer(json_writer),
        );
        tracing::subscriber::with_default(subscriber, || {
            worker_loop(0, requests_rx, shutdown_rx, worker_counts, None).unwrap();
            tracing::info!(probe = "after_worker", owner = "none");
        });
    });
    let (signals_tx, signals_rx) = std::sync::mpsc::channel();
    let (unrelated_tx, unrelated_rx) = mpsc::unbounded_channel();
    let (first, first_tx, first_result) = request("first", signals_tx.clone(), Some(unrelated_rx));
    let (second, second_tx, second_result) = request("second", signals_tx, None);

    counts[0].fetch_add(1, Ordering::Relaxed);
    assert!(requests_tx
        .blocking_send(AdmittedSpawn {
            request: first,
            metrics_guard: metrics.admit(),
        })
        .is_ok());
    expect_signal(&signals_rx, ("first", "suspended"));
    unrelated_tx.send(()).unwrap();
    expect_signal(&signals_rx, ("none", "unrelated"));

    // Admission while first is suspended also checks the second span's parent.
    counts[0].fetch_add(1, Ordering::Relaxed);
    assert!(requests_tx
        .blocking_send(AdmittedSpawn {
            request: second,
            metrics_guard: metrics.admit(),
        })
        .is_ok());
    expect_signal(&signals_rx, ("second", "suspended"));
    for (name, commands) in [("first", &first_tx), ("second", &second_tx)] {
        commands.send(Command::Resume).unwrap();
        expect_signal(&signals_rx, (name, "suspended"));
        unrelated_tx.send(()).unwrap();
        expect_signal(&signals_rx, ("none", "unrelated"));
    }

    if cancel {
        drop(shutdown_tx);
        let mut dropped = [
            signals_rx.recv_timeout(Duration::from_secs(10)).unwrap(),
            signals_rx.recv_timeout(Duration::from_secs(10)).unwrap(),
        ];
        dropped.sort_unstable();
        assert_eq!(dropped, [("first", "drop"), ("second", "drop")]);
        for result in [first_result, second_result] {
            assert_eq!(
                result.blocking_recv().unwrap().unwrap_err().to_string(),
                "executor shutting down"
            );
        }
    } else {
        for (name, commands, result) in [
            ("first", first_tx, first_result),
            ("second", second_tx, second_result),
        ] {
            commands.send(Command::Finish).unwrap();
            expect_signal(&signals_rx, (name, "drop"));
            assert_eq!(result.blocking_recv().unwrap().unwrap(), 0);
            unrelated_tx.send(()).unwrap();
            expect_signal(&signals_rx, ("none", "unrelated"));
        }
        drop(shutdown_tx);
    }
    worker.join().unwrap();
    assert_eq!(counts[0].load(Ordering::Relaxed), 0);

    let trace = trace.lock().unwrap();
    let executors: Vec<_> = trace
        .spans
        .iter()
        .filter(|(_, span)| span.kind == "executor")
        .collect();
    assert_eq!(executors.len(), 1);
    let (executor_id, executor) = executors[0];
    assert_eq!(executor.fields.0["worker"], "0");
    assert!(executor.parent.is_none());
    let cells: Vec<_> = trace
        .spans
        .iter()
        .filter(|(_, span)| span.kind == "cell")
        .collect();
    assert_eq!(cells.len(), 2);
    for (id, span) in &cells {
        assert_eq!(span.parent.as_ref(), Some(executor_id), "sibling parent");
        assert!(span.closed, "cell span {id:?} did not close");
        assert_eq!(span.enters, span.exits, "cell span did not exit");
        assert!(span.enters > 0, "cell span was never entered");
    }
    assert!(executor.closed);
    assert_eq!(executor.enters, executor.exits);

    let mut observed = BTreeMap::<String, Vec<String>>::new();
    for event in &trace.events {
        let Some(probe) = event.fields.0.get("probe") else {
            // The shutdown event also must not inherit a Cell span.
            assert_eq!(event.scope, vec![executor_id.clone()]);
            continue;
        };
        let owner = &event.fields.0["owner"];
        let expected = if probe == "after_worker" {
            Vec::new()
        } else if owner == "none" {
            vec![executor_id.clone()]
        } else {
            let (cell_id, _) = cells
                .iter()
                .find(|(_, span)| &span.fields.0["name"] == owner)
                .unwrap();
            vec![executor_id.clone(), (*cell_id).clone()]
        };
        assert_eq!(event.scope, expected, "{owner}: {probe} attribution");
        observed
            .entry(owner.clone())
            .or_default()
            .push(probe.clone());
    }
    for name in ["first", "second"] {
        let expected = if cancel {
            vec!["factory", "start", "resume", "drop"]
        } else {
            vec!["factory", "start", "resume", "complete", "drop"]
        };
        assert_eq!(observed[name], expected);
    }
    let mut unrelated = vec!["unrelated"; if cancel { 3 } else { 5 }];
    unrelated.push("after_worker");
    assert_eq!(observed["none"], unrelated);

    // JSON reserves span.name for metadata. Cell identity must survive parsing
    // for real worker events, including cancellation-time destructor events.
    let json = json_output.0.lock().unwrap();
    let events: Vec<serde_json::Value> = std::str::from_utf8(&json)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(events.len(), trace.events.len());
    for event in &events {
        match event["fields"]["owner"].as_str() {
            Some("first" | "second") => {
                assert_eq!(event["span"]["name"], "cell");
                assert_eq!(
                    event["span"]["cell_name"], event["fields"]["owner"],
                    "JSON lost Cell identity: {event}"
                );
            }
            _ => assert!(event["span"].get("cell_name").is_none()),
        }
    }
}

#[test]
fn worker_cell_spans_follow_polling_and_completion() {
    // Value: protects=Cell event attribution and executor sibling parentage;
    // fails_when=entered guards cross suspension; why_new=executor tests do not
    // inspect tracing context; seam=none.
    exercise_cells(false);
}

#[test]
fn worker_cell_spans_follow_shutdown_destruction() {
    // Value: protects=cancelled Cell destructor attribution and span exits;
    // fails_when=shutdown drops a Cell under another Cell span; why_new=shutdown
    // tests do not inspect destructor tracing context; seam=none.
    exercise_cells(true);
}
