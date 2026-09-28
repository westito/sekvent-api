//! Running an App under `sekvent-runtime`.

use sekvent_runtime::{
    HealthRegistry, RuntimeBuilder, ServiceStatus, Stage, UnitContext, UnitPolicy,
};

use crate::App;

/// Name of the runtime unit that runs the App.
const UNIT: &str = "components";

impl App {
    /// Add one critical unit, `components`, in [`Stage::Components`] that
    /// starts the App (every component, in install order), reports ready,
    /// and stops it (in reverse order, within the runtime's stage grace)
    /// when its stage drains.
    ///
    /// One unit rather than one per component, because units of one stage
    /// start concurrently and would lose the install order. Ingress stops
    /// before the components and infrastructure after them.
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
                    set_all(health, &services, ServiceStatus::NotServing).await;
                    app.start().await?;
                    set_all(health, &services, ServiceStatus::Serving).await;
                    ctx.ready();
                    ctx.shutdown().cancelled().await;
                    set_all(health, &services, ServiceStatus::NotServing).await;
                    app.stop(ctx.stage_grace()).await
                }
            },
        )
    }
}

async fn set_all(health: &HealthRegistry, services: &[String], status: ServiceStatus) {
    for service in services {
        health.set_status(service, status).await;
    }
}
