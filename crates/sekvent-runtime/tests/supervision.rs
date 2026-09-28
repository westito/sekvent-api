//! Lifecycle and supervision semantics, driven through the public API.
//!
//! Time-dependent tests run on tokio's paused clock, so backoffs, timeouts
//! and grace periods elapse instantly and exactly.

use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use sekvent_error::{AppError, ErrorCode};
use sekvent_runtime::{
    DependencyProbe, ProbeStatus, RestartPolicy, Runtime, RuntimeBuilder, ShutdownReason, Stage,
    UnitContext, UnitExit, UnitPolicy,
};
use tokio::sync::oneshot;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

fn runtime() -> RuntimeBuilder {
    Runtime::builder().without_signals()
}

async fn until_shutdown(ctx: UnitContext) -> Result<(), AppError> {
    ctx.ready();
    ctx.shutdown().cancelled().await;
    Ok(())
}

/// A one-shot value handed to the first run of a unit.
fn once<T>(value: T) -> impl FnMut() -> Option<T> {
    let mut slot = Some(value);
    move || slot.take()
}

type Log = Arc<Mutex<Vec<String>>>;

fn push(log: &Log, entry: String) {
    log.lock().unwrap().push(entry);
}

#[tokio::test]
async fn a_critical_failure_stops_every_unit_and_fails_the_run() {
    let (stopped_tx, stopped_rx) = oneshot::channel::<()>();
    let mut stopped = once(stopped_tx);
    let (fail_tx, fail_rx) = oneshot::channel::<()>();
    let mut fail = once(fail_rx);

    let run = runtime()
        .unit(
            "store",
            Stage::Infrastructure,
            UnitPolicy::Critical,
            move |ctx: UnitContext| {
                let stopped = stopped();
                async move {
                    until_shutdown(ctx).await?;
                    if let Some(stopped) = stopped {
                        let _ = stopped.send(());
                    }
                    Ok(())
                }
            },
        )
        .unit(
            "worker",
            Stage::Workers,
            UnitPolicy::Critical,
            move |ctx: UnitContext| {
                let fail = fail();
                async move {
                    ctx.ready();
                    if let Some(fail) = fail {
                        let _ = fail.await;
                    }
                    Err(AppError::unavailable("queue lost"))
                }
            },
        )
        .build()
        .unwrap();
    let running = tokio::spawn(run.run());
    fail_tx.send(()).unwrap();

    let error = running.await.unwrap().unwrap_err();
    assert_eq!(error.code(), ErrorCode::Unavailable);
    assert_eq!(error.message(), "queue lost");
    stopped_rx.await.expect("the other unit was drained");
}

#[tokio::test]
async fn a_critical_unit_that_finishes_shuts_down_cleanly() {
    let report = runtime()
        .unit(
            "serve",
            Stage::Components,
            UnitPolicy::Critical,
            until_shutdown,
        )
        .unit(
            "migrate",
            Stage::Workers,
            UnitPolicy::default(),
            |_ctx| async { Ok(()) },
        )
        .build()
        .unwrap()
        .run()
        .await
        .unwrap();
    assert_eq!(
        report.reason,
        ShutdownReason::UnitExited {
            unit: "migrate".into()
        }
    );
    assert_eq!(report.unit("migrate").unwrap().exit, UnitExit::Completed);
    assert_eq!(report.unit("serve").unwrap().exit, UnitExit::Completed);
    assert!(report.unit("nothing").is_none());
}

#[tokio::test(start_paused = true)]
async fn a_restarting_unit_backs_off_then_escalates() {
    let attempts: Arc<Mutex<Vec<(u32, Instant)>>> = Arc::default();
    let seen = Arc::clone(&attempts);
    let policy = RestartPolicy {
        initial: Duration::from_secs(1),
        max: Duration::from_secs(3),
        multiplier: 2.0,
        max_restarts: Some(3),
    };
    let error = runtime()
        .unit(
            "flaky",
            Stage::Workers,
            UnitPolicy::Restart(policy),
            move |ctx: UnitContext| {
                seen.lock().unwrap().push((ctx.attempt(), Instant::now()));
                async { Err(AppError::unavailable("connection refused")) }
            },
        )
        .build()
        .unwrap()
        .run()
        .await
        .unwrap_err();

    assert_eq!(error.code(), ErrorCode::Unavailable);
    assert!(error.message().contains("flaky") && error.message().contains("3 restarts"));
    let source = std::error::Error::source(&error).expect("last failure kept as source");
    assert!(source.to_string().contains("connection refused"));

    let attempts = attempts.lock().unwrap();
    let numbers: Vec<u32> = attempts.iter().map(|(n, _)| *n).collect();
    assert_eq!(numbers, [0, 1, 2, 3]);
    let gaps: Vec<Duration> = attempts.windows(2).map(|w| w[1].1 - w[0].1).collect();
    assert_eq!(
        gaps,
        [
            Duration::from_secs(1),
            Duration::from_secs(2),
            Duration::from_secs(3)
        ]
    );
}

#[tokio::test(start_paused = true)]
async fn a_unit_without_restarts_left_escalates_even_on_success() {
    let policy = RestartPolicy {
        max_restarts: Some(0),
        ..RestartPolicy::default()
    };
    let error = runtime()
        .unit(
            "job",
            Stage::Workers,
            UnitPolicy::Restart(policy),
            |_ctx| async { Ok(()) },
        )
        .build()
        .unwrap()
        .run()
        .await
        .unwrap_err();
    assert!(error.message().contains("job"));
    assert!(std::error::Error::source(&error).is_none());
}

#[tokio::test(start_paused = true)]
async fn shutdown_interrupts_a_restart_backoff() {
    let (ran_tx, ran_rx) = oneshot::channel::<()>();
    let mut ran = once(ran_tx);
    let policy = RestartPolicy {
        initial: Duration::from_secs(3_600),
        max: Duration::from_secs(3_600),
        ..RestartPolicy::default()
    };
    let handle = runtime()
        .unit(
            "loop",
            Stage::Workers,
            UnitPolicy::Restart(policy),
            move |ctx: UnitContext| {
                let ran = ran();
                async move {
                    ctx.ready();
                    if let Some(ran) = ran {
                        let _ = ran.send(());
                    }
                    Ok(())
                }
            },
        )
        .build()
        .unwrap()
        .start()
        .await
        .unwrap();
    ran_rx.await.unwrap();
    let started = Instant::now();
    handle.shutdown();
    let report = handle.wait().await.unwrap();
    assert_eq!(
        Instant::now(),
        started,
        "the hour-long backoff was cut short"
    );
    let unit = report.unit("loop").unwrap();
    assert_eq!(unit.restarts, 1);
    assert_eq!(unit.exit, UnitExit::Completed);
}

#[tokio::test]
async fn a_best_effort_exit_is_logged_and_ignored() {
    let handle = runtime()
        .unit(
            "warmup",
            Stage::Workers,
            UnitPolicy::BestEffort,
            |_ctx| async { Err(AppError::unavailable("cache cold")) },
        )
        .unit(
            "serve",
            Stage::Ingress,
            UnitPolicy::Critical,
            until_shutdown,
        )
        .build()
        .unwrap()
        .start()
        .await
        .unwrap();
    assert!(!handle.is_shutting_down());
    assert!(handle.health().is_ready());
    handle.shutdown();
    let report = handle.wait().await.unwrap();
    assert_eq!(report.reason, ShutdownReason::Requested);
    assert_eq!(
        report.unit("warmup").unwrap().exit,
        UnitExit::Failed("UNAVAILABLE: cache cold".into())
    );
    assert_eq!(report.unit("serve").unwrap().exit, UnitExit::Completed);
}

#[tokio::test]
async fn stages_start_in_order_and_stop_in_reverse() {
    let log: Log = Arc::default();
    let mut builder = runtime();
    // Registered out of order on purpose: the stage decides, not the call order.
    for stage in [
        Stage::Ingress,
        Stage::Infrastructure,
        Stage::Workers,
        Stage::Components,
    ] {
        let log = Arc::clone(&log);
        builder = builder.unit(
            stage.as_str(),
            stage,
            UnitPolicy::Critical,
            move |ctx: UnitContext| {
                let log = Arc::clone(&log);
                async move {
                    // Yield before reporting ready: a later stage must still wait.
                    tokio::task::yield_now().await;
                    push(&log, format!("start {}", ctx.name()));
                    ctx.ready();
                    ctx.shutdown().cancelled().await;
                    tokio::task::yield_now().await;
                    push(&log, format!("stop {}", ctx.name()));
                    Ok(())
                }
            },
        );
    }
    let handle = builder.build().unwrap().start().await.unwrap();
    handle.shutdown();
    let report = handle.wait().await.unwrap();

    assert_eq!(
        *log.lock().unwrap(),
        [
            "start infrastructure",
            "start components",
            "start workers",
            "start ingress",
            "stop ingress",
            "stop workers",
            "stop components",
            "stop infrastructure",
        ]
    );
    let order: Vec<&str> = report.units.iter().map(|unit| unit.name.as_str()).collect();
    assert_eq!(
        order,
        ["infrastructure", "components", "workers", "ingress"]
    );
}

#[tokio::test(start_paused = true)]
async fn a_stage_that_does_not_start_in_time_fails_startup() {
    let later_started = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&later_started);
    let started = Instant::now();
    let error = runtime()
        .start_timeout(Duration::from_secs(5))
        .unit(
            "slow",
            Stage::Components,
            UnitPolicy::Critical,
            |ctx: UnitContext| async move {
                ctx.shutdown().cancelled().await;
                Ok(())
            },
        )
        .unit(
            "fast",
            Stage::Components,
            UnitPolicy::Critical,
            until_shutdown,
        )
        .unit("api", Stage::Ingress, UnitPolicy::Critical, move |_ctx| {
            flag.store(true, Ordering::SeqCst);
            async { Ok(()) }
        })
        .build()
        .unwrap()
        .start()
        .await
        .unwrap_err();

    assert_eq!(Instant::now() - started, Duration::from_secs(5));
    assert_eq!(error.code(), ErrorCode::DeadlineExceeded);
    assert!(error.message().contains("components"));
    assert!(error.message().contains("slow") && !error.message().contains("fast"));
    assert!(!later_started.load(Ordering::SeqCst));
}

#[tokio::test]
async fn a_failure_during_startup_is_returned_by_start() {
    let error = runtime()
        .unit(
            "config",
            Stage::Infrastructure,
            UnitPolicy::Critical,
            |_ctx| async { Err(AppError::failed_precondition("missing key ORDERS_DB_URL")) },
        )
        .unit("api", Stage::Ingress, UnitPolicy::Critical, until_shutdown)
        .build()
        .unwrap()
        .start()
        .await
        .unwrap_err();
    assert_eq!(error.code(), ErrorCode::FailedPrecondition);
}

#[tokio::test]
async fn a_signal_during_startup_cancels_start_and_skips_later_stages() {
    let (never_tx, never_rx) = oneshot::channel::<()>();
    let error = runtime()
        .shutdown_on(async {})
        .unit(
            "boot",
            Stage::Infrastructure,
            UnitPolicy::Critical,
            move |ctx: UnitContext| async move {
                // Never reports ready; only shutdown ends it.
                ctx.shutdown().cancelled().await;
                Ok(())
            },
        )
        .unit("api", Stage::Ingress, UnitPolicy::Critical, move |_ctx| {
            let _ = &never_tx;
            async { Ok(()) }
        })
        .build()
        .unwrap()
        .start()
        .await
        .unwrap_err();
    assert_eq!(error.code(), ErrorCode::Cancelled);
    assert!(error.message().contains("shutdown signal"));
    // The ingress factory (which owns the sender) was dropped without running.
    assert!(never_rx.await.is_err());
}

#[tokio::test(start_paused = true)]
async fn a_unit_that_ignores_shutdown_is_aborted_after_its_grace() {
    let (guard_tx, guard_rx) = oneshot::channel::<()>();
    let mut guard = once(guard_tx);
    let handle = runtime()
        .stage_grace(Duration::from_secs(2))
        .unit(
            "stubborn",
            Stage::Workers,
            UnitPolicy::Critical,
            move |ctx: UnitContext| {
                let guard = guard();
                async move {
                    let _guard = guard;
                    ctx.ready();
                    std::future::pending::<()>().await;
                    Ok(())
                }
            },
        )
        .build()
        .unwrap()
        .start()
        .await
        .unwrap();
    let started = Instant::now();
    handle.shutdown();
    let report = handle.wait().await.unwrap();
    assert_eq!(Instant::now() - started, Duration::from_secs(2));
    assert_eq!(report.unit("stubborn").unwrap().exit, UnitExit::Aborted);
    assert!(
        guard_rx.await.is_err(),
        "the aborted unit's future was dropped"
    );
}

#[tokio::test(start_paused = true)]
async fn the_shutdown_deadline_bounds_every_stage() {
    let stubborn = |ctx: UnitContext| async move {
        ctx.ready();
        std::future::pending::<()>().await;
        Ok(())
    };
    let handle = runtime()
        .stage_grace(Duration::from_secs(10))
        .shutdown_delay(Duration::from_secs(1))
        .shutdown_deadline(Duration::from_secs(3))
        .unit("a", Stage::Components, UnitPolicy::Critical, stubborn)
        .unit("b", Stage::Workers, UnitPolicy::Critical, stubborn)
        .build()
        .unwrap()
        .start()
        .await
        .unwrap();
    let started = Instant::now();
    handle.shutdown();
    let report = handle.wait().await.unwrap();
    assert_eq!(Instant::now() - started, Duration::from_secs(3));
    assert!(
        report
            .units
            .iter()
            .all(|unit| unit.exit == UnitExit::Aborted)
    );
}

#[tokio::test(start_paused = true)]
async fn health_turns_not_serving_before_the_shutdown_delay() {
    let (seen_tx, seen_rx) = oneshot::channel::<(Instant, bool)>();
    let mut seen = once(seen_tx);
    let handle = runtime()
        .shutdown_delay(Duration::from_secs(5))
        .unit(
            "api",
            Stage::Ingress,
            UnitPolicy::Critical,
            move |ctx: UnitContext| {
                let seen = seen();
                async move {
                    ctx.ready();
                    ctx.shutdown().cancelled().await;
                    if let Some(seen) = seen {
                        let _ = seen.send((Instant::now(), ctx.health().is_ready()));
                    }
                    Ok(())
                }
            },
        )
        .build()
        .unwrap()
        .start()
        .await
        .unwrap();
    assert!(handle.health().is_ready());
    let requested = Instant::now();
    let trigger = handle.trigger();
    trigger.shutdown();
    assert!(trigger.is_triggered());
    trigger.triggered().await;

    let (drained_at, ready_when_drained) = seen_rx.await.unwrap();
    assert_eq!(drained_at - requested, Duration::from_secs(5));
    assert!(!ready_when_drained);
    assert!(!handle.health().is_ready());
    handle.wait().await.unwrap();
}

#[tokio::test]
async fn a_signal_source_that_failed_to_register_is_not_a_signal() {
    let (polled_tx, polled_rx) = oneshot::channel::<()>();
    let handle = runtime()
        .shutdown_on_signal("SIGTEST", async move {
            let _ = polled_tx.send(());
            Err(io::Error::other("handler refused"))
        })
        .unit("api", Stage::Ingress, UnitPolicy::Critical, until_shutdown)
        .build()
        .unwrap()
        .start()
        .await
        .unwrap();
    // The source has resolved with its error by the time this returns; the
    // watcher logged it in that same poll.
    polled_rx.await.unwrap();
    tokio::task::yield_now().await;
    assert!(!handle.is_shutting_down());

    handle.shutdown();
    assert_eq!(
        handle.wait().await.unwrap().reason,
        ShutdownReason::Requested
    );
}

#[tokio::test]
async fn a_delivered_signal_or_trigger_future_shuts_down() {
    let delivered = runtime()
        .shutdown_on_signal("SIGTEST", async { Ok(()) })
        .unit("api", Stage::Ingress, UnitPolicy::Critical, until_shutdown)
        .build()
        .unwrap()
        .run()
        .await
        .unwrap();
    assert_eq!(delivered.reason, ShutdownReason::Signal);

    let (go_tx, go_rx) = oneshot::channel::<()>();
    let running = tokio::spawn(
        runtime()
            .shutdown_on(async move {
                let _ = go_rx.await;
            })
            .unit("api", Stage::Ingress, UnitPolicy::Critical, until_shutdown)
            .build()
            .unwrap()
            .run(),
    );
    go_tx.send(()).unwrap();
    assert_eq!(
        running.await.unwrap().unwrap().reason,
        ShutdownReason::Signal
    );
}

#[tokio::test]
async fn os_signals_are_installed_by_default_without_firing() {
    let builder =
        Runtime::builder().unit("api", Stage::Ingress, UnitPolicy::Critical, until_shutdown);
    let trigger = builder.shutdown_trigger();
    let runtime = builder.build().unwrap();
    assert!(!runtime.health().is_ready());
    assert!(format!("{runtime:?}").contains("api"));
    let running = tokio::spawn(runtime.run());
    trigger.shutdown();
    let report = running.await.unwrap().unwrap();
    assert_eq!(report.reason, ShutdownReason::Requested);
}

#[tokio::test]
async fn dropping_the_handle_requests_shutdown() {
    let (stopped_tx, stopped_rx) = oneshot::channel::<()>();
    let mut stopped = once(stopped_tx);
    let handle = runtime()
        .unit(
            "api",
            Stage::Ingress,
            UnitPolicy::Critical,
            move |ctx: UnitContext| {
                let stopped = stopped();
                async move {
                    until_shutdown(ctx).await?;
                    if let Some(stopped) = stopped {
                        let _ = stopped.send(());
                    }
                    Ok(())
                }
            },
        )
        .build()
        .unwrap()
        .start()
        .await
        .unwrap();
    assert!(format!("{handle:?}").contains("shutting_down: false"));
    drop(handle);
    stopped_rx
        .await
        .expect("drained after the handle was dropped");
}

fn panic_with_literal() -> Result<(), AppError> {
    panic!("index out of bounds")
}

fn panic_with_message() -> Result<(), AppError> {
    panic!("index {} out of bounds", 3)
}

fn panic_with_value() -> Result<(), AppError> {
    std::panic::panic_any(7_u8)
}

#[tokio::test]
async fn a_panicking_critical_unit_fails_the_run() {
    for explode in [panic_with_literal, panic_with_message, panic_with_value] {
        let error = runtime()
            .unit(
                "bug",
                Stage::Workers,
                UnitPolicy::Critical,
                move |_ctx| async move { explode() },
            )
            .build()
            .unwrap()
            .run()
            .await
            .unwrap_err();
        assert_eq!(error.code(), ErrorCode::Internal);
        assert_eq!(error.message(), "unit bug panicked");
    }
}

#[tokio::test]
async fn a_critical_unit_failing_while_stopping_fails_the_run() {
    let handle = runtime()
        .unit(
            "flush",
            Stage::Workers,
            UnitPolicy::Critical,
            |ctx: UnitContext| async move {
                until_shutdown(ctx).await?;
                Err(AppError::new(ErrorCode::DataLoss, "buffer not flushed"))
            },
        )
        .unit(
            "side",
            Stage::Workers,
            UnitPolicy::BestEffort,
            |ctx: UnitContext| async move {
                until_shutdown(ctx).await?;
                Err(AppError::unavailable("ignored while stopping"))
            },
        )
        .build()
        .unwrap()
        .start()
        .await
        .unwrap();
    handle.shutdown();
    let error = handle.wait().await.unwrap_err();
    assert_eq!(error.code(), ErrorCode::DataLoss);
}

struct NamedProbe(&'static str);

impl DependencyProbe for NamedProbe {
    fn name(&self) -> &str {
        self.0
    }
    fn probe(&self) -> futures::future::BoxFuture<'_, ProbeStatus> {
        Box::pin(async { ProbeStatus::Up })
    }
}

#[test]
fn invalid_configurations_are_rejected() {
    let ok = |_ctx: UnitContext| async { Ok::<(), AppError>(()) };
    let cases: Vec<(&str, RuntimeBuilder)> = vec![
        (
            "empty name",
            runtime().unit("", Stage::Workers, UnitPolicy::Critical, ok),
        ),
        (
            "duplicate name",
            runtime()
                .unit("a", Stage::Workers, UnitPolicy::Critical, ok)
                .unit("a", Stage::Ingress, UnitPolicy::Critical, ok),
        ),
        (
            "bad restart policy",
            runtime().unit(
                "a",
                Stage::Workers,
                UnitPolicy::Restart(RestartPolicy {
                    multiplier: 0.0,
                    ..RestartPolicy::default()
                }),
                ok,
            ),
        ),
        (
            "zero start timeout",
            runtime().start_timeout(Duration::ZERO),
        ),
        (
            "zero restart reset period",
            runtime().restart_reset_after(Duration::ZERO),
        ),
        ("empty probe name", runtime().probe(NamedProbe(""))),
        (
            "duplicate probe",
            runtime().probe(NamedProbe("db")).probe(NamedProbe("db")),
        ),
        (
            "zero probe interval",
            runtime()
                .probe(NamedProbe("db"))
                .probe_interval(Duration::ZERO),
        ),
        (
            "zero probe timeout",
            runtime()
                .probe(NamedProbe("db"))
                .probe_timeout(Duration::ZERO),
        ),
        (
            "probe unit name taken",
            runtime().probe(NamedProbe("db")).unit(
                sekvent_runtime::PROBE_UNIT,
                Stage::Workers,
                UnitPolicy::Critical,
                ok,
            ),
        ),
    ];
    for (case, builder) in cases {
        let error = builder.build().expect_err(case);
        assert_eq!(error.code(), ErrorCode::InvalidArgument, "{case}");
    }
    assert!(format!("{:?}", runtime()).contains("RuntimeBuilder"));
}

/// Waits for a guard's sender to be dropped, failing (instead of hanging)
/// if the owning unit is never stopped.
async fn dropped(guard: oneshot::Receiver<()>) -> bool {
    tokio::time::timeout(Duration::from_secs(3_600), guard)
        .await
        .is_ok_and(|received| received.is_err())
}

#[allow(
    clippy::needless_pass_by_value,
    reason = "a factory takes its context by value"
)]
fn exploding_factory(_ctx: UnitContext) -> std::future::Ready<Result<(), AppError>> {
    panic!("factory exploded")
}

#[tokio::test]
async fn a_panicking_factory_fails_a_critical_unit() {
    let later_started = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&later_started);
    let error = runtime()
        .unit(
            "boot",
            Stage::Infrastructure,
            UnitPolicy::Critical,
            exploding_factory,
        )
        .unit(
            "api",
            Stage::Ingress,
            UnitPolicy::Critical,
            move |ctx: UnitContext| {
                flag.store(true, Ordering::SeqCst);
                until_shutdown(ctx)
            },
        )
        .build()
        .unwrap()
        .run()
        .await
        .unwrap_err();
    assert_eq!(error.code(), ErrorCode::Internal);
    assert_eq!(error.message(), "unit boot panicked");
    assert!(!later_started.load(Ordering::SeqCst));
}

#[tokio::test(start_paused = true)]
async fn a_restarting_unit_survives_a_panicking_factory() {
    let handle = runtime()
        .unit(
            "consumer",
            Stage::Workers,
            UnitPolicy::Restart(RestartPolicy::default()),
            |ctx: UnitContext| {
                assert!(ctx.attempt() > 0, "the first build fails");
                until_shutdown(ctx)
            },
        )
        .build()
        .unwrap()
        .start()
        .await
        .unwrap();
    handle.shutdown();
    let report = handle.wait().await.unwrap();
    let unit = report.unit("consumer").unwrap();
    assert_eq!(unit.restarts, 1);
    assert_eq!(unit.exit, UnitExit::Completed);
}

#[tokio::test(start_paused = true)]
async fn a_restarting_unit_holds_its_stage_until_a_run_is_ready() {
    let log: Log = Arc::default();
    let worker_log = Arc::clone(&log);
    let api_log = Arc::clone(&log);
    let handle = runtime()
        .unit(
            "worker",
            Stage::Workers,
            UnitPolicy::Restart(RestartPolicy::default()),
            move |ctx: UnitContext| {
                let log = Arc::clone(&worker_log);
                async move {
                    if ctx.attempt() == 0 {
                        push(&log, "worker failed".into());
                        return Err(AppError::unavailable("broker not up yet"));
                    }
                    push(&log, format!("worker ready on attempt {}", ctx.attempt()));
                    until_shutdown(ctx).await
                }
            },
        )
        .unit(
            "api",
            Stage::Ingress,
            UnitPolicy::Critical,
            move |ctx: UnitContext| {
                push(&api_log, "api started".into());
                until_shutdown(ctx)
            },
        )
        .build()
        .unwrap()
        .start()
        .await
        .unwrap();
    assert_eq!(
        *log.lock().unwrap(),
        ["worker failed", "worker ready on attempt 1", "api started"]
    );
    handle.shutdown();
    let report = handle.wait().await.unwrap();
    assert_eq!(report.unit("worker").unwrap().restarts, 1);
}

#[tokio::test(start_paused = true)]
async fn restarts_exhausted_during_startup_fail_start() {
    let later_started = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&later_started);
    let policy = RestartPolicy {
        max_restarts: Some(2),
        ..RestartPolicy::default()
    };
    let builder = runtime()
        .unit(
            "consumer",
            Stage::Workers,
            UnitPolicy::Restart(policy),
            |_ctx| async { Err(AppError::unavailable("broker down")) },
        )
        .unit(
            "api",
            Stage::Ingress,
            UnitPolicy::Critical,
            move |ctx: UnitContext| {
                flag.store(true, Ordering::SeqCst);
                until_shutdown(ctx)
            },
        );
    let health = builder.health();
    let error = builder.build().unwrap().start().await.unwrap_err();
    assert_eq!(error.code(), ErrorCode::Unavailable);
    assert!(error.message().contains("consumer") && error.message().contains("2 restarts"));
    assert!(!later_started.load(Ordering::SeqCst));
    assert!(!health.is_ready());
}

#[tokio::test(start_paused = true)]
async fn a_restart_loop_during_startup_times_out_naming_the_unit() {
    let error = runtime()
        .start_timeout(Duration::from_secs(5))
        .unit(
            "consumer",
            Stage::Workers,
            UnitPolicy::Restart(RestartPolicy::default()),
            |_ctx| async { Err(AppError::unavailable("broker down")) },
        )
        .unit("api", Stage::Ingress, UnitPolicy::Critical, until_shutdown)
        .build()
        .unwrap()
        .start()
        .await
        .unwrap_err();
    assert_eq!(error.code(), ErrorCode::DeadlineExceeded);
    assert!(error.message().contains("consumer"), "{}", error.message());
}

#[tokio::test(start_paused = true)]
async fn a_healthy_run_resets_the_restart_count() {
    let attempts: Arc<Mutex<Vec<(u32, Instant)>>> = Arc::default();
    let seen = Arc::clone(&attempts);
    let policy = RestartPolicy {
        initial: Duration::from_secs(1),
        max: Duration::from_secs(8),
        multiplier: 2.0,
        max_restarts: Some(1),
    };
    let error = runtime()
        .restart_reset_after(Duration::from_secs(60))
        .unit(
            "consumer",
            Stage::Workers,
            UnitPolicy::Restart(policy),
            move |ctx: UnitContext| {
                seen.lock().unwrap().push((ctx.attempt(), Instant::now()));
                async move {
                    ctx.ready();
                    if ctx.attempt() == 1 {
                        // A long, healthy run before failing again.
                        tokio::time::sleep(Duration::from_secs(60)).await;
                    }
                    Err(AppError::unavailable("connection lost"))
                }
            },
        )
        .build()
        .unwrap()
        .run()
        .await
        .unwrap_err();

    assert!(error.message().contains("consumer") && error.message().contains("1 restarts"));
    let attempts = attempts.lock().unwrap();
    let numbers: Vec<u32> = attempts.iter().map(|(n, _)| *n).collect();
    assert_eq!(numbers, [0, 1, 2], "the healthy run earned a fresh restart");
    assert_eq!(
        attempts[2].1 - attempts[1].1,
        Duration::from_secs(61),
        "and the backoff started over"
    );
}

/// Panics when dropped, standing in for a bug in code the supervisor runs
/// outside the unit's own future.
struct Bomb;

impl Drop for Bomb {
    #[allow(clippy::manual_assert)]
    fn drop(&mut self) {
        if !std::thread::panicking() {
            panic!("destructor exploded");
        }
    }
}

#[tokio::test]
async fn a_panic_in_supervision_fails_a_critical_unit() {
    let bomb = Bomb;
    let handle = runtime()
        .unit(
            "flush",
            Stage::Workers,
            UnitPolicy::Critical,
            move |ctx: UnitContext| {
                let _ = &bomb;
                until_shutdown(ctx)
            },
        )
        .build()
        .unwrap()
        .start()
        .await
        .unwrap();
    handle.shutdown();
    let error = handle.wait().await.unwrap_err();
    assert_eq!(error.code(), ErrorCode::Internal);
    assert_eq!(error.message(), "unit flush ended unexpectedly");
}

#[tokio::test]
async fn a_panic_in_best_effort_supervision_is_only_reported() {
    let bomb = Bomb;
    let handle = runtime()
        .unit(
            "warmup",
            Stage::Workers,
            UnitPolicy::BestEffort,
            move |_ctx| {
                let _ = &bomb;
                async { Ok(()) }
            },
        )
        .unit("api", Stage::Ingress, UnitPolicy::Critical, until_shutdown)
        .build()
        .unwrap()
        .start()
        .await
        .unwrap();
    assert!(!handle.is_shutting_down());
    handle.shutdown();
    let report = handle.wait().await.unwrap();
    assert_eq!(report.reason, ShutdownReason::Requested);
    assert_eq!(
        report.unit("warmup").unwrap().exit,
        UnitExit::Failed("INTERNAL: unit warmup ended unexpectedly".into())
    );
}

#[tokio::test(start_paused = true)]
async fn a_start_that_is_dropped_stops_the_started_units() {
    let (guard_tx, guard_rx) = oneshot::channel::<()>();
    let mut guard = once(guard_tx);
    let (token_tx, token_rx) = oneshot::channel::<CancellationToken>();
    let mut token = once(token_tx);
    let builder = runtime()
        .unit(
            "store",
            Stage::Infrastructure,
            UnitPolicy::Critical,
            move |ctx: UnitContext| {
                let guard = guard();
                let token = token();
                async move {
                    let _guard = guard;
                    if let Some(token) = token {
                        let _ = token.send(ctx.shutdown());
                    }
                    ctx.ready();
                    std::future::pending::<()>().await;
                    Ok(())
                }
            },
        )
        .unit(
            "api",
            Stage::Ingress,
            UnitPolicy::Critical,
            |ctx: UnitContext| async move {
                // Never reports ready, so startup waits here.
                ctx.shutdown().cancelled().await;
                Ok(())
            },
        );
    let trigger = builder.shutdown_trigger();
    let start = builder.build().unwrap().start();
    let outcome = tokio::time::timeout(Duration::from_secs(1), start).await;
    assert!(
        outcome.is_err(),
        "start was still waiting on the ingress stage"
    );

    assert!(trigger.is_triggered());
    assert!(token_rx.await.unwrap().is_cancelled());
    assert!(dropped(guard_rx).await, "the started unit was stopped");
}

#[tokio::test(start_paused = true)]
async fn a_run_that_is_dropped_stops_every_unit() {
    let (guard_tx, guard_rx) = oneshot::channel::<()>();
    let mut guard = once(guard_tx);
    let run = runtime()
        .unit(
            "worker",
            Stage::Workers,
            UnitPolicy::Critical,
            move |ctx: UnitContext| {
                let guard = guard();
                async move {
                    let _guard = guard;
                    ctx.ready();
                    std::future::pending::<()>().await;
                    Ok(())
                }
            },
        )
        .build()
        .unwrap()
        .run();
    assert!(
        tokio::time::timeout(Duration::from_secs(1), run)
            .await
            .is_err()
    );
    assert!(dropped(guard_rx).await, "the unit was stopped");
}

#[tokio::test(start_paused = true)]
async fn a_cancelled_wait_still_requests_shutdown() {
    let (stopped_tx, stopped_rx) = oneshot::channel::<()>();
    let mut stopped = once(stopped_tx);
    let handle = runtime()
        .unit(
            "api",
            Stage::Ingress,
            UnitPolicy::Critical,
            move |ctx: UnitContext| {
                let stopped = stopped();
                async move {
                    until_shutdown(ctx).await?;
                    if let Some(stopped) = stopped {
                        let _ = stopped.send(());
                    }
                    Ok(())
                }
            },
        )
        .build()
        .unwrap()
        .start()
        .await
        .unwrap();
    let trigger = handle.trigger();
    assert!(
        tokio::time::timeout(Duration::from_secs(1), handle.wait())
            .await
            .is_err()
    );
    assert!(trigger.is_triggered());
    stopped_rx
        .await
        .expect("drained after the wait was dropped");
}
