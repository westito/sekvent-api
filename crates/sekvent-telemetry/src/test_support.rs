//! Deterministic event capture for this crate's unit tests.
//!
//! tracing caches each callsite's interest process-wide. A test that hits a
//! callsite with no subscriber installed can cache "never" while another
//! thread installs its capturing subscriber, and that thread then sees no
//! events. A global default that answers "sometimes" for every callsite keeps
//! the decision with each thread's own subscriber.

use std::sync::Once;

use tracing::span::{Attributes, Id, Record};
use tracing::subscriber::{Interest, Subscriber};
use tracing::{Event, Metadata};

struct Undecided;

impl Subscriber for Undecided {
    fn register_callsite(&self, _metadata: &'static Metadata<'static>) -> Interest {
        Interest::sometimes()
    }

    fn enabled(&self, _metadata: &Metadata<'_>) -> bool {
        false
    }

    fn new_span(&self, _span: &Attributes<'_>) -> Id {
        Id::from_u64(1)
    }

    fn record(&self, _span: &Id, _values: &Record<'_>) {}

    fn record_follows_from(&self, _span: &Id, _follows: &Id) {}

    fn event(&self, _event: &Event<'_>) {}

    fn enter(&self, _span: &Id) {}

    fn exit(&self, _span: &Id) {}
}

/// Installs [`Undecided`] as the global default once per test binary.
pub(crate) fn keep_interest_open() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        tracing::subscriber::set_global_default(Undecided)
            .expect("no unit test installs a global subscriber");
    });
}
