//! Minimal WASI echo cell for integration testing.
//!
//! Reads all of stdin, writes it to stdout unchanged, then exits.
//! Used by executor integration tests to validate the spawn and stdio path.

use wasip2::cli::stdin::get_stdin;
use wasip2::cli::stdout::get_stdout;
use wasip2::exports::cli::run::Guest;
use wasip2::io::streams::{InputStream, OutputStream};

struct EchoCell;

impl Guest for EchoCell {
    fn run() -> Result<(), ()> {
        let stdin: InputStream = get_stdin();
        let stdout: OutputStream = get_stdout();

        // Read all of stdin, write to stdout
        loop {
            // Subscribe and wait for data
            let pollable = stdin.subscribe();
            pollable.block();

            match stdin.read(65536) {
                Ok(bytes) if bytes.is_empty() => break, // EOF
                Ok(bytes) => {
                    // Write all bytes to stdout
                    let mut offset = 0;
                    while offset < bytes.len() {
                        let out_pollable = stdout.subscribe();
                        out_pollable.block();
                        match stdout.check_write() {
                            Ok(n) if n > 0 => {
                                let end = std::cmp::min(offset + n as usize, bytes.len());
                                stdout.write(&bytes[offset..end]).unwrap();
                                stdout.flush().unwrap();
                                offset = end;
                            }
                            _ => break,
                        }
                    }
                }
                Err(_) => break, // stream closed
            }
        }

        // Flush and close stdout
        let _ = stdout.flush();
        Ok(())
    }
}

wasip2::cli::command::export!(EchoCell);
