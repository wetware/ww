//! CLI error output and identity creation are public automation contracts.
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::process::Command;

#[path = "support/lifecycle_effects.rs"]
mod lifecycle_effects;

fn ww(home: &std::path::Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_ww"));
    command.env("HOME", home).current_dir(home);
    command
}

#[test]
fn keygen_relative_filename_is_private_and_never_replaced() {
    let home = tempfile::tempdir().unwrap();
    std::fs::set_permissions(home.path(), std::fs::Permissions::from_mode(0o770)).unwrap();
    for relative in ["identity", "new-parent/nested/identity"] {
        let generate = || {
            let mut command = ww(home.path());
            command.args(["keygen", "--output", relative]);
            // SAFETY: umask is async-signal-safe and affects only this child.
            unsafe {
                command.pre_exec(|| {
                    libc::umask(0o002);
                    Ok(())
                });
            }
            command.output().unwrap()
        };
        let output = generate();
        assert!(output.status.success(), "{output:?}");
        let path = home.path().join(relative);
        let original = std::fs::read(&path).unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().mode() & 0o777, 0o600);
        for parent in path
            .parent()
            .unwrap()
            .ancestors()
            .take_while(|p| *p != home.path())
        {
            assert_eq!(std::fs::metadata(parent).unwrap().mode() & 0o777, 0o700);
        }
        let again = generate();
        assert!(!again.status.success());
        assert_eq!(std::fs::read(path).unwrap(), original);
        assert_eq!(
            std::fs::metadata(home.path()).unwrap().mode() & 0o777,
            0o770
        );
    }
}

#[test]
fn lifecycle_operational_failures_emit_one_json_result() {
    for args in [
        vec!["install"],
        vec!["image", "update"],
        vec!["uninstall"],
        vec!["daemon", "install"],
    ] {
        let home = tempfile::tempdir().unwrap();
        let output = ww(home.path()).args(&args).arg("--json").output().unwrap();
        assert!(!output.status.success(), "{args:?}: {output:?}");
        let result: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(result["version"], 1);
        assert_eq!(result["status"], "failed");
        assert!(result["error"].as_str().is_some_and(|s| !s.is_empty()));
        assert!(result["activation"].is_null());
        assert!(result["recovery"].is_object());
    }
}

#[test]
fn guarded_start_refusals_precede_kubo_ipns_otlp_and_listener_effects() {
    use lifecycle_effects::{bounded_output, Recorder};
    use std::net::TcpListener;
    use std::path::Path;

    // Moving effects ahead of GuardedStartup::open would either contact one
    // of these recorders or fail on an occupied listener before admission.
    let root = tempfile::tempdir().unwrap();
    let home = std::fs::canonicalize(root.path()).unwrap();
    let bin = home.join(".ww/bin");
    let store = bin.join(".ww-host-upgrade");
    for directory in [&bin, &store, &home.join("custom-fhs")] {
        std::fs::create_dir_all(directory).unwrap();
        std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let installed = bin.join("ww");
    std::fs::copy(env!("CARGO_BIN_EXE_ww"), &installed).unwrap();
    let identity = home.join(".ww/identity");
    let private_bytes = b"invalid identity sentinel: refusal must precede key loading\n";
    std::fs::write(&identity, private_bytes).unwrap();
    std::fs::set_permissions(&identity, std::fs::Permissions::from_mode(0o600)).unwrap();
    let kubo = Recorder::start();
    let ipns = Recorder::start();
    let otlp = Recorder::start();
    // Keep these exact addresses occupied for the complete child lifetime.
    // A premature required listener bind changes the error to AddrInUse.
    let admin = TcpListener::bind("127.0.0.1:0").unwrap();
    let wagi = TcpListener::bind("127.0.0.1:0").unwrap();
    let swarm = TcpListener::bind("127.0.0.1:0").unwrap();
    let configure = |executable: &Path| {
        let mut command = Command::new(executable);
        command
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", &home)
            .env("WW_COMPILE_WORKERS", "1")
            .env("WW_EXECUTOR_THREADS", "1")
            .env("WW_OTLP_ENDPOINT", format!("{}/v1/traces", otlp.url()))
            .env("WW_TRACE_SAMPLE_RATIO", "1")
            .env("WW_TRACE_MAX_SPANS_PER_SECOND", "100")
            .current_dir(&home)
            .args([
                "run",
                "--with-http-admin",
                &admin.local_addr().unwrap().to_string(),
                "--http-listen",
                &wagi.local_addr().unwrap().to_string(),
                "--listen",
                &format!("/ip4/127.0.0.1/tcp/{}", swarm.local_addr().unwrap().port()),
                "--ipfs-url",
                &kubo.url(),
                "--ipns-routing-url",
                &ipns.url(),
            ]);
        command
    };
    for case in [
        "missing-marker",
        "custom-baseline",
        "ephemeral-identity",
        "proxy",
        "missing-state",
        "corrupt-state",
    ] {
        let mut command = configure(&installed);
        let expected = match case {
            "custom-baseline" => {
                command.arg("--fhs-root").arg(home.join("custom-fhs"));
                "startup lease"
            }
            "ephemeral-identity" => {
                command.arg("--insecure-ephemeral");
                "guarded startup requires persistent identity"
            }
            "proxy" => {
                command.env("HTTPS_PROXY", kubo.url());
                "guarded startup rejects proxy"
            }
            "missing-state" => {
                for name in [".ww-startup.lock", ".ww-host-instance.lock"] {
                    let path = home.join(name);
                    std::fs::write(&path, []).unwrap();
                    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
                }
                "required startup state is missing"
            }
            "corrupt-state" => {
                let path = store.join("startup-state.json");
                std::fs::write(&path, br#"{"version":999}"#).unwrap();
                std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
                "startup"
            }
            _ => "startup lease",
        };
        let (status, stdout, stderr) = bounded_output(&mut command, &home);
        assert!(!status.success(), "{case}: unexpectedly ran Host");
        assert!(stdout.is_empty(), "{case}: unexpected stdout {stdout}");
        assert!(stderr.contains(expected), "{case}: {stderr}");
        assert!(
            !stderr.contains("invalid identity") && !stderr.contains("Address already in use"),
            "{case}: {stderr}"
        );
        assert_eq!(std::fs::read(&identity).unwrap(), private_bytes);
        for (name, recorder) in [("Kubo", &kubo), ("IPNS", &ipns), ("OTLP", &otlp)] {
            assert!(
                recorder.requests().is_empty(),
                "{case}: forbidden {name} connections: {:?}",
                recorder.requests()
            );
        }
    }

    // Exercise the same occupied admin address through the real unmanaged
    // branch. This control proves the socket would catch an attempted bind.
    let mut unmanaged = configure(Path::new(env!("CARGO_BIN_EXE_ww")));
    unmanaged.args(["--insecure-ephemeral", "."]);
    std::fs::remove_file(&identity).unwrap();
    let (status, _, stderr) = bounded_output(&mut unmanaged, &home);
    assert!(!status.success());
    assert!(
        stderr.contains("bind")
            && (stderr.contains("Address already in use") || stderr.contains("address in use")),
        "listener control did not reach the real bind: {stderr}"
    );
}

#[test]
fn refusal_effect_recorder_observes_real_otlp_exports() {
    use lifecycle_effects::Recorder;
    use tracing_subscriber::layer::SubscriberExt;
    let recorder = Recorder::start();
    let config =
        ww::telemetry::Config::new(&format!("{}/v1/traces", recorder.url()), 1.0, 100).unwrap();
    let (layer, guard) = ww::telemetry::start(config).unwrap();
    tracing::subscriber::with_default(tracing_subscriber::registry().with(layer), || {
        drop(tracing::info_span!("cell", owner_id = 739u64));
    });
    drop(guard);
    recorder.wait_for_request();
    assert!(recorder
        .requests()
        .iter()
        .any(|bytes| bytes.starts_with(b"POST /v1/traces HTTP/1.1\r\n")));
}
