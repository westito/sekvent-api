//! Test container hygiene around gate and coverage runs.
//!
//! Before a run, stale containers of the configured label namespace are swept
//! and a fresh run id is exported; after it, success or failure, that run's
//! containers are swept. Selection is always by label through
//! `sekvent-testing`, never by image.

use std::time::{Duration, SystemTime};

use sekvent_testing::{Harness, HarnessError, RUN_ID_ENV, SweepReport};

use crate::config::HarnessConfig;

/// Exported next to the run id so tools can find the configured namespace.
pub const HARNESS_NAMESPACE_ENV: &str = "SEKVENT_HARNESS_NAMESPACE";

/// Removes harness containers. [`DockerSweeper`] is the real one.
pub trait Sweeper {
    /// Remove the containers of run `run_id`.
    fn sweep_run(&self, harness: &Harness, run_id: &str) -> Result<SweepReport, HarnessError>;
    /// Remove other runs' containers older than `older_than`.
    fn sweep_stale(
        &self,
        harness: &Harness,
        older_than: Duration,
    ) -> Result<SweepReport, HarnessError>;
    /// Remove every container of the namespace that carries a creation label.
    fn sweep_all(&self, harness: &Harness) -> Result<SweepReport, HarnessError>;
}

/// Sweeps through the Docker CLI; a no-op when `docker` is not installed.
#[derive(Debug, Clone, Copy, Default)]
pub struct DockerSweeper;

fn docker_available() -> bool {
    which::which("docker").is_ok()
}

/// Run `sweep` only when Docker is installed; nothing to remove otherwise.
fn when_available(
    available: bool,
    sweep: impl FnOnce() -> Result<SweepReport, HarnessError>,
) -> Result<SweepReport, HarnessError> {
    if available {
        sweep()
    } else {
        Ok(SweepReport::default())
    }
}

impl Sweeper for DockerSweeper {
    fn sweep_run(&self, harness: &Harness, run_id: &str) -> Result<SweepReport, HarnessError> {
        when_available(docker_available(), || harness.sweep_run(run_id))
    }

    fn sweep_stale(
        &self,
        harness: &Harness,
        older_than: Duration,
    ) -> Result<SweepReport, HarnessError> {
        when_available(docker_available(), || harness.sweep_stale(older_than))
    }

    fn sweep_all(&self, harness: &Harness) -> Result<SweepReport, HarnessError> {
        // A zero age still requires `age > 0`; moving "now" forward also
        // catches containers created within the current second.
        when_available(docker_available(), || {
            harness.sweep_stale_at(Duration::ZERO, SystemTime::now() + Duration::from_secs(1))
        })
    }
}

/// One gate or coverage run's harness identity.
#[derive(Debug, Clone)]
pub struct HarnessSession {
    harness: Harness,
}

impl HarnessSession {
    /// Generate a run id under the configured namespace and sweep stale
    /// containers. Sweep failures are warnings: a machine without Docker
    /// still runs the gate.
    pub fn start(config: &HarnessConfig, sweeper: &dyn Sweeper) -> Result<Self, HarnessError> {
        let harness = Harness::with_run_id(&config.label_namespace, None)?;
        let session = Self { harness };
        match config.stale_after() {
            Ok(older_than) => report("stale", sweeper.sweep_stale(&session.harness, older_than)),
            Err(error) => eprintln!("warning: harness.stale_after: {error}"),
        }
        Ok(session)
    }

    /// The generated run id.
    pub fn run_id(&self) -> &str {
        self.harness.run_id()
    }

    /// Variables exported to every command of the run.
    pub fn env(&self) -> Vec<(String, String)> {
        vec![
            (RUN_ID_ENV.to_owned(), self.harness.run_id().to_owned()),
            (
                HARNESS_NAMESPACE_ENV.to_owned(),
                self.harness.namespace().to_owned(),
            ),
        ]
    }

    /// Sweep this run's containers.
    pub fn finish(self, sweeper: &dyn Sweeper) {
        report(
            "this run's",
            sweeper.sweep_run(&self.harness, self.harness.run_id()),
        );
    }
}

fn report(what: &str, result: Result<SweepReport, HarnessError>) {
    match result {
        Ok(report) if !report.removed.is_empty() => {
            println!(
                "==> harness: removed {} {what} container(s)",
                report.removed.len()
            );
        }
        Ok(_) => {}
        Err(error) => eprintln!("warning: harness sweep ({what}) failed: {error}"),
    }
}

/// What `cargo sekvent harness-clean` removes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CleanScope {
    /// One run's containers.
    Run(String),
    /// Containers older than `[harness].stale_after`.
    Stale,
    /// Every container of the namespace; needs explicit confirmation.
    All,
}

/// Run `harness-clean`. Only the configured label namespace is touched.
pub fn clean(
    config: &HarnessConfig,
    sweeper: &dyn Sweeper,
    scope: &CleanScope,
    confirmed: bool,
) -> anyhow::Result<SweepReport> {
    let harness = Harness::with_run_id(&config.label_namespace, None)?;
    let report = match scope {
        CleanScope::Run(run_id) => sweeper.sweep_run(&harness, run_id)?,
        CleanScope::Stale => sweeper.sweep_stale(&harness, config.stale_after()?)?,
        CleanScope::All if !confirmed => {
            anyhow::bail!(
                "--all removes every container labelled `{}`; pass --yes to confirm",
                config.label_namespace
            )
        }
        CleanScope::All => sweeper.sweep_all(&harness)?,
    };
    Ok(report)
}

#[cfg(test)]
pub(crate) mod fake {
    use std::cell::RefCell;

    use super::*;

    /// Records sweeps as `kind:namespace:arg` strings.
    #[derive(Debug, Default)]
    pub(crate) struct FakeSweeper {
        pub(crate) calls: RefCell<Vec<String>>,
    }

    impl Sweeper for FakeSweeper {
        fn sweep_run(&self, harness: &Harness, run_id: &str) -> Result<SweepReport, HarnessError> {
            self.calls
                .borrow_mut()
                .push(format!("run:{}:{run_id}", harness.namespace()));
            Ok(SweepReport {
                removed: vec!["c1".into()],
            })
        }

        fn sweep_stale(
            &self,
            harness: &Harness,
            older_than: Duration,
        ) -> Result<SweepReport, HarnessError> {
            self.calls.borrow_mut().push(format!(
                "stale:{}:{}",
                harness.namespace(),
                older_than.as_secs()
            ));
            Ok(SweepReport::default())
        }

        fn sweep_all(&self, harness: &Harness) -> Result<SweepReport, HarnessError> {
            self.calls
                .borrow_mut()
                .push(format!("all:{}", harness.namespace()));
            Ok(SweepReport::default())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fake::FakeSweeper;
    use super::*;

    fn config() -> HarnessConfig {
        HarnessConfig {
            label_namespace: "com.example.h".into(),
            stale_after: "1h".into(),
            docker_tests: false,
        }
    }

    #[test]
    fn a_session_sweeps_stale_then_its_own_run() {
        let sweeper = FakeSweeper::default();
        let session = HarnessSession::start(&config(), &sweeper).unwrap();
        let run_id = session.run_id().to_owned();
        assert_eq!(run_id.split('-').count(), 3);
        assert_eq!(
            session.env(),
            [
                ("SEKVENT_TEST_RUN_ID".to_owned(), run_id.clone()),
                (
                    "SEKVENT_HARNESS_NAMESPACE".to_owned(),
                    "com.example.h".to_owned()
                ),
            ]
        );
        session.finish(&sweeper);
        assert_eq!(
            *sweeper.calls.borrow(),
            [
                "stale:com.example.h:3600".to_owned(),
                format!("run:com.example.h:{run_id}")
            ]
        );
    }

    #[test]
    fn clean_scopes_map_to_sweeps() {
        let sweeper = FakeSweeper::default();
        let removed = clean(&config(), &sweeper, &CleanScope::Run("r9".into()), false).unwrap();
        assert_eq!(removed.removed, ["c1"]);
        clean(&config(), &sweeper, &CleanScope::Stale, false).unwrap();
        clean(&config(), &sweeper, &CleanScope::All, true).unwrap();
        assert_eq!(
            *sweeper.calls.borrow(),
            [
                "run:com.example.h:r9",
                "stale:com.example.h:3600",
                "all:com.example.h"
            ]
        );
    }

    #[test]
    fn clean_all_needs_confirmation() {
        let sweeper = FakeSweeper::default();
        let error = clean(&config(), &sweeper, &CleanScope::All, false).unwrap_err();
        assert!(error.to_string().contains("--yes"), "{error}");
        assert!(sweeper.calls.borrow().is_empty());
    }

    /// Fails every sweep.
    struct BrokenSweeper;

    fn broken() -> HarnessError {
        HarnessError::Docker("daemon unreachable".into())
    }

    impl Sweeper for BrokenSweeper {
        fn sweep_run(&self, _: &Harness, _: &str) -> Result<SweepReport, HarnessError> {
            Err(broken())
        }

        fn sweep_stale(&self, _: &Harness, _: Duration) -> Result<SweepReport, HarnessError> {
            Err(broken())
        }

        fn sweep_all(&self, _: &Harness) -> Result<SweepReport, HarnessError> {
            Err(broken())
        }
    }

    #[test]
    fn sweep_failures_do_not_stop_a_session() {
        let session = HarnessSession::start(&config(), &BrokenSweeper).unwrap();
        session.finish(&BrokenSweeper);
        assert!(clean(&config(), &BrokenSweeper, &CleanScope::Stale, false).is_err());
        assert!(clean(&config(), &BrokenSweeper, &CleanScope::All, true).is_err());
    }

    #[test]
    fn a_bad_stale_age_skips_the_stale_sweep() {
        let config = HarnessConfig {
            stale_after: "soon".into(),
            ..config()
        };
        let sweeper = FakeSweeper::default();
        let session = HarnessSession::start(&config, &sweeper).unwrap();
        assert!(sweeper.calls.borrow().is_empty());
        session.finish(&sweeper);
        assert_eq!(sweeper.calls.borrow().len(), 1);
        assert!(clean(&config, &sweeper, &CleanScope::Stale, false).is_err());
    }

    #[test]
    fn a_bad_namespace_is_rejected_up_front() {
        let config = HarnessConfig {
            label_namespace: "bad namespace".into(),
            ..config()
        };
        let sweeper = FakeSweeper::default();
        assert!(HarnessSession::start(&config, &sweeper).is_err());
        assert!(clean(&config, &sweeper, &CleanScope::Stale, false).is_err());
        assert!(sweeper.calls.borrow().is_empty());
    }

    #[test]
    fn sweeps_run_only_with_docker_installed() {
        let removed = SweepReport {
            removed: vec!["c1".into()],
        };
        let skipped = when_available(false, || panic!("must not sweep")).unwrap();
        assert!(skipped.removed.is_empty());
        let swept = when_available(true, || Ok(removed.clone())).unwrap();
        assert_eq!(swept, removed);
    }
}
