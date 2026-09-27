use std::fmt;
use std::sync::Arc;

use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use crate::{HealthRegistry, Stage};

/// What a running unit gets from the supervisor.
///
/// A fresh context is handed to every run of the unit, restarts included.
/// Clones share the same shutdown token and readiness flag.
#[derive(Clone)]
pub struct UnitContext {
    name: Arc<str>,
    stage: Stage,
    attempt: u32,
    shutdown: CancellationToken,
    ready: Arc<watch::Sender<bool>>,
    health: HealthRegistry,
}

impl UnitContext {
    pub(crate) fn new(
        name: Arc<str>,
        stage: Stage,
        attempt: u32,
        shutdown: CancellationToken,
        ready: Arc<watch::Sender<bool>>,
        health: HealthRegistry,
    ) -> Self {
        Self {
            name,
            stage,
            attempt,
            shutdown,
            ready,
            health,
        }
    }

    /// The unit's registered name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The unit's stage.
    pub fn stage(&self) -> Stage {
        self.stage
    }

    /// How many times the unit was restarted before this run; `0` on the
    /// first run.
    pub fn attempt(&self) -> u32 {
        self.attempt
    }

    /// A token that fires when this unit's stage begins draining. The unit
    /// should then finish its in-flight work and return.
    pub fn shutdown(&self) -> CancellationToken {
        self.shutdown.clone()
    }

    /// Whether this unit's stage is draining.
    pub fn is_shutting_down(&self) -> bool {
        self.shutdown.is_cancelled()
    }

    /// Report that the unit is up. The next stage starts once every unit of
    /// this stage has reported ready (or exited). Calling it again is harmless.
    pub fn ready(&self) {
        self.ready.send_replace(true);
    }

    /// The process-wide health registry.
    pub fn health(&self) -> &HealthRegistry {
        &self.health
    }
}

impl fmt::Debug for UnitContext {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("UnitContext")
            .field("name", &self.name)
            .field("stage", &self.stage)
            .field("attempt", &self.attempt)
            .field("shutting_down", &self.shutdown.is_cancelled())
            .finish_non_exhaustive()
    }
}
