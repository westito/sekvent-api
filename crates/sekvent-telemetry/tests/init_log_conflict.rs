//! An existing `log` logger only disables the bridge; tracing still works.

use sekvent_telemetry::{LogBuffer, TelemetryOptions, init};

struct Silent;

impl log::Log for Silent {
    fn enabled(&self, _metadata: &log::Metadata<'_>) -> bool {
        false
    }

    fn log(&self, _record: &log::Record<'_>) {}

    fn flush(&self) {}
}

static SILENT: Silent = Silent;

#[test]
fn an_existing_log_logger_disables_only_the_bridge() {
    log::set_logger(&SILENT).expect("no logger was installed yet");
    let buffer = LogBuffer::new(4);
    let guard = init(
        "billing",
        TelemetryOptions::new().log_buffer(buffer.clone()),
    )
    .expect("the subscriber still installs");
    assert!(guard.installed());
    assert!(!guard.log_bridge());
    assert!(
        buffer
            .snapshot()
            .iter()
            .any(|record| record.message.contains("not captured")),
        "{:?}",
        buffer.snapshot()
    );
}
