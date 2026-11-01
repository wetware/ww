//! Privacy and error topology tests through the public, final subscriber.
#![cfg(not(target_arch = "wasm32"))]

use std::error::Error;
use std::fmt;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::Value;
use tracing::Instrument;

const SECRET: &str = "SECRET-capability-token-/private/home/guest-payload";
const PROBE: &str = "WW_LOG_CONTRACT_PROBE";

fn capture(format: &str, filter: &str) -> String {
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "logging_contract_probe",
            "--ignored",
            "--nocapture",
        ])
        .env(PROBE, "1")
        .env("WW_LOG_FORMAT", format)
        .env("RUST_LOG", filter)
        .env("NO_COLOR", "1")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn contract probe");
    // Drain stderr while polling: bounded output tests must not deadlock on a
    // full pipe when the subscriber has a regression.
    let mut stderr = child.stderr.take().unwrap();
    let reader = std::thread::spawn(move || {
        use std::io::Read;
        let mut bytes = Vec::new();
        stderr.read_to_end(&mut bytes).unwrap();
        bytes
    });
    let deadline = Instant::now() + Duration::from_secs(15);
    while child.try_wait().unwrap().is_none() {
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("contract probe deadline");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let output = child.wait_with_output().unwrap();
    let logs = String::from_utf8(reader.join().unwrap()).unwrap();
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(output.status.success(), "probe failed: {stdout}\n{logs}");
    assert!(stdout.contains("contract-command-output"));
    assert!(!stdout.contains("Cell process failed"));
    assert!(!logs.contains(SECRET), "sensitive value escaped: {logs}");
    assert!(
        !logs.contains("9876543210987654321"),
        "opaque numeric identifier escaped"
    );
    assert!(!logs.contains('\x1b'), "unexpected terminal controls");
    assert!(logs.lines().all(|line| line.len() < 16_384));
    logs
}

fn json_events(filter: &str) -> Vec<Value> {
    capture("json", filter)
        .lines()
        .map(|line| serde_json::from_str(line).expect("one valid JSON object per event"))
        .collect()
}

fn event(events: &[Value], count: u64) -> &Value {
    events
        .iter()
        .find(|event| event["fields"]["count"] == count)
        .unwrap_or_else(|| panic!("missing event {count}: {events:#?}"))
}

#[test]
fn final_json_subscriber_uses_catalog_and_redacts_values() {
    let events = json_events("ww::logging_contract_probe=info");
    let fields = &event(&events, 1)["fields"];
    assert_eq!(fields["event_code"], 201);
    assert_eq!(fields["message"], "Cell process failed");
    assert_eq!(fields["category"], "cell");
    assert_eq!(fields["operation"], "run");
    assert_eq!(fields["outcome"], "failed");
    assert_eq!(fields["reason"], "process_failed");
    assert_eq!(fields["active"], true);
    assert_eq!(fields["elapsed_ms"], 7);
    assert_eq!(event(&events, 2)["span"]["cell_id"], 42);
    assert_eq!(event(&events, 2)["span"]["cell_name"], "[redacted]");
    assert_eq!(
        event(&events, 3)["fields"]["message"],
        "unclassified event (text redacted)"
    );
    // Unknown fields and values cannot create unbounded records.
    assert!(event(&events, 4)["fields"].as_object().unwrap().len() <= 40);
    assert_eq!(event(&events, 4)["fields"]["fields_truncated"], true);
    assert!(events
        .iter()
        .any(|event| event["fields"]["record_truncated"] == true));
    assert!(events.iter().all(|event| event.get("filename").is_none()));
}

#[test]
fn final_json_subscriber_preserves_real_source_relationships_without_error_text() {
    let events = json_events("ww::logging_contract_probe=info");
    let error = &event(&events, 5)["fields"]["error"];
    assert_eq!(error["truncated"], false);
    assert_eq!(error["messages_redacted"], true);
    let chain = error["chain"].as_array().expect("structured source chain");
    assert_eq!(chain.len(), 2, "anyhow context followed by real io::Error");
    assert_eq!(chain[0]["depth"], 0);
    assert_eq!(chain[0]["source"], 1);
    assert_eq!(chain[0]["category"], "error");
    assert_eq!(chain[1]["depth"], 1);
    assert_eq!(chain[1]["source"], Value::Null);
    assert_eq!(chain[1]["category"], "io");
    assert_eq!(chain[1]["kind"], "permission_denied");
    assert!(chain.iter().all(|cause| cause["message"] == "[redacted]"));
    let cyclic = &event(&events, 6)["fields"]["error"];
    assert_eq!(cyclic["truncated"], true);
    let chain = cyclic["chain"].as_array().unwrap();
    assert_eq!(chain.len(), 8);
    assert_eq!(chain.last().unwrap()["source"], Value::Null);
    for (depth, cause) in chain.iter().enumerate().take(7) {
        assert_eq!(cause["depth"], depth);
        assert_eq!(cause["source"], depth + 1);
    }
    // This Error's Debug and Display panic. Successful capture establishes that
    // only source() and type classification were used.
    let bomb = &event(&events, 7)["fields"]["error"];
    assert_eq!(bomb["chain"].as_array().unwrap().len(), 1);
    assert_eq!(bomb["truncated"], false);
}

#[test]
fn final_compact_subscriber_redacts_and_formats_bounded_error_chains() {
    let logs = capture("compact", "ww::logging_contract_probe=info");
    assert!(logs.contains("Cell process failed"));
    assert!(logs.contains("[redacted]"));
    assert!(logs.contains("truncated"));
    assert!(logs.contains("source"));
    assert!(logs.contains("unclassified event (text redacted)"));
}

#[test]
fn final_subscriber_bounds_wide_integers_and_rejects_control_characters_in_field_names() {
    let events = json_events("ww::logging_contract_probe=info");
    let fields = &event(&events, 9)["fields"];
    assert_eq!(fields["elapsed_ms"], u64::MAX);
    assert_eq!(fields["exit_code"], i64::MIN);
    assert_eq!(fields["fields_truncated"], true);
    assert!(fields.get("forged\nfield").is_none());
    let compact = capture("compact", "ww::logging_contract_probe=info");
    assert!(!compact.contains("forged"));
}

#[test]
fn final_subscriber_filters_before_value_evaluation_and_omits_filtered_spans() {
    for format in ["json", "compact"] {
        let logs = capture(format, "ww::logging_contract_probe=warn");
        if format == "json" {
            let events: Vec<Value> = logs
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect();
            assert!(event(&events, 2).get("span").is_none());
        }
    }
}

#[test]
// Value: protects=explicit and ambient event correlation under the final subscriber;
// fails_when=an ambient span overrides an explicit parent or parent None;
// why_new=the existing interleaving fixture only tested inherited parents; seam=none.
fn final_subscriber_keeps_interleaved_task_correlation_after_redaction() {
    let events = json_events("ww::logging_contract_probe=info");
    for (count, cell_id) in [(101, 1), (102, 2), (201, 1), (202, 2)] {
        assert_eq!(event(&events, count)["span"]["cell_id"], cell_id);
    }
    assert!(event(&events, 300).get("span").is_none());
    assert_eq!(event(&events, 10)["span"]["cell_id"], 43);
    assert!(event(&events, 11).get("span").is_none());
    let compact = capture("compact", "ww::logging_contract_probe=info");
    for (count, has_parent) in [(10, true), (11, false)] {
        let token = format!("count={count}");
        let line = compact
            .lines()
            .find(|line| line.split_whitespace().any(|field| field == token))
            .unwrap();
        assert_eq!(line.contains("cell{cell_id=43"), has_parent, "{line}");
        assert!(!line.contains("cell_id=42"), "ambient Cell leaked: {line}");
        if !has_parent {
            assert!(
                !line.contains("cell{"),
                "explicit root inherited context: {line}"
            );
        }
    }
}

struct FormattingBomb;

impl fmt::Display for FormattingBomb {
    fn fmt(&self, _: &mut fmt::Formatter<'_>) -> fmt::Result {
        panic!("subscriber evaluated sensitive Display")
    }
}

impl fmt::Debug for FormattingBomb {
    fn fmt(&self, _: &mut fmt::Formatter<'_>) -> fmt::Result {
        panic!("subscriber evaluated sensitive Debug")
    }
}

impl Error for FormattingBomb {}

#[derive(Debug)]
struct Cycle;

impl fmt::Display for Cycle {
    fn fmt(&self, _: &mut fmt::Formatter<'_>) -> fmt::Result {
        panic!("subscriber evaluated cyclic error Display")
    }
}

impl Error for Cycle {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(self)
    }
}

fn filtered_value() -> u64 {
    panic!("filtered TRACE event evaluated its value")
}

#[test]
#[ignore = "isolated subprocess entry point"]
fn logging_contract_probe() {
    if std::env::var(PROBE).as_deref() != Ok("1") {
        return;
    }
    ww::config::init_tracing();
    tracing::warn!(target: "ww::logging_contract_probe",
        event_code = 201u64, count = 1u64, active = true, elapsed_ms = 7u128,
        category = SECRET, operation = SECRET, reason = SECRET, outcome = SECRET,
        credentials = SECRET, payload = SECRET, path = SECRET, token = SECRET,
        capability = 9_876_543_210_987_654_321u64,
        debug_value = ?FormattingBomb, display_value = %FormattingBomb,
        "{SECRET}");
    let span = tracing::info_span!(target: "ww::logging_contract_probe", "cell",
        cell_id = 42u64, cell_name = SECRET, payload = tracing::field::Empty);
    span.record("payload", SECRET);
    span.record("cell_name", SECRET.repeat(100_000));
    span.in_scope(|| {
        tracing::warn!(target: "ww::logging_contract_probe", event_code = 201u64, count = 2u64,
            "{SECRET}");
    });
    let other = tracing::info_span!(target: "ww::logging_contract_probe", parent: None,
        "cell", cell_id = 43u64);
    span.in_scope(|| {
        tracing::warn!(target: "ww::logging_contract_probe", parent: &other,
            event_code = 201u64, count = 10u64, "{SECRET}");
        tracing::warn!(target: "ww::logging_contract_probe", parent: None,
            event_code = 201u64, count = 11u64, "{SECRET}");
    });
    tracing::warn!(target: "ww::logging_contract_probe", count = 3u64,
        "{}", FormattingBomb);
    tracing::warn!(target: "ww::logging_contract_probe", count = 4u64,
        f01 = SECRET, f02 = SECRET, f03 = SECRET, f04 = SECRET, f05 = SECRET,
        f06 = SECRET, f07 = SECRET, f08 = SECRET, f09 = SECRET, f10 = SECRET,
        f11 = SECRET, f12 = SECRET, f13 = SECRET, f14 = SECRET, f15 = SECRET,
        f16 = SECRET, f17 = SECRET, f18 = SECRET, f19 = SECRET, f20 = SECRET,
        f21 = SECRET, f22 = SECRET, f23 = SECRET, f24 = SECRET, f25 = SECRET,
        f26 = SECRET, f27 = SECRET, f28 = SECRET, f29 = SECRET, f30 = SECRET,
        f31 = SECRET, f32 = SECRET, f33 = SECRET, f34 = SECRET, f35 = SECRET,
        f36 = SECRET, f37 = SECRET, f38 = SECRET, f39 = SECRET, f40 = SECRET,
        large = SECRET.repeat(100_000).as_str(), "{SECRET}");
    let chain = anyhow::Error::new(std::io::Error::new(
        std::io::ErrorKind::PermissionDenied,
        SECRET,
    ))
    .context(SECRET);
    let error: &(dyn Error + 'static) = chain.as_ref();
    tracing::warn!(target: "ww::logging_contract_probe", event_code = 201u64,
        count = 5u64, error = error, "{SECRET}");
    let error: &(dyn Error + 'static) = &Cycle;
    tracing::warn!(target: "ww::logging_contract_probe", event_code = 201u64,
        count = 6u64, error = error, "{SECRET}");
    let error: &(dyn Error + 'static) = &FormattingBomb;
    tracing::warn!(target: "ww::logging_contract_probe", event_code = 201u64,
        count = 7u64, error = error, "{SECRET}");
    // Many individually bounded source chains exceed the whole-record cap.
    // The final subscriber must replace the record, never write partial JSON.
    let error: &(dyn Error + 'static) = &Cycle;
    tracing::warn!(target: "ww::logging_contract_probe", count = 8u64,
        e01 = error, e02 = error, e03 = error, e04 = error, e05 = error,
        e06 = error, e07 = error, e08 = error, e09 = error, e10 = error,
        e11 = error, e12 = error, e13 = error, e14 = error, e15 = error,
        e16 = error, e17 = error, e18 = error, e19 = error, e20 = error,
        e21 = error, e22 = error, e23 = error, e24 = error, e25 = error,
        e26 = error, e27 = error, e28 = error, e29 = error, e30 = error,
        e31 = error, "{SECRET}");
    tracing::warn!(target: "ww::logging_contract_probe", count = 9u64,
        elapsed_ms = u128::MAX, exit_code = i128::MIN, "forged\nfield" = SECRET);
    tracing::trace!(target: "ww::logging_contract_probe", count = filtered_value(), "{SECRET}");

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(tokio::task::LocalSet::new().run_until(async {
        let barrier = Arc::new(tokio::sync::Barrier::new(3));
        let mut tasks = Vec::new();
        for cell_id in [1u64, 2u64] {
            let barrier = Arc::clone(&barrier);
            let span = tracing::info_span!(target: "ww::logging_contract_probe", "cell", cell_id,
                cell_name = SECRET);
            tasks.push(tokio::task::spawn_local(async move {
                tracing::warn!(target: "ww::logging_contract_probe", count = 100 + cell_id, "{SECRET}");
                barrier.wait().await;
                tokio::task::yield_now().await;
                tracing::warn!(target: "ww::logging_contract_probe", count = 200 + cell_id, "{SECRET}");
            }.instrument(span)));
        }
        barrier.wait().await;
        tracing::warn!(target: "ww::logging_contract_probe", count = 300u64, "{SECRET}");
        for task in tasks {
            task.await.unwrap();
        }
    }));
    println!("contract-command-output");
}
