//! The tenant cache as a runtime unit.

use futures::future::BoxFuture;
use sekvent_error::AppError;
use sekvent_runtime::UnitContext;

use super::keyed::Tenants;

/// The one type both forms' units have.
type Unit = Box<dyn FnMut(UnitContext) -> BoxFuture<'static, Result<(), AppError>> + Send>;

impl<S: Send + Sync + 'static> Tenants<S> {
    /// The runtime unit of the cache: `KeyedPools::into_unit` for the keyed form; for the fixed
    /// form a unit that reports ready and returns `Ok(())` when its shutdown token fires.
    ///
    /// The keyed form's unit reports ready at once, evicts the scopes idle for
    /// `evict_idle_after` every `reap_interval`, and closes the cache when its stage drains.
    /// Register it in `Stage::Infrastructure` with `UnitPolicy::Critical`: that stage drains
    /// last, after the calls that hold leases. A closed cache stays closed.
    pub fn into_unit(
        self,
    ) -> impl FnMut(UnitContext) -> BoxFuture<'static, Result<(), AppError>> + Send + 'static {
        self.unit()
    }

    fn unit(&self) -> Unit {
        match self.cache() {
            Some(pools) => Box::new(pools.clone().into_unit()),
            None => Box::new(fixed_unit),
        }
    }
}

/// The fixed form's unit.
fn fixed_unit(ctx: UnitContext) -> BoxFuture<'static, Result<(), AppError>> {
    Box::pin(until_shutdown(ctx))
}

async fn until_shutdown(ctx: UnitContext) -> Result<(), AppError> {
    ctx.ready();
    ctx.shutdown().cancelled().await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use sekvent_runtime::{Runtime, Stage, UnitExit, UnitPolicy};

    use crate::Tenants;

    const UNIT: &str = "tenants";

    #[tokio::test]
    async fn the_fixed_unit_reports_ready_and_completes_on_shutdown() {
        let handle = Runtime::builder()
            .without_signals()
            .unit(
                UNIT,
                Stage::Infrastructure,
                UnitPolicy::Critical,
                Tenants::fixed(3_u32).into_unit(),
            )
            .build()
            .expect("a runtime")
            .start()
            .await
            .expect("the unit reported ready");
        handle.shutdown();
        let report = handle.wait().await.expect("a report");
        assert_eq!(
            report.unit(UNIT).expect("the unit's report").exit,
            UnitExit::Completed
        );
    }
}
