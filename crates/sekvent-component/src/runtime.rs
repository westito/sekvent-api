//! Running an App under `sekvent-runtime`.

use std::time::Duration;

use sekvent_runtime::{
    HealthRegistry, RuntimeBuilder, ServiceStatus, Stage, UnitContext, UnitPolicy,
};
use tokio::time::Instant;

use crate::App;

/// Name of the runtime unit that runs the App.
const UNIT: &str = "components";

impl App {
    /// Add one critical unit, `components`, in [`Stage::Components`] that
    /// starts the App (every component, in install order), reports ready,
    /// and stops it (in reverse order) when its stage drains.
    ///
    /// One unit rather than one per component, because units of one stage
    /// start concurrently and would lose the install order. Ingress stops
    /// before the components and infrastructure after them.
    ///
    /// A shutdown that arrives while the App is starting abandons the start
    /// (see [`App::start`]) instead of waiting for it.
    ///
    /// The runtime aborts the unit at its
    /// [`stop_deadline`](UnitContext::stop_deadline): the end of the stage
    /// grace or of the overall shutdown deadline, whichever comes first. The
    /// App drains in-flight calls for half of the time left until then, so
    /// the `on_stop` hooks keep the other half; a hook that runs past the
    /// deadline still finishes on its own task, which the process may not
    /// outlive.
    ///
    /// Every service of [`App::grpc_services`] reports `NotServing` in the
    /// runtime's health until the App has started, `Serving` while it runs,
    /// and `NotServing` again before it stops.
    #[must_use]
    pub fn register(&self, runtime: RuntimeBuilder) -> RuntimeBuilder {
        let app = self.clone();
        runtime.unit(
            UNIT,
            Stage::Components,
            UnitPolicy::Critical,
            move |ctx: UnitContext| {
                let app = app.clone();
                async move {
                    let services = app.grpc_services();
                    let health = ctx.health();
                    let shutdown = ctx.shutdown();
                    set_all(health, &services, ServiceStatus::NotServing).await;
                    let started = tokio::select! {
                        biased;
                        () = shutdown.cancelled() => None,
                        outcome = app.start() => Some(outcome),
                    };
                    if let Some(outcome) = started {
                        outcome?;
                        set_all(health, &services, ServiceStatus::Serving).await;
                        ctx.ready();
                        shutdown.cancelled().await;
                        set_all(health, &services, ServiceStatus::NotServing).await;
                    }
                    app.stop(drain_grace(&ctx)).await
                }
            },
        )
    }
}

/// Half of the time left before the runtime aborts the unit (the stage
/// grace when no deadline is known), so the stop hooks keep the rest.
fn drain_grace(ctx: &UnitContext) -> Duration {
    let left = ctx.stop_deadline().map_or(ctx.stage_grace(), |deadline| {
        deadline.saturating_duration_since(Instant::now())
    });
    left / 2
}

async fn set_all(health: &HealthRegistry, services: &[String], status: ServiceStatus) {
    for service in services {
        health.set_status(service, status).await;
    }
}
