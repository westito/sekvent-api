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

#[cfg(test)]
mod tests {
    use super::*;

    fn context(attempt: u32) -> (UnitContext, CancellationToken, watch::Receiver<bool>) {
        let shutdown = CancellationToken::new();
        let (ready, ready_rx) = watch::channel(false);
        let ctx = UnitContext::new(
            Arc::from("orders-consumer"),
            Stage::Workers,
            attempt,
            shutdown.clone(),
            Arc::new(ready),
            HealthRegistry::new(),
        );
        (ctx, shutdown, ready_rx)
    }

    #[test]
    fn accessors_report_what_the_supervisor_passed_in() {
        let (ctx, _, _) = context(3);
        assert_eq!(ctx.name(), "orders-consumer");
        assert_eq!(ctx.stage(), Stage::Workers);
        assert_eq!(ctx.attempt(), 3);
        assert!(ctx.health().is_live());
        assert!(!ctx.health().is_ready());
    }

    #[test]
    fn shutdown_is_seen_through_the_token_and_every_clone() {
        let (ctx, shutdown, _) = context(0);
        let clone = ctx.clone();
        let token = ctx.shutdown();
        assert!(!ctx.is_shutting_down());
        assert!(!token.is_cancelled());

        shutdown.cancel();

        assert!(ctx.is_shutting_down());
        assert!(clone.is_shutting_down());
        assert!(token.is_cancelled());
        assert!(ctx.shutdown().is_cancelled());
    }

    #[test]
    fn ready_raises_the_shared_flag_and_repeats_harmlessly() {
        let (ctx, _, mut ready_rx) = context(0);
        assert!(!*ready_rx.borrow_and_update());

        ctx.clone().ready();
        assert!(ready_rx.has_changed().unwrap());
        assert!(*ready_rx.borrow_and_update());

        ctx.ready();
        assert!(*ready_rx.borrow());
    }

    #[test]
    fn debug_shows_identity_and_draining_but_not_internals() {
        let (ctx, shutdown, _) = context(2);
        let running = format!("{ctx:?}");
        assert!(running.starts_with("UnitContext {"));
        assert!(running.contains("\"orders-consumer\""));
        assert!(running.contains("Workers"));
        assert!(running.contains("attempt: 2"));
        assert!(running.contains("shutting_down: false"));
        assert!(running.ends_with(".. }"));
        assert!(!running.contains("health"));

        shutdown.cancel();
        assert!(format!("{ctx:?}").contains("shutting_down: true"));
    }
}
