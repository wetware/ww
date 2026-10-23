//! Exercise the public initializer in fresh processes: no test mutates the
//! parent process's environment or installs its global subscriber.
#![cfg(not(target_arch = "wasm32"))]

use std::io::{self, Read};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;
use tracing::Instrument;

const STDOUT_MARKER: &str = "logging-probe-command-output";
const INVALID_FORMAT_MESSAGE: &str =
    "invalid WW_LOG_FORMAT; expected auto, compact, or json; using auto";

struct Capture {
    stdout: String,
    stderr: String,
}

impl Capture {
    fn assert_stdout_reserved(&self) {
        assert!(self.stdout.contains(STDOUT_MARKER), "{}", self.stdout);
        for diagnostic in [
            "native-event",
            "filter-event",
            "cell-event",
            "invalid WW_LOG_FORMAT",
        ] {
            assert!(
                !self.stdout.contains(diagnostic),
                "diagnostic {diagnostic:?} leaked to stdout: {}",
                self.stdout
            );
        }
    }

    fn json(&self) -> Vec<Value> {
        assert!(
            !self.stderr.contains('\x1b'),
            "JSON contains ANSI: {}",
            self.stderr
        );
        let events: Vec<Value> = self
            .stderr
            .lines()
            .map(|line| {
                serde_json::from_str(line)
                    .unwrap_or_else(|err| panic!("invalid JSON log line {line:?}: {err}"))
            })
            .collect();
        assert!(
            !events.is_empty(),
            "no events on stderr; stdout: {}",
            self.stdout
        );
        events
    }
}

fn capture(terminal: bool, env: &[(&str, &str)]) -> Capture {
    let env: Vec<_> = env
        .iter()
        .map(|(key, value)| (*key, std::ffi::OsStr::new(value)))
        .collect();
    capture_os(terminal, &env)
}

fn capture_os(terminal: bool, env: &[(&str, &std::ffi::OsStr)]) -> Capture {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args(["--exact", "logging_probe", "--ignored", "--nocapture"])
        .env_remove("RUST_LOG")
        .env_remove("WW_LOG_FORMAT")
        .env_remove("NO_COLOR")
        .env_remove("WW_LOG_TEST_PROBE")
        .envs(env.iter().copied())
        .stdout(Stdio::piped());

    #[cfg(unix)]
    let terminal_reader = if terminal {
        let (master, slave) = open_pty();
        command.stderr(Stdio::from(slave));
        Some(master)
    } else {
        command.stderr(Stdio::piped());
        None
    };
    #[cfg(not(unix))]
    {
        assert!(!terminal, "terminal tests require Unix openpty");
        command.stderr(Stdio::piped());
    }

    let mut child = command.spawn().expect("spawn logging probe");
    // Command retains the PTY slave. Close it so the reader sees EOF when the
    // child exits, including when the test kills a timed-out child.
    drop(command);
    #[cfg(unix)]
    let reader: Box<dyn Read + Send> = match terminal_reader {
        Some(master) => Box::new(master),
        None => Box::new(child.stderr.take().unwrap()),
    };
    #[cfg(not(unix))]
    let reader: Box<dyn Read + Send> = Box::new(child.stderr.take().unwrap());
    let stderr_reader = std::thread::spawn(move || read_stderr(reader));

    let deadline = Instant::now() + Duration::from_secs(15);
    while child.try_wait().unwrap().is_none() {
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("logging probe exceeded 15 seconds");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let output = child.wait_with_output().unwrap();
    let capture = Capture {
        stdout: String::from_utf8(output.stdout).unwrap(),
        stderr: String::from_utf8(stderr_reader.join().unwrap()).unwrap(),
    };
    assert!(
        output.status.success(),
        "probe failed: {}\n{}",
        capture.stdout,
        capture.stderr
    );
    if !env
        .iter()
        .any(|(key, value)| *key == "WW_LOG_TEST_PROBE" && *value == "legacy_stdout")
    {
        capture.assert_stdout_reserved();
    }
    capture
}

fn read_stderr(mut reader: Box<dyn Read + Send>) -> Vec<u8> {
    let mut output = Vec::new();
    let mut buffer = [0; 4096];
    loop {
        match reader.read(&mut buffer) {
            Ok(0) => return output,
            Ok(size) => output.extend_from_slice(&buffer[..size]),
            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
            // Linux PTY masters signal slave closure with EIO instead of EOF.
            #[cfg(unix)]
            Err(err) if err.raw_os_error() == Some(libc::EIO) => return output,
            Err(err) => panic!("read probe stderr: {err}"),
        }
    }
}

#[cfg(unix)]
fn open_pty() -> (std::fs::File, std::fs::File) {
    use std::os::fd::FromRawFd;

    let mut master = -1;
    let mut slave = -1;
    // SAFETY: both output pointers are valid. Null optional arguments request
    // the platform's default terminal settings. Successful descriptors are
    // transferred exactly once into owning Files below.
    let result = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    assert_eq!(result, 0, "openpty: {}", io::Error::last_os_error());
    for descriptor in [master, slave] {
        // SAFETY: openpty returned valid descriptors. Prevent other concurrently
        // spawned probes from retaining this terminal after its child exits.
        let result = unsafe { libc::fcntl(descriptor, libc::F_SETFD, libc::FD_CLOEXEC) };
        assert_eq!(
            result,
            0,
            "set PTY close-on-exec: {}",
            io::Error::last_os_error()
        );
    }
    // SAFETY: openpty succeeded and returned two distinct, owned descriptors.
    unsafe {
        (
            std::fs::File::from_raw_fd(master),
            std::fs::File::from_raw_fd(slave),
        )
    }
}

#[test]
fn redirected_stderr_defaults_to_json() {
    let capture = capture(false, &[]);
    let events = capture.json();
    assert!(events
        .iter()
        .any(|event| event["fields"]["message"] == "native-event"));
}

#[test]
fn explicit_auto_uses_redirected_stderr() {
    capture(false, &[("WW_LOG_FORMAT", "auto")]).json();
}

#[cfg(unix)]
#[test]
fn terminal_stderr_uses_compact_ansi_even_when_stdout_is_redirected() {
    let capture = capture(true, &[]);
    assert!(
        capture.stderr.contains("native-event"),
        "{}",
        capture.stderr
    );
    assert!(capture.stderr.contains('\x1b'), "{}", capture.stderr);
    assert!(serde_json::from_str::<Value>(capture.stderr.lines().next().unwrap()).is_err());
}

#[cfg(unix)]
#[test]
fn json_override_on_terminal_has_no_ansi() {
    capture(true, &[("WW_LOG_FORMAT", "json")]).json();
}

#[test]
fn compact_override_on_pipe_has_no_ansi() {
    let capture = capture(false, &[("WW_LOG_FORMAT", "compact")]);
    assert!(
        capture.stderr.contains("native-event"),
        "{}",
        capture.stderr
    );
    assert!(!capture.stderr.contains('\x1b'), "{}", capture.stderr);
    assert!(serde_json::from_str::<Value>(capture.stderr.lines().next().unwrap()).is_err());
}

#[cfg(unix)]
#[test]
fn nonempty_no_color_disables_terminal_ansi() {
    let capture = capture(true, &[("NO_COLOR", "1")]);
    assert!(
        capture.stderr.contains("native-event"),
        "{}",
        capture.stderr
    );
    assert!(!capture.stderr.contains('\x1b'), "{}", capture.stderr);
}

#[cfg(unix)]
#[test]
fn empty_no_color_keeps_terminal_ansi() {
    let capture = capture(true, &[("NO_COLOR", "")]);
    assert!(capture.stderr.contains('\x1b'), "{}", capture.stderr);
}

#[test]
fn json_preserves_typed_fields_and_multiline_errors_in_one_event() {
    let capture = capture(false, &[]);
    let events = capture.json();
    let native: Vec<_> = events
        .iter()
        .filter(|event| event["fields"]["message"] == "native-event")
        .collect();
    assert_eq!(native.len(), 1, "initialization must not duplicate events");
    let event = native[0];
    assert_eq!(event["level"], "WARN");
    assert_eq!(event["target"], "ww::logging_probe");
    assert!(event["timestamp"]
        .as_str()
        .is_some_and(|timestamp| !timestamp.is_empty()));
    assert_eq!(event["fields"]["count"], 42);
    assert_eq!(event["fields"]["active"], true);
    assert_eq!(event["fields"]["text"], "line one\nline two");
    assert_eq!(event["fields"]["error"], "probe failed\nuseful detail");
}

#[test]
fn json_keeps_only_current_span_with_correct_async_attribution() {
    let capture = capture(false, &[]);
    let events = capture.json();
    let cells: Vec<_> = events
        .iter()
        .filter(|event| event["fields"]["message"] == "cell-event")
        .collect();
    assert_eq!(cells.len(), 2);
    for event in cells {
        assert_eq!(event["span"]["name"], "cell");
        assert_eq!(
            event["span"]["cell_id"],
            event["fields"]["expected_cell_id"]
        );
        assert!(
            event.get("spans").is_none(),
            "full span tree duplicated: {event}"
        );
    }
}

#[test]
fn redirected_defaults_cover_internal_crates_at_info() {
    let capture = capture(false, &[]);
    let events = capture.json();
    for target in [
        "ww", "atom", "cache", "cell", "ipfs", "membrane", "rpc", "stem",
    ] {
        assert!(
            events
                .iter()
                .any(|event| event["target"] == target && event["level"] == "INFO"),
            "missing info for {target}"
        );
    }
    assert!(!events.iter().any(|event| event["level"] == "DEBUG"));
    assert!(!events
        .iter()
        .any(|event| event["target"] == "external_dependency"));
}

#[cfg(unix)]
#[test]
fn terminal_defaults_cover_internal_crates_at_warn_even_with_json_override() {
    let capture = capture(true, &[("WW_LOG_FORMAT", "json")]);
    let events = capture.json();
    for target in [
        "ww", "atom", "cache", "cell", "ipfs", "membrane", "rpc", "stem",
    ] {
        assert!(
            events
                .iter()
                .any(|event| event["target"] == target && event["level"] == "WARN"),
            "missing warn for {target}"
        );
    }
    assert!(!events
        .iter()
        .any(|event| event["level"] == "INFO" || event["level"] == "DEBUG"));
}

#[test]
fn rust_log_overrides_default_internal_filters() {
    let capture = capture(false, &[("RUST_LOG", "rpc=debug")]);
    let events = capture.json();
    assert!(events.iter().any(|event| event["level"] == "DEBUG"));
    assert!(events.iter().all(|event| event["target"] == "rpc"));
}

#[test]
fn rust_log_off_suppresses_all_diagnostics() {
    let capture = capture(false, &[("RUST_LOG", "off")]);
    assert!(capture.stderr.is_empty(), "{}", capture.stderr);
}

#[test]
fn filtered_spans_do_not_add_context_to_enabled_events() {
    let capture = capture(false, &[("RUST_LOG", "ww=warn")]);
    let events = capture.json();
    let cells: Vec<_> = events
        .iter()
        .filter(|event| event["fields"]["message"] == "cell-event-warning")
        .collect();
    assert_eq!(cells.len(), 2);
    for event in cells {
        assert!(event.get("span").is_none(), "filtered span leaked: {event}");
        assert!(
            event.get("spans").is_none(),
            "filtered span tree leaked: {event}"
        );
    }
}

#[test]
fn empty_format_falls_back_to_auto_with_warning() {
    let capture = capture(false, &[("WW_LOG_FORMAT", "")]);
    let events = capture.json();
    assert_eq!(
        events
            .iter()
            .filter(|event| event["fields"]["message"] == INVALID_FORMAT_MESSAGE
                && event["level"] == "WARN")
            .count(),
        1
    );
}

#[test]
fn invalid_rust_log_falls_back_to_internal_defaults() {
    let capture = capture(false, &[("RUST_LOG", "ww=not-a-level")]);
    let events = capture.json();
    assert!(events
        .iter()
        .any(|event| event["target"] == "cell" && event["level"] == "INFO"));
}

#[test]
fn invalid_format_falls_back_to_auto_without_echoing_value() {
    let capture = capture(false, &[("WW_LOG_FORMAT", "secret-value\nforged-log")]);
    let events = capture.json();
    assert_eq!(
        events
            .iter()
            .filter(|event| event["fields"]["message"] == INVALID_FORMAT_MESSAGE
                && event["level"] == "WARN")
            .count(),
        1
    );
    assert!(!capture.stderr.contains("secret-value"));
    assert!(!capture.stderr.contains("forged-log"));
    assert!(events
        .iter()
        .any(|event| event["fields"]["message"] == "native-event"));
}

#[test]
fn legacy_explicit_stderr_initializer_uses_same_output_policy() {
    capture(false, &[("WW_LOG_TEST_PROBE", "legacy_stderr")]).json();
}

#[test]
fn legacy_explicit_stdout_initializer_preserves_stdout_destination() {
    let capture = capture(false, &[("WW_LOG_TEST_PROBE", "legacy_stdout")]);
    assert!(capture.stderr.is_empty(), "{}", capture.stderr);
    assert!(capture.stdout.contains(STDOUT_MARKER), "{}", capture.stdout);
    let events: Vec<Value> = capture
        .stdout
        .lines()
        .filter(|line| line.starts_with('{'))
        .map(|line| serde_json::from_str(line).expect("stdout log is valid JSON"))
        .collect();
    assert_eq!(
        events
            .iter()
            .filter(|event| event["fields"]["message"] == "native-event")
            .count(),
        1
    );
}

#[test]
fn existing_subscriber_ignores_invalid_format_without_warning() {
    let capture = capture(
        false,
        &[
            ("WW_LOG_TEST_PROBE", "existing"),
            ("WW_LOG_FORMAT", "invalid-value"),
        ],
    );
    assert_eq!(capture.stderr.matches("native-event").count(), 1);
    assert!(
        !capture.stderr.contains("WW_LOG_FORMAT"),
        "{}",
        capture.stderr
    );
}

#[cfg(unix)]
#[test]
fn nonunicode_format_falls_back_to_auto_with_warning() {
    use std::os::unix::ffi::OsStringExt;
    let invalid = std::ffi::OsString::from_vec(vec![0xff]);
    let capture = capture_os(false, &[("WW_LOG_FORMAT", invalid.as_os_str())]);
    let events = capture.json();
    assert_eq!(
        events
            .iter()
            .filter(|event| event["fields"]["message"] == INVALID_FORMAT_MESSAGE
                && event["level"] == "WARN")
            .count(),
        1
    );
}

#[test]
fn cli_startup_diagnostics_use_json_stderr_and_leave_stdout_empty() {
    let root = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let missing_identity = root.path().join("missing-identity");
    let mut child = Command::new(env!("CARGO_BIN_EXE_ww"))
        .arg("run")
        .arg(root.path())
        .arg("--identity")
        .arg(&missing_identity)
        .args(["--with-http-admin", "off"])
        // No inherited CLI defaults or real installed FHS enter this probe.
        .env_clear()
        .env("HOME", home.path())
        .env("WW_FHS_ROOT", root.path())
        .env("RUST_LOG", "ww=debug")
        .env("WW_LOG_FORMAT", "json")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(15);
    while child.try_wait().unwrap().is_none() {
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("CLI identity probe exceeded 15 seconds");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let output = child.wait_with_output().unwrap();
    let stdout = String::from_utf8(output.stdout).unwrap();
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        !output.status.success(),
        "CLI unexpectedly accepted missing identity"
    );
    assert!(
        stdout.is_empty(),
        "startup diagnostics leaked to stdout: {stdout}"
    );
    assert!(stderr.contains("Identity file not found:"), "{stderr}");
    assert!(
        stderr.contains(missing_identity.to_str().unwrap()),
        "{stderr}"
    );
    // The final command error uses Rust's process error rendering. Trace events
    // before that error must remain individually parseable JSON lines.
    let events: Vec<Value> = stderr
        .lines()
        .filter(|line| line.starts_with('{'))
        .map(|line| serde_json::from_str(line).expect("CLI trace event is valid JSON"))
        .collect();
    assert_eq!(
        events
            .iter()
            .filter(
                |event| event["fields"]["message"] == "resolving identity..."
                    && event["level"] == "DEBUG"
            )
            .count(),
        1,
        "{stderr}"
    );
    assert!(!stderr.contains('\x1b'), "{stderr}");
}

#[test]
fn existing_subscriber_is_preserved() {
    let capture = capture(
        false,
        &[("WW_LOG_FORMAT", "json"), ("WW_LOG_TEST_PROBE", "existing")],
    );
    assert_eq!(capture.stderr.matches("native-event").count(), 1);
    assert!(serde_json::from_str::<Value>(capture.stderr.lines().next().unwrap()).is_err());
}

/// Invoked only by the parent tests, with an isolated environment and subscriber.
#[test]
#[ignore = "subprocess probe"]
fn logging_probe() {
    if std::env::var("WW_LOG_TEST_PROBE").as_deref() == Ok("existing") {
        tracing_subscriber::fmt()
            .with_writer(std::io::stderr)
            .with_ansi(false)
            .try_init()
            .unwrap();
    }
    for _ in 0..2 {
        match std::env::var("WW_LOG_TEST_PROBE").as_deref() {
            Ok("legacy_stderr") => ww::config::init_tracing_to_stderr(true),
            Ok("legacy_stdout") => ww::config::init_tracing_to_stderr(false),
            _ => ww::config::init_tracing(),
        }
    }
    println!("{STDOUT_MARKER}");
    let error = io::Error::other("probe failed\nuseful detail");
    tracing::warn!(target: "ww::logging_probe", count = 42u64, active = true,
        text = "line one\nline two", error = &error as &dyn std::error::Error, "native-event");

    macro_rules! internal_events {
        ($($target:literal),+ $(,)?) => { $(
            tracing::info!(target: $target, "filter-event-info");
            tracing::warn!(target: $target, "filter-event-warn");
            tracing::debug!(target: $target, "filter-event-debug");
        )+ };
    }
    internal_events!("ww", "atom", "cache", "cell", "ipfs", "membrane", "rpc", "stem");
    tracing::info!(target: "external_dependency", "filter-event-external");

    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    runtime.block_on(async {
        let parent = tracing::info_span!(target: "ww::logging_probe", "parent", parent_id = 99);
        let first =
            tracing::info_span!(target: "ww::logging_probe", parent: &parent, "cell", cell_id = 1);
        let second =
            tracing::info_span!(target: "ww::logging_probe", parent: &parent, "cell", cell_id = 2);
        tokio::join!(
            emit_cell_event(1).instrument(first),
            emit_cell_event(2).instrument(second)
        );
    });
}

async fn emit_cell_event(cell_id: u64) {
    tokio::task::yield_now().await;
    tracing::info!(target: "ww::logging_probe", expected_cell_id = cell_id, "cell-event");
    tracing::warn!(target: "ww::logging_probe", expected_cell_id = cell_id, "cell-event-warning");
}
