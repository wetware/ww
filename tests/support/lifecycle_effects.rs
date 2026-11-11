//! Bounded real sockets and subprocesses for refused-start effect observations.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener};
use std::path::Path;
use std::process::{Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

pub struct Recorder {
    address: SocketAddr,
    requests: Arc<Mutex<Vec<Vec<u8>>>>,
    stopped: Arc<AtomicBool>,
    idle_generation: Arc<AtomicU64>,
    thread: Option<JoinHandle<()>>,
}

impl Recorder {
    pub fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        listener.set_nonblocking(true).unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let stopped = Arc::new(AtomicBool::new(false));
        let idle_generation = Arc::new(AtomicU64::new(0));
        let received = Arc::clone(&requests);
        let done = Arc::clone(&stopped);
        let idle = Arc::clone(&idle_generation);
        let thread = std::thread::spawn(move || {
            while !done.load(Ordering::Acquire) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        // Accepted sockets can inherit the listener's nonblocking mode.
                        stream.set_nonblocking(false).unwrap();
                        stream
                            .set_write_timeout(Some(Duration::from_millis(100)))
                            .unwrap();
                        // TCP reads can split the request line. Retain one
                        // bounded prefix per connection, not its first packet.
                        let deadline = Instant::now() + Duration::from_millis(100);
                        let mut bytes = Vec::with_capacity(8192);
                        let mut chunk = [0; 1024];
                        while bytes.len() < 8192 && !bytes.windows(2).any(|pair| pair == b"\r\n") {
                            let Some(remaining) = deadline.checked_duration_since(Instant::now())
                            else {
                                break;
                            };
                            if remaining.is_zero() {
                                break;
                            }
                            stream.set_read_timeout(Some(remaining)).unwrap();
                            let limit = (8192 - bytes.len()).min(chunk.len());
                            match stream.read(&mut chunk[..limit]) {
                                Ok(0) => break,
                                Ok(count) => bytes.extend_from_slice(&chunk[..count]),
                                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                                Err(_) => break,
                            }
                        }
                        let mut recorded = received.lock().unwrap();
                        // The first 64 connections suffice to disprove zero
                        // effects; retain at most 512 KiB even on regression.
                        if recorded.len() < 64 {
                            recorded.push(bytes);
                        }
                        drop(recorded);
                        let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}");
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        idle.fetch_add(1, Ordering::Release);
                        std::thread::sleep(Duration::from_millis(2));
                    }
                    Err(error) => panic!("effect recorder accept failed: {error}"),
                }
            }
        });
        Self {
            address,
            requests,
            stopped,
            idle_generation,
            thread: Some(thread),
        }
    }

    pub fn url(&self) -> String {
        format!("http://{}", self.address)
    }

    pub fn requests(&self) -> Vec<Vec<u8>> {
        // After a child exits, drain connections already queued in the kernel.
        // Sampling the vector without this barrier can report a false zero.
        let generation = self.idle_generation.load(Ordering::Acquire);
        let deadline = Instant::now() + Duration::from_secs(10);
        while self.idle_generation.load(Ordering::Acquire) == generation {
            assert!(Instant::now() < deadline, "effect recorder did not drain");
            std::thread::sleep(Duration::from_millis(2));
        }
        self.requests.lock().unwrap().clone()
    }

    pub fn wait_for_request(&self) {
        let deadline = Instant::now() + Duration::from_secs(3);
        while self.requests().is_empty() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(
            !self.requests().is_empty(),
            "effect recorder did not observe the positive control"
        );
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Release);
        self.thread.take().unwrap().join().unwrap();
    }
}

pub fn bounded_output(command: &mut Command, directory: &Path) -> (ExitStatus, String, String) {
    let stdout_path = directory.join("child-stdout");
    let stderr_path = directory.join("child-stderr");
    command
        .stdout(std::fs::File::create(&stdout_path).unwrap())
        .stderr(std::fs::File::create(&stderr_path).unwrap())
        .stdin(Stdio::null());
    let mut child = command.spawn().unwrap();
    let deadline = Instant::now() + Duration::from_secs(15);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        let excessive_output = [&stdout_path, &stderr_path]
            .iter()
            .any(|path| std::fs::metadata(path).unwrap().len() > 64 * 1024);
        if Instant::now() >= deadline || excessive_output {
            let _ = child.kill();
            child.wait().unwrap();
            panic!(
                "CLI exceeded refusal deadline or output bound: {}",
                std::fs::read_to_string(&stderr_path).unwrap()
            );
        }
        std::thread::sleep(Duration::from_millis(5));
    };
    (
        status,
        std::fs::read_to_string(stdout_path).unwrap(),
        std::fs::read_to_string(stderr_path).unwrap(),
    )
}

#[test]
fn recorder_preserves_a_request_line_split_across_tcp_writes() {
    let recorder = Recorder::start();
    let mut client = std::net::TcpStream::connect(recorder.address).unwrap();
    client
        .set_read_timeout(Some(Duration::from_secs(1)))
        .unwrap();
    client
        .set_write_timeout(Some(Duration::from_secs(1)))
        .unwrap();
    client.write_all(b"P").unwrap();
    // Give the recorder its first byte separately from the request-line suffix.
    std::thread::sleep(Duration::from_millis(20));
    let _ = client
        .write_all(b"OST /v1/traces HTTP/1.1\r\nHost: localhost\r\nContent-Length: 0\r\n\r\n");
    let mut response = Vec::new();
    let _ = client.read_to_end(&mut response);
    recorder.wait_for_request();
    let requests = recorder.requests();
    assert!(
        requests
            .iter()
            .any(|bytes| bytes.starts_with(b"POST /v1/traces HTTP/1.1\r\n")),
        "recorder truncated a fragmented request line: {requests:?}"
    );
}

#[test]
fn recorder_counts_an_empty_connection_and_bounds_its_wait() {
    let recorder = Recorder::start();
    let _client = std::net::TcpStream::connect(recorder.address).unwrap();
    let started = Instant::now();
    recorder.wait_for_request();
    assert!(started.elapsed() < Duration::from_secs(2));
    assert_eq!(recorder.requests(), vec![Vec::<u8>::new()]);
}

#[test]
fn recorder_bounds_an_unterminated_request_prefix() {
    let recorder = Recorder::start();
    let mut client = std::net::TcpStream::connect(recorder.address).unwrap();
    client
        .set_write_timeout(Some(Duration::from_secs(1)))
        .unwrap();
    client.write_all(&[b'A'; 8192]).unwrap();
    recorder.wait_for_request();
    assert_eq!(recorder.requests(), vec![vec![b'A'; 8192]]);
}
