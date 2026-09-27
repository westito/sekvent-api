use std::sync::Arc;
use std::time::Duration;

use futures::future::{BoxFuture, join_all};
use sekvent_error::AppError;

use crate::UnitContext;

/// A check of one external dependency (database, broker, upstream service)
/// that feeds readiness.
///
/// The runtime polls every probe on an interval with a per-probe timeout; a
/// probe that does not answer in time counts as
/// [`ProbeFailure::Unreachable`].
pub trait DependencyProbe: Send + Sync + 'static {
    /// Stable name shown in full health output, e.g. `postgres`.
    fn name(&self) -> &str;

    /// Whether readiness depends on this probe. An optional probe is
    /// reported but never makes the service unready.
    fn required(&self) -> bool {
        true
    }

    /// Check the dependency once.
    fn probe(&self) -> BoxFuture<'_, ProbeStatus>;
}

/// The outcome of one probe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbeStatus {
    /// The dependency answered and accepted us.
    Up,
    /// The dependency is not usable.
    Down(ProbeFailure),
}

impl ProbeStatus {
    /// Whether the dependency is usable.
    pub fn is_up(self) -> bool {
        matches!(self, Self::Up)
    }

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Up => "up",
            Self::Down(_) => "down",
        }
    }
}

/// Why a dependency is down.
///
/// The detail is a static, caller-safe description chosen by the probe's
/// author. It must never carry upstream response text, which may contain
/// credentials or internal addresses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ProbeFailure {
    /// The dependency answered but refused us, e.g. bad credentials.
    Rejected(&'static str),
    /// The dependency could not be reached or did not answer in time.
    Unreachable(&'static str),
}

impl ProbeFailure {
    /// `rejected` or `unreachable`.
    pub fn kind(self) -> &'static str {
        match self {
            Self::Rejected(_) => "rejected",
            Self::Unreachable(_) => "unreachable",
        }
    }

    /// The caller-safe detail.
    pub fn detail(self) -> &'static str {
        match self {
            Self::Rejected(detail) | Self::Unreachable(detail) => detail,
        }
    }
}

/// Detail recorded for a probe that exceeded its timeout.
pub(crate) const TIMED_OUT: &str = "probe timed out";

/// The unit that polls every probe and records the results.
///
/// It reports ready after the first round, so the infrastructure stage (and
/// therefore readiness) starts from real probe results, not from "unknown".
pub(crate) fn probe_unit(
    probes: Arc<[Arc<dyn DependencyProbe>]>,
    interval: Duration,
    timeout: Duration,
) -> impl FnMut(UnitContext) -> BoxFuture<'static, Result<(), AppError>> + Send + 'static {
    move |ctx: UnitContext| -> BoxFuture<'static, Result<(), AppError>> {
        Box::pin(run_probes(ctx, Arc::clone(&probes), interval, timeout))
    }
}

async fn run_probes(
    ctx: UnitContext,
    probes: Arc<[Arc<dyn DependencyProbe>]>,
    interval: Duration,
    timeout: Duration,
) -> Result<(), AppError> {
    let shutdown = ctx.shutdown();
    loop {
        let round = join_all(probes.iter().map(|probe| async move {
            let status = tokio::time::timeout(timeout, probe.probe())
                .await
                .unwrap_or(ProbeStatus::Down(ProbeFailure::Unreachable(TIMED_OUT)));
            (probe.name(), status)
        }))
        .await;
        for (name, status) in round {
            if ctx.health().record_probe(name, status) {
                log_transition(name, status);
            }
        }
        ctx.health().publish().await;
        ctx.ready();

        tokio::select! {
            () = shutdown.cancelled() => return Ok(()),
            () = tokio::time::sleep(interval) => {}
        }
    }
}

fn log_transition(name: &str, status: ProbeStatus) {
    match status {
        ProbeStatus::Up => tracing::info!(probe = name, "dependency is up"),
        ProbeStatus::Down(failure) => {
            let kind = failure.kind();
            let detail = failure.detail();
            tracing::warn!(probe = name, kind, detail, "dependency is down");
        }
    }
}
