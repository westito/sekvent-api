//! Deterministic event capture for this crate's unit tests.
//!
//! tracing caches each callsite's interest process-wide. A test that hits a
//! callsite with no subscriber installed can cache "never" while another
//! thread installs its capturing subscriber, and that thread then sees no
//! events. A global default that answers "sometimes" for every callsite keeps
//! the decision with each thread's own subscriber.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::{Arc, Mutex, Once};

use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id, Record};
use tracing::subscriber::{Interest, Subscriber};
use tracing::{Event, Level, Metadata};

/// One captured event.
#[derive(Debug)]
pub(crate) struct Captured {
    pub(crate) target: String,
    pub(crate) level: Level,
    pub(crate) fields: BTreeMap<String, String>,
}

/// The value of `name` in `event`, if recorded.
pub(crate) fn field<'a>(event: &'a Captured, name: &str) -> Option<&'a str> {
    event.fields.get(name).map(String::as_str)
}

/// Runs `f` with a capturing subscriber on this thread and returns the
/// events it emitted with target `sekvent::error`.
pub(crate) fn capture(f: impl FnOnce()) -> Vec<Captured> {
    keep_interest_open();
    let events = Arc::new(Mutex::new(Vec::new()));
    tracing::subscriber::with_default(
        Recorder {
            events: Some(Arc::clone(&events)),
        },
        f,
    );
    let events = std::mem::take(&mut *events.lock().expect("capture lock"));
    events
        .into_iter()
        .filter(|event| event.target == "sekvent::error")
        .collect()
}

/// Records events when `events` is set; the global default (no `events`)
/// records nothing but keeps every callsite's interest open.
struct Recorder {
    events: Option<Arc<Mutex<Vec<Captured>>>>,
}

impl Subscriber for Recorder {
    fn register_callsite(&self, _metadata: &'static Metadata<'static>) -> Interest {
        Interest::sometimes()
    }

    fn enabled(&self, _metadata: &Metadata<'_>) -> bool {
        self.events.is_some()
    }

    fn new_span(&self, _span: &Attributes<'_>) -> Id {
        Id::from_u64(1)
    }

    fn record(&self, _span: &Id, _values: &Record<'_>) {}

    fn record_follows_from(&self, _span: &Id, _follows: &Id) {}

    fn event(&self, event: &Event<'_>) {
        let mut fields = Fields::default();
        event.record(&mut fields);
        if let Some(events) = &self.events {
            events.lock().expect("capture lock").push(Captured {
                target: event.metadata().target().to_owned(),
                level: *event.metadata().level(),
                fields: fields.0,
            });
        }
    }

    fn enter(&self, _span: &Id) {}

    fn exit(&self, _span: &Id) {}
}

#[derive(Default)]
struct Fields(BTreeMap<String, String>);

impl Visit for Fields {
    fn record_str(&mut self, field: &Field, value: &str) {
        self.0.insert(field.name().to_owned(), value.to_owned());
    }

    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        self.0.insert(field.name().to_owned(), format!("{value:?}"));
    }
}

/// Installs a non-recording [`Recorder`] as the global default once per test
/// binary.
fn keep_interest_open() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        tracing::subscriber::set_global_default(Recorder { events: None })
            .expect("no unit test installs a global subscriber");
    });
}
