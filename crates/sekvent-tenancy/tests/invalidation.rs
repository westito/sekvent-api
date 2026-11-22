//! The invalidation channel on a real server: two processes are two registries on two pools over
//! one control database, each with a tenant cache of its own; one sends, the other listens.
#![cfg(all(feature = "serving", feature = "lifecycle"))]

mod common;

use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Once};
use std::time::Duration;

use common::Cluster;
use common::product::{OrdersFactory, OrdersScope};
use sekvent_context::Clock;
use sekvent_error::{AppError, ErrorCode};
use sekvent_tenancy::invalidation::{self, ChangeKind, Channel, Listener, Sender, Sink, Topic};
use sekvent_tenancy::{Registry, SelectorRules, TenantId, TenantState, Tenants, reasons};
use sqlx::PgPool;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id, Record};
use tracing::subscriber::{Interest, Subscriber};
use tracing::{Level, Metadata};

/// One process: its control pool, its test product and its tenant cache.
struct Process {
    pool: PgPool,
    factory: OrdersFactory,
    tenants: Tenants<OrdersScope>,
}

fn process(cluster: &Cluster, registry: &Registry, pool: PgPool) -> Process {
    let factory = OrdersFactory::new();
    let tenants = cluster.tenants(registry, SelectorRules::new(), &factory, 8);
    Process {
        pool,
        factory,
        tenants,
    }
}

/// Process A applies the control schema; B has a pool of 4 of its own on the same database.
async fn two_processes(cluster: &Cluster) -> (Process, Process) {
    let clock: Arc<dyn Clock> = Arc::new(common::manual_clock());
    let (registry_a, pool_a) = cluster.registry(Arc::clone(&clock)).await;
    let pool_b = cluster.control_pool_of(4).await;
    let registry_b = Registry::new(pool_b.clone(), clock).expect("a pool of 4 is enough");
    (
        process(cluster, &registry_a, pool_a),
        process(cluster, &registry_b, pool_b),
    )
}

fn channel() -> Channel {
    Channel::new("app_changes").expect("a valid channel name")
}

/// What a listener's sink was told.
#[derive(Debug, PartialEq, Eq)]
enum Heard {
    Change(TenantId, ChangeKind),
    Hint(TenantId, Topic, i64),
    Resync(Vec<TenantId>),
    Reconnected,
}

/// A sink that forwards every call into a channel.
struct Forward(mpsc::UnboundedSender<Heard>);

impl Sink for Forward {
    fn change(&self, tenant: TenantId, kind: &ChangeKind) {
        let _ = self.0.send(Heard::Change(tenant, kind.clone()));
    }

    fn hint(&self, tenant: TenantId, topic: &Topic, seq: i64) {
        let _ = self.0.send(Heard::Hint(tenant, topic.clone(), seq));
    }

    fn resync(&self, changed: &[TenantId]) {
        let _ = self.0.send(Heard::Resync(changed.to_vec()));
    }

    fn reconnected(&self) {
        let _ = self.0.send(Heard::Reconnected);
    }
}

fn forward() -> (Forward, mpsc::UnboundedReceiver<Heard>) {
    let (tx, rx) = mpsc::unbounded_channel();
    (Forward(tx), rx)
}

async fn next(heard: &mut mpsc::UnboundedReceiver<Heard>) -> Heard {
    heard.recv().await.expect("the listener is running")
}

/// A listener on `process`'s tenant cache, telling `sink`.
fn listening(process: &Process, sink: Forward) -> Listener<OrdersScope> {
    Listener::new(process.tenants.clone(), channel()).sink(sink)
}

type Run = JoinHandle<Result<(), AppError>>;

/// Runs `listener` on a task of its own; returns once it called `ready`.
async fn start(listener: Listener<OrdersScope>) -> (Run, CancellationToken) {
    let (ready, live) = oneshot::channel();
    let shutdown = CancellationToken::new();
    let handle = tokio::spawn(listener.run(
        move || {
            let _ = ready.send(());
        },
        shutdown.clone(),
    ));
    live.await.expect("the listener called ready");
    (handle, shutdown)
}

async fn stop(handle: Run, shutdown: CancellationToken) {
    shutdown.cancel();
    handle
        .await
        .expect("the listener did not panic")
        .expect("the listener ended without an error");
}

fn all_idle(pool: &PgPool) -> bool {
    usize::try_from(pool.size()).is_ok_and(|size| size == pool.num_idle())
}

#[tokio::test]
#[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
async fn a_change_sent_by_one_process_drops_the_scope_on_the_other() {
    let Some(cluster) = Cluster::start().await else {
        return;
    };
    common::guard(async {
        let (a, b) = two_processes(&cluster).await;
        let row = cluster
            .create_tenant(&a.pool, "billing", &["billing.example.com"])
            .await;
        let id = row.info.id;
        drop(b.tenants.get(id).await.expect("the tenant opens on B"));
        assert!(b.tenants.peek(id).is_some());
        let (sink, mut heard) = forward();
        let (handle, shutdown) = start(listening(&b, sink)).await;

        cluster.set_state(&a.pool, id, TenantState::Suspended).await;
        Sender::new(a.pool.clone(), channel()).change(id, &ChangeKind::STATE);

        assert_eq!(next(&mut heard).await, Heard::Change(id, ChangeKind::STATE));
        b.factory.dropped_at_least(1).await;
        assert!(b.tenants.peek(id).is_none());
        let refused = b
            .tenants
            .get(id)
            .await
            .expect_err("a suspended tenant is refused");
        assert_eq!(refused.reason(), Some(reasons::TENANT_SUSPENDED));
        stop(handle, shutdown).await;
    })
    .await;
}

#[tokio::test]
#[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
async fn a_host_change_reaches_the_snapshot_before_the_sink() {
    let Some(cluster) = Cluster::start().await else {
        return;
    };
    common::guard(async {
        let (a, b) = two_processes(&cluster).await;
        let row = cluster
            .insert_row(
                &a.pool,
                "billing",
                &["billing.example.com"],
                TenantState::Active,
            )
            .await;
        let id = row.info.id;
        let (sink, mut heard) = forward();
        let (handle, shutdown) = start(listening(&b, sink)).await;
        let snapshot = b
            .tenants
            .hosts()
            .expect("a keyed cache has a snapshot")
            .clone();
        assert_eq!(snapshot.tenant_of("billing.example.com"), Some(id));
        assert_eq!(snapshot.tenant_of("orders.example.com"), None);

        cluster
            .set_hosts(
                &a.pool,
                id,
                "billing",
                &["orders.example.com", "billing.example.com"],
            )
            .await;
        Sender::new(a.pool.clone(), channel()).change(id, &ChangeKind::HOSTS);

        assert_eq!(next(&mut heard).await, Heard::Change(id, ChangeKind::HOSTS));
        assert_eq!(snapshot.tenant_of("orders.example.com"), Some(id));
        assert_eq!(snapshot.tenant_of("billing.example.com"), Some(id));
        stop(handle, shutdown).await;
    })
    .await;
}

#[tokio::test]
#[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
async fn what_is_queued_while_the_sender_waits_for_a_connection_arrives_coalesced() {
    let Some(cluster) = Cluster::start().await else {
        return;
    };
    common::guard(async {
        let (_a, b) = two_processes(&cluster).await;
        let (sink, mut heard) = forward();
        let (handle, shutdown) = start(listening(&b, sink)).await;

        let pool = cluster.control_pool_of(1).await;
        let held = pool.acquire().await.expect("the pool's one connection");
        let sender = Sender::new(pool.clone(), channel());
        let (first, second) = (common::new_tenant(), common::new_tenant());
        let orders = Topic::new("orders").expect("a valid topic");
        sender.hint(first, &orders, 4);
        sender.change(first, &ChangeKind::STATE);
        sender.hint(first, &orders, 9);
        sender.change(second, &ChangeKind::HOSTS);
        sender.change(first, &ChangeKind::STATE);
        sender.hint(first, &orders, 7);
        drop(held);

        assert_eq!(
            next(&mut heard).await,
            Heard::Change(first, ChangeKind::STATE)
        );
        assert_eq!(
            next(&mut heard).await,
            Heard::Change(second, ChangeKind::HOSTS)
        );
        assert_eq!(next(&mut heard).await, Heard::Hint(first, orders, 9));
        stop(handle, shutdown).await;
        assert!(heard.try_recv().is_err());
    })
    .await;
}

#[tokio::test]
#[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
async fn a_lost_notification_is_repaired_by_the_next_successful_resync() {
    let Some(cluster) = Cluster::start().await else {
        return;
    };
    common::guard(async {
        let (a, b) = two_processes(&cluster).await;
        let row = cluster
            .create_tenant(&a.pool, "billing", &["billing.example.com"])
            .await;
        let id = row.info.id;
        drop(b.tenants.get(id).await.expect("the tenant opens on B"));
        let (sink, mut heard) = forward();
        let listener = Listener::new(b.tenants.clone(), channel())
            .sink(sink)
            .resync_every(Duration::from_secs(1))
            .expect("a positive interval");
        let (handle, shutdown) = start(listener).await;

        cluster.set_state(&a.pool, id, TenantState::Suspended).await;
        loop {
            match next(&mut heard).await {
                Heard::Resync(changed) if changed.contains(&id) => {
                    assert_eq!(changed, [id]);
                    break;
                }
                Heard::Resync(changed) => assert_eq!(changed, Vec::<TenantId>::new()),
                other => panic!("only resyncs are expected, got {other:?}"),
            }
        }
        b.factory.dropped_at_least(1).await;
        let refused = b
            .tenants
            .get(id)
            .await
            .expect_err("a suspended tenant is refused");
        assert_eq!(refused.reason(), Some(reasons::TENANT_SUSPENDED));
        stop(handle, shutdown).await;
    })
    .await;
}

#[tokio::test]
#[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
async fn a_lost_listening_session_is_replaced_and_changes_arrive_again() {
    let Some(cluster) = Cluster::start().await else {
        return;
    };
    common::guard(async {
        let (a, b) = two_processes(&cluster).await;
        let (sink, mut heard) = forward();
        let (handle, shutdown) = start(listening(&b, sink)).await;

        let mut admin = cluster.admin().await;
        let ended: Vec<bool> = sqlx::query_scalar(
            "SELECT pg_terminate_backend(pid) FROM pg_stat_activity \
             WHERE datname = $1 AND usename = $2 AND query LIKE 'LISTEN%'",
        )
        .bind(cluster.control_database())
        .bind(cluster.control_owner())
        .fetch_all(&mut admin)
        .await
        .expect("pg_stat_activity is readable");
        assert_eq!(ended, [true], "exactly the listening session was ended");

        loop {
            match next(&mut heard).await {
                Heard::Reconnected => break,
                Heard::Resync(_) => {}
                other => panic!("a resync and the reconnect are expected, got {other:?}"),
            }
        }
        let tenant = common::new_tenant();
        Sender::new(a.pool.clone(), channel()).change(tenant, &ChangeKind::STATE);
        assert_eq!(
            next(&mut heard).await,
            Heard::Change(tenant, ChangeKind::STATE)
        );
        stop(handle, shutdown).await;
    })
    .await;
}

/// Signals every failed read of the invalidation channel logged on this thread when `failed` is
/// set; the global default (no `failed`) records nothing but keeps every callsite's interest open.
struct FailedReads {
    failed: Option<mpsc::UnboundedSender<()>>,
}

impl Subscriber for FailedReads {
    fn register_callsite(&self, _metadata: &'static Metadata<'static>) -> Interest {
        Interest::sometimes()
    }

    fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        self.failed.is_some() && metadata.target().starts_with("sekvent_tenancy")
    }

    fn new_span(&self, _span: &Attributes<'_>) -> Id {
        Id::from_u64(1)
    }

    fn record(&self, _span: &Id, _values: &Record<'_>) {}

    fn record_follows_from(&self, _span: &Id, _follows: &Id) {}

    fn event(&self, event: &tracing::Event<'_>) {
        let mut message = MessageText::default();
        event.record(&mut message);
        if let Some(failed) = &self.failed
            && *event.metadata().level() == Level::WARN
            && message
                .0
                .starts_with("reading the invalidation channel failed")
        {
            let _ = failed.send(());
        }
    }

    fn enter(&self, _span: &Id) {}

    fn exit(&self, _span: &Id) {}
}

#[derive(Default)]
struct MessageText(String);

impl Visit for MessageText {
    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        if field.name() == "message" {
            self.0 = format!("{value:?}");
        }
    }
}

/// Starts watching this thread's failed channel reads until the guard drops.
fn watch_failed_reads() -> (
    mpsc::UnboundedReceiver<()>,
    tracing::subscriber::DefaultGuard,
) {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        tracing::subscriber::set_global_default(FailedReads { failed: None })
            .expect("no other test installs a global subscriber");
    });
    let (tx, rx) = mpsc::unbounded_channel();
    let guard = tracing::subscriber::set_default(FailedReads { failed: Some(tx) });
    (rx, guard)
}

#[tokio::test]
#[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
async fn a_reconnect_after_failed_attempts_is_reported_too() {
    let Some(cluster) = Cluster::start().await else {
        return;
    };
    // The listener's task runs on this thread (a current-thread runtime), so its events reach
    // this thread's subscriber.
    let (mut failures, _watch) = watch_failed_reads();
    common::guard(async {
        let (a, b) = two_processes(&cluster).await;
        let (sink, mut heard) = forward();
        let (handle, shutdown) = start(listening(&b, sink)).await;

        // No session of the control owner can be opened, and none is left: the read fails or
        // reports the loss, and listening again on a new connection fails, again and again.
        let owner = common::quote(cluster.control_owner());
        let control = cluster.control_database();
        cluster
            .admin_execute(control, &format!("ALTER ROLE {owner} NOLOGIN"))
            .await;
        let mut admin = cluster.admin().await;
        let ended: Vec<bool> = sqlx::query_scalar(
            "SELECT pg_terminate_backend(pid) FROM pg_stat_activity \
             WHERE datname = $1 AND usename = $2",
        )
        .bind(control)
        .bind(cluster.control_owner())
        .fetch_all(&mut admin)
        .await
        .expect("pg_stat_activity is readable");
        assert!(ended.contains(&true), "the owner's sessions were ended");
        failures
            .recv()
            .await
            .expect("a failed read or attempt to listen again was logged");
        // After the wait, the listener tries again before it reads: that fails as well, and is
        // retried like a failed read.
        failures
            .recv()
            .await
            .expect("a failed attempt to listen again was logged");
        assert!(heard.try_recv().is_err(), "nothing reached the sink yet");

        cluster
            .admin_execute(control, &format!("ALTER ROLE {owner} LOGIN"))
            .await;
        loop {
            match next(&mut heard).await {
                Heard::Reconnected => break,
                Heard::Resync(_) => {}
                other => panic!("a resync and the reconnect are expected, got {other:?}"),
            }
        }
        let tenant = common::new_tenant();
        Sender::new(a.pool.clone(), channel()).change(tenant, &ChangeKind::STATE);
        assert_eq!(
            next(&mut heard).await,
            Heard::Change(tenant, ChangeKind::STATE)
        );
        stop(handle, shutdown).await;
    })
    .await;
}

/// A sink that forwards every call and, at each change, says whether the tenant's scope was
/// still in the cache at that moment.
struct WarmAtChange {
    tenants: Tenants<OrdersScope>,
    forward: Forward,
    warm: mpsc::UnboundedSender<bool>,
}

impl Sink for WarmAtChange {
    fn change(&self, tenant: TenantId, kind: &ChangeKind) {
        let _ = self.warm.send(self.tenants.peek(tenant).is_some());
        self.forward.change(tenant, kind);
    }

    fn hint(&self, tenant: TenantId, topic: &Topic, seq: i64) {
        self.forward.hint(tenant, topic, seq);
    }

    fn resync(&self, changed: &[TenantId]) {
        self.forward.resync(changed);
    }

    fn reconnected(&self) {
        self.forward.reconnected();
    }
}

#[tokio::test]
#[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
async fn a_retired_pool_waiting_for_a_held_connection_never_stalls_the_listener() {
    let Some(cluster) = Cluster::start().await else {
        return;
    };
    common::guard(async {
        let (a, b) = two_processes(&cluster).await;
        let row = cluster
            .create_tenant(&a.pool, "billing", &["billing.example.com"])
            .await;
        let id = row.info.id;
        let lease = b.tenants.get(id).await.expect("the tenant opens on B");
        // A product task keeps a connection of the tenant's pool after the lease is gone.
        let held = lease
            .pool
            .acquire()
            .await
            .expect("a connection of the tenant's pool");
        drop(lease);
        let (forward, mut heard) = forward();
        let (warm_tx, mut warm_at_change) = mpsc::unbounded_channel();
        let sink = WarmAtChange {
            tenants: b.tenants.clone(),
            forward,
            warm: warm_tx,
        };
        let listener = Listener::new(b.tenants.clone(), channel()).sink(sink);
        let (handle, shutdown) = start(listener).await;

        let sender = Sender::new(a.pool.clone(), channel());
        sender.change(id, &ChangeKind::STATE);
        assert_eq!(next(&mut heard).await, Heard::Change(id, ChangeKind::STATE));
        assert_eq!(
            warm_at_change.recv().await,
            Some(false),
            "the scope is out of the cache before the sink is told"
        );
        b.factory.dropped_at_least(1).await;
        assert!(b.tenants.peek(id).is_none());
        // Closing the retired pool waits for the held connection; the listener goes on.
        let orders = Topic::new("orders").expect("a valid topic");
        sender.hint(id, &orders, 1);
        assert_eq!(next(&mut heard).await, Heard::Hint(id, orders, 1));
        assert_eq!(b.tenants.stats().retiring, 1);

        drop(held);
        sekvent_testing::await_until!(b.tenants.stats().retiring == 0);
        stop(handle, shutdown).await;
    })
    .await;
}

#[tokio::test]
#[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
async fn ready_is_called_once_the_listener_hears_the_channel() {
    let Some(cluster) = Cluster::start().await else {
        return;
    };
    common::guard(async {
        let (a, b) = two_processes(&cluster).await;
        let (sink, mut heard) = forward();
        let (ready, live) = oneshot::channel();
        let shutdown = CancellationToken::new();
        let handle = tokio::spawn(invalidation::listen(
            b.tenants.clone(),
            channel(),
            sink,
            move || {
                let _ = ready.send(());
            },
            shutdown.clone(),
        ));
        live.await.expect("the listener called ready");

        let tenant = common::new_tenant();
        let plan = ChangeKind::new("plan").expect("a valid kind");
        Sender::new(a.pool.clone(), channel()).change(tenant, &plan);
        assert_eq!(next(&mut heard).await, Heard::Change(tenant, plan));
        stop(handle, shutdown).await;
    })
    .await;
}

#[tokio::test]
#[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
async fn a_listener_that_cannot_listen_is_an_error_and_never_ready() {
    let Some(cluster) = Cluster::start().await else {
        return;
    };
    common::guard(async {
        let (_a, b) = two_processes(&cluster).await;
        b.pool.close().await;
        let called = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&called);
        let error = Listener::new(b.tenants.clone(), channel())
            .run(
                move || flag.store(true, Ordering::SeqCst),
                CancellationToken::new(),
            )
            .await
            .expect_err("a closed pool cannot listen");
        assert_eq!(error.code(), ErrorCode::Unavailable);
        assert!(!called.load(Ordering::SeqCst));

        let flag = Arc::clone(&called);
        let error = Listener::new(Tenants::fixed(()), channel())
            .run(
                move || flag.store(true, Ordering::SeqCst),
                CancellationToken::new(),
            )
            .await
            .expect_err("a fixed scope has nothing to listen for");
        assert_eq!(error.code(), ErrorCode::FailedPrecondition);
        assert!(!called.load(Ordering::SeqCst));
    })
    .await;
}

#[tokio::test]
#[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
async fn closing_the_control_pool_ends_a_running_listener() {
    let Some(cluster) = Cluster::start().await else {
        return;
    };
    common::guard(async {
        let (_a, b) = two_processes(&cluster).await;
        let (handle, _shutdown) = start(Listener::new(b.tenants.clone(), channel())).await;

        let pool = b.pool.clone();
        let closing = tokio::spawn(async move { pool.close().await });
        let error = handle
            .await
            .expect("the listener did not panic")
            .expect_err("a closed pool ends the listener");
        assert_eq!(error.code(), ErrorCode::Unavailable);
        closing.await.expect("the pool closed");
        assert!(b.pool.is_closed());
    })
    .await;
}

#[tokio::test]
#[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
async fn shutdown_ends_the_listener_and_gives_its_connection_back() {
    let Some(cluster) = Cluster::start().await else {
        return;
    };
    common::guard(async {
        let (_a, b) = two_processes(&cluster).await;
        let (handle, shutdown) = start(Listener::new(b.tenants.clone(), channel())).await;
        assert!(!all_idle(&b.pool), "the listener holds a connection");

        stop(handle, shutdown).await;
        sekvent_testing::await_until!(all_idle(&b.pool));
    })
    .await;
}

#[cfg(feature = "runtime")]
#[tokio::test]
#[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
async fn the_listener_runs_as_a_runtime_unit() {
    use sekvent_runtime::{Runtime, Stage, UnitExit, UnitPolicy};

    const UNIT: &str = "invalidation";

    let Some(cluster) = Cluster::start().await else {
        return;
    };
    common::guard(async {
        let (a, b) = two_processes(&cluster).await;
        let (sink, mut heard) = forward();
        let listener = Listener::new(b.tenants.clone(), channel()).sink(sink);
        let handle = Runtime::builder()
            .without_signals()
            .unit(
                UNIT,
                Stage::Infrastructure,
                UnitPolicy::Critical,
                listener.into_unit(),
            )
            .build()
            .expect("the runtime is valid")
            .start()
            .await
            .expect("the runtime started");

        let tenant = common::new_tenant();
        Sender::new(a.pool.clone(), channel()).change(tenant, &ChangeKind::STATE);
        assert_eq!(
            next(&mut heard).await,
            Heard::Change(tenant, ChangeKind::STATE)
        );

        handle.shutdown();
        let report = handle.wait().await.expect("the runtime stopped");
        assert_eq!(
            report.unit(UNIT).expect("the unit ran").exit,
            UnitExit::Completed
        );
    })
    .await;
}

#[tokio::test]
#[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
async fn a_payload_that_is_no_message_is_ignored_and_the_listener_goes_on() {
    let Some(cluster) = Cluster::start().await else {
        return;
    };
    common::guard(async {
        let (a, b) = two_processes(&cluster).await;
        let (sink, mut heard) = forward();
        let (handle, shutdown) = start(listening(&b, sink)).await;

        cluster
            .admin_execute(
                cluster.control_database(),
                "SELECT pg_notify('app_changes', 'not a message')",
            )
            .await;
        let tenant = common::new_tenant();
        Sender::new(a.pool.clone(), channel()).change(tenant, &ChangeKind::STATE);
        assert_eq!(
            next(&mut heard).await,
            Heard::Change(tenant, ChangeKind::STATE)
        );
        stop(handle, shutdown).await;
    })
    .await;
}

#[tokio::test]
#[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
async fn shutdown_while_waiting_for_the_first_connection_ends_the_listener_unready() {
    let Some(cluster) = Cluster::start().await else {
        return;
    };
    common::guard(async {
        let (_a, b) = two_processes(&cluster).await;
        let mut held = Vec::new();
        for _ in 0..4 {
            held.push(
                b.pool
                    .acquire()
                    .await
                    .expect("a connection of the pool of 4"),
            );
        }
        let shutdown = CancellationToken::new();
        shutdown.cancel();
        let called = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&called);
        Listener::new(b.tenants.clone(), channel())
            .run(move || flag.store(true, Ordering::SeqCst), shutdown)
            .await
            .expect("shutdown ends the listener without an error");
        assert!(!called.load(Ordering::SeqCst));
        assert_eq!(held.len(), 4);
        drop(held);
        sekvent_testing::await_until!(all_idle(&b.pool));
    })
    .await;
}

#[tokio::test]
#[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
async fn two_failed_resyncs_empty_the_host_snapshot_and_a_successful_one_fills_it() {
    let Some(cluster) = Cluster::start().await else {
        return;
    };
    common::guard(async {
        let (a, b) = two_processes(&cluster).await;
        let row = cluster
            .insert_row(
                &a.pool,
                "billing",
                &["billing.example.com"],
                TenantState::Active,
            )
            .await;
        let id = row.info.id;
        let (sink, mut heard) = forward();
        let listener = listening(&b, sink)
            .resync_every(Duration::from_millis(100))
            .expect("a positive interval");
        let (handle, shutdown) = start(listener).await;
        let snapshot = b
            .tenants
            .hosts()
            .expect("a keyed cache has a snapshot")
            .clone();
        assert_eq!(snapshot.tenant_of("billing.example.com"), Some(id));

        // The active tenants cannot be read: every resync fails from now on.
        let owner = common::quote(cluster.control_owner());
        let control = cluster.control_database();
        cluster
            .admin_execute(
                control,
                &format!("REVOKE SELECT ON tenant_hosts FROM {owner}"),
            )
            .await;
        sekvent_testing::await_until!(snapshot.tenant_of("billing.example.com").is_none());
        // What the sink heard came from resyncs that succeeded before the revoke.
        while let Ok(earlier) = heard.try_recv() {
            assert_eq!(earlier, Heard::Resync(Vec::new()));
        }

        cluster
            .admin_execute(control, &format!("GRANT SELECT ON tenant_hosts TO {owner}"))
            .await;
        assert_eq!(next(&mut heard).await, Heard::Resync(Vec::new()));
        assert_eq!(snapshot.tenant_of("billing.example.com"), Some(id));
        stop(handle, shutdown).await;
    })
    .await;
}
