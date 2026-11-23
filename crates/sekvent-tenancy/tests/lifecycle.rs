//! Suspend, resume, set hosts, usage and describe on a real server: the state table, idempotent
//! repeats that announce again, strictness, the hooks' errors, read-only usage, the warm pool a
//! cached tenant lends and the cache slot an opened pool holds, and every committed change
//! reaching this process before the call returns and the other processes after.
#![cfg(feature = "lifecycle")]

mod common;

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use common::Cluster;
use common::product::hooks::{BEFORE_RESUME, BEFORE_SUSPEND, DESCRIBE, OrdersHooks, USAGE};
use common::product::{OrdersFactory, OrdersScope, TENANT_MIGRATOR};
use sekvent_context::Clock;
use sekvent_error::{AppError, ErrorCode};
use sekvent_tenancy::invalidation::{ChangeKind, Listener, Sender, Sink};
use sekvent_tenancy::lifecycle::{
    HookFuture, HostsChange, ProvisionRequest, Provisioner, TenantHooks, UsageCounters,
};
use sekvent_tenancy::{
    Registry, SelectorRules, TenantId, TenantInfo, TenantRow, TenantState, Tenants, reasons,
};
use sqlx::PgPool;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

/// The registry of this process, its control pool and the test product's hooks.
async fn set_up(cluster: &Cluster) -> (Registry, PgPool, OrdersHooks) {
    let clock: Arc<dyn Clock> = Arc::new(common::manual_clock());
    let (registry, pool) = cluster.registry(clock).await;
    (registry, pool, OrdersHooks::new())
}

/// A provisioned, active tenant labelled `label` on `<label>.example.com`.
async fn provision(provisioner: &Provisioner<OrdersHooks>, label: &str) -> TenantRow {
    let host = format!("{label}.example.com");
    provisioner
        .provision(common::provision_request(label, &[host.as_str()]))
        .await
        .expect("the tenant was provisioned")
        .row
}

fn state_of(row: &TenantRow) -> TenantState {
    row.info.state
}

async fn committed(provisioner: &Provisioner<OrdersHooks>, id: TenantId) -> TenantRow {
    provisioner
        .tenant(id)
        .await
        .expect("the tenant's row is readable")
}

fn assert_refused(error: &AppError, code: ErrorCode, reason: &str) {
    assert_eq!(error.code(), code, "{error:?}");
    assert_eq!(error.reason(), Some(reason), "{error:?}");
    assert_eq!(error.domain(), None, "lifecycle refusals carry no domain");
}

fn change(subdomain: &str, hosts: &[&str], primary: &str) -> HostsChange {
    HostsChange {
        subdomain: subdomain.to_owned(),
        hosts: hosts.iter().map(|host| (*host).to_owned()).collect(),
        primary_host: primary.to_owned(),
    }
}

/// A sink that records every change it hears.
#[derive(Default)]
struct Recorded(Mutex<Vec<(TenantId, ChangeKind)>>);

impl Recorded {
    /// The kinds heard for `tenant`, in order.
    fn of(&self, tenant: TenantId) -> Vec<ChangeKind> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .filter(|(id, _)| *id == tenant)
            .map(|(_, kind)| kind.clone())
            .collect()
    }
}

impl Sink for Recorded {
    fn change(&self, tenant: TenantId, kind: &ChangeKind) {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push((tenant, kind.clone()));
    }
}

/// A sink that forwards every change into a channel.
struct Forward(mpsc::UnboundedSender<(TenantId, ChangeKind)>);

impl Sink for Forward {
    fn change(&self, tenant: TenantId, kind: &ChangeKind) {
        let _ = self.0.send((tenant, kind.clone()));
    }
}

/// A second process: its own control pool, registry and tenant cache.
struct Other {
    factory: OrdersFactory,
    tenants: Tenants<OrdersScope>,
}

async fn other_process(cluster: &Cluster) -> Other {
    let pool = cluster.control_pool_of(4).await;
    let clock: Arc<dyn Clock> = Arc::new(common::manual_clock());
    let registry = Registry::new(pool, clock).expect("a pool of 4 is enough");
    let factory = OrdersFactory::new();
    let tenants = cluster.tenants(&registry, SelectorRules::new(), &factory, 8);
    Other { factory, tenants }
}

type Run = JoinHandle<Result<(), AppError>>;

/// Runs a listener on `tenants` on the test's channel; returns once it called `ready`.
async fn listen(
    cluster: &Cluster,
    tenants: &Tenants<OrdersScope>,
) -> (
    Run,
    CancellationToken,
    mpsc::UnboundedReceiver<(TenantId, ChangeKind)>,
) {
    let (tx, rx) = mpsc::unbounded_channel();
    let listener = Listener::new(tenants.clone(), cluster.channel()).sink(Forward(tx));
    let (ready, live) = oneshot::channel();
    let shutdown = CancellationToken::new();
    let handle = tokio::spawn(listener.run(
        move || {
            let _ = ready.send(());
        },
        shutdown.clone(),
    ));
    live.await.expect("the listener called ready");
    (handle, shutdown, rx)
}

async fn stop(handle: Run, shutdown: CancellationToken) {
    shutdown.cancel();
    handle
        .await
        .expect("the listener did not panic")
        .expect("the listener ended without an error");
}

type Heard = mpsc::UnboundedReceiver<(TenantId, ChangeKind)>;

/// Waits for the next state change of `tenant` the other process hears; changes of other
/// tenants are skipped.
async fn next_state_change(heard: &mut Heard, tenant: TenantId) {
    loop {
        let (id, kind) = heard.recv().await.expect("the listener is running");
        if id == tenant {
            assert_eq!(kind, ChangeKind::STATE);
            return;
        }
    }
}

/// Starts the other process's listener, provisions `label` and waits until the other process
/// heard the provisioning's change, so the next change it hears is a later call's. The tenant
/// is then warm on the other process.
async fn provision_heard(
    cluster: &Cluster,
    provisioner: &Provisioner<OrdersHooks>,
    other: &Other,
    label: &str,
) -> (TenantId, Run, CancellationToken, Heard) {
    let host = format!("{label}.example.com");
    let request = common::provision_request(label, &[host.as_str()]);
    let id = request.tenant;
    let (handle, shutdown, mut heard) = listen(cluster, &other.tenants).await;
    provisioner
        .provision(request)
        .await
        .expect("the tenant was provisioned");
    next_state_change(&mut heard, id).await;
    drop(other.tenants.get(id).await.expect("the tenant opens there"));
    assert!(other.tenants.peek(id).is_some());
    (id, handle, shutdown, heard)
}

fn orders_open() -> AppError {
    AppError::failed_precondition("open orders remain")
        .with_reason("ORDERS_OPEN")
        .with_domain("orders.example.v1")
        .with_metadata("orders", "3")
}

fn assert_orders_open(error: &AppError) {
    assert_eq!(error.code(), ErrorCode::FailedPrecondition);
    assert_eq!(error.message(), "open orders remain");
    assert_eq!(error.reason(), Some("ORDERS_OPEN"));
    assert_eq!(error.domain(), Some("orders.example.v1"));
    assert_eq!(
        error.metadata().get("orders").map(String::as_str),
        Some("3")
    );
}

#[tokio::test]
#[ignore = "needs Docker"]
#[allow(clippy::too_many_lines)]
async fn the_state_table_refuses_every_state_it_does_not_list() {
    let Some(cluster) = Cluster::start().await else {
        return;
    };
    common::guard(async {
        let (registry, pool, hooks) = set_up(&cluster).await;
        let provisioner = cluster.provisioner(&registry, &hooks);
        for (label, state) in [
            ("provisioning", TenantState::Provisioning),
            ("purging", TenantState::Purging),
            ("purged", TenantState::Purged),
        ] {
            let host = format!("{label}.example.com");
            let row = cluster
                .insert_row(&pool, label, &[host.as_str()], state)
                .await;
            let id = row.info.id;
            let suspended = provisioner.suspend(id).await.expect_err("refused");
            assert_refused(
                &suspended,
                ErrorCode::FailedPrecondition,
                reasons::TENANT_STATE,
            );
            let resumed = provisioner.resume(id).await.expect_err("refused");
            assert_refused(
                &resumed,
                ErrorCode::FailedPrecondition,
                reasons::TENANT_STATE,
            );
            let moved = provisioner
                .set_hosts(
                    id,
                    change(label, &["moved.example.com"], "moved.example.com"),
                )
                .await
                .expect_err("refused");
            assert_refused(&moved, ErrorCode::FailedPrecondition, reasons::TENANT_STATE);
            let usage = provisioner.usage(id).await.expect_err("refused");
            assert_refused(&usage, ErrorCode::FailedPrecondition, reasons::TENANT_STATE);
            let described = provisioner.describe(id).await.expect_err("refused");
            assert_refused(
                &described,
                ErrorCode::FailedPrecondition,
                reasons::TENANT_STATE,
            );
            let after = committed(&provisioner, id).await;
            assert_eq!(state_of(&after), state, "{label} is unchanged");
            assert_eq!(after.info.hosts, [host], "{label} keeps its hosts");
        }

        let unknown = common::new_tenant();
        let refusals = [
            provisioner.suspend(unknown).await.map(drop),
            provisioner.resume(unknown).await.map(drop),
            provisioner
                .set_hosts(
                    unknown,
                    change("orders", &["orders.example.com"], "orders.example.com"),
                )
                .await
                .map(drop),
            provisioner.purge(unknown).await.map(drop),
            provisioner.usage(unknown).await.map(drop),
            provisioner.describe(unknown).await.map(drop),
        ];
        for refusal in refusals {
            let error = refusal.expect_err("an unknown id is refused");
            assert_refused(&error, ErrorCode::NotFound, reasons::TENANT_NOT_FOUND);
        }
        assert_eq!(hooks.calls(), [] as [&str; 0], "no hook ran");
    })
    .await;
}

#[tokio::test]
#[ignore = "needs Docker"]
#[allow(clippy::too_many_lines)]
async fn suspend_and_resume_run_their_hooks_and_a_repeat_announces_again() {
    let Some(cluster) = Cluster::start().await else {
        return;
    };
    common::guard(async {
        let (registry, _pool, hooks) = set_up(&cluster).await;
        let sink = Arc::new(Recorded::default());
        let provisioner = cluster
            .provisioner_builder(&registry, &hooks)
            .sink(Arc::clone(&sink))
            .build()
            .expect("the provisioner settings are valid");
        let row = provision(&provisioner, "billing").await;
        let id = row.info.id;
        let before = sink.of(id).len();

        let resumed = provisioner.resume(id).await.expect("an active tenant");
        assert_eq!(state_of(&resumed), TenantState::Active);
        assert_eq!(resumed.updated_at, row.updated_at, "nothing changed");
        assert_eq!(hooks.count(BEFORE_RESUME), 0);
        assert_eq!(sink.of(id).len(), before + 1, "announced again");

        let suspended = provisioner.suspend(id).await.expect("suspended");
        assert_eq!(state_of(&suspended), TenantState::Suspended);
        assert_eq!(
            state_of(&committed(&provisioner, id).await),
            TenantState::Suspended
        );
        assert_eq!(hooks.count(BEFORE_SUSPEND), 1);
        assert_eq!(suspended.info.hosts, row.info.hosts);

        let again = provisioner.suspend(id).await.expect("already suspended");
        assert_eq!(state_of(&again), TenantState::Suspended);
        assert_eq!(again.updated_at, suspended.updated_at);
        assert_eq!(hooks.count(BEFORE_SUSPEND), 1, "no second hook");
        assert_eq!(
            sink.of(id)[before..],
            [ChangeKind::STATE, ChangeKind::STATE, ChangeKind::STATE]
        );

        let active = provisioner.resume(id).await.expect("resumed");
        assert_eq!(state_of(&active), TenantState::Active);
        assert_eq!(hooks.count(BEFORE_RESUME), 1);
        let again = provisioner.resume(id).await.expect("already active");
        assert_eq!(state_of(&again), TenantState::Active);
        assert_eq!(hooks.count(BEFORE_RESUME), 1, "no second hook");
        assert_eq!(sink.of(id).len(), before + 5);
        assert_eq!(
            state_of(&committed(&provisioner, id).await),
            TenantState::Active
        );
    })
    .await;
}

#[tokio::test]
#[ignore = "needs Docker"]
async fn a_suspend_of_a_tenant_whose_database_cannot_be_opened_keeps_it_active() {
    let Some(cluster) = Cluster::start().await else {
        return;
    };
    common::guard(async {
        let (registry, _pool, hooks) = set_up(&cluster).await;
        let provisioner = cluster.provisioner(&registry, &hooks);
        let row = provision(&provisioner, "billing").await;
        let id = row.info.id;
        let database = common::quote(&row.db_name);
        cluster
            .admin_execute(
                "postgres",
                &format!("ALTER DATABASE {database} ALLOW_CONNECTIONS false"),
            )
            .await;

        let error = provisioner.suspend(id).await.expect_err("strict");
        assert_eq!(error.code(), ErrorCode::Unavailable, "{error:?}");
        assert_eq!(error.reason(), None);
        assert_eq!(hooks.count(BEFORE_SUSPEND), 0, "the hook never ran");
        assert_eq!(
            state_of(&committed(&provisioner, id).await),
            TenantState::Active
        );

        cluster
            .admin_execute(
                "postgres",
                &format!("ALTER DATABASE {database} ALLOW_CONNECTIONS true"),
            )
            .await;
        let suspended = provisioner.suspend(id).await.expect("open again");
        assert_eq!(state_of(&suspended), TenantState::Suspended);
        assert_eq!(hooks.count(BEFORE_SUSPEND), 1);
    })
    .await;
}

#[tokio::test]
#[ignore = "needs Docker"]
async fn a_failing_hook_keeps_the_state_and_its_error_reaches_the_caller() {
    let Some(cluster) = Cluster::start().await else {
        return;
    };
    common::guard(async {
        let (registry, _pool, hooks) = set_up(&cluster).await;
        let provisioner = cluster.provisioner(&registry, &hooks);
        let id = provision(&provisioner, "billing").await.info.id;

        hooks.fail(BEFORE_SUSPEND, Some(orders_open));
        let error = provisioner.suspend(id).await.expect_err("the hook failed");
        assert_orders_open(&error);
        assert_eq!(
            state_of(&committed(&provisioner, id).await),
            TenantState::Active
        );
        hooks.fail(BEFORE_SUSPEND, None);
        provisioner.suspend(id).await.expect("suspended");

        hooks.fail(BEFORE_RESUME, Some(orders_open));
        let error = provisioner.resume(id).await.expect_err("the hook failed");
        assert_orders_open(&error);
        assert_eq!(
            state_of(&committed(&provisioner, id).await),
            TenantState::Suspended
        );
        hooks.fail(BEFORE_RESUME, None);
        let active = provisioner.resume(id).await.expect("resumed");
        assert_eq!(state_of(&active), TenantState::Active);
        assert_eq!(hooks.count(BEFORE_SUSPEND), 2);
        assert_eq!(hooks.count(BEFORE_RESUME), 2);
    })
    .await;
}

#[tokio::test]
#[ignore = "needs Docker"]
#[allow(clippy::too_many_lines)]
async fn set_hosts_replaces_the_label_and_the_whole_host_set() {
    let Some(cluster) = Cluster::start().await else {
        return;
    };
    common::guard(async {
        let (registry, pool, hooks) = set_up(&cluster).await;
        let sink = Arc::new(Recorded::default());
        let factory = OrdersFactory::new();
        let tenants = cluster.tenants(&registry, SelectorRules::new(), &factory, 8);
        let provisioner = cluster
            .provisioner_builder(&registry, &hooks)
            .sink(Arc::clone(&sink))
            .tenants(tenants.clone())
            .build()
            .expect("the provisioner settings are valid");
        let row = provision(&provisioner, "billing").await;
        let id = row.info.id;
        let snapshot = tenants
            .hosts()
            .expect("a keyed cache has a snapshot")
            .clone();
        assert_eq!(snapshot.tenant_of("billing.example.com"), Some(id));
        let before = sink.of(id).len();

        let moved = provisioner
            .set_hosts(
                id,
                change(
                    "Invoices",
                    &["WWW.invoices.example.com", "invoices.example.com."],
                    "Invoices.Example.Com",
                ),
            )
            .await
            .expect("the hosts were replaced");
        assert_eq!(moved.info.subdomain, "invoices");
        assert_eq!(
            moved.info.hosts,
            ["invoices.example.com", "www.invoices.example.com"]
        );
        assert_eq!(moved.info.primary_host, "invoices.example.com");
        assert_eq!(state_of(&moved), TenantState::Active);
        assert_eq!(committed(&provisioner, id).await.info, moved.info);
        assert_eq!(sink.of(id)[before..], [ChangeKind::HOSTS]);
        assert_eq!(snapshot.tenant_of("invoices.example.com"), Some(id));
        assert_eq!(snapshot.tenant_of("billing.example.com"), None);

        let invalid = provisioner
            .set_hosts(id, change("-invoices", &["not a host"], ""))
            .await
            .expect_err("invalid");
        assert_eq!(invalid.code(), ErrorCode::InvalidArgument);
        let fields: Vec<&str> = invalid
            .field_violations()
            .iter()
            .map(|violation| violation.field.as_str())
            .collect();
        assert_eq!(fields, ["subdomain", "hosts[0]", "primary_host"]);

        let other = cluster
            .insert_row(
                &pool,
                "orders",
                &["orders.example.com"],
                TenantState::Active,
            )
            .await;
        let taken = provisioner
            .set_hosts(
                id,
                change(
                    "invoices",
                    &["invoices.example.com", "orders.example.com"],
                    "invoices.example.com",
                ),
            )
            .await
            .expect_err("a host of another tenant");
        assert_refused(&taken, ErrorCode::FailedPrecondition, reasons::TENANT_STATE);
        assert_eq!(committed(&provisioner, id).await.info, moved.info);
        assert_eq!(
            committed(&provisioner, other.info.id).await.info.hosts,
            ["orders.example.com"]
        );
        assert_eq!(sink.of(id).len(), before + 1, "a refusal announces nothing");

        provisioner.suspend(id).await.expect("suspended");
        let suspended = provisioner
            .set_hosts(
                id,
                change("billing", &["billing.example.com"], "billing.example.com"),
            )
            .await
            .expect("a suspended tenant's hosts change too");
        assert_eq!(state_of(&suspended), TenantState::Suspended);
        assert_eq!(suspended.info.subdomain, "billing");
        assert_eq!(suspended.info.hosts, ["billing.example.com"]);
    })
    .await;
}

#[tokio::test]
#[ignore = "needs Docker"]
async fn usage_and_describe_read_active_and_suspended_tenants() {
    let Some(cluster) = Cluster::start().await else {
        return;
    };
    common::guard(async {
        let (registry, _pool, hooks) = set_up(&cluster).await;
        let provisioner = cluster.provisioner(&registry, &hooks);
        let id = provision(&provisioner, "billing").await.info.id;
        for state in [TenantState::Active, TenantState::Suspended] {
            assert_eq!(state_of(&committed(&provisioner, id).await), state);
            let usage = provisioner.usage(id).await.expect("measured");
            assert!(usage.database_bytes > 0, "{usage:?}");
            assert_eq!(
                usage.counters.counters,
                BTreeMap::from([("orders".to_owned(), 1)])
            );
            let description = provisioner.describe(id).await.expect("described");
            assert_eq!(description.display_name, "billing");
            assert_eq!(
                description.attributes,
                BTreeMap::from([("orders".to_owned(), "1".to_owned())])
            );
            if state == TenantState::Active {
                provisioner.suspend(id).await.expect("suspended");
            }
        }
        assert_eq!(hooks.count(USAGE), 2);
        assert_eq!(hooks.count(DESCRIBE), 2);
    })
    .await;
}

/// Hooks whose usage reads its session's settings and then tries to write.
struct Writing;

/// The SQLSTATE of a database error, or `""`.
fn sqlstate(error: &sqlx::Error) -> String {
    error
        .as_database_error()
        .and_then(sqlx::error::DatabaseError::code)
        .map(std::borrow::Cow::into_owned)
        .unwrap_or_default()
}

impl TenantHooks for Writing {
    type Seed = ();

    fn usage<'a>(
        &'a self,
        tenant: &'a TenantInfo,
        pool: &'a PgPool,
    ) -> HookFuture<'a, UsageCounters> {
        let _ = tenant;
        Box::pin(async move {
            let read_only: String = sqlx::query_scalar("SHOW transaction_read_only")
                .fetch_one(pool)
                .await
                .map_err(sekvent_db::to_app_error)?;
            let timeout: String = sqlx::query_scalar("SHOW statement_timeout")
                .fetch_one(pool)
                .await
                .map_err(sekvent_db::to_app_error)?;
            let refused = sqlx::query("INSERT INTO orders (item) VALUES ('written')")
                .execute(pool)
                .await
                .expect_err("a read-only session refuses the write");
            Err(AppError::failed_precondition("the write was refused")
                .with_metadata("sqlstate", sqlstate(&refused))
                .with_metadata("read_only", read_only)
                .with_metadata("timeout", timeout))
        })
    }
}

#[tokio::test]
#[ignore = "needs Docker"]
async fn a_usage_hook_that_writes_gets_the_read_only_error() {
    let Some(cluster) = Cluster::start().await else {
        return;
    };
    common::guard(async {
        let (registry, pool, _hooks) = set_up(&cluster).await;
        let provisioner = Provisioner::builder(
            registry.clone(),
            cluster.maintenance(),
            cluster.connector(),
            &TENANT_MIGRATOR,
            Sender::new(pool.clone(), cluster.channel()),
            Writing,
        )
        .role_key_id("k1")
        .connection_limit(20)
        .tenant_cap(100)
        .clock(Arc::new(common::manual_clock()))
        .build()
        .expect("the provisioner settings are valid");
        let id = common::new_tenant();
        provisioner
            .provision(ProvisionRequest {
                tenant: id,
                subdomain: "billing".to_owned(),
                display_name: "billing".to_owned(),
                hosts: vec!["billing.example.com".to_owned()],
                primary_host: "billing.example.com".to_owned(),
                seed: (),
            })
            .await
            .expect("the tenant was provisioned");

        let error = provisioner.usage(id).await.expect_err("the hook's error");
        assert_eq!(error.message(), "the write was refused");
        let metadata = error.metadata();
        assert_eq!(metadata.get("sqlstate").map(String::as_str), Some("25006"));
        assert_eq!(metadata.get("read_only").map(String::as_str), Some("on"));
        assert_eq!(metadata.get("timeout").map(String::as_str), Some("5s"));

        let mut admin = cluster
            .admin_on(&committed_db_name(&provisioner, id).await)
            .await;
        let orders: i64 = sqlx::query_scalar("SELECT count(*) FROM orders")
            .fetch_one(&mut admin)
            .await
            .expect("orders are readable");
        assert_eq!(orders, 0, "nothing was written");
    })
    .await;
}

async fn committed_db_name<H: TenantHooks>(provisioner: &Provisioner<H>, id: TenantId) -> String {
    provisioner
        .tenant(id)
        .await
        .expect("the tenant's row is readable")
        .db_name
}

#[tokio::test]
#[ignore = "needs Docker"]
#[allow(clippy::too_many_lines)]
async fn every_committed_change_is_seen_here_before_the_call_returns_and_reaches_another_process() {
    let Some(cluster) = Cluster::start().await else {
        return;
    };
    common::guard(async {
        let (registry, _pool, hooks) = set_up(&cluster).await;
        let factory = OrdersFactory::new();
        let tenants = cluster.tenants(&registry, SelectorRules::new(), &factory, 8);
        let provisioner = cluster
            .provisioner_builder(&registry, &hooks)
            .tenants(tenants.clone())
            .build()
            .expect("the provisioner settings are valid");
        let other = other_process(&cluster).await;
        let (id, handle, shutdown, mut heard) =
            provision_heard(&cluster, &provisioner, &other, "billing").await;
        drop(tenants.get(id).await.expect("the tenant opens here"));
        assert!(tenants.peek(id).is_some());

        provisioner.suspend(id).await.expect("suspended");
        assert!(tenants.peek(id).is_none(), "the scope was retired here");
        factory.dropped_at_least(1).await;
        let here = tenants.get(id).await.expect_err("suspended here");
        assert_eq!(here.reason(), Some(reasons::TENANT_SUSPENDED));
        assert_eq!(here.domain(), Some(tenants.domains().serving));
        next_state_change(&mut heard, id).await;
        other.factory.dropped_at_least(1).await;
        assert!(other.tenants.peek(id).is_none());
        let there = other.tenants.get(id).await.expect_err("suspended there");
        assert_eq!(there.reason(), Some(reasons::TENANT_SUSPENDED));

        provisioner.resume(id).await.expect("resumed");
        drop(tenants.get(id).await.expect("served here again"));
        provisioner
            .set_hosts(
                id,
                change(
                    "invoices",
                    &["invoices.example.com"],
                    "invoices.example.com",
                ),
            )
            .await
            .expect("the hosts were replaced");
        assert!(
            tenants.peek(id).is_none(),
            "a host change retires the scope"
        );
        let snapshot = tenants.hosts().expect("a keyed cache has a snapshot");
        assert_eq!(snapshot.tenant_of("invoices.example.com"), Some(id));
        assert_eq!(snapshot.tenant_of("billing.example.com"), None);
        stop(handle, shutdown).await;
    })
    .await;
}

/// A sink whose first armed change signals and then blocks its thread until released.
#[derive(Default)]
struct Blocking {
    armed: Mutex<Option<(oneshot::Sender<()>, std::sync::mpsc::Receiver<()>)>>,
}

impl Blocking {
    /// Arms the sink: the receiver resolves when a change arrived, the sender releases it (or
    /// its drop does).
    fn arm(&self) -> (oneshot::Receiver<()>, std::sync::mpsc::Sender<()>) {
        let (reached, arrived) = oneshot::channel();
        let (release, released) = std::sync::mpsc::channel();
        *self.armed.lock().unwrap_or_else(PoisonError::into_inner) = Some((reached, released));
        (arrived, release)
    }
}

impl Sink for Blocking {
    fn change(&self, tenant: TenantId, kind: &ChangeKind) {
        let _ = (tenant, kind);
        let armed = self
            .armed
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if let Some((reached, released)) = armed {
            let _ = reached.send(());
            let _ = released.recv();
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs Docker"]
async fn a_call_dropped_while_it_announces_still_announces() {
    let Some(cluster) = Cluster::start().await else {
        return;
    };
    common::guard(async {
        let (registry, _pool, hooks) = set_up(&cluster).await;
        let sink = Arc::new(Blocking::default());
        let provisioner = cluster
            .provisioner_builder(&registry, &hooks)
            .sink(Arc::clone(&sink))
            .build()
            .expect("the provisioner settings are valid");
        let other = other_process(&cluster).await;
        let (id, handle, shutdown, mut heard) =
            provision_heard(&cluster, &provisioner, &other, "billing").await;

        let (arrived, release) = sink.arm();
        let call = tokio::spawn({
            let provisioner = provisioner.clone();
            async move { provisioner.suspend(id).await }
        });
        arrived.await.expect("the sink heard the change");
        call.abort();
        let dropped = call.await.expect_err("the call was dropped");
        assert!(dropped.is_cancelled());
        assert_eq!(
            state_of(&committed(&provisioner, id).await),
            TenantState::Suspended,
            "the change was committed before the announce"
        );
        assert!(
            other.tenants.peek(id).is_some(),
            "nothing was sent before the sink was told"
        );
        release.send(()).expect("the sink waits for the release");

        next_state_change(&mut heard, id).await;
        assert!(other.tenants.peek(id).is_none());
        let there = other.tenants.get(id).await.expect_err("suspended there");
        assert_eq!(there.reason(), Some(reasons::TENANT_SUSPENDED));
        stop(handle, shutdown).await;
    })
    .await;
}

/// A provisioner announcing to a tenant cache of `capacity` over the test product, and the cache.
fn cached(
    cluster: &Cluster,
    registry: &Registry,
    hooks: &OrdersHooks,
    capacity: usize,
) -> (Provisioner<OrdersHooks>, Tenants<OrdersScope>) {
    let factory = OrdersFactory::new();
    let tenants = cluster.tenants(registry, SelectorRules::new(), &factory, capacity);
    let provisioner = cluster
        .provisioner_builder(registry, hooks)
        .tenants(tenants.clone())
        .build()
        .expect("the provisioner settings are valid");
    (provisioner, tenants)
}

#[tokio::test]
#[ignore = "needs Docker"]
#[allow(clippy::too_many_lines)]
async fn a_warm_active_tenant_lends_its_pool_and_a_cold_one_holds_a_cache_slot() {
    let Some(cluster) = Cluster::start().await else {
        return;
    };
    common::guard(async {
        let (registry, _pool, hooks) = set_up(&cluster).await;
        let (provisioner, tenants) = cached(&cluster, &registry, &hooks, 4);
        let row = provision(&provisioner, "billing").await;
        let id = row.info.id;
        drop(tenants.get(id).await.expect("the tenant opens"));
        let sessions = cluster.sessions_of(&row.db_role).await;
        assert!(sessions >= 1, "the warm pool keeps its connection");

        // Suspend on the warm tenant: its hook runs on the entry's pool, leased, no pool opened.
        hooks.hold(BEFORE_SUSPEND);
        let suspending = tokio::spawn({
            let provisioner = provisioner.clone();
            async move { provisioner.suspend(id).await }
        });
        hooks.arrived(1).await;
        let stats = tenants.stats();
        assert_eq!((stats.in_use, stats.reserved, stats.creations), (1, 0, 1));
        assert!(
            cluster.sessions_of(&row.db_role).await <= sessions,
            "no session was opened for the hook"
        );
        hooks.release();
        let suspended = suspending
            .await
            .expect("the call finished")
            .expect("suspended");
        assert_eq!(state_of(&suspended), TenantState::Suspended);
        assert!(tenants.peek(id).is_none(), "the entry was retired");

        // Usage on the suspended tenant: a pool of its own, holding a slot of the cache.
        hooks.hold(USAGE);
        let measuring = tokio::spawn({
            let provisioner = provisioner.clone();
            async move { provisioner.usage(id).await }
        });
        hooks.arrived(2).await;
        let stats = tenants.stats();
        assert_eq!((stats.warm, stats.reserved), (0, 1));
        hooks.release();
        measuring
            .await
            .expect("the call finished")
            .expect("measured");
        assert_eq!(tenants.stats().reserved, 0);

        // Resumed and warm again: usage and describe read on the entry's pool.
        provisioner.resume(id).await.expect("resumed");
        assert_eq!(tenants.stats().reserved, 0);
        drop(tenants.get(id).await.expect("served again"));
        hooks.hold(DESCRIBE);
        let describing = tokio::spawn({
            let provisioner = provisioner.clone();
            async move { provisioner.describe(id).await }
        });
        hooks.arrived(3).await;
        let stats = tenants.stats();
        assert_eq!((stats.in_use, stats.reserved), (1, 0));
        hooks.release();
        let description = describing
            .await
            .expect("the call finished")
            .expect("described");
        assert_eq!(
            description.attributes,
            BTreeMap::from([("orders".to_owned(), "1".to_owned())])
        );
        let usage = provisioner.usage(id).await.expect("measured warm");
        assert_eq!(
            usage.counters.counters,
            BTreeMap::from([("orders".to_owned(), 1)])
        );
        let stats = tenants.stats();
        assert_eq!((stats.warm, stats.in_use, stats.reserved), (1, 0, 0));
        tenants.close().await;
    })
    .await;
}

#[tokio::test]
#[ignore = "needs Docker"]
#[allow(clippy::too_many_lines)]
async fn a_lifecycle_open_when_the_cache_is_full_is_busy_and_opens_no_connection() {
    let Some(cluster) = Cluster::start().await else {
        return;
    };
    common::guard(async {
        let (registry, _pool, hooks) = set_up(&cluster).await;
        let (provisioner, tenants) = cached(&cluster, &registry, &hooks, 1);
        let first = provision(&provisioner, "billing").await.info.id;
        let second = provision(&provisioner, "orders").await;
        let id = second.info.id;
        let lease = tenants
            .get(first)
            .await
            .expect("the first tenant fills the cache");
        sekvent_testing::await_until!(cluster.sessions_of(&second.db_role).await == 0);
        // A connection to the second tenant's database would fail: the busy answer comes first.
        let database = common::quote(&second.db_name);
        cluster
            .admin_execute(
                "postgres",
                &format!("ALTER DATABASE {database} ALLOW_CONNECTIONS false"),
            )
            .await;
        let refusals = tenants.stats().capacity_refusals;

        let busy = [
            provisioner.suspend(id).await.map(drop),
            provisioner.usage(id).await.map(drop),
            provisioner.describe(id).await.map(drop),
        ];
        for answer in busy {
            let error = answer.expect_err("the cache is full");
            assert_eq!(error.code(), ErrorCode::Unavailable, "{error:?}");
            assert_eq!(error.reason(), Some(reasons::TENANT_BUSY));
            assert_eq!(error.domain(), None);
            assert_eq!(error.retry_after(), Some(Duration::from_secs(1)));
        }
        assert_eq!(tenants.stats().capacity_refusals, refusals + 3);
        assert_eq!(hooks.count(BEFORE_SUSPEND), 0);
        assert_eq!(hooks.count(USAGE), 0);
        assert_eq!(cluster.sessions_of(&second.db_role).await, 0);
        assert_eq!(
            state_of(&committed(&provisioner, id).await),
            TenantState::Active
        );

        // Once nothing holds the warm entry, the lifecycle pool takes its place.
        cluster
            .admin_execute(
                "postgres",
                &format!("ALTER DATABASE {database} ALLOW_CONNECTIONS true"),
            )
            .await;
        drop(lease);
        let suspended = provisioner.suspend(id).await.expect("room once evicted");
        assert_eq!(state_of(&suspended), TenantState::Suspended);
        assert!(tenants.peek(first).is_none(), "the idle entry was evicted");
        assert_eq!(tenants.stats().reserved, 0);
        tenants.close().await;
    })
    .await;
}
