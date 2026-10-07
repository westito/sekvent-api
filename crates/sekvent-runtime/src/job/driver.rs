//! The job unit: one task that owns the schedule, the guard calls, the run
//! in progress and the triggers.

use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use futures::FutureExt;
use futures::future::BoxFuture;
use sekvent_context::{CallContext, Clock};
use sekvent_error::{AppError, ErrorCode};
use tokio::sync::{mpsc, oneshot};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use tracing::Instrument;

use super::guard::Release;
use super::schedule::{self, AtTick, PassedOver, WallGrid};
use super::{
    CancelReason, JobContext, JobGuard, JobPermit, JobRun, JobRunOutcome, JobSpec, JobState,
    Request, RunCancel, RunFn, RunInfo, RunStarted, RunTrigger, Schedule, Shared, TriggerError,
    TriggerErrorKind,
};
use crate::UnitContext;
use crate::reasons;
use crate::runtime::panic_detail;

/// Triggers that may wait for the unit at once.
const REQUEST_QUEUE: usize = 16;

/// The ticks still to come.
enum Ticks {
    /// In process: `first + index × period` on tokio's clock.
    Local {
        first: Instant,
        period: Duration,
        index: u64,
    },
    /// On the wall clock: the next tick of `grid`.
    Wall { grid: WallGrid, at: SystemTime },
}

/// The next tick and the jitter drawn for it.
struct Pending {
    ticks: Ticks,
    jitter: Duration,
}

/// What a guard call in progress is for.
enum StartKind {
    Scheduled(SystemTime),
    CatchUp(SystemTime),
    Manual(oneshot::Sender<Result<RunStarted, TriggerError>>),
}

/// What a guard call in progress came back with.
enum Acquired {
    Permit(JobPermit),
    Held,
    Failed(ErrorCode),
    /// The permit came after the misfire grace and was given back.
    Late,
    /// No catch-up was due.
    Nothing,
}

struct Starting {
    kind: StartKind,
    future: BoxFuture<'static, Acquired>,
}

struct Running {
    cancel: RunCancel,
    future: BoxFuture<'static, JobRunOutcome>,
}

/// How a run that did not return `Ok` ended.
enum Ended {
    Failed(AppError),
    Panicked(String),
}

/// Drives one job inside its unit.
pub(super) struct Driver {
    name: Arc<str>,
    run: RunFn,
    shared: Arc<Shared>,
    schedule: Schedule,
    #[cfg(feature = "cron")]
    cron: Option<Arc<croner::Cron>>,
    jitter: Option<Duration>,
    rng: Box<dyn FnMut() -> u64 + Send>,
    initial_delay: Option<Duration>,
    timeout: Option<Duration>,
    grace: Duration,
    guard: Option<Arc<dyn JobGuard>>,
    clock: Arc<dyn Clock>,
    pending: Option<Pending>,
    starting: Option<Starting>,
    running: Option<Running>,
}

/// Marks the job stopped however the unit ends, an abort included.
struct MarkStopped(Arc<Shared>);

impl Drop for MarkStopped {
    fn drop(&mut self) {
        self.0.close();
        self.0.update(|status| {
            status.state = JobState::Stopped;
            status.current = None;
            status.next_tick = None;
        });
    }
}

/// `at + by`, capped a century ahead.
fn later(at: Instant, by: Duration) -> Instant {
    schedule::local_tick(at, by, 1)
}

fn millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

impl Driver {
    pub(super) fn new(name: Arc<str>, spec: JobSpec, run: RunFn) -> Self {
        let grace = spec.grace();
        Self {
            name,
            run,
            shared: spec.shared,
            schedule: spec.schedule,
            #[cfg(feature = "cron")]
            cron: spec.cron,
            jitter: spec.jitter,
            rng: spec.rng,
            initial_delay: spec.initial_delay,
            timeout: spec.timeout,
            grace,
            guard: spec.guard,
            clock: spec.clock,
            pending: None,
            starting: None,
            running: None,
        }
    }

    /// The unit: ready at once, then ticks, guard answers, runs and
    /// triggers until the stage drains; a run in progress then gets until
    /// the runtime aborts the unit.
    pub(super) async fn run(mut self, unit: UnitContext) {
        let _stopped = MarkStopped(Arc::clone(&self.shared));
        let (sender, mut requests) = mpsc::channel(REQUEST_QUEUE);
        self.begin();
        self.publish_next_tick();
        self.shared.open(sender);
        unit.ready();
        let shutdown = unit.shutdown();
        loop {
            let wake = self.wake();
            let accepting = self.starting.is_none();
            tokio::select! {
                biased;
                () = shutdown.cancelled() => break,
                outcome = next_outcome(&mut self.running) => {
                    self.running = None;
                    self.shared.finished(outcome);
                }
                acquired = next_acquired(&mut self.starting) => {
                    if let Some(starting) = self.starting.take() {
                        self.acquired(starting.kind, acquired);
                    }
                }
                () = sleep_until(wake) => self.on_wake(),
                Some(request) = requests.recv(), if accepting => self.on_trigger(request),
            }
            self.publish_next_tick();
        }

        // Triggers still queued are dropped and answer `NotRunning`.
        self.shared.close();
        drop(requests);
        self.starting = None;
        if let Some(running) = self.running.take() {
            running.cancel.cancel(CancelReason::Shutdown);
            let outcome = running.future.await;
            self.shared.finished(outcome);
        }
    }

    /// Lay out the first tick and, for a guarded job, the catch-up check.
    fn begin(&mut self) {
        let ticks = match (&self.schedule, &self.guard) {
            (Schedule::Interval(period), None) => Some(Ticks::Local {
                first: later(Instant::now(), self.initial_delay.unwrap_or(*period)),
                period: *period,
                index: 0,
            }),
            (Schedule::Interval(period), Some(_)) => self.first_wall(WallGrid::Epoch(*period)),
            #[cfg(feature = "cron")]
            (Schedule::Cron(_), _) => self
                .cron
                .clone()
                .and_then(|cron| self.first_wall(WallGrid::Cron(cron))),
            (Schedule::Manual, _) => None,
        };
        if let (Some(guard), Some(Ticks::Wall { grid, at })) = (&self.guard, &ticks)
            && let Some(prev) = grid.at_or_before(self.clock.now()).filter(|prev| prev < at)
        {
            self.starting = Some(Starting {
                kind: StartKind::CatchUp(prev),
                future: catch_up(
                    Arc::clone(guard),
                    Arc::clone(&self.name),
                    prev,
                    Arc::clone(&self.clock),
                    self.grace,
                ),
            });
        }
        self.pending = ticks.map(|ticks| self.pend(ticks));
    }

    /// The first wall-clock tick at or after `now + initial_delay`.
    fn first_wall(&self, grid: WallGrid) -> Option<Ticks> {
        let anchor = self
            .clock
            .now()
            .checked_add(self.initial_delay.unwrap_or_default())?;
        let at = grid.at_or_after(anchor)?;
        Some(Ticks::Wall { grid, at })
    }

    /// Make `ticks` the next one, drawing its jitter.
    fn pend(&mut self, ticks: Ticks) -> Pending {
        let jitter = self.jitter.map_or(Duration::ZERO, |max| {
            schedule::jitter_from(max, (self.rng)())
        });
        Pending { ticks, jitter }
    }

    /// The wall-clock time of a tokio instant.
    fn wall_of(&self, at: Instant) -> SystemTime {
        let now = Instant::now();
        let wall = self.clock.now();
        if at >= now {
            wall.checked_add(at - now)
        } else {
            wall.checked_sub(now - at)
        }
        .unwrap_or(wall)
    }

    fn publish_next_tick(&self) {
        let next = self.pending.as_ref().map(|pending| match &pending.ticks {
            Ticks::Local {
                first,
                period,
                index,
            } => self.wall_of(schedule::local_tick(*first, *period, *index)),
            Ticks::Wall { at, .. } => *at,
        });
        self.shared.update(|status| status.next_tick = next);
    }

    /// When to look at the next tick: its jittered time on tokio's clock,
    /// or at most a minute ahead for wall-clock ticks.
    fn wake(&self) -> Option<Instant> {
        let pending = self.pending.as_ref()?;
        Some(match &pending.ticks {
            Ticks::Local {
                first,
                period,
                index,
            } => later(
                schedule::local_tick(*first, *period, *index),
                pending.jitter,
            ),
            Ticks::Wall { at, .. } => {
                let fire = at.checked_add(pending.jitter).unwrap_or(*at);
                let wait = schedule::wall_wait(fire, self.clock.now()).unwrap_or_default();
                later(Instant::now(), wait)
            }
        })
    }

    /// The tick's wall time and lateness once it is due; `None` for a
    /// wall-clock tick still ahead (the wait was capped, or the clock moved).
    fn due(&self, pending: &Pending) -> Option<(SystemTime, Duration)> {
        match &pending.ticks {
            Ticks::Local {
                first,
                period,
                index,
            } => {
                let at = schedule::local_tick(*first, *period, *index);
                let fire = later(at, pending.jitter);
                Some((
                    self.wall_of(at),
                    Instant::now().saturating_duration_since(fire),
                ))
            }
            Ticks::Wall { at, .. } => {
                let fire = at.checked_add(pending.jitter).unwrap_or(*at);
                let lateness = self.clock.now().duration_since(fire).ok()?;
                Some((*at, lateness))
            }
        }
    }

    fn on_wake(&mut self) {
        if let Some(pending) = self.pending.take() {
            match self.due(&pending) {
                Some((tick, lateness)) => {
                    let decision = self.at_tick(tick, pending.jitter, lateness);
                    let coalesce = decision != AtTick::Misfire;
                    let next = self.advance(pending.ticks, pending.jitter, coalesce);
                    self.pending = next.map(|ticks| self.pend(ticks));
                }
                None => self.pending = Some(pending),
            }
        }
    }

    /// The tick after `ticks`, whose run was delayed by `jitter`.
    ///
    /// After a misfire, ticks already later than the misfire grace are
    /// passed over (a tick exactly at the grace still runs). Once a tick was
    /// handled on time (`coalesce`), every tick up to now less `jitter` is
    /// passed over too, so a clock jump or a stalled process yields one run
    /// rather than a burst. Passed-over ticks count as skipped.
    fn advance(&self, ticks: Ticks, jitter: Duration, coalesce: bool) -> Option<Ticks> {
        let lag = if coalesce { jitter } else { self.grace };
        match ticks {
            Ticks::Local {
                first,
                period,
                index,
            } => {
                let floor = Instant::now().checked_sub(lag).map_or(0, |floor| {
                    if coalesce {
                        schedule::local_index_after(first, period, floor)
                    } else {
                        schedule::local_index_at_or_after(first, period, floor)
                    }
                });
                let next = index.saturating_add(1).max(floor);
                let count = next.saturating_sub(index).saturating_sub(1);
                if count > 0 {
                    let tick = |index| self.wall_of(schedule::local_tick(first, period, index));
                    self.passed_over(PassedOver {
                        count,
                        first: tick(index.saturating_add(1)),
                        last: tick(next.saturating_sub(1)),
                    });
                }
                Some(Ticks::Local {
                    first,
                    period,
                    index: next,
                })
            }
            Ticks::Wall { grid, at } => {
                let floor = self
                    .clock
                    .now()
                    .checked_sub(lag)
                    .unwrap_or(SystemTime::UNIX_EPOCH);
                let next = if coalesce {
                    grid.after(at.max(floor))?
                } else {
                    grid.next(at, floor)?
                };
                if let Some(passed) = grid.between(at, next) {
                    self.passed_over(passed);
                }
                Some(Ticks::Wall { grid, at: next })
            }
        }
    }

    fn passed_over(&self, passed: PassedOver) {
        self.shared.skipped(passed.count);
        tracing::info!(
            job = &*self.name,
            passed_over = passed.count,
            first_tick_ms = schedule::unix_millis(passed.first),
            last_tick_ms = schedule::unix_millis(passed.last),
            "ticks passed over: due while the job was late or the clock jumped"
        );
    }

    fn at_tick(&mut self, tick: SystemTime, jitter: Duration, lateness: Duration) -> AtTick {
        let busy = self.running.is_some() || self.starting.is_some();
        let job = Arc::clone(&self.name);
        let tick_ms = schedule::unix_millis(tick);
        let decision = schedule::at_tick(busy, lateness, self.grace);
        match decision {
            AtTick::Overlap => {
                self.shared.skipped(1);
                tracing::debug!(
                    job = &*job,
                    tick_ms,
                    "tick skipped: a run of the job is in progress"
                );
            }
            AtTick::Misfire => {
                self.shared.skipped(1);
                tracing::info!(
                    job = &*job,
                    tick_ms,
                    lateness_ms = millis(lateness),
                    "tick skipped: later than the misfire grace"
                );
            }
            AtTick::Run => match self.guard.clone() {
                None => {
                    self.start_run(RunTrigger::Schedule, Some(tick), None);
                }
                Some(guard) => {
                    let future = acquire_tick(
                        guard,
                        job,
                        Arc::clone(&self.clock),
                        tick,
                        tick.checked_add(jitter).unwrap_or(tick),
                        self.grace,
                    );
                    self.starting = Some(Starting {
                        kind: StartKind::Scheduled(tick),
                        future,
                    });
                }
            },
        }
        decision
    }

    fn trigger_error(&self, kind: TriggerErrorKind) -> TriggerError {
        TriggerError {
            job: self.name.to_string(),
            kind,
        }
    }

    fn on_trigger(&mut self, request: Request) {
        if self.running.is_some() {
            let refused = self.trigger_error(TriggerErrorKind::AlreadyRunning);
            let _ = request.reply.send(Err(refused));
            return;
        }
        match self.guard.clone() {
            None => {
                let started = self.start_run(RunTrigger::Manual, None, None);
                let _ = request.reply.send(Ok(started));
            }
            Some(guard) => {
                self.starting = Some(Starting {
                    kind: StartKind::Manual(request.reply),
                    future: acquire_manual(guard, Arc::clone(&self.name)),
                });
            }
        }
    }

    fn acquired(&mut self, kind: StartKind, acquired: Acquired) {
        match (kind, acquired) {
            (StartKind::Manual(reply), Acquired::Permit(permit)) => {
                let started = self.start_run(RunTrigger::Manual, None, Some(permit));
                let _ = reply.send(Ok(started));
            }
            (StartKind::Manual(reply), Acquired::Held) => {
                let _ = reply.send(Err(self.trigger_error(TriggerErrorKind::HeldElsewhere)));
            }
            (StartKind::Manual(reply), Acquired::Failed(code)) => {
                let _ = reply.send(Err(self.trigger_error(TriggerErrorKind::GuardFailed(code))));
            }
            (StartKind::Scheduled(tick), Acquired::Permit(permit)) => {
                self.start_run(RunTrigger::Schedule, Some(tick), Some(permit));
            }
            (StartKind::CatchUp(tick), Acquired::Permit(permit)) => {
                self.start_run(RunTrigger::CatchUp, Some(tick), Some(permit));
            }
            (StartKind::Scheduled(tick) | StartKind::CatchUp(tick), Acquired::Held) => {
                self.shared.skipped(1);
                tracing::debug!(
                    job = &*self.name,
                    tick_ms = schedule::unix_millis(tick),
                    "tick skipped: held by another instance or already run"
                );
            }
            (StartKind::Scheduled(tick) | StartKind::CatchUp(tick), Acquired::Failed(code)) => {
                self.shared.skipped(1);
                tracing::warn!(
                    job = &*self.name,
                    tick_ms = schedule::unix_millis(tick),
                    code = code.as_str(),
                    "tick skipped: the job's guard kept failing"
                );
            }
            (StartKind::Scheduled(tick) | StartKind::CatchUp(tick), Acquired::Late) => {
                self.shared.skipped(1);
                tracing::info!(
                    job = &*self.name,
                    tick_ms = schedule::unix_millis(tick),
                    "tick skipped: the guard answered after the misfire grace"
                );
            }
            (_, Acquired::Nothing | Acquired::Late) => {}
        }
    }

    fn start_run(
        &mut self,
        trigger: RunTrigger,
        tick: Option<SystemTime>,
        permit: Option<JobPermit>,
    ) -> RunStarted {
        let run_id = CallContext::new().request_id().to_owned();
        let fence = permit.as_ref().and_then(JobPermit::fence);
        let cancel = RunCancel::new();
        let cx = JobContext {
            inner: Arc::new(RunInfo {
                job: Arc::clone(&self.name),
                run_id: run_id.clone(),
                trigger,
                tick,
                fence,
                cancel: cancel.clone(),
                deadline: self.timeout.map(|limit| later(Instant::now(), limit)),
            }),
        };
        let run = JobRun {
            run_id: run_id.clone(),
            trigger,
            tick,
            started_at: self.clock.now(),
            fence,
        };
        self.shared.started(run.clone());
        let future = execute(
            Arc::clone(&self.run),
            cx,
            permit,
            Arc::clone(&self.clock),
            run,
        );
        self.running = Some(Running { cancel, future });
        RunStarted { run_id, fence }
    }
}

async fn next_outcome(running: &mut Option<Running>) -> JobRunOutcome {
    match running {
        Some(running) => (&mut running.future).await,
        None => std::future::pending().await,
    }
}

async fn next_acquired(starting: &mut Option<Starting>) -> Acquired {
    match starting {
        Some(starting) => (&mut starting.future).await,
        None => std::future::pending().await,
    }
}

async fn sleep_until(wake: Option<Instant>) {
    match wake {
        Some(at) => tokio::time::sleep_until(at).await,
        None => std::future::pending().await,
    }
}

/// Ask the guard for a scheduled or catch-up `tick`, due at `fire`, until
/// `fire + grace`: failures are retried every `guard_retry(grace)`, an
/// attempt still unanswered then fails with `DEADLINE_EXCEEDED`, and a
/// permit that comes back later than that is given back.
fn acquire_tick(
    guard: Arc<dyn JobGuard>,
    job: Arc<str>,
    clock: Arc<dyn Clock>,
    tick: SystemTime,
    fire: SystemTime,
    grace: Duration,
) -> BoxFuture<'static, Acquired> {
    Box::pin(async move {
        let lateness = || clock.now().duration_since(fire).unwrap_or_default();
        let end = later(Instant::now(), grace.saturating_sub(lateness()));
        let retry = schedule::guard_retry(grace);
        loop {
            let attempt = guard_call(&job, || guard.acquire(&job, Some(tick)));
            let code = match tokio::time::timeout_at(end, attempt).await {
                Ok(Ok(Some(permit))) if lateness() > grace => {
                    let (_, release) = permit.into_parts();
                    release_permit(&job, release).await;
                    return Acquired::Late;
                }
                Ok(Ok(Some(permit))) => return Acquired::Permit(permit),
                Ok(Ok(None)) => return Acquired::Held,
                Ok(Err(error)) => error.code(),
                Err(_) => return Acquired::Failed(ErrorCode::DeadlineExceeded),
            };
            let next = later(Instant::now(), retry);
            if next > end {
                return Acquired::Failed(code);
            }
            tracing::debug!(
                job = &*job,
                code = code.as_str(),
                "the job's guard failed; retrying"
            );
            tokio::time::sleep_until(next).await;
        }
    })
}

/// Ask the guard once, for a manual run, waiting at most
/// `TRIGGER_ACQUIRE_WAIT` (5 s); an attempt still unanswered then fails
/// with `DEADLINE_EXCEEDED`.
fn acquire_manual(guard: Arc<dyn JobGuard>, job: Arc<str>) -> BoxFuture<'static, Acquired> {
    Box::pin(async move {
        let attempt = guard_call(&job, || guard.acquire(&job, None));
        let code = match tokio::time::timeout(schedule::TRIGGER_ACQUIRE_WAIT, attempt).await {
            Ok(Ok(Some(permit))) => return Acquired::Permit(permit),
            Ok(Ok(None)) => return Acquired::Held,
            Ok(Err(error)) => error.code(),
            Err(_) => ErrorCode::DeadlineExceeded,
        };
        tracing::warn!(
            job = &*job,
            code = code.as_str(),
            "the job's guard failed on a trigger"
        );
        Acquired::Failed(code)
    })
}

/// Run `prev` once if the guard's last tick is older and it is still
/// within the grace; the guard's answers are awaited until `prev + grace`.
fn catch_up(
    guard: Arc<dyn JobGuard>,
    job: Arc<str>,
    prev: SystemTime,
    clock: Arc<dyn Clock>,
    grace: Duration,
) -> BoxFuture<'static, Acquired> {
    Box::pin(async move {
        let lateness = clock.now().duration_since(prev).unwrap_or_default();
        let read = guard_call(&job, || guard.last_tick(&job));
        let last = match tokio::time::timeout(grace.saturating_sub(lateness), read).await {
            Ok(Ok(last)) => last,
            Ok(Err(error)) => {
                tracing::warn!(
                    job = &*job,
                    code = error.code().as_str(),
                    "could not read the job's last tick; no catch-up"
                );
                return Acquired::Nothing;
            }
            Err(_) => {
                tracing::warn!(
                    job = &*job,
                    "the job's last tick was not read within the misfire grace; no catch-up"
                );
                return Acquired::Nothing;
            }
        };
        let now = clock.now();
        if !schedule::catch_up_due(prev, last, now, grace) {
            return Acquired::Nothing;
        }
        tracing::info!(
            job = &*job,
            tick_ms = schedule::unix_millis(prev),
            "catching up on a missed tick"
        );
        acquire_tick(guard, job, clock, prev, prev, grace).await
    })
}

/// Run `call` and its future, turning a panic in either into `Err` with the
/// payload text.
async fn unwind<'a, T>(call: impl FnOnce() -> BoxFuture<'a, T>) -> Result<T, String> {
    let future =
        std::panic::catch_unwind(AssertUnwindSafe(call)).map_err(|panic| panic_detail(&*panic))?;
    AssertUnwindSafe(future)
        .catch_unwind()
        .await
        .map_err(|panic| panic_detail(&*panic))
}

/// A call to the guard; a panic in it is logged and becomes an `INTERNAL`
/// error with reason `GUARD_PANICKED`, so it counts as a guard error.
async fn guard_call<'a, T>(
    job: &str,
    call: impl FnOnce() -> BoxFuture<'a, Result<T, AppError>>,
) -> Result<T, AppError> {
    unwind(call).await.unwrap_or_else(|detail| {
        guard_panicked(job, &detail);
        Err(AppError::new(
            ErrorCode::Internal,
            format!("the guard of job {job} panicked"),
        )
        .with_reason(reasons::GUARD_PANICKED))
    })
}

fn guard_panicked(job: &str, detail: &str) {
    tracing::error!(
        job = job,
        code = ErrorCode::Internal.as_str(),
        reason = reasons::GUARD_PANICKED,
        panic = %detail,
        "the job's guard panicked"
    );
}

/// One run under its span: the job's future, its timeout and lost guard,
/// the logs, then the permit's release.
fn execute(
    run: RunFn,
    cx: JobContext,
    permit: Option<JobPermit>,
    clock: Arc<dyn Clock>,
    info: JobRun,
) -> BoxFuture<'static, JobRunOutcome> {
    let span = tracing::info_span!(
        "job",
        job = cx.job(),
        run_id = cx.run_id(),
        trigger = info.trigger.as_str()
    );
    Box::pin(
        async move {
            let (lost, release) = permit.map(JobPermit::into_parts).unzip();
            let started = Instant::now();
            tracing::info!("job run started");
            let ended = run_once(&run, &cx, lost).await;
            let duration_ms = millis(started.elapsed());
            let error = match ended {
                Ok(()) => {
                    tracing::info!(duration_ms, "job run finished");
                    None
                }
                Err(Ended::Failed(error)) => {
                    tracing::warn!(
                        duration_ms,
                        code = error.code().as_str(),
                        reason = error.reason().unwrap_or_default(),
                        error = error.message(),
                        "job run failed"
                    );
                    Some(error.code())
                }
                Err(Ended::Panicked(detail)) => {
                    tracing::error!(
                        duration_ms,
                        code = ErrorCode::Internal.as_str(),
                        reason = reasons::JOB_PANICKED,
                        panic = %detail,
                        "job run panicked"
                    );
                    Some(ErrorCode::Internal)
                }
            };
            let finished_at = clock.now();
            if let Some(release) = release {
                release_permit(cx.job(), release).await;
            }
            JobRunOutcome {
                run: info,
                finished_at,
                error,
            }
        }
        .instrument(span),
    )
}

/// The job's future, until it ends, its guard is lost or its timeout
/// passes. In the last two cases the run's token fires before the future is
/// dropped; they win over a future that ends at the same time, and when
/// either already happened the future is never made.
async fn run_once(
    run: &RunFn,
    cx: &JobContext,
    lost: Option<CancellationToken>,
) -> Result<(), Ended> {
    let deadline = cx.inner.deadline;
    if lost.as_ref().is_some_and(CancellationToken::is_cancelled) {
        cx.cancel(CancelReason::LeaseLost);
        return Err(lease_lost(cx));
    }
    if deadline.is_some_and(|deadline| deadline <= Instant::now()) {
        cx.cancel(CancelReason::Timeout);
        return Err(timed_out(cx));
    }
    let future = match std::panic::catch_unwind(AssertUnwindSafe(|| run(cx.clone()))) {
        Ok(future) => future,
        Err(panic) => return Err(Ended::Panicked(panic_detail(&*panic))),
    };
    let withdrawn = async {
        match &lost {
            Some(lost) => lost.cancelled().await,
            None => std::future::pending::<()>().await,
        }
        cx.cancel(CancelReason::LeaseLost);
    };
    let expired = async {
        match deadline {
            Some(deadline) => tokio::time::sleep_until(deadline).await,
            None => std::future::pending::<()>().await,
        }
        cx.cancel(CancelReason::Timeout);
    };
    tokio::select! {
        biased;
        () = withdrawn => Err(lease_lost(cx)),
        () = expired => Err(timed_out(cx)),
        result = AssertUnwindSafe(future).catch_unwind() => match result {
            Ok(result) => result.map_err(Ended::Failed),
            Err(panic) => Err(Ended::Panicked(panic_detail(&*panic))),
        },
    }
}

fn lease_lost(cx: &JobContext) -> Ended {
    Ended::Failed(
        AppError::new(
            ErrorCode::Aborted,
            format!("job {} lost the right to run", cx.job()),
        )
        .with_reason(reasons::LEASE_LOST),
    )
}

fn timed_out(cx: &JobContext) -> Ended {
    Ended::Failed(
        AppError::deadline_exceeded(format!("job {} ran longer than its timeout", cx.job()))
            .with_reason(reasons::JOB_TIMED_OUT),
    )
}

/// Give the permit back, waiting at most `RELEASE_WAIT`; a release that
/// panics is logged like any guard panic.
async fn release_permit(job: &str, release: Release) {
    match tokio::time::timeout(schedule::RELEASE_WAIT, unwind(release)).await {
        Ok(Ok(())) => {}
        Ok(Err(detail)) => guard_panicked(job, &detail),
        Err(_) => tracing::warn!(
            job = job,
            "the job's permit was not released within 5 s; left to the guard"
        ),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::super::RunInfo;
    use super::*;

    fn context(deadline: Option<Instant>) -> JobContext {
        JobContext {
            inner: Arc::new(RunInfo {
                job: Arc::from("orders-sync"),
                run_id: "run-1".into(),
                trigger: RunTrigger::Schedule,
                tick: None,
                fence: None,
                cancel: RunCancel::new(),
                deadline,
            }),
        }
    }

    /// A job that counts the futures made and runs `body` in each.
    fn counted(
        calls: &Arc<AtomicUsize>,
        body: impl Fn() -> BoxFuture<'static, Result<(), AppError>> + Send + Sync + 'static,
    ) -> RunFn {
        let calls = Arc::clone(calls);
        Arc::new(
            move |_cx: JobContext| -> BoxFuture<'static, Result<(), AppError>> {
                calls.fetch_add(1, Ordering::SeqCst);
                body()
            },
        )
    }

    fn failed(ended: Result<(), Ended>) -> AppError {
        let Err(Ended::Failed(error)) = ended else {
            panic!("the run did not fail");
        };
        error
    }

    #[tokio::test(start_paused = true)]
    async fn a_run_whose_guard_is_already_lost_is_never_made() {
        let calls = Arc::new(AtomicUsize::new(0));
        let run = counted(&calls, || Box::pin(async { Ok(()) }));
        let cx = context(None);
        let lost = CancellationToken::new();
        lost.cancel();

        let error = failed(run_once(&run, &cx, Some(lost)).await);
        assert_eq!(error.code(), ErrorCode::Aborted);
        assert_eq!(error.reason(), Some(reasons::LEASE_LOST));
        assert_eq!(cx.cancel_reason(), Some(CancelReason::LeaseLost));
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn a_run_already_past_its_timeout_is_never_made() {
        let calls = Arc::new(AtomicUsize::new(0));
        let run = counted(&calls, || Box::pin(async { Ok(()) }));
        let cx = context(Some(Instant::now()));

        let error = failed(run_once(&run, &cx, None).await);
        assert_eq!(error.code(), ErrorCode::DeadlineExceeded);
        assert_eq!(error.reason(), Some(reasons::JOB_TIMED_OUT));
        assert_eq!(cx.cancel_reason(), Some(CancelReason::Timeout));
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn a_guard_lost_while_the_run_ends_wins() {
        let calls = Arc::new(AtomicUsize::new(0));
        let lost = CancellationToken::new();
        let withdraw = lost.clone();
        let run = counted(&calls, move || {
            let withdraw = withdraw.clone();
            Box::pin(async move {
                withdraw.cancel();
                tokio::task::yield_now().await;
                Ok(())
            })
        });
        let cx = context(None);

        let error = failed(run_once(&run, &cx, Some(lost)).await);
        assert_eq!(error.reason(), Some(reasons::LEASE_LOST));
        assert_eq!(cx.cancel_reason(), Some(CancelReason::LeaseLost));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn a_timeout_due_when_the_run_ends_wins() {
        let calls = Arc::new(AtomicUsize::new(0));
        let run = counted(&calls, || {
            Box::pin(async {
                tokio::time::sleep(Duration::from_secs(5)).await;
                Ok(())
            })
        });
        let cx = context(Some(Instant::now() + Duration::from_secs(5)));

        let error = failed(run_once(&run, &cx, None).await);
        assert_eq!(error.reason(), Some(reasons::JOB_TIMED_OUT));
        assert_eq!(cx.cancel_reason(), Some(CancelReason::Timeout));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    fn explode<T>() -> T {
        panic!("the lease store exploded")
    }

    #[tokio::test(start_paused = true)]
    async fn guard_panics_become_errors_with_their_reason() {
        let called = guard_call(
            "orders-sync",
            explode::<BoxFuture<'static, Result<(), AppError>>>,
        )
        .await
        .unwrap_err();
        assert_eq!(called.code(), ErrorCode::Internal);
        assert_eq!(called.reason(), Some(reasons::GUARD_PANICKED));
        assert_eq!(called.message(), "the guard of job orders-sync panicked");

        let polled = guard_call("orders-sync", || {
            Box::pin(async { explode::<Result<(), AppError>>() })
        })
        .await
        .unwrap_err();
        assert_eq!(polled.reason(), Some(reasons::GUARD_PANICKED));

        let fine = guard_call("orders-sync", || Box::pin(async { Ok::<u8, AppError>(7) })).await;
        assert_eq!(fine.unwrap(), 7);
    }
}
