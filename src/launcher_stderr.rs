//! Metadata-only child stderr ownership. No guest bytes enter tracing fields.

use std::{cell::Cell, rc::Rc, time::Duration};

use rpc::observability::EventCode;
use tokio::{
    io::{self, AsyncRead, AsyncReadExt},
    task::JoinHandle,
};
use tracing::Instrument;

pub(super) const PIPE_BYTES: usize = 64 * 1024;
const READ_BYTES: usize = 8 * 1024;
const DRAIN_GRACE: Duration = Duration::from_secs(1);

/// Owns the reader even while `finish` is suspended. Dropping the owner aborts
/// the reader and reports an uncertified drain; it does not claim async cleanup.
pub(super) struct ChildStderr {
    task: Option<JoinHandle<io::Result<()>>>,
    bytes: Rc<Cell<u64>>,
    cell_id: u64,
    span: tracing::Span,
}

impl ChildStderr {
    pub(super) fn spawn(
        reader: impl AsyncRead + Unpin + 'static,
        cell_id: u64,
        span: tracing::Span,
    ) -> Self {
        let bytes = Rc::new(Cell::new(0));
        let task = tokio::task::spawn_local(drain(reader, bytes.clone()).instrument(span.clone()));
        Self {
            task: Some(task),
            bytes,
            cell_id,
            span,
        }
    }

    /// Join through EOF, or abort and join after the teardown deadline.
    ///
    /// The return value certifies worker teardown, not output completeness.
    /// A handled I/O error or deadline can release all resources with an unknown
    /// unread tail. An unexpected JoinError cannot certify normal worker return.
    pub(super) async fn finish(&mut self) -> bool {
        let Some(task) = self.task.as_mut() else {
            return true;
        };
        let (event, terminalized) = match tokio::time::timeout(DRAIN_GRACE, &mut *task).await {
            Ok(Ok(Ok(()))) => (EventCode::CellStderrDrained, true),
            Ok(Ok(Err(_))) => (EventCode::CellStderrReadFailed, true),
            Ok(Err(_)) => (EventCode::CellStderrWorkerLost, false),
            Err(_) => {
                task.abort();
                match task.await {
                    // Completion can win the race with the deadline/abort.
                    Ok(Ok(())) => (EventCode::CellStderrDrained, true),
                    Ok(Err(_)) => (EventCode::CellStderrReadFailed, true),
                    Err(error) if error.is_cancelled() => (EventCode::CellStderrTimedOut, true),
                    Err(_) => (EventCode::CellStderrWorkerLost, false),
                }
            }
        };
        self.task.take();
        self.report(event);
        terminalized
    }

    fn report(&self, event: EventCode) {
        let completed = event == EventCode::CellStderrDrained;
        // Explicit parent and identity also cover Drop outside the child task,
        // and filters that disable the child span but retain WARN events.
        if completed {
            tracing::trace!(
                target: "ww::launcher",
                parent: &self.span,
                event_code = event as u16,
                cell_id = self.cell_id,
                bytes = self.bytes.get(),
                completed,
                "child stderr drained"
            );
        } else {
            tracing::warn!(
                target: "ww::launcher",
                parent: &self.span,
                event_code = event as u16,
                cell_id = self.cell_id,
                bytes = self.bytes.get(),
                completed,
                timeout_ms = DRAIN_GRACE.as_millis() as u64,
                "child stderr drain incomplete"
            );
        }
    }
}

impl Drop for ChildStderr {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
            self.report(EventCode::CellStderrCancelled);
        }
    }
}

async fn drain(mut reader: impl AsyncRead + Unpin, bytes: Rc<Cell<u64>>) -> io::Result<()> {
    let mut buffer = [0; READ_BYTES];
    loop {
        let count = reader.read(&mut buffer).await?;
        if count == 0 {
            return Ok(());
        }
        bytes.set(bytes.get().saturating_add(count as u64));
        // Do not let an always-ready stream monopolize the LocalSet or starve
        // its teardown timer. Guest text is neither decoded nor retained.
        tokio::task::yield_now().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::launcher::tracing_tests::Output;
    use futures::FutureExt;
    use std::{
        pin::Pin,
        task::{Context, Poll},
    };
    use tokio::io::{AsyncWriteExt, ReadBuf};
    use tracing_subscriber::{layer::SubscriberExt, Layer};

    const SECRET: &[u8] = b"{\"level\":\"ERROR\",\"event_code\":100,\"message\":\"guest-secret\"}\nfinal fragment\xff";

    fn capture(json: bool, spans: bool) -> (Output, tracing::subscriber::DefaultGuard) {
        let output = Output::default();
        let writer = output.clone();
        let layer = tracing_subscriber::fmt::layer()
            .fmt_fields(crate::logging::SafeFields)
            .event_format(crate::logging::SafeEvent { json, ansi: false })
            .with_writer(move || writer.clone())
            .with_filter(tracing_subscriber::filter::filter_fn(move |metadata| {
                spans || !metadata.is_span()
            }));
        let guard = tracing::subscriber::set_default(tracing_subscriber::registry().with(layer));
        (output, guard)
    }

    fn events(output: &Output) -> Vec<serde_json::Value> {
        let bytes = output.0.lock().unwrap();
        let text = std::str::from_utf8(&bytes).unwrap();
        assert!(!text.contains("guest-secret"));
        text.lines()
            .map(|line| {
                assert!(line.len() < 16_384);
                serde_json::from_str(line).unwrap()
            })
            .collect()
    }

    // Value: protects=tail bytes and binary data before cleanup; fails_when=the
    // reader is aborted at teardown or uses UTF-8 lines; seam=real duplex.
    #[tokio::test(flavor = "current_thread")]
    async fn buffered_tail_is_drained_before_finish_even_without_reader_poll() {
        let (output, _guard) = capture(true, true);
        tokio::task::LocalSet::new()
            .run_until(async {
                let (reader, mut writer) = io::duplex(PIPE_BYTES);
                writer.write_all(SECRET).await.unwrap();
                drop(writer);
                let span = tracing::info_span!(parent: None, "cell", cell_id = 41u64);
                let mut stderr = ChildStderr::spawn(reader, 41, span);
                assert!(stderr.finish().await);
                assert_eq!(stderr.bytes.get(), SECRET.len() as u64);
            })
            .await;
        let events = events(&output);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["fields"]["event_code"], 204);
        assert_eq!(events[0]["fields"]["completed"], true);
        assert_eq!(events[0]["fields"]["bytes"], SECRET.len());
        assert_eq!(events[0]["fields"]["cell_id"], 41);
        assert_eq!(events[0]["span"]["cell_id"], 41);
        assert_eq!(events[0]["fields"]["reason"], "eof");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn large_newline_free_binary_stream_emits_one_summary() {
        let (output, _guard) = capture(true, false);
        tokio::task::LocalSet::new()
            .run_until(async {
                let (reader, mut writer) = io::duplex(PIPE_BYTES);
                let mut stderr = ChildStderr::spawn(reader, 42, tracing::Span::none());
                // Fixed-size producer chunks; no input-size allocation on either side.
                let chunk = [0xff; READ_BYTES];
                for _ in 0..256 {
                    writer.write_all(&chunk).await.unwrap();
                }
                drop(writer);
                assert!(stderr.finish().await);
                assert_eq!(stderr.bytes.get(), (READ_BYTES * 256) as u64);
            })
            .await;
        let events = events(&output);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["fields"]["cell_id"], 42);
        assert_eq!(events[0]["fields"]["completed"], true);
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn held_open_writer_times_out_then_reader_is_joined() {
        let (output, _guard) = capture(true, false);
        tokio::task::LocalSet::new()
            .run_until(async {
                let (reader, mut writer) = io::duplex(PIPE_BYTES);
                writer.write_all(SECRET).await.unwrap();
                let mut stderr = ChildStderr::spawn(reader, 43, tracing::Span::none());
                let start = tokio::time::Instant::now();
                assert!(stderr.finish().await, "cancelled worker was joined");
                assert_eq!(start.elapsed(), DRAIN_GRACE);
                assert_eq!(stderr.bytes.get(), SECRET.len() as u64);
                assert!(
                    writer.write_all(b"late").await.is_err(),
                    "reader was released"
                );
            })
            .await;
        let events = events(&output);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["fields"]["event_code"], 205);
        assert_eq!(events[0]["fields"]["completed"], false);
        assert_eq!(events[0]["fields"]["timeout_ms"], 1000);
        assert_eq!(events[0]["fields"]["bytes"], SECRET.len());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn owner_cancellation_during_finish_preserves_identity_and_aborts_reader() {
        let (output, _guard) = capture(true, true);
        tokio::task::LocalSet::new()
            .run_until(async {
                let (reader, mut writer) = io::duplex(PIPE_BYTES);
                let span = tracing::info_span!(parent: None, "cell", cell_id = 44u64);
                let mut stderr = ChildStderr::spawn(reader, 44, span);
                let mut finish = Box::pin(stderr.finish());
                assert!(finish.as_mut().now_or_never().is_none());
                drop(finish);
                drop(stderr);
                tokio::task::yield_now().await;
                assert!(writer.write_all(b"late").await.is_err());
            })
            .await;
        let events = events(&output);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["fields"]["event_code"], 206);
        assert_eq!(events[0]["fields"]["completed"], false);
        assert_eq!(events[0]["span"]["cell_id"], 44);
    }

    struct FailedReader {
        panic: bool,
    }

    // Unlike DuplexStream, this reader has no Tokio cooperative I/O budget.
    // The drain must yield explicitly so a ready producer cannot starve teardown.
    struct AlwaysReadyReader;
    impl AsyncRead for AlwaysReadyReader {
        fn poll_read(
            self: Pin<&mut Self>,
            _: &mut Context<'_>,
            buffer: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            buffer.put_slice(&[0xff; READ_BYTES]);
            Poll::Ready(Ok(()))
        }
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn always_ready_output_allows_deadline_and_sibling_progress() {
        let (output, _guard) = capture(true, false);
        tokio::task::LocalSet::new()
            .run_until(async {
                let mut stderr = ChildStderr::spawn(AlwaysReadyReader, 47, tracing::Span::none());
                let mut finish = Box::pin(stderr.finish());
                assert!(finish.as_mut().now_or_never().is_none());
                tokio::task::yield_now().await;
                tokio::time::advance(DRAIN_GRACE).await;
                assert!(finish.await);
            })
            .await;
        let events = events(&output);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["fields"]["event_code"], 205);
        assert!(events[0]["fields"]["bytes"].as_u64().unwrap() > 0);
    }

    impl AsyncRead for FailedReader {
        fn poll_read(
            self: Pin<&mut Self>,
            _: &mut Context<'_>,
            _: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            assert!(!self.panic, "injected reader panic");
            Poll::Ready(Err(io::Error::other("guest-secret")))
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn read_failure_and_worker_loss_have_distinct_cleanup_results() {
        let (output, _guard) = capture(true, false);
        tokio::task::LocalSet::new()
            .run_until(async {
                for panic in [false, true] {
                    let mut stderr =
                        ChildStderr::spawn(FailedReader { panic }, 45, tracing::Span::none());
                    assert_eq!(stderr.finish().await, !panic);
                }
            })
            .await;
        let events = events(&output);
        assert_eq!(events.len(), 2);
        assert_eq!(events[0]["fields"]["event_code"], 207);
        assert_eq!(events[1]["fields"]["event_code"], 208);
        assert!(events
            .iter()
            .all(|event| event["fields"]["completed"] == false));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn compact_summary_does_not_render_guest_content_or_guest_level() {
        let (output, _guard) = capture(false, false);
        tokio::task::LocalSet::new()
            .run_until(async {
                let mut stderr =
                    ChildStderr::spawn(std::io::Cursor::new(SECRET), 46, tracing::Span::none());
                assert!(stderr.finish().await);
            })
            .await;
        let bytes = output.0.lock().unwrap();
        let text = std::str::from_utf8(&bytes).unwrap();
        assert_eq!(text.lines().count(), 1);
        assert!(text.contains("TRACE"));
        assert!(text.contains("event_code=204"));
        assert!(text.contains("cell_id=46"));
        assert!(!text.contains("guest-secret"));
        assert!(!text.contains("ERROR"));
    }
}
