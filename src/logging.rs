//! Output policy for the Host's existing tracing subscriber.
//!
//! Values are untrusted, including `message`, Display/Debug, and span updates.
//! Only approved numeric/boolean fields and the event catalog reach output.
use std::{error::Error, fmt};

use serde_json::{json, Map, Value};
use tracing::{
    field::{Field, Visit},
    span, Event, Subscriber,
};
use tracing_subscriber::{
    field::RecordFields,
    fmt::{
        format::{FormatEvent, FormatFields, Writer},
        FmtContext, FormattedFields,
    },
    registry::LookupSpan,
};

const MAX_FIELDS: usize = 32;
const MAX_CHAIN: usize = 8;
const MAX_RECORD_BYTES: usize = 16_384;
const REDACTED: &str = "[redacted]";

#[derive(Default)]
pub(crate) struct Fields {
    values: Map<String, Value>,
    truncated: bool,
}

impl Fields {
    fn insert(&mut self, name: &str, value: Value) {
        // Field names come from callsite metadata, but still have a size cap.
        if name.len() > 64
            || !name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.'))
            || (self.values.len() >= MAX_FIELDS && !self.values.contains_key(name))
        {
            self.truncated = true;
        } else {
            self.values.insert(name.to_owned(), value);
        }
    }

    pub(crate) fn finish(mut self) -> Map<String, Value> {
        if self.truncated {
            self.values
                .insert("fields_truncated".into(), Value::Bool(true));
        }
        self.values
    }
}

fn numeric(name: &str) -> bool {
    matches!(
        name,
        "event_code"
            | "connection_id"
            | "registration_id"
            | "issued_epoch"
            | "seq"
            | "retry_ms"
            | "capacity"
            | "active"
            | "owner_id"
            | "cell_id"
            | "rpc_id"
            | "epoch"
            | "count"
            | "bytes"
            | "exit_code"
            | "elapsed_ms"
            | "timeout_ms"
            | "workers"
            | "worker"
            | "code"
            | "old_epoch"
            | "new_epoch"
            | "intended_seq"
            | "live_seq"
            | "teardown_ms"
            | "grant"
            | "current_epoch"
            | "generation"
            | "duration_us"
    )
}

impl Visit for Fields {
    fn record_u128(&mut self, field: &Field, value: u128) {
        if numeric(field.name()) {
            let bounded = u64::try_from(value).unwrap_or(u64::MAX);
            if u128::from(bounded) != value {
                self.truncated = true;
            }
            self.record_u64(field, bounded);
        } else {
            self.insert(field.name(), REDACTED.into());
        }
    }

    fn record_i128(&mut self, field: &Field, value: i128) {
        if numeric(field.name()) {
            let bounded = value.clamp(i64::MIN.into(), i64::MAX.into()) as i64;
            if i128::from(bounded) != value {
                self.truncated = true;
            }
            self.record_i64(field, bounded);
        } else {
            self.insert(field.name(), REDACTED.into());
        }
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        self.insert(
            field.name(),
            if numeric(field.name()) {
                value.into()
            } else {
                REDACTED.into()
            },
        );
    }

    fn record_i64(&mut self, field: &Field, value: i64) {
        self.insert(
            field.name(),
            if numeric(field.name()) {
                value.into()
            } else {
                REDACTED.into()
            },
        );
    }

    fn record_bool(&mut self, field: &Field, value: bool) {
        self.insert(
            field.name(),
            if matches!(field.name(), "active" | "completed") {
                value.into()
            } else {
                REDACTED.into()
            },
        );
    }

    fn record_str(&mut self, field: &Field, _: &str) {
        self.insert(field.name(), REDACTED.into());
    }

    fn record_debug(&mut self, field: &Field, _: &dyn fmt::Debug) {
        // In particular, never render fmt::Arguments in the message field.
        self.insert(field.name(), REDACTED.into());
    }

    fn record_error(&mut self, field: &Field, value: &(dyn Error + 'static)) {
        let mut chain = Vec::with_capacity(MAX_CHAIN);
        let mut current = Some(value);
        while let Some(error) = current {
            if chain.len() == MAX_CHAIN {
                break;
            }
            let depth = chain.len();
            let source = error.source();
            let mut node = json!({
                "depth": depth,
                "source": if source.is_some() && depth + 1 < MAX_CHAIN { Some(depth + 1) } else { None },
                "category": if error.is::<std::io::Error>() { "io" } else { "error" },
                "message": REDACTED,
            });
            if let Some(error) = error.downcast_ref::<std::io::Error>() {
                node["kind"] = io_kind(error.kind()).into();
            }
            chain.push(node);
            current = source;
        }
        self.insert(
            field.name(),
            json!({"chain": chain, "truncated": current.is_some(), "messages_redacted": true}),
        );
    }
}

fn io_kind(kind: std::io::ErrorKind) -> &'static str {
    use std::io::ErrorKind;
    match kind {
        ErrorKind::NotFound => "not_found",
        ErrorKind::PermissionDenied => "permission_denied",
        ErrorKind::ConnectionRefused => "connection_refused",
        ErrorKind::ConnectionReset => "connection_reset",
        ErrorKind::ConnectionAborted => "connection_aborted",
        ErrorKind::NotConnected => "not_connected",
        ErrorKind::AddrInUse => "address_in_use",
        ErrorKind::AddrNotAvailable => "address_unavailable",
        ErrorKind::BrokenPipe => "broken_pipe",
        ErrorKind::AlreadyExists => "already_exists",
        ErrorKind::WouldBlock => "would_block",
        ErrorKind::InvalidInput => "invalid_input",
        ErrorKind::InvalidData => "invalid_data",
        ErrorKind::TimedOut => "timed_out",
        ErrorKind::WriteZero => "write_zero",
        ErrorKind::Interrupted => "interrupted",
        ErrorKind::UnexpectedEof => "unexpected_eof",
        ErrorKind::OutOfMemory => "out_of_memory",
        _ => "other",
    }
}

#[derive(Default)]
pub(crate) struct SafeFields;

impl<'writer> FormatFields<'writer> for SafeFields {
    fn format_fields<R: RecordFields>(
        &self,
        mut writer: Writer<'writer>,
        fields: R,
    ) -> fmt::Result {
        let mut safe = Fields::default();
        fields.record(&mut safe);
        write!(writer, "{}", Value::Object(safe.finish()))
    }

    fn add_fields(
        &self,
        current: &'writer mut FormattedFields<Self>,
        fields: &span::Record<'_>,
    ) -> fmt::Result {
        let mut safe = Fields {
            values: serde_json::from_str(&current.fields).map_err(|_| fmt::Error)?,
            truncated: false,
        };
        fields.record(&mut safe);
        current.fields = Value::Object(safe.finish()).to_string();
        Ok(())
    }
}

pub(crate) struct SafeEvent {
    pub json: bool,
    pub ansi: bool,
}

impl<S> FormatEvent<S, SafeFields> for SafeEvent
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn format_event(
        &self,
        ctx: &FmtContext<'_, S, SafeFields>,
        mut writer: Writer<'_>,
        event: &Event<'_>,
    ) -> fmt::Result {
        let mut safe = Fields::default();
        event.record(&mut safe);
        let mut fields = safe.finish();
        let contract = fields
            .get("event_code")
            .and_then(Value::as_u64)
            .and_then(rpc::observability::contract);
        fields.insert(
            "message".into(),
            contract
                .as_ref()
                .map_or("unclassified event (text redacted)", |c| c.message)
                .into(),
        );
        if let Some(contract) = contract {
            fields.insert(
                "contract_version".into(),
                rpc::observability::CONTRACT_VERSION.into(),
            );
            for (key, value) in [
                ("category", contract.category),
                ("operation", contract.operation),
                ("outcome", contract.outcome),
                ("reason", contract.reason),
            ] {
                fields.insert(key.into(), value.into());
            }
        }
        // Scope honors an explicit event parent, including parent: None.
        let span = ctx
            .event_scope()
            .and_then(|mut scope| scope.next())
            .map(|span| {
                let extensions = span.extensions();
                let mut values: Map<String, Value> = extensions
                    .get::<FormattedFields<SafeFields>>()
                    .and_then(|fields| serde_json::from_str(&fields.fields).ok())
                    .unwrap_or_default();
                values.insert("name".into(), bounded_metadata(span.name()).into());
                Value::Object(values)
            });
        let timestamp = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Micros, true);
        let level = event.metadata().level().as_str();
        let target = bounded_metadata(event.metadata().target());
        let mut record = json!({"timestamp": timestamp, "level": level, "target": target,
            "line": event.metadata().line(), "fields": fields});
        if let Some(scope) = ctx.event_scope() {
            if let Some(span) = scope.into_iter().next() {
                if let Some(context) = span.extensions().get::<crate::telemetry::TraceContext>() {
                    record["trace_id"] = context.trace_id.clone().into();
                    record["span_id"] = context.span_id.clone().into();
                }
            }
        }
        if let Some(span) = span {
            record["span"] = span;
        }
        // No asynchronous queue: output is synchronous. Oversize records become
        // a bounded marker; never emit a partial JSON object or secret fallback.
        let mut output = if self.json {
            record.to_string()
        } else {
            compact(&record, self.ansi)
        };
        if output.len() > MAX_RECORD_BYTES - 1 {
            record["fields"] =
                json!({"message": "event exceeded output limit", "record_truncated": true});
            record.as_object_mut().unwrap().remove("span");
            output = if self.json {
                record.to_string()
            } else {
                compact(&record, self.ansi)
            };
        }
        writeln!(writer, "{output}")
    }
}

pub(crate) fn bounded_metadata(value: &str) -> String {
    value
        .chars()
        .filter(|c| !c.is_control())
        .take(128)
        .collect()
}

fn compact(record: &Value, ansi: bool) -> String {
    use fmt::Write;
    let mut text = format!("{} ", record["timestamp"].as_str().unwrap());
    if ansi {
        text.push_str("\x1b[1m");
    }
    text.push_str(record["level"].as_str().unwrap());
    if ansi {
        text.push_str("\x1b[0m");
    }
    let _ = write!(
        text,
        " {}:{}: ",
        record["target"].as_str().unwrap(),
        record["line"]
    );
    if let Some(span) = record.get("span") {
        let _ = write!(text, "{}{{", span["name"].as_str().unwrap());
        for (name, value) in span.as_object().unwrap() {
            if name != "name" {
                let _ = write!(text, "{name}={value} ");
            }
        }
        text.push_str("} ");
    }
    text.push_str(record["fields"]["message"].as_str().unwrap());
    for (name, value) in record["fields"].as_object().unwrap() {
        if name != "message" {
            let _ = write!(text, " {name}={value}");
        }
    }
    text
}
