//! The caller side of the `grpc` binding on loopback: an unreachable
//! service, retries and their limits, the circuit breaker, and the remote
//! component's gate through start, drain and stop.

mod support;

use std::sync::Arc;
use std::time::Duration;

use sekvent_component::{AppError, CallContext, ComponentState, ErrorCode, reasons};
use sekvent_config::MapSource;
use support::fakes::{Behaviour, FakeInventory, FlakyInventory, Probe, build_with};
use support::grpc::{
    Service, TOKEN, caller, caller_app, caller_keys, guarded, serve, service_keys,
};
use support::inventory::{InventoryError, InventoryHandle, ReleaseRequest, ReserveRequest};
use tokio::sync::{Semaphore, mpsc};

fn reserve(quantity: u32) -> ReserveRequest {
    ReserveRequest {
        order_id: "o1".into(),
        sku: "sku-1".into(),
        quantity,
    }
}

fn release() -> ReleaseRequest {
    ReleaseRequest {
        reservation_id: "res-o1".into(),
    }
}

fn other(error: InventoryError) -> AppError {
    match error {
        InventoryError::Other(error) => error,
        unexpected => panic!("expected Other, got {unexpected:?}"),
    }
}

fn busy() -> AppError {
    AppError::unavailable("busy")
}

async fn flaky_service(inventory: FlakyInventory) -> (Service, Probe) {
    let probe = inventory.probe.clone();
    let service = serve(&service_keys(), move |builder| {
        InventoryHandle::install(builder, move |_| Ok(inventory))
    })
    .await;
    (service, probe)
}

const BREAKER: [(&str, &str); 3] = [
    ("SEKVENT_COMPONENT_INVENTORY_BREAKER_WINDOW", "4"),
    ("SEKVENT_COMPONENT_INVENTORY_BREAKER_MIN_CALLS", "4"),
    ("SEKVENT_COMPONENT_INVENTORY_BREAKER_WAIT_IN_OPEN", "1h"),
];

#[tokio::test]
async fn nothing_listening_is_unreachable() {
    guarded(async {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        drop(listener);
        let (app, inventory) = caller(&endpoint, &[]).await;
        for error in [
            other(
                inventory
                    .reserve(&CallContext::new(), reserve(1))
                    .await
                    .unwrap_err(),
            ),
            other(
                inventory
                    .release(&CallContext::new(), release())
                    .await
                    .unwrap_err(),
            ),
        ] {
            assert_eq!(error.code(), ErrorCode::Unavailable);
            assert_eq!(error.reason(), Some(reasons::UNREACHABLE));
            assert_eq!(error.message(), "component inventory is unreachable");
            assert_eq!(error.metadata()["component"], "inventory");
        }
        app.stop(Duration::ZERO).await.unwrap();
    })
    .await;
}

#[tokio::test]
async fn idempotent_methods_are_retried_with_the_same_identity() {
    guarded(async {
        let (service, probe) = flaky_service(FlakyInventory::new("reserve", 1, busy)).await;
        let (app, inventory) = caller(&service.endpoint(), &[]).await;
        let cx = CallContext::new()
            .with_request_id("req-7")
            .with_idempotency_key("idem-7");
        let reply = inventory.reserve(&cx, reserve(1)).await.unwrap();
        assert_eq!(reply.remaining, 99);
        let seen = probe.calls();
        assert_eq!(seen.len(), 2);
        for entry in &seen {
            assert_eq!(entry.request_id, "req-7");
            assert_eq!(entry.idempotency_key.as_deref(), Some("idem-7"));
        }
        app.stop(Duration::ZERO).await.unwrap();
        service.stop().await;
    })
    .await;
}

#[tokio::test]
async fn other_methods_are_not_retried() {
    guarded(async {
        let (service, probe) = flaky_service(FlakyInventory::new("release", 1, busy)).await;
        let (app, inventory) = caller(&service.endpoint(), &[]).await;
        let error = other(
            inventory
                .release(&CallContext::new(), release())
                .await
                .unwrap_err(),
        );
        assert_eq!(error.code(), ErrorCode::Unavailable);
        assert_eq!(error.message(), "busy");
        assert!(!error.metadata().contains_key("component"));
        assert_eq!(probe.calls().len(), 1);
        app.stop(Duration::ZERO).await.unwrap();
        service.stop().await;
    })
    .await;
}

#[tokio::test]
async fn a_long_retry_after_is_not_waited_for() {
    guarded(async {
        let (service, probe) = flaky_service(FlakyInventory::new("reserve", 1, || {
            busy().with_retry_after(Duration::from_secs(60))
        }))
        .await;
        let (app, inventory) = caller(&service.endpoint(), &[]).await;
        let error = other(
            inventory
                .reserve(&CallContext::new(), reserve(1))
                .await
                .unwrap_err(),
        );
        assert_eq!(error.code(), ErrorCode::Unavailable);
        assert_eq!(error.retry_after(), Some(Duration::from_secs(60)));
        assert_eq!(probe.calls().len(), 1);
        app.stop(Duration::ZERO).await.unwrap();
        service.stop().await;
    })
    .await;
}

#[tokio::test]
async fn the_retry_budget_limits_retries() {
    guarded(async {
        let (service, probe) = flaky_service(FlakyInventory::new("reserve", 100, busy)).await;
        let (app, inventory) = caller(
            &service.endpoint(),
            &[
                ("SEKVENT_COMPONENT_INVENTORY_RETRY_BUDGET_RATIO", "0"),
                ("SEKVENT_COMPONENT_INVENTORY_RETRY_BUDGET_MIN_PER_SEC", "1"),
                ("SEKVENT_COMPONENT_INVENTORY_BREAKER_ENABLED", "false"),
            ],
        )
        .await;
        for _ in 0..2 {
            let error = other(
                inventory
                    .reserve(&CallContext::new(), reserve(1))
                    .await
                    .unwrap_err(),
            );
            assert_eq!(error.code(), ErrorCode::Unavailable);
        }
        // Without the budget, two calls of three attempts make six entries.
        let entries = probe.calls().len();
        assert!((2..6).contains(&entries), "{entries}");
        app.stop(Duration::ZERO).await.unwrap();
        service.stop().await;
    })
    .await;
}

#[tokio::test]
async fn the_breaker_opens_and_fails_fast() {
    guarded(async {
        let (service, probe) = flaky_service(FlakyInventory::new("release", 100, busy)).await;
        let (app, inventory) = caller(&service.endpoint(), &BREAKER).await;
        for _ in 0..4 {
            let error = other(
                inventory
                    .release(&CallContext::new(), release())
                    .await
                    .unwrap_err(),
            );
            assert_eq!(error.code(), ErrorCode::Unavailable);
            assert_eq!(error.message(), "busy");
        }
        let error = other(
            inventory
                .release(&CallContext::new(), release())
                .await
                .unwrap_err(),
        );
        assert_eq!(error.code(), ErrorCode::Unavailable);
        assert_eq!(error.reason(), Some(reasons::CIRCUIT_OPEN));
        assert!(error.retry_after().is_some());
        assert_eq!(error.metadata()["component"], "inventory");
        assert_eq!(error.metadata()["method"], "release");
        assert_eq!(probe.calls().len(), 4);
        app.stop(Duration::ZERO).await.unwrap();
        service.stop().await;
    })
    .await;
}

#[tokio::test]
async fn business_errors_never_open_the_breaker() {
    guarded(async {
        let inventory = FakeInventory::stock(1);
        let service = serve(&service_keys(), move |builder| {
            InventoryHandle::install(builder, move |_| Ok(inventory))
        })
        .await;
        let (app, inventory) = caller(&service.endpoint(), &BREAKER).await;
        for _ in 0..10 {
            let error = inventory
                .reserve(&CallContext::new(), reserve(5))
                .await
                .unwrap_err();
            assert!(
                matches!(error, InventoryError::OutOfStock { .. }),
                "{error:?}"
            );
        }
        inventory
            .reserve(&CallContext::new(), reserve(1))
            .await
            .unwrap();
        app.stop(Duration::ZERO).await.unwrap();
        service.stop().await;
    })
    .await;
}

#[tokio::test]
async fn a_remote_component_has_a_gate() {
    // Nothing listens on port 1; the gate answers before any connection.
    let app = caller_app(&caller_keys("http://127.0.0.1:1", &[])).unwrap();
    let inventory = app.handle::<InventoryHandle>().unwrap();
    assert_eq!(app.state("inventory"), Some(ComponentState::NotStarted));
    let error = other(
        inventory
            .reserve(&CallContext::new(), reserve(1))
            .await
            .unwrap_err(),
    );
    assert_eq!(error.code(), ErrorCode::Unavailable);
    assert_eq!(error.reason(), Some(reasons::NOT_STARTED));
    assert_eq!(error.metadata()["component"], "inventory");
    app.stop(Duration::ZERO).await.unwrap();
    let error = other(
        inventory
            .reserve(&CallContext::new(), reserve(1))
            .await
            .unwrap_err(),
    );
    assert_eq!(error.reason(), Some(reasons::STOPPED));
}

#[tokio::test]
async fn stopping_drains_outbound_calls() {
    guarded(async {
        let (entered_tx, mut entered) = mpsc::unbounded_channel();
        let permits = Arc::new(Semaphore::new(0));
        let inventory = FakeInventory::new(Behaviour::Gated {
            entered: entered_tx,
            permits: Arc::clone(&permits),
        });
        let service = serve(&service_keys(), move |builder| {
            InventoryHandle::install(builder, move |_| Ok(inventory))
        })
        .await;
        let (app, inventory) = caller(&service.endpoint(), &[]).await;
        let in_flight = {
            let inventory = inventory.clone();
            tokio::spawn(async move { inventory.reserve(&CallContext::new(), reserve(1)).await })
        };
        entered.recv().await.unwrap();
        let stopping = {
            let app = app.clone();
            tokio::spawn(async move { app.stop(Duration::from_secs(30)).await })
        };
        while app.state("inventory") != Some(ComponentState::Draining) {
            tokio::task::yield_now().await;
        }
        let error = other(
            inventory
                .reserve(&CallContext::new(), reserve(1))
                .await
                .unwrap_err(),
        );
        assert_eq!(error.reason(), Some(reasons::DRAINING));
        permits.add_permits(1);
        in_flight.await.unwrap().unwrap();
        stopping.await.unwrap().unwrap();
        assert_eq!(app.state("inventory"), Some(ComponentState::Stopped));
        service.stop().await;
    })
    .await;
}

#[tokio::test]
async fn an_exposed_component_needs_its_routes_mounted() {
    let config: MapSource = [
        ("SEKVENT_COMPONENT_INVENTORY_SERVE", "grpc"),
        ("SEKVENT_LINK_INBOUND_SHOP", TOKEN),
    ]
    .into_iter()
    .collect();
    let build = || {
        build_with(&config, |builder| {
            InventoryHandle::install(builder, |_| Ok(FakeInventory::stock(1)))
        })
        .unwrap()
    };
    let app = build();
    assert_eq!(app.grpc_services(), ["shop.inventory.v1.Inventory"]);
    let error = app.start().await.unwrap_err();
    assert_eq!(error.code(), ErrorCode::FailedPrecondition);
    assert_eq!(error.reason(), Some(reasons::GRPC_NOT_MOUNTED));
    assert_eq!(error.metadata()["component"], "inventory");
    assert!(
        error
            .message()
            .contains("SEKVENT_COMPONENT_INVENTORY_SERVE"),
        "{error}"
    );
    assert_eq!(app.state("inventory"), Some(ComponentState::NotStarted));

    let app = build();
    drop(app.grpc_routes());
    app.start().await.unwrap();
    assert_eq!(app.state("inventory"), Some(ComponentState::Serving));
    app.stop(Duration::ZERO).await.unwrap();
}
