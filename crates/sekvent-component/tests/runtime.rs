//! An App run as one unit of the sekvent runtime, and a serialized call
//! whose serving runtime goes away.

mod support;

use std::pin::pin;
use std::sync::Arc;
use std::task::{Context, Waker};
use std::time::Duration;

use sekvent_component::{Binding, CallContext, ComponentState, ErrorCode};
use sekvent_runtime::{Runtime, ShutdownReason, Stage, UnitExit};
use support::fakes::{
    Behaviour, FakeInventory, HookedInventory, Hooks, LOCAL_BINDINGS, NotesHandle, Recorder,
    SlowStart, build_inventory, build_with, source,
};
use support::inventory::{InventoryError, InventoryHandle, ReserveRequest};
use tokio::sync::{Semaphore, mpsc};
use tokio::time::Instant;

fn reserve() -> ReserveRequest {
    ReserveRequest {
        order_id: "o1".into(),
        sku: "sku-1".into(),
        quantity: 1,
    }
}

fn hooked(recorder: &Recorder, fail_start: bool, fail_stop: bool) -> sekvent_component::App {
    let mut hooks = Hooks::new("inventory", recorder);
    hooks.fail_start = fail_start;
    hooks.fail_stop = fail_stop;
    build_with(&source(Binding::Local, &[]), |builder| {
        InventoryHandle::install_with_lifecycle(builder, move |_| {
            Ok(HookedInventory {
                inner: FakeInventory::stock(10),
                hooks,
            })
        })
    })
    .unwrap()
}

#[tokio::test]
async fn the_runtime_starts_serves_and_stops_the_app() {
    for binding in LOCAL_BINDINGS {
        let app = build_inventory(binding, &[], FakeInventory::stock(10)).unwrap();
        let runtime = app
            .register(Runtime::builder().without_signals())
            .build()
            .unwrap();
        let handle = runtime.start().await.unwrap();
        assert_eq!(app.state("inventory"), Some(ComponentState::Serving));
        assert!(handle.health().is_ready(), "{binding}");

        let inventory = app.handle::<InventoryHandle>().unwrap();
        let reply = inventory
            .reserve(&CallContext::new(), reserve())
            .await
            .unwrap();
        assert_eq!(reply.remaining, 9, "{binding}");

        handle.shutdown();
        let report = handle.wait().await.unwrap();
        assert_eq!(report.reason, ShutdownReason::Requested);
        let unit = report.unit("components").unwrap();
        assert_eq!(unit.stage, Stage::Components);
        assert_eq!(unit.exit, UnitExit::Completed);
        assert_eq!(app.state("inventory"), Some(ComponentState::Stopped));
        let after = inventory
            .reserve(&CallContext::new(), reserve())
            .await
            .unwrap_err();
        assert!(matches!(after, InventoryError::Other(_)), "{after:?}");
    }
}

#[tokio::test]
async fn hooks_run_once_each_under_the_runtime() {
    let recorder = Recorder::default();
    let app = hooked(&recorder, false, false);
    let handle = app
        .register(Runtime::builder().without_signals())
        .build()
        .unwrap()
        .start()
        .await
        .unwrap();
    assert_eq!(recorder.events(), ["start inventory"]);
    handle.shutdown();
    handle.wait().await.unwrap();
    assert_eq!(recorder.events(), ["start inventory", "stop inventory"]);
}

#[tokio::test]
async fn a_failed_start_fails_the_runtime() {
    let recorder = Recorder::default();
    let app = hooked(&recorder, true, false);
    let error = app
        .register(Runtime::builder().without_signals())
        .build()
        .unwrap()
        .start()
        .await
        .unwrap_err();
    assert_eq!(error.code(), ErrorCode::Unavailable, "{error:?}");
    assert_eq!(error.metadata()["component"], "inventory");
    assert_eq!(app.state("inventory"), Some(ComponentState::Stopped));
    assert_eq!(recorder.events(), ["start inventory"]);
}

#[tokio::test]
async fn a_failed_stop_hook_fails_the_run() {
    let recorder = Recorder::default();
    let app = hooked(&recorder, false, true);
    let handle = app
        .register(Runtime::builder().without_signals())
        .build()
        .unwrap()
        .start()
        .await
        .unwrap();
    handle.shutdown();
    let error = handle.wait().await.unwrap_err();
    assert_eq!(error.code(), ErrorCode::Internal, "{error:?}");
    assert_eq!(error.metadata()["component"], "inventory");
    assert_eq!(app.state("inventory"), Some(ComponentState::Stopped));
}

/// The shutdown deadline (4 s) is far shorter than the stage grace (60 s):
/// the drain takes half of it, so the stop hook still runs before the
/// runtime would abort the unit.
#[tokio::test(start_paused = true)]
async fn the_drain_leaves_the_stop_hooks_time_before_the_shutdown_deadline() {
    let recorder = Recorder::default();
    let (entered, mut entered_rx) = mpsc::unbounded_channel();
    let (dropped, _dropped_rx) = mpsc::unbounded_channel();
    let hooks = Hooks::new("inventory", &recorder);
    let fake = FakeInventory::new(Behaviour::Pending { entered, dropped });
    let config = source(
        Binding::Local,
        &[("SEKVENT_COMPONENT_INVENTORY_TIMEOUT", "1h")],
    );
    let app = build_with(&config, |builder| {
        InventoryHandle::install_with_lifecycle(builder, move |_| {
            Ok(HookedInventory { inner: fake, hooks })
        })
    })
    .unwrap();
    let handle = app
        .register(
            Runtime::builder()
                .without_signals()
                .stage_grace(Duration::from_secs(60))
                .shutdown_deadline(Duration::from_secs(4)),
        )
        .build()
        .unwrap()
        .start()
        .await
        .unwrap();
    let inventory = app.handle::<InventoryHandle>().unwrap();
    let stuck =
        tokio::spawn(async move { inventory.reserve(&CallContext::new(), reserve()).await });
    entered_rx.recv().await.unwrap();

    let started = Instant::now();
    handle.shutdown();
    let report = handle.wait().await.unwrap();
    assert_eq!(started.elapsed(), Duration::from_secs(2));
    assert_eq!(report.unit("components").unwrap().exit, UnitExit::Completed);
    assert_eq!(recorder.events(), ["start inventory", "stop inventory"]);
    assert_eq!(app.state("inventory"), Some(ComponentState::Stopped));
    stuck.abort();
}

#[tokio::test]
async fn a_shutdown_during_start_abandons_it() {
    let recorder = Recorder::default();
    let (entered, mut entered_rx) = mpsc::unbounded_channel();
    let slow = SlowStart {
        entered,
        permits: Arc::new(Semaphore::new(0)),
        recorder: recorder.clone(),
    };
    let hooks = Hooks::new("inventory", &recorder);
    let app = build_with(&source(Binding::Local, &[]), |builder| {
        InventoryHandle::install_with_lifecycle(builder, move |_| {
            Ok(HookedInventory {
                inner: FakeInventory::stock(10),
                hooks,
            })
        })?;
        NotesHandle::install_with_lifecycle(builder, move |_| Ok(slow))
    })
    .unwrap();
    let builder = app.register(Runtime::builder().without_signals());
    let trigger = builder.shutdown_trigger();
    let starting = tokio::spawn(async move { builder.build().unwrap().start().await });
    entered_rx.recv().await.unwrap();

    trigger.shutdown();
    let error = starting.await.unwrap().unwrap_err();
    assert_eq!(error.code(), ErrorCode::Cancelled, "{error:?}");
    assert_eq!(app.state("inventory"), Some(ComponentState::Stopped));
    assert_eq!(app.state("notes"), Some(ComponentState::Stopped));
    assert_eq!(
        recorder.events(),
        ["start inventory", "start notes", "stop inventory"]
    );
}

/// The serving task is spawned on a second runtime that never runs it and is
/// then shut down, so the caller sees the task cancelled rather than
/// panicked.
#[test]
fn a_serialized_call_is_cancelled_when_its_serving_runtime_shuts_down() {
    let driver = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let app = build_inventory(Binding::LocalSerialized, &[], FakeInventory::stock(10)).unwrap();
    driver.block_on(app.start()).unwrap();
    let inventory = app.handle::<InventoryHandle>().unwrap();
    let cx = CallContext::new();
    let mut call = pin!(inventory.reserve(&cx, reserve()));

    let serving = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    {
        let _entered = serving.enter();
        let mut poll = Context::from_waker(Waker::noop());
        assert!(call.as_mut().poll(&mut poll).is_pending());
    }
    drop(serving);

    let error = driver.block_on(call).unwrap_err();
    let InventoryError::Other(error) = error else {
        panic!("expected Other, got {error:?}");
    };
    assert_eq!(error.code(), ErrorCode::Cancelled, "{error:?}");
    assert_eq!(error.metadata()["component"], "inventory");
    assert_eq!(error.metadata()["method"], "reserve");

    driver.block_on(app.stop(Duration::ZERO)).unwrap();
    assert_eq!(app.state("inventory"), Some(ComponentState::Stopped));
}
