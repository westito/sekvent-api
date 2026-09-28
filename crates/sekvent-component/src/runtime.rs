//! Running an App under `sekvent-runtime`.

use sekvent_runtime::{RuntimeBuilder, Stage, UnitContext, UnitPolicy};

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
                    app.start().await?;
                    ctx.ready();
                    ctx.shutdown().cancelled().await;
                    app.stop(ctx.stage_grace()).await
                }
            },
        )
    }
}
