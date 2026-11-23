//! The product's side of the lifecycle: what it does when a tenant is seeded, adopted, suspended,
//! resumed, described or measured.
//!
//! Every hook but [`TenantHooks::check_adoptable`] receives the tenant's info and a pool on the
//! tenant's migrated database, opened as the tenant's role for this one operation (at most two
//! connections, closed when the operation ends) and outside the tenant cache: a hook never gets a
//! built scope, so an expensive scope is never built to seed, suspend or measure a tenant.

use std::collections::BTreeMap;
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::time::SystemTime;

use sekvent_config::Secret;
use sekvent_error::AppError;
use sqlx::PgPool;

use crate::cluster::handover::CHECK_KEY;
use crate::{DatabaseName, TenantId, TenantInfo, reasons};

/// The future a hook returns.
pub type HookFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, AppError>> + Send + 'a>>;

/// The `check` of the refusal [`TenantHooks::check_adoptable`] gives when the product does not
/// implement it.
pub const NOT_CHECKED: &str = "not_checked";

/// The product's side of the lifecycle. Every method has a default; each default does nothing
/// and succeeds, except [`check_adoptable`](Self::check_adoptable), which refuses every database
/// (fail closed: a product that adopts databases says which ones it accepts).
///
/// A hook's error is the lifecycle call's answer, unchanged. A hook runs within the call: a
/// failure leaves the work committed before it in place, and a repeated call runs the hook again.
pub trait TenantHooks: Send + Sync + 'static {
    /// The product's seed payload, handed to [`seed`](Self::seed) as given in the
    /// [`ProvisionRequest`](crate::lifecycle::ProvisionRequest).
    type Seed: Send + Sync + 'static;

    /// Fills a new tenant's database. Must be idempotent: a call made after an earlier one
    /// committed (the step's own record failed, say) changes nothing and returns no handover.
    ///
    /// The returned handover secret (an owner's first sign-in token, say) reaches the caller of
    /// the provisioning call that ran the seed, and never again.
    fn seed<'a>(
        &'a self,
        tenant: &'a TenantInfo,
        pool: &'a PgPool,
        seed: &'a Self::Seed,
    ) -> HookFuture<'a, SeedOutcome> {
        let _ = (tenant, pool, seed);
        Box::pin(std::future::ready(Ok(SeedOutcome::default())))
    }

    /// Whether `candidate` is a database of this product that may be adopted. Read-only.
    ///
    /// The exception among the hooks: `pool` is the maintenance role's, on the candidate
    /// database, before the handover and before any migration of the product's; its sessions
    /// start read-only and keep the maintenance connection's server-side bounds. A refusal is the
    /// adoption's answer and leaves no control row and no role.
    ///
    /// The default refuses every database: `FAILED_PRECONDITION` /
    /// [`ADOPTION_REFUSED`](reasons::ADOPTION_REFUSED) with metadata `check` set to
    /// [`NOT_CHECKED`].
    fn check_adoptable<'a>(
        &'a self,
        candidate: &'a AdoptCandidate,
        pool: &'a PgPool,
    ) -> HookFuture<'a, ()> {
        let _ = (candidate, pool);
        Box::pin(std::future::ready(Err(not_checked())))
    }

    /// Takes an adopted database into use after it was handed over and migrated: the adoption's
    /// counterpart of [`seed`](Self::seed). Must be idempotent.
    fn adopted<'a>(&'a self, tenant: &'a TenantInfo, pool: &'a PgPool) -> HookFuture<'a, ()> {
        let _ = (tenant, pool);
        Box::pin(std::future::ready(Ok(())))
    }

    /// Runs before an active tenant is suspended; an error leaves it active.
    fn before_suspend<'a>(
        &'a self,
        tenant: &'a TenantInfo,
        pool: &'a PgPool,
    ) -> HookFuture<'a, ()> {
        let _ = (tenant, pool);
        Box::pin(std::future::ready(Ok(())))
    }

    /// Runs before a suspended tenant is resumed; an error leaves it suspended.
    fn before_resume<'a>(&'a self, tenant: &'a TenantInfo, pool: &'a PgPool) -> HookFuture<'a, ()> {
        let _ = (tenant, pool);
        Box::pin(std::future::ready(Ok(())))
    }

    /// What an operator's view shows of the tenant. The default is the tenant's display name
    /// and no attribute.
    fn describe<'a>(
        &'a self,
        tenant: &'a TenantInfo,
        pool: &'a PgPool,
    ) -> HookFuture<'a, Description> {
        let _ = pool;
        Box::pin(std::future::ready(Ok(Description {
            display_name: tenant.display_name.clone(),
            attributes: BTreeMap::new(),
        })))
    }

    /// The product's usage counters of the tenant, read on sessions that start read-only and
    /// whose statements time out after 5 s. The default has no counter and no activity.
    fn usage<'a>(
        &'a self,
        tenant: &'a TenantInfo,
        pool: &'a PgPool,
    ) -> HookFuture<'a, UsageCounters> {
        let _ = (tenant, pool);
        Box::pin(std::future::ready(Ok(UsageCounters::default())))
    }
}

/// The refusal of the default [`TenantHooks::check_adoptable`].
fn not_checked() -> AppError {
    AppError::failed_precondition("the database cannot be adopted")
        .with_reason(reasons::ADOPTION_REFUSED)
        .with_metadata(CHECK_KEY, NOT_CHECKED)
}

/// What a seed hands back, once. `Debug` tells whether there is a handover, never its value.
#[derive(Clone, Default)]
pub struct SeedOutcome {
    /// A secret for whoever takes the new tenant over; `None` when there is none, or when this
    /// call found the seed done already.
    pub handover: Option<Secret>,
}

impl SeedOutcome {
    /// An outcome with `handover`.
    pub fn with_handover(handover: Secret) -> Self {
        Self {
            handover: Some(handover),
        }
    }
}

// `SeedOutcome { handover: true }`
impl fmt::Debug for SeedOutcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SeedOutcome")
            .field("handover", &self.handover.is_some())
            .finish()
    }
}

/// The database an adoption would take, as [`TenantHooks::check_adoptable`] sees it.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct AdoptCandidate {
    /// The tenant the database would become.
    pub tenant: TenantId,
    /// The database.
    pub database: DatabaseName,
    /// The tenant's label as requested.
    pub subdomain: String,
    /// The tenant's display name as requested.
    pub display_name: String,
}

impl AdoptCandidate {
    /// A candidate; for a product's own tests of its hook.
    pub fn new(
        tenant: TenantId,
        database: DatabaseName,
        subdomain: impl Into<String>,
        display_name: impl Into<String>,
    ) -> Self {
        Self {
            tenant,
            database,
            subdomain: subdomain.into(),
            display_name: display_name.into(),
        }
    }
}

/// A tenant as an operator's view shows it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Description {
    /// The name to show.
    pub display_name: String,
    /// The product's attributes, by name.
    pub attributes: BTreeMap<String, String>,
}

/// The product's usage counters of a tenant.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct UsageCounters {
    /// Named counts (accounts, documents, …).
    pub counters: BTreeMap<String, i64>,
    /// When the tenant was last active, if the product knows.
    pub last_activity: Option<SystemTime>,
}

#[cfg(test)]
mod tests {
    use sekvent_error::ErrorCode;
    use uuid::Uuid;

    use super::*;
    use crate::{RoleKey, TenantState};

    struct Defaults;

    impl TenantHooks for Defaults {
        type Seed = ();
    }

    fn info() -> TenantInfo {
        TenantInfo {
            id: TenantId::from_uuid(Uuid::from_u128(7)),
            subdomain: "orders".to_owned(),
            display_name: "Orders".to_owned(),
            state: TenantState::Active,
            primary_host: "orders.example.com".to_owned(),
            hosts: vec!["orders.example.com".to_owned()],
            role_key: RoleKey {
                key_id: "k1".to_owned(),
                generation: 1,
            },
        }
    }

    /// A pool that never connects: nobody listens on port 1.
    fn lazy_pool() -> PgPool {
        sqlx::postgres::PgPoolOptions::new().connect_lazy_with(
            sqlx::postgres::PgConnectOptions::new()
                .host("127.0.0.1")
                .port(1),
        )
    }

    #[tokio::test]
    async fn the_defaults_do_nothing_and_succeed() {
        let hooks = Defaults;
        let info = info();
        let pool = lazy_pool();
        let seeded = hooks
            .seed(&info, &pool, &())
            .await
            .expect("the default seed");
        assert!(seeded.handover.is_none());
        hooks
            .adopted(&info, &pool)
            .await
            .expect("the default adopted");
        hooks
            .before_suspend(&info, &pool)
            .await
            .expect("the default suspend");
        hooks
            .before_resume(&info, &pool)
            .await
            .expect("the default resume");
        let description = hooks
            .describe(&info, &pool)
            .await
            .expect("the default describe");
        assert_eq!(description.display_name, "Orders");
        assert_eq!(description.attributes, BTreeMap::new());
        let usage = hooks.usage(&info, &pool).await.expect("the default usage");
        assert_eq!(usage, UsageCounters::default());
        assert_eq!(pool.size(), 0, "no default touched the pool");
    }

    #[tokio::test]
    async fn the_default_adoption_check_refuses() {
        let candidate = AdoptCandidate::new(
            TenantId::from_uuid(Uuid::from_u128(7)),
            DatabaseName::parse("legacy_orders").expect("a database name"),
            "orders",
            "Orders",
        );
        let error = Defaults
            .check_adoptable(&candidate, &lazy_pool())
            .await
            .expect_err("the default refuses");
        assert_eq!(error.code(), ErrorCode::FailedPrecondition);
        assert_eq!(error.reason(), Some(reasons::ADOPTION_REFUSED));
        assert_eq!(error.domain(), None);
        assert_eq!(
            error.metadata().get(CHECK_KEY).map(String::as_str),
            Some(NOT_CHECKED)
        );
    }

    #[test]
    fn a_seed_outcome_prints_whether_it_has_a_handover_only() {
        let outcome = SeedOutcome::with_handover(Secret::new("first-sign-in-0123".to_owned()));
        let printed = format!("{outcome:?}");
        assert_eq!(printed, "SeedOutcome { handover: true }");
        assert_eq!(
            format!("{:?}", SeedOutcome::default()),
            "SeedOutcome { handover: false }"
        );
    }
}
