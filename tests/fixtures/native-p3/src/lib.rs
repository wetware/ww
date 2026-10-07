use futures::future::join3;
use std::fs;
use wasip3::filesystem::types::{Descriptor, DescriptorFlags, ErrorCode, OpenFlags, PathFlags};
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

async fn open_file(base: &Descriptor, path: &str) -> Descriptor {
    base.open_at(
        PathFlags::empty(),
        path.to_string(),
        OpenFlags::empty(),
        DescriptorFlags::READ,
    )
    .await
    .unwrap_or_else(|error| panic!("open {path}: {error:?}"))
}

async fn read_file(file: &Descriptor) -> Vec<u8> {
    let (reader, completion) = file.read_via_stream(0);
    let (bytes, result) = futures::join!(read_to_end(reader), async { completion.await });
    result.expect("file read completion");
    bytes
}

async fn assert_open_error(
    base: &Descriptor,
    path: &str,
    open_flags: OpenFlags,
    flags: DescriptorFlags,
    expected: ErrorCode,
) {
    match base
        .open_at(PathFlags::empty(), path.to_string(), open_flags, flags)
        .await
    {
        Ok(_) => panic!("open {path} with {open_flags:?}/{flags:?} unexpectedly succeeded"),
        Err(error) => assert_eq!(
            std::mem::discriminant(&error),
            std::mem::discriminant(&expected),
            "open {path} with {open_flags:?}/{flags:?}: expected {expected:?}, got {error:?}"
        ),
    }
}

async fn write_file(file: &Descriptor, bytes: &[u8], append: bool) {
    let (writer, reader) = wasip3::wit_stream::new();
    let completion = if append {
        file.append_via_stream(reader)
    } else {
        file.write_via_stream(reader, 0)
    };
    let ((), result) = futures::join!(write_and_close(writer, bytes.to_vec()), async {
        completion.await
    });
    result.expect("file write completion");
}

async fn assert_mutation_flags(base: &Descriptor, directory_name: &str) {
    // Reject every nonempty combination before any file contents are requested.
    for mask in 1..32 {
        let mut open_flags = OpenFlags::empty();
        let mut flags = DescriptorFlags::READ;
        if mask & 1 != 0 {
            flags |= DescriptorFlags::WRITE;
        }
        if mask & 2 != 0 {
            flags |= DescriptorFlags::MUTATE_DIRECTORY;
        }
        if mask & 4 != 0 {
            open_flags |= OpenFlags::CREATE;
        }
        if mask & 8 != 0 {
            open_flags |= OpenFlags::EXCLUSIVE;
        }
        if mask & 16 != 0 {
            open_flags |= OpenFlags::TRUNCATE;
        }
        for directory in [OpenFlags::empty(), OpenFlags::DIRECTORY] {
            for target in ["child", "missing", directory_name] {
                assert_open_error(
                    base,
                    target,
                    open_flags | directory,
                    flags,
                    ErrorCode::NotPermitted,
                )
                .await;
            }
        }
    }
}

async fn raw_filesystem_policy() {
    let preopens = wasip3::filesystem::preopens::get_directories();
    let root = &preopens.iter().find(|(_, path)| path == "/").unwrap().0;
    let scratch = &preopens.iter().find(|(_, path)| path == "/tmp").unwrap().0;

    assert_mutation_flags(root, "nested").await;

    // A directory remains a valid open result without DIRECTORY intent.
    let nested = open_file(root, "nested").await;
    assert_mutation_flags(&nested, "deeper").await;
    let sibling = open_file(root, "sibling").await;
    let deeper = nested
        .open_at(
            PathFlags::empty(),
            "deeper".into(),
            OpenFlags::DIRECTORY,
            DescriptorFlags::READ,
        )
        .await
        .unwrap();
    for (base, expected) in [
        (root, b"root child".as_slice()),
        (&nested, b"nested child".as_slice()),
        (&deeper, b"deep child".as_slice()),
        (&sibling, b"sibling child".as_slice()),
        (&nested, b"nested child".as_slice()),
    ] {
        assert_eq!(read_file(&open_file(base, "child").await).await, expected);
    }

    for path in [
        "..",
        "../sibling",
        "a/../../b",
        "/child",
        "//child",
        "/nested/child",
    ] {
        assert_open_error(
            &nested,
            path,
            OpenFlags::empty(),
            DescriptorFlags::READ,
            ErrorCode::Invalid,
        )
        .await;
    }
    for path in [".", "missing"] {
        assert_open_error(
            &nested,
            path,
            OpenFlags::empty(),
            DescriptorFlags::READ,
            ErrorCode::NoEntry,
        )
        .await;
    }
    assert_eq!(
        read_file(&open_file(&nested, "deeper//child").await).await,
        b"deep child"
    );
    for (path, expected) in [
        ("%2e%2e", b"literal dot escape".as_slice()),
        ("%2f", b"literal slash escape".as_slice()),
        (r"back\slash", b"literal backslash".as_slice()),
        (
            "ipfs/QmYwAPJzv5CZsnN625s3Xf2nemtYgPpHdWEz79ojWnPbdG/child",
            b"nested ipfs child".as_slice(),
        ),
    ] {
        assert_eq!(read_file(&open_file(&nested, path).await).await, expected);
    }

    let child = open_file(&nested, "child").await;
    assert_open_error(
        &child,
        "child",
        OpenFlags::empty(),
        DescriptorFlags::READ,
        ErrorCode::NotDirectory,
    )
    .await;
    assert_open_error(
        &child,
        "missing",
        OpenFlags::CREATE,
        DescriptorFlags::WRITE,
        ErrorCode::NotDirectory,
    )
    .await;
    assert_open_error(
        &nested,
        "child",
        OpenFlags::DIRECTORY,
        DescriptorFlags::READ,
        ErrorCode::NotDirectory,
    )
    .await;
    for path in ["child/", "child/other"] {
        assert_open_error(
            &nested,
            path,
            OpenFlags::empty(),
            DescriptorFlags::READ,
            ErrorCode::NotDirectory,
        )
        .await;
    }
    let (reader, completion) = nested.read_via_stream(0);
    let (bytes, result) = futures::join!(read_to_end(reader), async { completion.await });
    assert!(bytes.is_empty());
    assert!(
        matches!(result, Err(ErrorCode::IsDirectory)),
        "directory read: {result:?}"
    );

    // Observe each terminal future, including early host rejection of input.
    for append in [false, true] {
        let (mut writer, reader) = wasip3::wit_stream::new();
        let completion = if append {
            child.append_via_stream(reader)
        } else {
            child.write_via_stream(reader, 0)
        };
        let send = async move {
            let _ = writer.write_all(b"mutated".to_vec()).await;
            drop(writer);
        };
        let ((), result) = futures::join!(send, async { completion.await });
        assert!(
            matches!(result, Err(ErrorCode::NotPermitted)),
            "immutable write: {result:?}"
        );
    }
    assert_eq!(read_file(&child).await, b"nested child");

    let writable = scratch
        .open_at(
            PathFlags::empty(),
            "raw-probe.txt".into(),
            OpenFlags::CREATE | OpenFlags::EXCLUSIVE,
            DescriptorFlags::READ | DescriptorFlags::WRITE,
        )
        .await
        .unwrap();
    write_file(&writable, b"discarded contents", false).await;
    drop(writable);
    let writable = scratch
        .open_at(
            PathFlags::empty(),
            "raw-probe.txt".into(),
            OpenFlags::TRUNCATE,
            DescriptorFlags::READ | DescriptorFlags::WRITE,
        )
        .await
        .unwrap();
    assert!(read_file(&writable).await.is_empty());
    write_file(&writable, b"raw scratch", false).await;
    write_file(&writable, b" appended", true).await;
    assert_eq!(read_file(&writable).await, b"raw scratch appended");

    // Drop both descriptor classes, then open replacements while siblings live.
    drop(deeper);
    drop(writable);
    let replacement = open_file(&sibling, "child").await;
    assert_eq!(read_file(&replacement).await, b"sibling child");
    assert_open_error(
        &nested,
        "child",
        OpenFlags::TRUNCATE,
        DescriptorFlags::WRITE,
        ErrorCode::NotPermitted,
    )
    .await;
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
        raw_filesystem_policy().await;
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
