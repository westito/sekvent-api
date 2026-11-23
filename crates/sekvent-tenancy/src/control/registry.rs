//! The registry: the control pool of this process with its lookup cache and its two admissions.

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use sekvent_context::Clock;
use sekvent_db::to_app_error;
use sekvent_error::AppError;
use sqlx::PgPool;

use super::admission::{LifecycleAdmission, LifecyclePermit};
use super::lookup::Lookups;
use super::rows::{self, TenantRow, VersionStored};
use crate::label::LabelRules;
use crate::{Selector, TenantId, TenantInfo};

/// The control database of this process: the pool, the lookup cache and the two admissions.
/// Clones share everything: one lookup cache with its reads in flight, one set of lookup permits and
/// one set of lifecycle permits, however many clones exist. Each value keeps its own label rules
/// (`labels`) and lifecycle wait (`with_admission_wait`), copied into the clones made after they
/// are set. `Debug` prints the type name only.
#[derive(Clone)]
pub struct Registry {
    pool: PgPool,
    lookups: Lookups,
    lifecycle: LifecycleAdmission,
}

impl Registry {
    /// The fewest connections a control pool may have.
    pub const MIN_CONNECTIONS: u32 = 3;

    /// `INVALID_ARGUMENT` when `control.options().get_max_connections()` is below
    /// `MIN_CONNECTIONS`. No I/O.
    pub fn new(control: PgPool, clock: Arc<dyn Clock>) -> Result<Self, AppError> {
        let max = control.options().get_max_connections();
        if max < Self::MIN_CONNECTIONS {
            return Err(AppError::invalid_argument(
                "the control pool needs at least 3 connections",
            ));
        }
        Ok(Self {
            pool: control,
            lookups: Lookups::new(max, clock),
            lifecycle: LifecycleAdmission::new(max),
        })
    }

    /// The product's label rules (default `LabelRules::new()`), the same as it gives
    /// `SelectorRules::labels`: `lookup` answers `None` for a `Selector::PathLabel` whose segment
    /// they refuse, on every listener host, before it takes a permit or a connection. The
    /// resolver checks a request's segment with `SelectorRules::labels` before its lookup; set
    /// these when `lookup` is also called with path labels from elsewhere.
    #[must_use]
    pub fn labels(mut self, rules: LabelRules) -> Self {
        self.lookups.set_labels(rules);
        self
    }

    /// The control pool.
    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// A request's lookup: through the cache, a miss under the lookup admission. `TENANT_BUSY`
    /// (no domain) without a permit, before any connection is taken. Returns the row in any state.
    /// A path label is checked with `labels` first; on a loopback listener host it selects the one
    /// row not `Purged` of that subdomain that has a canonical loopback host.
    pub async fn lookup(&self, selector: &Selector) -> Result<Option<TenantRow>, AppError> {
        self.lookups.find(&self.pool, selector).await
    }

    /// An internal read by id: through the cache, no permit, never joined with a request's read.
    pub async fn tenant(&self, id: TenantId) -> Result<Option<TenantRow>, AppError> {
        self.lookups.find_internal(&self.pool, id).await
    }

    /// The row as committed now: no cache, no permit, one statement.
    pub async fn tenant_now(&self, id: TenantId) -> Result<Option<TenantRow>, AppError> {
        let mut conn = self.pool.acquire().await.map_err(to_app_error)?;
        rows::get(&mut conn, id).await
    }

    /// `rows::list_active` on a connection held for that one statement.
    pub async fn active(&self) -> Result<Vec<TenantInfo>, AppError> {
        let mut conn = self.pool.acquire().await.map_err(to_app_error)?;
        rows::list_active(&mut conn).await
    }

    /// `rows::store_schema_version` on a connection held for that one statement.
    pub async fn store_schema_version(
        &self,
        id: TenantId,
        version: i64,
    ) -> Result<VersionStored, AppError> {
        let mut conn = self.pool.acquire().await.map_err(to_app_error)?;
        rows::store_schema_version(&mut conn, id, version).await
    }

    /// Drops the tenant's cached lookups and every not-found host entry; a read already running
    /// does not store its answer.
    pub fn forget(&self, tenant: TenantId) {
        self.lookups.forget(tenant);
    }

    /// Drops every cached lookup; a read already running does not store its answer.
    pub fn clear_lookups(&self) {
        self.lookups.clear();
    }

    /// How long [`admit`](Self::admit) waits for a lifecycle permit (default 10 s;
    /// `Duration::ZERO` takes a free permit and never waits). The permits stay shared with every
    /// clone; the wait is this value's, and the clones made after it.
    #[must_use]
    pub fn with_admission_wait(mut self, wait: Duration) -> Self {
        self.lifecycle = self.lifecycle.with_wait(wait);
        self
    }

    /// The lifecycle admission: waits up to the admission wait (10 s by default) for a permit,
    /// then `UNAVAILABLE` without a reason.
    pub async fn admit(&self) -> Result<LifecyclePermit, AppError> {
        self.lifecycle.admit().await
    }

    /// The lifecycle permits free now, shared by every clone: `max(1, (max_connections − 1) /
    /// 2)` of the control pool while no lifecycle call runs.
    pub fn lifecycle_available(&self) -> usize {
        self.lifecycle.available()
    }
}

// `Registry(..)`, one `write_str`
impl fmt::Debug for Registry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Registry(..)")
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, SystemTime};

    use sekvent_context::ManualClock;
    use sekvent_error::ErrorCode;
    use sqlx::postgres::{PgConnectOptions, PgPoolOptions};

    use super::*;

    /// A pool that never connects until asked: nobody listens on port 1.
    fn lazy_pool(max_connections: u32) -> PgPool {
        PgPoolOptions::new()
            .max_connections(max_connections)
            .connect_lazy_with(PgConnectOptions::new().host("127.0.0.1").port(1))
    }

    fn clock() -> Arc<dyn Clock> {
        Arc::new(ManualClock::new(
            SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000),
        ))
    }

    #[tokio::test]
    async fn a_pool_of_two_connections_is_refused() {
        let error = Registry::new(lazy_pool(2), clock()).expect_err("two are too few");
        assert_eq!(error.code(), ErrorCode::InvalidArgument);
        assert_eq!(error.reason(), None);
        assert_eq!(
            error.message(),
            "the control pool needs at least 3 connections"
        );
    }

    #[tokio::test]
    async fn a_pool_of_three_connections_is_accepted_without_io() {
        let pool = lazy_pool(3);
        let registry = Registry::new(pool.clone(), clock()).expect("three are enough");
        assert_eq!(registry.pool().options().get_max_connections(), 3);
        assert_eq!(pool.size(), 0);
    }

    #[tokio::test]
    async fn debug_prints_the_type_name_only() {
        let registry = Registry::new(lazy_pool(3), clock()).expect("three are enough");
        assert_eq!(format!("{registry:?}"), "Registry(..)");
    }

    #[tokio::test]
    async fn a_path_label_the_label_rules_refuse_is_none_without_a_connection() {
        let pool = lazy_pool(3);
        let rules = LabelRules::new()
            .with_length(3, 20)
            .expect("valid lengths")
            .with_reserved(|label| label == "admin");
        let registry = Registry::new(pool.clone(), clock())
            .expect("three are enough")
            .labels(rules);
        let lax = registry.clone().labels(LabelRules::new());
        pool.close().await;
        let on = |segment: &str, listener: &str| Selector::PathLabel {
            segment: segment.to_owned(),
            listener_host: listener.to_owned(),
        };

        for listener in ["localhost:8080", "app.example.com"] {
            for segment in ["admin", "ADMIN", "ab"] {
                let found = registry.lookup(&on(segment, listener)).await;
                assert_eq!(found.expect("no connection is asked for"), None);
            }
            // A closed pool fails every read: the lookups above never asked it.
            let read = registry.lookup(&on("billing", listener)).await;
            assert!(read.is_err(), "{listener}: an allowed segment reads");
            let read = lax.lookup(&on("admin", listener)).await;
            assert!(read.is_err(), "{listener}: a clone keeps its own rules");
        }
    }

    #[tokio::test(start_paused = true)]
    async fn the_admission_wait_is_a_values_own_and_the_free_permits_are_shared() {
        let registry = Registry::new(lazy_pool(5), clock()).expect("five are enough");
        assert_eq!(registry.lifecycle_available(), 2);
        let brief = registry.clone().with_admission_wait(Duration::from_secs(1));
        let first = brief.admit().await.expect("a first permit");
        let second = registry.admit().await.expect("a second permit");
        assert_eq!(registry.lifecycle_available(), 0);
        assert_eq!(brief.lifecycle_available(), 0);

        let started = tokio::time::Instant::now();
        let refused = brief.clone().admit().await.expect_err("both are held");
        assert_eq!(started.elapsed(), Duration::from_secs(1));
        assert_eq!(refused.code(), ErrorCode::Unavailable);
        assert_eq!(refused.reason(), None);

        drop(first);
        assert_eq!(registry.lifecycle_available(), 1);
        drop(second);
        assert_eq!(brief.lifecycle_available(), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn clones_share_the_lifecycle_permits() {
        let registry = Registry::new(lazy_pool(3), clock()).expect("three are enough");
        let holder = registry.clone();
        let waiter = registry.clone();

        let permit = holder.admit().await.expect("the one permit is free");
        let started = tokio::time::Instant::now();
        let refused = waiter
            .admit()
            .await
            .expect_err("the permit is held through another clone");
        assert!(started.elapsed() >= Duration::from_secs(10));
        assert_eq!(refused.code(), ErrorCode::Unavailable);
        assert_eq!(refused.reason(), None);

        drop(permit);
        let third = registry.clone();
        let admitted = third.admit().await;
        assert!(admitted.is_ok(), "the returned permit is free again");
    }
}
