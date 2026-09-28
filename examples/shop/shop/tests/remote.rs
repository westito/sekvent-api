//! Remote fault injection, `split-grpc` only: what the network hop adds to
//! the shared suite — the circuit breaker, retries of idempotent methods,
//! `Retry-After`, deadlines across the hop, link authentication and trust,
//! and a graceful shutdown of the inventory service.
//!
//! Everything crosses a real loopback socket, so the tests run on the real
//! clock inside a hang guard and assert lower time bounds only.

mod support;

use std::time::Duration;

use inventory_api::{
    InventoryError, InventoryHandle, ReleaseRequest, ReserveRequest, StockRequest,
};
use rstest::rstest;
use sekvent::component::{App, reasons};
use sekvent::context::ServiceIdentity;
use sekvent::prelude::*;
use sekvent::runtime::ServiceStatus;
use sekvent_testing::await_until;
use support::fakes::{
    Entry, FlakyInventory, GatedInventory, Method, PendingInventory, RecordingInventory, entries,
};
use support::{
    INVENTORY_AUTH_KEY, INVENTORY_SERVICE, LinkSetup, OTHER_TOKEN, OUTBOUND_KEY, Profile,
    SHOP_LINK, SKU, STOCK, fake, guarded, missing_keys, real_inventory, service_source, start,
    start_with, try_build,
};
use tokio::time::Instant;

const BREAKER_KEYS: [(&str, &str); 3] = [
    ("SEKVENT_COMPONENT_INVENTORY_BREAKER_WINDOW", "4"),
    ("SEKVENT_COMPONENT_INVENTORY_BREAKER_MIN_CALLS", "4"),
    ("SEKVENT_COMPONENT_INVENTORY_BREAKER_WAIT_IN_OPEN", "1h"),
];

fn reserve(order_id: &str, quantity: u32) -> ReserveRequest {
    ReserveRequest {
        order_id: order_id.to_owned(),
        sku: SKU.to_owned(),
        quantity,
    }
}

fn release() -> ReleaseRequest {
    ReleaseRequest {
        reservation_id: "res-1".to_owned(),
    }
}

fn stock() -> StockRequest {
    StockRequest {
        sku: SKU.to_owned(),
    }
}

/// The `AppError` inside `InventoryError::Other`.
fn other(error: InventoryError) -> AppError {
    match error {
        InventoryError::Other(error) => error,
        typed => panic!("expected InventoryError::Other, got {typed:?}"),
    }
}

fn inventory(app: &App) -> InventoryHandle {
    app.handle::<InventoryHandle>()
        .expect("inventory is installed")
}

fn methods(log: &[Entry]) -> Vec<Method> {
    log.iter().map(|entry| entry.method).collect()
}

#[tokio::test]
async fn the_breaker_opens_after_repeated_unavailability() {
    guarded(async {
        let (flaky, log) = FlakyInventory::new(Method::Release, usize::MAX, || {
            AppError::unavailable("the store room is closed")
        });
        let shop = start(Profile::SplitGrpc, &BREAKER_KEYS, fake(flaky)).await;
        let inventory = inventory(&shop.app);

        for attempt in 1..=4 {
            let error = other(
                inventory
                    .release(&CallContext::new(), release())
                    .await
                    .unwrap_err(),
            );
            assert_eq!(error.code(), ErrorCode::Unavailable, "call {attempt}");
            assert_ne!(
                error.reason(),
                Some(reasons::CIRCUIT_OPEN),
                "call {attempt}"
            );
        }
        assert_eq!(entries(&log).len(), 4, "release is never retried");

        let error = other(
            inventory
                .release(&CallContext::new(), release())
                .await
                .unwrap_err(),
        );
        assert_eq!(error.code(), ErrorCode::Unavailable, "{error:?}");
        assert_eq!(error.reason(), Some(reasons::CIRCUIT_OPEN));
        assert!(error.retry_after().is_some(), "{error:?}");
        assert_eq!(
            error.metadata().get("component").map(String::as_str),
            Some("inventory")
        );
        assert_eq!(entries(&log).len(), 4, "the open breaker sent nothing");
    })
    .await;
}

#[tokio::test]
async fn business_errors_never_open_the_breaker() {
    guarded(async {
        let shop = start(Profile::SplitGrpc, &BREAKER_KEYS, real_inventory).await;
        let inventory = inventory(&shop.app);

        for attempt in 1..=10 {
            let order_id = format!("ord-{attempt}");
            let error = inventory
                .reserve(&CallContext::new(), reserve(&order_id, STOCK + 1))
                .await
                .unwrap_err();
            assert!(
                matches!(&error, InventoryError::OutOfStock { sku, available } if sku == SKU && *available == STOCK),
                "call {attempt}: {error:?}"
            );
        }

        let reply = inventory
            .reserve(&CallContext::new(), reserve("ord-ok", 1))
            .await
            .expect("the breaker stayed closed");
        assert_eq!(reply.remaining, STOCK - 1);
    })
    .await;
}

#[tokio::test]
async fn an_idempotent_method_is_retried_with_the_same_identity() {
    guarded(async {
        let (flaky, log) = FlakyInventory::new(Method::Stock, 1, || {
            AppError::unavailable("the store room is busy")
        });
        let shop = start(Profile::SplitGrpc, &[], fake(flaky)).await;
        let inventory = inventory(&shop.app);

        let cx = CallContext::new().with_idempotency_key("stock-check-1");
        let reply = inventory
            .stock(&cx, stock())
            .await
            .expect("the retry succeeds");
        assert_eq!(reply.sku, SKU);

        let log = entries(&log);
        assert_eq!(methods(&log), [Method::Stock, Method::Stock]);
        for entry in &log {
            assert_eq!(entry.request_id, cx.request_id());
            assert_eq!(entry.idempotency_key.as_deref(), Some("stock-check-1"));
        }
    })
    .await;
}

#[tokio::test]
async fn a_method_that_is_not_idempotent_is_never_retried() {
    guarded(async {
        let (flaky, log) = FlakyInventory::new(Method::Release, 1, || {
            AppError::unavailable("the store room is busy")
        });
        let shop = start(Profile::SplitGrpc, &[], fake(flaky)).await;
        let inventory = inventory(&shop.app);

        let error = other(
            inventory
                .release(&CallContext::new(), release())
                .await
                .unwrap_err(),
        );
        assert_eq!(error.code(), ErrorCode::Unavailable, "{error:?}");
        assert_eq!(methods(&entries(&log)), [Method::Release]);
    })
    .await;
}

#[tokio::test]
async fn a_retry_after_above_the_cap_is_not_waited_for() {
    guarded(async {
        let (flaky, log) = FlakyInventory::new(Method::Stock, 1, || {
            AppError::unavailable("come back later").with_retry_after(Duration::from_secs(60))
        });
        let shop = start(Profile::SplitGrpc, &[], fake(flaky)).await;
        let inventory = inventory(&shop.app);

        let error = other(
            inventory
                .stock(&CallContext::new(), stock())
                .await
                .unwrap_err(),
        );
        assert_eq!(error.code(), ErrorCode::Unavailable, "{error:?}");
        assert_eq!(error.retry_after(), Some(Duration::from_secs(60)));
        assert_eq!(methods(&entries(&log)), [Method::Stock]);
    })
    .await;
}

#[tokio::test]
async fn the_callers_deadline_crosses_the_hop() {
    guarded(async {
        let (fake_inventory, mut pending) = PendingInventory::new();
        let shop = start(Profile::SplitGrpc, &[], fake(fake_inventory)).await;
        let inventory = inventory(&shop.app);

        let budget = Duration::from_millis(300);
        let started = Instant::now();
        let cx = CallContext::new().with_deadline((started + budget).into_std());
        let error = other(
            inventory
                .reserve(&cx, reserve("ord-1", 1))
                .await
                .unwrap_err(),
        );
        let elapsed = started.elapsed();
        assert_eq!(error.code(), ErrorCode::DeadlineExceeded, "{error:?}");
        assert!(elapsed >= budget, "{elapsed:?}");

        let remaining = pending
            .entered
            .recv()
            .await
            .expect("the call reached the service")
            .expect("the service saw a deadline");
        assert!(
            remaining > Duration::ZERO && remaining <= budget,
            "{remaining:?}"
        );
        pending
            .dropped
            .await
            .expect("the service dropped the implementation's future");
    })
    .await;
}

#[tokio::test]
async fn a_wrong_token_is_rejected_before_any_work() {
    guarded(async {
        let (recording, seen) = RecordingInventory::new();
        let link = LinkSetup {
            inbound: OTHER_TOKEN,
            ..LinkSetup::default()
        };
        let shop = start_with(Profile::SplitGrpc, &[], link, fake(recording)).await;
        let inventory = inventory(&shop.app);

        let error = other(
            inventory
                .stock(&CallContext::new(), stock())
                .await
                .unwrap_err(),
        );
        assert_eq!(error.code(), ErrorCode::Unauthenticated, "{error:?}");
        assert_eq!(error.message(), "service authentication required");
        assert!(entries(&seen).is_empty(), "inventory saw no call");
    })
    .await;
}

#[tokio::test]
async fn a_missing_outbound_token_fails_the_build() {
    guarded(async {
        let (recording, _seen) = RecordingInventory::new();
        let link = LinkSetup {
            outbound: None,
            ..LinkSetup::default()
        };
        let Err(error) = try_build(Profile::SplitGrpc, &[], link, fake(recording)).await else {
            panic!("the caller's App built without an outbound token");
        };
        assert_eq!(missing_keys(&error), [OUTBOUND_KEY], "{error}");
    })
    .await;
}

#[tokio::test]
async fn no_token_is_rejected_by_an_authenticated_service() {
    guarded(async {
        let (recording, seen) = RecordingInventory::new();
        let link = LinkSetup {
            outbound: None,
            ..LinkSetup::default()
        };
        let extra = [(INVENTORY_AUTH_KEY, "none")];
        let shop = start_with(Profile::SplitGrpc, &extra, link, fake(recording)).await;
        let inventory = inventory(&shop.app);

        let error = other(
            inventory
                .stock(&CallContext::new(), stock())
                .await
                .unwrap_err(),
        );
        assert_eq!(error.code(), ErrorCode::Unauthenticated, "{error:?}");
        assert!(entries(&seen).is_empty(), "inventory saw no call");
    })
    .await;
}

#[rstest]
#[case::trusted(true)]
#[case::untrusted(false)]
#[tokio::test]
async fn only_a_trusted_link_carries_the_tenant(#[case] trusted: bool) {
    guarded(async {
        let (recording, seen) = RecordingInventory::new();
        let link = LinkSetup {
            trusted,
            ..LinkSetup::default()
        };
        let shop = start_with(Profile::SplitGrpc, &[], link, fake(recording)).await;
        let inventory = inventory(&shop.app);

        let cx = CallContext::new().with_tenant("t-1");
        inventory.stock(&cx, stock()).await.expect("stock succeeds");

        let seen = entries(&seen);
        assert_eq!(seen.len(), 1);
        if trusted {
            assert_eq!(seen[0].caller, Some(ServiceIdentity::trusted(SHOP_LINK)));
            assert_eq!(seen[0].tenant.as_deref(), Some("t-1"));
        } else {
            assert_eq!(seen[0].caller, Some(ServiceIdentity::untrusted(SHOP_LINK)));
            assert_eq!(seen[0].tenant, None);
        }
    })
    .await;
}

#[tokio::test]
async fn a_graceful_shutdown_finishes_in_flight_calls_then_goes_away() {
    guarded(async {
        let (gated, mut gate) = GatedInventory::new();
        let mut shop = start(Profile::SplitGrpc, &[], fake(gated)).await;
        let inventory = inventory(&shop.app);
        let service = shop
            .take_service()
            .expect("the split profile has a service");

        let in_flight = tokio::spawn({
            let inventory = inventory.clone();
            async move {
                inventory
                    .reserve(&CallContext::new(), reserve("ord-1", 1))
                    .await
            }
        });
        gate.entered.recv().await.expect("the call entered");

        service.runtime.shutdown();
        await_until!(
            service.runtime.health().status(INVENTORY_SERVICE) == Some(ServiceStatus::NotServing)
        );

        gate.permits.add_permits(1);
        let reply = in_flight
            .await
            .expect("the call did not panic")
            .expect("the in-flight call finishes");
        assert_eq!(reply.reservation_id, "res-ord-1");
        service
            .runtime
            .wait()
            .await
            .expect("the service stops cleanly");

        let error = other(
            inventory
                .reserve(&CallContext::new(), reserve("ord-2", 1))
                .await
                .unwrap_err(),
        );
        assert_eq!(error.code(), ErrorCode::Unavailable, "{error:?}");
        assert_eq!(error.reason(), Some(reasons::UNREACHABLE));
    })
    .await;
}

#[tokio::test]
async fn an_exposed_component_needs_its_routes_mounted() {
    guarded(async {
        let source = service_source(&[], LinkSetup::default());
        let mut builder = App::builder(&source);
        real_inventory(&mut builder).expect("inventory installs");
        let app = builder.build().expect("the App builds");

        let error = app
            .start()
            .await
            .expect_err("start fails without the routes");
        assert_eq!(error.code(), ErrorCode::FailedPrecondition, "{error:?}");
        assert_eq!(error.reason(), Some(reasons::GRPC_NOT_MOUNTED));
    })
    .await;
}
