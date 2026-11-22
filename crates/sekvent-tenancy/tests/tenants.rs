//! The tenant cache on a real server: the state table, cold opens, the product's refusals, the
//! log surface of a failed open, the stored-row check, invalidation, capacity, isolation,
//! selection, the walks and the migration sweep.
//!
//! This file is its own test binary: one test installs a global subscriber that keeps every
//! callsite's interest open and records the events of its own thread.
#![cfg(all(feature = "serving", feature = "lifecycle"))]

mod common;

use std::collections::BTreeMap;
use std::error::Error as StdError;
use std::fmt;
use std::pin::pin;
use std::str::FromStr;
use std::sync::{Arc, Mutex, Once};
use std::time::Duration;

use common::Cluster;
use common::product::{LATEST_TENANT_VERSION, OrdersFactory, OrdersScope};
use sekvent_component::ScopeResolver;
use sekvent_context::CallContext;
use sekvent_db::reasons::{DB_CREDENTIALS_REJECTED, DB_PERMISSION_DENIED, KEYED_CAPACITY};
use sekvent_error::{AppError, ErrorCode, FieldViolation};
use sekvent_tenancy::{
    ErrorDomains, Registry, Selector, SelectorRules, StreamTenant, TenantId, TenantResolver,
    TenantRow, TenantState, Tenants, reasons,
};
use sqlx::PgPool;
use sqlx::postgres::PgConnectOptions;
use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id, Record};
use tracing::subscriber::{Interest, Subscriber};
use tracing::{Event, Level, Metadata};

/// The header the id-header hosts of these tests read.
const TENANT_HEADER: &str = "x-tenant-id";

/// What every test starts from: a registry on a frozen clock, its control pool, a factory.
struct Fixture {
    registry: Registry,
    control: PgPool,
    factory: OrdersFactory,
}

async fn fixture(cluster: &Cluster) -> Fixture {
    let (registry, control) = cluster.registry(Arc::new(common::manual_clock())).await;
    Fixture {
        registry,
        control,
        factory: OrdersFactory::new(),
    }
}

impl Fixture {
    /// A cache over the test product with `capacity` and no selector rules.
    fn tenants(&self, cluster: &Cluster, capacity: usize) -> Tenants<OrdersScope> {
        cluster.tenants(
            &self.registry,
            SelectorRules::new(),
            &self.factory,
            capacity,
        )
    }

    /// A whole active tenant whose one host is `<label>.example.com`.
    async fn tenant(&self, cluster: &Cluster, label: &str) -> TenantRow {
        let host = format!("{label}.example.com");
        cluster
            .create_tenant(&self.control, label, &[host.as_str()])
            .await
    }
}

fn role_of(cluster: &Cluster, id: TenantId) -> String {
    cluster.names().role(id).as_str().to_owned()
}

fn database_of(cluster: &Cluster, id: TenantId) -> String {
    cluster.names().database(id).as_str().to_owned()
}

#[track_caller]
fn assert_serving(error: &AppError, code: ErrorCode, reason: &str) {
    assert_eq!(error.reason(), Some(reason), "{:?}", error.code());
    assert_eq!(error.code(), code, "{reason}");
    assert_eq!(
        error.domain(),
        Some(ErrorDomains::DEFAULT_SERVING),
        "{reason}"
    );
}

/// A failed open as the caller sees it: the fixed answer and nothing else.
#[track_caller]
fn assert_unavailable(error: &AppError) {
    assert_serving(error, ErrorCode::Unavailable, reasons::TENANT_UNAVAILABLE);
    assert_eq!(
        error.message(),
        "this tenant is unavailable, try again later"
    );
    assert!(error.metadata().is_empty());
    assert_eq!(error.field_violations(), []);
    assert!(StdError::source(error).is_none());
}

/// Sets the tenant's stored role generation.
async fn set_generation(control: &PgPool, id: TenantId, generation: i32) {
    sqlx::query("UPDATE tenants SET db_role_generation = $1 WHERE id = $2::uuid")
        .bind(generation)
        .bind(id.to_string())
        .execute(control)
        .await
        .expect("the generation was set");
}

/// A stored text of a control row.
#[derive(Clone, Copy, Debug)]
enum Stored {
    Role,
    Cluster,
    Database,
    KeyId,
}

async fn set_stored(control: &PgPool, id: TenantId, column: Stored, value: &str) {
    let statement = match column {
        Stored::Role => "UPDATE tenants SET db_role = $1 WHERE id = $2::uuid",
        Stored::Cluster => "UPDATE tenants SET db_cluster = $1 WHERE id = $2::uuid",
        Stored::Database => "UPDATE tenants SET db_name = $1 WHERE id = $2::uuid",
        Stored::KeyId => "UPDATE tenants SET db_role_key_id = $1 WHERE id = $2::uuid",
    };
    sqlx::query(statement)
        .bind(value)
        .bind(id.to_string())
        .execute(control)
        .await
        .expect("the stored name was set");
}

async fn schema_version(control: &PgPool, id: TenantId) -> Option<i64> {
    sqlx::query_scalar::<_, Option<i64>>("SELECT schema_version FROM tenants WHERE id = $1::uuid")
        .bind(id.to_string())
        .fetch_one(control)
        .await
        .expect("the row exists")
}

/// A request head to `host` and `path`, naming `tenant` in the id header when given.
fn request(host: &str, path: &str, tenant: Option<TenantId>) -> http::request::Parts {
    let mut builder = http::Request::builder()
        .uri(path)
        .header(http::header::HOST, host);
    if let Some(id) = tenant {
        builder = builder.header(TENANT_HEADER, id.to_string());
    }
    builder.body(()).expect("a request head").into_parts().0
}

/// The text the product's refusal carries in its source: a connection string and a marker.
const SOURCE_TEXT: &str = "postgres://app_t_owner:hunter2@db.example.com/orders zebracorn";
/// A word that appears only in the product's source.
const MARKER: &str = "zebracorn";
/// The product's refusal message and metadata value.
const OWNED_MESSAGE: &str = "this tenant is served by another process";
const OWNER: &str = "process-seven";

fn owned_elsewhere() -> AppError {
    AppError::failed_precondition(OWNED_MESSAGE)
        .with_reason("OWNED_ELSEWHERE")
        .with_domain("billing.v1")
        .with_metadata("owner", OWNER)
        .with_retry_after(Duration::from_secs(3))
        .with_violation(FieldViolation::new("tenant", "held by another process"))
        .with_source(std::io::Error::other(SOURCE_TEXT))
}

fn refused_with_capacity() -> AppError {
    AppError::resource_exhausted("the product is at its limit").with_reason(KEYED_CAPACITY)
}

fn refused_with_credentials() -> AppError {
    AppError::permission_denied("the product refused the caller")
        .with_reason(DB_CREDENTIALS_REJECTED)
}

fn refused_as_busy() -> AppError {
    AppError::unavailable("the product is busy").with_reason(reasons::TENANT_BUSY)
}

fn failed_without_reason() -> AppError {
    AppError::unavailable("the product failed")
}

fn panicking_build() -> AppError {
    panic!("the test product's build panics on purpose")
}

#[track_caller]
fn assert_owned_elsewhere(error: &AppError) {
    assert_eq!(error.code(), ErrorCode::FailedPrecondition);
    assert_eq!(error.message(), OWNED_MESSAGE);
    assert_eq!(error.reason(), Some("OWNED_ELSEWHERE"));
    assert_eq!(error.domain(), Some("billing.v1"));
    assert_eq!(
        error.metadata(),
        &BTreeMap::from([("owner".to_owned(), OWNER.to_owned())])
    );
    assert_eq!(error.retry_after(), Some(Duration::from_secs(3)));
    assert_eq!(
        error.field_violations(),
        [FieldViolation::new("tenant", "held by another process")]
    );
    assert!(StdError::source(error).is_none());
}

#[tokio::test]
#[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
async fn every_state_but_active_is_refused_without_a_build() {
    let Some(cluster) = Cluster::start().await else {
        return;
    };
    common::guard(async {
        let fx = fixture(&cluster).await;
        let tenants = fx.tenants(&cluster, 4);
        let refused = [
            (
                TenantState::Provisioning,
                ErrorCode::Unavailable,
                reasons::TENANT_PROVISIONING,
            ),
            (
                TenantState::Suspended,
                ErrorCode::FailedPrecondition,
                reasons::TENANT_SUSPENDED,
            ),
            (
                TenantState::Purging,
                ErrorCode::NotFound,
                reasons::TENANT_NOT_HERE,
            ),
            (
                TenantState::Purged,
                ErrorCode::NotFound,
                reasons::TENANT_NOT_HERE,
            ),
        ];
        for (n, (state, code, reason)) in refused.into_iter().enumerate() {
            let label = format!("state{n}");
            let host = format!("{label}.example.com");
            let row = cluster
                .insert_row(&fx.control, &label, &[host.as_str()], state)
                .await;
            let error = tenants.get(row.info.id).await.expect_err("not served");
            assert_serving(&error, code, reason);
        }
        let unknown = tenants.get(common::new_tenant()).await.expect_err("no row");
        assert_serving(&unknown, ErrorCode::NotFound, reasons::TENANT_NOT_HERE);
        assert_eq!(fx.factory.built(), 0);
        assert_eq!(tenants.stats().creations, 0);

        let active = fx.tenant(&cluster, "alpha").await;
        let lease = tenants.get(active.info.id).await.expect("served");
        assert_eq!(lease.id(), active.info.id);
        assert_eq!(lease.info().state, TenantState::Active);
        assert_eq!(lease.tenant, active.info.id);
        assert_eq!(fx.factory.built(), 1);
    })
    .await;
}

#[tokio::test]
#[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
async fn twenty_cold_gets_build_and_migrate_once_as_the_tenant_role() {
    let Some(cluster) = Cluster::start().await else {
        return;
    };
    common::guard(async {
        let fx = fixture(&cluster).await;
        let tenants = fx.tenants(&cluster, 4);
        let id = fx.tenant(&cluster, "alpha").await.info.id;

        let answers = futures::future::join_all((0..20).map(|_| tenants.get(id))).await;
        let leases: Vec<_> = answers
            .into_iter()
            .map(|answer| answer.expect("served"))
            .collect();
        assert_eq!(fx.factory.built(), 1);
        assert_eq!(tenants.stats().creations, 1);
        let first = &leases[0];
        assert!(
            leases
                .iter()
                .all(|lease| std::ptr::eq(&raw const **lease, &raw const **first)),
            "every caller got the one scope"
        );

        let applied: i64 =
            sqlx::query_scalar("SELECT count(*) FROM _sqlx_migrations WHERE success")
                .fetch_one(&first.pool)
                .await
                .expect("the migrations table is readable");
        assert_eq!(applied, 2);
        let (user, database): (String, String) =
            sqlx::query_as("SELECT current_user::text, current_database()::text")
                .fetch_one(&first.pool)
                .await
                .expect("the session is readable");
        assert_eq!(user, role_of(&cluster, id));
        assert_eq!(database, database_of(&cluster, id));
    })
    .await;
}

#[tokio::test]
#[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
async fn a_gated_build_is_counted_as_creating_until_the_gate_opens() {
    let Some(cluster) = Cluster::start().await else {
        return;
    };
    common::guard(async {
        let fx = fixture(&cluster).await;
        let tenants = fx.tenants(&cluster, 4);
        let id = fx.tenant(&cluster, "alpha").await.info.id;

        fx.factory.close_gate();
        let waiter = tokio::spawn({
            let tenants = tenants.clone();
            async move { tenants.get(id).await }
        });
        fx.factory.arrived(1).await;
        assert_eq!(tenants.stats().creating, 1);
        assert_eq!(fx.factory.built(), 0);

        fx.factory.open_gate();
        let lease = waiter.await.expect("the task ran").expect("served");
        assert_eq!(lease.id(), id);
        let stats = tenants.stats();
        assert_eq!((stats.creating, stats.warm, stats.in_use), (0, 1, 1));
    })
    .await;
}

#[tokio::test]
#[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
async fn a_cancelled_waiter_neither_cancels_the_open_nor_frees_its_slot() {
    let Some(cluster) = Cluster::start().await else {
        return;
    };
    common::guard(async {
        let fx = fixture(&cluster).await;
        let tenants = fx.tenants(&cluster, 1);
        let first = fx.tenant(&cluster, "alpha").await.info.id;
        let second = fx.tenant(&cluster, "beta").await.info.id;

        fx.factory.close_gate();
        let waiter = tokio::spawn({
            let tenants = tenants.clone();
            async move { tenants.get(first).await }
        });
        fx.factory.arrived(1).await;
        waiter.abort();
        assert!(waiter.await.is_err(), "the waiter was cancelled");
        assert_eq!(tenants.stats().creating, 1);

        let refused = tenants.get(second).await.expect_err("the slot is taken");
        assert_serving(&refused, ErrorCode::Unavailable, reasons::TENANT_BUSY);
        assert_eq!(refused.retry_after(), Some(Duration::from_secs(1)));
        assert_eq!(tenants.stats().capacity_refusals, 1);

        fx.factory.open_gate();
        let lease = tenants.get(first).await.expect("the open went on");
        assert_eq!(lease.id(), first);
        assert_eq!(fx.factory.built(), 1);
        assert_eq!(tenants.stats().creations, 1);
    })
    .await;
}

#[tokio::test]
#[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
async fn a_build_refusal_reaches_the_caller_as_the_product_wrote_it() {
    let Some(cluster) = Cluster::start().await else {
        return;
    };
    common::guard(async {
        let fx = fixture(&cluster).await;
        let tenants = fx.tenants(&cluster, 4);
        let id = fx.tenant(&cluster, "alpha").await.info.id;

        fx.factory.fail_with(Some(owned_elsewhere));
        let error = tenants.get(id).await.expect_err("refused");
        assert_owned_elsewhere(&error);

        let framework_reasons = [
            refused_with_capacity as fn() -> AppError,
            refused_with_credentials,
            refused_as_busy,
        ];
        for refusal in framework_reasons {
            fx.factory.fail_with(Some(refusal));
            let error = tenants.get(id).await.expect_err("refused");
            assert_eq!(error.to_wire(), refusal().to_wire());
            assert!(StdError::source(&error).is_none());
        }
        let busy = tenants.get(id).await.expect_err("refused");
        assert_eq!(busy.domain(), None, "no serving domain is added");
        assert_eq!(busy.retry_after(), None, "not the cache's own busy answer");

        fx.factory.fail_with(Some(failed_without_reason));
        assert_unavailable(&tenants.get(id).await.expect_err("failed"));

        fx.factory.fail_with(Some(panicking_build));
        let panicked = tenants.get(id).await.expect_err("panicked");
        assert_eq!(panicked.code(), ErrorCode::Internal);
        assert_eq!(panicked.message(), "internal error");
        assert_eq!(panicked.reason(), None);
        assert_eq!(panicked.domain(), None);

        let stats = tenants.stats();
        assert_eq!((stats.warm, stats.create_failures), (0, 7));
        fx.factory.fail_with(None);
        let lease = tenants.get(id).await.expect("a failure is never cached");
        assert_eq!(lease.id(), id);
        assert_eq!(fx.factory.built(), 1);
        assert_eq!(tenants.stats().creations, 8);
    })
    .await;
}

/// One recorded event.
struct Captured {
    level: Level,
    fields: BTreeMap<String, String>,
}

/// Records events of the two crates when `events` is set; the global default (no `events`)
/// records nothing but keeps every callsite's interest open.
struct Recorder {
    events: Option<Arc<Mutex<Vec<Captured>>>>,
}

impl Subscriber for Recorder {
    fn register_callsite(&self, _metadata: &'static Metadata<'static>) -> Interest {
        Interest::sometimes()
    }

    fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        self.events.is_some()
            && (metadata.target().starts_with("sekvent_db")
                || metadata.target().starts_with("sekvent_tenancy"))
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
            events.lock().expect("the recorder lock").push(Captured {
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

/// Starts recording this thread's events until the guard drops.
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

/// Moves the events recorded so far into `all` and returns the fields of the one failure event
/// among them.
#[track_caller]
fn one_failure(
    events: &Mutex<Vec<Captured>>,
    all: &mut Vec<BTreeMap<String, String>>,
) -> BTreeMap<String, String> {
    let drained: Vec<Captured> = std::mem::take(&mut *events.lock().expect("the recorder lock"));
    let failures: Vec<BTreeMap<String, String>> = drained
        .iter()
        .filter(|event| {
            event.level == Level::WARN
                && event.fields.get("message").map(String::as_str)
                    == Some("creating a cached value failed")
        })
        .map(|event| event.fields.clone())
        .collect();
    all.extend(drained.into_iter().map(|event| event.fields));
    assert_eq!(failures.len(), 1, "{failures:?}");
    let failure = failures.into_iter().next().expect("one failure");
    assert!(failure.contains_key("code"), "{failure:?}");
    assert!(failure.contains_key("reason"), "{failure:?}");
    failure
}

fn field<'a>(fields: &'a BTreeMap<String, String>, name: &str) -> Option<&'a str> {
    fields.get(name).map(String::as_str)
}

/// Whether `source` is exactly the fixed text of a failed open at `stage`, alone or followed by
/// ` (sqlstate ` and five characters of `[0-9A-Z]` and `)`, and nothing else.
fn is_stage_text(source: &str, stage: &str) -> bool {
    let fixed = format!("opening the tenant failed: {stage}");
    let Some(rest) = source.strip_prefix(fixed.as_str()) else {
        return false;
    };
    if rest.is_empty() {
        return true;
    }
    rest.strip_prefix(" (sqlstate ")
        .and_then(|code| code.strip_suffix(')'))
        .is_some_and(|code| {
            code.len() == 5
                && code
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || byte.is_ascii_uppercase())
        })
}

#[test]
fn the_stage_text_matcher_accepts_the_fixed_text_and_a_sqlstate_only() {
    assert!(is_stage_text(
        "opening the tenant failed: migrate",
        "migrate"
    ));
    assert!(is_stage_text(
        "opening the tenant failed: migrate (sqlstate 42P07)",
        "migrate"
    ));
    for refused in [
        "opening the tenant failed: migrate: duplicate table",
        "opening the tenant failed: migrate (sqlstate 42p07)",
        "opening the tenant failed: migrate (sqlstate 42P0)",
        "opening the tenant failed: migrate (sqlstate 42P07) app_t_",
        "opening the tenant failed: migrate (sqlstate 42P07",
        "opening the tenant failed: migrated",
        "opening the tenant failed: build",
        "",
    ] {
        assert!(!is_stage_text(refused, "migrate"), "{refused}");
    }
}

#[tokio::test]
#[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
#[allow(clippy::too_many_lines)]
async fn a_failed_open_logs_a_stage_and_no_connection_detail() {
    let Some(cluster) = Cluster::start().await else {
        return;
    };
    common::guard(async {
        let fx = fixture(&cluster).await;
        let tenants = fx.tenants(&cluster, 8);
        let refused = fx.tenant(&cluster, "alpha").await.info.id;
        let rejected = fx.tenant(&cluster, "beta").await.info.id;
        let conflicting = fx.tenant(&cluster, "gamma").await.info.id;
        let mismatched = fx.tenant(&cluster, "delta").await.info.id;
        let (events, _recording) = record();
        let mut all = Vec::new();
        let mut answers = Vec::new();

        fx.factory.fail_with(Some(owned_elsewhere));
        let error = tenants.get(refused).await.expect_err("refused");
        fx.factory.fail_with(None);
        assert_owned_elsewhere(&error);
        let failure = one_failure(&events, &mut all);
        assert_eq!(field(&failure, "code"), Some("FAILED_PRECONDITION"));
        assert_eq!(field(&failure, "reason"), Some("OWNED_ELSEWHERE"));
        assert_eq!(
            field(&failure, "source"),
            Some("opening the tenant failed: build")
        );

        set_generation(&fx.control, rejected, 2).await;
        fx.registry.forget(rejected);
        let error = tenants.get(rejected).await.expect_err("rejected");
        assert_unavailable(&error);
        answers.push(format!("{error:?}"));
        let failure = one_failure(&events, &mut all);
        assert_eq!(field(&failure, "reason"), Some(DB_CREDENTIALS_REJECTED));
        assert_eq!(
            field(&failure, "source"),
            Some("connect failed: auth (sqlstate 28P01), 1 of 1 candidates tried")
        );

        cluster
            .admin_execute(
                &database_of(&cluster, conflicting),
                "CREATE TABLE orders (id integer)",
            )
            .await;
        let error = tenants.get(conflicting).await.expect_err("not migrated");
        assert_unavailable(&error);
        answers.push(format!("{error:?}"));
        let failure = one_failure(&events, &mut all);
        assert_eq!(field(&failure, "code"), Some("UNAVAILABLE"));
        assert_eq!(field(&failure, "reason"), Some(reasons::TENANT_UNAVAILABLE));
        assert!(
            field(&failure, "source").is_some_and(|source| is_stage_text(source, "migrate")),
            "{failure:?}"
        );

        let stranger = role_of(&cluster, common::new_tenant());
        set_stored(&fx.control, mismatched, Stored::Role, &stranger).await;
        fx.registry.forget(mismatched);
        let error = tenants.get(mismatched).await.expect_err("mismatched");
        assert_unavailable(&error);
        answers.push(format!("{error:?}"));
        let failure = one_failure(&events, &mut all);
        assert_eq!(field(&failure, "reason"), Some(reasons::TENANT_UNAVAILABLE));
        assert_eq!(
            field(&failure, "source"),
            Some("opening the tenant failed: row")
        );

        let host = PgConnectOptions::from_str(&cluster.harness().admin_url())
            .expect("the harness URL parses")
            .get_host()
            .to_owned();
        let mut forbidden = vec![
            cluster.control_owner().to_owned(),
            host,
            "postgres://".to_owned(),
            MARKER.to_owned(),
            OWNED_MESSAGE.to_owned(),
            OWNER.to_owned(),
            stranger,
        ];
        for id in [refused, rejected, conflicting, mismatched] {
            forbidden.push(role_of(&cluster, id));
            forbidden.push(database_of(&cluster, id));
            let mut key = cluster.role_key();
            for generation in [1, 2] {
                key.generation = generation;
                let password = cluster
                    .secrets()
                    .derive(id, &key)
                    .expect("the fixture key derives a password");
                forbidden.push(password.expose().to_owned());
            }
        }
        for fields in &all {
            for (name, value) in fields {
                for (n, text) in forbidden.iter().enumerate() {
                    assert!(
                        !value.contains(text.as_str()),
                        "event field {name} holds forbidden text {n}"
                    );
                }
            }
        }
        for answer in &answers {
            for (n, text) in forbidden.iter().enumerate() {
                assert!(
                    !answer.contains(text.as_str()),
                    "an answer holds forbidden text {n}"
                );
            }
        }
    })
    .await;
}

#[tokio::test]
#[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
async fn a_mismatched_stored_row_is_refused_before_any_connect() {
    let Some(cluster) = Cluster::start().await else {
        return;
    };
    common::guard(async {
        let fx = fixture(&cluster).await;
        let tenants = fx.tenants(&cluster, 4);
        let id = fx.tenant(&cluster, "alpha").await.info.id;
        let own_role = role_of(&cluster, id);
        let own_database = database_of(&cluster, id);
        let stranger = role_of(&cluster, common::new_tenant());

        let changes = [
            (Stored::Role, stranger.clone(), own_role.clone()),
            (Stored::Cluster, "replica".to_owned(), "main".to_owned()),
            (Stored::Database, "Not-A-Database".to_owned(), own_database),
            (Stored::KeyId, "k9".to_owned(), "k1".to_owned()),
        ];
        for (column, changed, original) in changes {
            set_stored(&fx.control, id, column, &changed).await;
            fx.registry.forget(id);
            let error = tenants.get(id).await.expect_err("refused");
            assert_unavailable(&error);
            assert_eq!(fx.factory.built(), 0, "{column:?}");
            assert_eq!(cluster.sessions_of(&own_role).await, 0, "{column:?}");
            assert_eq!(cluster.sessions_of(&stranger).await, 0, "{column:?}");
            set_stored(&fx.control, id, column, &original).await;
        }

        fx.registry.forget(id);
        let lease = tenants.get(id).await.expect("the restored row opens");
        assert_eq!(lease.id(), id);
        assert_eq!(fx.factory.built(), 1);
    })
    .await;
}

#[tokio::test]
#[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
async fn an_open_past_its_deadline_is_unavailable_caches_nothing_and_frees_its_slot() {
    let Some(cluster) = Cluster::start().await else {
        return;
    };
    common::guard(async {
        let fx = fixture(&cluster).await;
        let tenants = cluster
            .tenants_builder(&fx.registry, SelectorRules::new(), &fx.factory)
            .capacity(1)
            .create_timeout(Duration::from_secs(1))
            .build()
            .expect("valid settings");
        let id = fx.tenant(&cluster, "alpha").await.info.id;
        let role = role_of(&cluster, id);

        // The closed gate holds the build, so the deadline passes whether it runs out in the
        // connect, the migration or the build; the test never waits for the build to start.
        fx.factory.close_gate();
        let error = tenants.get(id).await.expect_err("past its deadline");
        assert_unavailable(&error);
        assert_eq!(fx.factory.built(), 0);
        assert!(tenants.peek(id).is_none(), "nothing is cached");
        let stats = tenants.stats();
        assert_eq!(
            (stats.warm, stats.creating, stats.create_failures),
            (0, 0, 1),
            "the one slot is free again"
        );
        sekvent_testing::await_until!(cluster.sessions_of(&role).await == 0);

        fx.factory.open_gate();
        let patient = fx.tenants(&cluster, 1);
        let lease = patient
            .get(id)
            .await
            .expect("served under the default deadline");
        assert_eq!(lease.id(), id);
        assert_eq!(fx.factory.built(), 1);
    })
    .await;
}

#[tokio::test]
#[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
async fn invalidate_retires_a_held_scope_and_the_next_get_builds_anew() {
    let Some(cluster) = Cluster::start().await else {
        return;
    };
    common::guard(async {
        let fx = fixture(&cluster).await;
        let tenants = fx.tenants(&cluster, 4);
        let id = fx.tenant(&cluster, "alpha").await.info.id;

        let held = tenants.get(id).await.expect("served");
        assert!(tenants.invalidate(id).await);
        let one: i32 = sqlx::query_scalar("SELECT 1")
            .fetch_one(&held.pool)
            .await
            .expect("the held scope still works");
        assert_eq!(one, 1);
        assert_eq!(tenants.stats().retiring, 1);

        let next = tenants.get(id).await.expect("served anew");
        assert_eq!(fx.factory.built(), 2);
        assert!(
            !std::ptr::eq(&raw const *held, &raw const *next),
            "a second scope"
        );
        assert_eq!(fx.factory.dropped(), 0);
        drop(held);
        fx.factory.dropped_at_least(1).await;
        assert!(
            tenants
                .peek(id)
                .is_some_and(|lease| std::ptr::eq(&raw const *lease, &raw const *next))
        );
    })
    .await;
}

#[tokio::test]
#[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
async fn invalidate_discards_a_creation_in_flight_and_its_waiter_gets_the_next_build() {
    let Some(cluster) = Cluster::start().await else {
        return;
    };
    common::guard(async {
        let fx = fixture(&cluster).await;
        let tenants = fx.tenants(&cluster, 4);
        let id = fx.tenant(&cluster, "alpha").await.info.id;

        fx.factory.close_gate();
        let waiter = tokio::spawn({
            let tenants = tenants.clone();
            async move { tenants.get(id).await }
        });
        fx.factory.arrived(1).await;
        assert!(tenants.invalidate(id).await, "the creation was retired");
        fx.factory.open_gate();

        let lease = waiter.await.expect("the task ran").expect("served");
        assert_eq!(lease.id(), id);
        assert_eq!(fx.factory.built(), 2);
        fx.factory.dropped_at_least(1).await;
        assert!(
            tenants
                .peek(id)
                .is_some_and(|warm| std::ptr::eq(&raw const *warm, &raw const *lease))
        );
    })
    .await;
}

#[tokio::test]
#[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
async fn a_row_suspended_under_a_creation_is_refused_by_the_fresh_read() {
    let Some(cluster) = Cluster::start().await else {
        return;
    };
    common::guard(async {
        let fx = fixture(&cluster).await;
        let tenants = fx.tenants(&cluster, 4);
        let id = fx.tenant(&cluster, "alpha").await.info.id;

        let warmed = fx
            .registry
            .tenant(id)
            .await
            .expect("the row is readable")
            .expect("the row exists");
        assert_eq!(warmed.info.state, TenantState::Active);
        cluster
            .set_state(&fx.control, id, TenantState::Suspended)
            .await;

        let error = tenants.get(id).await.expect_err("suspended");
        assert_serving(
            &error,
            ErrorCode::FailedPrecondition,
            reasons::TENANT_SUSPENDED,
        );
        assert!(StdError::source(&error).is_none());
        assert_eq!(fx.factory.built(), 0);
        let stats = tenants.stats();
        assert_eq!(
            (stats.warm, stats.creations, stats.create_failures),
            (0, 1, 1)
        );
    })
    .await;
}

#[tokio::test]
#[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
#[allow(clippy::too_many_lines)]
async fn a_row_changed_under_a_cold_open_is_refused_and_the_next_get_opens_the_current_row() {
    let Some(cluster) = Cluster::start().await else {
        return;
    };
    common::guard(async {
        let fx = fixture(&cluster).await;
        let tenants = fx.tenants(&cluster, 4);
        let id = fx.tenant(&cluster, "alpha").await.info.id;
        let role = role_of(&cluster, id);
        let first_database = database_of(&cluster, id);
        let moved = cluster.unique_name("moved");
        cluster.create_database_named(&moved, &role).await;

        // The connector step reads the row through the lookup cache, which keeps the row read
        // here; the build step reads the row as committed. So the pool is opened on the first
        // database and the build sees the moved one.
        let warmed = fx
            .registry
            .tenant(id)
            .await
            .expect("the row is readable")
            .expect("the row exists");
        assert_eq!(warmed.db_name, first_database);
        set_stored(&fx.control, id, Stored::Database, &moved).await;

        let (events, recording) = record();
        let mut all = Vec::new();
        let error = tenants
            .get(id)
            .await
            .expect_err("the pool's database is not the row's");
        assert_unavailable(&error);
        let failure = one_failure(&events, &mut all);
        drop(recording);
        assert_eq!(field(&failure, "reason"), Some(reasons::TENANT_UNAVAILABLE));
        assert_eq!(
            field(&failure, "source"),
            Some("opening the tenant failed: row")
        );
        assert_eq!(fx.factory.built(), 0, "nothing was built on the stale pool");
        assert!(tenants.peek(id).is_none(), "nothing is cached");
        let stats = tenants.stats();
        assert_eq!(
            (stats.warm, stats.creations, stats.create_failures),
            (0, 1, 1)
        );

        // A role key that changed under the open is refused the same way: the stale pool
        // connected with the key read first.
        set_stored(&fx.control, id, Stored::Database, &first_database).await;
        for (generation, key_id) in [(2, "k1"), (1, "k9")] {
            fx.registry.forget(id);
            drop(fx.registry.tenant(id).await.expect("the row is readable"));
            set_generation(&fx.control, id, generation).await;
            set_stored(&fx.control, id, Stored::KeyId, key_id).await;
            let committed = fx
                .registry
                .tenant_now(id)
                .await
                .expect("the row is readable")
                .expect("the row exists");
            assert_eq!(
                (
                    committed.info.role_key.key_id.as_str(),
                    committed.info.role_key.generation
                ),
                (
                    key_id,
                    u32::try_from(generation).expect("a positive generation")
                )
            );
            let what = format!("generation {generation}, key {key_id}");
            assert_unavailable(&tenants.get(id).await.expect_err(&what));
            assert_eq!(fx.factory.built(), 0, "{what}");
            assert!(tenants.peek(id).is_none(), "{what}");
            set_generation(&fx.control, id, 1).await;
            set_stored(&fx.control, id, Stored::KeyId, "k1").await;
        }

        // The next open reads the row as it is now and serves the database it names.
        set_stored(&fx.control, id, Stored::Database, &moved).await;
        fx.registry.forget(id);
        let lease = tenants.get(id).await.expect("the current row opens");
        assert_eq!(fx.factory.built(), 1);
        let (user, database): (String, String) =
            sqlx::query_as("SELECT current_user::text, current_database()::text")
                .fetch_one(&lease.pool)
                .await
                .expect("the session is readable");
        assert_eq!(user, role);
        assert_eq!(database, moved);
    })
    .await;
}

#[tokio::test]
#[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
#[allow(clippy::too_many_lines)]
async fn concurrent_gets_of_a_cold_id_share_one_admitted_lookup() {
    let Some(cluster) = Cluster::start().await else {
        return;
    };
    common::guard(async {
        let fx = fixture(&cluster).await;
        let tenants = fx.tenants(&cluster, 4);
        let id = fx.tenant(&cluster, "alpha").await.info.id;
        let other = fx.tenant(&cluster, "beta").await.info.id;
        fx.registry.clear_lookups();

        // Every control connection is held, so a read that took a lookup permit waits for a
        // connection and keeps its permit. The registry's pool of 4 has 2 lookup permits.
        let mut held = Vec::new();
        for _ in 0..4 {
            held.push(fx.control.acquire().await.expect("a control connection"));
        }
        let elsewhere = Selector::Id(common::new_tenant());
        let mut holder = pin!(fx.registry.lookup(&elsewhere));
        assert!(
            futures::poll!(holder.as_mut()).is_pending(),
            "the read holds one permit"
        );

        // The first get leads the read of the id with the last permit; the others join it.
        let mut gets = pin!(futures::future::join_all((0..8).map(|_| tenants.get(id))));
        assert!(futures::poll!(gets.as_mut()).is_pending());

        // No permit is left: a get of another cold id is busy at once, without a read.
        let refused = tenants
            .get(other)
            .await
            .expect_err("no lookup permit is free");
        assert_serving(&refused, ErrorCode::Unavailable, reasons::TENANT_BUSY);
        assert_eq!(refused.retry_after(), Some(Duration::from_secs(1)));
        assert_eq!(tenants.stats().creations, 0);

        drop(held);
        let leases: Vec<_> = gets
            .await
            .into_iter()
            .map(|answer| answer.expect("served through the one read"))
            .collect();
        assert_eq!(leases.len(), 8);
        assert_eq!(fx.factory.built(), 1);
        assert_eq!(holder.await.expect("the other read ended"), None);

        let lease = tenants
            .get(other)
            .await
            .expect("served once a permit is free");
        assert_eq!(lease.id(), other);
    })
    .await;
}

#[tokio::test]
#[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
#[allow(clippy::too_many_lines)]
async fn a_factory_refusal_with_the_busy_reason_is_a_failure_of_walks_and_the_sweep() {
    let Some(cluster) = Cluster::start().await else {
        return;
    };
    common::guard(async {
        let fx = fixture(&cluster).await;
        let tenants = fx.tenants(&cluster, 4);
        let id = fx.tenant(&cluster, "alpha").await.info.id;
        fx.factory.fail_with(Some(refused_as_busy));

        let (events, recording) = record();
        let walked = tenants
            .for_each_active(|_lease| async { Ok(()) })
            .await
            .expect("the walk ran");
        let swept = tenants.migration_sweep().await.expect("the sweep ran");
        drop(recording);
        for report in [&walked, &swept] {
            assert_eq!((report.visited, report.busy), (1, 0));
            assert_eq!(report.failed, [id]);
        }
        assert_eq!(schema_version(&fx.control, id).await, None);
        let skipped: Vec<BTreeMap<String, String>> = events
            .lock()
            .expect("the recorder lock")
            .iter()
            .filter(|event| {
                event.level == Level::WARN
                    && field(&event.fields, "message")
                        == Some("a tenant walk skipped a tenant that failed")
            })
            .map(|event| event.fields.clone())
            .collect();
        assert_eq!(skipped.len(), 2, "{skipped:?}");
        for fields in &skipped {
            assert_eq!(field(fields, "code"), Some(ErrorCode::Unavailable.as_str()));
            assert_eq!(field(fields, "reason"), Some(reasons::TENANT_BUSY));
            assert_eq!(field(fields, "tenant"), Some(id.to_string().as_str()));
        }

        fx.factory.fail_with(None);
        let swept = tenants.migration_sweep().await.expect("the sweep ran");
        assert_eq!((swept.visited, swept.busy), (1, 0));
        assert_eq!(swept.failed, [] as [TenantId; 0]);
        assert_eq!(
            schema_version(&fx.control, id).await,
            Some(LATEST_TENANT_VERSION)
        );
    })
    .await;
}

#[tokio::test]
#[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
async fn a_sweep_over_a_cache_of_one_opens_one_tenant_at_a_time() {
    let Some(cluster) = Cluster::start().await else {
        return;
    };
    common::guard(async {
        let fx = fixture(&cluster).await;
        let tenants = fx.tenants(&cluster, 1);
        let mut ids = Vec::new();
        for label in ["alpha", "beta"] {
            ids.push(fx.tenant(&cluster, label).await.info.id);
        }

        let report = tenants.migration_sweep().await.expect("the sweep ran");
        assert_eq!((report.visited, report.busy), (2, 0));
        assert_eq!(report.failed, [] as [TenantId; 0]);
        for id in &ids {
            assert_eq!(
                schema_version(&fx.control, *id).await,
                Some(LATEST_TENANT_VERSION)
            );
        }
        let stats = tenants.stats();
        assert_eq!((stats.creations, stats.capacity_refusals), (2, 0));
        assert_eq!(fx.factory.built(), 2);
    })
    .await;
}

#[tokio::test]
#[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
async fn a_warm_tenant_that_is_suspended_is_refused_but_still_peekable() {
    let Some(cluster) = Cluster::start().await else {
        return;
    };
    common::guard(async {
        let fx = fixture(&cluster).await;
        let tenants = fx.tenants(&cluster, 4);
        let id = fx.tenant(&cluster, "alpha").await.info.id;
        drop(tenants.get(id).await.expect("served"));

        cluster
            .set_state(&fx.control, id, TenantState::Suspended)
            .await;
        fx.registry.forget(id);
        let error = tenants.get(id).await.expect_err("suspended");
        assert_serving(
            &error,
            ErrorCode::FailedPrecondition,
            reasons::TENANT_SUSPENDED,
        );

        let peeked = tenants.peek(id).expect("the scope is still warm");
        assert_eq!(peeked.id(), id);
        drop(peeked);
        assert!(tenants.invalidate(id).await);
        fx.factory.dropped_at_least(1).await;
        assert!(tenants.peek(id).is_none());
    })
    .await;
}

#[tokio::test]
#[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
async fn a_full_cache_refuses_a_third_tenant_and_evicts_an_idle_one() {
    let Some(cluster) = Cluster::start().await else {
        return;
    };
    common::guard(async {
        let fx = fixture(&cluster).await;
        let tenants = fx.tenants(&cluster, 2);
        let first = fx.tenant(&cluster, "alpha").await.info.id;
        let second = fx.tenant(&cluster, "beta").await.info.id;
        let third = fx.tenant(&cluster, "gamma").await.info.id;

        let held_first = tenants.get(first).await.expect("served");
        let held_second = tenants.get(second).await.expect("served");
        let refused = tenants.get(third).await.expect_err("full");
        assert_serving(&refused, ErrorCode::Unavailable, reasons::TENANT_BUSY);

        drop(held_second);
        let held_third = tenants.get(third).await.expect("served after an eviction");
        assert_eq!(tenants.stats().evictions, 1);
        assert!(tenants.peek(second).is_none());
        fx.factory.dropped_at_least(1).await;
        let second_role = role_of(&cluster, second);
        sekvent_testing::await_until!(cluster.sessions_of(&second_role).await == 0);

        drop(held_first);
        drop(held_third);
        assert_eq!(tenants.evict_idle_older_than(Duration::ZERO).await, 2);
        let stats = tenants.stats();
        assert_eq!((stats.warm, stats.evictions), (0, 3));
        fx.factory.dropped_at_least(3).await;
    })
    .await;
}

#[tokio::test]
#[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
async fn a_clone_of_a_lease_holds_the_scope_like_the_original() {
    let Some(cluster) = Cluster::start().await else {
        return;
    };
    common::guard(async {
        let fx = fixture(&cluster).await;
        let tenants = fx.tenants(&cluster, 1);
        let first = fx.tenant(&cluster, "alpha").await.info.id;
        let second = fx.tenant(&cluster, "beta").await.info.id;

        let lease = tenants.get(first).await.expect("served");
        let copy = lease.clone();
        assert!(
            std::ptr::eq(&raw const *lease, &raw const *copy),
            "clones share one scope"
        );
        assert_eq!((copy.id(), copy.info()), (first, lease.info()));
        drop(lease);

        let refused = tenants
            .get(second)
            .await
            .expect_err("the clone holds the one slot");
        assert_serving(&refused, ErrorCode::Unavailable, reasons::TENANT_BUSY);
        assert_eq!(tenants.stats().evictions, 0);

        drop(copy);
        let other = tenants
            .get(second)
            .await
            .expect("served after the clone was dropped");
        assert_eq!(other.id(), second);
        assert_eq!(tenants.stats().evictions, 1);
        assert!(tenants.peek(first).is_none());
        assert_eq!(fx.factory.built(), 2);
    })
    .await;
}

#[tokio::test]
#[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
async fn rejected_credentials_are_unavailable_until_the_generation_is_restored() {
    let Some(cluster) = Cluster::start().await else {
        return;
    };
    common::guard(async {
        let fx = fixture(&cluster).await;
        let tenants = fx.tenants(&cluster, 4);
        let id = fx.tenant(&cluster, "alpha").await.info.id;

        set_generation(&fx.control, id, 2).await;
        fx.registry.forget(id);
        assert_unavailable(&tenants.get(id).await.expect_err("rejected"));
        assert_eq!(fx.factory.built(), 0);

        set_generation(&fx.control, id, 1).await;
        fx.registry.forget(id);
        let lease = tenants
            .get(id)
            .await
            .expect("the restored generation opens");
        assert_eq!(lease.info().role_key.generation, 1);
        assert_eq!(fx.factory.built(), 1);
    })
    .await;
}

#[tokio::test]
#[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
async fn a_failed_migration_is_unavailable_until_the_conflict_is_gone() {
    let Some(cluster) = Cluster::start().await else {
        return;
    };
    common::guard(async {
        let fx = fixture(&cluster).await;
        let tenants = fx.tenants(&cluster, 4);
        let id = fx.tenant(&cluster, "alpha").await.info.id;
        let database = database_of(&cluster, id);

        cluster
            .admin_execute(&database, "CREATE TABLE orders (id integer)")
            .await;
        assert_unavailable(&tenants.get(id).await.expect_err("not migrated"));
        assert_eq!(fx.factory.built(), 0);

        cluster.admin_execute(&database, "DROP TABLE orders").await;
        let lease = tenants.get(id).await.expect("migrated now");
        let applied: i64 =
            sqlx::query_scalar("SELECT count(*) FROM _sqlx_migrations WHERE success")
                .fetch_one(&lease.pool)
                .await
                .expect("the migrations table is readable");
        assert_eq!(applied, 2);
    })
    .await;
}

#[tokio::test]
#[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
async fn a_database_ahead_of_the_binary_is_unavailable() {
    let Some(cluster) = Cluster::start().await else {
        return;
    };
    common::guard(async {
        let fx = fixture(&cluster).await;
        let tenants = fx.tenants(&cluster, 4);
        let id = fx.tenant(&cluster, "alpha").await.info.id;
        drop(tenants.get(id).await.expect("served"));

        cluster
            .admin_execute(
                &database_of(&cluster, id),
                "INSERT INTO _sqlx_migrations \
                 (version, description, success, checksum, execution_time) \
                 VALUES (99, 'ahead', true, decode('00', 'hex'), 0)",
            )
            .await;
        assert!(tenants.invalidate(id).await);
        assert_unavailable(&tenants.get(id).await.expect_err("ahead of the binary"));
        assert_eq!(fx.factory.built(), 1);
    })
    .await;
}

#[tokio::test]
#[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
async fn a_tenant_role_reaches_its_own_database_only() {
    let Some(cluster) = Cluster::start().await else {
        return;
    };
    common::guard(async {
        let fx = fixture(&cluster).await;
        let tenants = fx.tenants(&cluster, 4);
        let first = fx.tenant(&cluster, "alpha").await.info.id;
        let second = fx.tenant(&cluster, "beta").await.info.id;

        let password = cluster
            .secrets()
            .derive(first, &cluster.role_key())
            .expect("the fixture key derives a password");
        let own = cluster
            .connect_as(
                &role_of(&cluster, first),
                &password,
                &database_of(&cluster, first),
            )
            .await
            .expect("the role reaches its own database");
        sqlx::Connection::close(own)
            .await
            .expect("the session closed");
        let foreign = cluster
            .connect_as(
                &role_of(&cluster, first),
                &password,
                &database_of(&cluster, second),
            )
            .await;
        assert!(foreign.is_err(), "the role does not reach another database");

        let lease_first = tenants.get(first).await.expect("served");
        let lease_second = tenants.get(second).await.expect("served");
        for (lease, item) in [(&lease_first, "from-alpha"), (&lease_second, "from-beta")] {
            sqlx::query("INSERT INTO orders (item) VALUES ($1)")
                .bind(item)
                .execute(&lease.pool)
                .await
                .expect("an order was written");
        }
        let items: Vec<String> = sqlx::query_scalar("SELECT item FROM orders ORDER BY id")
            .fetch_all(&lease_first.pool)
            .await
            .expect("the orders are readable");
        assert_eq!(items, ["from-alpha"]);
    })
    .await;
}

#[tokio::test]
#[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
#[allow(clippy::too_many_lines)]
async fn select_and_resolve_name_the_tenant_without_opening_it() {
    let Some(cluster) = Cluster::start().await else {
        return;
    };
    common::guard(async {
        let fx = fixture(&cluster).await;
        let rules = SelectorRules::new()
            .id_header(
                http::HeaderName::from_static(TENANT_HEADER),
                ["api.example.com".to_owned()],
            )
            .expect("the id-header hosts")
            .path_label("/t/", ["app.example.com".to_owned()])
            .expect("the path-label hosts")
            .bypass(&["/billing.health.v1.Health/"]);
        let tenants = cluster.tenants(&fx.registry, rules, &fx.factory, 4);
        let id = fx.tenant(&cluster, "alpha").await.info.id;

        let by_header = request("api.example.com", "/billing.v1.Billing/Charge", Some(id));
        let by_label = request(
            "app.example.com",
            "/t/alpha/billing.v1.Billing/Charge",
            None,
        );
        let bypass = request("api.example.com", "/billing.health.v1.Health/Check", None);
        assert_eq!(tenants.select(&by_header).await.ok(), Some(Some(id)));
        assert_eq!(tenants.select(&by_label).await.ok(), Some(Some(id)));
        assert_eq!(tenants.select(&bypass).await.ok(), Some(None));
        assert!(tenants.resolve(&bypass).await.expect("resolved").is_none());
        assert_eq!(fx.factory.built(), 0);
        assert_eq!(tenants.stats().creations, 0);

        let resolver = TenantResolver::new(tenants.clone(), &["billing.v1.Billing/Watch"]);
        let streaming = request("api.example.com", "/billing.v1.Billing/Watch", Some(id));
        let scope = resolver
            .resolve(&streaming, &CallContext::new())
            .await
            .expect("resolved");
        assert_eq!(scope.tenant(), Some(id.to_string().as_str()));
        assert_eq!(scope.extension::<StreamTenant>(), Some(&StreamTenant(id)));
        assert_eq!(fx.factory.built(), 0);
        assert_eq!(tenants.stats().creations, 0);

        let lease = tenants
            .resolve(&by_label)
            .await
            .expect("resolved")
            .expect("a tenant");
        assert_eq!(lease.id(), id);
        assert_eq!(fx.factory.built(), 1);
        drop(lease);

        fx.registry.clear_lookups();
        fx.control.close().await;
        let error = tenants
            .select(&by_header)
            .await
            .expect_err("no control read");
        assert_serving(&error, ErrorCode::Unavailable, reasons::TENANT_UNAVAILABLE);
    })
    .await;
}

#[tokio::test]
#[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
#[allow(clippy::too_many_lines)]
async fn for_each_active_visits_the_active_tenants_in_id_order() {
    let Some(cluster) = Cluster::start().await else {
        return;
    };
    common::guard(async {
        let fx = fixture(&cluster).await;
        let tenants = fx.tenants(&cluster, 1);
        let mut ids = Vec::new();
        for label in ["alpha", "beta", "gamma"] {
            ids.push(fx.tenant(&cluster, label).await.info.id);
        }
        ids.sort_unstable();
        let suspended = cluster
            .insert_row(
                &fx.control,
                "delta",
                &["delta.example.com"],
                TenantState::Suspended,
            )
            .await;

        let mut seen = Vec::new();
        let report = tenants
            .for_each_active(|lease| {
                seen.push(lease.id());
                async { Ok(()) }
            })
            .await
            .expect("the walk ran");
        assert_eq!(seen, ids);
        assert!(!seen.contains(&suspended.info.id));
        assert_eq!((report.visited, report.busy), (3, 0));
        assert_eq!(report.failed, [] as [TenantId; 0]);
        assert_eq!(tenants.stats().evictions, 2);

        let held = tenants.get(ids[0]).await.expect("served");
        let report = tenants
            .for_each_active(|_lease| async { Ok(()) })
            .await
            .expect("the walk ran");
        assert_eq!((report.visited, report.busy), (3, 2));
        assert_eq!(report.failed, [] as [TenantId; 0]);
        drop(held);

        let failing = ids[1];
        let report = tenants
            .for_each_active(|lease| {
                let fails = lease.id() == failing;
                async move {
                    if fails {
                        Err(AppError::internal("the visit failed"))
                    } else {
                        Ok(())
                    }
                }
            })
            .await
            .expect("the walk ran");
        assert_eq!((report.visited, report.busy), (3, 0));
        assert_eq!(report.failed, [failing]);

        set_generation(&fx.control, ids[2], 2).await;
        assert!(tenants.invalidate(ids[2]).await);
        let report = tenants
            .for_each_active(|_lease| async { Ok(()) })
            .await
            .expect("the walk ran");
        assert_eq!((report.visited, report.busy), (3, 0));
        assert_eq!(report.failed, [ids[2]]);

        fx.control.close().await;
        tenants
            .for_each_active(|_lease| async { Ok(()) })
            .await
            .expect_err("the list read failed");
    })
    .await;
}

#[tokio::test]
#[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
#[allow(clippy::too_many_lines)]
async fn the_migration_sweep_stores_the_latest_version_and_skips_a_locked_row() {
    let Some(cluster) = Cluster::start().await else {
        return;
    };
    common::guard(async {
        let fx = fixture(&cluster).await;
        let tenants = fx.tenants(&cluster, 4);
        let mut ids = Vec::new();
        for label in ["alpha", "beta", "gamma"] {
            ids.push(fx.tenant(&cluster, label).await.info.id);
        }
        let locked = ids[0];

        drop(tenants.get(locked).await.expect("served"));
        assert_eq!(schema_version(&fx.control, locked).await, None);

        let mut holder = fx.control.begin().await.expect("a transaction began");
        sqlx::query("SELECT 1 FROM tenants WHERE id = $1::uuid FOR UPDATE")
            .bind(locked.to_string())
            .execute(&mut *holder)
            .await
            .expect("the row is locked");
        let report = tenants.migration_sweep().await.expect("the sweep ran");
        assert_eq!((report.visited, report.busy), (3, 1));
        assert_eq!(report.failed, [] as [TenantId; 0]);
        assert_eq!(schema_version(&fx.control, locked).await, None);
        for id in &ids[1..] {
            assert_eq!(
                schema_version(&fx.control, *id).await,
                Some(LATEST_TENANT_VERSION)
            );
        }
        holder.rollback().await.expect("the lock was released");

        let report = tenants.migration_sweep().await.expect("the sweep ran");
        assert_eq!((report.visited, report.busy), (3, 0));
        assert_eq!(report.failed, [] as [TenantId; 0]);
        for id in &ids {
            assert_eq!(
                schema_version(&fx.control, *id).await,
                Some(LATEST_TENANT_VERSION)
            );
        }

        let small = fx.tenants(&cluster, 1);
        let mut sorted = ids.clone();
        sorted.sort_unstable();
        let held = small.get(sorted[0]).await.expect("served");
        let report = small.migration_sweep().await.expect("the sweep ran");
        assert_eq!((report.visited, report.busy), (3, 2));
        assert_eq!(report.failed, [] as [TenantId; 0]);
        drop(held);

        set_generation(&fx.control, sorted[2], 2).await;
        assert!(tenants.invalidate(sorted[2]).await);
        let report = tenants.migration_sweep().await.expect("the sweep ran");
        assert_eq!((report.visited, report.busy), (3, 0));
        assert_eq!(report.failed, [sorted[2]]);
    })
    .await;
}

#[tokio::test]
#[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
async fn a_version_write_that_fails_is_recorded_and_logged_without_the_error_text() {
    let Some(cluster) = Cluster::start().await else {
        return;
    };
    common::guard(async {
        let fx = fixture(&cluster).await;
        let tenants = fx.tenants(&cluster, 4);
        let mut ids = Vec::new();
        for label in ["alpha", "beta", "gamma"] {
            ids.push(fx.tenant(&cluster, label).await.info.id);
        }
        ids.sort_unstable();
        let owner = common::quote(cluster.control_owner());
        cluster
            .admin_execute(
                cluster.control_database(),
                &format!("REVOKE UPDATE ON tenants FROM {owner}"),
            )
            .await;

        let (events, recording) = record();
        let report = tenants.migration_sweep().await.expect("the sweep ran");
        drop(recording);
        assert_eq!((report.visited, report.busy), (3, 0));
        assert_eq!(report.failed, ids);
        for id in &ids {
            assert_eq!(schema_version(&fx.control, *id).await, None);
            assert!(tenants.peek(*id).is_some(), "the open itself succeeded");
        }
        let skipped: Vec<BTreeMap<String, String>> = events
            .lock()
            .expect("the recorder lock")
            .iter()
            .filter(|event| {
                event.level == Level::WARN
                    && field(&event.fields, "message")
                        == Some("a tenant walk skipped a tenant that failed")
            })
            .map(|event| event.fields.clone())
            .collect();
        let mut logged: Vec<String> = Vec::new();
        for fields in &skipped {
            assert_eq!(
                fields.keys().map(String::as_str).collect::<Vec<_>>(),
                ["code", "message", "reason", "tenant"],
                "{fields:?}"
            );
            assert_eq!(
                field(fields, "code"),
                Some(ErrorCode::PermissionDenied.as_str())
            );
            assert_eq!(field(fields, "reason"), Some(DB_PERMISSION_DENIED));
            logged.push(field(fields, "tenant").expect("a tenant").to_owned());
        }
        logged.sort_unstable();
        let mut expected: Vec<String> = ids.iter().map(ToString::to_string).collect();
        expected.sort_unstable();
        assert_eq!(logged, expected);

        cluster
            .admin_execute(
                cluster.control_database(),
                &format!("GRANT UPDATE ON tenants TO {owner}"),
            )
            .await;
        let report = tenants.migration_sweep().await.expect("the sweep ran");
        assert_eq!((report.visited, report.busy), (3, 0));
        assert_eq!(report.failed, [] as [TenantId; 0]);
        for id in &ids {
            assert_eq!(
                schema_version(&fx.control, *id).await,
                Some(LATEST_TENANT_VERSION)
            );
        }
    })
    .await;
}

#[tokio::test]
#[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
async fn stats_hosts_and_close_reflect_the_cache() {
    let Some(cluster) = Cluster::start().await else {
        return;
    };
    common::guard(async {
        let fx = fixture(&cluster).await;
        let tenants = fx.tenants(&cluster, 4);
        let first = fx.tenant(&cluster, "alpha").await.info.id;
        let second = fx.tenant(&cluster, "beta").await.info.id;
        cluster
            .insert_row(
                &fx.control,
                "gamma",
                &["gamma.example.com"],
                TenantState::Suspended,
            )
            .await;

        let mut read: Vec<TenantId> = tenants
            .refresh_hosts()
            .await
            .expect("the hosts were read")
            .iter()
            .map(|info| info.id)
            .collect();
        read.sort_unstable();
        let mut expected = vec![first, second];
        expected.sort_unstable();
        assert_eq!(read, expected);
        let snapshot = tenants.hosts().expect("the keyed form has a snapshot");
        assert_eq!(snapshot.tenant_of("alpha.example.com"), Some(first));
        assert_eq!(snapshot.tenant_of("beta.example.com"), Some(second));
        assert_eq!(snapshot.tenant_of("gamma.example.com"), None);
        assert_eq!(snapshot.len(), 2);
        assert!(tenants.registry().is_some());

        let cold = tenants.stats();
        assert_eq!((cold.capacity, cold.warm, cold.creations), (4, 0, 0));
        let lease = tenants.get(first).await.expect("served");
        let open = tenants.stats();
        assert_eq!(
            (open.warm, open.in_use, open.creating, open.creations),
            (1, 1, 0, 1)
        );
        drop(lease);
        let idle = tenants.stats();
        assert_eq!((idle.warm, idle.in_use), (1, 0));

        tenants.close().await;
        assert_unavailable(&tenants.get(first).await.expect_err("closed"));
        assert_eq!(tenants.stats().warm, 0);
    })
    .await;
}

#[cfg(feature = "runtime")]
mod unit {
    use sekvent_runtime::{Runtime, Stage, UnitExit, UnitPolicy};

    use super::{Cluster, assert_unavailable, common, fixture};

    const UNIT: &str = "tenant-cache";

    #[tokio::test]
    #[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
    async fn the_runtime_unit_serves_and_closes_the_cache_on_shutdown() {
        let Some(cluster) = Cluster::start().await else {
            return;
        };
        common::guard(async {
            let fx = fixture(&cluster).await;
            let tenants = fx.tenants(&cluster, 4);
            let id = fx.tenant(&cluster, "alpha").await.info.id;

            let handle = Runtime::builder()
                .without_signals()
                .unit(
                    UNIT,
                    Stage::Infrastructure,
                    UnitPolicy::Critical,
                    tenants.clone().into_unit(),
                )
                .build()
                .expect("a runtime")
                .start()
                .await
                .expect("the unit reported ready");
            drop(tenants.get(id).await.expect("served"));

            handle.shutdown();
            let report = handle.wait().await.expect("a report");
            assert_eq!(
                report.unit(UNIT).expect("the unit's report").exit,
                UnitExit::Completed
            );
            assert_unavailable(&tenants.get(id).await.expect_err("closed"));
        })
        .await;
    }
}
