use std::any::Any;
use std::collections::BTreeSet;
use std::fmt;
use std::future::Future;
use std::io;
use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};
use std::time::Duration;

use futures::FutureExt;
use futures::future::{BoxFuture, join_all, try_join_all};
use sekvent_error::{AppError, ErrorCode};
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::probe::{DependencyProbe, probe_unit};
use crate::signal::{Trigger, any_trigger, os_signals, signal_arrived};
use crate::unit::StopWindow;
use crate::{HealthRegistry, Stage, UnitContext, UnitPolicy};

type UnitFuture = BoxFuture<'static, Result<(), AppError>>;
type UnitFactory = Box<dyn FnMut(UnitContext) -> UnitFuture + Send>;

/// Name of the built-in unit that polls dependency probes.
pub const PROBE_UNIT: &str = "health-probes";

struct UnitSpec {
    name: Arc<str>,
    stage: Stage,
    policy: UnitPolicy,
    factory: UnitFactory,
}

#[derive(Debug, Clone, Copy)]
struct Settings {
    start_timeout: Duration,
    shutdown_delay: Duration,
    stage_grace: Duration,
    shutdown_deadline: Duration,
    restart_reset_after: Duration,
}

/// Why the runtime shut down.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ShutdownReason {
    /// An operating-system signal or a [`RuntimeBuilder::shutdown_on`] future.
    Signal,
    /// [`ShutdownTrigger::shutdown`], [`RuntimeHandle::shutdown`] or a dropped handle.
    Requested,
    /// A critical unit returned successfully.
    UnitExited {
        /// The unit's name.
        unit: String,
    },
    /// A critical unit failed, or a restarting unit ran out of restarts.
    UnitFailed {
        /// The unit's name.
        unit: String,
    },
    /// A stage did not finish starting within the start timeout.
    StartupTimeout {
        /// The stage that did not start.
        stage: Stage,
    },
}

impl fmt::Display for ShutdownReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Signal => f.write_str("shutdown signal"),
            Self::Requested => f.write_str("shutdown requested"),
            Self::UnitExited { unit } => write!(f, "critical unit {unit} exited"),
            Self::UnitFailed { unit } => write!(f, "unit {unit} failed"),
            Self::StartupTimeout { stage } => write!(f, "stage {stage} did not start in time"),
        }
    }
}

/// How one unit ended.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum UnitExit {
    /// Returned `Ok`.
    Completed,
    /// Returned an error (or panicked); the error's caller-visible text.
    Failed(String),
    /// Did not stop within its grace period and was aborted.
    Aborted,
    /// Never started, because shutdown began during startup.
    NotStarted,
}

/// The final state of one unit.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct UnitReport {
    /// The unit's name.
    pub name: String,
    /// The unit's stage.
    pub stage: Stage,
    /// How many times it was restarted.
    pub restarts: u32,
    /// How its last run ended.
    pub exit: UnitExit,
}

/// The outcome of a runtime that shut down without a critical failure.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct RunReport {
    /// What started the shutdown.
    pub reason: ShutdownReason,
    /// Every unit, in start order.
    pub units: Vec<UnitReport>,
}

impl RunReport {
    /// The report of the unit called `name`.
    pub fn unit(&self, name: &str) -> Option<&UnitReport> {
        self.units.iter().find(|unit| unit.name == name)
    }
}

/// Shared shutdown state: the request flag, its first reason and the first
/// critical failure.
struct Control {
    requested: CancellationToken,
    reason: Mutex<Option<ShutdownReason>>,
    failure: Mutex<Option<AppError>>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Control {
    fn new() -> Self {
        Self {
            requested: CancellationToken::new(),
            reason: Mutex::new(None),
            failure: Mutex::new(None),
        }
    }

    fn request(&self, reason: ShutdownReason) {
        {
            let mut slot = lock(&self.reason);
            if slot.is_none() {
                let shown = reason.to_string();
                tracing::info!(reason = %shown, "shutdown requested");
                *slot = Some(reason);
            }
        }
        self.requested.cancel();
    }

    fn fail(&self, error: AppError) {
        let mut slot = lock(&self.failure);
        if slot.is_none() {
            *slot = Some(error);
        }
    }
}

/// Requests shutdown of a runtime from anywhere. Cheap to clone.
#[derive(Clone)]
pub struct ShutdownTrigger {
    control: Arc<Control>,
}

impl ShutdownTrigger {
    /// Begin a graceful shutdown. Idempotent.
    pub fn shutdown(&self) {
        self.control.request(ShutdownReason::Requested);
    }

    /// Whether shutdown has begun, for any reason.
    pub fn is_triggered(&self) -> bool {
        self.control.requested.is_cancelled()
    }

    /// Resolve once shutdown has begun, for any reason.
    pub async fn triggered(&self) {
        self.control.requested.cancelled().await;
    }
}

impl fmt::Debug for ShutdownTrigger {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ShutdownTrigger")
            .field("triggered", &self.control.requested.is_cancelled())
            .finish_non_exhaustive()
    }
}

/// A staged, supervised set of units: see [`Runtime::builder`].
pub struct Runtime {
    units: Vec<UnitSpec>,
    settings: Settings,
    signals: bool,
    triggers: Vec<Trigger>,
    health: HealthRegistry,
    control: Arc<Control>,
}

impl fmt::Debug for Runtime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let units: Vec<&str> = self.units.iter().map(|unit| &*unit.name).collect();
        f.debug_struct("Runtime")
            .field("units", &units)
            .field("settings", &self.settings)
            .field("signals", &self.signals)
            .finish_non_exhaustive()
    }
}

/// Configures a [`Runtime`].
pub struct RuntimeBuilder {
    units: Vec<UnitSpec>,
    settings: Settings,
    signals: bool,
    triggers: Vec<Trigger>,
    probes: Vec<Arc<dyn DependencyProbe>>,
    probe_interval: Duration,
    probe_timeout: Duration,
    health: HealthRegistry,
    control: Arc<Control>,
}

impl fmt::Debug for RuntimeBuilder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let units: Vec<&str> = self.units.iter().map(|unit| &*unit.name).collect();
        f.debug_struct("RuntimeBuilder")
            .field("units", &units)
            .field("settings", &self.settings)
            .field("signals", &self.signals)
            .finish_non_exhaustive()
    }
}

impl Runtime {
    /// Start configuring a runtime.
    ///
    /// Defaults: a 30 s start timeout per stage, no shutdown delay, a 10 s
    /// grace period per stage, a 30 s bound on the whole shutdown, restart
    /// counts reset after 60 s of healthy running, `SIGTERM` and `SIGINT`
    /// (Ctrl-C off unix) as shutdown signals, and dependency probes every
    /// 10 s with a 2 s timeout.
    pub fn builder() -> RuntimeBuilder {
        RuntimeBuilder {
            units: Vec::new(),
            settings: Settings {
                start_timeout: Duration::from_secs(30),
                shutdown_delay: Duration::ZERO,
                stage_grace: Duration::from_secs(10),
                shutdown_deadline: Duration::from_secs(30),
                restart_reset_after: Duration::from_secs(60),
            },
            signals: true,
            triggers: Vec::new(),
            probes: Vec::new(),
            probe_interval: Duration::from_secs(10),
            probe_timeout: Duration::from_secs(2),
            health: HealthRegistry::new(),
            control: Arc::new(Control::new()),
        }
    }

    /// The health registry shared by every unit.
    pub fn health(&self) -> &HealthRegistry {
        &self.health
    }

    /// A trigger that requests shutdown of this runtime.
    pub fn shutdown_trigger(&self) -> ShutdownTrigger {
        ShutdownTrigger {
            control: Arc::clone(&self.control),
        }
    }

    /// Start every stage, run until shutdown is requested, then drain.
    ///
    /// Returns the first critical failure (a critical unit's error or panic,
    /// exhausted restarts, or a stage that did not start in time) as `Err`;
    /// otherwise a report of how every unit ended.
    ///
    /// Dropping the returned future stops the runtime on the spot: every
    /// stage's shutdown token fires and every unit still running is aborted.
    pub async fn run(self) -> Result<RunReport, AppError> {
        let mut supervisor = Supervisor::new(self);
        supervisor.start_stages().await;
        let control = Arc::clone(&supervisor.control);
        control.requested.cancelled().await;
        supervisor.finish().await
    }

    /// Start every stage and return once all have started; the runtime then
    /// keeps running in a background task until shutdown.
    ///
    /// If shutdown begins during startup, the runtime drains and this returns
    /// the critical failure, or a `CANCELLED` error when there was none.
    ///
    /// Dropping the returned future (e.g. under a timeout) stops the units
    /// already started, as for [`run`](Self::run).
    pub async fn start(self) -> Result<RuntimeHandle, AppError> {
        let mut supervisor = Supervisor::new(self);
        supervisor.start_stages().await;
        let control = Arc::clone(&supervisor.control);
        if control.requested.is_cancelled() {
            return Err(match supervisor.finish().await {
                Err(failure) => failure,
                Ok(report) => {
                    AppError::cancelled(format!("shutdown began during startup: {}", report.reason))
                }
            });
        }
        let health = supervisor.health.clone();
        let waiting = Arc::clone(&control);
        let join = tokio::spawn(async move {
            waiting.requested.cancelled().await;
            supervisor.finish().await
        });
        Ok(RuntimeHandle {
            trigger: ShutdownTrigger { control },
            health,
            join,
            awaited: false,
        })
    }
}

impl RuntimeBuilder {
    /// Register a unit.
    ///
    /// `factory` is called for every run of the unit (again after each
    /// restart) and returns the unit's future. A long-running unit should
    /// call [`UnitContext::ready`] once it is up and return when
    /// [`UnitContext::shutdown`] fires.
    #[must_use]
    pub fn unit<F, Fut>(
        mut self,
        name: impl Into<String>,
        stage: Stage,
        policy: UnitPolicy,
        mut factory: F,
    ) -> Self
    where
        F: FnMut(UnitContext) -> Fut + Send + 'static,
        Fut: Future<Output = Result<(), AppError>> + Send + 'static,
    {
        self.units.push(UnitSpec {
            name: Arc::from(name.into()),
            stage,
            policy,
            factory: Box::new(move |ctx: UnitContext| -> UnitFuture { Box::pin(factory(ctx)) }),
        });
        self
    }

    /// Longest a stage may take to start (default 30 s). Exceeding it is a
    /// startup failure.
    #[must_use]
    pub fn start_timeout(mut self, timeout: Duration) -> Self {
        self.settings.start_timeout = timeout;
        self
    }

    /// Pause between turning health to not-serving and draining the first
    /// stage (default 0), so load balancers can notice.
    #[must_use]
    pub fn shutdown_delay(mut self, delay: Duration) -> Self {
        self.settings.shutdown_delay = delay;
        self
    }

    /// Longest the units of one stage may take to stop before they are
    /// aborted (default 10 s).
    #[must_use]
    pub fn stage_grace(mut self, grace: Duration) -> Self {
        self.settings.stage_grace = grace;
        self
    }

    /// Bound on the whole shutdown, delay included (default 30 s). Units
    /// still running when it passes are aborted.
    #[must_use]
    pub fn shutdown_deadline(mut self, deadline: Duration) -> Self {
        self.settings.shutdown_deadline = deadline;
        self
    }

    /// How long a restarting unit must keep running before its restart count
    /// and backoff start over (default 60 s).
    ///
    /// A run at least this long counts as healthy, so
    /// [`RestartPolicy::max_restarts`](crate::RestartPolicy::max_restarts)
    /// bounds a crash loop rather than the restarts over the unit's lifetime.
    #[must_use]
    pub fn restart_reset_after(mut self, period: Duration) -> Self {
        self.settings.restart_reset_after = period;
        self
    }

    /// Do not install operating-system signal handlers (for tests and for
    /// embedding in a host that owns the signals).
    #[must_use]
    pub fn without_signals(mut self) -> Self {
        self.signals = false;
        self
    }

    /// Shut down when `trigger` resolves.
    #[must_use]
    pub fn shutdown_on(mut self, trigger: impl Future<Output = ()> + Send + 'static) -> Self {
        self.triggers.push(Box::pin(trigger));
        self
    }

    /// Shut down when a signal-like source reports a delivered signal, e.g.
    /// `tokio::signal::ctrl_c()`.
    ///
    /// An `Err` means the handler could not be installed: it is logged and
    /// the source is ignored from then on, never treated as a signal.
    #[must_use]
    pub fn shutdown_on_signal(
        mut self,
        name: &'static str,
        source: impl Future<Output = io::Result<()>> + Send + 'static,
    ) -> Self {
        self.triggers.push(Box::pin(signal_arrived(name, source)));
        self
    }

    /// Add a dependency probe. Probes run in a best-effort infrastructure
    /// unit named [`PROBE_UNIT`].
    #[must_use]
    pub fn probe(mut self, probe: impl DependencyProbe) -> Self {
        self.probes.push(Arc::new(probe));
        self
    }

    /// How often probes run (default 10 s).
    #[must_use]
    pub fn probe_interval(mut self, interval: Duration) -> Self {
        self.probe_interval = interval;
        self
    }

    /// Longest one probe may take (default 2 s) before it counts as
    /// unreachable.
    #[must_use]
    pub fn probe_timeout(mut self, timeout: Duration) -> Self {
        self.probe_timeout = timeout;
        self
    }

    /// The health registry the runtime and its units will share.
    pub fn health(&self) -> HealthRegistry {
        self.health.clone()
    }

    /// A trigger that requests shutdown of the runtime being built.
    pub fn shutdown_trigger(&self) -> ShutdownTrigger {
        ShutdownTrigger {
            control: Arc::clone(&self.control),
        }
    }

    /// Validate the configuration.
    ///
    /// Fails on an empty or duplicate unit or probe name, an invalid restart
    /// policy, or a zero start timeout, restart reset period, probe interval
    /// or probe timeout.
    pub fn build(mut self) -> Result<Runtime, AppError> {
        let invalid = |message: String| Err(AppError::invalid_argument(message));
        if self.settings.start_timeout.is_zero() {
            return invalid("the stage start timeout must be positive".into());
        }
        if self.settings.restart_reset_after.is_zero() {
            return invalid("the restart reset period must be positive".into());
        }

        if !self.probes.is_empty() {
            if self.probe_interval.is_zero() || self.probe_timeout.is_zero() {
                return invalid("the probe interval and timeout must be positive".into());
            }
            let mut names = BTreeSet::new();
            for probe in &self.probes {
                let name = probe.name();
                if name.is_empty() {
                    return invalid("a dependency probe has an empty name".into());
                }
                if !names.insert(name.to_owned()) {
                    return invalid(format!("dependency probe {name} is registered twice"));
                }
            }
            for probe in &self.probes {
                self.health.register_probe(probe.name(), probe.required());
            }
            let probes: Arc<[Arc<dyn DependencyProbe>]> = std::mem::take(&mut self.probes).into();
            let unit = probe_unit(probes, self.probe_interval, self.probe_timeout);
            self = self.unit(
                PROBE_UNIT,
                Stage::Infrastructure,
                UnitPolicy::BestEffort,
                unit,
            );
        }

        let mut names = BTreeSet::new();
        for unit in &self.units {
            if unit.name.is_empty() {
                return invalid("a unit has an empty name".into());
            }
            if !names.insert(Arc::clone(&unit.name)) {
                return invalid(format!("unit {} is registered twice", unit.name));
            }
            if let UnitPolicy::Restart(policy) = &unit.policy
                && let Err(problem) = policy.validate()
            {
                return invalid(format!("unit {}: {problem}", unit.name));
            }
        }

        Ok(Runtime {
            units: self.units,
            settings: self.settings,
            signals: self.signals,
            triggers: self.triggers,
            health: self.health,
            control: self.control,
        })
    }
}

/// Controls a runtime started with [`Runtime::start`].
///
/// Dropping the handle requests shutdown; the runtime still drains in the
/// background.
pub struct RuntimeHandle {
    trigger: ShutdownTrigger,
    health: HealthRegistry,
    join: JoinHandle<Result<RunReport, AppError>>,
    awaited: bool,
}

impl fmt::Debug for RuntimeHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RuntimeHandle")
            .field("shutting_down", &self.trigger.is_triggered())
            .finish_non_exhaustive()
    }
}

impl RuntimeHandle {
    /// Begin a graceful shutdown. Idempotent.
    pub fn shutdown(&self) {
        self.trigger.shutdown();
    }

    /// Whether shutdown has begun, for any reason.
    pub fn is_shutting_down(&self) -> bool {
        self.trigger.is_triggered()
    }

    /// A trigger for this runtime.
    pub fn trigger(&self) -> ShutdownTrigger {
        self.trigger.clone()
    }

    /// The shared health registry.
    pub fn health(&self) -> &HealthRegistry {
        &self.health
    }

    /// Wait until the runtime has shut down (for any reason) and drained;
    /// the result is as for [`Runtime::run`]. Does not request shutdown.
    ///
    /// Cancel-safe: dropping the returned future drops the handle, which
    /// requests shutdown like any dropped handle.
    pub async fn wait(mut self) -> Result<RunReport, AppError> {
        let outcome = (&mut self.join).await;
        self.awaited = true;
        outcome.unwrap_or_else(|error| Err(AppError::internal(error)))
    }
}

impl Drop for RuntimeHandle {
    fn drop(&mut self) {
        if !self.awaited {
            self.trigger.shutdown();
        }
    }
}

/// A spawned task that is aborted when dropped, so a dropped supervisor
/// never leaves a unit running unsupervised.
struct Task<T>(JoinHandle<T>);

impl<T> Drop for Task<T> {
    fn drop(&mut self) {
        self.0.abort();
    }
}

struct RunningUnit {
    name: Arc<str>,
    stage: Stage,
    restarts: Arc<AtomicU32>,
    ready: watch::Receiver<bool>,
    handle: Task<UnitReport>,
}

struct RunningStage {
    stage: Stage,
    token: CancellationToken,
    /// When the stage's units are aborted; set just before `token` fires.
    stop_by: Arc<OnceLock<Instant>>,
    units: Vec<RunningUnit>,
}

struct Supervisor {
    control: Arc<Control>,
    health: HealthRegistry,
    settings: Settings,
    pending: Vec<UnitSpec>,
    stages: Vec<RunningStage>,
    not_started: Vec<UnitReport>,
    _signals: Task<()>,
}

impl Drop for Supervisor {
    /// Reached early only when `start` or `run` is dropped mid-way: the
    /// units go down with the supervisor instead of running on detached.
    fn drop(&mut self) {
        if !self.control.requested.is_cancelled() {
            self.control.request(ShutdownReason::Requested);
        }
        for stage in &self.stages {
            stage.token.cancel();
        }
    }
}

impl Supervisor {
    /// Take over the runtime and arm its shutdown triggers.
    fn new(runtime: Runtime) -> Self {
        let mut triggers = runtime.triggers;
        if runtime.signals {
            triggers.extend(os_signals());
        }
        let control = Arc::clone(&runtime.control);
        let signals = Task(tokio::spawn(async move {
            tokio::select! {
                () = any_trigger(triggers) => control.request(ShutdownReason::Signal),
                () = control.requested.cancelled() => {}
            }
        }));
        Self {
            control: runtime.control,
            health: runtime.health,
            settings: runtime.settings,
            pending: runtime.units,
            stages: Vec::new(),
            not_started: Vec::new(),
            _signals: signals,
        }
    }

    /// Start stages in order until all are up or shutdown is requested.
    async fn start_stages(&mut self) {
        self.health.publish().await;
        let mut pending = std::mem::take(&mut self.pending);
        for stage in Stage::ALL {
            let (specs, rest): (Vec<_>, Vec<_>) =
                pending.into_iter().partition(|unit| unit.stage == stage);
            pending = rest;
            if specs.is_empty() {
                continue;
            }
            if self.control.requested.is_cancelled() {
                self.not_started
                    .extend(specs.into_iter().map(|spec| UnitReport {
                        name: spec.name.to_string(),
                        stage: spec.stage,
                        restarts: 0,
                        exit: UnitExit::NotStarted,
                    }));
                continue;
            }
            self.start_stage(stage, specs).await;
        }
        if !self.control.requested.is_cancelled() {
            self.health.mark_started().await;
            tracing::info!("all stages started");
        }
    }

    async fn start_stage(&mut self, stage: Stage, specs: Vec<UnitSpec>) {
        let token = CancellationToken::new();
        let stop_by = Arc::new(OnceLock::new());
        let units: Vec<RunningUnit> = specs
            .into_iter()
            .map(|spec| self.spawn_unit(spec, token.clone(), Arc::clone(&stop_by)))
            .collect();
        let signals: Vec<(Arc<str>, watch::Receiver<bool>)> = units
            .iter()
            .map(|unit| (Arc::clone(&unit.name), unit.ready.clone()))
            .collect();
        self.stages.push(RunningStage {
            stage,
            token,
            stop_by,
            units,
        });

        tokio::select! {
            biased;
            () = self.control.requested.cancelled() => {}
            settled = all_ready(signals) => match settled {
                Ok(()) => tracing::debug!(stage = stage.as_str(), "stage started"),
                Err(unit) => {
                    let message = format!("unit {unit} stopped without reporting ready");
                    tracing::error!(stage = stage.as_str(), "{message}");
                    self.control.fail(AppError::new(ErrorCode::Internal, message));
                    self.control.request(ShutdownReason::UnitFailed {
                        unit: unit.to_string(),
                    });
                }
            },
            () = tokio::time::sleep(self.settings.start_timeout) => {
                let waiting: Vec<&str> = self
                    .stages
                    .iter()
                    .rev()
                    .take(1)
                    .flat_map(|running| running.units.iter())
                    .filter(|unit| !*unit.ready.borrow())
                    .map(|unit| &*unit.name)
                    .collect();
                let message = format!(
                    "stage {stage} did not start within {:?}; still waiting on {}",
                    self.settings.start_timeout,
                    waiting.join(", ")
                );
                tracing::error!(stage = stage.as_str(), "{message}");
                self.control.fail(AppError::deadline_exceeded(message));
                self.control.request(ShutdownReason::StartupTimeout { stage });
            }
        }
    }

    fn spawn_unit(
        &self,
        spec: UnitSpec,
        token: CancellationToken,
        stop_by: Arc<OnceLock<Instant>>,
    ) -> RunningUnit {
        let (ready_tx, ready) = watch::channel(false);
        let restarts = Arc::new(AtomicU32::new(0));
        let name = Arc::clone(&spec.name);
        let stage = spec.stage;
        let driver = Driver {
            spec,
            token,
            stop_by,
            control: Arc::clone(&self.control),
            health: self.health.clone(),
            ready: Arc::new(ready_tx),
            restarts: Arc::clone(&restarts),
            settings: self.settings,
        };
        RunningUnit {
            name,
            stage,
            restarts,
            ready,
            handle: Task(tokio::spawn(driver.supervise())),
        }
    }

    /// Drain every started stage in reverse order and produce the outcome.
    async fn finish(mut self) -> Result<RunReport, AppError> {
        let deadline = instant_after(self.settings.shutdown_deadline);
        self.health.set_all_not_serving().await;
        if !self.settings.shutdown_delay.is_zero() {
            tokio::time::sleep_until(deadline.min(instant_after(self.settings.shutdown_delay)))
                .await;
        }

        let mut drained: Vec<Vec<UnitReport>> = Vec::new();
        while let Some(stage) = self.stages.pop() {
            let grace_end = deadline.min(instant_after(self.settings.stage_grace));
            drained.push(drain_stage(stage, grace_end).await);
        }
        let mut units: Vec<UnitReport> = drained.into_iter().rev().flatten().collect();
        units.append(&mut self.not_started);

        let reason = lock(&self.control.reason)
            .clone()
            .unwrap_or(ShutdownReason::Requested);
        let failure = lock(&self.control.failure).take();
        tracing::info!("runtime stopped");
        match failure {
            Some(error) => Err(error),
            None => Ok(RunReport { reason, units }),
        }
    }
}

/// `now + duration`, saturating at a far-future instant instead of
/// overflowing on huge durations.
fn instant_after(duration: Duration) -> Instant {
    const FAR_FUTURE: Duration = Duration::from_hours(100 * 365 * 24);
    let now = Instant::now();
    now.checked_add(duration.min(FAR_FUTURE)).unwrap_or(now)
}

/// Resolve once every unit has reported ready (or settled for good), or
/// with the name of the first whose supervision ended without doing so.
async fn all_ready(signals: Vec<(Arc<str>, watch::Receiver<bool>)>) -> Result<(), Arc<str>> {
    try_join_all(signals.into_iter().map(|(name, mut ready)| async move {
        match ready.wait_for(|ready| *ready).await {
            Ok(_) => Ok(()),
            Err(_) => Err(name),
        }
    }))
    .await
    .map(|_| ())
}

/// The text of a panic payload, when it is one.
pub(crate) fn panic_detail(panic: &(dyn Any + Send)) -> String {
    panic
        .downcast_ref::<&str>()
        .map(|text| (*text).to_owned())
        .or_else(|| panic.downcast_ref::<String>().cloned())
        .unwrap_or_default()
}

/// Cancel one stage and wait for all its units together, aborting those
/// still running at `grace_end`.
async fn drain_stage(stage: RunningStage, grace_end: Instant) -> Vec<UnitReport> {
    tracing::debug!(stage = stage.stage.as_str(), "draining stage");
    // Set before the token fires, so a unit that sees shutdown sees it too.
    let _ = stage.stop_by.set(grace_end);
    stage.token.cancel();
    join_all(stage.units.into_iter().map(|unit| async move {
        let RunningUnit {
            name,
            stage: unit_stage,
            restarts,
            mut handle,
            ..
        } = unit;
        let exit = match tokio::time::timeout_at(grace_end, &mut handle.0).await {
            Ok(Ok(report)) => return report,
            Ok(Err(_)) => UnitExit::Failed("the unit's task ended abnormally".into()),
            Err(_) => {
                drop(handle);
                tracing::warn!(
                    unit = &*name,
                    "unit did not stop within its grace period; aborted"
                );
                UnitExit::Aborted
            }
        };
        UnitReport {
            name: name.to_string(),
            stage: unit_stage,
            restarts: restarts.load(Ordering::Relaxed),
            exit,
        }
    }))
    .await
}

/// Runs one unit under its policy.
struct Driver {
    spec: UnitSpec,
    token: CancellationToken,
    stop_by: Arc<OnceLock<Instant>>,
    control: Arc<Control>,
    health: HealthRegistry,
    ready: Arc<watch::Sender<bool>>,
    restarts: Arc<AtomicU32>,
    settings: Settings,
}

impl Driver {
    /// Drive the unit, turning a panic in its supervision (outside the
    /// unit's own code, e.g. a factory's destructor) into a failure of the
    /// unit under its policy instead of a silently finished task.
    async fn supervise(self) -> UnitReport {
        let name = Arc::clone(&self.spec.name);
        let stage = self.spec.stage;
        let policy = self.spec.policy;
        let control = Arc::clone(&self.control);
        let ready = Arc::clone(&self.ready);
        let restarts = Arc::clone(&self.restarts);
        match AssertUnwindSafe(self.drive()).catch_unwind().await {
            Ok(report) => report,
            Err(panic) => {
                let detail = panic_detail(&*panic);
                tracing::error!(unit = &*name, panic = %detail, "unit supervision panicked");
                let error = AppError::new(
                    ErrorCode::Internal,
                    format!("unit {name} ended unexpectedly"),
                );
                let shown = error.to_string();
                if policy == UnitPolicy::BestEffort {
                    ready.send_replace(true);
                } else {
                    control.fail(error);
                    control.request(ShutdownReason::UnitFailed {
                        unit: name.to_string(),
                    });
                }
                UnitReport {
                    name: name.to_string(),
                    stage,
                    restarts: restarts.load(Ordering::Relaxed),
                    exit: UnitExit::Failed(shown),
                }
            }
        }
    }

    async fn drive(mut self) -> UnitReport {
        // Every restart, for the report and `UnitContext::attempt`.
        let mut restarts = 0_u32;
        // Restarts since the unit last ran healthily, for backoff and the limit.
        let mut streak = 0_u32;
        loop {
            let started = Instant::now();
            let result = self.run_once(restarts).await;
            let policy = self.spec.policy;
            if !matches!(policy, UnitPolicy::Restart(_)) {
                // Gone for good, so it no longer holds up its stage's start.
                self.ready.send_replace(true);
            }

            let name = &*self.spec.name;
            let stopping = self.token.is_cancelled() || self.control.requested.is_cancelled();
            if stopping {
                return match result {
                    Ok(()) => self.finish(restarts, Ok(())),
                    Err(error) => {
                        let shown = error.to_string();
                        tracing::warn!(unit = name, error = %shown, "unit failed while stopping");
                        if policy == UnitPolicy::Critical {
                            self.control.fail(error);
                        }
                        self.report(restarts, UnitExit::Failed(shown))
                    }
                };
            }

            let policy = match policy {
                UnitPolicy::BestEffort => {
                    let shown = result.as_ref().err().map(ToString::to_string);
                    tracing::warn!(unit = name, error = ?shown, "best-effort unit exited");
                    return self.finish(restarts, result);
                }
                UnitPolicy::Critical => return self.critical(restarts, result),
                UnitPolicy::Restart(policy) => policy,
            };
            if started.elapsed() >= self.settings.restart_reset_after {
                streak = 0;
            }
            if policy.max_restarts.is_some_and(|max| streak >= max) {
                let mut error = AppError::unavailable(format!(
                    "unit {name} exited after exhausting its {streak} restarts"
                ));
                if let Err(last) = result {
                    error = error.with_source(last);
                }
                return self.critical(restarts, Err(error));
            }

            let delay = policy.delay(streak);
            let shown = result.as_ref().err().map(ToString::to_string);
            let delay_ms = u64::try_from(delay.as_millis()).unwrap_or(u64::MAX);
            let was_ready = *self.ready.borrow();
            tracing::warn!(
                unit = name,
                error = ?shown,
                delay_ms,
                was_ready,
                "unit exited; restarting"
            );
            streak = streak.saturating_add(1);
            restarts = restarts.saturating_add(1);
            self.restarts.store(restarts, Ordering::Relaxed);
            tokio::select! {
                () = self.token.cancelled() => return self.finish(restarts, result),
                () = self.control.requested.cancelled() => return self.finish(restarts, result),
                () = tokio::time::sleep(delay) => {}
            }
        }
    }

    /// One run of the unit. A panic, in the factory or in the future it
    /// returned, is an `INTERNAL` failure like any other.
    async fn run_once(&mut self, attempt: u32) -> Result<(), AppError> {
        let ctx = UnitContext::new(
            Arc::clone(&self.spec.name),
            self.spec.stage,
            attempt,
            self.token.clone(),
            Arc::clone(&self.ready),
            self.health.clone(),
            StopWindow {
                grace: self.settings.stage_grace,
                by: Arc::clone(&self.stop_by),
            },
        );
        let run = match std::panic::catch_unwind(AssertUnwindSafe(|| (self.spec.factory)(ctx))) {
            Ok(run) => run,
            Err(panic) => return Err(self.panicked(&*panic)),
        };
        match AssertUnwindSafe(run).catch_unwind().await {
            Ok(result) => result,
            Err(panic) => Err(self.panicked(&*panic)),
        }
    }

    fn panicked(&self, panic: &(dyn Any + Send)) -> AppError {
        let name = &*self.spec.name;
        let detail = panic_detail(panic);
        tracing::error!(unit = name, panic = %detail, "unit panicked");
        AppError::new(ErrorCode::Internal, format!("unit {name} panicked"))
    }

    /// A critical exit: shut everything down, failing the run on error.
    fn critical(self, restarts: u32, result: Result<(), AppError>) -> UnitReport {
        let unit = self.spec.name.to_string();
        match result {
            Ok(()) => {
                self.control.request(ShutdownReason::UnitExited { unit });
                self.finish(restarts, Ok(()))
            }
            Err(error) => {
                let shown = error.to_string();
                tracing::error!(unit = %unit, error = %shown, "critical unit failed");
                self.control.fail(error);
                self.control.request(ShutdownReason::UnitFailed { unit });
                self.report(restarts, UnitExit::Failed(shown))
            }
        }
    }

    fn finish(self, restarts: u32, result: Result<(), AppError>) -> UnitReport {
        let exit = match result {
            Ok(()) => UnitExit::Completed,
            Err(error) => UnitExit::Failed(error.to_string()),
        };
        self.report(restarts, exit)
    }

    fn report(self, restarts: u32, exit: UnitExit) -> UnitReport {
        UnitReport {
            name: self.spec.name.to_string(),
            stage: self.spec.stage,
            restarts,
            exit,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn signal(name: &str) -> (watch::Sender<bool>, (Arc<str>, watch::Receiver<bool>)) {
        let (tx, rx) = watch::channel(false);
        (tx, (Arc::from(name), rx))
    }

    #[tokio::test]
    async fn a_stage_is_ready_once_every_unit_is() {
        let (first, first_rx) = signal("first");
        let (second, second_rx) = signal("second");
        first.send_replace(true);
        second.send_replace(true);
        assert_eq!(all_ready(vec![first_rx, second_rx]).await, Ok(()));
        assert_eq!(all_ready(Vec::new()).await, Ok(()));
    }

    #[tokio::test]
    async fn a_unit_gone_without_reporting_ready_is_named() {
        let (ready, ready_rx) = signal("ready");
        let (lost, lost_rx) = signal("lost");
        ready.send_replace(true);
        drop(lost);
        let error = all_ready(vec![ready_rx, lost_rx]).await.unwrap_err();
        assert_eq!(&*error, "lost");
    }

    #[test]
    fn panic_payloads_are_read_when_they_are_text() {
        let literal: Box<dyn Any + Send> = Box::new("boom");
        let owned: Box<dyn Any + Send> = Box::new(String::from("bang"));
        let other: Box<dyn Any + Send> = Box::new(7_u8);
        assert_eq!(panic_detail(&*literal), "boom");
        assert_eq!(panic_detail(&*owned), "bang");
        assert_eq!(panic_detail(&*other), "");
    }

    #[tokio::test(start_paused = true)]
    async fn a_draining_unit_learns_when_it_is_aborted() {
        let (seen_tx, seen_rx) = tokio::sync::oneshot::channel();
        let mut seen_tx = Some(seen_tx);
        let handle = Runtime::builder()
            .without_signals()
            .stage_grace(Duration::from_secs(10))
            .shutdown_deadline(Duration::from_secs(3))
            .unit(
                "worker",
                Stage::Workers,
                UnitPolicy::Critical,
                move |ctx: UnitContext| {
                    let seen_tx = seen_tx.take();
                    async move {
                        let before = ctx.stop_deadline();
                        ctx.ready();
                        ctx.shutdown().cancelled().await;
                        if let Some(seen_tx) = seen_tx {
                            seen_tx.send((before, ctx.stop_deadline())).unwrap();
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
        let requested = Instant::now();
        handle.shutdown();
        let (before, during) = seen_rx.await.unwrap();
        assert_eq!(before, None);
        assert_eq!(during, Some(requested + Duration::from_secs(3)));
        handle.wait().await.unwrap();
    }

    #[tokio::test]
    async fn a_dropped_task_is_aborted() {
        let (guard_tx, guard_rx) = tokio::sync::oneshot::channel::<()>();
        let task = Task(tokio::spawn(async move {
            let _guard = guard_tx;
            std::future::pending::<()>().await;
        }));
        drop(task);
        assert!(guard_rx.await.is_err(), "the task's future was dropped");
    }
}
