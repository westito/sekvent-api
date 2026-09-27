//! Executing the gate, its single steps and the coverage gate.
//!
//! These functions run where the compile happens (the builder, CI or an
//! explicitly local machine); forwarding to the builder is decided before
//! they are called.

use std::path::PathBuf;

use anyhow::{Context as _, bail};

use crate::boundaries;
use crate::context::Context;
use crate::coverage::{self, Export};
use crate::harness::HarnessSession;
use crate::metadata::Metadata;
use crate::plan::{self, Selection, Step};
use crate::sdk;

/// Run `steps` in order with a banner each; stop at the first failure and
/// return its exit code, or 130 after Ctrl-C.
pub fn run_steps(
    ctx: &Context<'_>,
    label: &str,
    steps: &[Step],
    meta: Option<&Metadata>,
) -> anyhow::Result<i32> {
    for step in steps {
        println!("==> {label}: {}", step.name());
        let code = match step {
            Step::Run { cmd, .. } => ctx.runner.status(cmd)?,
            Step::Boundaries => {
                let meta = meta.context("the boundary check needs the dependency graph")?;
                boundaries::print_report(&boundaries::check(meta, &ctx.config.gate.boundaries)?)
            }
        };
        if ctx.is_interrupted() {
            eprintln!("==> {label}: interrupted");
            return Ok(130);
        }
        if code != 0 {
            eprintln!("==> {label}: {} failed (exit {code})", step.name());
            return Ok(code);
        }
    }
    Ok(0)
}

/// Run `body` with a harness session: stale containers are swept before it,
/// its run id and namespace are passed in as environment, and the run's
/// containers are swept after it whether it failed or not.
pub fn with_harness<T>(
    ctx: &Context<'_>,
    body: impl FnOnce(&[(String, String)]) -> anyhow::Result<T>,
) -> anyhow::Result<T> {
    let session = HarnessSession::start(&ctx.config.harness, ctx.sweeper)?;
    let env = session.env();
    let result = body(&env);
    session.finish(ctx.sweeper);
    result
}

fn selection(ctx: &Context<'_>, with_deps: bool) -> anyhow::Result<(Metadata, Selection)> {
    let meta = Metadata::load(ctx.runner, &ctx.root, with_deps)?;
    let selection = Selection::new(&meta, &ctx.config.gate)?;
    Ok((meta, selection))
}

/// `cargo sekvent gate`: hooks, fmt, clippy, test, doc and boundaries.
pub fn gate(ctx: &Context<'_>) -> anyhow::Result<i32> {
    if let Some(hint) = sdk::update_hint(ctx.runner, &ctx.root, &ctx.env) {
        println!("{hint}");
    }
    let (meta, selection) = selection(ctx, !ctx.config.gate.boundaries.is_empty())?;
    let code = with_harness(ctx, |harness_env| {
        let steps = plan::gate_steps(&ctx.root, &ctx.config, &selection, &ctx.env, harness_env);
        run_steps(ctx, "gate", &steps, Some(&meta))
    })?;
    if code == 0 {
        println!("==> gate: ok");
    }
    Ok(code)
}

/// A gate step run on its own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SingleStep {
    /// `cargo check`.
    Check,
    /// `cargo clippy`.
    Clippy,
    /// `cargo test`, with arguments for the test binaries.
    Test(Vec<String>),
}

/// `cargo sekvent check|clippy|test`: one step with the gate's selection.
pub fn single(ctx: &Context<'_>, step: &SingleStep) -> anyhow::Result<i32> {
    let (_, selection) = selection(ctx, false)?;
    let gate = &ctx.config.gate;
    match step {
        SingleStep::Check => {
            let cmd = plan::check_command(&ctx.root, &selection, gate);
            run_steps(
                ctx,
                "check",
                &[Step::Run {
                    name: "check".into(),
                    cmd,
                }],
                None,
            )
        }
        SingleStep::Clippy => {
            let cmd = plan::clippy_command(&ctx.root, &selection, gate);
            run_steps(
                ctx,
                "clippy",
                &[Step::Run {
                    name: "clippy".into(),
                    cmd,
                }],
                None,
            )
        }
        SingleStep::Test(extra) => with_harness(ctx, |harness_env| {
            let cmd =
                plan::test_command(&ctx.root, &selection, gate, &ctx.env, extra).envs(harness_env);
            run_steps(
                ctx,
                "test",
                &[Step::Run {
                    name: "test".into(),
                    cmd,
                }],
                None,
            )
        }),
    }
}

/// `cargo sekvent boundaries`: only the boundary check.
pub fn boundaries(ctx: &Context<'_>) -> anyhow::Result<i32> {
    if ctx.config.gate.boundaries.is_empty() {
        println!("boundaries: none configured");
        return Ok(0);
    }
    let meta = Metadata::load(ctx.runner, &ctx.root, true)?;
    let report = boundaries::check(&meta, &ctx.config.gate.boundaries)?;
    Ok(boundaries::print_report(&report))
}

/// Options of `cargo sekvent coverage`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CoverageOptions {
    /// Also write an LCOV report here.
    pub lcov: Option<PathBuf>,
    /// Print uncovered line ranges of this package.
    pub misses: Option<String>,
}

/// `cargo sekvent coverage`: instrumented test run, per-package report and
/// floors.
pub fn coverage(ctx: &Context<'_>, options: &CoverageOptions) -> anyhow::Result<i32> {
    let (meta, selection) = selection(ctx, false)?;
    if let Some(package) = &options.misses
        && !selection.members.contains(package)
    {
        bail!("--misses: `{package}` is not a selected workspace member");
    }
    let regex = coverage::ignore_regex(&ctx.config.coverage);
    let work = tempfile::tempdir().context("cannot create a temporary directory")?;
    let json_path = work.path().join("coverage.json");
    let root = &ctx.root;

    let code = with_harness(ctx, |harness_env| {
        let mut steps = plan::hook_steps(
            root,
            "pre_coverage",
            &ctx.config.hooks.pre_coverage,
            harness_env,
        );
        steps.push(Step::Run {
            name: "clean".into(),
            cmd: plan::coverage_clean_command(root),
        });
        steps.push(Step::Run {
            name: "run".into(),
            cmd: plan::coverage_run_command(root, &selection, &ctx.config.gate, &ctx.env)
                .envs(harness_env),
        });
        steps.push(Step::Run {
            name: "report".into(),
            cmd: plan::coverage_json_command(root, &regex, &json_path, options.misses.is_none()),
        });
        if let Some(lcov) = &options.lcov {
            steps.push(Step::Run {
                name: "lcov".into(),
                cmd: plan::coverage_lcov_command(root, &regex, lcov),
            });
        }
        run_steps(ctx, "coverage", &steps, None)
    })?;
    if code != 0 {
        return Ok(code);
    }

    let json = std::fs::read_to_string(&json_path)
        .with_context(|| format!("cannot read {}", json_path.display()))?;
    let export = Export::parse(&json).context("cannot parse the llvm-cov JSON export")?;
    let rows = coverage::aggregate(&export, &meta, &selection, &ctx.config.coverage);
    print!("{}", coverage::render_table(&rows));
    if let Some(package) = &options.misses {
        println!("==> coverage: missed lines in {package}");
        print!(
            "{}",
            coverage::render_misses(&coverage::misses_for_package(&export, &meta, package))
        );
    }

    let failed: Vec<&str> = rows
        .iter()
        .filter(|row| !row.passes())
        .map(|row| row.package.as_str())
        .collect();
    let post = plan::hook_steps(root, "post_coverage", &ctx.config.hooks.post_coverage, &[]);
    let post_code = run_steps(ctx, "coverage", &post, None)?;
    if !failed.is_empty() {
        eprintln!("==> coverage: below the floor: {}", failed.join(", "));
        return Ok(1);
    }
    if post_code == 0 {
        println!("==> coverage: ok");
    }
    Ok(post_code)
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;

    use super::*;
    use crate::config::{Boundary, Config, Project};
    use crate::fixtures::METADATA;
    use crate::harness::fake::FakeSweeper;
    use crate::location::EnvMap;
    use crate::process::fake::FakeRunner;

    fn context<'a>(
        config: Config,
        runner: &'a FakeRunner,
        sweeper: &'a FakeSweeper,
    ) -> Context<'a> {
        let mut env = EnvMap::new();
        env.insert("SEKVENT_NO_UPDATE_CHECK".into(), "1".into());
        Context::new(
            Project {
                root: "/work".into(),
                config,
            },
            env,
            runner,
            sweeper,
            Arc::new(AtomicBool::new(false)),
        )
    }

    fn config() -> Config {
        let mut config = Config::with_name("orders");
        config.gate.exclude = vec!["vendored".into()];
        config
    }

    #[test]
    fn the_gate_runs_every_step_and_sweeps_its_run() {
        let runner = FakeRunner::default();
        runner.push_output(METADATA);
        let sweeper = FakeSweeper::default();
        let ctx = context(config(), &runner, &sweeper);
        assert_eq!(gate(&ctx).unwrap(), 0);
        let lines = runner.lines();
        assert_eq!(lines[0], "cargo metadata --format-version 1 --no-deps");
        assert!(
            lines[1].starts_with("cargo fmt --check -p orders-api"),
            "{lines:?}"
        );
        assert!(lines[2].starts_with("cargo clippy"), "{lines:?}");
        assert!(lines[3].starts_with("cargo test"), "{lines:?}");
        assert_eq!(lines.len(), 4);
        let run_id = runner.calls()[3]
            .env_value("SEKVENT_TEST_RUN_ID")
            .unwrap()
            .to_owned();
        let calls = sweeper.calls.borrow();
        assert_eq!(calls.len(), 2);
        assert!(calls[0].starts_with("stale:io.sekvent.harness:"));
        assert_eq!(calls[1], format!("run:io.sekvent.harness:{run_id}"));
    }

    #[test]
    fn the_first_failure_stops_the_gate_but_still_sweeps() {
        let runner = FakeRunner::with_codes(&[0, 101]);
        runner.push_output(METADATA);
        let sweeper = FakeSweeper::default();
        let ctx = context(config(), &runner, &sweeper);
        assert_eq!(gate(&ctx).unwrap(), 101);
        assert_eq!(runner.lines().len(), 3);
        assert_eq!(sweeper.calls.borrow().len(), 2);
    }

    #[test]
    fn an_interrupt_stops_after_the_current_step() {
        let runner = FakeRunner::default();
        runner.push_output(METADATA);
        let sweeper = FakeSweeper::default();
        let ctx = context(config(), &runner, &sweeper);
        ctx.interrupted
            .store(true, std::sync::atomic::Ordering::SeqCst);
        assert_eq!(gate(&ctx).unwrap(), 130);
        assert_eq!(runner.lines().len(), 2);
        assert_eq!(sweeper.calls.borrow().len(), 2);
    }

    #[test]
    fn boundaries_load_the_full_graph_and_fail_on_violations() {
        let mut config = config();
        config.gate.boundaries = vec![Boundary {
            from: "orders-api".into(),
            to: "orders-db".into(),
        }];
        let runner = FakeRunner::default();
        runner.push_output(METADATA);
        let sweeper = FakeSweeper::default();
        let ctx = context(config.clone(), &runner, &sweeper);
        assert_eq!(gate(&ctx).unwrap(), 1);
        assert_eq!(runner.lines()[0], "cargo metadata --format-version 1");

        let runner = FakeRunner::default();
        runner.push_output(METADATA);
        let ctx = context(config, &runner, &sweeper);
        assert_eq!(boundaries(&ctx).unwrap(), 1);
    }

    #[test]
    fn single_steps_use_the_gate_selection() {
        for (step, expected) in [
            (
                SingleStep::Check,
                "cargo check --workspace --exclude vendored --all-targets --all-features --locked",
            ),
            (
                SingleStep::Clippy,
                "cargo clippy --workspace --exclude vendored --no-deps --all-targets \
                 --all-features --locked -- -D warnings",
            ),
            (
                SingleStep::Test(vec!["--nocapture".into()]),
                "cargo test --workspace --exclude vendored --all-features --locked -- --nocapture",
            ),
        ] {
            let runner = FakeRunner::default();
            runner.push_output(METADATA);
            let sweeper = FakeSweeper::default();
            let ctx = context(config(), &runner, &sweeper);
            assert_eq!(single(&ctx, &step).unwrap(), 0);
            assert_eq!(runner.lines()[1], expected);
        }
    }

    #[test]
    fn coverage_fails_when_the_report_step_fails_and_rejects_unknown_packages() {
        let runner = FakeRunner::with_codes(&[0, 0, 3]);
        runner.push_output(METADATA);
        let sweeper = FakeSweeper::default();
        let ctx = context(config(), &runner, &sweeper);
        let options = CoverageOptions {
            lcov: Some(Path::new("/tmp/lcov.info").to_owned()),
            misses: None,
        };
        assert_eq!(coverage(&ctx, &options).unwrap(), 3);
        let lines = runner.lines();
        assert_eq!(lines[1], "cargo llvm-cov clean --workspace");
        assert!(lines[2].ends_with("--locked --no-report"), "{lines:?}");
        assert!(
            lines[3].starts_with("cargo llvm-cov report --json --summary-only"),
            "{lines:?}"
        );
        assert_eq!(lines.len(), 4);

        let runner = FakeRunner::default();
        runner.push_output(METADATA);
        let ctx = context(config(), &runner, &sweeper);
        let options = CoverageOptions {
            lcov: None,
            misses: Some("vendored".into()),
        };
        assert!(coverage(&ctx, &options).is_err());
    }
}
