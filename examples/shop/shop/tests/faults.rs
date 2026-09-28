//! Fault injection for milestone C1, each test run under the
//! `monolith-local` and `monolith-serialized` profiles.
//!
//! Timed tests run on tokio's paused clock: it only moves when every task is
//! idle, so a deadline fires exactly when nothing else can happen, and a
//! test that must not wait asserts that no virtual time passed.

mod support;

use std::time::Duration;

use inventory_api::{InventoryError, InventoryHandle, ReserveRequest};
use rstest::rstest;
use sekvent::component::{ComponentState, reasons};
use sekvent::prelude::*;
use support::fakes::{FUTURE_REASON, FutureReasonInventory, GatedInventory, PendingInventory};
use support::{
    Profile, RESERVE_BULKHEAD_KEY, RESERVE_TIMEOUT_KEY, SKU, STOCK, shop_with_inventory,
    started_shop,
};
use tokio::time::Instant;

fn reserve(order_id: &str) -> ReserveRequest {
    ReserveRequest {
        order_id: order_id.to_owned(),
        sku: SKU.to_owned(),
        quantity: 1,
    }
}

/// The `AppError` inside `InventoryError::Other`.
fn other(error: InventoryError) -> AppError {
    match error {
        InventoryError::Other(error) => error,
        typed => panic!("expected InventoryError::Other, got {typed:?}"),
    }
}

/// A context whose deadline is `after` from now, on the tokio clock.
fn deadline_in(after: Duration) -> CallContext {
    CallContext::new().with_deadline((Instant::now() + after).into_std())
}

/// Yield until `condition` holds, letting spawned tasks run in between.
/// Panics instead of spinning forever.
async fn yield_until(what: &str, mut condition: impl FnMut() -> bool) {
    for _ in 0..10_000 {
        if condition() {
            return;
        }
        tokio::task::yield_now().await;
    }
    panic!("{what} never happened");
}

#[rstest]
#[case::monolith_local(Profile::MonolithLocal)]
#[case::monolith_serialized(Profile::MonolithSerialized)]
#[tokio::test(start_paused = true)]
async fn the_method_timeout_bounds_a_call(#[case] profile: Profile) {
    let (fake, _pending) = PendingInventory::new();
    let app = shop_with_inventory(&profile.source(), profile, fake).await;
    let inventory = app.handle::<InventoryHandle>().unwrap();

    let started = Instant::now();
    let result = inventory
        .reserve(&CallContext::new(), reserve("ord-1"))
        .await;
    let elapsed = started.elapsed();

    let error = other(result.unwrap_err());
    assert_eq!(error.code(), ErrorCode::DeadlineExceeded, "{error:?}");
    assert!(
        elapsed >= Duration::from_secs(2) && elapsed < Duration::from_secs(3),
        "{elapsed:?}"
    );
}

#[rstest]
#[case::monolith_local(Profile::MonolithLocal)]
#[case::monolith_serialized(Profile::MonolithSerialized)]
#[tokio::test(start_paused = true)]
async fn a_shorter_caller_deadline_wins(#[case] profile: Profile) {
    let (fake, _pending) = PendingInventory::new();
    let app = shop_with_inventory(&profile.source(), profile, fake).await;
    let inventory = app.handle::<InventoryHandle>().unwrap();

    let started = Instant::now();
    let cx = deadline_in(Duration::from_millis(100));
    let result = inventory.reserve(&cx, reserve("ord-1")).await;
    let elapsed = started.elapsed();

    let error = other(result.unwrap_err());
    assert_eq!(error.code(), ErrorCode::DeadlineExceeded, "{error:?}");
    assert!(
        elapsed >= Duration::from_millis(100) && elapsed < Duration::from_secs(2),
        "{elapsed:?}"
    );
}

#[rstest]
#[case::monolith_local(Profile::MonolithLocal)]
#[case::monolith_serialized(Profile::MonolithSerialized)]
#[tokio::test(start_paused = true)]
async fn configuration_overrides_the_method_timeout(#[case] profile: Profile) {
    let source = profile.source().with(RESERVE_TIMEOUT_KEY, "50ms");
    let (fake, _pending) = PendingInventory::new();
    let app = shop_with_inventory(&source, profile, fake).await;
    let inventory = app.handle::<InventoryHandle>().unwrap();

    let started = Instant::now();
    let result = inventory
        .reserve(&CallContext::new(), reserve("ord-1"))
        .await;
    let elapsed = started.elapsed();

    let error = other(result.unwrap_err());
    assert_eq!(error.code(), ErrorCode::DeadlineExceeded, "{error:?}");
    assert!(
        elapsed >= Duration::from_millis(50) && elapsed < Duration::from_millis(100),
        "{elapsed:?}"
    );
}

#[rstest]
#[case::monolith_local(Profile::MonolithLocal)]
#[case::monolith_serialized(Profile::MonolithSerialized)]
#[tokio::test(start_paused = true)]
async fn a_full_bulkhead_sheds_at_once(#[case] profile: Profile) {
    let source = profile.source().with(RESERVE_BULKHEAD_KEY, "2");
    let (fake, mut gate) = GatedInventory::new();
    let app = shop_with_inventory(&source, profile, fake).await;
    let inventory = app.handle::<InventoryHandle>().unwrap();

    let calls: Vec<_> = ["ord-1", "ord-2"]
        .into_iter()
        .map(|order_id| {
            let inventory = inventory.clone();
            tokio::spawn(async move {
                inventory
                    .reserve(&CallContext::new(), reserve(order_id))
                    .await
            })
        })
        .collect();
    for _ in 0..2 {
        gate.entered.recv().await.expect("a call entered");
    }

    let started = Instant::now();
    let result = inventory
        .reserve(&CallContext::new(), reserve("ord-3"))
        .await;
    assert_eq!(
        started.elapsed(),
        Duration::ZERO,
        "the shed call did not wait"
    );
    let error = other(result.unwrap_err());
    assert_eq!(error.code(), ErrorCode::ResourceExhausted, "{error:?}");
    assert_eq!(error.reason(), Some(reasons::BULKHEAD_FULL));
    assert!(
        gate.entered.try_recv().is_err(),
        "the shed call never entered"
    );

    gate.permits.add_permits(2);
    for call in calls {
        let reply = call
            .await
            .expect("the call did not panic")
            .expect("an admitted call succeeds");
        assert!(reply.reservation_id.starts_with("res-ord-"));
    }
}

#[rstest]
#[case::monolith_local(Profile::MonolithLocal)]
#[case::monolith_serialized(Profile::MonolithSerialized)]
#[tokio::test]
async fn dead_contexts_are_rejected_before_any_work(#[case] profile: Profile) {
    let (fake, mut gate) = GatedInventory::new();
    let app = shop_with_inventory(&profile.source(), profile, fake).await;
    let inventory = app.handle::<InventoryHandle>().unwrap();

    let cancelled = CallContext::new();
    cancelled.cancel_token().cancel();
    let error = other(
        inventory
            .reserve(&cancelled, reserve("ord-1"))
            .await
            .unwrap_err(),
    );
    assert_eq!(error.code(), ErrorCode::Cancelled, "{error:?}");

    let expired = deadline_in(Duration::ZERO);
    let error = other(
        inventory
            .reserve(&expired, reserve("ord-2"))
            .await
            .unwrap_err(),
    );
    assert_eq!(error.code(), ErrorCode::DeadlineExceeded, "{error:?}");

    assert!(gate.entered.try_recv().is_err(), "the fake saw no call");
}

#[rstest]
#[case::monolith_local(Profile::MonolithLocal)]
#[case::monolith_serialized(Profile::MonolithSerialized)]
#[tokio::test]
async fn a_typed_error_round_trips(#[case] profile: Profile) {
    let app = started_shop(profile).await;
    let inventory = app.handle::<InventoryHandle>().unwrap();

    let request = ReserveRequest {
        order_id: "ord-1".to_owned(),
        sku: SKU.to_owned(),
        quantity: STOCK + 1,
    };
    let error = inventory
        .reserve(&CallContext::new(), request)
        .await
        .unwrap_err();
    assert!(
        matches!(&error, InventoryError::OutOfStock { sku, available } if sku == SKU && *available == STOCK),
        "{error:?}"
    );

    let error = AppError::from(error);
    assert_eq!(error.code(), ErrorCode::FailedPrecondition);
    assert_eq!(error.reason(), Some("OUT_OF_STOCK"));
    assert_eq!(error.domain(), Some("shop.inventory.v1"));
    assert_eq!(error.message(), format!("only {STOCK} of {SKU} left"));
    assert_eq!(error.metadata().get("sku").map(String::as_str), Some(SKU));
    assert_eq!(
        error.metadata().get("available").map(String::as_str),
        Some(STOCK.to_string().as_str())
    );
}

#[rstest]
#[case::monolith_local(Profile::MonolithLocal)]
#[case::monolith_serialized(Profile::MonolithSerialized)]
#[tokio::test]
async fn an_unknown_reason_decodes_as_other(#[case] profile: Profile) {
    let app = shop_with_inventory(&profile.source(), profile, FutureReasonInventory).await;
    let inventory = app.handle::<InventoryHandle>().unwrap();

    let result = inventory
        .reserve(&CallContext::new(), reserve("ord-1"))
        .await;
    let error = other(result.unwrap_err());
    assert_eq!(error.code(), ErrorCode::FailedPrecondition, "{error:?}");
    assert_eq!(error.reason(), Some(FUTURE_REASON));
}

#[rstest]
#[case::monolith_local(Profile::MonolithLocal)]
#[case::monolith_serialized(Profile::MonolithSerialized)]
#[tokio::test(start_paused = true)]
async fn dropping_the_caller_drops_the_call(#[case] profile: Profile) {
    let (fake, mut pending) = PendingInventory::new();
    let app = shop_with_inventory(&profile.source(), profile, fake).await;
    let inventory = app.handle::<InventoryHandle>().unwrap();

    let started = Instant::now();
    let caller = tokio::spawn(async move {
        inventory
            .reserve(&CallContext::new(), reserve("ord-1"))
            .await
    });
    pending.entered.recv().await.expect("the call entered");
    caller.abort();
    pending
        .dropped
        .await
        .expect("the implementation's future was dropped");
    // Had the call ended by its 2 s timeout instead, the paused clock would
    // have moved.
    assert_eq!(started.elapsed(), Duration::ZERO);
    assert!(caller.await.unwrap_err().is_cancelled());
}

#[rstest]
#[case::monolith_local(Profile::MonolithLocal)]
#[case::monolith_serialized(Profile::MonolithSerialized)]
#[tokio::test(start_paused = true)]
async fn draining_rejects_new_calls_and_lets_in_flight_ones_finish(#[case] profile: Profile) {
    let (fake, mut gate) = GatedInventory::new();
    let app = shop_with_inventory(&profile.source(), profile, fake).await;
    let inventory = app.handle::<InventoryHandle>().unwrap();

    let in_flight = tokio::spawn({
        let inventory = inventory.clone();
        async move {
            inventory
                .reserve(&CallContext::new(), reserve("ord-1"))
                .await
        }
    });
    gate.entered.recv().await.expect("the call entered");

    let stopping = tokio::spawn({
        let app = app.clone();
        async move { app.stop(Duration::from_secs(30)).await }
    });
    yield_until("inventory draining", || {
        app.state("inventory") == Some(ComponentState::Draining)
    })
    .await;

    let result = inventory
        .reserve(&CallContext::new(), reserve("ord-2"))
        .await;
    let error = other(result.unwrap_err());
    assert_eq!(error.code(), ErrorCode::Unavailable, "{error:?}");
    assert_eq!(error.reason(), Some(reasons::DRAINING));

    gate.permits.add_permits(1);
    in_flight
        .await
        .expect("the call did not panic")
        .expect("the in-flight call finishes");
    stopping
        .await
        .expect("stop did not panic")
        .expect("stop succeeds");
    assert_eq!(app.state("inventory"), Some(ComponentState::Stopped));

    let result = inventory
        .reserve(&CallContext::new(), reserve("ord-3"))
        .await;
    let error = other(result.unwrap_err());
    assert_eq!(error.code(), ErrorCode::Unavailable, "{error:?}");
    assert_eq!(error.reason(), Some(reasons::STOPPED));
}
