//! Routing: which commands compile (and so may be forwarded to the remote
//! builder), custom `[tasks]`, reserved names and the `xtask` fallback.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, bail};

use crate::config::{RemoteConfig, TaskConfig};
use crate::context::Context;
use crate::location::Location;
use crate::metadata::Metadata;
use crate::process::Cmd;

/// The `[run.commands]` entry compiling commands are forwarded to.
pub const REMOTE_COMMAND: &str = crate::remote_build::COMMAND_NAME;

/// Subcommand names held for the component model.
pub const RESERVED: [&str; 5] = ["component", "contract", "extract", "queue", "schedule"];

/// Exit code of a reserved subcommand.
pub const RESERVED_EXIT: i32 = 2;

/// `name` is held for the component model.
pub fn is_reserved(name: &str) -> bool {
    RESERVED.contains(&name)
}

/// The message printed for a reserved subcommand.
pub fn reserved_message(name: &str) -> String {
    format!("`{name}` is reserved for the sekvent component model; not available yet")
}

/// What to do with a subcommand the CLI does not know.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum External {
    /// A reserved name: print the notice and exit 2.
    Reserved,
    /// Forward to the workspace's `xtask` package.
    Xtask,
    /// Print the help.
    Help,
}

/// Classify an unknown subcommand. Reserved names never reach `xtask`.
pub fn classify_external(name: &str, has_xtask: bool) -> External {
    if is_reserved(name) {
        External::Reserved
    } else if has_xtask {
        External::Xtask
    } else {
        External::Help
    }
}

/// The workspace has a member package named `xtask`.
pub fn has_xtask(meta: &Metadata) -> bool {
    meta.member("xtask").is_some()
}

/// `cargo run -q -p xtask -- <args>` in `root`.
pub fn xtask_command(root: &Path, args: &[String]) -> Cmd {
    Cmd::new("cargo")
        .args(["run", "-q", "-p", "xtask", "--"])
        .args(args.iter().cloned())
        .cwd(root)
}

/// The command of a custom task with `args` appended.
pub fn task_command(root: &Path, task: &TaskConfig, args: &[String]) -> Option<Cmd> {
    Cmd::from_argv(&task.run).map(|cmd| cmd.args(args.iter().cloned()).cwd(root))
}

/// One line per custom task.
pub fn list_tasks(tasks: &std::collections::BTreeMap<String, TaskConfig>) -> String {
    if tasks.is_empty() {
        return "no [tasks] in sekvent.toml\n".to_owned();
    }
    let width = tasks.keys().map(String::len).max().unwrap_or(0);
    let mut out = String::new();
    for (name, task) in tasks {
        let remote = if task.remote { " (remote)" } else { "" };
        let _ = writeln!(
            out,
            "{name:<width$}  {}{remote}",
            task.description.as_deref().unwrap_or("")
        );
    }
    out
}

/// Remove the `sekvent` that cargo passes as the first argument when the
/// binary runs as `cargo sekvent …`.
pub fn normalize_args(mut argv: Vec<String>) -> Vec<String> {
    if argv.get(1).is_some_and(|arg| arg == "sekvent") {
        argv.remove(1);
    }
    argv
}

/// The `rrb` executable: the configured path (with `~` expanded) or, for a
/// bare name, a `PATH` lookup.
pub fn resolve_rrb(remote: &RemoteConfig, home: Option<&Path>) -> anyhow::Result<PathBuf> {
    let path = remote.rrb_path(home);
    if path.components().count() == 1 && !remote.rrb.contains('/') {
        return which::which(&path).with_context(|| {
            format!(
                "`{}` is not on PATH; remote builds need rrb (set [remote].rrb)",
                remote.rrb
            )
        });
    }
    if !path.is_file() {
        bail!(
            "rrb not found at {}; remote builds need it (set [remote].rrb). \
             There is no local fallback: compiling commands never run on this machine",
            path.display()
        );
    }
    Ok(path)
}

/// `rrb run sekvent <args>` in `root`.
pub fn remote_command(rrb: &Path, root: &Path, args: &[String]) -> Cmd {
    Cmd::new(rrb.display().to_string())
        .args(["run", REMOTE_COMMAND])
        .args(args.iter().cloned())
        .cwd(root)
}

/// Hand `args` to the remote builder. On Unix this replaces the process, so
/// rrb's exit code (and its `RTX NOT AVAILABLE` failure) is the result.
pub fn forward_remote(ctx: &Context<'_>, args: &[String]) -> anyhow::Result<i32> {
    let rrb = resolve_rrb(&ctx.config.remote, dirs::home_dir().as_deref())?;
    let cmd = remote_command(&rrb, &ctx.root, args);
    ctx.runner
        .exec(&cmd)
        .with_context(|| format!("cannot hand over to rrb ({cmd}); there is no local fallback"))
}

/// Run a compiling command: forwarded to the builder when the location is
/// remote, else `local` runs here.
pub fn compile_or_forward(
    ctx: &Context<'_>,
    args: &[String],
    local: impl FnOnce(&Context<'_>) -> anyhow::Result<i32>,
) -> anyhow::Result<i32> {
    match ctx.location() {
        Location::Remote => forward_remote(ctx, args),
        Location::Local(_) => local(ctx),
    }
}

/// `cargo sekvent run <task> [args]`.
pub fn run_task(
    ctx: &Context<'_>,
    original: &[String],
    name: &str,
    args: &[String],
) -> anyhow::Result<i32> {
    let task = ctx.config.tasks.get(name).with_context(|| {
        format!(
            "no task `{name}` in sekvent.toml; known tasks:\n{}",
            list_tasks(&ctx.config.tasks)
        )
    })?;
    let cmd = task_command(&ctx.root, task, args).context("the task has an empty `run`")?;
    let local = |ctx: &Context<'_>| -> anyhow::Result<i32> {
        println!("==> run: {name}");
        Ok(ctx.runner.status(&cmd)?)
    };
    if task.remote {
        compile_or_forward(ctx, original, local)
    } else {
        local(ctx)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;

    use super::*;
    use crate::config::{Config, Project, RemoteMode};
    use crate::fixtures::METADATA;
    use crate::harness::fake::FakeSweeper;
    use crate::location::EnvMap;
    use crate::process::fake::FakeRunner;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|arg| (*arg).to_owned()).collect()
    }

    fn context<'a>(
        config: Config,
        env: &[(&str, &str)],
        runner: &'a FakeRunner,
        sweeper: &'a FakeSweeper,
    ) -> Context<'a> {
        let env: EnvMap = env
            .iter()
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
            .collect();
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

    fn with_rrb(dir: &Path) -> Config {
        let rrb = dir.join("rrb");
        std::fs::write(&rrb, "#!/bin/sh\n").unwrap();
        let mut config = Config::with_name("orders");
        config.remote.rrb = rrb.display().to_string();
        config
    }

    #[test]
    fn reserved_names_never_reach_xtask() {
        for name in RESERVED {
            assert_eq!(classify_external(name, true), External::Reserved);
            assert!(reserved_message(name).contains("reserved for the sekvent component model"));
        }
        assert_eq!(classify_external("release", true), External::Xtask);
        assert_eq!(classify_external("release", false), External::Help);
    }

    #[test]
    fn xtask_is_detected_among_members() {
        let meta = Metadata::parse(METADATA).unwrap();
        assert!(!has_xtask(&meta));
        let with_xtask = METADATA.replace("\"name\": \"vendored\"", "\"name\": \"xtask\"");
        assert!(has_xtask(&Metadata::parse(&with_xtask).unwrap()));
        assert_eq!(
            xtask_command(Path::new("/w"), &args(&["dist", "--fast"])).to_string(),
            "cargo run -q -p xtask -- dist --fast"
        );
    }

    #[test]
    fn cargo_subcommand_argv_is_normalized() {
        assert_eq!(
            normalize_args(args(&["cargo-sekvent", "sekvent", "gate"])),
            ["cargo-sekvent", "gate"]
        );
        assert_eq!(
            normalize_args(args(&["cargo-sekvent", "gate"])),
            ["cargo-sekvent", "gate"]
        );
        assert_eq!(normalize_args(args(&["cargo-sekvent"])), ["cargo-sekvent"]);
    }

    #[test]
    fn remote_location_execs_rrb_with_the_original_args() {
        let dir = tempfile::tempdir().unwrap();
        let config = with_rrb(dir.path());
        let runner = FakeRunner::default();
        let sweeper = FakeSweeper::default();
        let ctx = context(config, &[], &runner, &sweeper);
        let code = compile_or_forward(&ctx, &args(&["test", "--", "--nocapture"]), |_| {
            panic!("must not run locally")
        })
        .unwrap();
        assert_eq!(code, 0);
        let calls = runner.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(
            calls[0].args,
            ["run", "sekvent", "test", "--", "--nocapture"]
        );
        assert_eq!(
            calls[0].program,
            dir.path().join("rrb").display().to_string()
        );
        assert_eq!(calls[0].cwd.as_deref(), Some(Path::new("/work")));
    }

    #[test]
    fn local_locations_run_in_place() {
        let dir = tempfile::tempdir().unwrap();
        for env in [
            &[("RRB_CONTAINER", "1")][..],
            &[("CI", "true")][..],
            &[("SEKVENT_LOCAL", "1")][..],
        ] {
            let runner = FakeRunner::default();
            let sweeper = FakeSweeper::default();
            let ctx = context(with_rrb(dir.path()), env, &runner, &sweeper);
            assert_eq!(
                compile_or_forward(&ctx, &args(&["gate"]), |_| Ok(7)).unwrap(),
                7
            );
            assert!(runner.calls().is_empty());
        }
        let mut config = Config::with_name("orders");
        config.remote.mode = RemoteMode::Local;
        let runner = FakeRunner::default();
        let sweeper = FakeSweeper::default();
        let ctx = context(config, &[], &runner, &sweeper);
        assert_eq!(
            compile_or_forward(&ctx, &args(&["gate"]), |_| Ok(3)).unwrap(),
            3
        );
    }

    #[test]
    fn a_missing_rrb_is_an_error_never_a_local_build() {
        let mut config = Config::with_name("orders");
        config.remote.rrb = "/nonexistent/rrb".into();
        let runner = FakeRunner::default();
        let sweeper = FakeSweeper::default();
        let ctx = context(config, &[], &runner, &sweeper);
        let error =
            compile_or_forward(&ctx, &args(&["gate"]), |_| panic!("no fallback")).unwrap_err();
        assert!(error.to_string().contains("no local fallback"), "{error}");
        assert!(runner.calls().is_empty());

        let bare = RemoteConfig {
            rrb: "sekvent-no-such-rrb-binary".into(),
            ..RemoteConfig::default()
        };
        assert!(resolve_rrb(&bare, None).is_err());
    }

    #[test]
    fn custom_tasks_run_locally_or_forward() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = with_rrb(dir.path());
        config.tasks.insert(
            "seed".into(),
            TaskConfig {
                run: args(&["cargo", "run", "--bin", "seed", "--"]),
                remote: true,
                description: Some("Seed".into()),
            },
        );
        config.tasks.insert(
            "lint-docs".into(),
            TaskConfig {
                run: args(&["npx", "markdownlint", "."]),
                remote: false,
                description: None,
            },
        );
        let listing = list_tasks(&config.tasks);
        assert_eq!(listing, "lint-docs  \nseed       Seed (remote)\n");

        let runner = FakeRunner::default();
        let sweeper = FakeSweeper::default();
        let ctx = context(config.clone(), &[], &runner, &sweeper);
        run_task(
            &ctx,
            &args(&["run", "seed", "--rows", "5"]),
            "seed",
            &args(&["--rows", "5"]),
        )
        .unwrap();
        run_task(&ctx, &args(&["run", "lint-docs"]), "lint-docs", &[]).unwrap();
        let lines = runner.lines();
        assert!(
            lines[0].ends_with("rrb run sekvent run seed --rows 5"),
            "{lines:?}"
        );
        assert_eq!(lines[1], "npx markdownlint .");
        assert!(run_task(&ctx, &[], "nope", &[]).is_err());

        let runner = FakeRunner::default();
        let ctx = context(config, &[("RRB_CONTAINER", "1")], &runner, &sweeper);
        run_task(&ctx, &[], "seed", &args(&["--rows", "5"])).unwrap();
        assert_eq!(runner.lines(), ["cargo run --bin seed -- --rows 5"]);
        assert_eq!(
            list_tasks(&std::collections::BTreeMap::new()),
            "no [tasks] in sekvent.toml\n"
        );
    }
}
