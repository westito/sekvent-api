//! Readiness probes per pool, without a database server.
//!
//! The pools point at `127.0.0.1:1`, where nobody listens, so every probe
//! is down; the tests run on the real clock with a 30 s guard.

#![cfg(all(feature = "runtime", feature = "sqlx-postgres", feature = "sqlx-mysql"))]

use std::time::Duration;

use sekvent_config::Secret;
use sekvent_db::{PoolProbe, PoolRegistry, PoolSpec};
use sekvent_error::AppError;
use sekvent_runtime::{
    DependencyProbe, ProbeFailure, ProbeStatus, Runtime, RuntimeBuilder, Stage, UnitContext,
    UnitPolicy,
};

const GUARD: Duration = Duration::from_secs(30);

fn down_spec(name: &str, scheme: &str) -> PoolSpec {
    PoolSpec::new(
        name,
        Secret::new(format!("{scheme}://u:hunter2@127.0.0.1:1/{name}")),
    )
    .lazy()
    .with_acquire_timeout(Duration::from_millis(200))
}

async fn until_shutdown(ctx: UnitContext) -> Result<(), AppError> {
    ctx.ready();
    ctx.shutdown().cancelled().await;
    Ok(())
}

#[tokio::test]
async fn probes_mirror_the_specs() {
    let registry = PoolRegistry::build(vec![
        down_spec("orders_db", "postgres"),
        down_spec("legacy_db", "mysql").optional(),
        PoolSpec::new("reports_db", Secret::new("")).optional(),
    ])
    .await
    .unwrap();

    let probes: Vec<(String, bool)> = registry
        .probes()
        .iter()
        .map(|probe| (probe.name().to_owned(), probe.required()))
        .collect();

    assert_eq!(
        probes,
        [
            ("legacy_db".to_owned(), false),
            ("orders_db".to_owned(), true)
        ],
        "sorted by name; the unconfigured optional pool has none"
    );
    registry.close().await;
}

#[tokio::test]
async fn an_unreachable_lazy_pool_probes_down() {
    let registry = PoolRegistry::build(vec![
        down_spec("orders_db", "postgres"),
        down_spec("legacy_db", "mysql"),
    ])
    .await
    .unwrap();

    for probe in registry.probes() {
        let status = tokio::time::timeout(GUARD, probe.probe()).await.unwrap();
        assert!(
            matches!(status, ProbeStatus::Down(ProbeFailure::Unreachable(_))),
            "{}: {status:?}",
            probe.name()
        );
        // Never the driver's text or the URL.
        let ProbeStatus::Down(failure) = status else {
            unreachable!()
        };
        assert!(!failure.detail().contains("127.0.0.1"));
    }
    registry.close().await;
}

#[tokio::test]
async fn a_closed_pool_probes_down_without_connecting() {
    let registry = PoolRegistry::build(vec![
        down_spec("orders_db", "postgres"),
        down_spec("legacy_db", "mysql"),
    ])
    .await
    .unwrap();
    let probes = registry.probes();
    registry.close().await;

    for probe in &probes {
        let status = tokio::time::timeout(GUARD, probe.probe()).await.unwrap();
        assert_eq!(
            status,
            ProbeStatus::Down(ProbeFailure::Unreachable("pool closed")),
            "{}",
            probe.name()
        );
    }
}

#[tokio::test]
async fn an_optional_pool_never_makes_the_service_unready() {
    let registry = PoolRegistry::build(vec![down_spec("legacy_db", "mysql").optional()])
        .await
        .unwrap();
    let builder = registry
        .probes()
        .into_iter()
        .fold(Runtime::builder().without_signals(), RuntimeBuilder::probe)
        .probe_interval(Duration::from_secs(60))
        .probe_timeout(Duration::from_secs(5))
        .unit("api", Stage::Ingress, UnitPolicy::Critical, until_shutdown);
    let health = builder.health();

    let handle = tokio::time::timeout(GUARD, builder.build().unwrap().start())
        .await
        .unwrap()
        .unwrap();

    let readiness = health.readiness();
    assert!(readiness.ready);
    assert_eq!(readiness.probes.len(), 1);
    assert!(!readiness.probes[0].required);
    assert!(matches!(
        readiness.probes[0].status,
        Some(ProbeStatus::Down(ProbeFailure::Unreachable(_)))
    ));
    handle.shutdown();
    tokio::time::timeout(GUARD, handle.wait())
        .await
        .unwrap()
        .unwrap();
    registry.close().await;
}

#[tokio::test]
async fn a_required_pool_that_is_down_makes_the_service_unready() {
    let registry = PoolRegistry::build(vec![down_spec("orders_db", "postgres")])
        .await
        .unwrap();
    let probe = PoolProbe::new("orders_db", registry.get("orders_db").unwrap().clone());
    let builder = Runtime::builder()
        .without_signals()
        .probe(probe)
        .probe_interval(Duration::from_secs(60))
        .probe_timeout(Duration::from_secs(5))
        .unit("api", Stage::Ingress, UnitPolicy::Critical, until_shutdown);
    let health = builder.health();

    let handle = tokio::time::timeout(GUARD, builder.build().unwrap().start())
        .await
        .unwrap()
        .unwrap();

    let readiness = health.readiness();
    assert!(!readiness.ready);
    assert!(readiness.probes[0].required);
    handle.shutdown();
    tokio::time::timeout(GUARD, handle.wait())
        .await
        .unwrap()
        .unwrap();
    registry.close().await;
}

#[test]
fn an_explicitly_optional_probe_is_not_required() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let pool = sqlx::PgPool::connect_lazy("postgres://u@127.0.0.1:1/orders").unwrap();
        let probe = PoolProbe::new("orders_db", sekvent_db::Pool::Postgres(pool.clone()));
        assert!(probe.required());
        let probe = probe.optional();
        assert!(!probe.required());
        assert_eq!(probe.name(), "orders_db");
        pool.close().await;
    });
}
