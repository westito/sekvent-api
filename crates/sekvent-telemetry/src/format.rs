//! Output formats. Every one of them carries the service name.

use std::fmt;
use std::sync::Arc;

use serde_json::{Map, Value};
use tracing::{Event, Subscriber};
use tracing_subscriber::fmt::format::Writer;
use tracing_subscriber::fmt::time::{FormatTime, SystemTime};
use tracing_subscriber::fmt::{FmtContext, FormatEvent, FormatFields, FormattedFields};
use tracing_subscriber::registry::LookupSpan;

use crate::fields::FieldVisitor;

/// How log lines are written.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum LogFormat {
    /// One human-readable line per event.
    #[default]
    Compact,
    /// One JSON object per line, for log shippers.
    Json,
    /// Multi-line human-readable output, for local development.
    Pretty,
}

impl LogFormat {
    /// Parse a format name (`compact`, `json`, `pretty`), ignoring case and
    /// surrounding whitespace.
    pub fn from_name(name: &str) -> Option<Self> {
        match name.trim().to_ascii_lowercase().as_str() {
            "compact" => Some(Self::Compact),
            "json" => Some(Self::Json),
            "pretty" => Some(Self::Pretty),
            _ => None,
        }
    }

    /// The canonical name.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Compact => "compact",
            Self::Json => "json",
            Self::Pretty => "pretty",
        }
    }
}

impl fmt::Display for LogFormat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Timer for the text formats: the timestamp followed by `service=<name>`,
/// so the service appears on every line right after the time.
#[derive(Debug, Clone)]
pub(crate) struct ServiceTimer {
    service: Arc<str>,
}

impl ServiceTimer {
    pub(crate) fn new(service: Arc<str>) -> Self {
        Self { service }
    }
}

impl FormatTime for ServiceTimer {
    fn format_time(&self, w: &mut Writer<'_>) -> fmt::Result {
        SystemTime.format_time(w)?;
        write!(w, " service={}", self.service)
    }
}

/// One JSON object per event:
/// `{"timestamp","level","service","target","message","fields","spans"}`.
///
/// Span fields are read from the JSON the layer's `JsonFields` formatter
/// stored on each span.
#[derive(Debug, Clone)]
pub(crate) struct JsonFormat {
    service: Arc<str>,
}

impl JsonFormat {
    pub(crate) fn new(service: Arc<str>) -> Self {
        Self { service }
    }
}

impl<S, N> FormatEvent<S, N> for JsonFormat
where
    S: Subscriber + for<'a> LookupSpan<'a>,
    N: for<'a> FormatFields<'a> + 'static,
{
    fn format_event(
        &self,
        ctx: &FmtContext<'_, S, N>,
        mut writer: Writer<'_>,
        event: &Event<'_>,
    ) -> fmt::Result {
        let metadata = event.metadata();
        let mut visitor = FieldVisitor::default();
        event.record(&mut visitor);

        let mut timestamp = String::new();
        SystemTime.format_time(&mut Writer::new(&mut timestamp))?;

        let mut object = Map::new();
        object.insert("timestamp".into(), Value::String(timestamp));
        object.insert("level".into(), Value::from(metadata.level().as_str()));
        object.insert("service".into(), Value::from(&*self.service));
        let target = visitor
            .log_target
            .take()
            .unwrap_or_else(|| metadata.target().to_owned());
        object.insert("target".into(), Value::String(target));
        if let Some(message) = visitor.message.take() {
            object.insert("message".into(), Value::String(message));
        }
        if !visitor.fields.is_empty() {
            let fields = std::mem::take(&mut visitor.fields);
            object.insert("fields".into(), Value::Object(fields.into_iter().collect()));
        }
        if let Some(scope) = ctx.event_scope() {
            let spans: Vec<Value> = scope
                .from_root()
                .map(|span| {
                    let mut entry = Map::new();
                    entry.insert("name".into(), Value::from(span.name()));
                    if let Some(fields) = span.extensions().get::<FormattedFields<N>>() {
                        merge_span_fields(&mut entry, &fields.fields);
                    }
                    Value::Object(entry)
                })
                .collect();
            if !spans.is_empty() {
                object.insert("spans".into(), Value::Array(spans));
            }
        }

        let line = serde_json::to_string(&Value::Object(object)).map_err(|_| fmt::Error)?;
        writeln!(writer, "{line}")
    }
}

/// Merge a span's formatted fields into its JSON entry: a JSON object is
/// merged key by key, any other non-empty text is kept under `fields`.
fn merge_span_fields(entry: &mut Map<String, Value>, formatted: &str) {
    if formatted.is_empty() {
        return;
    }
    if let Ok(Value::Object(fields)) = serde_json::from_str::<Value>(formatted) {
        entry.extend(fields);
    } else {
        entry.insert("fields".into(), Value::from(formatted));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_names() {
        for format in [LogFormat::Compact, LogFormat::Json, LogFormat::Pretty] {
            assert_eq!(LogFormat::from_name(format.as_str()), Some(format));
            assert_eq!(format.to_string(), format.as_str());
        }
        assert_eq!(LogFormat::from_name(" JSON "), Some(LogFormat::Json));
        assert_eq!(LogFormat::from_name("xml"), None);
        assert_eq!(LogFormat::default(), LogFormat::Compact);
    }

    #[test]
    fn timer_appends_the_service() {
        let mut text = String::new();
        ServiceTimer::new(Arc::from("billing"))
            .format_time(&mut Writer::new(&mut text))
            .expect("writing to a string cannot fail");
        assert!(text.ends_with(" service=billing"), "{text}");
        assert!(text.len() > " service=billing".len(), "{text}");
    }

    #[test]
    fn span_field_merging() {
        let mut entry = Map::new();
        merge_span_fields(&mut entry, "");
        assert!(entry.is_empty());
        merge_span_fields(&mut entry, r#"{"id":1}"#);
        assert_eq!(entry["id"], 1);
        merge_span_fields(&mut entry, "id=1");
        assert_eq!(entry["fields"], "id=1");
    }
}
