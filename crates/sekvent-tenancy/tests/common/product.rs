//! The test product: a tenant schema of two migrations, and a scope whose
//! build a test can hold at a gate, fail and count.
//!
//! The signals are `tokio::sync::watch` channels, so a test waits for a
//! count to be reached and never polls or sleeps.

use std::sync::{Arc, Mutex, PoisonError};

use sekvent_error::AppError;
use sekvent_tenancy::{ScopeFactory, ScopeFuture, TenantId, TenantInfo};
use sqlx::PgPool;
use tokio::sync::watch;

/// The test product's tenant migrator: two migrations.
pub static TENANT_MIGRATOR: sqlx::migrate::Migrator =
    sqlx::migrate!("tests/common/migrations/tenant");

/// The version of its last migration.
pub const LATEST_TENANT_VERSION: i64 = 2;

/// What a factory and every scope it built share.
struct Signals {
    /// Whether builds may pass the gate.
    gate: watch::Sender<bool>,
    /// Builds that reached the gate.
    arrived: watch::Sender<usize>,
    /// Builds that ended with a scope.
    built: watch::Sender<usize>,
    /// Scopes dropped.
    dropped: watch::Sender<usize>,
    /// The error every build fails with, when set.
    fail: Mutex<Option<fn() -> AppError>>,
}

/// The test product's scope: the tenant's pool. Its `Drop` is counted and
/// signalled. It derives nothing, `Clone` in particular.
pub struct OrdersScope {
    /// The tenant's migrated pool.
    pub pool: PgPool,
    /// The tenant the scope was built for.
    pub tenant: TenantId,
    signals: Arc<Signals>,
}

impl Drop for OrdersScope {
    fn drop(&mut self) {
        self.signals.dropped.send_modify(|count| *count += 1);
    }
}

/// The test product's factory. Clones share the gate and the counters.
#[derive(Clone)]
pub struct OrdersFactory {
    signals: Arc<Signals>,
}

impl OrdersFactory {
    /// A factory with an open gate that builds every scope.
    pub fn new() -> Self {
        Self {
            signals: Arc::new(Signals {
                gate: watch::Sender::new(true),
                arrived: watch::Sender::new(0),
                built: watch::Sender::new(0),
                dropped: watch::Sender::new(0),
                fail: Mutex::new(None),
            }),
        }
    }

    /// Builds wait at the gate from now on.
    pub fn close_gate(&self) {
        self.signals.gate.send_replace(false);
    }

    /// Lets every waiting and later build through.
    pub fn open_gate(&self) {
        self.signals.gate.send_replace(true);
    }

    /// Resolves once `count` builds have reached the gate since the factory
    /// was made.
    pub async fn arrived(&self, count: usize) {
        reach(&self.signals.arrived, count).await;
    }

    /// Every build from now on fails with `error()`; `None` builds again.
    pub fn fail_with(&self, error: Option<fn() -> AppError>) {
        *self
            .signals
            .fail
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = error;
    }

    /// Builds finished with a scope.
    pub fn built(&self) -> usize {
        *self.signals.built.borrow()
    }

    /// Scopes dropped.
    pub fn dropped(&self) -> usize {
        *self.signals.dropped.borrow()
    }

    /// Resolves once `count` scopes have been dropped since the factory was
    /// made.
    pub async fn dropped_at_least(&self, count: usize) {
        reach(&self.signals.dropped, count).await;
    }
}

impl Default for OrdersFactory {
    fn default() -> Self {
        Self::new()
    }
}

impl ScopeFactory for OrdersFactory {
    type Scope = OrdersScope;

    fn build<'a>(&'a self, info: &'a TenantInfo, pool: PgPool) -> ScopeFuture<'a, OrdersScope> {
        Box::pin(async move {
            self.signals.arrived.send_modify(|count| *count += 1);
            let mut gate = self.signals.gate.subscribe();
            let opened = gate.wait_for(|open| *open).await.is_ok();
            assert!(opened, "the factory outlives its builds");
            let fail = *self
                .signals
                .fail
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            if let Some(error) = fail {
                return Err(error());
            }
            self.signals.built.send_modify(|count| *count += 1);
            Ok(OrdersScope {
                pool,
                tenant: info.id,
                signals: Arc::clone(&self.signals),
            })
        })
    }
}

/// Resolves once `counter` is at least `count`.
async fn reach(counter: &watch::Sender<usize>, count: usize) {
    let mut seen = counter.subscribe();
    let reached = seen.wait_for(|value| *value >= count).await.is_ok();
    assert!(reached, "the factory outlives its waiters");
}
