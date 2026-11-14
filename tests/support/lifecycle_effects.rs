//! Bounded subprocess observation for ordinary startup and lifecycle tests.
use std::path::Path;
use std::process::{Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

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
