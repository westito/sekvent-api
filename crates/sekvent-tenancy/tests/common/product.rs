//! The test product: a tenant schema of two migrations, a scope whose build a
//! test can hold at a gate, fail and count, and (feature `lifecycle`) hooks
//! whose seed inserts one order and hands over a secret once, and whose usage
//! counts the orders.
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

/// The lifecycle hooks of the test product (feature `lifecycle`).
#[cfg(feature = "lifecycle")]
pub mod hooks {
    use std::collections::{BTreeMap, HashMap};
    use std::sync::{Arc, Mutex, PoisonError};

    use sekvent_config::Secret;
    use sekvent_error::AppError;
    use sekvent_tenancy::TenantInfo;
    use sekvent_tenancy::lifecycle::{
        AdoptCandidate, Description, HookFuture, SeedOutcome, TenantHooks, UsageCounters,
    };
    use sqlx::PgPool;
    use tokio::sync::watch;

    /// The hooks by name, as [`OrdersHooks::calls`] records them and
    /// [`OrdersHooks::fail`] and [`OrdersHooks::hold`] take them.
    pub const SEED: &str = "seed";
    /// `check_adoptable`.
    pub const CHECK_ADOPTABLE: &str = "check_adoptable";
    /// `adopted`.
    pub const ADOPTED: &str = "adopted";
    /// `before_suspend`.
    pub const BEFORE_SUSPEND: &str = "before_suspend";
    /// `before_resume`.
    pub const BEFORE_RESUME: &str = "before_resume";
    /// `describe`.
    pub const DESCRIBE: &str = "describe";
    /// `usage`.
    pub const USAGE: &str = "usage";

    /// The seed payload: the item of the one order a seed inserts.
    #[derive(Clone, Debug, PartialEq, Eq)]
    pub struct OrdersSeed {
        /// The order's item.
        pub item: String,
    }

    impl OrdersSeed {
        /// A seed of `item`.
        pub fn new(item: &str) -> Self {
            Self {
                item: item.to_owned(),
            }
        }
    }

    /// The secret a seed hands over for `tenant`.
    pub fn handover_of(tenant: &TenantInfo) -> String {
        format!("handover-{}", tenant.id)
    }

    /// A hook's injected failure.
    type Failure = fn() -> AppError;

    struct State {
        /// Every hook call, in order, by name.
        calls: Mutex<Vec<&'static str>>,
        /// Hooks that fail, with their error.
        fail: Mutex<HashMap<&'static str, Failure>>,
        /// The hook held at the gate, if any, and whether the gate is open.
        held: Mutex<Option<&'static str>>,
        gate: watch::Sender<bool>,
        /// Hook calls that reached the gate.
        arrived: watch::Sender<usize>,
    }

    /// The test product's hooks. Clones share the record, the failures and
    /// the gate.
    #[derive(Clone)]
    pub struct OrdersHooks {
        state: Arc<State>,
    }

    impl Default for OrdersHooks {
        fn default() -> Self {
            Self::new()
        }
    }

    impl OrdersHooks {
        /// Hooks that succeed: the seed inserts one order (only into an empty
        /// table) and hands over [`handover_of`] the tenant when it inserted;
        /// `check_adoptable` accepts every database; usage counts `orders`.
        pub fn new() -> Self {
            Self {
                state: Arc::new(State {
                    calls: Mutex::new(Vec::new()),
                    fail: Mutex::new(HashMap::new()),
                    held: Mutex::new(None),
                    gate: watch::Sender::new(true),
                    arrived: watch::Sender::new(0),
                }),
            }
        }

        /// The hooks called so far, in order.
        pub fn calls(&self) -> Vec<&'static str> {
            self.state
                .calls
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone()
        }

        /// How often `hook` was called.
        pub fn count(&self, hook: &str) -> usize {
            self.calls().iter().filter(|name| **name == hook).count()
        }

        /// `hook` fails with `error()` from now on; `None` lets it succeed again.
        pub fn fail(&self, hook: &'static str, error: Option<fn() -> AppError>) {
            let mut fail = self
                .state
                .fail
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            match error {
                Some(error) => fail.insert(hook, error),
                None => fail.remove(hook),
            };
        }

        /// Calls of `hook` wait at a closed gate from now on (one hook at a
        /// time); [`release`](Self::release) opens it.
        pub fn hold(&self, hook: &'static str) {
            *self
                .state
                .held
                .lock()
                .unwrap_or_else(PoisonError::into_inner) = Some(hook);
            self.state.gate.send_replace(false);
        }

        /// Opens the gate and holds no hook any more.
        pub fn release(&self) {
            *self
                .state
                .held
                .lock()
                .unwrap_or_else(PoisonError::into_inner) = None;
            self.state.gate.send_replace(true);
        }

        /// Resolves once `count` calls of the held hook reached the gate.
        pub async fn arrived(&self, count: usize) {
            let mut seen = self.state.arrived.subscribe();
            let reached = seen.wait_for(|value| *value >= count).await.is_ok();
            assert!(reached, "the hooks outlive their waiters");
        }

        /// Records the call, waits at the gate when `hook` is held, then
        /// answers its failure if one is set.
        async fn enter(&self, hook: &'static str) -> Result<(), AppError> {
            self.state
                .calls
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(hook);
            let held = *self
                .state
                .held
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            if held == Some(hook) {
                self.state.arrived.send_modify(|count| *count += 1);
                let mut gate = self.state.gate.subscribe();
                let opened = gate.wait_for(|open| *open).await.is_ok();
                assert!(opened, "the hooks outlive their calls");
            }
            let fail = self
                .state
                .fail
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .get(hook)
                .copied();
            match fail {
                Some(error) => Err(error()),
                None => Ok(()),
            }
        }
    }

    /// The number of orders in the tenant's database.
    async fn orders(pool: &PgPool) -> Result<i64, AppError> {
        sqlx::query_scalar("SELECT count(*) FROM orders")
            .fetch_one(pool)
            .await
            .map_err(sekvent_db::to_app_error)
    }

    impl TenantHooks for OrdersHooks {
        type Seed = OrdersSeed;

        fn seed<'a>(
            &'a self,
            tenant: &'a TenantInfo,
            pool: &'a PgPool,
            seed: &'a OrdersSeed,
        ) -> HookFuture<'a, SeedOutcome> {
            Box::pin(async move {
                self.enter(SEED).await?;
                let inserted = sqlx::query(
                    "INSERT INTO orders (item) SELECT $1 WHERE NOT EXISTS (SELECT 1 FROM orders)",
                )
                .bind(seed.item.as_str())
                .execute(pool)
                .await
                .map_err(sekvent_db::to_app_error)?
                .rows_affected();
                Ok(if inserted == 1 {
                    SeedOutcome::with_handover(Secret::new(handover_of(tenant)))
                } else {
                    SeedOutcome::default()
                })
            })
        }

        fn check_adoptable<'a>(
            &'a self,
            candidate: &'a AdoptCandidate,
            pool: &'a PgPool,
        ) -> HookFuture<'a, ()> {
            let _ = (candidate, pool);
            Box::pin(self.enter(CHECK_ADOPTABLE))
        }

        fn adopted<'a>(&'a self, tenant: &'a TenantInfo, pool: &'a PgPool) -> HookFuture<'a, ()> {
            let _ = (tenant, pool);
            Box::pin(self.enter(ADOPTED))
        }

        fn before_suspend<'a>(
            &'a self,
            tenant: &'a TenantInfo,
            pool: &'a PgPool,
        ) -> HookFuture<'a, ()> {
            let _ = tenant;
            Box::pin(async move {
                self.enter(BEFORE_SUSPEND).await?;
                orders(pool).await.map(drop)
            })
        }

        fn before_resume<'a>(
            &'a self,
            tenant: &'a TenantInfo,
            pool: &'a PgPool,
        ) -> HookFuture<'a, ()> {
            let _ = tenant;
            Box::pin(async move {
                self.enter(BEFORE_RESUME).await?;
                orders(pool).await.map(drop)
            })
        }

        fn describe<'a>(
            &'a self,
            tenant: &'a TenantInfo,
            pool: &'a PgPool,
        ) -> HookFuture<'a, Description> {
            Box::pin(async move {
                self.enter(DESCRIBE).await?;
                let count = orders(pool).await?;
                Ok(Description {
                    display_name: tenant.display_name.clone(),
                    attributes: BTreeMap::from([("orders".to_owned(), count.to_string())]),
                })
            })
        }

        fn usage<'a>(
            &'a self,
            tenant: &'a TenantInfo,
            pool: &'a PgPool,
        ) -> HookFuture<'a, UsageCounters> {
            let _ = tenant;
            Box::pin(async move {
                self.enter(USAGE).await?;
                let count = orders(pool).await?;
                Ok(UsageCounters {
                    counters: BTreeMap::from([("orders".to_owned(), count)]),
                    last_activity: None,
                })
            })
        }
    }
}
