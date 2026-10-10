//! Deterministic accepted-stream child: EOF -> Membrane gate -> output ->
//! Membrane completion gate -> exit. Gates are RPC promises, never sleeps.

use system::{system_capnp::membrane, Guest};
use wit_bindgen::StreamResult;

mod stdio {
    wit_bindgen::generate!({
        inline: r#"
            package wasi:cli@0.3.0;
            interface types { enum error-code { io, illegal-byte-sequence, pipe } }
            interface stdin {
                use types.{error-code};
                read-via-stream: func() -> tuple<stream<u8>, future<result<_, error-code>>>;
            }
            interface stdout {
                use types.{error-code};
                write-via-stream: func(data: stream<u8>) -> future<result<_, error-code>>;
            }
            world lifecycle-stdio { import stdin; import stdout; }
        "#,
        world: "lifecycle-stdio",
        generate_all,
    });
}

fn failed(message: impl std::fmt::Display) -> capnp::Error {
    capnp::Error::failed(message.to_string())
}

struct StreamLifecycle;

impl Guest for StreamLifecycle {
    async fn run() -> Result<(), ()> {
        system::run(|membrane: membrane::Client| async move {
            let (mut input, completion) = stdio::wasi::cli::stdin::read_via_stream();
            let mut request = Vec::new();
            loop {
                let (status, bytes) = input.read(Vec::with_capacity(16 * 1024)).await;
                request.extend_from_slice(&bytes);
                match status {
                    StreamResult::Complete(_) => {}
                    StreamResult::Dropped => break,
                    StreamResult::Cancelled => return Err(failed("stdin cancelled")),
                }
            }
            drop(input);
            completion
                .await
                .map_err(|error| failed(format!("stdin: {error:?}")))?;

            // Calling the supplied Membrane proves input EOF reached the child.
            // The host controls when this RPC resolves and supplies output bytes.
            let response = membrane.graft_request().send().promise.await?;
            let mut output = response.get()?.get_peer_id()?.to_vec();
            output.extend_from_slice(&request);
            drop(response);

            let (mut writer, reader) = stdio::wit_stream::new();
            let completion = stdio::wasi::cli::stdout::write_via_stream(reader);
            let write = async move {
                let remaining = writer.write_all(output).await;
                drop(writer);
                if remaining.is_empty() {
                    Ok(())
                } else {
                    Err(failed("stdout dropped before all bytes were accepted"))
                }
            };
            let (written, flushed) = futures::join!(write, async { completion.await });
            written?;
            flushed.map_err(|error| failed(format!("stdout: {error:?}")))?;

            // Keep the child alive after output until the test authorizes exit.
            // A failed gateway must release it even while this RPC is pending.
            membrane.graft_request().send().promise.await?;
            Ok(())
        })
        .await
        .map_err(|error| eprintln!("stream lifecycle probe: {error}"))
    }
}

system::export!(StreamLifecycle);
