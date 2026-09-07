use futures::future::join3;
use std::fs;
use wit_bindgen::{StreamResult, StreamWriter};

wit_bindgen::generate!({
    path: "../../../crates/cell/wit/p3",
    world: "host-substrate-test",
    generate_all,
});

struct Fixture;

async fn write_and_close(mut writer: StreamWriter<u8>, bytes: Vec<u8>) {
    let remaining = writer.write_all(bytes).await;
    assert!(
        remaining.is_empty(),
        "transport output closed before all bytes were written"
    );
    drop(writer);
}

async fn read_to_end(mut reader: wit_bindgen::StreamReader<u8>) -> Vec<u8> {
    let mut output = Vec::new();
    loop {
        let (status, bytes) = reader.read(Vec::with_capacity(16 * 1024)).await;
        output.extend_from_slice(&bytes);
        match status {
            StreamResult::Complete(count) => {
                assert_eq!(count, bytes.len());
                assert!(count > 0, "transport read completed without progress");
            }
            StreamResult::Dropped => return output,
            StreamResult::Cancelled => panic!("transport read was cancelled"),
        }
    }
}

fn open() -> (
    StreamWriter<u8>,
    wit_bindgen::StreamReader<u8>,
    wit_bindgen::FutureReader<Result<(), wetware::transport::connection::TransportError>>,
) {
    let (outgoing, outgoing_reader) = wit_stream::new();
    let (incoming, completion) = wetware::transport::connection::open(outgoing_reader);
    (outgoing, incoming, completion)
}

impl exports::wetware::transport::fixture::Guest for Fixture {
    async fn exchange(outgoing: Vec<u8>) -> Vec<u8> {
        let (writer, incoming, _completion) = open();
        let ((), incoming) =
            futures::join!(write_and_close(writer, outgoing), read_to_end(incoming));
        incoming
    }

    async fn receive_then_send(outgoing: Vec<u8>) -> Vec<u8> {
        let (writer, incoming, _completion) = open();
        let received = read_to_end(incoming).await;
        write_and_close(writer, outgoing).await;
        received
    }

    async fn send(outgoing: Vec<u8>) {
        let (writer, incoming, _completion) = open();
        drop(incoming);
        write_and_close(writer, outgoing).await;
    }

    async fn drop_incoming(outgoing: Vec<u8>) -> bool {
        let (writer, incoming, completion) = open();
        drop(incoming);
        write_and_close(writer, outgoing).await;
        completion.await.is_ok()
    }

    async fn orderly_close() -> bool {
        let (writer, incoming, completion) = open();
        drop(writer);
        let bytes = read_to_end(incoming).await;
        bytes.is_empty() && completion.await.is_ok()
    }

    async fn abnormal_failure() -> String {
        let (mut writer, _incoming, completion) = open();
        let _ = writer.write_all(vec![1]).await;
        drop(writer);
        match completion.await {
            Ok(()) => "unexpected orderly close".to_string(),
            Err(wetware::transport::connection::TransportError::Failed(message)) => message,
        }
    }

    async fn second_open() -> String {
        let (first_writer, first_incoming, _first_completion) = open();
        drop(first_writer);
        drop(first_incoming);

        let (second_writer, second_incoming, second_completion) = open();
        drop(second_writer);
        drop(second_incoming);
        match second_completion.await {
            Ok(()) => "unexpected second connection".to_string(),
            Err(wetware::transport::connection::TransportError::Failed(message)) => message,
        }
    }

    async fn wait_on_live_resources() {
        let (mut writer, mut incoming, _completion) = open();
        let write = writer.write_all(vec![0xa5; 256 * 1024]);
        let read = incoming.next();
        let clock = wasip3::clocks::monotonic_clock::wait_for(60_000_000_000);
        let _ = join3(write, read, clock).await;
    }

    async fn filesystem() -> exports::wetware::transport::fixture::FilesystemObservation {
        let image = fs::read("/known.txt").expect("read image-backed file");
        let missing_rejected = fs::read("/missing.txt").is_err();
        let traversal_rejected = fs::read("/../ambient-secret").is_err();
        let image_write_rejected = fs::write("/known.txt", b"mutated").is_err();
        fs::write("/tmp/probe.txt", b"scratch-data").expect("write /tmp file");
        let scratch = fs::read("/tmp/probe.txt").expect("read /tmp file");

        exports::wetware::transport::fixture::FilesystemObservation {
            image,
            missing_rejected,
            traversal_rejected,
            image_write_rejected,
            scratch,
        }
    }
}

export!(Fixture);
