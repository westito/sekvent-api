//! The registry's lookups and their admission on a real server: the cache, the lookup permits,
//! joining, a caller that goes away, internal reads and the loopback path label.
#![cfg(feature = "serving")]

mod common;

use std::pin::pin;
use std::sync::Arc;
use std::time::Duration;

use common::Cluster;
use sekvent_context::ManualClock;
use sekvent_error::{AppError, ErrorCode};
use sekvent_tenancy::{Registry, Selector, TenantRow, TenantState, reasons};
use sqlx::pool::PoolConnection;
use sqlx::{Connection, PgPool, Postgres};

/// A registry on a control pool of 3 connections (one lookup permit), that pool, a pool of the
/// fixture's own for writing rows, and the registry's clock.
async fn small_registry(cluster: &Cluster) -> (Registry, PgPool, PgPool, ManualClock) {
    cluster.apply_control_schema().await;
    let pool = cluster.control_pool_of(3).await;
    let clock = common::manual_clock();
    let registry =
        Registry::new(pool.clone(), Arc::new(clock.clone())).expect("a pool of 3 is enough");
    let writer = cluster.control_pool().await;
    (registry, pool, writer, clock)
}

/// Every connection of `pool`, held by the test until the vector is dropped.
async fn hold_all(pool: &PgPool) -> Vec<PoolConnection<Postgres>> {
    let mut held = Vec::new();
    for _ in 0..pool.options().get_max_connections() {
        held.push(pool.acquire().await.expect("a control connection"));
    }
    held
}

/// Sessions of the control owner waiting on a lock, from `pg_stat_activity`.
async fn lock_waiters(cluster: &Cluster) -> i64 {
    let mut admin = cluster.admin().await;
    let waiting: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM pg_stat_activity WHERE usename = $1 AND wait_event_type = 'Lock'",
    )
    .bind(cluster.control_owner())
    .fetch_one(&mut admin)
    .await
    .expect("pg_stat_activity is readable");
    admin.close().await.expect("the admin session closed");
    waiting
}

fn assert_busy(error: &AppError) {
    assert_eq!(error.code(), ErrorCode::Unavailable);
    assert_eq!(error.reason(), Some(reasons::TENANT_BUSY));
    assert_eq!(error.domain(), None);
    assert_eq!(error.retry_after(), Some(Duration::from_secs(1)));
}

fn by_label(segment: &str, listener_host: &str) -> Selector {
    Selector::PathLabel {
        segment: segment.to_owned(),
        listener_host: listener_host.to_owned(),
    }
}

fn by_host(host: &str) -> Selector {
    Selector::Host(host.to_owned())
}

/// The state of the row `selector` finds.
async fn state_of(registry: &Registry, selector: &Selector) -> TenantState {
    registry
        .lookup(selector)
        .await
        .expect("the lookup succeeded")
        .expect("the tenant exists")
        .info
        .state
}

#[tokio::test]
#[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
async fn a_lookup_by_id_host_or_path_label_returns_the_row_in_every_state() {
    let Some(cluster) = Cluster::start().await else {
        return;
    };
    common::guard(async {
        let (registry, control) = cluster.registry(Arc::new(common::manual_clock())).await;
        for (label, state) in [
            ("provisioning", TenantState::Provisioning),
            ("active", TenantState::Active),
            ("suspended", TenantState::Suspended),
            ("purging", TenantState::Purging),
            ("purged", TenantState::Purged),
        ] {
            let host = format!("{label}.example.com");
            let row = cluster.insert_row(&control, label, &[&host], state).await;
            assert_eq!(row.info.state, state);
            let expected: Option<&TenantRow> = Some(&row);

            let by_id = registry.lookup(&Selector::Id(row.info.id)).await.unwrap();
            assert_eq!(by_id.as_ref(), expected, "{label} by id");
            let found = registry.lookup(&by_host(&host)).await.unwrap();
            assert_eq!(found.as_ref(), expected, "{label} by host");
            let labelled = registry
                .lookup(&by_label(label, "app.example.com"))
                .await
                .unwrap();
            assert_eq!(labelled.as_ref(), expected, "{label} by path label");
        }

        let unknown = registry.lookup(&Selector::Id(common::new_tenant())).await;
        assert_eq!(unknown.unwrap(), None);
        let unknown = registry.lookup(&by_host("unknown.example.com")).await;
        assert_eq!(unknown.unwrap(), None);
        let unknown = registry
            .lookup(&by_label("unknown", "app.example.com"))
            .await;
        assert_eq!(unknown.unwrap(), None);
        let internal = registry.tenant(common::new_tenant()).await;
        assert_eq!(internal.unwrap(), None);
    })
    .await;
}

#[tokio::test]
#[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
async fn with_the_lookup_permit_held_a_lookup_through_a_clone_is_busy_without_a_connection() {
    let Some(cluster) = Cluster::start().await else {
        return;
    };
    common::guard(async {
        let (registry, pool, writer, _clock) = small_registry(&cluster).await;
        let active = TenantState::Active;
        let billing = cluster
            .insert_row(&writer, "billing", &["billing.example.com"], active)
            .await;
        let orders = cluster
            .insert_row(&writer, "orders", &["orders.example.com"], active)
            .await;

        // The one lookup permit is held by a read that waits on a table lock, not in the pool:
        // the registry's pool can still hand out and open connections.
        let mut lock = writer.begin().await.expect("a control transaction began");
        sqlx::query("LOCK TABLE tenants IN ACCESS EXCLUSIVE MODE")
            .execute(&mut *lock)
            .await
            .expect("the table is locked");
        let first_selector = Selector::Id(billing.info.id);
        let mut first = pin!(registry.lookup(&first_selector));
        assert!(
            futures::poll!(first.as_mut()).is_pending(),
            "its read waits on the lock"
        );
        sekvent_testing::await_until!(lock_waiters(&cluster).await > 0);

        let idle = pool.num_idle();
        let size = pool.size();
        assert!(
            size < pool.options().get_max_connections(),
            "the pool could open another connection"
        );
        let before = cluster.sessions_of(cluster.control_owner()).await;
        let clone = registry.clone();
        let refused = tokio::time::timeout(
            Duration::from_secs(10),
            clone.lookup(&Selector::Id(orders.info.id)),
        )
        .await
        .expect("the refused lookup did not wait on the lock");
        assert_busy(&refused.expect_err("no lookup permit is free"));
        assert_eq!(pool.num_idle(), idle, "no idle connection was taken");
        assert_eq!(pool.size(), size, "no connection was opened");
        let after = cluster.sessions_of(cluster.control_owner()).await;
        assert_eq!(before, after, "the refused lookup took no connection");

        lock.rollback().await.expect("the lock was released");
        assert_eq!(first.await.unwrap().as_ref(), Some(&billing));
    })
    .await;
}

#[tokio::test]
#[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
async fn a_second_caller_of_the_same_key_through_a_clone_joins_the_read() {
    let Some(cluster) = Cluster::start().await else {
        return;
    };
    common::guard(async {
        let (registry, pool, writer, _clock) = small_registry(&cluster).await;
        let billing = cluster
            .insert_row(
                &writer,
                "billing",
                &["billing.example.com"],
                TenantState::Active,
            )
            .await;
        let selector = Selector::Id(billing.info.id);
        let clone = registry.clone();

        let held = hold_all(&pool).await;
        let mut first = pin!(registry.lookup(&selector));
        let mut joiner = pin!(clone.lookup(&selector));
        assert!(futures::poll!(first.as_mut()).is_pending());
        assert!(
            futures::poll!(joiner.as_mut()).is_pending(),
            "it joined, it is not busy"
        );
        drop(held);
        assert_eq!(first.await.unwrap().as_ref(), Some(&billing));
        assert_eq!(joiner.await.unwrap().as_ref(), Some(&billing));

        // With every connection held, only the cache can answer.
        let held = hold_all(&pool).await;
        assert_eq!(
            registry.lookup(&selector).await.unwrap().as_ref(),
            Some(&billing)
        );
        assert_eq!(
            clone.lookup(&selector).await.unwrap().as_ref(),
            Some(&billing)
        );
        drop(held);
    })
    .await;
}

#[tokio::test]
#[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
async fn a_caller_that_goes_away_leaves_its_read_to_fill_the_cache() {
    let Some(cluster) = Cluster::start().await else {
        return;
    };
    common::guard(async {
        let (registry, pool, writer, _clock) = small_registry(&cluster).await;
        let billing = cluster
            .insert_row(
                &writer,
                "billing",
                &["billing.example.com"],
                TenantState::Active,
            )
            .await;
        let selector = by_host("billing.example.com");

        let held = hold_all(&pool).await;
        let mut caller = Box::pin(registry.lookup(&selector));
        assert!(futures::poll!(caller.as_mut()).is_pending());
        drop(caller);
        drop(held);
        sekvent_testing::await_until!(pool.num_idle() == 3);

        let held = hold_all(&pool).await;
        assert_eq!(
            registry.lookup(&selector).await.unwrap().as_ref(),
            Some(&billing)
        );
        let by_id = registry.lookup(&Selector::Id(billing.info.id)).await;
        assert_eq!(by_id.unwrap().as_ref(), Some(&billing));
        drop(held);
    })
    .await;
}

#[tokio::test]
#[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
async fn an_internal_read_answers_while_the_lookup_permit_is_held() {
    let Some(cluster) = Cluster::start().await else {
        return;
    };
    common::guard(async {
        let (registry, pool, writer, _clock) = small_registry(&cluster).await;
        let active = TenantState::Active;
        let billing = cluster
            .insert_row(&writer, "billing", &["billing.example.com"], active)
            .await;
        let orders = cluster
            .insert_row(&writer, "orders", &["orders.example.com"], active)
            .await;

        let held = hold_all(&pool).await;
        let first_selector = Selector::Id(billing.info.id);
        let mut first = pin!(registry.lookup(&first_selector));
        assert!(futures::poll!(first.as_mut()).is_pending());
        let refused = registry.lookup(&Selector::Id(orders.info.id)).await;
        assert_busy(&refused.unwrap_err());

        let mut internal = pin!(registry.tenant(orders.info.id));
        assert!(
            futures::poll!(internal.as_mut()).is_pending(),
            "it waits for a connection, not for a permit"
        );
        drop(held);
        assert_eq!(internal.await.unwrap().as_ref(), Some(&orders));
        assert_eq!(first.await.unwrap().as_ref(), Some(&billing));

        let held = hold_all(&pool).await;
        let cached = registry.lookup(&Selector::Id(orders.info.id)).await;
        assert_eq!(
            cached.unwrap().as_ref(),
            Some(&orders),
            "the internal read was cached"
        );
        let cached = registry.tenant(billing.info.id).await;
        assert_eq!(
            cached.unwrap().as_ref(),
            Some(&billing),
            "internal reads use the cache"
        );
        drop(held);
    })
    .await;
}

#[tokio::test]
#[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
#[allow(clippy::too_many_lines)]
async fn the_loopback_path_label_selects_the_one_live_tenant_with_a_loopback_host() {
    let Some(cluster) = Cluster::start().await else {
        return;
    };
    common::guard(async {
        let (registry, pool, writer, _clock) = small_registry(&cluster).await;
        let active = TenantState::Active;
        let billing = cluster
            .insert_row(
                &writer,
                "billing",
                &["billing.example.com", "billing.localhost:5173"],
                active,
            )
            .await;

        for listener in ["localhost:8080", "localhost:3000", "127.0.0.1:9000"] {
            let found = registry
                .lookup(&by_label("Billing", listener))
                .await
                .unwrap();
            assert_eq!(found.as_ref(), Some(&billing), "{listener}");
        }

        // Never cached: a changed row is seen on the next call without `forget`.
        let selector = by_label("billing", "localhost:8080");
        cluster
            .set_state(&writer, billing.info.id, TenantState::Suspended)
            .await;
        assert_eq!(state_of(&registry, &selector).await, TenantState::Suspended);
        cluster.set_state(&writer, billing.info.id, active).await;
        assert_eq!(state_of(&registry, &selector).await, TenantState::Active);

        // Two live tenants with the label select nothing.
        cluster
            .insert_row(
                &writer,
                "orders",
                &["orders.example.com", "orders.localhost:5173"],
                active,
            )
            .await;
        cluster
            .insert_row(&writer, "orders", &["orders-two.example.com"], active)
            .await;
        let orders = registry.lookup(&by_label("orders", "localhost:8080")).await;
        assert_eq!(orders.unwrap(), None);

        // A purged namesake does not count.
        let inventory = cluster
            .insert_row(
                &writer,
                "inventory",
                &["inventory.example.com", "inventory.localhost:5173"],
                active,
            )
            .await;
        cluster
            .insert_row(
                &writer,
                "inventory",
                &["inventory-old.example.com"],
                TenantState::Purged,
            )
            .await;
        let found = registry
            .lookup(&by_label("inventory", "localhost:8080"))
            .await;
        assert_eq!(found.unwrap().as_ref(), Some(&inventory));

        // The one tenant of the label needs a loopback host.
        cluster
            .insert_row(&writer, "remote", &["remote.example.com"], active)
            .await;
        let remote = registry.lookup(&by_label("remote", "localhost:8080")).await;
        assert_eq!(remote.unwrap(), None);

        // Each label read spends a lookup permit.
        let held = hold_all(&pool).await;
        let first_selector = Selector::Id(inventory.info.id);
        let mut first = pin!(registry.lookup(&first_selector));
        assert!(futures::poll!(first.as_mut()).is_pending());
        let refused = registry.clone().lookup(&selector).await;
        assert_busy(&refused.unwrap_err());
        drop(held);
        assert_eq!(first.await.unwrap().as_ref(), Some(&inventory));
    })
    .await;
}

#[tokio::test]
#[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
async fn a_host_lookup_also_answers_the_id_lookup_that_follows() {
    let Some(cluster) = Cluster::start().await else {
        return;
    };
    common::guard(async {
        let (registry, control) = cluster.registry(Arc::new(common::manual_clock())).await;
        let billing = cluster
            .insert_row(
                &control,
                "billing",
                &["billing.example.com"],
                TenantState::Active,
            )
            .await;
        let found = registry.lookup(&by_host("billing.example.com")).await;
        assert_eq!(found.unwrap().as_ref(), Some(&billing));

        // With every connection held, only the cache can answer.
        let held = hold_all(registry.pool()).await;
        let by_id = registry.lookup(&Selector::Id(billing.info.id)).await;
        assert_eq!(by_id.unwrap().as_ref(), Some(&billing));
        drop(held);
    })
    .await;
}

#[tokio::test]
#[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
async fn a_cached_answer_is_reused_until_forget_or_thirty_seconds() {
    let Some(cluster) = Cluster::start().await else {
        return;
    };
    common::guard(async {
        let clock = common::manual_clock();
        let (registry, control) = cluster.registry(Arc::new(clock.clone())).await;
        let billing = cluster
            .insert_row(
                &control,
                "billing",
                &["billing.example.com"],
                TenantState::Active,
            )
            .await;
        let id = billing.info.id;
        let selector = Selector::Id(id);
        assert_eq!(state_of(&registry, &selector).await, TenantState::Active);

        cluster
            .set_state(&control, id, TenantState::Suspended)
            .await;
        assert_eq!(
            state_of(&registry, &selector).await,
            TenantState::Active,
            "cached"
        );
        registry.forget(id);
        assert_eq!(state_of(&registry, &selector).await, TenantState::Suspended);

        cluster.set_state(&control, id, TenantState::Active).await;
        registry.clear_lookups();
        assert_eq!(state_of(&registry, &selector).await, TenantState::Active);

        cluster
            .set_state(&control, id, TenantState::Suspended)
            .await;
        clock.advance(Duration::from_millis(29_999));
        assert_eq!(
            state_of(&registry, &selector).await,
            TenantState::Active,
            "still cached"
        );
        clock.advance(Duration::from_millis(1));
        assert_eq!(
            state_of(&registry, &selector).await,
            TenantState::Suspended,
            "expired"
        );
    })
    .await;
}
