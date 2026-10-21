//! Bounded publication of captured local bytes. This module never opens host paths.

use anyhow::{ensure, Context, Result};
use bytes::Bytes;
use cid::Cid;
use http_body_util::BodyExt;
use serde::Deserialize;
use std::{
    collections::BTreeMap,
    sync::atomic::{AtomicUsize, Ordering},
};

use crate::{BootClient, HttpClient, KuboApiError, KuboResponseLimit};

const MAX_MULTIPART_BYTES: usize = 80 * 1024 * 1024;
const MAX_RESPONSE_BYTES: usize = 16 * 1024 * 1024;
const MAX_LINE_BYTES: usize = 16 * 1024;

/// Batch-wide allowances. Reuse this value across every source and retry.
/// Charges are conservative and remain consumed after cancellation or failure.
#[derive(Debug, Default)]
pub struct PublicationBudget {
    multipart_bytes: AtomicUsize,
    response_bytes: AtomicUsize,
}

impl PublicationBudget {
    pub fn multipart_bytes(&self) -> usize {
        self.multipart_bytes.load(Ordering::Relaxed)
    }
    pub fn response_bytes(&self) -> usize {
        self.response_bytes.load(Ordering::Relaxed)
    }

    fn require_response_allowance(&self) -> Result<()> {
        ensure!(
            self.response_bytes() < MAX_RESPONSE_BYTES,
            "snapshot add response batch exceeds {MAX_RESPONSE_BYTES} bytes"
        );
        Ok(())
    }

    fn remaining_multipart_bytes(&self) -> usize {
        MAX_MULTIPART_BYTES.saturating_sub(self.multipart_bytes())
    }
}

fn charge(counter: &AtomicUsize, amount: usize, limit: usize, what: &str) -> Result<()> {
    counter
        .try_update(Ordering::Relaxed, Ordering::Relaxed, |used| {
            used.checked_add(amount).filter(|total| *total <= limit)
        })
        .map_err(|_| anyhow::anyhow!("snapshot {what} exceeds {limit} bytes"))?;
    Ok(())
}

/// One immutable captured manifest entry; the root has an empty relative path.
#[derive(Clone, Debug)]
pub struct CapturedEntry {
    pub path: String,
    /// `None` represents a directory, including an empty directory.
    pub data: Option<Bytes>,
}

/// Kubo's claim, pending independent DAG and captured-manifest verification.
#[derive(Clone, Debug)]
pub struct AddedEntry {
    pub cid: Cid,
    pub dag_size: u64,
}

/// An acknowledged publication. This alone is not a verified frozen source.
#[derive(Debug)]
pub struct PublishedSnapshot {
    pub events: BTreeMap<String, AddedEntry>,
}

/// Validate the complete response; success does not establish DAG correctness.
/// Schema follows Kubo v0.33.0 `core/commands/add.go` (not coreiface's Mode type).
pub fn parse_add_response(body: &[u8], entries: &[CapturedEntry]) -> Result<PublishedSnapshot> {
    ensure!(
        body.len() <= MAX_RESPONSE_BYTES,
        "snapshot add response too large"
    );
    let expected = validate_entries(entries)?;
    let text = std::str::from_utf8(body).context("snapshot add response is not UTF-8")?;
    let mut events = BTreeMap::new();
    let mut root_seen = false;
    for line in text.split('\n') {
        ensure!(
            line.len() <= MAX_LINE_BYTES,
            "snapshot add JSON line too large"
        );
        if line.trim().is_empty() {
            continue;
        }
        ensure!(!root_seen, "snapshot add returned an event after its root");
        ensure!(
            line.trim_start().starts_with('{'),
            "snapshot add event must be an object"
        );
        let event: AddEvent = serde_json::from_str(line).context("invalid snapshot add event")?;
        ensure!(event.bytes == 0, "snapshot add returned a progress event");
        ensure!(
            event.mode.is_empty() && event.mtime == 0 && event.mtime_nsecs == 0,
            "snapshot add returned host metadata"
        );
        let path = if event.name == "source" {
            ""
        } else {
            let path = event
                .name
                .strip_prefix("source/")
                .context("snapshot add returned an unknown path")?;
            ensure!(
                !path.is_empty(),
                "snapshot add returned an invalid root name"
            );
            path
        };
        ensure!(
            expected.contains_key(path),
            "snapshot add returned an unknown path: {}",
            event.name
        );
        let cid = crate::cid_identity::parse_cid(&event.hash)?;
        ensure!(
            cid.version() == cid::Version::V1
                && matches!(cid.codec(), 0x55 | 0x70)
                && cid.hash().code() == 0x12
                && cid.hash().size() == 32,
            "snapshot add returned a CID outside the import profile"
        );
        ensure!(
            !event.size.is_empty() && event.size.bytes().all(|b| b.is_ascii_digit()),
            "snapshot add Size must be a decimal u64"
        );
        let dag_size = event
            .size
            .parse::<u64>()
            .context("snapshot add Size exceeds u64")?;
        ensure!(
            events
                .insert(path.to_owned(), AddedEntry { cid, dag_size })
                .is_none(),
            "snapshot add returned a duplicate path"
        );
        root_seen = path.is_empty();
    }
    ensure!(
        root_seen && events.len() == expected.len(),
        "snapshot add omitted manifest entries or its final root"
    );
    Ok(PublishedSnapshot { events })
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "PascalCase")]
struct AddEvent {
    name: String,
    hash: String,
    size: String,
    #[serde(default)]
    bytes: i64,
    #[serde(default)]
    mode: String,
    #[serde(default)]
    mtime: i64,
    #[serde(default, rename = "MtimeNsecs")]
    mtime_nsecs: i64,
}

fn validate_entries(entries: &[CapturedEntry]) -> Result<BTreeMap<&str, bool>> {
    ensure!(
        !entries.is_empty() && entries.len() <= 100_000,
        "snapshot manifest node limit"
    );
    ensure!(
        entries[0].path.is_empty(),
        "snapshot manifest must start with root"
    );
    let mut expected = BTreeMap::new();
    let mut path_bytes = 0usize;
    let mut file_bytes = 0usize;
    for (index, entry) in entries.iter().enumerate() {
        ensure!(
            entry.path.len() <= 4096,
            "snapshot relative path exceeds 4096 bytes"
        );
        path_bytes = path_bytes
            .checked_add(entry.path.len())
            .context("snapshot path size overflow")?;
        ensure!(path_bytes <= 8 * 1024 * 1024, "snapshot path byte limit");
        if index > 0 {
            ensure!(
                !entry.path.is_empty()
                    && entry.path.split('/').count() <= 128
                    && entry.path.split('/').all(|c| !c.is_empty()
                        && c != "."
                        && c != ".."
                        && !c.bytes().any(|byte| byte.is_ascii_control())),
                "invalid snapshot relative path"
            );
            let parent = entry.path.rsplit_once('/').map_or("", |(p, _)| p);
            ensure!(
                expected.get(parent) == Some(&true),
                "snapshot entry lacks its parent directory"
            );
            ensure!(
                entries[index - 1]
                    .path
                    .split('/')
                    .cmp(entry.path.split('/'))
                    .is_lt(),
                "snapshot entries must follow sorted depth-first order"
            );
        }
        if let Some(data) = &entry.data {
            file_bytes = file_bytes
                .checked_add(data.len())
                .context("snapshot byte size overflow")?;
            ensure!(
                file_bytes <= 64 * 1024 * 1024,
                "snapshot captured byte limit"
            );
        }
        ensure!(
            expected
                .insert(entry.path.as_str(), entry.data.is_none())
                .is_none(),
            "duplicate snapshot entry"
        );
    }
    Ok(expected)
}

// One encoding is reused for every attempt. File bodies share captured Bytes;
// only bounded multipart framing is allocated. Kubo query-unescapes filenames.
struct Multipart {
    boundary: String,
    chunks: Vec<Bytes>,
    len: usize,
}

impl Multipart {
    fn encode(entries: &[CapturedEntry], remaining_bytes: usize) -> Result<Self> {
        validate_entries(entries)?;
        let boundary = reqwest::multipart::Form::new().boundary().to_owned();
        // Count the exact encoded body before allocating any multipart framing.
        // A retry reuses this encoding but independently charges the full body.
        let expected_len = Self::encoded_len(entries, boundary.len(), remaining_bytes)?;
        let mut body = Self {
            boundary,
            chunks: Vec::new(),
            len: 0,
        };
        for entry in entries {
            let mut name = String::from("source");
            if !entry.path.is_empty() {
                name.push('/');
                // QueryUnescape treats '+' as space. Escape it and '%' too.
                for byte in entry.path.bytes() {
                    if byte.is_ascii_alphanumeric() || b"-._~/".contains(&byte) {
                        name.push(byte as char);
                    } else {
                        use std::fmt::Write;
                        write!(name, "%{byte:02X}")?;
                    }
                }
            }
            let mime = if entry.data.is_none() {
                "application/x-directory"
            } else {
                "application/octet-stream"
            };
            body.push(Bytes::from(format!("--{}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"{name}\"\r\nContent-Type: {mime}\r\n\r\n", body.boundary)))?;
            if let Some(bytes) = &entry.data {
                body.push(bytes.clone())?;
            }
            body.push(Bytes::from_static(b"\r\n"))?;
        }
        body.push(Bytes::from(format!("--{}--\r\n", body.boundary)))?;
        debug_assert_eq!(body.len, expected_len);
        Ok(body)
    }

    fn encoded_len(
        entries: &[CapturedEntry],
        boundary_len: usize,
        remaining_bytes: usize,
    ) -> Result<usize> {
        let limit = remaining_bytes.min(MAX_MULTIPART_BYTES);
        let mut len = boundary_len
            .checked_add(6)
            .context("snapshot multipart size overflow")?;
        ensure!(
            len <= limit,
            "snapshot multipart batch exceeds remaining {limit} bytes"
        );
        for entry in entries {
            let name_len = "source".len()
                + usize::from(!entry.path.is_empty())
                + entry
                    .path
                    .bytes()
                    .map(|b| {
                        if b.is_ascii_alphanumeric() || b"-._~/".contains(&b) {
                            1
                        } else {
                            3
                        }
                    })
                    .sum::<usize>();
            let mime = if entry.data.is_none() {
                "application/x-directory"
            } else {
                "application/octet-stream"
            };
            for size in [
                "--".len(),
                boundary_len,
                "\r\nContent-Disposition: form-data; name=\"file\"; filename=\"".len(),
                name_len,
                "\"\r\nContent-Type: ".len(),
                mime.len(),
                "\r\n\r\n".len(),
                entry.data.as_ref().map_or(0, Bytes::len),
                "\r\n".len(),
            ] {
                len = len
                    .checked_add(size)
                    .context("snapshot multipart size overflow")?;
                ensure!(
                    len <= limit,
                    "snapshot multipart batch exceeds remaining {limit} bytes"
                );
            }
        }
        Ok(len)
    }

    fn push(&mut self, chunk: Bytes) -> Result<()> {
        self.len = self
            .len
            .checked_add(chunk.len())
            .context("snapshot multipart size overflow")?;
        ensure!(
            self.len <= MAX_MULTIPART_BYTES,
            "snapshot multipart exceeds {MAX_MULTIPART_BYTES} bytes"
        );
        self.chunks.push(chunk);
        Ok(())
    }
}

impl BootClient {
    /// Publish only captured bytes under the existing retry/timeout policy.
    /// No partial or unacknowledged result is returned. Failure/cancellation may
    /// leave blocks or pins; this path never issues compensating unpins.
    pub async fn publish_snapshot(
        &self,
        entries: &[CapturedEntry],
        budget: &PublicationBudget,
    ) -> Result<PublishedSnapshot> {
        budget.require_response_allowance()?;
        let multipart = Multipart::encode(entries, budget.remaining_multipart_bytes())?;
        self.retry("captured local source publication", || {
            self.client
                .publish_snapshot_body(entries, &multipart, budget)
        })
        .await
    }
}

impl HttpClient {
    /// Perform one publication attempt. Prefer BootClient for bounded timeouts.
    pub async fn publish_snapshot(
        &self,
        entries: &[CapturedEntry],
        budget: &PublicationBudget,
    ) -> Result<PublishedSnapshot> {
        budget.require_response_allowance()?;
        let multipart = Multipart::encode(entries, budget.remaining_multipart_bytes())?;
        self.publish_snapshot_body(entries, &multipart, budget)
            .await
    }

    async fn publish_snapshot_body(
        &self,
        entries: &[CapturedEntry],
        multipart: &Multipart,
        budget: &PublicationBudget,
    ) -> Result<PublishedSnapshot> {
        budget.require_response_allowance()?;
        charge(
            &budget.multipart_bytes,
            multipart.len,
            MAX_MULTIPART_BYTES,
            "multipart batch",
        )?;
        let chunks = multipart.chunks.clone();
        let body = reqwest::Body::wrap_stream(futures::stream::iter(
            chunks.into_iter().map(Ok::<_, std::io::Error>),
        ));
        let response = self
            .http_client
            .post(format!("{}/api/v0/add", self.base_url))
            .query(&[
                ("pin", "true"),
                ("wrap-with-directory", "false"),
                ("cid-version", "1"),
                ("hash", "sha2-256"),
                ("raw-leaves", "true"),
                ("chunker", "size-262144"),
                ("trickle", "false"),
                ("inline", "false"),
                ("nocopy", "false"),
                ("fscache", "false"),
                ("preserve-mode", "false"),
                ("preserve-mtime", "false"),
                ("progress", "false"),
                ("silent", "false"),
                ("only-hash", "false"),
            ])
            .header(
                reqwest::header::CONTENT_TYPE,
                format!("multipart/form-data; boundary={}", multipart.boundary),
            )
            .header(reqwest::header::CONTENT_LENGTH, multipart.len)
            .body(body)
            .send()
            .await
            .context("failed to publish captured source")?;
        let response = read_add_response(response, budget).await?;
        let published = parse_add_response(&response, entries)?;
        self.import_composed(&published.events[""].cid, &[])
            .await
            .context("captured source pin acknowledgement failed; residual pins may remain")?;
        Ok(published)
    }
}

async fn read_add_response(
    response: reqwest::Response,
    budget: &PublicationBudget,
) -> Result<Vec<u8>> {
    let status = response.status();
    ensure!(
        response
            .headers()
            .get_all("X-Stream-Error")
            .iter()
            .all(|value| value.as_bytes().is_empty()),
        "snapshot add returned a streaming error header"
    );
    let limit = (if status.is_success() {
        MAX_RESPONSE_BYTES
    } else {
        64 * 1024
    })
    .min(MAX_RESPONSE_BYTES.saturating_sub(budget.response_bytes()));
    ensure!(
        response.content_length().is_none_or(|n| n <= limit as u64),
        KuboResponseLimit {
            operation: "snapshot add".into(),
            status,
            limit
        }
    );
    let response: http::Response<reqwest::Body> = response.into();
    let mut frames = response.into_body();
    let mut body = Vec::new();
    let mut line_bytes = 0usize;
    while let Some(frame) = frames.frame().await {
        let frame = frame.context("incomplete snapshot add response")?;
        let chunk = match frame.into_data() {
            Ok(chunk) => chunk,
            Err(frame) => {
                if let Some(trailers) = frame.trailers_ref() {
                    ensure!(
                        trailers
                            .get_all("X-Stream-Error")
                            .iter()
                            .all(|value| value.as_bytes().is_empty()),
                        "snapshot add returned a streaming error trailer"
                    );
                }
                continue;
            }
        };
        ensure!(
            chunk.len() <= limit.saturating_sub(body.len()),
            KuboResponseLimit {
                operation: "snapshot add".into(),
                status,
                limit
            }
        );
        charge(
            &budget.response_bytes,
            chunk.len(),
            MAX_RESPONSE_BYTES,
            "add response batch",
        )?;
        if status.is_success() {
            for byte in &chunk {
                if *byte == b'\n' {
                    line_bytes = 0;
                } else {
                    line_bytes += 1;
                }
                ensure!(
                    line_bytes <= MAX_LINE_BYTES,
                    "snapshot add JSON line too large"
                );
            }
        }
        body.extend_from_slice(&chunk);
    }
    if !status.is_success() {
        return Err(KuboApiError {
            operation: "snapshot add".into(),
            status,
            message: String::from_utf8_lossy(&body).into_owned(),
        }
        .into());
    }
    Ok(body)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cid() -> Cid {
        Cid::new_v1(
            0x55,
            cid::multihash::Multihash::wrap(0x12, &[7; 32]).unwrap(),
        )
    }

    fn entries() -> Vec<CapturedEntry> {
        vec![
            CapturedEntry {
                path: String::new(),
                data: None,
            },
            CapturedEntry {
                path: "a".into(),
                data: Some(Bytes::from_static(b"abc")),
            },
        ]
    }

    fn event(name: &str) -> String {
        format!(r#"{{"Name":"{name}","Hash":"{}","Size":"3"}}"#, cid())
    }

    #[test]
    fn accepts_exact_manifest_and_final_root() {
        let parsed = parse_add_response(
            format!("{}\n{}\n", event("source/a"), event("source")).as_bytes(),
            &entries(),
        )
        .unwrap();
        assert_eq!(parsed.events.len(), 2);
        assert_eq!(parsed.events[""].cid, cid());
        assert_eq!(parsed.events["a"].dag_size, 3);
    }

    #[test]
    fn rejects_incomplete_duplicate_unknown_and_late_events() {
        let child = event("source/a");
        let root = event("source");
        for body in [
            String::new(),
            child.clone(),
            root.clone(),
            format!("{root}\n{child}"),
            format!("{child}\n{child}\n{root}"),
            format!("{}\n{child}\n{root}", event("source/no")),
            format!("{child}\n{root}\n{{\"Message\":\"failed\"}}"),
            format!("{child}\n{root}\ngarbage"),
            format!("{child}\n{root}\n{{"),
        ] {
            assert!(
                parse_add_response(body.as_bytes(), &entries()).is_err(),
                "accepted {body}"
            );
        }
    }

    #[test]
    fn rejects_invalid_schema_types_and_duplicate_fields() {
        let file = [CapturedEntry {
            path: String::new(),
            data: Some(Bytes::new()),
        }];
        for body in [
            format!(r#"["source","{}","0"]"#, cid()),
            format!(
                r#"{{"Name":"source","Hash":"{}","Size":"0","Unknown":0}}"#,
                cid()
            ),
            format!(
                r#"{{"Name":"source","Name":"source","Hash":"{}","Size":"0"}}"#,
                cid()
            ),
            format!(r#"{{"Name":"source","Hash":"{}","Size":0}}"#, cid()),
            format!(r#"{{"Name":"source","Hash":"{}/bad","Size":"0"}}"#, cid()),
            format!(r#"{{"Name":"source","Hash":"{}","Size":"+0"}}"#, cid()),
            format!(
                r#"{{"Name":"source","Hash":"{}","Size":"18446744073709551616"}}"#,
                cid()
            ),
            format!(
                r#"{{"Name":"source","Hash":"{}","Size":"0","Bytes":1}}"#,
                cid()
            ),
            format!(
                r#"{{"Name":"source","Hash":"{}","Size":"0","Bytes":null}}"#,
                cid()
            ),
            format!(
                r#"{{"Name":"source","Hash":"{}","Size":"0","Mode":0}}"#,
                cid()
            ),
            format!(
                r#"{{"Name":"source","Hash":"{}","Size":"0","Mtime":"0"}}"#,
                cid()
            ),
            format!(
                r#"{{"Name":"source","Hash":"{}","Size":"0","MtimeNsecs":0.5}}"#,
                cid()
            ),
            format!(r#"{{"Name":"source/","Hash":"{}","Size":"0"}}"#, cid()),
            r#"{"Name":"source","Bytes":0}"#.to_owned(),
        ] {
            assert!(
                parse_add_response(body.as_bytes(), &file).is_err(),
                "accepted {body}"
            );
        }
    }

    #[test]
    fn optional_fields_have_exact_types_and_metadata_remains_omitted() {
        let file = [CapturedEntry {
            path: String::new(),
            data: Some(Bytes::new()),
        }];
        let body = format!(
            r#"{{"Name":"source","Hash":"{}","Size":"0","Bytes":0,"Mode":"","Mtime":0,"MtimeNsecs":0}}"#,
            cid()
        );
        parse_add_response(body.as_bytes(), &file).unwrap();
        for (from, to) in [
            ("\"Bytes\":0", "\"Bytes\":-1"),
            ("\"Mode\":\"\"", "\"Mode\":\"0644\""),
            ("\"Mtime\":0", "\"Mtime\":1"),
            ("\"MtimeNsecs\":0", "\"MtimeNsecs\":1"),
        ] {
            assert!(parse_add_response(body.replace(from, to).as_bytes(), &file).is_err());
        }
    }

    #[test]
    fn line_and_aggregate_limits_are_inclusive() {
        let file = [CapturedEntry {
            path: String::new(),
            data: Some(Bytes::new()),
        }];
        let mut body = event("source").into_bytes();
        body.resize(MAX_LINE_BYTES, b' ');
        parse_add_response(&body, &file).unwrap();
        body.push(b' ');
        assert!(parse_add_response(&body, &file).is_err());
        body.truncate(MAX_LINE_BYTES);
        body.resize(MAX_RESPONSE_BYTES, b'\n');
        parse_add_response(&body, &file).unwrap();
        body.push(b'\n');
        assert!(parse_add_response(&body, &file).is_err());
        for limit in [MAX_MULTIPART_BYTES, MAX_RESPONSE_BYTES] {
            let counter = AtomicUsize::new(limit - 1);
            charge(&counter, 1, limit, "test").unwrap();
            assert!(charge(&counter, 1, limit, "test").is_err());
            assert_eq!(counter.load(Ordering::Relaxed), limit);
        }
    }

    #[test]
    fn multipart_preserves_empty_directories_and_escapes_exact_names() {
        let entries = vec![
            CapturedEntry {
                path: "".into(),
                data: None,
            },
            CapturedEntry {
                path: "empty".into(),
                data: None,
            },
            CapturedEntry {
                path: "q+%\"é".into(),
                data: Some(Bytes::from_static(b"frozen bytes")),
            },
        ];
        let multipart = Multipart::encode(&entries, MAX_MULTIPART_BYTES).unwrap();
        let body = multipart.chunks.concat();
        assert_eq!(multipart.len, body.len());
        let body = String::from_utf8(body).unwrap();
        assert_eq!(body.matches("application/x-directory").count(), 2);
        assert!(body.contains("filename=\"source\""));
        assert!(body.contains("filename=\"source/empty\""));
        assert!(body.contains("filename=\"source/q%2B%25%22%C3%A9\""));
        assert!(body.contains("frozen bytes"));
        assert!(!body.contains("mtime"));
    }

    #[test]
    fn multipart_preflight_counts_framing_and_encoded_names_before_allocation() {
        let entries = [
            CapturedEntry {
                path: "".into(),
                data: None,
            },
            CapturedEntry {
                path: "empty".into(),
                data: None,
            },
            CapturedEntry {
                path: "q+%\"é".into(),
                data: Some(Bytes::from_static(b"payload")),
            },
        ];
        let body = Multipart::encode(&entries, MAX_MULTIPART_BYTES).unwrap();
        assert_eq!(
            Multipart::encoded_len(&entries, body.boundary.len(), body.len).unwrap(),
            body.len
        );
        assert!(Multipart::encoded_len(&entries, body.boundary.len(), body.len - 1).is_err());
        assert!(Multipart::encode(&entries, 0).is_err());
        assert!(Multipart::encode(&entries, body.len - 1).is_err());
        assert_eq!(Multipart::encode(&entries, body.len).unwrap().len, body.len);
    }

    #[test]
    fn multipart_rejects_invalid_missing_duplicate_and_disordered_paths() {
        for path in [
            "/absolute",
            "a//b",
            "a/../b",
            "a/./b",
            "a/",
            "bad\0name",
            "missing/child",
        ] {
            let entries = [
                CapturedEntry {
                    path: "".into(),
                    data: None,
                },
                CapturedEntry {
                    path: path.into(),
                    data: Some(Bytes::new()),
                },
            ];
            assert!(
                Multipart::encode(&entries, MAX_MULTIPART_BYTES).is_err(),
                "accepted {path:?}"
            );
        }
        assert!(Multipart::encode(&[], MAX_MULTIPART_BYTES).is_err());
        let entries = [
            CapturedEntry {
                path: "".into(),
                data: None,
            },
            CapturedEntry {
                path: "b".into(),
                data: None,
            },
            CapturedEntry {
                path: "a".into(),
                data: None,
            },
        ];
        assert!(Multipart::encode(&entries, MAX_MULTIPART_BYTES).is_err());
    }

    #[test]
    fn c1_and_format_names_preserve_existing_unicode_policy() {
        let entries = [
            CapturedEntry {
                path: String::new(),
                data: None,
            },
            CapturedEntry {
                path: "\u{85}\u{200d}".into(),
                data: Some(Bytes::new()),
            },
        ];
        Multipart::encode(&entries, MAX_MULTIPART_BYTES).unwrap();
        parse_add_response(
            format!("{}\n{}\n", event("source/\u{85}\u{200d}"), event("source")).as_bytes(),
            &entries,
        )
        .unwrap();
    }

    #[tokio::test]
    async fn late_streaming_error_trailer_rejects_complete_root_before_car() {
        let body = format!("{}\n{}\n", event("source/a"), event("source"));
        let response = format!("HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nTrailer: X-Stream-Error\r\nConnection: close\r\n\r\n{:x}\r\n{body}\r\n0\r\nX-Stream-Error: failed after root\r\n\r\n", body.len());
        let (client, server) = raw_server(vec![response]).await;
        let error = client
            .publish_snapshot(&entries(), &PublicationBudget::default())
            .await
            .unwrap_err();
        assert!(
            format!("{error:#}").contains("streaming error trailer"),
            "{error:#}"
        );
        let requests = server.await.unwrap();
        assert_eq!(requests.len(), 1);
        assert!(String::from_utf8_lossy(&requests[0]).contains("/api/v0/add?"));
    }

    async fn server(
        responses: Vec<(u16, String)>,
    ) -> (HttpClient, tokio::task::JoinHandle<Vec<Vec<u8>>>) {
        raw_server(responses.into_iter().map(|(status,body)|format!("HTTP/1.1 {status} Test\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len())).collect()).await
    }

    async fn read_request(stream: &mut tokio::net::TcpStream) -> Vec<u8> {
        use tokio::io::AsyncReadExt;
        let mut request = Vec::new();
        loop {
            let mut bytes = [0u8; 4096];
            let n = stream.read(&mut bytes).await.unwrap();
            if n == 0 {
                break;
            }
            request.extend_from_slice(&bytes[..n]);
            if let Some(end) = request.windows(4).position(|x| x == b"\r\n\r\n") {
                let header = String::from_utf8_lossy(&request[..end]);
                let length: usize = header
                    .lines()
                    .find_map(|line| {
                        line.to_ascii_lowercase()
                            .strip_prefix("content-length: ")
                            .map(|v| v.parse().unwrap())
                    })
                    .unwrap();
                if request.len() >= end + 4 + length {
                    break;
                }
            }
        }
        request
    }

    async fn raw_server(
        responses: Vec<String>,
    ) -> (HttpClient, tokio::task::JoinHandle<Vec<Vec<u8>>>) {
        use tokio::io::AsyncWriteExt;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let client = HttpClient::new(format!("http://{}", listener.local_addr().unwrap()));
        let task = tokio::spawn(async move {
            let mut requests = Vec::new();
            for response in responses {
                let (mut stream, _) = listener.accept().await.unwrap();
                requests.push(read_request(&mut stream).await);
                let _ = stream.write_all(response.as_bytes()).await;
            }
            requests
        });
        (client, task)
    }

    fn ack() -> String {
        format!(
            r#"{{"Root":{{"Cid":{{"/":"{}"}},"PinErrorMsg":""}}}}"#,
            cid()
        )
    }

    #[tokio::test]
    async fn publication_uses_profile_and_requires_second_pin_acknowledgement() {
        let (client, task) = server(vec![
            (200, format!("{}\n{}\n", event("source/a"), event("source"))),
            (200, ack()),
        ])
        .await;
        let budget = PublicationBudget::default();
        client.publish_snapshot(&entries(), &budget).await.unwrap();
        let requests = task.await.unwrap();
        let add = String::from_utf8_lossy(&requests[0]);
        for option in [
            "pin=true",
            "wrap-with-directory=false",
            "cid-version=1",
            "hash=sha2-256",
            "raw-leaves=true",
            "chunker=size-262144",
            "trickle=false",
            "inline=false",
            "nocopy=false",
            "fscache=false",
            "preserve-mode=false",
            "preserve-mtime=false",
            "progress=false",
            "silent=false",
            "only-hash=false",
        ] {
            assert!(add.contains(option), "missing {option}");
        }
        let pin = String::from_utf8_lossy(&requests[1]);
        assert!(pin.starts_with("POST /api/v0/dag/import?"));
        assert!(pin.contains("pin-roots=true"));
        assert!(!pin.contains("pin/rm"));
        let body_start = requests[0]
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
            .unwrap()
            + 4;
        assert_eq!(budget.multipart_bytes(), requests[0].len() - body_start);
        assert!(budget.response_bytes() > 0);
    }

    #[tokio::test]
    async fn retry_resends_identical_bytes_and_consumes_the_shared_budget() {
        let good = format!("{}\n{}\n", event("source/a"), event("source"));
        let (client, task) = server(vec![
            (503, "unavailable".into()),
            (200, good.clone()),
            (200, ack()),
        ])
        .await;
        let budget = PublicationBudget::default();
        BootClient::new(client, 5, 2)
            .publish_snapshot(&entries(), &budget)
            .await
            .unwrap();
        let requests = task.await.unwrap();
        assert_eq!(requests[0], requests[1]);
        let start = requests[0]
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
            .unwrap()
            + 4;
        assert_eq!(budget.multipart_bytes(), 2 * (requests[0].len() - start));
        assert_eq!(budget.response_bytes(), "unavailable".len() + good.len());
    }

    #[tokio::test]
    async fn malformed_or_missing_ack_never_returns_a_published_source() {
        for acknowledgement in [
            String::new(),
            format!("{}\n{{", ack()),
            ack().replace(
                "\"PinErrorMsg\":\"\"",
                "\"PinErrorMsg\":\"missing descendant\"",
            ),
        ] {
            let (client, task) = server(vec![
                (200, format!("{}\n{}", event("source/a"), event("source"))),
                (200, acknowledgement),
            ])
            .await;
            assert!(client
                .publish_snapshot(&entries(), &PublicationBudget::default())
                .await
                .is_err());
            assert_eq!(task.await.unwrap().len(), 2);
        }
    }

    #[tokio::test]
    async fn invalid_full_response_never_requests_pin_acknowledgement() {
        let body = format!(
            "{}\n{}\n{{\"Message\":\"late error\"}}",
            event("source/a"),
            event("source")
        );
        let (client, task) = server(vec![(200, body)]).await;
        assert!(client
            .publish_snapshot(&entries(), &PublicationBudget::default())
            .await
            .is_err());
        assert_eq!(task.await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn truncated_stream_and_oversized_responses_fail_after_root_event() {
        let good = format!("{}\n{}\n", event("source/a"), event("source"));
        for raw in [
            format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{good}",good.len()+1),
            format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{good}",MAX_RESPONSE_BYTES+1),
            format!("HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n{:x}\r\n{good}\r\n",good.len()),
            format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{good}{}",good.len()+MAX_LINE_BYTES+1," ".repeat(MAX_LINE_BYTES+1)),
        ] {
            let (client,task) = raw_server(vec![raw]).await;
            assert!(client.publish_snapshot(&entries(),&PublicationBudget::default()).await.is_err());
            assert_eq!(task.await.unwrap().len(),1);
        }
    }

    #[tokio::test]
    async fn batch_limits_persist_between_sources_and_precede_network_requests() {
        let good = format!("{}\n{}\n", event("source/a"), event("source"));
        let (client, task) = server(vec![(200, good), (200, ack())]).await;
        let budget = PublicationBudget::default();
        client.publish_snapshot(&entries(), &budget).await.unwrap();
        let uploaded = budget.multipart_bytes();
        budget
            .response_bytes
            .store(MAX_RESPONSE_BYTES, Ordering::Relaxed);
        let error = client
            .publish_snapshot(&entries(), &budget)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("add response batch"));
        assert_eq!(budget.multipart_bytes(), uploaded);
        assert_eq!(task.await.unwrap().len(), 2);
        budget.response_bytes.store(0, Ordering::Relaxed);
        budget
            .multipart_bytes
            .store(MAX_MULTIPART_BYTES, Ordering::Relaxed);
        let error = client
            .publish_snapshot(&entries(), &budget)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("multipart batch"));
    }

    #[tokio::test]
    async fn retry_checks_exhausted_response_allowance_before_another_upload() {
        let (client, task) = server(vec![(503, "unavailable".into())]).await;
        let budget = PublicationBudget::default();
        budget
            .response_bytes
            .store(MAX_RESPONSE_BYTES - "unavailable".len(), Ordering::Relaxed);
        let error = BootClient::new(client, 5, 2)
            .publish_snapshot(&entries(), &budget)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("add response batch"));
        let requests = task.await.unwrap();
        assert_eq!(requests.len(), 1);
        let start = requests[0]
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
            .unwrap()
            + 4;
        assert_eq!(budget.multipart_bytes(), requests[0].len() - start);
        assert_eq!(budget.response_bytes(), MAX_RESPONSE_BYTES);
    }

    #[tokio::test]
    async fn cancellation_before_complete_add_or_pin_ack_retains_uncertain_roots() {
        use std::sync::Arc;
        use tokio::{io::AsyncWriteExt, sync::oneshot};
        // Gate before the root event, after the root event but before /add
        // completes, and after /add while CAR acknowledgement is incomplete.
        for stage in 0..3 {
            let after_add = stage == 2;
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let client = HttpClient::new(format!("http://{}", listener.local_addr().unwrap()));
            let (ready_tx, ready_rx) = oneshot::channel();
            let (release_tx, release_rx) = oneshot::channel();
            let server = tokio::spawn(async move {
                let mut requests = Vec::new();
                if after_add {
                    let (mut stream, _) = listener.accept().await.unwrap();
                    requests.push(read_request(&mut stream).await);
                    let good = format!("{}\n{}\n", event("source/a"), event("source"));
                    stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{good}",good.len()).as_bytes()).await.unwrap();
                }
                let (mut stream, _) = listener.accept().await.unwrap();
                requests.push(read_request(&mut stream).await);
                let partial = if after_add {
                    ack()
                } else if stage == 0 {
                    format!("{}\n", event("source/a"))
                } else {
                    format!("{}\n{}\n", event("source/a"), event("source"))
                };
                stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{partial}",partial.len()+1).as_bytes()).await.unwrap();
                ready_tx.send(()).unwrap();
                release_rx.await.unwrap();
                assert!(
                    tokio::time::timeout(std::time::Duration::from_millis(20), listener.accept())
                        .await
                        .is_err(),
                    "cancellation issued another request"
                );
                requests
            });
            let budget = Arc::new(PublicationBudget::default());
            let task_budget = budget.clone();
            let publication =
                tokio::spawn(
                    async move { client.publish_snapshot(&entries(), &task_budget).await },
                );
            ready_rx.await.unwrap();
            assert!(!publication.is_finished());
            publication.abort();
            assert!(publication.await.unwrap_err().is_cancelled());
            release_tx.send(()).unwrap();
            let requests = server.await.unwrap();
            assert_eq!(requests.len(), if after_add { 2 } else { 1 });
            assert!(requests
                .iter()
                .all(|r| !String::from_utf8_lossy(r).contains("pin/rm")));
            assert!(budget.multipart_bytes() > 0);
        }
    }
}
