//! `init` installs the global subscriber once and bridges the `log` crate.
//!
//! One test per binary: the subscriber is process-wide.

use sekvent_telemetry::{LogBuffer, LogFormat, TelemetryOptions, init, try_init_for_tests};

#[test]
fn init_is_idempotent_and_bridges_log_records() {
    let buffer = LogBuffer::new(16);
    let guard = init(
        "billing",
        TelemetryOptions::new()
            .format(LogFormat::Json)
            .log_buffer(buffer.clone()),
    )
    .expect("first init succeeds");
    assert!(guard.installed());
    assert!(guard.log_bridge());

    let again = init(
        "orders",
        TelemetryOptions::new().default_directive("never=parsed=here"),
    )
    .expect("a repeat call is a no-op");
    assert!(!again.installed());
    assert!(again.log_bridge());
    assert!(!try_init_for_tests());

    tracing::error!(target: "billing::api", order = 7u64, "tracing event");
    log::error!(target: "billing::legacy", "log record {}", 1);

    let records = buffer.snapshot();
    let from_tracing = records
        .iter()
        .find(|record| record.message == "tracing event")
        .expect("the tracing event is buffered");
    assert_eq!(from_tracing.target, "billing::api");
    assert_eq!(from_tracing.fields["order"], "7");

    let from_log = records
        .iter()
        .find(|record| record.message == "log record 1")
        .expect("the log record is buffered");
    assert_eq!(from_log.target, "billing::legacy");
    assert_eq!(from_log.level, tracing::Level::ERROR);
    assert!(from_log.fields.is_empty(), "{:?}", from_log.fields);
}
