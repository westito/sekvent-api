//! `KeyedPools` against a real Postgres server, and the parts that need
//! none.
//!
//! ```sh
//! SEKVENT_DOCKER_TESTS=1 cargo test -p sekvent-db --all-features --test keyed_pools -- --include-ignored
//! ```
//!
//! Without `SEKVENT_DOCKER_TESTS=1` the Docker tests return immediately.
//! Roles are server-wide, so each test derives their names from its own
//! freshly created database. This file is its own test binary, so it
//! installs a global subscriber that keeps every callsite's interest open
//! and records per thread.

#![cfg(all(feature = "keyed", feature = "sqlx-postgres"))]

use std::collections::BTreeMap;
use std::fmt;
use std::future::Future;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, Once};
use std::time::Duration;

use sekvent_db::reasons::{DB_CREDENTIALS_REJECTED, KEYED_CREATE_TIMEOUT, KEYED_FACTORY_PANICKED};
use sekvent_db::{ConnectCandidates, KeyedPools, to_app_error};
use sekvent_error::{AppError, ErrorCode};
use sekvent_testing::{PostgresHarness, TestDatabase, docker_tests_enabled};
use sqlx::postgres::PgConnectOptions;
use sqlx::{AssertSqlSafe, Connection, PgConnection, PgPool};
use tokio::sync::mpsc;
use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id, Record};
use tracing::subscriber::{Interest, Subscriber};
use tracing::{Event, Level, Metadata};

const PASSWORD: &str = "Granted7pass";
const WRONG: &str = "Wrong7pass";
const OTHER_WRONG: &str = "Rotated7pass";
const GUARD: Duration = Duration::from_secs(30);

fn skip() -> bool {
    if docker_tests_enabled() {
        return false;
    }
    eprintln!("skipped: set SEKVENT_DOCKER_TESTS=1 to run Docker-backed tests");
    true
}

/// Runs one scenario under the file's 30 s bound.
async fn bounded<F: Future>(scenario: F) -> F::Output {
    tokio::time::timeout(GUARD, scenario)
        .await
        .expect("the scenario finished in time")
}

/// A role name unique to `database`.
fn role_for(database: &TestDatabase, prefix: char) -> String {
    let suffix: String = database.name.chars().rev().take(20).collect();
    format!("{prefix}{suffix}")
}

/// A test database with a role that may log in and one that may not, both
/// with [`PASSWORD`].
struct Fixture {
    database: TestDatabase,
    role: String,
    no_login: String,
}

impl Fixture {
    async fn new() -> Self {
        let server = PostgresHarness::shared().await;
        let database = server.create_database().await.unwrap();
        let role = role_for(&database, 'u');
        let no_login = role_for(&database, 'n');
        let mut admin = PgConnection::connect(&database.url).await.unwrap();
        for statement in [
            format!("CREATE ROLE {role} LOGIN PASSWORD '{PASSWORD}'"),
            format!("CREATE ROLE {no_login} NOLOGIN PASSWORD '{PASSWORD}'"),
        ] {
            sqlx::raw_sql(AssertSqlSafe(statement))
                .execute(&mut admin)
                .await
                .unwrap();
        }
        admin.close().await.unwrap();
        Self {
            database,
            role,
            no_login,
        }
    }

    /// The harness's own (superuser) options for this database.
    fn admin(&self) -> PgConnectOptions {
        self.database.url.parse().unwrap()
    }

    fn as_role(&self, role: &str, password: &str) -> PgConnectOptions {
        self.admin().username(role).password(password)
    }
}

async fn current_user(pool: &PgPool) -> String {
    sqlx::query_scalar("SELECT current_user::text")
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn current_database(pool: &PgPool) -> String {
    sqlx::query_scalar("SELECT current_database()::text")
        .fetch_one(pool)
        .await
        .unwrap()
}

/// A cache of one fixed candidate list.
fn candidates_cache<F>(candidates: F) -> KeyedPools<u32>
where
    F: Fn() -> ConnectCandidates + Send + Sync + 'static,
{
    KeyedPools::builder(move |_key: u32| {
        let candidates = candidates();
        async move { Ok(candidates) }
    })
    .build()
    .unwrap()
}

/// Waits until `pool` is closed; a pool that was only dropped while this
/// clone lives never is.
async fn closed(pool: &PgPool) {
    tokio::time::timeout(GUARD, pool.close_event())
        .await
        .expect("the pool was closed");
    assert!(pool.is_closed());
}

#[tokio::test]
#[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
async fn a_pool_per_database_answers_with_its_database() {
    if skip() {
        return;
    }
    bounded(async {
        let server = PostgresHarness::shared().await;
        let first = server.create_database().await.unwrap();
        let second = server.create_database().await.unwrap();
        let admin: PgConnectOptions = first.url.parse().unwrap();
        let pools = KeyedPools::builder(move |name: String| {
            let options = admin.clone().database(&name);
            async move { Ok(options) }
        })
        .build()
        .unwrap();

        for database in [&first, &second] {
            let pool = pools.get(&database.name).await.unwrap();
            assert_eq!(current_database(&pool).await, database.name);
        }
        assert_eq!(pools.stats().warm, 2);
        pools.close().await;
    })
    .await;
}

/// What `build_value` makes: the pool and whether `init` had run.
struct Tenant {
    pool: PgPool,
    initialised: bool,
}

#[tokio::test]
#[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
async fn init_and_build_value_run_once_before_publication() {
    if skip() {
        return;
    }
    bounded(async {
        let fixture = Fixture::new().await;
        let admin = fixture.admin();
        let inits = Arc::new(AtomicUsize::new(0));
        let builds = Arc::new(AtomicUsize::new(0));
        let (init_count, build_count) = (Arc::clone(&inits), Arc::clone(&builds));
        let pools = KeyedPools::builder(move |_key: u32| {
            let options = admin.clone();
            async move { Ok(options) }
        })
        .init(move |_key, pool| {
            init_count.fetch_add(1, Ordering::SeqCst);
            async move {
                sqlx::raw_sql("CREATE TABLE marker (id INT PRIMARY KEY)")
                    .execute(&pool)
                    .await
                    .map_err(to_app_error)?;
                Ok::<(), AppError>(())
            }
        })
        .build_value(move |_key, pool| {
            build_count.fetch_add(1, Ordering::SeqCst);
            async move {
                let initialised: bool =
                    sqlx::query_scalar("SELECT to_regclass('marker') IS NOT NULL")
                        .fetch_one(&pool)
                        .await
                        .map_err(to_app_error)?;
                Ok::<_, AppError>(Tenant { pool, initialised })
            }
        })
        .build()
        .unwrap();

        let callers: Vec<_> = (0..8)
            .map(|_| {
                let pools = pools.clone();
                tokio::spawn(async move { pools.get(&1).await })
            })
            .collect();
        for caller in callers {
            let tenant = caller.await.unwrap().unwrap();
            assert!(tenant.initialised);
            sqlx::query("SELECT id FROM marker")
                .fetch_all(&tenant.pool)
                .await
                .unwrap();
        }
        assert_eq!(inits.load(Ordering::SeqCst), 1);
        assert_eq!(builds.load(Ordering::SeqCst), 1);
        assert_eq!(pools.stats().creations, 1);
        pools.close().await;
    })
    .await;
}

#[tokio::test]
#[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
async fn a_failing_init_or_build_value_publishes_nothing_and_closes_the_pool() {
    if skip() {
        return;
    }
    bounded(async {
        let fixture = Fixture::new().await;

        let admin = fixture.admin();
        let (handed, mut pools_seen) = mpsc::unbounded_channel::<PgPool>();
        let calls = Arc::new(AtomicUsize::new(0));
        let init_calls = Arc::clone(&calls);
        let failing_init = KeyedPools::builder(move |_key: u32| {
            let options = admin.clone();
            async move { Ok(options) }
        })
        .init(move |_key, pool| {
            let first = init_calls.fetch_add(1, Ordering::SeqCst) == 0;
            let _ = handed.send(pool);
            async move {
                if first {
                    Err(AppError::unavailable("init failed").with_reason("INIT_FAILED"))
                } else {
                    Ok(())
                }
            }
        })
        .build()
        .unwrap();
        let error = failing_init.get(&1).await.unwrap_err();
        assert_eq!(error.reason(), Some("INIT_FAILED"));
        closed(&pools_seen.recv().await.unwrap()).await;
        assert!(failing_init.peek(&1).is_none());
        assert_eq!(failing_init.stats().warm, 0);
        let pool = failing_init.get(&1).await.unwrap();
        assert_eq!(current_database(&pool).await, fixture.database.name);
        drop(pool);
        failing_init.close().await;

        let admin = fixture.admin();
        let (handed, mut pools_seen) = mpsc::unbounded_channel::<PgPool>();
        let calls = Arc::new(AtomicUsize::new(0));
        let build_calls = Arc::clone(&calls);
        let failing_build = KeyedPools::builder(move |_key: u32| {
            let options = admin.clone();
            async move { Ok(options) }
        })
        .build_value(move |_key, pool| {
            let first = build_calls.fetch_add(1, Ordering::SeqCst) == 0;
            let _ = handed.send(pool.clone());
            async move {
                if first {
                    Err(AppError::unavailable("build failed").with_reason("BUILD_FAILED"))
                } else {
                    Ok(pool)
                }
            }
        })
        .build()
        .unwrap();
        let error = failing_build.get(&1).await.unwrap_err();
        assert_eq!(error.reason(), Some("BUILD_FAILED"));
        closed(&pools_seen.recv().await.unwrap()).await;
        assert_eq!(failing_build.stats().warm, 0);
        let pool = failing_build.get(&1).await.unwrap();
        assert_eq!(current_database(&pool).await, fixture.database.name);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        drop(pool);
        failing_build.close().await;
    })
    .await;
}

#[tokio::test]
#[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
async fn the_first_candidate_that_connects_is_used() {
    if skip() {
        return;
    }
    let (events, _guard) = record();
    bounded(async {
        let fixture = Arc::new(Fixture::new().await);
        let source = Arc::clone(&fixture);
        let pools = candidates_cache(move || {
            ConnectCandidates::new(source.as_role(&source.role, PASSWORD)).or(source.admin())
        });
        let pool = pools.get(&1).await.unwrap();
        assert_eq!(current_user(&pool).await, fixture.role);
        drop(pool);
        pools.close().await;
    })
    .await;
    assert_eq!(infos(&events), [] as [BTreeMap<String, String>; 0]);
}

#[tokio::test]
#[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
async fn a_rejected_password_falls_back_to_the_next_candidate() {
    if skip() {
        return;
    }
    let (events, _guard) = record();
    bounded(async {
        let fixture = Arc::new(Fixture::new().await);
        let source = Arc::clone(&fixture);
        let pools = candidates_cache(move || {
            ConnectCandidates::new(source.as_role(&source.role, WRONG))
                .or(source.as_role(&source.role, PASSWORD))
        });
        let pool = pools.get(&1).await.unwrap();
        assert_eq!(current_user(&pool).await, fixture.role);
        drop(pool);
        pools.close().await;
    })
    .await;
    let infos = infos(&events);
    assert_eq!(infos.len(), 1, "{infos:?}");
    assert_eq!(infos[0].get("candidate").map(String::as_str), Some("2"));
    assert_eq!(infos[0].get("candidates").map(String::as_str), Some("2"));
    assert!(!mentions(&infos[0], PASSWORD));
    assert!(!mentions(&infos[0], WRONG));
}

#[tokio::test]
#[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
async fn a_missing_database_does_not_fall_back() {
    if skip() {
        return;
    }
    let (events, _guard) = record();
    bounded(async {
        let fixture = Arc::new(Fixture::new().await);
        let source = Arc::clone(&fixture);
        let missing = format!("{}_missing", fixture.database.name);
        let pools = candidates_cache(move || {
            ConnectCandidates::new(source.admin().database(&missing))
                .or(source.as_role(&source.role, PASSWORD))
        });
        let error = pools.get(&1).await.unwrap_err();
        assert_eq!(error.code(), ErrorCode::Internal);
        assert_eq!(error.message(), "internal error");
        assert_eq!(error.reason(), None);
        assert_eq!(pools.stats().warm, 0);
        pools.close().await;
    })
    .await;
    let warnings = warnings(&events);
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    let source = &warnings[0]["source"];
    assert!(source.contains("3D000"), "{source}");
    assert!(source.contains("1 of 2"), "{source}");
}

#[tokio::test]
#[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
async fn every_rejected_password_is_credentials_rejected() {
    if skip() {
        return;
    }
    let (events, _guard) = record();
    bounded(async {
        let fixture = Arc::new(Fixture::new().await);
        let source = Arc::clone(&fixture);
        let pools = candidates_cache(move || {
            ConnectCandidates::new(source.as_role(&source.role, WRONG))
                .or(source.as_role(&source.role, OTHER_WRONG))
        });
        let error = pools.get(&1).await.unwrap_err();
        assert_eq!(error.code(), ErrorCode::Internal);
        assert_eq!(error.message(), "internal error");
        assert_eq!(error.reason(), Some(DB_CREDENTIALS_REJECTED));
        for text in [format!("{error:?}"), format!("{pools:?}")] {
            assert!(!text.contains(WRONG), "{text}");
            assert!(!text.contains(OTHER_WRONG), "{text}");
        }
        assert!(pools.peek(&1).is_none());
        assert!(pools.get(&1).await.is_err());
        let stats = pools.stats();
        assert_eq!(
            (stats.warm, stats.creations, stats.create_failures),
            (0, 2, 2)
        );
        pools.close().await;
    })
    .await;
    let warnings = warnings(&events);
    assert_eq!(warnings.len(), 2, "{warnings:?}");
    for fields in &warnings {
        let source = &fields["source"];
        assert!(source.contains("auth"), "{source}");
        assert!(source.contains("28P01"), "{source}");
        assert!(source.contains("2 of 2"), "{source}");
        assert!(!mentions(fields, WRONG));
        assert!(!mentions(fields, OTHER_WRONG));
    }
}

#[tokio::test]
#[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
async fn a_role_that_may_not_log_in_does_not_fall_back() {
    if skip() {
        return;
    }
    let (events, _guard) = record();
    bounded(async {
        let fixture = Arc::new(Fixture::new().await);
        let source = Arc::clone(&fixture);
        let pools = candidates_cache(move || {
            ConnectCandidates::new(source.as_role(&source.no_login, PASSWORD))
                .or(source.as_role(&source.role, PASSWORD))
        });
        let error = pools.get(&1).await.unwrap_err();
        assert_eq!(error.code(), ErrorCode::Internal);
        assert_eq!(error.reason(), Some(DB_CREDENTIALS_REJECTED));
        pools.close().await;
    })
    .await;
    let warnings = warnings(&events);
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    let source = &warnings[0]["source"];
    assert!(source.contains("28000"), "{source}");
    assert!(source.contains("1 of 2"), "{source}");
    assert_eq!(infos(&events), [] as [BTreeMap<String, String>; 0]);
}

#[tokio::test]
#[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
async fn an_init_past_the_deadline_closes_its_pool() {
    if skip() {
        return;
    }
    bounded(async {
        let fixture = Fixture::new().await;
        let admin = fixture.admin();
        let (handed, mut pools_seen) = mpsc::unbounded_channel::<PgPool>();
        let pools = KeyedPools::builder(move |_key: u32| {
            let options = admin.clone();
            async move { Ok(options) }
        })
        .acquire_timeout(Duration::from_secs(2))
        .create_timeout(Duration::from_secs(3))
        .init(move |_key, pool| {
            let _ = handed.send(pool);
            std::future::pending::<Result<(), AppError>>()
        })
        .build()
        .unwrap();
        let error = pools.get(&1).await.unwrap_err();
        assert_eq!(error.reason(), Some(KEYED_CREATE_TIMEOUT));
        closed(&pools_seen.recv().await.unwrap()).await;
        pools.close().await;
        assert_eq!(pools.stats().retiring, 0);
    })
    .await;
}

fn explode() -> Result<PgPool, AppError> {
    panic!("build_value failed")
}

#[tokio::test]
#[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
async fn a_panicking_build_value_closes_its_pool() {
    if skip() {
        return;
    }
    bounded(async {
        let fixture = Fixture::new().await;
        let admin = fixture.admin();
        let (handed, mut pools_seen) = mpsc::unbounded_channel::<PgPool>();
        let pools = KeyedPools::builder(move |_key: u32| {
            let options = admin.clone();
            async move { Ok(options) }
        })
        .build_value(move |_key, pool| {
            let _ = handed.send(pool);
            async move { explode() }
        })
        .build()
        .unwrap();
        let error = pools.get(&1).await.unwrap_err();
        assert_eq!(error.reason(), Some(KEYED_FACTORY_PANICKED));
        closed(&pools_seen.recv().await.unwrap()).await;
        pools.close().await;
    })
    .await;
}

#[tokio::test]
#[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
async fn eviction_and_invalidation_close_the_pool_once_unreferenced() {
    if skip() {
        return;
    }
    bounded(async {
        let server = PostgresHarness::shared().await;
        let first = server.create_database().await.unwrap();
        let second = server.create_database().await.unwrap();
        let admin: PgConnectOptions = first.url.parse().unwrap();
        let pools = KeyedPools::builder(move |name: String| {
            let options = admin.clone().database(&name);
            async move { Ok(options) }
        })
        .capacity(1)
        .build()
        .unwrap();

        // `peek` never creates.
        assert!(pools.peek(&first.name).is_none());
        assert_eq!(pools.stats().creations, 0);

        // A capacity eviction.
        let reference = pools.get(&first.name).await.unwrap();
        let evicted = (*reference).clone();
        assert!(pools.peek(&first.name).is_some());
        drop(reference);
        let reference = pools.get(&second.name).await.unwrap();
        closed(&evicted).await;

        // An idle eviction.
        let idle = (*reference).clone();
        drop(reference);
        assert_eq!(pools.evict_idle_older_than(Duration::ZERO).await, 1);
        closed(&idle).await;

        // An invalidation with a reference in flight.
        let reference = pools.get(&first.name).await.unwrap();
        let invalidated = (*reference).clone();
        assert!(pools.invalidate(&first.name).await);
        assert_eq!(current_database(&reference).await, first.name);
        assert!(!invalidated.is_closed());
        assert_eq!(pools.stats().retiring, 1);
        drop(reference);
        closed(&invalidated).await;
        pools.close().await;
        assert_eq!(pools.stats().retiring, 0);
    })
    .await;
}

/// A value that reports, when it is dropped, whether its pool was closed.
struct Reporting {
    pool: PgPool,
    dropped: mpsc::UnboundedSender<bool>,
}

impl Drop for Reporting {
    fn drop(&mut self) {
        let _ = self.dropped.send(self.pool.is_closed());
    }
}

#[tokio::test]
#[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
async fn a_built_value_is_dropped_before_its_pool_closes() {
    if skip() {
        return;
    }
    bounded(async {
        let fixture = Fixture::new().await;
        let admin = fixture.admin();
        let (dropped, mut drops) = mpsc::unbounded_channel();
        let pools = KeyedPools::builder(move |_key: u32| {
            let options = admin.clone();
            async move { Ok(options) }
        })
        .build_value(move |_key, pool| {
            let dropped = dropped.clone();
            async move { Ok(Reporting { pool, dropped }) }
        })
        .build()
        .unwrap();
        let reference = pools.get(&1).await.unwrap();
        let pool = reference.pool.clone();
        drop(reference);
        assert!(pools.invalidate(&1).await);
        assert_eq!(drops.recv().await, Some(false));
        closed(&pool).await;
        pools.close().await;
    })
    .await;
}

#[tokio::test]
async fn a_connector_error_passes_through() {
    let pools = KeyedPools::builder(|_key: u32| async {
        Err::<PgConnectOptions, _>(
            AppError::new(ErrorCode::NotFound, "no such tenant").with_reason("TENANT_UNKNOWN"),
        )
    })
    .build()
    .unwrap();
    let error = pools.get(&1).await.unwrap_err();
    assert_eq!(error.code(), ErrorCode::NotFound);
    assert_eq!(error.message(), "no such tenant");
    assert_eq!(error.reason(), Some("TENANT_UNKNOWN"));
    assert_eq!(pools.stats().warm, 0);
}

#[tokio::test]
async fn an_unreachable_server_is_unavailable_and_retried() {
    let options = PgConnectOptions::new()
        .host("127.0.0.1")
        .port(1)
        .username("nobody")
        .password("Unused7pass")
        .database("nothing");
    let pools = KeyedPools::builder(move |_key: u32| {
        let options = options.clone();
        async move { Ok(options) }
    })
    .acquire_timeout(Duration::from_millis(300))
    .build()
    .unwrap();
    for creations in 1..=2 {
        let error = bounded(pools.get(&1)).await.unwrap_err();
        assert_eq!(error.code(), ErrorCode::Unavailable);
        assert_eq!(error.message(), "database unavailable");
        assert_eq!(error.reason(), None);
        assert_eq!(pools.stats().creations, creations);
    }
}

#[test]
fn candidates_debug_shows_only_their_count() {
    let options = PgConnectOptions::new()
        .host("db.internal.example")
        .username("tenant")
        .password("Hidden7pass");
    let one = ConnectCandidates::from(options.clone());
    assert_eq!(format!("{one:?}"), "ConnectCandidates { count: 1 }");
    let two = ConnectCandidates::new(options.clone()).or(options);
    let debug = format!("{two:?}");
    assert_eq!(debug, "ConnectCandidates { count: 2 }");
    assert!(!debug.contains("Hidden7pass"));
}

#[tokio::test]
async fn build_refuses_zero_pool_settings() {
    let builder = || {
        KeyedPools::builder(|_key: u32| async {
            Ok(PgConnectOptions::new().host("127.0.0.1").port(1))
        })
    };
    for (error, setting) in [
        (builder().max_connections(0).build(), "max_connections"),
        (
            builder().acquire_timeout(Duration::ZERO).build(),
            "acquire_timeout",
        ),
        (
            builder()
                .connection_idle_timeout(Some(Duration::ZERO))
                .build(),
            "connection_idle_timeout",
        ),
        (
            builder()
                .connection_max_lifetime(Some(Duration::ZERO))
                .build(),
            "connection_max_lifetime",
        ),
        (builder().capacity(0).build(), "capacity"),
    ] {
        let error = error.unwrap_err();
        assert_eq!(error.code(), ErrorCode::InvalidArgument, "{setting}");
        assert!(error.message().contains(setting), "{}", error.message());
    }
}

#[cfg(feature = "runtime")]
#[tokio::test]
#[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
async fn the_unit_closes_the_pools_on_shutdown() {
    use sekvent_runtime::{Runtime, Stage, UnitExit, UnitPolicy};

    if skip() {
        return;
    }
    bounded(async {
        let fixture = Fixture::new().await;
        let admin = fixture.admin();
        let pools = KeyedPools::builder(move |_key: u32| {
            let options = admin.clone();
            async move { Ok(options) }
        })
        .build()
        .unwrap();
        let handle = Runtime::builder()
            .without_signals()
            .unit(
                "tenant-pools",
                Stage::Infrastructure,
                UnitPolicy::Critical,
                pools.clone().into_unit(),
            )
            .build()
            .unwrap()
            .start()
            .await
            .unwrap();

        let reference = pools.get(&1).await.unwrap();
        let pool = (*reference).clone();
        drop(reference);
        handle.shutdown();
        let report = handle.wait().await.unwrap();
        assert_eq!(
            report.unit("tenant-pools").unwrap().exit,
            UnitExit::Completed
        );
        assert!(pools.is_closed());
        assert!(pool.is_closed());
        assert_eq!(pools.stats().retiring, 0);
    })
    .await;
}

/// One recorded event.
struct Captured {
    level: Level,
    fields: BTreeMap<String, String>,
}

/// Records events when `events` is set; the global default (no `events`)
/// records nothing but keeps every callsite's interest open.
struct Recorder {
    events: Option<Arc<Mutex<Vec<Captured>>>>,
}

impl Subscriber for Recorder {
    fn register_callsite(&self, _metadata: &'static Metadata<'static>) -> Interest {
        Interest::sometimes()
    }

    fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        self.events.is_some() && metadata.target().starts_with("sekvent_db")
    }

    fn new_span(&self, _span: &Attributes<'_>) -> Id {
        Id::from_u64(1)
    }

    fn record(&self, _span: &Id, _values: &Record<'_>) {}

    fn record_follows_from(&self, _span: &Id, _follows: &Id) {}

    fn event(&self, event: &Event<'_>) {
        let mut fields = Fields::default();
        event.record(&mut fields);
        if let Some(events) = &self.events {
            events.lock().unwrap().push(Captured {
                level: *event.metadata().level(),
                fields: fields.0,
            });
        }
    }

    fn enter(&self, _span: &Id) {}

    fn exit(&self, _span: &Id) {}
}

#[derive(Default)]
struct Fields(BTreeMap<String, String>);

impl Visit for Fields {
    fn record_str(&mut self, field: &Field, value: &str) {
        self.0.insert(field.name().to_owned(), value.to_owned());
    }

    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        self.0.insert(field.name().to_owned(), format!("{value:?}"));
    }
}

/// Starts recording this thread's `sekvent_db` events until the guard
/// drops.
fn record() -> (Arc<Mutex<Vec<Captured>>>, tracing::subscriber::DefaultGuard) {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        tracing::subscriber::set_global_default(Recorder { events: None })
            .expect("no other test installs a global subscriber");
    });
    let events = Arc::new(Mutex::new(Vec::new()));
    let guard = tracing::subscriber::set_default(Recorder {
        events: Some(Arc::clone(&events)),
    });
    (events, guard)
}

fn at_level(events: &Mutex<Vec<Captured>>, level: Level) -> Vec<BTreeMap<String, String>> {
    events
        .lock()
        .unwrap()
        .iter()
        .filter(|event| event.level == level)
        .map(|event| event.fields.clone())
        .collect()
}

fn warnings(events: &Mutex<Vec<Captured>>) -> Vec<BTreeMap<String, String>> {
    at_level(events, Level::WARN)
}

fn infos(events: &Mutex<Vec<Captured>>) -> Vec<BTreeMap<String, String>> {
    at_level(events, Level::INFO)
}

fn mentions(fields: &BTreeMap<String, String>, text: &str) -> bool {
    fields.values().any(|value| value.contains(text))
}
