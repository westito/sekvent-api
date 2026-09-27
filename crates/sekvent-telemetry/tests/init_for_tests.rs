//! `try_init_for_tests` installs once and makes later `init` calls no-ops.

use sekvent_telemetry::{TelemetryOptions, init, try_init_for_tests};

#[test]
fn test_helper_installs_once() {
    assert!(try_init_for_tests());
    assert!(!try_init_for_tests());
    let guard =
        init("billing", TelemetryOptions::new()).expect("already installed is not an error");
    assert!(!guard.installed());
    tracing::info!("shown only when this test fails");
}
