//! Field capture shared by the JSON formatter and the log buffer.

use std::collections::BTreeMap;
use std::fmt;

use serde_json::Value;
use tracing::field::{Field, Visit};

/// Field the `log` bridge uses for the original record target.
const LOG_TARGET: &str = "log.target";
/// Other bridge bookkeeping fields, dropped from the output.
const LOG_META: [&str; 3] = ["log.module_path", "log.file", "log.line"];

/// Collects an event's or span's fields. Records bridged from the `log`
/// crate carry their real target in `log.target`; it is lifted out here so
/// it can replace the bridge's placeholder target.
#[derive(Debug, Default)]
pub(crate) struct FieldVisitor {
    pub(crate) message: Option<String>,
    pub(crate) log_target: Option<String>,
    pub(crate) fields: BTreeMap<String, Value>,
}

impl FieldVisitor {
    fn insert(&mut self, field: &Field, value: Value) {
        match field.name() {
            "message" => self.message = Some(into_text(value)),
            LOG_TARGET => self.log_target = Some(into_text(value)),
            name if LOG_META.contains(&name) => {}
            name => {
                self.fields.insert(name.to_owned(), value);
            }
        }
    }

    /// The captured fields rendered as text.
    pub(crate) fn into_text_fields(self) -> BTreeMap<String, String> {
        self.fields
            .into_iter()
            .map(|(name, value)| (name, into_text(value)))
            .collect()
    }
}

impl Visit for FieldVisitor {
    fn record_f64(&mut self, field: &Field, value: f64) {
        self.insert(field, Value::from(value));
    }

    fn record_i64(&mut self, field: &Field, value: i64) {
        self.insert(field, Value::from(value));
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        self.insert(field, Value::from(value));
    }

    fn record_bool(&mut self, field: &Field, value: bool) {
        self.insert(field, Value::from(value));
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        self.insert(field, Value::from(value));
    }

    fn record_error(&mut self, field: &Field, value: &(dyn std::error::Error + 'static)) {
        self.insert(field, Value::from(value.to_string()));
    }

    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        self.insert(field, Value::from(format!("{value:?}")));
    }
}

/// Strings stay as they are; every other JSON value uses its JSON text.
pub(crate) fn into_text(value: Value) -> String {
    match value {
        Value::String(text) => text,
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use tracing::subscriber::with_default;
    use tracing_subscriber::Registry;
    use tracing_subscriber::layer::{Context, Layer, SubscriberExt};

    use super::*;

    struct Capture(std::sync::Arc<std::sync::Mutex<Option<FieldVisitor>>>);

    impl<S: tracing::Subscriber> Layer<S> for Capture {
        fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
            let mut visitor = FieldVisitor::default();
            event.record(&mut visitor);
            *self.0.lock().expect("not poisoned") = Some(visitor);
        }
    }

    fn capture(emit: impl FnOnce()) -> FieldVisitor {
        let slot = std::sync::Arc::default();
        with_default(
            Registry::default().with(Capture(std::sync::Arc::clone(&slot))),
            emit,
        );
        let mut guard = slot.lock().expect("not poisoned");
        guard.take().expect("an event was recorded")
    }

    #[test]
    fn records_every_primitive() {
        let error = std::io::Error::other("disk full");
        let visitor = capture(|| {
            tracing::info!(
                ratio = 0.5,
                delta = -3i64,
                count = 7u64,
                ok = true,
                name = "orders",
                err = &error as &(dyn std::error::Error + 'static),
                shape = ?(1, 2),
                "processed {}",
                "batch"
            );
        });
        assert_eq!(visitor.message.as_deref(), Some("processed batch"));
        assert_eq!(visitor.log_target, None);
        let text = visitor.into_text_fields();
        assert_eq!(text["ratio"], "0.5");
        assert_eq!(text["delta"], "-3");
        assert_eq!(text["count"], "7");
        assert_eq!(text["ok"], "true");
        assert_eq!(text["name"], "orders");
        assert_eq!(text["err"], "disk full");
        assert_eq!(text["shape"], "(1, 2)");
    }

    #[test]
    fn lifts_the_log_bridge_target() {
        let visitor = capture(|| {
            tracing::event!(
                target: "log",
                tracing::Level::WARN,
                log.target = "orders::db",
                log.module_path = "orders::db",
                log.file = "src/db.rs",
                log.line = 12u64,
                "pool exhausted"
            );
        });
        assert_eq!(visitor.log_target.as_deref(), Some("orders::db"));
        assert!(visitor.fields.is_empty(), "{:?}", visitor.fields);
    }
}
