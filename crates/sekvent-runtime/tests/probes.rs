//! Dependency probes feeding readiness, on tokio's paused clock.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::future::BoxFuture;
use sekvent_error::AppError;
use sekvent_runtime::{
    DependencyProbe, PROBE_UNIT, ProbeFailure, ProbeStatus, Runtime, Stage, UnitContext, UnitExit,
    UnitPolicy,
};

/// A probe whose answer the test controls.
#[derive(Clone)]
struct Switch {
    status: Arc<Mutex<ProbeStatus>>,
}

impl Switch {
    fn new(status: ProbeStatus) -> Self {
        Self {
            status: Arc::new(Mutex::new(status)),
        }
    }
    fn set(&self, status: ProbeStatus) {
        *self.status.lock().unwrap() = status;
    }
}

impl DependencyProbe for Switch {
    fn name(&self) -> &'static str {
        "db"
    }
    fn probe(&self) -> BoxFuture<'_, ProbeStatus> {
        let status = *self.status.lock().unwrap();
        Box::pin(async move { status })
    }
}

/// An optional probe that never answers.
struct Hung;

impl DependencyProbe for Hung {
    fn name(&self) -> &'static str {
        "search"
    }
    fn required(&self) -> bool {
        false
    }
    fn probe(&self) -> BoxFuture<'_, ProbeStatus> {
        Box::pin(std::future::pending())
    }
}

async fn until_shutdown(ctx: UnitContext) -> Result<(), AppError> {
    ctx.ready();
    ctx.shutdown().cancelled().await;
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn readiness_follows_the_required_probe() {
    let db = Switch::new(ProbeStatus::Up);
    let builder = Runtime::builder()
        .without_signals()
        .probe(db.clone())
        .probe(Hung)
        .probe_interval(Duration::from_secs(10))
        .probe_timeout(Duration::from_secs(1))
        .unit("api", Stage::Ingress, UnitPolicy::Critical, until_shutdown);
    let health = builder.health();
    let handle = builder.build().unwrap().start().await.unwrap();

    // Startup waited for the first probe round.
    let readiness = health.readiness();
    assert!(readiness.ready);
    assert_eq!(readiness.probes.len(), 2);
    assert_eq!(readiness.probes[0].status, Some(ProbeStatus::Up));
    assert_eq!(
        readiness.probes[1].status,
        Some(ProbeStatus::Down(ProbeFailure::Unreachable(
            "probe timed out"
        )))
    );
    assert!(!readiness.probes[1].required);

    let mut ready = health.watch_ready();
    let rejected = ProbeFailure::Rejected("bad credentials");
    db.set(ProbeStatus::Down(rejected));
    ready.wait_for(|ready| !*ready).await.unwrap();
    assert_eq!(
        health.readiness().probes[0].status,
        Some(ProbeStatus::Down(rejected))
    );
    assert_eq!(rejected.kind(), "rejected");
    assert_eq!(rejected.detail(), "bad credentials");

    db.set(ProbeStatus::Up);
    ready.wait_for(|ready| *ready).await.unwrap();
    assert!(ProbeStatus::Up.is_up());

    handle.shutdown();
    let report = handle.wait().await.unwrap();
    let probes = report.unit(PROBE_UNIT).expect("the probe unit is reported");
    assert_eq!(probes.stage, Stage::Infrastructure);
    assert_eq!(probes.exit, UnitExit::Completed);
}
