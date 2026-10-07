//! Jobs driven through the public API on tokio's paused clock: cadence,
//! overlaps, failures, jitter, timeouts, drain, triggers, cron and guards.
//!
//! Runs report through channels and times are asserted exactly against
//! `tokio::time::Instant`; wall-clock schedules use `TokioWallClock` (or a
//! `ManualClock` where the test moves the wall clock itself).

use std::collections::VecDeque;
use std::convert::Infallible;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use futures::future::BoxFuture;
use rand::TryRng;
use sekvent_context::ManualClock;
use sekvent_error::{AppError, ErrorCode};
use sekvent_runtime::reasons;
use sekvent_runtime::{
    CancelReason, JobContext, JobGuard, JobPermit, JobSpec, JobState, RunTrigger, Runtime,
    RuntimeBuilder, RuntimeHandle, Stage, TokioWallClock, TriggerErrorKind, UnitExit,
};
use tokio::sync::{Notify, mpsc};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

fn secs(value: u64) -> Duration {
    Duration::from_secs(value)
}

fn epoch(value: u64) -> SystemTime {
    SystemTime::UNIX_EPOCH + secs(value)
}

fn runtime() -> RuntimeBuilder {
    Runtime::builder().without_signals()
}

async fn start(builder: RuntimeBuilder) -> RuntimeHandle {
    builder.build().unwrap().start().await.unwrap()
}

/// What a run reports when it starts.
#[derive(Debug, Clone)]
struct Seen {
    at: Instant,
    trigger: RunTrigger,
    tick: Option<SystemTime>,
    fence: Option<u64>,
    run_id: String,
}

impl Seen {
    fn of(cx: &JobContext) -> Self {
        Self {
            at: Instant::now(),
            trigger: cx.trigger(),
            tick: cx.tick(),
            fence: cx.fence(),
            run_id: cx.run_id().to_owned(),
        }
    }
}

type Runs = mpsc::UnboundedReceiver<Seen>;

/// The next run to start; a day of paused time guards against a hang.
async fn next(runs: &mut Runs) -> Seen {
    tokio::time::timeout(secs(86_400), runs.recv())
        .await
        .expect("a run starts within a day")
        .expect("the job is registered")
}

/// A job that reports its start and returns at once.
fn quick(
    tx: mpsc::UnboundedSender<Seen>,
) -> impl Fn(JobContext) -> BoxFuture<'static, Result<(), AppError>> + Send + Sync + 'static {
    move |cx: JobContext| -> BoxFuture<'static, Result<(), AppError>> {
        let tx = tx.clone();
        Box::pin(async move {
            tx.send(Seen::of(&cx)).unwrap();
            Ok(())
        })
    }
}

/// Reports, when dropped, why the run was cancelled by then.
struct DropProbe {
    cx: JobContext,
    tx: mpsc::UnboundedSender<(Instant, Option<CancelReason>)>,
}

impl Drop for DropProbe {
    fn drop(&mut self) {
        let _ = self.tx.send((Instant::now(), self.cx.cancel_reason()));
    }
}

/// A job that reports its start, then waits for its cancellation (or
/// forever when `stubborn`) with a `DropProbe` in hand.
fn probed(
    tx: mpsc::UnboundedSender<Seen>,
    dropped: mpsc::UnboundedSender<(Instant, Option<CancelReason>)>,
    stubborn: bool,
) -> impl Fn(JobContext) -> BoxFuture<'static, Result<(), AppError>> + Send + Sync + 'static {
    move |cx: JobContext| -> BoxFuture<'static, Result<(), AppError>> {
        let tx = tx.clone();
        let dropped = dropped.clone();
        Box::pin(async move {
            tx.send(Seen::of(&cx)).unwrap();
            let _probe = DropProbe {
                cx: cx.clone(),
                tx: dropped,
            };
            if stubborn {
                std::future::pending::<()>().await;
            }
            cx.cancelled().await;
            Ok(())
        })
    }
}

/// Always returns the same word.
struct FixedRng(u64);

impl TryRng for FixedRng {
    type Error = Infallible;
    fn try_next_u32(&mut self) -> Result<u32, Infallible> {
        Ok(u32::try_from(self.0 >> 32).unwrap_or(u32::MAX))
    }
    fn try_next_u64(&mut self) -> Result<u64, Infallible> {
        Ok(self.0)
    }
    fn try_fill_bytes(&mut self, dst: &mut [u8]) -> Result<(), Infallible> {
        dst.fill(0);
        Ok(())
    }
}

// --- in-process schedules -------------------------------------------------

#[tokio::test(start_paused = true)]
async fn interval_runs_keep_a_fixed_cadence_whatever_they_take() {
    let t0 = Instant::now();
    let (tx, mut runs) = mpsc::unbounded_channel();
    let spec = JobSpec::interval(secs(10));
    let job = spec.handle();
    let handle = start(runtime().job("cadence", Stage::Workers, spec, move |cx| {
        let tx = tx.clone();
        async move {
            tx.send(Seen::of(&cx)).unwrap();
            tokio::time::sleep(secs(3)).await;
            Ok(())
        }
    }))
    .await;
    assert_eq!(job.status().state, JobState::Idle);

    for expected in [10, 20, 30] {
        let seen = next(&mut runs).await;
        assert_eq!(seen.at, t0 + secs(expected));
        assert_eq!(seen.trigger, RunTrigger::Schedule);
        assert!(seen.tick.is_some());
        assert_eq!(seen.fence, None);
    }
    let status = job.status();
    assert_eq!((status.runs, status.failures, status.skipped), (3, 0, 0));
    assert_eq!(status.state, JobState::Running);
    assert_eq!(status.current.unwrap().trigger, RunTrigger::Schedule);
    assert_eq!(status.last.unwrap().error, None);

    handle.shutdown();
    let report = handle.wait().await.unwrap();
    let unit = report.unit("cadence").unwrap();
    assert_eq!(unit.exit, UnitExit::Completed);
    assert_eq!(unit.restarts, 0);
    let status = job.status();
    assert_eq!(status.state, JobState::Stopped);
    assert_eq!(status.current, None);
    assert_eq!(status.next_tick, None);
}

#[tokio::test(start_paused = true)]
async fn a_zero_initial_delay_runs_at_once_and_ticks_read_on_the_wall_clock() {
    let t0 = Instant::now();
    let base = epoch(1_000_000);
    let (tx, mut runs) = mpsc::unbounded_channel();
    let spec = JobSpec::interval(secs(10))
        .initial_delay(Duration::ZERO)
        .clock(Arc::new(TokioWallClock::new(base)));
    let job = spec.handle();
    let handle = start(runtime().job("eager", Stage::Workers, spec, quick(tx))).await;

    let first = next(&mut runs).await;
    assert_eq!(first.at, t0);
    assert_eq!(first.tick, Some(base));
    assert_eq!(job.status().next_tick, Some(base + secs(10)));
    let second = next(&mut runs).await;
    assert_eq!(second.at, t0 + secs(10));
    assert_eq!(second.tick, Some(base + secs(10)));
    assert_ne!(first.run_id, second.run_id);
    assert_eq!(first.run_id.len(), 36, "a UUID");
    handle.shutdown();
    handle.wait().await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn a_long_run_skips_the_ticks_it_overlaps() {
    let t0 = Instant::now();
    let (tx, mut runs) = mpsc::unbounded_channel();
    let spec = JobSpec::interval(secs(10));
    let job = spec.handle();
    let handle = start(runtime().job("slow", Stage::Workers, spec, move |cx| {
        let tx = tx.clone();
        async move {
            tx.send(Seen::of(&cx)).unwrap();
            tokio::time::sleep(secs(25)).await;
            Ok(())
        }
    }))
    .await;

    assert_eq!(next(&mut runs).await.at, t0 + secs(10));
    assert_eq!(next(&mut runs).await.at, t0 + secs(40));
    let status = job.status();
    assert_eq!((status.runs, status.skipped), (2, 2));
    handle.shutdown();
    handle.wait().await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn errors_and_panics_are_recorded_and_the_next_tick_still_runs() {
    let t0 = Instant::now();
    let (tx, mut runs) = mpsc::unbounded_channel();
    let calls = Arc::new(AtomicU32::new(0));
    let spec = JobSpec::interval(secs(10));
    let job = spec.handle();
    let handle = start(runtime().job("flaky", Stage::Workers, spec, move |cx| {
        let call = calls.fetch_add(1, Ordering::SeqCst);
        assert_ne!(call, 2, "the job could not even start");
        let tx = tx.clone();
        async move {
            tx.send(Seen::of(&cx)).unwrap();
            tokio::time::sleep(secs(1)).await;
            match call {
                0 => Err(AppError::unavailable("upstream down").with_reason("UPSTREAM_DOWN")),
                1 => panic!("the job exploded"),
                _ => Ok(()),
            }
        }
    }))
    .await;

    assert_eq!(next(&mut runs).await.at, t0 + secs(10));
    assert_eq!(next(&mut runs).await.at, t0 + secs(20));
    let status = job.status();
    assert_eq!(status.failures, 1);
    assert_eq!(status.last.unwrap().error, Some(ErrorCode::Unavailable));

    // The third run panicked before it was a future, so it never reported.
    assert_eq!(next(&mut runs).await.at, t0 + secs(40));
    let status = job.status();
    assert_eq!((status.runs, status.failures), (4, 3));
    assert_eq!(status.last.unwrap().error, Some(ErrorCode::Internal));

    handle.shutdown();
    let report = handle.wait().await.unwrap();
    let unit = report.unit("flaky").unwrap();
    assert_eq!(unit.restarts, 0);
    assert_eq!(unit.exit, UnitExit::Completed);
}

#[tokio::test(start_paused = true)]
async fn jitter_delays_each_run_without_moving_the_grid() {
    let t0 = Instant::now();
    let base = epoch(1_000_000);
    let (tx, mut runs) = mpsc::unbounded_channel();
    let spec = JobSpec::interval(secs(10))
        .jitter(secs(4))
        .jitter_rng(FixedRng(1 << 63))
        .clock(Arc::new(TokioWallClock::new(base)));
    let handle = start(runtime().job("jittery", Stage::Workers, spec, quick(tx))).await;

    for k in 1..=3 {
        let seen = next(&mut runs).await;
        assert_eq!(seen.at, t0 + secs(10 * k + 2));
        assert_eq!(seen.tick, Some(base + secs(10 * k)));
    }
    handle.shutdown();
    handle.wait().await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn a_timeout_cancels_then_drops_a_run_that_never_ends() {
    let t0 = Instant::now();
    let (tx, mut runs) = mpsc::unbounded_channel();
    let (dropped_tx, mut dropped) = mpsc::unbounded_channel();
    let spec = JobSpec::interval(secs(10)).timeout(secs(5));
    let job = spec.handle();
    let handle = start(runtime().job(
        "endless",
        Stage::Workers,
        spec,
        probed(tx, dropped_tx, true),
    ))
    .await;

    assert_eq!(next(&mut runs).await.at, t0 + secs(10));
    let (at, reason) = dropped.recv().await.unwrap();
    assert_eq!(at, t0 + secs(15));
    assert_eq!(reason, Some(CancelReason::Timeout));

    assert_eq!(next(&mut runs).await.at, t0 + secs(20));
    let status = job.status();
    assert_eq!(status.failures, 1);
    assert_eq!(
        status.last.unwrap().error,
        Some(ErrorCode::DeadlineExceeded)
    );
    handle.shutdown();
    handle.wait().await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn drain_cancels_the_run_which_returns_within_the_grace() {
    let (tx, mut runs) = mpsc::unbounded_channel();
    let (dropped_tx, mut dropped) = mpsc::unbounded_channel();
    let spec = JobSpec::interval(secs(10)).initial_delay(Duration::ZERO);
    let job = spec.handle();
    let handle = start(runtime().job(
        "drained",
        Stage::Workers,
        spec,
        probed(tx, dropped_tx, false),
    ))
    .await;

    let seen = next(&mut runs).await;
    let stopping = Instant::now();
    handle.shutdown();
    let (at, reason) = dropped.recv().await.unwrap();
    assert_eq!((at, reason), (stopping, Some(CancelReason::Shutdown)));
    let report = handle.wait().await.unwrap();
    assert_eq!(report.unit("drained").unwrap().exit, UnitExit::Completed);

    let status = job.status();
    assert_eq!(status.state, JobState::Stopped);
    let last = status.last.unwrap();
    assert_eq!(last.run.run_id, seen.run_id);
    assert_eq!(last.error, None);
    let refused = job.trigger().await.unwrap_err();
    assert_eq!(refused.kind(), TriggerErrorKind::NotRunning);
}

#[tokio::test(start_paused = true)]
async fn a_run_that_ignores_drain_is_aborted_at_the_stop_deadline() {
    let (tx, mut runs) = mpsc::unbounded_channel();
    let (dropped_tx, mut dropped) = mpsc::unbounded_channel();
    let spec = JobSpec::interval(secs(10)).initial_delay(Duration::ZERO);
    let job = spec.handle();
    let handle = start(runtime().stage_grace(secs(2)).job(
        "stubborn",
        Stage::Workers,
        spec,
        probed(tx, dropped_tx, true),
    ))
    .await;

    next(&mut runs).await;
    let stopping = Instant::now();
    handle.shutdown();
    let report = handle.wait().await.unwrap();
    assert_eq!(Instant::now(), stopping + secs(2));
    assert_eq!(report.unit("stubborn").unwrap().exit, UnitExit::Aborted);
    let (_, reason) = dropped.recv().await.unwrap();
    assert_eq!(reason, Some(CancelReason::Shutdown));
    let status = job.status();
    assert_eq!(status.state, JobState::Stopped);
    assert_eq!(status.current, None);
}

#[tokio::test(start_paused = true)]
async fn a_stalled_interval_runs_once_and_counts_the_ticks_it_passed_over() {
    let t0 = Instant::now();
    let base = epoch(1_000_000);
    let (tx, mut runs) = mpsc::unbounded_channel();
    let spec = JobSpec::interval(secs(10)).clock(Arc::new(TokioWallClock::new(base)));
    let job = spec.handle();
    let handle = start(runtime().job("stalled", Stage::Workers, spec, quick(tx))).await;

    tokio::time::advance(secs(100)).await;
    let seen = next(&mut runs).await;
    assert_eq!(seen.at, t0 + secs(100));
    assert_eq!(
        seen.tick,
        Some(base + secs(40)),
        "the oldest tick within the grace"
    );
    let status = job.status();
    assert_eq!(
        status.skipped, 9,
        "+10 misfired, +20 and +30 passed over, +50 to +100 coalesced"
    );
    assert_eq!(status.next_tick, Some(base + secs(110)));
    assert_eq!(next(&mut runs).await.at, t0 + secs(110));
    handle.shutdown();
    handle.wait().await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn a_late_wake_on_a_tick_runs_it_once_not_twice() {
    let t0 = Instant::now();
    let (tx, mut runs) = mpsc::unbounded_channel();
    let spec = JobSpec::interval(secs(10));
    let job = spec.handle();
    let handle = start(runtime().job("boundary", Stage::Workers, spec, quick(tx))).await;

    tokio::time::advance(secs(20)).await;
    assert_eq!(next(&mut runs).await.at, t0 + secs(20));
    assert_eq!(next(&mut runs).await.at, t0 + secs(30));
    assert_eq!(job.status().skipped, 1, "the tick at +20 was coalesced");
    handle.shutdown();
    handle.wait().await.unwrap();
}

// --- triggers ---------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn triggers_start_runs_now_refuse_overlaps_and_need_a_running_unit() {
    let t0 = Instant::now();
    let (tx, mut runs) = mpsc::unbounded_channel();
    let gate = Arc::new(Notify::new());
    let spec = JobSpec::interval(secs(10));
    let job = spec.handle();
    assert_eq!(job.name(), None);

    let opened = Arc::clone(&gate);
    let builder = runtime().job("orders-sync", Stage::Workers, spec, move |cx| {
        let tx = tx.clone();
        let gate = Arc::clone(&opened);
        async move {
            tx.send(Seen::of(&cx)).unwrap();
            tokio::select! {
                () = gate.notified() => {}
                () = cx.cancelled() => {}
            }
            Ok(())
        }
    });
    assert_eq!(job.name().as_deref(), Some("orders-sync"));
    let refused = job.trigger().await.unwrap_err();
    assert_eq!(refused.kind(), TriggerErrorKind::NotRunning);
    assert_eq!(refused.job(), "orders-sync");
    let error = AppError::from(refused);
    assert_eq!(error.code(), ErrorCode::Unavailable);
    assert_eq!(error.reason(), Some(reasons::JOB_NOT_RUNNING));

    let handle = start(builder).await;
    let started = job.trigger().await.unwrap();
    assert_eq!(started.fence, None);
    let seen = next(&mut runs).await;
    assert_eq!(seen.at, t0);
    assert_eq!(seen.trigger, RunTrigger::Manual);
    assert_eq!(seen.tick, None);
    assert_eq!(seen.run_id, started.run_id);

    let busy = job.trigger().await.unwrap_err();
    assert_eq!(busy.kind(), TriggerErrorKind::AlreadyRunning);
    let error = AppError::from(busy);
    assert_eq!(error.code(), ErrorCode::FailedPrecondition);
    assert_eq!(error.reason(), Some(reasons::JOB_ALREADY_RUNNING));
    let status = job.status();
    assert_eq!(status.state, JobState::Running);
    assert_eq!(status.current.unwrap().run_id, started.run_id);

    gate.notify_one();
    let scheduled = next(&mut runs).await;
    assert_eq!(
        scheduled.at,
        t0 + secs(10),
        "a manual run never shifts the grid"
    );
    assert_eq!(scheduled.trigger, RunTrigger::Schedule);

    gate.notify_one();
    job.trigger().await.unwrap();
    let again = next(&mut runs).await;
    assert_eq!(
        (again.at, again.trigger),
        (t0 + secs(10), RunTrigger::Manual)
    );

    handle.shutdown();
    handle.wait().await.unwrap();
    let after = job.trigger().await.unwrap_err();
    assert_eq!(after.kind(), TriggerErrorKind::NotRunning);
}

#[tokio::test(start_paused = true)]
async fn a_manual_job_runs_only_when_triggered() {
    let t0 = Instant::now();
    let (tx, mut runs) = mpsc::unbounded_channel();
    let spec = JobSpec::manual();
    let job = spec.handle();
    let handle = start(runtime().job("on-demand", Stage::Workers, spec, quick(tx))).await;

    tokio::time::sleep(secs(3_600)).await;
    assert!(runs.try_recv().is_err());
    let status = job.status();
    assert_eq!((status.runs, status.next_tick), (0, None));

    job.trigger().await.unwrap();
    let seen = next(&mut runs).await;
    assert_eq!(
        (seen.at, seen.trigger),
        (t0 + secs(3_600), RunTrigger::Manual)
    );
    handle.shutdown();
    handle.wait().await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn the_call_context_carries_the_run_id_cancellation_and_deadline() {
    let t0 = Instant::now();
    let (tx, mut seen) = mpsc::unbounded_channel();
    let spec = JobSpec::interval(secs(10))
        .initial_delay(Duration::ZERO)
        .timeout(secs(30));
    let handle = start(
        runtime().job("contextual", Stage::Workers, spec, move |cx| {
            let tx = tx.clone();
            async move {
                let call = cx.call_context();
                tx.send((
                    call.request_id().to_owned(),
                    cx.run_id().to_owned(),
                    call.deadline(),
                    None,
                ))
                .unwrap();
                call.cancel_token().cancelled().await;
                tx.send((String::new(), String::new(), None, cx.cancel_reason()))
                    .unwrap();
                Ok(())
            }
        }),
    )
    .await;

    let (request_id, run_id, deadline, _) = seen.recv().await.unwrap();
    assert_eq!(request_id, run_id);
    assert_eq!(deadline, Some((t0 + secs(30)).into_std()));
    handle.shutdown();
    let (_, _, _, reason) = seen.recv().await.unwrap();
    assert_eq!(reason, Some(CancelReason::Shutdown));
    handle.wait().await.unwrap();
}

// --- validation -------------------------------------------------------------

#[tokio::test]
async fn every_validation_rule_fails_build_naming_the_job() {
    let guard = || FakeGuard::new(None);
    let cases: Vec<(String, JobSpec)> = vec![
        ("bad name!".into(), JobSpec::interval(secs(10))),
        ("a".repeat(101), JobSpec::interval(secs(10))),
        ("zero-interval".into(), JobSpec::interval(Duration::ZERO)),
        (
            "short-singleton".into(),
            JobSpec::interval(Duration::from_millis(500)).singleton(guard()),
        ),
        (
            "fractional-singleton".into(),
            JobSpec::interval(Duration::from_micros(1_500_500)).singleton(guard()),
        ),
        (
            "wide-jitter".into(),
            JobSpec::interval(secs(10)).jitter(secs(10)),
        ),
        (
            "zero-timeout".into(),
            JobSpec::interval(secs(10)).timeout(Duration::ZERO),
        ),
        (
            "manual-jitter".into(),
            JobSpec::manual().jitter(Duration::ZERO),
        ),
        (
            "manual-delay".into(),
            JobSpec::manual().initial_delay(secs(1)),
        ),
        (
            "manual-grace".into(),
            JobSpec::manual().misfire_grace(secs(1)),
        ),
        (
            "zero-grace".into(),
            JobSpec::interval(secs(10)).misfire_grace(Duration::ZERO),
        ),
        (
            "short-grace".into(),
            JobSpec::interval(secs(10)).misfire_grace(Duration::from_millis(999)),
        ),
    ];
    for (name, spec) in cases {
        let error = runtime()
            .job(name.clone(), Stage::Workers, spec, |_cx| async { Ok(()) })
            .build()
            .unwrap_err();
        assert_eq!(error.code(), ErrorCode::InvalidArgument, "{name}");
        assert!(
            error.message().contains(&name),
            "{name}: {}",
            error.message()
        );
    }

    let empty = runtime()
        .job("", Stage::Workers, JobSpec::manual(), |_cx| async {
            Ok(())
        })
        .build()
        .unwrap_err();
    assert!(
        empty.message().contains("job name \"\""),
        "{}",
        empty.message()
    );

    runtime()
        .job(
            "fine-singleton",
            Stage::Workers,
            JobSpec::interval(Duration::from_millis(1_500)).singleton(guard()),
            |_cx| async { Ok(()) },
        )
        .job(
            "fine-jitter",
            Stage::Workers,
            JobSpec::interval(secs(10))
                .jitter(secs(9))
                .timeout(secs(1))
                .misfire_grace(secs(1)),
            |_cx| async { Ok(()) },
        )
        .build()
        .unwrap();

    let twice = runtime()
        .job("twice", Stage::Workers, JobSpec::manual(), |_cx| async {
            Ok(())
        })
        .job("twice", Stage::Workers, JobSpec::manual(), |_cx| async {
            Ok(())
        })
        .build()
        .unwrap_err();
    assert!(twice.message().contains("twice"));
}

// --- cron -------------------------------------------------------------------

#[cfg(feature = "cron")]
mod cron {
    use sekvent_runtime::Schedule;

    use super::*;

    /// 2026-01-01T00:00:00Z.
    const NEW_YEAR_2026: u64 = 1_767_225_600;

    #[tokio::test(start_paused = true)]
    async fn cron_fires_at_its_occurrences_on_the_wall_clock() {
        for pattern in ["0 */15 * * * *", "*/15 * * * *"] {
            let t0 = Instant::now();
            let (tx, mut runs) = mpsc::unbounded_channel();
            let spec = JobSpec::cron(pattern)
                .unwrap()
                .clock(Arc::new(TokioWallClock::new(epoch(NEW_YEAR_2026 + 7 * 60))));
            assert!(matches!(spec.schedule(), Schedule::Cron(p) if p == pattern));
            let job = spec.handle();
            let handle = start(runtime().job("quarterly", Stage::Workers, spec, quick(tx))).await;
            assert_eq!(job.status().next_tick, Some(epoch(NEW_YEAR_2026 + 15 * 60)));

            for (minutes, tick) in [(8, 15), (23, 30), (38, 45)] {
                let seen = next(&mut runs).await;
                assert_eq!(seen.at, t0 + secs(minutes * 60), "{pattern}");
                assert_eq!(seen.tick, Some(epoch(NEW_YEAR_2026 + tick * 60)));
                assert_eq!(seen.trigger, RunTrigger::Schedule);
            }
            handle.shutdown();
            handle.wait().await.unwrap();
        }
    }

    #[tokio::test(start_paused = true)]
    async fn cron_jitter_longer_than_the_cadence_drops_no_tick() {
        let t0 = Instant::now();
        let (tx, mut runs) = mpsc::unbounded_channel();
        let spec = JobSpec::cron("* * * * *")
            .unwrap()
            .jitter(secs(3_600))
            .jitter_rng(FixedRng(1 << 63))
            .clock(Arc::new(TokioWallClock::new(epoch(NEW_YEAR_2026))));
        let job = spec.handle();
        let handle = start(runtime().job("minutely", Stage::Workers, spec, quick(tx))).await;

        for minute in 0..3 {
            let seen = next(&mut runs).await;
            assert_eq!(seen.at, t0 + secs(30 * 60 + minute * 60));
            assert_eq!(seen.tick, Some(epoch(NEW_YEAR_2026 + minute * 60)));
        }
        assert_eq!(job.status().skipped, 0);
        handle.shutdown();
        handle.wait().await.unwrap();
    }

    #[tokio::test]
    async fn cron_patterns_and_jitter_are_checked() {
        let error = JobSpec::cron("61 * * * *").unwrap_err();
        assert_eq!(error.code(), ErrorCode::InvalidArgument);
        assert!(
            error.message().contains("\"61 * * * *\""),
            "{}",
            error.message()
        );

        let error = runtime()
            .job(
                "late-cron",
                Stage::Workers,
                JobSpec::cron("*/5 * * * *").unwrap().jitter(secs(3_601)),
                |_cx| async { Ok(()) },
            )
            .build()
            .unwrap_err();
        assert!(error.message().contains("late-cron"));

        let error = runtime()
            .job(
                "leap-cron",
                Stage::Workers,
                JobSpec::cron("0 0 30 2 *").unwrap(),
                |_cx| async { Ok(()) },
            )
            .build()
            .unwrap_err();
        assert_eq!(error.code(), ErrorCode::InvalidArgument);
        assert!(
            error.message().contains("leap-cron") && error.message().contains("no occurrence"),
            "{}",
            error.message()
        );

        runtime()
            .job(
                "hourly-cron",
                Stage::Workers,
                JobSpec::cron("0 * * * *").unwrap().jitter(secs(3_600)),
                |_cx| async { Ok(()) },
            )
            .build()
            .unwrap();
    }
}

// --- guards -----------------------------------------------------------------

#[derive(Debug, Clone, Copy)]
enum Answer {
    Held,
    Fail(ErrorCode),
    /// Never answers.
    Hang,
    /// Panics while answering.
    Panic,
    /// Moves the guard's clock to this time, then grants a permit.
    GrantAt(SystemTime),
}

/// What the guard answers to `last_tick`.
#[derive(Debug, Clone, Copy)]
enum LastTick {
    At(Option<SystemTime>),
    Fail,
    Hang,
    Panic,
}

#[derive(Default)]
struct GuardState {
    script: VecDeque<Answer>,
    calls: Vec<(Instant, Option<SystemTime>)>,
    fence: u64,
    lost: Vec<CancellationToken>,
}

/// A guard that answers from a script, then grants permits with growing
/// fences.
#[derive(Clone)]
struct FakeGuard {
    state: Arc<Mutex<GuardState>>,
    last_tick: LastTick,
    released: Arc<AtomicUsize>,
    stuck_release: bool,
    panicking_release: bool,
    clock: Option<ManualClock>,
}

impl FakeGuard {
    fn new(last_tick: Option<SystemTime>) -> Self {
        Self {
            state: Arc::default(),
            last_tick: LastTick::At(last_tick),
            released: Arc::default(),
            stuck_release: false,
            panicking_release: false,
            clock: None,
        }
    }

    fn script(self, answers: impl IntoIterator<Item = Answer>) -> Self {
        self.state.lock().unwrap().script.extend(answers);
        self
    }

    fn answering_last_tick(mut self, answer: LastTick) -> Self {
        self.last_tick = answer;
        self
    }

    fn stuck_release(mut self) -> Self {
        self.stuck_release = true;
        self
    }

    fn panicking_release(mut self) -> Self {
        self.panicking_release = true;
        self
    }

    /// The clock `Answer::GrantAt` moves.
    fn moving(mut self, clock: &ManualClock) -> Self {
        self.clock = Some(clock.clone());
        self
    }

    fn permit(&self) -> JobPermit {
        let mut state = self.state.lock().unwrap();
        state.fence += 1;
        let lost = CancellationToken::new();
        state.lost.push(lost.clone());
        let released = Arc::clone(&self.released);
        let stuck = self.stuck_release;
        let panicking = self.panicking_release;
        JobPermit::new(Some(state.fence), lost, move || {
            assert!(!panicking, "the lease store exploded on release");
            Box::pin(async move {
                if stuck {
                    std::future::pending::<()>().await;
                }
                released.fetch_add(1, Ordering::SeqCst);
            })
        })
    }

    fn calls(&self) -> Vec<(Instant, Option<SystemTime>)> {
        self.state.lock().unwrap().calls.clone()
    }

    /// Withdraw the `index`th permit granted.
    fn lose(&self, index: usize) {
        self.state.lock().unwrap().lost[index].cancel();
    }

    fn released(&self) -> usize {
        self.released.load(Ordering::SeqCst)
    }
}

impl JobGuard for FakeGuard {
    fn acquire<'a>(
        &'a self,
        _job: &'a str,
        tick: Option<SystemTime>,
    ) -> BoxFuture<'a, Result<Option<JobPermit>, AppError>> {
        let answer = {
            let mut state = self.state.lock().unwrap();
            state.calls.push((Instant::now(), tick));
            state.script.pop_front()
        };
        let granted = match answer {
            Some(Answer::Held | Answer::Hang | Answer::Panic) => Ok(None),
            Some(Answer::Fail(code)) => Err(AppError::new(code, "the lease store is down")),
            Some(Answer::GrantAt(at)) => {
                self.clock.as_ref().expect("a clock to move").set(at);
                Ok(Some(self.permit()))
            }
            None => Ok(Some(self.permit())),
        };
        Box::pin(async move {
            match answer {
                Some(Answer::Hang) => std::future::pending().await,
                Some(Answer::Panic) => panic!("the lease store exploded"),
                _ => granted,
            }
        })
    }

    fn last_tick<'a>(
        &'a self,
        _job: &'a str,
    ) -> BoxFuture<'a, Result<Option<SystemTime>, AppError>> {
        let answer = self.last_tick;
        Box::pin(async move {
            match answer {
                LastTick::At(last) => Ok(last),
                LastTick::Fail => Err(AppError::new(
                    ErrorCode::Unavailable,
                    "the lease store is down",
                )),
                LastTick::Hang => std::future::pending().await,
                LastTick::Panic => panic!("the lease store exploded"),
            }
        })
    }
}

/// The wall clock singletons start at: 3 s past a multiple of 10 s.
const START: u64 = 1_000_003;

/// A singleton `interval(10s)` whose first tick is 7 s after the start.
fn singleton(guard: &FakeGuard) -> JobSpec {
    JobSpec::interval(secs(10))
        .singleton(guard.clone())
        .clock(Arc::new(TokioWallClock::new(epoch(START))))
}

fn ticks(guard: &FakeGuard) -> Vec<Option<SystemTime>> {
    guard.calls().into_iter().map(|(_, tick)| tick).collect()
}

#[tokio::test(start_paused = true)]
async fn singleton_ticks_are_epoch_aligned_and_carry_the_fence() {
    let t0 = Instant::now();
    let guard = FakeGuard::new(Some(epoch(1_000_000)));
    let (tx, mut runs) = mpsc::unbounded_channel();
    let spec = singleton(&guard);
    let job = spec.handle();
    let handle = start(runtime().job("ledger", Stage::Workers, spec, quick(tx))).await;
    assert_eq!(job.status().next_tick, Some(epoch(1_000_010)));

    let first = next(&mut runs).await;
    assert_eq!(first.at, t0 + secs(7));
    assert_eq!(first.tick, Some(epoch(1_000_010)));
    assert_eq!(first.fence, Some(1));
    let second = next(&mut runs).await;
    assert_eq!(second.at, t0 + secs(17));
    assert_eq!(second.fence, Some(2));
    assert_eq!(
        guard.calls(),
        vec![
            (t0 + secs(7), Some(epoch(1_000_010))),
            (t0 + secs(17), Some(epoch(1_000_020))),
        ]
    );
    handle.shutdown();
    handle.wait().await.unwrap();
    assert_eq!(guard.released(), 2, "one release per permit");
    assert_eq!(job.status().last.unwrap().run.fence, Some(2));
}

#[tokio::test(start_paused = true)]
async fn a_tick_held_elsewhere_is_skipped() {
    let t0 = Instant::now();
    let guard = FakeGuard::new(Some(epoch(1_000_000))).script([Answer::Held]);
    let (tx, mut runs) = mpsc::unbounded_channel();
    let spec = singleton(&guard);
    let job = spec.handle();
    let handle = start(runtime().job("ledger", Stage::Workers, spec, quick(tx))).await;

    let seen = next(&mut runs).await;
    assert_eq!(seen.at, t0 + secs(17));
    assert_eq!(seen.tick, Some(epoch(1_000_020)));
    assert_eq!(seen.fence, Some(1));
    assert_eq!(job.status().skipped, 1);
    handle.shutdown();
    handle.wait().await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn guard_errors_are_retried_then_the_tick_is_skipped_at_the_grace() {
    let t0 = Instant::now();
    let guard =
        FakeGuard::new(Some(epoch(1_000_000))).script([Answer::Fail(ErrorCode::Unavailable); 5]);
    let (tx, mut runs) = mpsc::unbounded_channel();
    let spec = singleton(&guard).misfire_grace(secs(4));
    let job = spec.handle();
    let handle = start(runtime().job("ledger", Stage::Workers, spec, quick(tx))).await;

    let seen = next(&mut runs).await;
    assert_eq!(seen.at, t0 + secs(17));
    assert_eq!(seen.tick, Some(epoch(1_000_020)));
    let at: Vec<Instant> = guard.calls().into_iter().map(|(at, _)| at).collect();
    let expected: Vec<Instant> = [7, 8, 9, 10, 11, 17]
        .into_iter()
        .map(|offset| t0 + secs(offset))
        .collect();
    assert_eq!(at, expected, "retried every grace / 4 until tick + grace");
    assert_eq!(job.status().skipped, 1);
    handle.shutdown();
    handle.wait().await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn a_lost_guard_cancels_then_drops_the_run() {
    let t0 = Instant::now();
    let guard = FakeGuard::new(Some(epoch(1_000_000)));
    let (tx, mut runs) = mpsc::unbounded_channel();
    let (dropped_tx, mut dropped) = mpsc::unbounded_channel();
    let spec = singleton(&guard);
    let job = spec.handle();
    let handle = start(runtime().job(
        "ledger",
        Stage::Workers,
        spec,
        probed(tx, dropped_tx, false),
    ))
    .await;

    assert_eq!(next(&mut runs).await.at, t0 + secs(7));
    guard.lose(0);
    let (at, reason) = dropped.recv().await.unwrap();
    assert_eq!((at, reason), (t0 + secs(7), Some(CancelReason::LeaseLost)));

    assert_eq!(next(&mut runs).await.at, t0 + secs(17));
    let status = job.status();
    assert_eq!(status.last.unwrap().error, Some(ErrorCode::Aborted));
    assert_eq!(status.failures, 1);
    assert_eq!(guard.released(), 1);
    handle.shutdown();
    handle.wait().await.unwrap();
    assert_eq!(guard.released(), 2);
}

#[tokio::test(start_paused = true)]
async fn a_missed_tick_is_caught_up_once_after_start() {
    for last in [Some(epoch(999_990)), None] {
        let t0 = Instant::now();
        let guard = FakeGuard::new(last);
        let (tx, mut runs) = mpsc::unbounded_channel();
        let handle =
            start(runtime().job("ledger", Stage::Workers, singleton(&guard), quick(tx))).await;

        let caught_up = next(&mut runs).await;
        assert_eq!(caught_up.at, t0);
        assert_eq!(caught_up.trigger, RunTrigger::CatchUp);
        assert_eq!(caught_up.tick, Some(epoch(1_000_000)));
        assert_eq!(caught_up.fence, Some(1));
        let regular = next(&mut runs).await;
        assert_eq!(regular.at, t0 + secs(7));
        assert_eq!(regular.trigger, RunTrigger::Schedule);
        assert_eq!(regular.fence, Some(2));
        assert_eq!(
            ticks(&guard),
            vec![Some(epoch(1_000_000)), Some(epoch(1_000_010))]
        );
        handle.shutdown();
        handle.wait().await.unwrap();
    }
}

#[tokio::test(start_paused = true)]
async fn no_catch_up_past_the_grace_or_without_the_last_tick() {
    let cases = [
        (FakeGuard::new(Some(epoch(990_000))), Some(secs(2))),
        (
            FakeGuard::new(None).answering_last_tick(LastTick::Fail),
            None,
        ),
        (
            FakeGuard::new(None).answering_last_tick(LastTick::Panic),
            None,
        ),
        // Read until the tick passes its grace: 2 s after the start.
        (
            FakeGuard::new(None).answering_last_tick(LastTick::Hang),
            Some(secs(5)),
        ),
    ];
    for (guard, grace) in cases {
        let t0 = Instant::now();
        let (tx, mut runs) = mpsc::unbounded_channel();
        let mut spec = singleton(&guard);
        if let Some(grace) = grace {
            spec = spec.misfire_grace(grace);
        }
        let handle = start(runtime().job("ledger", Stage::Workers, spec, quick(tx))).await;

        let seen = next(&mut runs).await;
        assert_eq!(
            (seen.at, seen.trigger),
            (t0 + secs(7), RunTrigger::Schedule)
        );
        assert_eq!(ticks(&guard), vec![Some(epoch(1_000_010))]);
        handle.shutdown();
        handle.wait().await.unwrap();
    }
}

#[tokio::test(start_paused = true)]
async fn a_wall_clock_jump_misfires_the_tick_and_resumes_within_the_grace() {
    let clock = ManualClock::new(epoch(START));
    let guard = FakeGuard::new(Some(epoch(1_000_000)));
    let (tx, mut runs) = mpsc::unbounded_channel();
    let spec = JobSpec::interval(secs(10))
        .singleton(guard.clone())
        .clock(Arc::new(clock.clone()));
    let job = spec.handle();
    let handle = start(runtime().job("ledger", Stage::Workers, spec, quick(tx))).await;

    clock.set(epoch(1_000_105));
    let seen = next(&mut runs).await;
    assert_eq!(
        seen.tick,
        Some(epoch(1_000_050)),
        "the first tick within the grace"
    );
    assert_eq!(seen.trigger, RunTrigger::Schedule);
    assert_eq!(
        job.status().skipped,
        9,
        "1_000_010 misfired, 20 to 40 passed over, 60 to 100 coalesced"
    );
    assert_eq!(job.status().next_tick, Some(epoch(1_000_110)));
    assert_eq!(ticks(&guard), vec![Some(epoch(1_000_050))]);
    handle.shutdown();
    handle.wait().await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn a_release_that_hangs_is_abandoned_after_five_seconds() {
    let t0 = Instant::now();
    let guard = FakeGuard::new(Some(epoch(1_000_000))).stuck_release();
    let (tx, mut runs) = mpsc::unbounded_channel();
    let spec = singleton(&guard);
    let job = spec.handle();
    let handle = start(runtime().job("ledger", Stage::Workers, spec, quick(tx))).await;

    assert_eq!(next(&mut runs).await.at, t0 + secs(7));
    let second = next(&mut runs).await;
    assert_eq!(
        second.at,
        t0 + secs(17),
        "the release ended before the next tick"
    );
    let status = job.status();
    assert_eq!((status.skipped, status.failures), (0, 0));
    assert_eq!(guard.released(), 0);
    handle.shutdown();
    handle.wait().await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn singleton_triggers_report_why_the_guard_refused() {
    let guard = FakeGuard::new(None).script([
        Answer::Held,
        Answer::Fail(ErrorCode::Unavailable),
        Answer::Panic,
    ]);
    let (tx, mut runs) = mpsc::unbounded_channel();
    let gate = Arc::new(Notify::new());
    let spec = JobSpec::manual().singleton(guard.clone());
    let job = spec.handle();
    let opened = Arc::clone(&gate);
    let handle = start(
        runtime().job("ledger-sync", Stage::Workers, spec, move |cx| {
            let tx = tx.clone();
            let gate = Arc::clone(&opened);
            async move {
                tx.send(Seen::of(&cx)).unwrap();
                gate.notified().await;
                Ok(())
            }
        }),
    )
    .await;

    let held = job.trigger().await.unwrap_err();
    assert_eq!(held.kind(), TriggerErrorKind::HeldElsewhere);
    let error = AppError::from(held);
    assert_eq!(error.code(), ErrorCode::FailedPrecondition);
    assert_eq!(error.reason(), Some(reasons::JOB_HELD_ELSEWHERE));

    let failed = job.trigger().await.unwrap_err();
    assert_eq!(
        failed.kind(),
        TriggerErrorKind::GuardFailed(ErrorCode::Unavailable)
    );
    let error = AppError::from(failed);
    assert_eq!(error.code(), ErrorCode::Unavailable);
    assert_eq!(error.reason(), Some(reasons::JOB_GUARD_FAILED));
    assert_eq!(
        error.metadata().get("guard_code").map(String::as_str),
        Some("UNAVAILABLE")
    );

    let panicked = job.trigger().await.unwrap_err();
    assert_eq!(
        panicked.kind(),
        TriggerErrorKind::GuardFailed(ErrorCode::Internal)
    );

    let started = job.trigger().await.unwrap();
    assert_eq!(started.fence, Some(1));
    let seen = next(&mut runs).await;
    assert_eq!((seen.trigger, seen.fence), (RunTrigger::Manual, Some(1)));
    let busy = job.trigger().await.unwrap_err();
    assert_eq!(busy.kind(), TriggerErrorKind::AlreadyRunning);
    assert_eq!(
        ticks(&guard),
        vec![None, None, None, None],
        "manual runs pass no tick"
    );

    gate.notify_one();
    handle.shutdown();
    handle.wait().await.unwrap();
    assert_eq!(guard.released(), 1);
}

#[tokio::test(start_paused = true)]
async fn a_guard_that_never_answers_a_tick_is_given_up_at_the_grace() {
    let t0 = Instant::now();
    let guard = FakeGuard::new(Some(epoch(1_000_000))).script([Answer::Hang]);
    let (tx, mut runs) = mpsc::unbounded_channel();
    let spec = singleton(&guard).misfire_grace(secs(4));
    let job = spec.handle();
    let handle = start(runtime().job("ledger", Stage::Workers, spec, quick(tx))).await;

    let seen = next(&mut runs).await;
    assert_eq!(seen.at, t0 + secs(17), "the next tick is no overlap");
    assert_eq!(seen.tick, Some(epoch(1_000_020)));
    assert_eq!(seen.fence, Some(1));
    let at: Vec<Instant> = guard.calls().into_iter().map(|(at, _)| at).collect();
    assert_eq!(at, vec![t0 + secs(7), t0 + secs(17)]);
    assert_eq!(job.status().skipped, 1);
    handle.shutdown();
    handle.wait().await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn a_catch_up_whose_guard_never_answers_is_given_up_at_the_grace() {
    let t0 = Instant::now();
    let guard = FakeGuard::new(Some(epoch(999_990))).script([Answer::Hang]);
    let (tx, mut runs) = mpsc::unbounded_channel();
    let spec = singleton(&guard).misfire_grace(secs(5));
    let job = spec.handle();
    let handle = start(runtime().job("ledger", Stage::Workers, spec, quick(tx))).await;

    let seen = next(&mut runs).await;
    assert_eq!(
        (seen.at, seen.trigger),
        (t0 + secs(7), RunTrigger::Schedule)
    );
    assert_eq!(
        ticks(&guard),
        vec![Some(epoch(1_000_000)), Some(epoch(1_000_010))]
    );
    assert_eq!(job.status().skipped, 1);
    handle.shutdown();
    handle.wait().await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn a_permit_granted_after_the_grace_is_given_back_and_the_tick_skipped() {
    let t0 = Instant::now();
    let clock = ManualClock::new(epoch(1_000_010));
    let guard = FakeGuard::new(Some(epoch(1_000_000)))
        .moving(&clock)
        .script([Answer::GrantAt(epoch(1_000_075))]);
    let (tx, mut runs) = mpsc::unbounded_channel();
    let (dropped_tx, _dropped) = mpsc::unbounded_channel();
    let spec = JobSpec::interval(secs(10))
        .singleton(guard.clone())
        .clock(Arc::new(clock.clone()));
    let job = spec.handle();
    let handle = start(runtime().job(
        "ledger",
        Stage::Workers,
        spec,
        probed(tx, dropped_tx, false),
    ))
    .await;

    let seen = next(&mut runs).await;
    assert_eq!(seen.at, t0);
    assert_eq!(seen.tick, Some(epoch(1_000_020)));
    assert_eq!(seen.fence, Some(2));
    assert_eq!(guard.released(), 1, "the late permit was given back");
    assert_eq!(
        ticks(&guard),
        vec![Some(epoch(1_000_010)), Some(epoch(1_000_020))]
    );
    assert_eq!(
        job.status().skipped,
        6,
        "the late tick, then 1_000_030 to 70 coalesced"
    );
    handle.shutdown();
    handle.wait().await.unwrap();
    assert_eq!(guard.released(), 2);
}

#[tokio::test(start_paused = true)]
async fn a_panicking_guard_counts_as_a_failing_guard() {
    let t0 = Instant::now();
    let guard = FakeGuard::new(Some(epoch(1_000_000)))
        .script([Answer::Panic])
        .panicking_release();
    let (tx, mut runs) = mpsc::unbounded_channel();
    let spec = singleton(&guard);
    let job = spec.handle();
    let handle = start(runtime().job("ledger", Stage::Workers, spec, quick(tx))).await;

    let first = next(&mut runs).await;
    assert_eq!(
        first.at,
        t0 + secs(12),
        "retried after grace / 4, capped at 5 s"
    );
    assert_eq!(first.tick, Some(epoch(1_000_010)));
    assert_eq!(first.fence, Some(1));
    let second = next(&mut runs).await;
    assert_eq!((second.at, second.fence), (t0 + secs(17), Some(2)));
    let status = job.status();
    assert_eq!((status.runs, status.failures, status.skipped), (2, 0, 0));
    assert_eq!(guard.released(), 0, "every release panicked");

    handle.shutdown();
    let report = handle.wait().await.unwrap();
    assert_eq!(report.unit("ledger").unwrap().exit, UnitExit::Completed);
}

#[tokio::test(start_paused = true)]
async fn a_trigger_whose_guard_never_answers_fails_after_five_seconds() {
    let t0 = Instant::now();
    let guard = FakeGuard::new(None).script([Answer::Hang]);
    let (tx, mut runs) = mpsc::unbounded_channel();
    let spec = JobSpec::manual().singleton(guard.clone());
    let job = spec.handle();
    let handle = start(runtime().job("ledger-sync", Stage::Workers, spec, quick(tx))).await;

    let refused = job.trigger().await.unwrap_err();
    assert_eq!(
        refused.kind(),
        TriggerErrorKind::GuardFailed(ErrorCode::DeadlineExceeded)
    );
    assert_eq!(Instant::now(), t0 + secs(5));

    let started = job.trigger().await.unwrap();
    assert_eq!(started.fence, Some(1));
    assert_eq!(next(&mut runs).await.run_id, started.run_id);
    handle.shutdown();
    handle.wait().await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn triggers_waiting_on_a_full_queue_answer_not_running_at_shutdown() {
    let guard = FakeGuard::new(None).script([Answer::Hang]);
    let (tx, _runs) = mpsc::unbounded_channel();
    let spec = JobSpec::manual().singleton(guard.clone());
    let job = spec.handle();
    let handle = start(runtime().job("ledger-sync", Stage::Workers, spec, quick(tx))).await;

    // The guard holds the first trigger, 16 fill the queue, the rest wait
    // to be queued.
    let mut waiting = Vec::new();
    for _ in 0..20 {
        let job = job.clone();
        waiting.push(tokio::spawn(async move { job.trigger().await }));
    }
    tokio::time::sleep(Duration::from_millis(1)).await;
    assert_eq!(guard.calls().len(), 1);

    handle.shutdown();
    handle.wait().await.unwrap();
    for trigger in waiting {
        let answer = tokio::time::timeout(secs(30), trigger)
            .await
            .expect("no trigger hangs")
            .unwrap();
        assert_eq!(answer.unwrap_err().kind(), TriggerErrorKind::NotRunning);
    }
}
