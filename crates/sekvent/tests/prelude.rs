//! The prelude and the default modules resolve through the facade.

#![cfg(all(
    feature = "config",
    feature = "error",
    feature = "context",
    feature = "runtime"
))]

use std::time::Duration;

use sekvent::config::MapSource;
use sekvent::prelude::*;

#[derive(Debug, EnvConfig)]
struct Settings {
    #[config(default = "5s")]
    poll_interval: Duration,
    api_token: Secret,
}

#[test]
fn a_config_struct_derives_through_the_prelude() {
    let source = MapSource::new().with("API_TOKEN", "t0ken");
    let settings = Settings::from_config(&source).unwrap();
    assert_eq!(settings.poll_interval, Duration::from_secs(5));
    assert_eq!(settings.api_token.expose(), "t0ken");
    assert!(!format!("{settings:?}").contains("t0ken"));
}

#[test]
fn errors_and_context_come_from_the_prelude() {
    let error = AppError::not_found("no such order");
    assert_eq!(error.code(), ErrorCode::NotFound);
    assert!(!CallContext::new().request_id().is_empty());
}

#[tokio::test]
async fn a_runtime_starts_and_stops_through_the_prelude() {
    let handle = Runtime::builder()
        .without_signals()
        .unit(
            "idle",
            Stage::Workers,
            UnitPolicy::Critical,
            |ctx: UnitContext| async move {
                ctx.ready();
                ctx.shutdown().cancelled().await;
                Ok(())
            },
        )
        .build()
        .unwrap()
        .start()
        .await
        .unwrap();
    assert!(handle.health().is_ready());
    handle.shutdown();
    handle.wait().await.unwrap();
}
