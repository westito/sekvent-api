//! Interval, cron and manual jobs, run as one runtime unit each.

mod clock;
mod driver;
mod guard;
mod schedule;

use std::error::Error as StdError;
use std::fmt;
use std::future::Future;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};
use std::time::{Duration, SystemTime};

use futures::future::BoxFuture;
use rand::rngs::SmallRng;
use rand::{Rng, SeedableRng};
use sekvent_context::{CallContext, Clock, SystemClock};
use sekvent_error::{AppError, ErrorCode};
use tokio::sync::{mpsc, oneshot};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

pub use clock::TokioWallClock;
pub use guard::{JobGuard, JobPermit};

use crate::UnitContext;
use crate::reasons;

/// The future of one run, boxed.
pub(crate) type RunFn =
    Arc<dyn Fn(JobContext) -> BoxFuture<'static, Result<(), AppError>> + Send + Sync>;

/// A unit factory that drives one job.
pub(crate) type JobFactory =
    Box<dyn FnMut(UnitContext) -> BoxFuture<'static, Result<(), AppError>> + Send>;

/// When a job runs.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum Schedule {
    /// Every period: in process anchored at the unit's start, for a
    /// singleton aligned to the Unix epoch on the wall clock.
    Interval(Duration),
    /// A cron pattern in UTC (five fields, or six with leading seconds),
    /// parsed when the spec was made.
    #[cfg(feature = "cron")]
    Cron(String),
    /// Only when triggered through a [`JobHandle`].
    Manual,
}

/// When and how a job runs; register it with
/// [`RuntimeBuilder::job`](crate::RuntimeBuilder::job).
///
/// The rules the builder checks are listed on
/// [`RuntimeBuilder::build`](crate::RuntimeBuilder::build).
pub struct JobSpec {
    schedule: Schedule,
    #[cfg(feature = "cron")]
    cron: Option<Arc<croner::Cron>>,
    jitter: Option<Duration>,
    rng: Box<dyn FnMut() -> u64 + Send>,
    initial_delay: Option<Duration>,
    timeout: Option<Duration>,
    misfire_grace: Option<Duration>,
    guard: Option<Arc<dyn JobGuard>>,
    clock: Arc<dyn Clock>,
    shared: Arc<Shared>,
}

impl fmt::Debug for JobSpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("JobSpec")
            .field("schedule", &self.schedule)
            .field("jitter", &self.jitter)
            .field("initial_delay", &self.initial_delay)
            .field("timeout", &self.timeout)
            .field("misfire_grace", &self.misfire_grace)
            .field("singleton", &self.guard.is_some())
            .finish_non_exhaustive()
    }
}

impl JobSpec {
    fn with_schedule(schedule: Schedule) -> Self {
        let mut rng = SmallRng::from_rng(&mut rand::rng());
        Self {
            schedule,
            #[cfg(feature = "cron")]
            cron: None,
            jitter: None,
            rng: Box::new(move || rng.next_u64()),
            initial_delay: None,
            timeout: None,
            misfire_grace: None,
            guard: None,
            clock: Arc::new(SystemClock),
            shared: Arc::new(Shared::new()),
        }
    }

    /// Run every `period`. In process the first run comes `period` after
    /// the unit starts (see [`initial_delay`](Self::initial_delay)) and the
    /// cadence is fixed whatever the runs take; a singleton runs at the
    /// multiples of `period` since the Unix epoch.
    pub fn interval(period: Duration) -> Self {
        Self::with_schedule(Schedule::Interval(period))
    }

    /// Run at the occurrences of a cron pattern in UTC: five fields, or six
    /// with leading seconds.
    ///
    /// `INVALID_ARGUMENT` naming the pattern when it does not parse.
    #[cfg(feature = "cron")]
    pub fn cron(pattern: &str) -> Result<Self, AppError> {
        let cron = schedule::parse_cron(pattern).map_err(|error| {
            AppError::invalid_argument(format!("invalid cron pattern {pattern:?}: {error}"))
        })?;
        let mut spec = Self::with_schedule(Schedule::Cron(pattern.to_owned()));
        spec.cron = Some(Arc::new(cron));
        Ok(spec)
    }

    /// Runs only when triggered through its handle.
    pub fn manual() -> Self {
        Self::with_schedule(Schedule::Manual)
    }

    /// Delay each scheduled run by a uniform amount in `[0, max]`. The ticks
    /// themselves do not move; catch-up and manual runs are never delayed.
    #[must_use]
    pub fn jitter(mut self, max: Duration) -> Self {
        self.jitter = Some(max);
        self
    }

    /// The generator for jitter (default: seeded from the thread RNG).
    #[must_use]
    pub fn jitter_rng(mut self, mut rng: impl Rng + Send + 'static) -> Self {
        self.rng = Box::new(move || rng.next_u64());
        self
    }

    /// Earliest start of a run after the unit starts; anchors in-process
    /// intervals (default: the period for in-process intervals, zero
    /// otherwise).
    #[must_use]
    pub fn initial_delay(mut self, delay: Duration) -> Self {
        self.initial_delay = Some(delay);
        self
    }

    /// Cancel and drop a run that takes longer; the run fails with
    /// `DEADLINE_EXCEEDED`.
    #[must_use]
    pub fn timeout(mut self, limit: Duration) -> Self {
        self.timeout = Some(limit);
        self
    }

    /// How late a tick may start before it is skipped (default 1 min, at
    /// least 1 s).
    #[must_use]
    pub fn misfire_grace(mut self, grace: Duration) -> Self {
        self.misfire_grace = Some(grace);
        self
    }

    /// Run on one instance at a time, as `guard` decides. Singleton
    /// intervals follow the wall clock and are aligned to the Unix epoch.
    #[must_use]
    pub fn singleton(mut self, guard: impl JobGuard) -> Self {
        self.guard = Some(Arc::new(guard));
        self
    }

    /// The wall clock for cron and singleton ticks (default `SystemClock`).
    #[must_use]
    pub fn clock(mut self, clock: Arc<dyn Clock>) -> Self {
        self.clock = clock;
        self
    }

    /// A handle for triggers and status, usable before the job is registered.
    pub fn handle(&self) -> JobHandle {
        JobHandle {
            shared: Arc::clone(&self.shared),
        }
    }

    /// When the job runs.
    pub fn schedule(&self) -> &Schedule {
        &self.schedule
    }

    fn grace(&self) -> Duration {
        self.misfire_grace
            .unwrap_or(schedule::DEFAULT_MISFIRE_GRACE)
    }
}

/// The checks `RuntimeBuilder::build` runs on a job, as a message naming it.
pub(crate) fn validate(name: &str, spec: &JobSpec) -> Result<(), String> {
    let name_ok = (1..=100).contains(&name.len())
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b':' | b'-'));
    if !name_ok {
        return Err(format!(
            "job name {name:?} must be 1 to 100 characters of A-Z, a-z, 0-9, '.', '_', ':' and '-'"
        ));
    }
    let problem = |text: &str| Err(format!("job {name}: {text}"));
    if spec.timeout.is_some_and(|timeout| timeout.is_zero()) {
        return problem("the timeout must be positive");
    }
    if spec
        .misfire_grace
        .is_some_and(|grace| grace < schedule::MIN_MISFIRE_GRACE)
    {
        return problem("the misfire grace must be at least 1 s");
    }
    match &spec.schedule {
        Schedule::Interval(period) => {
            if period.is_zero() {
                return problem("the interval must be positive");
            }
            if spec.guard.is_some()
                && (*period < Duration::from_secs(1) || period.subsec_nanos() % 1_000_000 != 0)
            {
                return problem(
                    "a singleton interval must be at least 1 s and a whole number of milliseconds",
                );
            }
            if spec.jitter.is_some_and(|jitter| jitter >= *period) {
                return problem("the jitter must be below the interval");
            }
        }
        #[cfg(feature = "cron")]
        Schedule::Cron(_) => {
            if spec
                .jitter
                .is_some_and(|jitter| jitter > schedule::MAX_CRON_JITTER)
            {
                return problem("the jitter of a cron job must not exceed 1 h");
            }
            let now = spec.clock.now();
            if spec
                .cron
                .as_deref()
                .and_then(|cron| schedule::cron_at_or_after(cron, now))
                .is_none()
            {
                return problem("the cron pattern has no occurrence ahead");
            }
        }
        Schedule::Manual => {
            if spec.jitter.is_some() || spec.initial_delay.is_some() || spec.misfire_grace.is_some()
            {
                return problem("a manual job takes no jitter, initial delay or misfire grace");
            }
        }
    }
    Ok(())
}

/// The unit factory for job `name`: the first call drives the job, any
/// later one fails (the unit is critical, so it is never restarted).
pub(crate) fn factory<F, Fut>(name: &str, spec: JobSpec, run: F) -> JobFactory
where
    F: Fn(JobContext) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<(), AppError>> + Send + 'static,
{
    let name: Arc<str> = Arc::from(name);
    let _ = spec.shared.name.set(Arc::clone(&name));
    let run: RunFn = Arc::new(
        move |cx: JobContext| -> BoxFuture<'static, Result<(), AppError>> { Box::pin(run(cx)) },
    );
    let mut driver = Some(driver::Driver::new(Arc::clone(&name), spec, run));
    Box::new(
        move |unit: UnitContext| -> BoxFuture<'static, Result<(), AppError>> {
            let driver = driver.take();
            let name = Arc::clone(&name);
            Box::pin(async move {
                match driver {
                    Some(driver) => {
                        driver.run(unit).await;
                        Ok(())
                    }
                    None => Err(AppError::new(
                        ErrorCode::Internal,
                        format!("job {name} cannot be started twice"),
                    )),
                }
            })
        },
    )
}

/// How a run was started.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum RunTrigger {
    /// A tick of the schedule.
    Schedule,
    /// A missed tick of a guarded job, run once after the unit started.
    CatchUp,
    /// [`JobHandle::trigger`].
    Manual,
}

impl RunTrigger {
    fn as_str(self) -> &'static str {
        match self {
            Self::Schedule => "schedule",
            Self::CatchUp => "catch_up",
            Self::Manual => "manual",
        }
    }
}

/// Why a run was cancelled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum CancelReason {
    /// The job's stage drains; the run may finish until the stop deadline.
    Shutdown,
    /// The run passed its timeout and is dropped.
    Timeout,
    /// The guard withdrew the right to run and the run is dropped.
    LeaseLost,
}

/// A run's cancellation: the reason is set before the token fires.
#[derive(Clone)]
struct RunCancel {
    token: CancellationToken,
    reason: Arc<OnceLock<CancelReason>>,
}

impl RunCancel {
    fn new() -> Self {
        Self {
            token: CancellationToken::new(),
            reason: Arc::new(OnceLock::new()),
        }
    }

    /// Fire the token; the first reason given wins.
    fn cancel(&self, reason: CancelReason) {
        let _ = self.reason.set(reason);
        self.token.cancel();
    }
}

/// What one run gets.
#[derive(Clone)]
pub struct JobContext {
    inner: Arc<RunInfo>,
}

struct RunInfo {
    job: Arc<str>,
    run_id: String,
    trigger: RunTrigger,
    tick: Option<SystemTime>,
    fence: Option<u64>,
    cancel: RunCancel,
    deadline: Option<Instant>,
}

impl JobContext {
    /// The job's registered name.
    pub fn job(&self) -> &str {
        &self.inner.job
    }

    /// A fresh request id per run (UUID v7), also on the run's span.
    pub fn run_id(&self) -> &str {
        &self.inner.run_id
    }

    /// How the run was started.
    pub fn trigger(&self) -> RunTrigger {
        self.inner.trigger
    }

    /// The scheduled time of a scheduled or catch-up run.
    pub fn tick(&self) -> Option<SystemTime> {
        self.inner.tick
    }

    /// The guard's fencing token, for a singleton run.
    pub fn fence(&self) -> Option<u64> {
        self.inner.fence
    }

    /// Whether the run was cancelled.
    pub fn is_cancelled(&self) -> bool {
        self.inner.cancel.token.is_cancelled()
    }

    /// Resolve once the run is cancelled.
    pub async fn cancelled(&self) {
        self.inner.cancel.token.cancelled().await;
    }

    /// A child token for tasks the run spawns.
    pub fn cancel_token(&self) -> CancellationToken {
        self.inner.cancel.token.child_token()
    }

    /// Why the run was cancelled; `None` while it was not.
    pub fn cancel_reason(&self) -> Option<CancelReason> {
        self.inner.cancel.reason.get().copied()
    }

    /// A root `CallContext`: the run id, the run's cancellation, the timeout
    /// as deadline.
    pub fn call_context(&self) -> CallContext {
        let ctx = CallContext::new()
            .with_request_id(self.run_id())
            .with_cancel(self.cancel_token());
        match self.inner.deadline {
            Some(deadline) => ctx.with_deadline(deadline.into_std()),
            None => ctx,
        }
    }

    fn cancel(&self, reason: CancelReason) {
        self.inner.cancel.cancel(reason);
    }
}

impl fmt::Debug for JobContext {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("JobContext")
            .field("job", &self.inner.job)
            .field("run_id", &self.inner.run_id)
            .field("trigger", &self.inner.trigger)
            .field("tick", &self.inner.tick)
            .field("fence", &self.inner.fence)
            .field("cancelled", &self.is_cancelled())
            .finish_non_exhaustive()
    }
}

/// A run that started.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct RunStarted {
    /// The run's id ([`JobContext::run_id`]).
    pub run_id: String,
    /// The guard's fencing token, for a singleton run.
    pub fence: Option<u64>,
}

/// Status of one job, as seen by this process.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct JobStatus {
    /// Where the job's unit is.
    pub state: JobState,
    /// The run in progress.
    pub current: Option<JobRun>,
    /// The last finished run.
    pub last: Option<JobRunOutcome>,
    /// Next scheduled tick on the job's wall clock.
    pub next_tick: Option<SystemTime>,
    /// Runs started.
    pub runs: u64,
    /// Runs that returned an error, panicked, timed out or lost their guard.
    pub failures: u64,
    /// Ticks that did not run here: overlaps, misfires, ticks passed over
    /// after a stall or a clock jump, ticks another instance held and ticks
    /// the guard kept failing on.
    pub skipped: u64,
}

/// Where a job's unit is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum JobState {
    /// The unit has not started.
    NotStarted,
    /// Waiting for the next tick or trigger.
    Idle,
    /// A run is in progress.
    Running,
    /// The unit stopped.
    Stopped,
}

/// One run.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct JobRun {
    /// The run's id.
    pub run_id: String,
    /// How it was started.
    pub trigger: RunTrigger,
    /// The scheduled time of a scheduled or catch-up run.
    pub tick: Option<SystemTime>,
    /// When it started, on the job's wall clock.
    pub started_at: SystemTime,
    /// The guard's fencing token, for a singleton run.
    pub fence: Option<u64>,
}

/// How one run ended.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct JobRunOutcome {
    /// The run.
    pub run: JobRun,
    /// When it ended, on the job's wall clock.
    pub finished_at: SystemTime,
    /// The failure's code; `None` for a run that returned `Ok`.
    pub error: Option<ErrorCode>,
}

/// A trigger sent to the job's unit.
struct Request {
    reply: oneshot::Sender<Result<RunStarted, TriggerError>>,
}

/// State shared by a spec's handles and its unit.
struct Shared {
    name: OnceLock<Arc<str>>,
    status: Mutex<JobStatus>,
    requests: Mutex<Option<mpsc::Sender<Request>>>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Shared {
    fn new() -> Self {
        Self {
            name: OnceLock::new(),
            status: Mutex::new(JobStatus {
                state: JobState::NotStarted,
                current: None,
                last: None,
                next_tick: None,
                runs: 0,
                failures: 0,
                skipped: 0,
            }),
            requests: Mutex::new(None),
        }
    }

    fn name(&self) -> String {
        self.name.get().map(ToString::to_string).unwrap_or_default()
    }

    fn update(&self, change: impl FnOnce(&mut JobStatus)) {
        change(&mut lock(&self.status));
    }

    /// Start accepting triggers.
    fn open(&self, requests: mpsc::Sender<Request>) {
        *lock(&self.requests) = Some(requests);
        self.update(|status| status.state = JobState::Idle);
    }

    /// Stop accepting triggers.
    fn close(&self) {
        *lock(&self.requests) = None;
    }

    fn started(&self, run: JobRun) {
        self.update(|status| {
            status.state = JobState::Running;
            status.current = Some(run);
            status.runs = status.runs.saturating_add(1);
        });
    }

    fn finished(&self, outcome: JobRunOutcome) {
        self.update(|status| {
            status.state = JobState::Idle;
            status.current = None;
            if outcome.error.is_some() {
                status.failures = status.failures.saturating_add(1);
            }
            status.last = Some(outcome);
        });
    }

    fn skipped(&self, count: u64) {
        self.update(|status| status.skipped = status.skipped.saturating_add(count));
    }
}

/// Triggers and status of one job. Cheap to clone.
#[derive(Clone)]
pub struct JobHandle {
    shared: Arc<Shared>,
}

impl JobHandle {
    /// Start a run now; resolves once it started, or with why not.
    ///
    /// A manual run gets no jitter and never shifts the schedule; a tick
    /// falling inside it is an overlap.
    ///
    /// A singleton's guard gets at most 5 s to answer; past that the trigger
    /// fails with `GuardFailed(DEADLINE_EXCEEDED)`.
    pub async fn trigger(&self) -> Result<RunStarted, TriggerError> {
        let not_running = || self.error(TriggerErrorKind::NotRunning);
        let Some(requests) = lock(&self.shared.requests).clone() else {
            return Err(not_running());
        };
        let (reply, answer) = oneshot::channel();
        // A failed send hands the request back, reply sender included:
        // awaiting the answer while it is alive would never end.
        if requests.send(Request { reply }).await.is_err() {
            return Err(not_running());
        }
        answer.await.unwrap_or_else(|_| Err(not_running()))
    }

    /// The job's status now.
    pub fn status(&self) -> JobStatus {
        lock(&self.shared.status).clone()
    }

    /// `None` until the spec is registered.
    pub fn name(&self) -> Option<String> {
        self.shared.name.get().map(ToString::to_string)
    }

    fn error(&self, kind: TriggerErrorKind) -> TriggerError {
        TriggerError {
            job: self.shared.name(),
            kind,
        }
    }
}

impl fmt::Debug for JobHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("JobHandle")
            .field("name", &self.shared.name.get())
            .field("state", &lock(&self.shared.status).state)
            .finish_non_exhaustive()
    }
}

/// Why a trigger did not start a run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TriggerError {
    job: String,
    kind: TriggerErrorKind,
}

impl TriggerError {
    /// The job's name (empty before the spec is registered).
    pub fn job(&self) -> &str {
        &self.job
    }

    /// Why no run started.
    pub fn kind(&self) -> TriggerErrorKind {
        self.kind
    }
}

impl fmt::Display for TriggerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let job = if self.job.is_empty() {
            "<unregistered>"
        } else {
            &self.job
        };
        match self.kind {
            TriggerErrorKind::AlreadyRunning => write!(f, "job {job} is already running"),
            TriggerErrorKind::HeldElsewhere => {
                write!(f, "job {job} is held by another instance")
            }
            TriggerErrorKind::NotRunning => write!(f, "job {job} is not running"),
            TriggerErrorKind::GuardFailed(code) => {
                write!(
                    f,
                    "job {job} could not be acquired: the guard failed with {code}"
                )
            }
        }
    }
}

impl StdError for TriggerError {}

/// Why a trigger did not start a run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum TriggerErrorKind {
    /// A run of the job is in progress here.
    AlreadyRunning,
    /// The guard answered that another instance holds the job.
    HeldElsewhere,
    /// The job's unit is not running: not started, draining or stopped.
    NotRunning,
    /// The guard failed with this code.
    GuardFailed(ErrorCode),
}

impl From<TriggerError> for AppError {
    fn from(error: TriggerError) -> Self {
        let message = error.to_string();
        let (code, reason) = match error.kind {
            TriggerErrorKind::AlreadyRunning => {
                (ErrorCode::FailedPrecondition, reasons::JOB_ALREADY_RUNNING)
            }
            TriggerErrorKind::HeldElsewhere => {
                (ErrorCode::FailedPrecondition, reasons::JOB_HELD_ELSEWHERE)
            }
            TriggerErrorKind::NotRunning => (ErrorCode::Unavailable, reasons::JOB_NOT_RUNNING),
            TriggerErrorKind::GuardFailed(_) => (ErrorCode::Unavailable, reasons::JOB_GUARD_FAILED),
        };
        let app = AppError::new(code, message)
            .with_reason(reason)
            .with_metadata("job", error.job);
        match error.kind {
            TriggerErrorKind::GuardFailed(guard) => app.with_metadata("guard_code", guard.as_str()),
            _ => app,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn context(deadline: Option<Instant>) -> JobContext {
        JobContext {
            inner: Arc::new(RunInfo {
                job: Arc::from("orders-sync"),
                run_id: "run-1".into(),
                trigger: RunTrigger::CatchUp,
                tick: Some(SystemTime::UNIX_EPOCH),
                fence: Some(4),
                cancel: RunCancel::new(),
                deadline,
            }),
        }
    }

    fn error(job: &str, kind: TriggerErrorKind) -> TriggerError {
        TriggerError {
            job: job.into(),
            kind,
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_context_reports_its_run_and_its_cancellation() {
        let deadline = Instant::now() + Duration::from_secs(30);
        let cx = context(Some(deadline));
        assert_eq!(cx.job(), "orders-sync");
        assert_eq!(cx.run_id(), "run-1");
        assert_eq!(cx.trigger(), RunTrigger::CatchUp);
        assert_eq!(cx.tick(), Some(SystemTime::UNIX_EPOCH));
        assert_eq!(cx.fence(), Some(4));
        assert!(!cx.is_cancelled());
        assert_eq!(cx.cancel_reason(), None);

        let call = cx.call_context();
        assert_eq!(call.request_id(), "run-1");
        assert_eq!(call.deadline(), Some(deadline.into_std()));
        let child = cx.cancel_token();
        assert!(format!("{cx:?}").contains("cancelled: false"));

        cx.cancel(CancelReason::Timeout);
        cx.clone().cancel(CancelReason::Shutdown);
        cx.cancelled().await;
        assert!(cx.is_cancelled());
        assert!(child.is_cancelled());
        assert!(call.cancel_token().is_cancelled());
        assert_eq!(
            cx.cancel_reason(),
            Some(CancelReason::Timeout),
            "the first reason wins"
        );
        assert!(context(None).call_context().deadline().is_none());

        let shown = format!("{cx:?}");
        assert!(shown.starts_with("JobContext {"));
        assert!(shown.contains("\"orders-sync\""));
        assert!(shown.contains("cancelled: true"));
    }

    #[test]
    fn triggers_are_named_for_logs() {
        assert_eq!(RunTrigger::Schedule.as_str(), "schedule");
        assert_eq!(RunTrigger::CatchUp.as_str(), "catch_up");
        assert_eq!(RunTrigger::Manual.as_str(), "manual");
    }

    #[test]
    fn trigger_errors_read_well_and_map_to_app_errors() {
        let rows = [
            (
                TriggerErrorKind::AlreadyRunning,
                "job orders-sync is already running",
                ErrorCode::FailedPrecondition,
                reasons::JOB_ALREADY_RUNNING,
            ),
            (
                TriggerErrorKind::HeldElsewhere,
                "job orders-sync is held by another instance",
                ErrorCode::FailedPrecondition,
                reasons::JOB_HELD_ELSEWHERE,
            ),
            (
                TriggerErrorKind::NotRunning,
                "job orders-sync is not running",
                ErrorCode::Unavailable,
                reasons::JOB_NOT_RUNNING,
            ),
            (
                TriggerErrorKind::GuardFailed(ErrorCode::Unavailable),
                "job orders-sync could not be acquired: the guard failed with UNAVAILABLE",
                ErrorCode::Unavailable,
                reasons::JOB_GUARD_FAILED,
            ),
        ];
        for (kind, text, code, reason) in rows {
            let trigger = error("orders-sync", kind);
            assert_eq!(trigger.job(), "orders-sync");
            assert_eq!(trigger.kind(), kind);
            assert_eq!(trigger.to_string(), text);
            let app = AppError::from(trigger);
            assert_eq!(app.code(), code, "{kind:?}");
            assert_eq!(app.reason(), Some(reason));
            assert_eq!(app.message(), text);
            assert_eq!(
                app.metadata().get("job").map(String::as_str),
                Some("orders-sync")
            );
            let guard_code = app.metadata().get("guard_code").map(String::as_str);
            match kind {
                TriggerErrorKind::GuardFailed(_) => assert_eq!(guard_code, Some("UNAVAILABLE")),
                _ => assert_eq!(guard_code, None),
            }
        }
        assert_eq!(
            error("", TriggerErrorKind::NotRunning).to_string(),
            "job <unregistered> is not running"
        );
    }

    #[test]
    fn specs_show_their_settings_but_not_their_internals() {
        let spec = JobSpec::interval(Duration::from_secs(10))
            .jitter(Duration::from_secs(1))
            .timeout(Duration::from_secs(5));
        assert!(
            matches!(spec.schedule(), Schedule::Interval(period) if *period == Duration::from_secs(10))
        );
        let shown = format!("{spec:?}");
        assert!(shown.starts_with("JobSpec { schedule: Interval(10s)"));
        assert!(shown.contains("singleton: false"));
        assert!(shown.ends_with(".. }"));
        assert!(matches!(JobSpec::manual().schedule(), Schedule::Manual));
    }

    #[tokio::test]
    async fn a_handle_before_registration_has_no_name_and_does_not_trigger() {
        let spec = JobSpec::manual();
        let handle = spec.handle();
        assert_eq!(handle.name(), None);
        assert_eq!(handle.status().state, JobState::NotStarted);
        let refused = handle.trigger().await.unwrap_err();
        assert_eq!(refused.kind(), TriggerErrorKind::NotRunning);
        assert_eq!(refused.job(), "");
        assert_eq!(
            format!("{handle:?}"),
            "JobHandle { name: None, state: NotStarted, .. }"
        );

        let _factory = factory("orders-sync", spec, |_cx| async { Ok(()) });
        assert_eq!(handle.name().as_deref(), Some("orders-sync"));
        assert!(format!("{:?}", handle.clone()).contains("\"orders-sync\""));
    }

    #[tokio::test]
    async fn a_job_unit_cannot_be_started_twice() {
        let stopped = CancellationToken::new();
        stopped.cancel();
        let unit = || {
            UnitContext::new(
                Arc::from("orders-sync"),
                crate::Stage::Workers,
                0,
                stopped.clone(),
                Arc::new(tokio::sync::watch::channel(false).0),
                crate::HealthRegistry::new(),
                crate::unit::StopWindow {
                    grace: Duration::from_secs(1),
                    by: Arc::default(),
                },
            )
        };
        let mut factory = factory("orders-sync", JobSpec::manual(), |_cx| async { Ok(()) });
        factory(unit()).await.unwrap();
        let error = factory(unit()).await.unwrap_err();
        assert_eq!(error.code(), ErrorCode::Internal);
        assert_eq!(error.message(), "job orders-sync cannot be started twice");
    }

    #[test]
    fn job_names_are_checked() {
        let spec = JobSpec::manual();
        assert_eq!(validate("orders.sync:v2_a-b", &spec), Ok(()));
        assert_eq!(validate(&"a".repeat(100), &spec), Ok(()));
        for bad in ["", "orders sync", "orders/sync", "ő"] {
            let problem = validate(bad, &spec).unwrap_err();
            assert!(problem.contains(&format!("{bad:?}")), "{problem}");
        }
        assert!(validate(&"a".repeat(101), &spec).is_err());
    }
}
