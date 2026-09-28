//! Starting, draining and stopping components.

mod support;

use std::sync::Arc;
use std::time::Duration;

use sekvent_component::{
    App, AppBuilder, AppError, BuildError, CallContext, ComponentState, ErrorCode, Lifecycle,
    reasons,
};
use support::fakes::{
    AuditHandle, Behaviour, Counter, FakeInventory, HookedInventory, Hooks, LOCAL_BINDINGS, Notes,
    NotesHandle, Recorder, SlowStart, build_inventory, build_with, source,
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

fn unavailable(error: InventoryError, reason: &str) {
    let InventoryError::Other(error) = error else {
        panic!("expected Other, got {error:?}");
    };
    assert_eq!(error.code(), ErrorCode::Unavailable, "{error:?}");
    assert_eq!(error.reason(), Some(reason), "{error:?}");
    assert_eq!(error.metadata()["component"], "inventory");
}

/// inventory, notes and audit, each with recording hooks; `tune` adjusts
/// the hooks of one of them by name.
fn hooked(
    recorder: &Recorder,
    tune: impl Fn(&mut Hooks),
) -> impl FnOnce(&mut AppBuilder<'_>) -> Result<(), BuildError> {
    let recorder = recorder.clone();
    move |builder: &mut AppBuilder<'_>| {
        let hooks = |name: &'static str| {
            let mut hooks = Hooks::new(name, &recorder);
            tune(&mut hooks);
            hooks
        };
        let (inventory, notes, audit) = (hooks("inventory"), hooks("notes"), hooks("audit"));
        InventoryHandle::install_with_lifecycle(builder, move |_| {
            Ok(HookedInventory {
                inner: FakeInventory::stock(10),
                hooks: inventory,
            })
        })?;
        NotesHandle::install_with_lifecycle(builder, move |_| Ok(Counter::hooked(notes)))?;
        AuditHandle::install_with_lifecycle(builder, move |_| Ok(Counter::hooked(audit)))
    }
}

fn states(app: &App) -> Vec<ComponentState> {
    app.components()
        .into_iter()
        .map(|name| app.state(name).unwrap())
        .collect()
}

#[tokio::test]
async fn components_start_in_install_order_and_stop_in_reverse() {
    for binding in LOCAL_BINDINGS {
        let recorder = Recorder::default();
        let config = source(binding, &[]);
        let app = build_with(&config, hooked(&recorder, |_| {})).unwrap();
        app.start().await.unwrap();
        assert_eq!(states(&app), [ComponentState::Serving; 3]);
        let inventory = app.handle::<InventoryHandle>().unwrap();
        inventory
            .reserve(&CallContext::new(), reserve())
            .await
            .unwrap();
        app.stop(Duration::from_secs(5)).await.unwrap();
        assert_eq!(states(&app), [ComponentState::Stopped; 3]);
        assert_eq!(
            recorder.events(),
            [
                "start inventory",
                "start notes",
                "start audit",
                "stop audit",
                "stop notes",
                "stop inventory"
            ]
        );
    }
}

#[tokio::test]
async fn a_failed_start_rolls_back() {
    let recorder = Recorder::default();
    let app = build_with(
        &source(sekvent_component::Binding::Local, &[]),
        hooked(&recorder, |hooks| hooks.fail_start = hooks.name == "notes"),
    )
    .unwrap();
    let error = app.start().await.unwrap_err();
    assert_eq!(error.code(), ErrorCode::Unavailable);
    assert_eq!(error.metadata()["component"], "notes");
    assert_eq!(
        recorder.events(),
        ["start inventory", "start notes", "stop inventory"]
    );
    assert_eq!(states(&app), [ComponentState::Stopped; 3]);

    let again = app.start().await.unwrap_err();
    assert_eq!(again.code(), ErrorCode::FailedPrecondition);
    app.stop(Duration::ZERO).await.unwrap();
    assert_eq!(recorder.events().len(), 3);
}

#[tokio::test]
async fn stop_hook_errors_are_reported_after_everything_stopped() {
    let recorder = Recorder::default();
    let app = build_with(
        &source(sekvent_component::Binding::Local, &[]),
        hooked(&recorder, |hooks| hooks.fail_stop = hooks.name != "notes"),
    )
    .unwrap();
    app.start().await.unwrap();
    let error = app.stop(Duration::from_secs(1)).await.unwrap_err();
    assert_eq!(error.code(), ErrorCode::Internal);
    assert_eq!(error.metadata()["component"], "audit");
    assert_eq!(states(&app), [ComponentState::Stopped; 3]);
    assert_eq!(recorder.events().len(), 6);
}

#[tokio::test]
async fn start_twice_and_stop_idempotently() {
    let recorder = Recorder::default();
    let app = build_with(
        &source(sekvent_component::Binding::Local, &[]),
        hooked(&recorder, |_| {}),
    )
    .unwrap();
    app.start().await.unwrap();
    let twice = app.start().await.unwrap_err();
    assert_eq!(twice.code(), ErrorCode::FailedPrecondition);
    assert_eq!(twice.message(), "the app was already started");
    app.stop(Duration::ZERO).await.unwrap();
    app.stop(Duration::ZERO).await.unwrap();
    assert_eq!(recorder.events().len(), 6);
    let after = app.start().await.unwrap_err();
    assert_eq!(
        after.message(),
        "the app was stopped and cannot start again"
    );
}

#[tokio::test]
async fn stopping_a_never_started_app_only_marks_it_stopped() {
    let recorder = Recorder::default();
    let app = build_with(
        &source(sekvent_component::Binding::Local, &[]),
        hooked(&recorder, |_| {}),
    )
    .unwrap();
    app.stop(Duration::from_secs(1)).await.unwrap();
    assert_eq!(states(&app), [ComponentState::Stopped; 3]);
    assert!(recorder.events().is_empty());
    assert_eq!(
        app.start().await.unwrap_err().code(),
        ErrorCode::FailedPrecondition
    );
}

#[tokio::test]
async fn calls_are_rejected_until_started_and_after_stopped() {
    for binding in LOCAL_BINDINGS {
        let app = build_inventory(binding, &[], FakeInventory::stock(10)).unwrap();
        let inventory = app.handle::<InventoryHandle>().unwrap();
        let error = inventory
            .reserve(&CallContext::new(), reserve())
            .await
            .unwrap_err();
        unavailable(error, reasons::NOT_STARTED);
        app.start().await.unwrap();
        inventory
            .reserve(&CallContext::new(), reserve())
            .await
            .unwrap();
        app.stop(Duration::ZERO).await.unwrap();
        let error = inventory
            .reserve(&CallContext::new(), reserve())
            .await
            .unwrap_err();
        unavailable(error, reasons::STOPPED);
    }
}

#[tokio::test]
async fn stopping_drains_calls_in_flight() {
    for binding in LOCAL_BINDINGS {
        let (entered, mut entered_rx) = mpsc::unbounded_channel();
        let permits = Arc::new(Semaphore::new(0));
        let fake = FakeInventory::new(Behaviour::Gated {
            entered,
            permits: Arc::clone(&permits),
        });
        let app = build_inventory(binding, &[], fake).unwrap();
        app.start().await.unwrap();
        let inventory = app.handle::<InventoryHandle>().unwrap();

        let in_flight = {
            let inventory = inventory.clone();
            tokio::spawn(async move { inventory.reserve(&CallContext::new(), reserve()).await })
        };
        entered_rx.recv().await.unwrap();

        let stopping = {
            let app = app.clone();
            tokio::spawn(async move { app.stop(Duration::from_secs(30)).await })
        };
        while app.state("inventory") != Some(ComponentState::Draining) {
            tokio::task::yield_now().await;
        }
        let error = inventory
            .reserve(&CallContext::new(), reserve())
            .await
            .unwrap_err();
        unavailable(error, reasons::DRAINING);
        assert!(!stopping.is_finished());

        permits.add_permits(1);
        in_flight.await.unwrap().unwrap();
        stopping.await.unwrap().unwrap();
        assert_eq!(app.state("inventory"), Some(ComponentState::Stopped));
        let error = inventory
            .reserve(&CallContext::new(), reserve())
            .await
            .unwrap_err();
        unavailable(error, reasons::STOPPED);
    }
}

#[tokio::test(start_paused = true)]
async fn the_grace_period_bounds_the_drain() {
    for binding in LOCAL_BINDINGS {
        let (entered, mut entered_rx) = mpsc::unbounded_channel();
        let (dropped, _dropped_rx) = mpsc::unbounded_channel();
        let fake = FakeInventory::new(Behaviour::Pending { entered, dropped });
        let app = build_inventory(
            binding,
            &[("SEKVENT_COMPONENT_INVENTORY_TIMEOUT", "1h")],
            fake,
        )
        .unwrap();
        app.start().await.unwrap();
        let inventory = app.handle::<InventoryHandle>().unwrap();
        let stuck =
            tokio::spawn(async move { inventory.reserve(&CallContext::new(), reserve()).await });
        entered_rx.recv().await.unwrap();

        let started = Instant::now();
        app.stop(Duration::from_secs(1)).await.unwrap();
        assert_eq!(started.elapsed(), Duration::from_secs(1), "{binding}");
        assert_eq!(app.state("inventory"), Some(ComponentState::Stopped));
        assert!(!stuck.is_finished());
        stuck.abort();
    }
}

/// inventory (recording hooks) then notes, whose `on_start` waits for a
/// permit that never comes; `entered` fires once it runs.
fn slow_second(recorder: &Recorder) -> (App, mpsc::UnboundedReceiver<()>) {
    let (entered, entered_rx) = mpsc::unbounded_channel();
    let slow = SlowStart {
        entered,
        permits: Arc::new(Semaphore::new(0)),
        recorder: recorder.clone(),
    };
    let hooks = Hooks::new("inventory", recorder);
    let app = build_with(&source(sekvent_component::Binding::Local, &[]), |builder| {
        InventoryHandle::install_with_lifecycle(builder, move |_| {
            Ok(HookedInventory {
                inner: FakeInventory::stock(10),
                hooks,
            })
        })?;
        NotesHandle::install_with_lifecycle(builder, move |_| Ok(slow))
    })
    .unwrap();
    (app, entered_rx)
}

#[tokio::test]
async fn a_stop_during_start_wins() {
    let recorder = Recorder::default();
    let (app, mut entered_rx) = slow_second(&recorder);

    let starting = {
        let app = app.clone();
        tokio::spawn(async move { app.start().await })
    };
    entered_rx.recv().await.unwrap();

    let second = {
        let app = app.clone();
        tokio::spawn(async move { app.stop(Duration::from_secs(1)).await })
    };
    app.stop(Duration::ZERO).await.unwrap();
    // Stopped only once every started component ran its stop hook, and
    // notes, whose start was cancelled, never runs one.
    assert_eq!(states(&app), [ComponentState::Stopped; 2]);
    assert_eq!(
        recorder.events(),
        ["start inventory", "start notes", "stop inventory"]
    );
    second.await.unwrap().unwrap();

    let error = starting.await.unwrap().unwrap_err();
    assert_eq!(error.code(), ErrorCode::FailedPrecondition);
    assert_eq!(error.message(), "the app was stopped while it was starting");
    assert_eq!(error.metadata()["component"], "notes");
    assert_eq!(recorder.events().len(), 3);
}

#[tokio::test]
async fn dropping_an_unfinished_start_rolls_it_back() {
    let recorder = Recorder::default();
    let (app, mut entered_rx) = slow_second(&recorder);
    let starting = {
        let app = app.clone();
        tokio::spawn(async move { app.start().await })
    };
    entered_rx.recv().await.unwrap();
    starting.abort();
    assert!(starting.await.unwrap_err().is_cancelled());

    // The rollback runs on its own task; a stop waits for it.
    app.stop(Duration::from_secs(1)).await.unwrap();
    assert_eq!(states(&app), [ComponentState::Stopped; 2]);
    assert_eq!(
        recorder.events(),
        ["start inventory", "start notes", "stop inventory"]
    );
    assert_eq!(
        app.start().await.unwrap_err().message(),
        "the app was stopped and cannot start again"
    );
}

#[test]
fn a_start_dropped_outside_a_runtime_marks_the_app_stopped() {
    let recorder = Recorder::default();
    let (app, mut entered_rx) = slow_second(&recorder);
    let mut starting = Box::pin(app.start());
    let mut poll = std::task::Context::from_waker(std::task::Waker::noop());
    assert!(starting.as_mut().poll(&mut poll).is_pending());
    entered_rx.try_recv().unwrap();
    drop(starting);

    assert_eq!(states(&app), [ComponentState::Stopped; 2]);
    assert_eq!(recorder.events(), ["start inventory", "start notes"]);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(app.stop(Duration::ZERO)).unwrap();
    assert!(runtime.block_on(app.start()).is_err());
}

#[tokio::test]
async fn dropping_a_stop_does_not_interrupt_it() {
    let recorder = Recorder::default();
    let (entered, mut entered_rx) = mpsc::unbounded_channel();
    let permits = Arc::new(Semaphore::new(0));
    let hooks = Hooks::new("inventory", &recorder);
    let fake = FakeInventory::new(Behaviour::Gated {
        entered,
        permits: Arc::clone(&permits),
    });
    let app = build_with(&source(sekvent_component::Binding::Local, &[]), |builder| {
        InventoryHandle::install_with_lifecycle(builder, move |_| {
            Ok(HookedInventory { inner: fake, hooks })
        })
    })
    .unwrap();
    app.start().await.unwrap();
    let inventory = app.handle::<InventoryHandle>().unwrap();
    let in_flight =
        tokio::spawn(async move { inventory.reserve(&CallContext::new(), reserve()).await });
    entered_rx.recv().await.unwrap();

    let stopping = {
        let app = app.clone();
        tokio::spawn(async move { app.stop(Duration::from_secs(30)).await })
    };
    while app.state("inventory") != Some(ComponentState::Draining) {
        tokio::task::yield_now().await;
    }
    stopping.abort();
    assert!(stopping.await.unwrap_err().is_cancelled());

    let waiting = {
        let app = app.clone();
        tokio::spawn(async move { app.stop(Duration::ZERO).await })
    };
    tokio::task::yield_now().await;
    assert!(!waiting.is_finished(), "the drain still waits for the call");
    assert_eq!(app.state("inventory"), Some(ComponentState::Draining));

    permits.add_permits(1);
    in_flight.await.unwrap().unwrap();
    waiting.await.unwrap().unwrap();
    assert_eq!(app.state("inventory"), Some(ComponentState::Stopped));
    assert_eq!(recorder.events(), ["start inventory", "stop inventory"]);
}

/// A `notes` implementation whose stop hook panics.
struct PanickyStop;

impl Notes for PanickyStop {
    async fn add(&self, _cx: &CallContext, req: String) -> Result<usize, AppError> {
        Ok(req.len())
    }
}

impl Lifecycle for PanickyStop {
    async fn on_stop(&self) -> Result<(), AppError> {
        panic!("the stop hook panicked");
    }
}

#[tokio::test]
async fn a_panicking_stop_hook_fails_only_its_component() {
    let recorder = Recorder::default();
    let hooks = Hooks::new("audit", &recorder);
    let app = build_with(&source(sekvent_component::Binding::Local, &[]), |builder| {
        AuditHandle::install_with_lifecycle(builder, move |_| Ok(Counter::hooked(hooks)))?;
        NotesHandle::install_with_lifecycle(builder, |_| Ok(PanickyStop))
    })
    .unwrap();
    app.start().await.unwrap();
    let error = app.stop(Duration::ZERO).await.unwrap_err();
    assert_eq!(error.code(), ErrorCode::Internal, "{error:?}");
    assert_eq!(error.metadata()["component"], "notes");
    assert_eq!(states(&app), [ComponentState::Stopped; 2]);
    assert_eq!(recorder.events(), ["start audit", "stop audit"]);
}
