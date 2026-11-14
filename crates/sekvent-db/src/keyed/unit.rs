//! The runtime unit of a [`Keyed`] cache: periodic idle eviction, and the
//! cache's `close` when its stage drains.

use std::hash::Hash;

use futures::future::BoxFuture;
use sekvent_error::AppError;
use sekvent_runtime::UnitContext;
use tokio::time::Instant;

use super::Keyed;

impl<K, V> Keyed<K, V>
where
    K: Clone + Eq + Hash + Send + Sync + 'static,
    V: Send + Sync + 'static,
{
    /// The runtime unit of this cache.
    ///
    /// It reports ready at once and evicts the entries idle for
    /// `evict_idle_after` every `reap_interval`, like
    /// [`evict_idle`](Self::evict_idle) but without waiting for their
    /// disposals: those run on tracked tasks of their own, so one slow
    /// disposal (a pool waiting for its connections) never stalls the
    /// sweeps after it. When its stage drains it calls
    /// [`close`](Self::close) under nine tenths of the time left until the
    /// unit's stop deadline (`stage_grace` from now when the runtime set
    /// none). It logs one `warn` when values are still referenced after the
    /// close, or when the close did not end in time, and never fails.
    ///
    /// Register it in `Stage::Infrastructure` with `UnitPolicy::Critical`:
    /// that stage drains last, after the calls holding references. A closed
    /// cache stays closed, so the unit is not meant to restart.
    pub fn into_unit(
        self,
    ) -> impl FnMut(UnitContext) -> BoxFuture<'static, Result<(), AppError>> + Send + 'static {
        move |ctx: UnitContext| -> BoxFuture<'static, Result<(), AppError>> {
            Box::pin(self.clone().run_unit(ctx))
        }
    }

    async fn run_unit(self, ctx: UnitContext) -> Result<(), AppError> {
        ctx.ready();
        let shutdown = ctx.shutdown();
        let interval = self.inner.settings.reap_interval;
        loop {
            tokio::select! {
                biased;
                () = shutdown.cancelled() => break,
                () = tokio::time::sleep(interval) => {
                    self.evict_idle_in_background();
                }
            }
        }

        let now = Instant::now();
        let deadline = ctx
            .stop_deadline()
            .unwrap_or_else(|| now + ctx.stage_grace());
        let bound = deadline.saturating_duration_since(now) / 10 * 9;
        if tokio::time::timeout(bound, self.close()).await.is_ok() {
            let retiring = self.inner.tracker.retiring();
            if retiring > 0 {
                tracing::warn!(
                    retiring,
                    "resource cache closed with values still referenced"
                );
            }
        } else {
            tracing::warn!(
                creating = self.inner.tracker.creating(),
                retiring = self.inner.tracker.retiring(),
                "resource cache did not close in time; abandoning the wait"
            );
        }
        Ok(())
    }
}
