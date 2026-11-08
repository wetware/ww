//! Process primitives only: these tests do not qualify Host admission or services.
use std::fs;
use std::os::unix::fs::{symlink, PermissionsExt};
use std::time::{Duration, Instant};
use ww::upgrade::attestation::{PinnedProcess, VerifiedExecutable};
use ww::upgrade::{ArtifactIdentity, DigestAlgorithm};

fn deadline() -> Instant {
    Instant::now() + Duration::from_secs(10)
}

fn identity(path: &std::path::Path, algorithm: DigestAlgorithm) -> ArtifactIdentity {
    use sha2::Digest;
    let bytes = fs::read(path).unwrap();
    let digest = match algorithm {
        DigestAlgorithm::Sha256 => hex::encode(sha2::Sha256::digest(&bytes)),
        DigestAlgorithm::Blake3 => blake3::hash(&bytes).to_hex().to_string(),
    };
    ArtifactIdentity {
        algorithm,
        digest,
        size: bytes.len() as u64,
    }
}

#[test]
fn descriptor_verification_checks_both_algorithms_exact_bytes_and_deadline() {
    let dir = tempfile::tempdir().unwrap();
    let executable = dir.path().join("fixture");
    fs::write(&executable, b"disposable byte fixture, not a release").unwrap();
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
    for algorithm in [DigestAlgorithm::Sha256, DigestAlgorithm::Blake3] {
        let expected = identity(&executable, algorithm);
        let verified = VerifiedExecutable::open(&executable, &expected, deadline()).unwrap();
        assert_eq!(verified.artifact(), &expected);
        verified.revalidate(deadline()).unwrap();
        assert!(verified.revalidate(Instant::now()).is_err());
        let mut wrong = expected.clone();
        wrong
            .digest
            .replace_range(..1, if &wrong.digest[..1] == "0" { "1" } else { "0" });
        assert!(VerifiedExecutable::open(&executable, &wrong, deadline()).is_err());
        wrong = expected;
        wrong.size += 1;
        assert!(VerifiedExecutable::open(&executable, &wrong, deadline()).is_err());
    }
    let link = dir.path().join("symlink");
    symlink(&executable, &link).unwrap();
    let expected = identity(&executable, DigestAlgorithm::Sha256);
    assert!(VerifiedExecutable::open(&link, &expected, deadline()).is_err());
    let verified = VerifiedExecutable::open(&executable, &expected, deadline()).unwrap();
    fs::write(&executable, b"changed bytes").unwrap();
    assert!(verified.revalidate(deadline()).is_err());
}

#[cfg(not(target_os = "linux"))]
#[test]
fn process_capture_refuses_unqualified_platform() {
    let expected = identity(&std::env::current_exe().unwrap(), DigestAlgorithm::Sha256);
    assert!(PinnedProcess::capture(std::process::id(), &expected, deadline()).is_err());
}

#[cfg(target_os = "linux")]
mod linux {
    use super::*;
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::os::unix::process::CommandExt;
    use std::path::PathBuf;
    use std::process::{Child, ChildStdin, Command, Stdio};
    use std::sync::mpsc::{self, Receiver};

    struct Probe {
        child: Child,
        input: ChildStdin,
        output: Receiver<String>,
        _directory: tempfile::TempDir,
        executable: PathBuf,
        socket: PathBuf,
        artifact: ArtifactIdentity,
        nonce: String,
    }

    impl Probe {
        fn start() -> Self {
            let directory = tempfile::tempdir().unwrap();
            let executable = directory.path().join("probe");
            fs::copy(std::env::current_exe().unwrap(), &executable).unwrap();
            let artifact = identity(&executable, DigestAlgorithm::Sha256);
            let socket = directory.path().join("evidence.sock");
            let mut child = Command::new(&executable)
                .args([
                    "--exact",
                    "linux::process_probe_child",
                    "--nocapture",
                    "--quiet",
                ])
                .env("WW739_ATTESTATION_CHILD", &socket)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::inherit())
                .spawn()
                .unwrap();
            let input = child.stdin.take().unwrap();
            let output = child.stdout.take().unwrap();
            let (tx, rx) = mpsc::channel();
            std::thread::spawn(move || {
                for line in BufReader::new(output).lines() {
                    if tx.send(line.unwrap()).is_err() {
                        break;
                    }
                }
            });
            let mut probe = Self {
                child,
                input,
                output: rx,
                _directory: directory,
                executable,
                socket,
                artifact,
                nonce: String::new(),
            };
            probe.nonce = probe.await_line("READY ");
            probe
        }

        fn await_line(&self, prefix: &str) -> String {
            let end = deadline();
            loop {
                let line = self
                    .output
                    .recv_timeout(end.saturating_duration_since(Instant::now()))
                    .expect("bounded child output");
                if let Some(value) = line.strip_prefix(prefix) {
                    return value.to_owned();
                }
            }
        }

        fn command(&mut self, command: &str) {
            writeln!(self.input, "{command}").unwrap();
            self.input.flush().unwrap();
        }
    }

    impl Drop for Probe {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }

    #[test]
    fn process_probe_child() {
        let Some(socket) = std::env::var_os("WW739_ATTESTATION_CHILD") else {
            return;
        };
        let _ = fs::remove_file(&socket);
        let listener = UnixListener::bind(&socket).unwrap();
        println!("READY {}", rand::random::<u64>());
        std::io::stdout().flush().unwrap();
        for line in std::io::stdin().lock().lines() {
            match line.unwrap().as_str() {
                "peer" => {
                    let (_stream, _) = listener.accept().unwrap();
                    println!("PEER accepted");
                }
                "exit" => std::process::exit(0),
                "exec-other" => {
                    let error = Command::new("/bin/sleep").arg("30").exec();
                    panic!("exec failed: {error}");
                }
                "exec-same" => {
                    let error = Command::new(std::env::current_exe().unwrap())
                        .args([
                            "--exact",
                            "linux::process_probe_child",
                            "--nocapture",
                            "--quiet",
                        ])
                        .exec();
                    panic!("exec failed: {error}");
                }
                other => panic!("unknown fixture command {other}"),
            }
            std::io::stdout().flush().unwrap();
        }
    }

    #[test]
    fn process_descriptor_survives_installed_path_replacement() {
        let probe = Probe::start();
        let pin = PinnedProcess::capture(probe.child.id(), &probe.artifact, deadline()).unwrap();
        let replacement = probe.executable.with_extension("replacement");
        fs::copy("/bin/sleep", &replacement).unwrap();
        fs::rename(replacement, &probe.executable).unwrap();
        pin.revalidate(deadline()).unwrap();
        assert!(VerifiedExecutable::open(&probe.executable, &probe.artifact, deadline()).is_err());
        assert_eq!(pin.executable().artifact(), &probe.artifact);
    }

    #[test]
    fn process_pin_refuses_exit_and_invalid_process() {
        let mut probe = Probe::start();
        let pin = PinnedProcess::capture(probe.child.id(), &probe.artifact, deadline()).unwrap();
        probe.command("exit");
        assert!(probe.child.wait().unwrap().success());
        assert!(pin.revalidate(deadline()).is_err());
        assert!(PinnedProcess::capture(0, &probe.artifact, deadline()).is_err());
        assert!(PinnedProcess::capture(u32::MAX, &probe.artifact, deadline()).is_err());
        assert!(
            PinnedProcess::capture(std::process::id(), &probe.artifact, Instant::now()).is_err()
        );
    }

    #[test]
    fn process_pin_reopens_executable_to_refuse_same_pid_changed_image() {
        let mut probe = Probe::start();
        let pin = PinnedProcess::capture(probe.child.id(), &probe.artifact, deadline()).unwrap();
        probe.command("exec-other");
        let end = deadline();
        loop {
            match fs::read_link(format!("/proc/{}/exe", probe.child.id())) {
                Ok(path) if path == fs::canonicalize("/bin/sleep").unwrap() => break,
                Ok(_) => {}
                // The fixture execs from a test worker thread. Linux can expose
                // no exe entry while exec replaces the original main thread.
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => panic!("observe fixture exec: {error}"),
            }
            assert!(probe.child.try_wait().unwrap().is_none());
            assert!(Instant::now() < end);
            std::thread::yield_now();
        }
        assert!(probe.child.try_wait().unwrap().is_none());
        assert!(pin.revalidate(deadline()).is_err());
        pin.executable().revalidate(deadline()).unwrap();
    }

    #[test]
    fn same_file_exec_is_not_an_os_execution_incarnation_token() {
        let mut probe = Probe::start();
        let pin = PinnedProcess::capture(probe.child.id(), &probe.artifact, deadline()).unwrap();
        probe.command("exec-same");
        let nonce = probe.await_line("READY ");
        assert_ne!(nonce, probe.nonce);
        let after = PinnedProcess::capture(probe.child.id(), &probe.artifact, deadline()).unwrap();
        assert_eq!(pin.start_token(), after.start_token());
        assert_eq!(
            pin.executable().file_identity(),
            after.executable().file_identity()
        );
        // This success is the documented limitation. Admission must separately
        // require a fresh protocol nonce and close-on-exec evidence connection.
        pin.revalidate(deadline()).unwrap();
    }

    #[test]
    fn socket_peer_must_match_pinned_pid_and_owner() {
        let mut probe = Probe::start();
        let pin = PinnedProcess::capture(probe.child.id(), &probe.artifact, deadline()).unwrap();
        probe.command("peer");
        let stream = UnixStream::connect(&probe.socket).unwrap();
        pin.verify_peer(&stream, deadline()).unwrap();
        assert_eq!(probe.await_line("PEER "), "accepted");
        let (wrong, _) = UnixStream::pair().unwrap();
        assert!(pin.verify_peer(&wrong, deadline()).is_err());
    }
}
