//! Leases against real Postgres and MySQL 8 servers.
//!
//! ```sh
//! SEKVENT_DOCKER_TESTS=1 cargo test -p sekvent-db --all-features -- --ignored
//! ```
//!
//! Every scenario runs once per backend (`postgres::*`, `mysql::*`), each
//! on a fresh database of the shared server. Without
//! `SEKVENT_DOCKER_TESTS=1` each test returns immediately. Socket tests run
//! on the real clock: they wait on observable state with a 30 s guard and
//! assert structure, never exact timings.

#![cfg(all(feature = "lease", feature = "sqlx-postgres", feature = "sqlx-mysql"))]

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use sekvent_db::reasons::{LEASE_LOST, LEASE_SCHEMA_MISSING};
use sekvent_db::{FencingToken, LeaseStore, Pool};
use sekvent_error::{AppError, ErrorCode};
use sekvent_testing::{MySqlHarness, PostgresHarness, await_until, docker_tests_enabled};
use sqlx::AssertSqlSafe;
use tokio::task::JoinSet;

const TTL: Duration = Duration::from_secs(30);
const GUARD: Duration = Duration::from_secs(30);

fn skip() -> bool {
    if docker_tests_enabled() {
        return false;
    }
    eprintln!("skipped: set SEKVENT_DOCKER_TESTS=1 to run Docker-backed tests");
    true
}

async fn postgres_pool() -> Pool {
    let server = PostgresHarness::shared().await;
    let database = server.create_database().await.unwrap();
    Pool::Postgres(sqlx::PgPool::connect(&database.url).await.unwrap())
}

async fn mysql_pool() -> Pool {
    let server = MySqlHarness::shared().await;
    let database = server.create_database().await.unwrap();
    Pool::MySql(sqlx::MySqlPool::connect(&database.url).await.unwrap())
}

async fn exec(pool: &Pool, sql: &str) {
    if let Some(pg) = pool.postgres() {
        sqlx::raw_sql(AssertSqlSafe(sql)).execute(pg).await.unwrap();
    } else {
        let mysql = pool.mysql().unwrap();
        sqlx::raw_sql(AssertSqlSafe(sql))
            .execute(mysql)
            .await
            .unwrap();
    }
}

/// Push the expiry of `name` into the past, as if the holder stalled.
async fn expire(pool: &Pool, name: &str) {
    let past = if pool.postgres().is_some() {
        "now() - interval '1 second'"
    } else {
        "UTC_TIMESTAMP(6) - INTERVAL 1 SECOND"
    };
    exec(
        pool,
        &format!("UPDATE sekvent_leases SET expires_at = {past} WHERE name = '{name}'"),
    )
    .await;
}

/// Take the row over behind the holder's back.
async fn steal(pool: &Pool, name: &str) {
    exec(
        pool,
        &format!("UPDATE sekvent_leases SET owner = 'thief' WHERE name = '{name}'"),
    )
    .await;
}

async fn store(pool: &Pool, holder: &str) -> LeaseStore {
    let store = LeaseStore::new(pool).with_holder(holder).unwrap();
    store.ensure_schema().await.unwrap();
    store
}

fn lost(error: &AppError) -> bool {
    error.code() == ErrorCode::Aborted && error.reason() == Some(LEASE_LOST)
}

async fn schema_is_created_once_and_verified(pool: &Pool) {
    let store = LeaseStore::new(pool);
    let missing = store.verify_schema().await.unwrap_err();
    assert_eq!(missing.code(), ErrorCode::FailedPrecondition);
    assert_eq!(missing.reason(), Some(LEASE_SCHEMA_MISSING));
    assert!(missing.message().contains("sekvent_leases"));

    store.ensure_schema().await.unwrap();
    store.ensure_schema().await.unwrap();
    store.verify_schema().await.unwrap();

    exec(
        pool,
        "CREATE TABLE partial_leases (name VARCHAR(200) PRIMARY KEY)",
    )
    .await;
    let partial = store.clone().with_table("partial_leases").unwrap();
    let incomplete = partial.verify_schema().await.unwrap_err();
    assert_eq!(incomplete.reason(), Some(LEASE_SCHEMA_MISSING));
    assert!(incomplete.message().contains("partial_leases"));

    let custom = store.with_table("app_leases").unwrap();
    custom.ensure_schema().await.unwrap();
    custom.verify_schema().await.unwrap();
    assert!(
        custom
            .try_acquire("orders-sync", TTL)
            .await
            .unwrap()
            .is_some()
    );
}

async fn a_held_lease_excludes_and_fences_grow(pool: &Pool) {
    let a = store(pool, "node-a").await;
    let b = store(pool, "node-b").await;
    assert_eq!(a.info("orders-sync").await.unwrap(), None);

    let first = a.try_acquire("orders-sync", TTL).await.unwrap().unwrap();
    assert_eq!(first.name(), "orders-sync");
    assert!(first.valid_until() > tokio::time::Instant::now());
    assert!(!format!("{first:?}").contains("owner:"));
    assert!(b.try_acquire("orders-sync", TTL).await.unwrap().is_none());
    assert!(a.try_acquire("orders-sync", TTL).await.unwrap().is_none());

    let info = a.info("orders-sync").await.unwrap().unwrap();
    assert_eq!(info.holder, "node-a");
    assert_eq!(info.fence, first.fence());
    assert!(
        info.remaining <= TTL && info.remaining > TTL.checked_sub(Duration::from_secs(10)).unwrap()
    );

    // After a release, another holder gets the next fence.
    let first_fence = first.fence();
    first.release().await.unwrap();
    assert_eq!(b.info("orders-sync").await.unwrap(), None);
    let mut second = b.try_acquire("orders-sync", TTL).await.unwrap().unwrap();
    assert_eq!(second.fence(), FencingToken::new(first_fence.get() + 1));
    second.renew().await.unwrap();

    // After an expiry, too; the stale holder can no longer renew.
    expire(pool, "orders-sync").await;
    let third = a.try_acquire("orders-sync", TTL).await.unwrap().unwrap();
    assert_eq!(third.fence(), FencingToken::new(first_fence.get() + 2));
    assert!(lost(&second.renew().await.unwrap_err()));
    // Releasing a lease that is no longer ours leaves the holder alone.
    second.release().await.unwrap();
    assert_eq!(
        b.info("orders-sync").await.unwrap().unwrap().fence,
        third.fence()
    );
}

async fn a_lease_renewed_after_its_expiry_is_lost(pool: &Pool) {
    let a = store(pool, "node-a").await;
    let mut lease = a.try_acquire("reports", TTL).await.unwrap().unwrap();
    lease.renew().await.unwrap();
    expire(pool, "reports").await;
    let error = lease.renew().await.unwrap_err();
    assert!(lost(&error));
    assert_eq!(error.metadata()["lease"], "reports");
}

async fn each_tick_runs_once(pool: &Pool) {
    let a = store(pool, "node-a").await;
    let b = store(pool, "node-b").await;
    let t1 = UNIX_EPOCH + Duration::from_hours(500_000);
    let t2 = t1 + Duration::from_secs(60);
    assert_eq!(a.last_tick("nightly").await.unwrap(), None);

    a.try_acquire_tick("nightly", t1, TTL)
        .await
        .unwrap()
        .unwrap()
        .release()
        .await
        .unwrap();
    assert!(
        b.try_acquire_tick("nightly", t1, TTL)
            .await
            .unwrap()
            .is_none(),
        "the tick already ran, even though the lease is free"
    );
    assert!(
        b.try_acquire_tick("nightly", t1 - Duration::from_secs(60), TTL)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(a.last_tick("nightly").await.unwrap(), Some(t1));

    // A manual acquisition leaves the tick record alone.
    a.try_acquire("nightly", TTL)
        .await
        .unwrap()
        .unwrap()
        .release()
        .await
        .unwrap();
    assert_eq!(a.last_tick("nightly").await.unwrap(), Some(t1));

    let held = b
        .try_acquire_tick("nightly", t2, TTL)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(b.last_tick("nightly").await.unwrap(), Some(t2));
    assert!(
        a.try_acquire_tick("nightly", t2 + Duration::from_secs(60), TTL)
            .await
            .unwrap()
            .is_none(),
        "a later tick still waits for the holder"
    );
    held.release().await.unwrap();
}

async fn check_fence(
    store: &LeaseStore,
    pool: &Pool,
    name: &str,
    fence: FencingToken,
) -> Result<(), AppError> {
    if let Some(pg) = pool.postgres() {
        let mut tx = pg.begin().await.unwrap();
        let result = store.check_fence_postgres(&mut tx, name, fence).await;
        tx.commit().await.unwrap();
        result
    } else {
        let mut tx = pool.mysql().unwrap().begin().await.unwrap();
        let result = store.check_fence_mysql(&mut tx, name, fence).await;
        tx.commit().await.unwrap();
        result
    }
}

async fn fences_are_checked_inside_a_transaction(pool: &Pool) {
    let a = store(pool, "node-a").await;
    let b = store(pool, "node-b").await;
    let old = a.try_acquire("ledger", TTL).await.unwrap().unwrap();
    check_fence(&a, pool, "ledger", old.fence()).await.unwrap();

    expire(pool, "ledger").await;
    let new = b.try_acquire("ledger", TTL).await.unwrap().unwrap();
    let error = check_fence(&a, pool, "ledger", old.fence())
        .await
        .unwrap_err();
    assert!(lost(&error));
    assert_eq!(error.metadata()["fence"], old.fence().to_string());
    check_fence(&b, pool, "ledger", new.fence()).await.unwrap();

    let fence = new.fence();
    new.release().await.unwrap();
    assert!(lost(
        &check_fence(&b, pool, "ledger", fence).await.unwrap_err()
    ));
}

async fn race(store: &LeaseStore, tick: Option<SystemTime>) -> usize {
    let mut racers = JoinSet::new();
    for i in 0..8 {
        let store = store.clone().with_holder(&format!("racer-{i}")).unwrap();
        racers.spawn(async move {
            match tick {
                Some(tick) => store.try_acquire_tick("race", tick, TTL).await.unwrap(),
                None => store.try_acquire("race", TTL).await.unwrap(),
            }
        });
    }
    let mut leases = Vec::new();
    while let Some(lease) = racers.join_next().await {
        leases.extend(lease.unwrap());
    }
    let won = leases.len();
    for lease in leases {
        lease.release().await.unwrap();
    }
    won
}

async fn racing_acquirers_get_exactly_one_lease(pool: &Pool) {
    let store = store(pool, "racer").await;
    // The row does not exist yet: the racers also race to create it.
    assert_eq!(race(&store, None).await, 1);
    assert_eq!(race(&store, None).await, 1);
    let tick = UNIX_EPOCH + Duration::from_hours(500_000);
    assert_eq!(race(&store, Some(tick)).await, 1);
    assert_eq!(race(&store, Some(tick)).await, 0, "the tick already ran");
}

async fn keep_alive_holds_the_lease_across_renewals(pool: &Pool) {
    let a = store(pool, "node-a").await;
    let ttl = Duration::from_secs(3);
    let lease = a.try_acquire("heartbeat", ttl).await.unwrap().unwrap();
    let fence = lease.fence();
    let held = lease.keep_alive();
    assert_eq!(held.name(), "heartbeat");
    assert_eq!(held.fence(), fence);

    // Each renewal pushes the remaining time back up by about a second.
    let mut renewals = 0;
    let mut previous = ttl;
    await_until!(
        {
            let info = a.info("heartbeat").await.unwrap().expect("still held");
            assert_eq!(info.fence, fence);
            if info.remaining > previous + Duration::from_millis(300) {
                renewals += 1;
            }
            previous = info.remaining;
            renewals >= 2
        },
        timeout = GUARD,
        interval = Duration::from_millis(100),
    );
    assert!(!held.is_lost());

    // Dropping it releases in the background.
    drop(held);
    await_until!(
        a.info("heartbeat").await.unwrap().is_none(),
        timeout = GUARD
    );
}

async fn a_stolen_lease_fires_lost(pool: &Pool) {
    let a = store(pool, "node-a").await;
    let lease = a
        .try_acquire("stolen", Duration::from_secs(3))
        .await
        .unwrap()
        .unwrap();
    let held = lease.keep_alive();
    steal(pool, "stolen").await;
    tokio::time::timeout(GUARD, held.lost().cancelled())
        .await
        .expect("the next renewal notices the theft");
    assert!(held.is_lost());
    held.release().await.unwrap();
    // The thief's row is untouched by the release.
    assert!(a.info("stolen").await.unwrap().is_some());
}

/// Starts an acquisition of `abandoned`, which waits for the row lock, and
/// drops it mid-transaction.
async fn abandon(store: &LeaseStore) {
    let abandoned =
        tokio::time::timeout(Duration::from_secs(1), store.try_acquire("abandoned", TTL)).await;
    assert!(abandoned.is_err(), "the acquisition waits for the row lock");
}

async fn an_abandoned_acquisition_leaves_the_lease_free(pool: &Pool) {
    let a = store(pool, "node-a").await;
    let b = store(pool, "node-b").await;
    a.try_acquire("abandoned", TTL)
        .await
        .unwrap()
        .unwrap()
        .release()
        .await
        .unwrap();

    let lock = AssertSqlSafe("SELECT name FROM sekvent_leases WHERE name = 'abandoned' FOR UPDATE");
    if let Some(pg) = pool.postgres() {
        let mut blocker = pg.begin().await.unwrap();
        sqlx::raw_sql(lock).execute(&mut *blocker).await.unwrap();
        abandon(&a).await;
        blocker.commit().await.unwrap();
    } else {
        let mut blocker = pool.mysql().unwrap().begin().await.unwrap();
        sqlx::raw_sql(lock).execute(&mut *blocker).await.unwrap();
        abandon(&a).await;
        blocker.commit().await.unwrap();
    }

    // The abandoned UPDATE ran once the lock was free, but inside a
    // transaction that never committed: no owner nobody knows holds the row.
    let lease = tokio::time::timeout(GUARD, b.try_acquire("abandoned", TTL))
        .await
        .expect("the abandoned transaction ends")
        .unwrap();
    assert!(
        lease.is_some(),
        "the lease must be free after an abandoned acquisition"
    );
}

macro_rules! on_both_backends {
    ($($scenario:ident),* $(,)?) => {
        mod postgres {
            $(
                #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
                #[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
                async fn $scenario() {
                    if super::skip() {
                        return;
                    }
                    super::$scenario(&super::postgres_pool().await).await;
                }
            )*
        }

        mod mysql {
            $(
                #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
                #[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
                async fn $scenario() {
                    if super::skip() {
                        return;
                    }
                    super::$scenario(&super::mysql_pool().await).await;
                }
            )*
        }
    };
}

on_both_backends!(
    schema_is_created_once_and_verified,
    a_held_lease_excludes_and_fences_grow,
    a_lease_renewed_after_its_expiry_is_lost,
    each_tick_runs_once,
    fences_are_checked_inside_a_transaction,
    racing_acquirers_get_exactly_one_lease,
    keep_alive_holds_the_lease_across_renewals,
    a_stolen_lease_fires_lost,
    an_abandoned_acquisition_leaves_the_lease_free,
);

#[cfg(feature = "runtime")]
mod singleton_jobs {
    use sekvent_db::LeaseGuard;
    use sekvent_runtime::{
        CancelReason, JobContext, JobHandle, JobSpec, Runtime, RuntimeHandle, Stage,
        TriggerErrorKind,
    };
    use tokio::sync::mpsc;

    use super::*;

    async fn start(builder: sekvent_runtime::RuntimeBuilder) -> RuntimeHandle {
        tokio::time::timeout(GUARD, builder.build().unwrap().start())
            .await
            .unwrap()
            .unwrap()
    }

    async fn stop(handle: RuntimeHandle) {
        handle.shutdown();
        tokio::time::timeout(GUARD, handle.wait())
            .await
            .unwrap()
            .unwrap();
    }

    /// One instance of a service running the singleton `orders-sync` job.
    fn instance(
        store: &LeaseStore,
        node: &'static str,
        runs: mpsc::UnboundedSender<(&'static str, SystemTime, u64)>,
    ) -> sekvent_runtime::RuntimeBuilder {
        let guard = LeaseGuard::new(store.clone().with_holder(node).unwrap())
            .with_ttl(Duration::from_secs(5))
            .unwrap();
        let spec = JobSpec::interval(Duration::from_secs(1)).singleton(guard);
        Runtime::builder().without_signals().job(
            "orders-sync",
            Stage::Workers,
            spec,
            move |cx: JobContext| {
                let runs = runs.clone();
                async move {
                    let tick = cx.tick().expect("scheduled runs carry their tick");
                    let fence = cx.fence().expect("singleton runs carry a fence");
                    runs.send((node, tick, fence)).unwrap();
                    Ok::<(), AppError>(())
                }
            },
        )
    }

    async fn two_instances_run_each_tick_once(pool: &Pool) {
        let store = store(pool, "setup").await;
        let (runs_tx, mut runs) = mpsc::unbounded_channel();
        let a = start(instance(&store, "node-a", runs_tx.clone())).await;
        let b = start(instance(&store, "node-b", runs_tx)).await;

        let mut seen = Vec::new();
        while seen.len() < 4 {
            let run = tokio::time::timeout(GUARD, runs.recv())
                .await
                .unwrap()
                .unwrap();
            seen.push(run);
        }
        stop(a).await;
        stop(b).await;

        for pair in seen.windows(2) {
            let ((_, earlier_tick, earlier_fence), (_, later_tick, later_fence)) =
                (pair[0], pair[1]);
            assert!(later_tick > earlier_tick, "each tick runs once: {seen:?}");
            assert!(later_fence > earlier_fence, "fences grow: {seen:?}");
        }
        for (_, tick, _) in &seen {
            let since = tick.duration_since(UNIX_EPOCH).unwrap();
            assert_eq!(
                since.subsec_nanos(),
                0,
                "ticks sit on the epoch-aligned grid"
            );
        }
    }

    fn manual(store: &LeaseStore, lease: &str, ttl: Duration) -> (JobSpec, JobHandle) {
        let guard = LeaseGuard::new(store.clone())
            .with_ttl(ttl)
            .unwrap()
            .with_lease_name(lease)
            .unwrap();
        let spec = JobSpec::manual().singleton(guard);
        let handle = spec.handle();
        (spec, handle)
    }

    async fn a_trigger_while_held_elsewhere_is_refused(pool: &Pool) {
        let store = store(pool, "node-a").await;
        let (spec, job) = manual(&store, "orders-full-sync", TTL);
        let (done_tx, mut done) = mpsc::unbounded_channel();
        let runtime = start(Runtime::builder().without_signals().job(
            "orders-full",
            Stage::Workers,
            spec,
            move |cx: JobContext| {
                let done = done_tx.clone();
                async move {
                    done.send(cx.fence()).unwrap();
                    Ok::<(), AppError>(())
                }
            },
        ))
        .await;

        let direct = store
            .try_acquire("orders-full-sync", TTL)
            .await
            .unwrap()
            .unwrap();
        let refused = job.trigger().await.unwrap_err();
        assert_eq!(refused.kind(), TriggerErrorKind::HeldElsewhere);

        let direct_fence = direct.fence();
        direct.release().await.unwrap();
        let started = job.trigger().await.unwrap();
        let fence = started.fence.expect("a singleton run has a fence");
        assert!(fence > direct_fence.get());
        let reported = tokio::time::timeout(GUARD, done.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(reported, Some(fence));
        // Manual runs never touch the tick record.
        assert_eq!(store.last_tick("orders-full-sync").await.unwrap(), None);
        stop(runtime).await;
    }

    /// Reports the run's cancel reason when the runtime drops the run.
    struct OnDrop {
        cx: JobContext,
        reasons: mpsc::UnboundedSender<Option<CancelReason>>,
    }

    impl Drop for OnDrop {
        fn drop(&mut self) {
            let _ = self.reasons.send(self.cx.cancel_reason());
        }
    }

    async fn a_run_whose_lease_is_stolen_is_cancelled(pool: &Pool) {
        let store = store(pool, "node-a").await;
        let (spec, job) = manual(&store, "orders-steal", Duration::from_secs(3));
        let (reasons_tx, mut reasons) = mpsc::unbounded_channel();
        let runtime = start(Runtime::builder().without_signals().job(
            "orders-steal",
            Stage::Workers,
            spec,
            move |cx: JobContext| {
                let reasons = reasons_tx.clone();
                async move {
                    let _report = OnDrop { cx, reasons };
                    std::future::pending::<()>().await;
                    Ok::<(), AppError>(())
                }
            },
        ))
        .await;

        job.trigger().await.unwrap();
        steal(pool, "orders-steal").await;
        let reason = tokio::time::timeout(GUARD, reasons.recv())
            .await
            .expect("the run is cancelled once the theft is noticed")
            .unwrap();
        assert_eq!(reason, Some(CancelReason::LeaseLost));
        await_until!(job.status().last.is_some(), timeout = GUARD);
        let last = job.status().last.unwrap();
        assert_eq!(last.error, Some(ErrorCode::Aborted));
        stop(runtime).await;
    }

    macro_rules! on_both_backends_with_runtime {
        ($($scenario:ident),* $(,)?) => {
            mod postgres {
                $(
                    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
                    #[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
                    async fn $scenario() {
                        if super::super::skip() {
                            return;
                        }
                        super::$scenario(&super::super::postgres_pool().await).await;
                    }
                )*
            }

            mod mysql {
                $(
                    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
                    #[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
                    async fn $scenario() {
                        if super::super::skip() {
                            return;
                        }
                        super::$scenario(&super::super::mysql_pool().await).await;
                    }
                )*
            }
        };
    }

    on_both_backends_with_runtime!(
        two_instances_run_each_tick_once,
        a_trigger_while_held_elsewhere_is_refused,
        a_run_whose_lease_is_stolen_is_cancelled,
    );
}
