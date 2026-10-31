use super::*;
use std::io::Write;
use std::sync::Mutex;
use tracing::Instrument;
use tracing_subscriber::{layer::SubscriberExt, Layer};

#[derive(Clone, Default)]
struct Output(Arc<Mutex<Vec<u8>>>);

#[derive(Clone, Default)]
struct CausalLinks(Arc<Mutex<Vec<(tracing::span::Id, tracing::span::Id)>>>);

impl<S> Layer<S> for CausalLinks
where
    S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
{
    fn on_follows_from(
        &self,
        span: &tracing::span::Id,
        follows: &tracing::span::Id,
        context: tracing_subscriber::layer::Context<'_, S>,
    ) {
        if context.span(span).unwrap().name() == "cell" {
            assert_eq!(context.span(follows).unwrap().name(), "caller");
            self.0.lock().unwrap().push((span.clone(), follows.clone()));
        }
    }
}

impl Write for Output {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[tokio::test(flavor = "current_thread")]
async fn spawned_children_keep_distinct_identity_through_execution_and_teardown() {
    exercise_children(true).await;
}

#[tokio::test(flavor = "current_thread")]
async fn filtered_child_spans_do_not_retain_the_spawn_caller() {
    exercise_children(false).await;
}

// Value: protects=bounded filesystem diagnostics before child exit with correct
// attribution; fails_when=the event/field is absent, late, or retains the caller;
// why_new=existing assertions covered fuel and exit only; seam=none.
async fn exercise_children(child_span_enabled: bool) {
    let output = Output::default();
    let writer = output.clone();
    let layer = tracing_subscriber::fmt::layer()
        .json()
        .with_ansi(false)
        .with_span_list(true)
        .with_writer(move || writer.clone())
        .with_filter(tracing_subscriber::filter::filter_fn(move |metadata| {
            child_span_enabled || !(metadata.is_span() && metadata.name() == "cell")
        }));
    let links = CausalLinks::default();
    // Both layers must reject disabled child spans, or the capture layer would
    // enable spans that the JSON layer alone filters out.
    let capture = links
        .clone()
        .with_filter(tracing_subscriber::filter::filter_fn(move |metadata| {
            child_span_enabled || !(metadata.is_span() && metadata.name() == "cell")
        }));
    let subscriber = tracing_subscriber::registry().with(layer).with(capture);
    let _subscriber = tracing::subscriber::set_default(subscriber);
    let mut callers = Vec::new();
    tokio::task::LocalSet::new()
        .run_until(async {
            let (runtime_engine, _publisher) = cell::engine::runtime_engine().unwrap();
            let bytecode = Arc::new(
                wat::parse_str(include_str!("../tests/fixtures/spinning-component.wat")).unwrap(),
            );
            let component = Arc::new(
                wasmtime::component::Component::new(&runtime_engine.engine(), &*bytecode).unwrap(),
            );
            let (_epoch, receiver) = tokio::sync::watch::channel(authority::Epoch {
                seq: 1,
                root: None,
                head: vec![],
            });
            let membrane = authority::membrane_client(receiver.clone(), b"child-tracing");
            let fuel = FuelObserver::default();
            let executor: system_capnp::executor::Client = capnp_rpc::new_client(ExecutorImpl {
                bytecode,
                component: Some(component),
                runtime_engine,
                wasm_debug: false,
                guard: EpochGuard {
                    issued_seq: 1,
                    receiver,
                },
                pinset_cache: None,
                filesystem_resources: crate::services::test_filesystem_resources(),
                fuel_observer: Some(fuel.clone()),
            });
            let mut children = Vec::new();
            for caller in ["first-caller", "second-caller"] {
                let mut request = executor.spawn_request();
                request.get().set_membrane(membrane.clone());
                let mut policy = request.get().init_fuel_policy().init_oneshot();
                policy.set_total_budget(u64::MAX);
                policy.set_max_per_epoch(10_000);
                policy.set_min_per_epoch(0);
                let caller_span = tracing::info_span!("caller", caller);
                callers.push(caller_span.id().unwrap());
                let response = request
                    .send()
                    .promise
                    .instrument(caller_span)
                    .await
                    .unwrap();
                children.push(response.get().unwrap().get_process().unwrap());
                // The real guest exhausts its epoch grant and suspends. Both
                // children remain pending before either is terminated.
                fuel.wait_for_suspension(children.len()).await;
            }
            tokio::task::spawn_local(async {
                tracing::info!("unrelated-child-probe");
            })
            .await
            .unwrap();
            for child in children {
                child.kill_request().send().promise.await.unwrap();
                let result = child.wait_request().send().promise.await.unwrap();
                assert_eq!(result.get().unwrap().get_exit_code(), 137);
            }
        })
        .await;
    // Value: protects=causal ownership; fails_when=follows_from is missing or
    // targets another caller; why_new=JSON omits causal edges; seam=none.
    let links = links.0.lock().unwrap();
    if child_span_enabled {
        assert_eq!(links.len(), callers.len());
        for ((_, follows), caller) in links.iter().zip(callers) {
            assert_eq!(*follows, caller);
        }
        assert_ne!(links[0].0, links[1].0);
    } else {
        assert!(links.is_empty());
    }
    let events: Vec<serde_json::Value> = String::from_utf8(output.0.lock().unwrap().clone())
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let mut started = Vec::new();
    let mut stopped = Vec::new();
    let mut filesystem_snapshots = Vec::new();
    let mut filesystem_completions = Vec::new();
    for event in events {
        let message = event["fields"]["message"].as_str().unwrap_or_default();
        if message == "unrelated-child-probe" {
            assert!(event.get("span").is_none(), "{event}");
        }
        let filesystem_snapshot =
            message == "executor: child filesystem ownership after guest exit";
        let filesystem_completion = message == "executor: child filesystem completion observed";
        if message == "fuel.initial"
            || filesystem_snapshot
            || filesystem_completion
            || message.starts_with("executor: child process exited")
        {
            let id = if child_span_enabled {
                assert_eq!(event["span"]["name"], "cell", "{event}");
                assert_eq!(event["spans"].as_array().unwrap().len(), 1, "{event}");
                event["span"]["cell_id"]
                    .as_u64()
                    .expect("numeric child identity")
            } else {
                assert!(
                    event.get("span").is_none(),
                    "filtered child retained caller: {event}"
                );
                0
            };
            if message == "fuel.initial" {
                started.push(id);
            } else if filesystem_snapshot {
                let value = event["fields"]["filesystem"]
                    .as_str()
                    .expect("filesystem snapshot or explicit read error");
                assert!(value.len() < 4096, "unbounded snapshot event: {event}");
                assert!(
                    value.starts_with("Ok(FsSnapshot {")
                        || matches!(value, "Err(Busy)" | "Err(Unavailable)" | "Err(Poisoned)"),
                    "unexpected snapshot diagnostic: {event}"
                );
                if value.starts_with("Ok(FsSnapshot {") {
                    assert!(value.contains("directory_builds:"), "{event}");
                }
                filesystem_snapshots.push(id);
            } else if filesystem_completion {
                assert_eq!(event["fields"]["succeeded"], true, "{event}");
                assert_eq!(filesystem_snapshots.len(), filesystem_completions.len() + 1);
                assert_eq!(filesystem_snapshots.last(), Some(&id));
                filesystem_completions.push(id);
            } else {
                assert_eq!(filesystem_snapshots.len(), stopped.len() + 1);
                assert_eq!(filesystem_snapshots.last(), Some(&id));
                assert_eq!(filesystem_completions.len(), stopped.len() + 1);
                assert_eq!(filesystem_completions.last(), Some(&id));
                stopped.push(id);
            }
        }
    }
    assert_eq!(started.len(), 2);
    assert_eq!(stopped, started);
    assert_eq!(filesystem_snapshots, started);
    assert_eq!(filesystem_completions, started);
    if child_span_enabled {
        assert_ne!(started[0], started[1]);
    }
}
