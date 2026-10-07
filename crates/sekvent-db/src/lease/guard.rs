use std::time::{Duration, SystemTime};

use futures::future::BoxFuture;
use sekvent_error::AppError;
use sekvent_runtime::{JobGuard, JobPermit};

use super::{DEFAULT_LEASE_TTL, LeaseStore, validate_name, validate_ttl};

/// A [`JobGuard`] over a [`LeaseStore`]: one instance runs each tick, and a
/// lost lease cancels the run.
///
/// A scheduled or catch-up run takes the lease for its tick
/// ([`LeaseStore::try_acquire_tick`]), so each tick runs once across all
/// instances; a manual run takes it without a tick and never touches the
/// tick record. The lease is renewed while the run goes on, its fence
/// reaches the run as `JobContext::fence`, and it is released after the
/// run.
///
/// ```ignore
/// let store = LeaseStore::new(registry.get("orders_db")?);
/// store.verify_schema().await?;
/// let spec = JobSpec::interval(Duration::from_secs(900)).singleton(LeaseGuard::new(store));
/// let builder = builder.job("orders-sync", Stage::Workers, spec, |cx| sync(cx));
/// ```
#[derive(Debug, Clone)]
pub struct LeaseGuard {
    store: LeaseStore,
    ttl: Duration,
    lease_name: Option<String>,
}

impl LeaseGuard {
    /// Lease named after the job, TTL 30 s.
    pub fn new(store: LeaseStore) -> Self {
        Self {
            store,
            ttl: DEFAULT_LEASE_TTL,
            lease_name: None,
        }
    }

    /// Hold the lease for `ttl` (1 s to 24 h) between renewals.
    pub fn with_ttl(mut self, ttl: Duration) -> Result<Self, AppError> {
        validate_ttl(ttl)?;
        self.ttl = ttl;
        Ok(self)
    }

    /// Use this lease name instead of the job's, so several jobs (or direct
    /// lease users) exclude each other.
    ///
    /// The tick record belongs to the lease row, so share a name between one
    /// scheduled job and on-demand work (manual jobs, direct
    /// [`LeaseStore::try_acquire`] callers), not between two scheduled jobs:
    /// of two scheduled jobs due at the same tick, only one would run.
    pub fn with_lease_name(mut self, name: &str) -> Result<Self, AppError> {
        validate_name(name)?;
        self.lease_name = Some(name.to_owned());
        Ok(self)
    }

    fn lease_name<'a>(&'a self, job: &'a str) -> &'a str {
        self.lease_name.as_deref().unwrap_or(job)
    }

    async fn take(
        &self,
        job: &str,
        tick: Option<SystemTime>,
    ) -> Result<Option<JobPermit>, AppError> {
        let name = self.lease_name(job);
        let lease = match tick {
            Some(tick) => self.store.try_acquire_tick(name, tick, self.ttl).await?,
            None => self.store.try_acquire(name, self.ttl).await?,
        };
        Ok(lease.map(|lease| {
            let held = lease.keep_alive();
            let fence = held.fence().get();
            let lost = held.lost();
            JobPermit::new(Some(fence), lost, move || -> BoxFuture<'static, ()> {
                Box::pin(async move {
                    let name = held.name().to_owned();
                    let fence = held.fence();
                    if let Err(error) = held.release().await {
                        tracing::debug!(
                            lease = %name,
                            fence = %fence,
                            code = %error.code(),
                            "releasing a job lease failed; it expires on its own"
                        );
                    }
                })
            })
        }))
    }
}

impl JobGuard for LeaseGuard {
    fn acquire<'a>(
        &'a self,
        job: &'a str,
        tick: Option<SystemTime>,
    ) -> BoxFuture<'a, Result<Option<JobPermit>, AppError>> {
        Box::pin(self.take(job, tick))
    }

    fn last_tick<'a>(
        &'a self,
        job: &'a str,
    ) -> BoxFuture<'a, Result<Option<SystemTime>, AppError>> {
        Box::pin(self.store.last_tick(self.lease_name(job)))
    }
}

#[cfg(all(test, feature = "sqlx-postgres"))]
mod tests {
    use sekvent_error::ErrorCode;

    use super::*;
    use crate::Pool;

    #[tokio::test]
    async fn settings_are_validated() {
        let pool =
            Pool::Postgres(sqlx::PgPool::connect_lazy("postgres://u@192.0.2.1:1/a").unwrap());
        let guard = LeaseGuard::new(LeaseStore::new(&pool));
        assert_eq!(guard.ttl, DEFAULT_LEASE_TTL);
        assert_eq!(guard.lease_name("orders-sync"), "orders-sync");

        let guard = guard
            .with_ttl(Duration::from_secs(5))
            .unwrap()
            .with_lease_name("orders/shared")
            .unwrap();
        assert_eq!(guard.ttl, Duration::from_secs(5));
        assert_eq!(guard.lease_name("orders-sync"), "orders/shared");
        assert!(format!("{guard:?}").contains("orders/shared"));

        assert_eq!(
            guard.clone().with_ttl(Duration::ZERO).unwrap_err().code(),
            ErrorCode::InvalidArgument
        );
        assert_eq!(
            guard
                .clone()
                .with_lease_name("no spaces")
                .unwrap_err()
                .code(),
            ErrorCode::InvalidArgument
        );
        // Invalid job names surface from the store before any query.
        let Err(error) = LeaseGuard::new(LeaseStore::new(&pool))
            .acquire("bad job", None)
            .await
        else {
            panic!("an invalid lease name must fail");
        };
        assert_eq!(error.code(), ErrorCode::InvalidArgument);
        let error = LeaseGuard::new(LeaseStore::new(&pool))
            .last_tick("bad job")
            .await
            .unwrap_err();
        assert_eq!(error.code(), ErrorCode::InvalidArgument);
        pool.close().await;
    }
}
