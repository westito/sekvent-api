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
