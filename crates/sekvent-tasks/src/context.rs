//! What every task needs: the project, the environment snapshot and the
//! side-effecting collaborators.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};

use crate::config::{Config, Project};
use crate::harness::Sweeper;
use crate::location::{EnvMap, Location, decide_location};
use crate::process::Runner;

/// A loaded project plus the collaborators tasks run through.
pub struct Context<'a> {
    /// The project root (where `sekvent.toml` lives).
    pub root: PathBuf,
    /// The validated configuration.
    pub config: Config,
    /// Environment snapshot used for every decision.
    pub env: EnvMap,
    /// Spawns processes.
    pub runner: &'a dyn Runner,
    /// Removes harness containers.
    pub sweeper: &'a dyn Sweeper,
    /// Set once Ctrl-C was pressed.
    pub interrupted: Arc<AtomicBool>,
}

impl<'a> Context<'a> {
    /// A context for `project`.
    pub fn new(
        project: Project,
        env: EnvMap,
        runner: &'a dyn Runner,
        sweeper: &'a dyn Sweeper,
        interrupted: Arc<AtomicBool>,
    ) -> Self {
        Self {
            root: project.root,
            config: project.config,
            env,
            runner,
            sweeper,
            interrupted,
        }
    }

    /// Where compiling commands run for this project and environment.
    pub fn location(&self) -> Location {
        decide_location(&self.env, &self.config.remote)
    }

    /// Ctrl-C was pressed.
    pub fn is_interrupted(&self) -> bool {
        self.interrupted.load(Ordering::SeqCst)
    }
}

/// Install a process-wide Ctrl-C handler that only records the interrupt, so
/// a running task can stop at the next step boundary and still clean up.
///
/// Children in the foreground process group receive the signal themselves.
/// Repeated calls return the same flag.
pub fn install_interrupt_handler() -> Arc<AtomicBool> {
    static FLAG: OnceLock<Arc<AtomicBool>> = OnceLock::new();
    FLAG.get_or_init(|| {
        let flag = Arc::new(AtomicBool::new(false));
        let handler_flag = Arc::clone(&flag);
        if let Err(error) = ctrlc::set_handler(move || handler_flag.store(true, Ordering::SeqCst)) {
            eprintln!("warning: cannot install the Ctrl-C handler: {error}");
        }
        flag
    })
    .clone()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::RemoteMode;
    use crate::harness::fake::FakeSweeper;
    use crate::location::{LocalReason, RRB_CONTAINER_ENV};
    use crate::process::fake::FakeRunner;

    #[test]
    fn the_context_exposes_location_and_interrupts() {
        let runner = FakeRunner::default();
        let sweeper = FakeSweeper::default();
        let interrupted = Arc::new(AtomicBool::new(false));
        let mut config = Config::with_name("orders");
        config.remote.mode = RemoteMode::Rrb;
        let project = Project {
            root: "/work".into(),
            config,
        };
        let mut ctx = Context::new(
            project,
            EnvMap::new(),
            &runner,
            &sweeper,
            Arc::clone(&interrupted),
        );
        assert_eq!(ctx.root, PathBuf::from("/work"));
        assert_eq!(ctx.location(), Location::Remote);
        ctx.env.insert(RRB_CONTAINER_ENV.into(), "1".into());
        assert_eq!(ctx.location(), Location::Local(LocalReason::InsideBuilder));

        assert!(!ctx.is_interrupted());
        interrupted.store(true, Ordering::SeqCst);
        assert!(ctx.is_interrupted());
    }
}
