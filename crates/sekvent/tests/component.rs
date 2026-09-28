//! The component names resolve through the facade: the prelude, the module
//! and the crate-root re-exports.

#![cfg(feature = "component")]

use std::time::Duration;

use sekvent::component::{AppBuilder, Binding, BuildError, ComponentState, reasons};
use sekvent::config::MapSource;
use sekvent::prelude::*;

struct Quiet;

impl Lifecycle for Quiet {}

fn empty(source: &MapSource) -> Result<App, BuildError> {
    let builder: AppBuilder<'_> = App::builder(source);
    builder.build()
}

#[test]
fn the_error_trait_comes_from_the_prelude() {
    let error = AppError::not_found("no such order").with_reason("ORDER_NOT_FOUND");
    let back = <AppError as ComponentError>::from_app_error(error.into_app_error());
    assert_eq!(back.code(), ErrorCode::NotFound);
    assert_eq!(back.reason(), Some("ORDER_NOT_FOUND"));
}

#[test]
fn the_module_exports_bindings_and_reasons() {
    assert_eq!(
        Binding::parse("local-serialized"),
        Some(Binding::LocalSerialized)
    );
    assert_eq!(reasons::NOT_STARTED, "COMPONENT_NOT_STARTED");
    assert_eq!(
        sekvent::component::DEFAULT_BINDING_KEY,
        "SEKVENT_COMPONENT_BINDING"
    );
    let source = MapSource::new();
    let app = sekvent::App::builder(&source).build().unwrap();
    assert!(app.components().is_empty());
}

#[test]
fn an_unknown_component_key_fails_the_build() {
    let source = MapSource::new().with("SEKVENT_COMPONENT_BINDNIG", "local");
    let error = empty(&source).unwrap_err();
    assert!(
        error.to_string().contains("SEKVENT_COMPONENT_BINDNIG"),
        "{error}"
    );
}

#[tokio::test]
async fn an_empty_app_starts_and_stops() {
    Quiet.on_start().await.unwrap();
    Quiet.on_stop().await.unwrap();

    let app = empty(&MapSource::new()).unwrap();
    app.start().await.unwrap();
    assert_eq!(app.state("anything"), None::<ComponentState>);
    app.stop(Duration::ZERO).await.unwrap();
}

#[cfg(feature = "runtime")]
#[tokio::test]
async fn an_app_registers_with_the_runtime() {
    let app = empty(&MapSource::new()).unwrap();
    let handle = app
        .register(Runtime::builder().without_signals())
        .build()
        .unwrap()
        .start()
        .await
        .unwrap();
    assert!(handle.health().is_ready());
    handle.shutdown();
    let report = handle.wait().await.unwrap();
    assert!(report.unit("components").is_some());
}

#[test]
fn the_module_exports_the_c2_keys() {
    assert_eq!(
        sekvent::component::MAX_HOPS_KEY,
        "SEKVENT_COMPONENT_MAX_HOPS"
    );
    assert_eq!(sekvent::component::DEFAULT_MAX_HOPS, 16);
    assert_eq!(sekvent::component::POLICY_PREFIX, "SEKVENT_POLICY_");
    assert!(empty(&MapSource::new()).unwrap().grpc_services().is_empty());
}

#[cfg(feature = "component-grpc")]
#[tokio::test]
async fn grpc_routes_are_available_with_component_grpc() {
    let app = empty(&MapSource::new()).unwrap();
    let _routes = app.grpc_routes();
    app.start().await.unwrap();
    app.stop(Duration::ZERO).await.unwrap();
}
