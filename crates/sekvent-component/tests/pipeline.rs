//! The call pipeline under both in-process bindings, driven through the
//! hand-expanded reference component.

mod support;

use std::sync::Arc;
use std::time::Duration;

use sekvent_component::__private::{Bytes, Dispatch};
use sekvent_component::{AppError, Binding, BuildError, CallContext, ErrorCode, reasons};
use sekvent_context::ServiceIdentity;
use support::fakes::{
    BareHandle, Behaviour, Counter, FakeInventory, GarbageDispatch, LOCAL_BINDINGS, NotesHandle,
    OddHandle, Probe, build_with, deadline_in, source, started_inventory,
};
use support::inventory::{
    InventoryError, InventoryHandle, ReleaseRequest, ReserveReply, ReserveRequest, dispatcher,
    install_with_dispatch,
};
use tokio::sync::{Semaphore, mpsc};
use tokio::time::Instant;

fn reserve(order: &str, quantity: u32) -> ReserveRequest {
    ReserveRequest {
        order_id: order.to_owned(),
        sku: "sku-1".to_owned(),
        quantity,
    }
}

fn release(id: &str) -> ReleaseRequest {
    ReleaseRequest {
        reservation_id: id.to_owned(),
    }
}

fn other(error: InventoryError) -> AppError {
    match error {
        InventoryError::Other(error) => error,
        unexpected => panic!("expected Other, got {unexpected:?}"),
    }
}

fn assert_tagged(error: &AppError, method: &str) {
    assert_eq!(error.metadata()["component"], "inventory", "{error:?}");
    assert_eq!(error.metadata()["method"], method, "{error:?}");
}

fn pending() -> (
    FakeInventory,
    mpsc::UnboundedReceiver<()>,
    mpsc::UnboundedReceiver<()>,
) {
    let (entered, entered_rx) = mpsc::unbounded_channel();
    let (dropped, dropped_rx) = mpsc::unbounded_channel();
    let fake = FakeInventory::new(Behaviour::Pending { entered, dropped });
    (fake, entered_rx, dropped_rx)
}

#[tokio::test]
async fn a_call_succeeds() {
    for binding in LOCAL_BINDINGS {
        let (_app, inventory) = started_inventory(binding, &[], FakeInventory::stock(10)).await;
        let cx = CallContext::new();
        let reply = inventory.reserve(&cx, reserve("o1", 3)).await.unwrap();
        assert_eq!(
            reply,
            ReserveReply {
                reservation_id: "res-o1".into(),
                remaining: 7
            }
        );
        let released = inventory.release(&cx, release("res-o1")).await.unwrap();
        assert!(released.released, "{binding}");
        assert!(
            format!("{inventory:?}").contains(&format!("binding: {binding:?}")),
            "{inventory:?}"
        );
    }
}

#[tokio::test]
async fn typed_errors_are_identical_under_both_bindings() {
    let mut seen = Vec::new();
    for binding in LOCAL_BINDINGS {
        let (_app, inventory) = started_inventory(binding, &[], FakeInventory::stock(10)).await;
        let cx = CallContext::new();
        let out_of_stock = inventory.reserve(&cx, reserve("o1", 20)).await.unwrap_err();
        assert!(
            matches!(
                &out_of_stock,
                InventoryError::OutOfStock { sku, available: 10 } if sku == "sku-1"
            ),
            "{out_of_stock:?}"
        );
        let hinted = inventory.release(&cx, release("bogus")).await.unwrap_err();
        assert!(
            matches!(
                &hinted,
                InventoryError::ReservationNotFound { reservation_id, hint: Some(hint) }
                    if reservation_id == "bogus" && hint == "ids start with res-"
            ),
            "{hinted:?}"
        );
        let bare = inventory.release(&cx, release("")).await.unwrap_err();
        assert!(
            matches!(
                &bare,
                InventoryError::ReservationNotFound { hint: None, .. }
            ),
            "{bare:?}"
        );
        seen.push(format!("{out_of_stock:?} {hinted:?} {bare:?}"));
    }
    assert_eq!(seen[0], seen[1]);
}

/// Builds an error, and the reason it carries on the wire.
type ErrorCase = (fn() -> InventoryError, &'static str);

#[tokio::test]
async fn unknown_reasons_and_unparseable_fields_decode_as_other() {
    let cases: [ErrorCase; 4] = [
        (
            || {
                InventoryError::Other(
                    AppError::failed_precondition("from a newer server")
                        .with_reason("FROM_THE_FUTURE"),
                )
            },
            "FROM_THE_FUTURE",
        ),
        (
            || {
                InventoryError::Other(
                    AppError::failed_precondition("bad field")
                        .with_reason("OUT_OF_STOCK")
                        .with_domain("shop.inventory.v1")
                        .with_metadata("sku", "sku-1")
                        .with_metadata("available", "many"),
                )
            },
            "OUT_OF_STOCK",
        ),
        (
            || {
                InventoryError::Other(
                    AppError::failed_precondition("other domain")
                        .with_reason("OUT_OF_STOCK")
                        .with_domain("shop.billing.v1")
                        .with_metadata("sku", "sku-1")
                        .with_metadata("available", "1"),
                )
            },
            "OUT_OF_STOCK",
        ),
        (
            || {
                InventoryError::Other(
                    AppError::failed_precondition("no domain").with_reason("INVENTORY_CLOSED"),
                )
            },
            "INVENTORY_CLOSED",
        ),
    ];
    for binding in LOCAL_BINDINGS {
        for (make, reason) in cases {
            let (_app, inventory) =
                started_inventory(binding, &[], FakeInventory::new(Behaviour::Fail(make))).await;
            let error = other(
                inventory
                    .reserve(&CallContext::new(), reserve("o1", 1))
                    .await
                    .unwrap_err(),
            );
            assert_eq!(error.code(), ErrorCode::FailedPrecondition, "{binding}");
            assert_eq!(error.reason(), Some(reason), "{binding}");
            assert!(
                !error.metadata().contains_key("component"),
                "implementation errors are untouched: {error:?}"
            );
        }

        let (_app, inventory) = started_inventory(
            binding,
            &[],
            FakeInventory::new(Behaviour::Fail(|| InventoryError::Closed)),
        )
        .await;
        let closed = inventory
            .reserve(&CallContext::new(), reserve("o1", 1))
            .await
            .unwrap_err();
        assert!(matches!(closed, InventoryError::Closed), "{closed:?}");
    }
}

#[tokio::test(start_paused = true)]
async fn the_method_timeout_bounds_a_call() {
    for binding in LOCAL_BINDINGS {
        let (fake, _entered, mut dropped) = pending();
        let (_app, inventory) = started_inventory(binding, &[], fake).await;
        let started = Instant::now();
        let error = other(
            inventory
                .reserve(&CallContext::new(), reserve("o1", 1))
                .await
                .unwrap_err(),
        );
        let elapsed = started.elapsed();
        assert_eq!(error.code(), ErrorCode::DeadlineExceeded, "{binding}");
        assert_tagged(&error, "reserve");
        assert!(elapsed >= Duration::from_secs(2), "{binding}: {elapsed:?}");
        assert!(elapsed < Duration::from_secs(3), "{binding}: {elapsed:?}");
        dropped.recv().await.unwrap();
    }
}

#[tokio::test(start_paused = true)]
async fn the_caller_deadline_wins_over_a_longer_timeout() {
    for binding in LOCAL_BINDINGS {
        let (fake, _entered, _dropped) = pending();
        let (_app, inventory) = started_inventory(binding, &[], fake).await;
        let started = Instant::now();
        let cx = CallContext::new().with_deadline(deadline_in(Duration::from_millis(100)));
        let error = other(inventory.reserve(&cx, reserve("o1", 1)).await.unwrap_err());
        let elapsed = started.elapsed();
        assert_eq!(error.code(), ErrorCode::DeadlineExceeded, "{binding}");
        assert!(
            elapsed >= Duration::from_millis(100),
            "{binding}: {elapsed:?}"
        );
        assert!(elapsed < Duration::from_secs(2), "{binding}: {elapsed:?}");
    }
}

#[tokio::test(start_paused = true)]
async fn a_configured_timeout_overrides_the_attribute() {
    for binding in LOCAL_BINDINGS {
        let (fake, _entered, _dropped) = pending();
        let (_app, inventory) = started_inventory(
            binding,
            &[("SEKVENT_COMPONENT_INVENTORY_RESERVE_TIMEOUT", "50ms")],
            fake,
        )
        .await;
        let started = Instant::now();
        let error = other(
            inventory
                .reserve(&CallContext::new(), reserve("o1", 1))
                .await
                .unwrap_err(),
        );
        let elapsed = started.elapsed();
        assert_eq!(error.code(), ErrorCode::DeadlineExceeded, "{binding}");
        assert!(
            elapsed >= Duration::from_millis(50),
            "{binding}: {elapsed:?}"
        );
        assert!(
            elapsed < Duration::from_millis(100),
            "{binding}: {elapsed:?}"
        );
    }
}

#[tokio::test]
async fn a_full_bulkhead_sheds_and_frees_its_permits() {
    for binding in LOCAL_BINDINGS {
        let (entered, mut entered_rx) = mpsc::unbounded_channel();
        let permits = Arc::new(Semaphore::new(0));
        let fake = FakeInventory::new(Behaviour::Gated {
            entered,
            permits: Arc::clone(&permits),
        });
        let (_app, inventory) = started_inventory(
            binding,
            &[(
                "SEKVENT_COMPONENT_INVENTORY_RESERVE_BULKHEAD_MAX_CONCURRENT",
                "2",
            )],
            fake,
        )
        .await;
        let calls: Vec<_> = (0..2)
            .map(|n| {
                let inventory = inventory.clone();
                tokio::spawn(async move {
                    inventory
                        .reserve(&CallContext::new(), reserve(&format!("o{n}"), 1))
                        .await
                })
            })
            .collect();
        entered_rx.recv().await.unwrap();
        entered_rx.recv().await.unwrap();

        let shed = other(
            inventory
                .reserve(&CallContext::new(), reserve("o3", 1))
                .await
                .unwrap_err(),
        );
        assert_eq!(shed.code(), ErrorCode::ResourceExhausted, "{binding}");
        assert_eq!(shed.reason(), Some(reasons::BULKHEAD_FULL));
        assert_tagged(&shed, "reserve");

        // Another method has its own (absent) bulkhead.
        inventory
            .release(&CallContext::new(), release("res-x"))
            .await
            .unwrap();

        permits.add_permits(2);
        for call in calls {
            call.await.unwrap().unwrap();
        }
        permits.add_permits(1);
        inventory
            .reserve(&CallContext::new(), reserve("o4", 1))
            .await
            .unwrap();
    }
}

#[tokio::test(start_paused = true)]
async fn dead_calls_are_rejected_before_any_work() {
    for binding in LOCAL_BINDINGS {
        let fake = FakeInventory::stock(10);
        let probe: Probe = fake.probe.clone();
        let (_app, inventory) = started_inventory(binding, &[], fake).await;

        let cancelled = CallContext::new();
        cancelled.cancel_token().cancel();
        let error = other(
            inventory
                .reserve(&cancelled, reserve("o1", 1))
                .await
                .unwrap_err(),
        );
        assert_eq!(error.code(), ErrorCode::Cancelled, "{binding}");
        assert_tagged(&error, "reserve");

        let expired = CallContext::new().with_deadline(deadline_in(Duration::from_millis(5)));
        tokio::time::advance(Duration::from_millis(10)).await;
        let error = other(
            inventory
                .reserve(&expired, reserve("o1", 1))
                .await
                .unwrap_err(),
        );
        assert_eq!(error.code(), ErrorCode::DeadlineExceeded, "{binding}");
        assert_tagged(&error, "reserve");

        assert!(probe.calls().is_empty(), "{binding}");
    }
}

#[tokio::test]
async fn caller_cancellation_stops_a_call_in_flight() {
    for binding in LOCAL_BINDINGS {
        let (fake, mut entered, mut dropped) = pending();
        let (_app, inventory) = started_inventory(binding, &[], fake).await;
        let cx = CallContext::new();
        let token = cx.cancel_token().clone();
        let call = tokio::spawn(async move { inventory.reserve(&cx, reserve("o1", 1)).await });
        entered.recv().await.unwrap();
        token.cancel();
        let error = other(call.await.unwrap().unwrap_err());
        assert_eq!(error.code(), ErrorCode::Cancelled, "{binding}");
        assert_tagged(&error, "reserve");
        dropped.recv().await.unwrap();
    }
}

#[tokio::test]
async fn dropping_the_caller_stops_the_call() {
    for binding in LOCAL_BINDINGS {
        let (fake, mut entered, mut dropped) = pending();
        let (_app, inventory) = started_inventory(binding, &[], fake).await;
        let call = tokio::spawn(async move {
            inventory
                .reserve(&CallContext::new(), reserve("o1", 1))
                .await
        });
        entered.recv().await.unwrap();
        call.abort();
        assert!(call.await.unwrap_err().is_cancelled());
        dropped.recv().await.unwrap();
    }
}

#[tokio::test]
async fn a_panicking_method_is_internal_when_serialized() {
    let (app, inventory) = started_inventory(
        Binding::LocalSerialized,
        &[],
        FakeInventory::new(Behaviour::Panic),
    )
    .await;
    let error = other(
        inventory
            .reserve(&CallContext::new(), reserve("o1", 1))
            .await
            .unwrap_err(),
    );
    assert_eq!(error.code(), ErrorCode::Internal);
    assert_eq!(
        error.message(),
        "component inventory method reserve panicked"
    );
    assert_tagged(&error, "reserve");
    // The panic released its admission: the component still stops at once.
    app.stop(Duration::ZERO).await.unwrap();
}

#[tokio::test]
async fn a_panicking_method_propagates_when_local() {
    let (_app, inventory) =
        started_inventory(Binding::Local, &[], FakeInventory::new(Behaviour::Panic)).await;
    let call = tokio::spawn(async move {
        inventory
            .reserve(&CallContext::new(), reserve("o1", 1))
            .await
    });
    assert!(call.await.unwrap_err().is_panic());
}

#[tokio::test]
async fn the_dispatcher_decodes_runs_and_encodes() {
    use prost::Message as _;

    let dispatch = dispatcher(FakeInventory::stock(5));
    let body = Bytes::from(reserve("o1", 2).encode_to_vec());
    let reply = dispatch
        .dispatch(0, CallContext::new(), body)
        .await
        .unwrap();
    assert_eq!(ReserveReply::decode(reply).unwrap().remaining, 3);

    let error = dispatch
        .dispatch(
            0,
            CallContext::new(),
            Bytes::from(reserve("o2", 9).encode_to_vec()),
        )
        .await
        .unwrap_err();
    assert_eq!(error.reason(), Some("OUT_OF_STOCK"));
    assert_eq!(error.metadata()["available"], "3");

    let garbage = dispatch
        .dispatch(1, CallContext::new(), Bytes::from_static(&[0x0a, 0xff]))
        .await
        .unwrap_err();
    assert_eq!(garbage.code(), ErrorCode::InvalidArgument);
    assert_eq!(garbage.reason(), Some(reasons::MALFORMED_REQUEST));

    let unknown = dispatch
        .dispatch(7, CallContext::new(), Bytes::new())
        .await
        .unwrap_err();
    assert_eq!(unknown.code(), ErrorCode::Unimplemented);
    assert!(unknown.message().contains("inventory"), "{unknown}");
}

#[tokio::test(start_paused = true)]
async fn the_callee_sees_the_callers_identity_through_a_trusted_local_link() {
    for binding in LOCAL_BINDINGS {
        let fake = FakeInventory::stock(10);
        let probe = fake.probe.clone();
        let (_app, inventory) = started_inventory(binding, &[], fake).await;
        let caller_deadline = deadline_in(Duration::from_secs(10));
        let cx = CallContext::new()
            .with_request_id("req-7")
            .with_caller(ServiceIdentity::untrusted("edge"))
            .with_subject("user-1")
            .with_tenant("tenant-a")
            .with_idempotency_key("order-7")
            .with_deadline(caller_deadline);
        inventory.reserve(&cx, reserve("o1", 1)).await.unwrap();
        inventory.release(&cx, release("res-o1")).await.unwrap();
        let short = CallContext::new().with_deadline(deadline_in(Duration::from_millis(200)));
        inventory.release(&short, release("res-o1")).await.unwrap();

        let calls = probe.calls();
        assert_eq!(calls.len(), 3);
        let expected_limits = [
            Duration::from_secs(2),
            Duration::from_millis(500),
            Duration::from_millis(200),
        ];
        for (seen, limit) in calls.iter().zip(expected_limits) {
            assert_eq!(seen.caller, Some(("local".to_owned(), true)), "{binding}");
            let deadline = seen.deadline.expect("a deadline");
            assert!(deadline <= deadline_in(limit), "{binding}: {seen:?}");
        }
        for seen in &calls[..2] {
            assert_eq!(seen.request_id, "req-7", "{binding}");
            assert_eq!(seen.subject.as_deref(), Some("user-1"), "{binding}");
            assert_eq!(seen.tenant.as_deref(), Some("tenant-a"), "{binding}");
            assert_eq!(
                seen.idempotency_key.as_deref(),
                Some("order-7"),
                "{binding}"
            );
            assert!(seen.deadline.unwrap() <= caller_deadline, "{binding}");
        }
    }
}

#[tokio::test]
async fn an_undecodable_reply_is_internal() {
    let config = source(Binding::LocalSerialized, &[]);
    let app = build_with(&config, |builder| {
        install_with_dispatch(builder, FakeInventory::stock(1), Arc::new(GarbageDispatch))
    })
    .unwrap();
    app.start().await.unwrap();
    let inventory = app.handle::<InventoryHandle>().unwrap();
    let error = other(
        inventory
            .reserve(&CallContext::new(), reserve("o1", 1))
            .await
            .unwrap_err(),
    );
    assert_eq!(error.code(), ErrorCode::Internal);
    assert_eq!(error.reason(), Some(reasons::MALFORMED_REPLY));
    assert_tagged(&error, "reserve");
}

#[tokio::test]
async fn local_only_components_take_plain_values() {
    for binding in LOCAL_BINDINGS {
        let config = source(binding, &[]);
        let app = build_with(&config, |builder| {
            NotesHandle::install(builder, |_| Ok(Counter::plain()))
        })
        .unwrap();
        assert_eq!(app.binding("notes"), Some(Binding::Local));
        app.start().await.unwrap();
        let notes = app.handle::<NotesHandle>().unwrap();
        assert_eq!(notes.binding(), Binding::Local);
        assert_eq!(
            notes
                .add(&CallContext::new(), "héllo".into())
                .await
                .unwrap(),
            5
        );
        let error = notes
            .add(&CallContext::new(), String::new())
            .await
            .unwrap_err();
        assert_eq!(error.reason(), Some("EMPTY_TEXT"));

        let cancelled = CallContext::new();
        cancelled.cancel_token().cancel();
        let error = notes.add(&cancelled, "x".into()).await.unwrap_err();
        assert_eq!(error.code(), ErrorCode::Cancelled);
        assert_eq!(error.metadata()["component"], "notes");
        assert_eq!(error.metadata()["method"], "add");
    }
}

#[tokio::test]
async fn a_component_without_a_dispatcher_cannot_be_serialized() {
    let config = source(Binding::LocalSerialized, &[]);
    let error = build_with(&config, |builder| {
        BareHandle::install(builder, |_| Ok(Counter::plain()))
    })
    .unwrap_err();
    let BuildError::Factory {
        component,
        source: cause,
    } = error
    else {
        panic!("expected a factory error, got {error}");
    };
    assert_eq!(component, "bare");
    assert_eq!(cause.code(), ErrorCode::FailedPrecondition);

    let config = source(Binding::Local, &[]);
    let app = build_with(&config, |builder| {
        BareHandle::install(builder, |_| Ok(Counter::plain()))
    })
    .unwrap();
    app.start().await.unwrap();
    let bare = app.handle::<BareHandle>().unwrap();
    assert_eq!(bare.add(&CallContext::new(), "ab".into()).await.unwrap(), 2);
}

#[tokio::test]
async fn a_local_call_on_a_serialized_route_is_internal_not_a_panic() {
    let config = source(Binding::LocalSerialized, &[]);
    let app = build_with(&config, |builder| {
        OddHandle::install(builder, |_| Ok(Counter::plain()))
    })
    .unwrap();
    app.start().await.unwrap();
    let odd = app.handle::<OddHandle>().unwrap();
    assert_eq!(odd.binding(), Binding::LocalSerialized);
    let error = odd.add(&CallContext::new(), "x".into()).await.unwrap_err();
    assert_eq!(error.code(), ErrorCode::Internal);
    assert_eq!(error.metadata()["component"], "odd");
}
