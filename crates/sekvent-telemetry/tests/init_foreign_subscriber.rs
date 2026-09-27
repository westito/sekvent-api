//! A global subscriber installed by someone else is reported, not replaced.

use sekvent_telemetry::{TelemetryError, TelemetryOptions, init, try_init_for_tests};
use tracing::subscriber::NoSubscriber;

#[test]
fn a_foreign_subscriber_is_an_error() {
    tracing::subscriber::set_global_default(NoSubscriber::default())
        .expect("no subscriber was installed yet");
    assert_eq!(
        init("billing", TelemetryOptions::new()),
        Err(TelemetryError::SubscriberAlreadySet)
    );
    assert!(!try_init_for_tests());
}
