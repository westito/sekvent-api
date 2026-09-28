//! `cargo sekvent`: the command line front end of sekvent.
//!
//! Argument parsing only; every task lives in `sekvent-tasks`. Compiling
//! commands are forwarded to the remote builder unless this machine is the
//! place to compile (see `sekvent_tasks::location`).

#![forbid(unsafe_code)]

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::Context as _;
use clap::{Args, CommandFactory, Parser, Subcommand, ValueEnum};
use sekvent_tasks::config::{ConfigError, Project, find_project_root, find_workspace_root};
use sekvent_tasks::contract;
use sekvent_tasks::dispatch::{self, External};
use sekvent_tasks::gate::{self, CoverageOptions, SingleStep};
use sekvent_tasks::harness::{self, CleanScope, DockerSweeper, Sweeper};
use sekvent_tasks::metadata::Metadata;
use sekvent_tasks::scaffold::{self, AGENTS_FILE, CiProvider, NewOptions};
use sekvent_tasks::template::ServiceKind;
use sekvent_tasks::{
    Context, Runner, SystemRunner, deps, install_interrupt_handler, process_env, sdk, self_update,
    skills,
};

/// Backend workspaces on sekvent: gate, coverage, scaffolding and pins.
#[derive(Debug, Parser)]
#[command(name = "cargo-sekvent", bin_name = "cargo sekvent", version)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// fmt --check, clippy, test, optional doc and boundaries, with hooks and
    /// harness cleanup (compiles: runs on the builder).
    Gate,
    /// cargo check over the gate's package selection (compiles).
    Check,
    /// cargo clippy with warnings denied (compiles).
    Clippy,
    /// cargo test over the gate's selection; arguments after `--` go to the
    /// test binaries (compiles).
    Test {
        /// Arguments for the test binaries.
        #[arg(last = true)]
        args: Vec<String>,
    },
    /// Instrumented tests and per-package line coverage floors (compiles).
    Coverage {
        /// Also write an LCOV report to this path.
        #[arg(long, value_name = "PATH")]
        lcov: Option<PathBuf>,
        /// Print uncovered line ranges of this package.
        #[arg(long, value_name = "PACKAGE")]
        misses: Option<String>,
    },
    /// Check [[gate.boundaries]] against the dependency graph.
    Boundaries,
    /// Protobuf service contracts of the [contract] roots: write baselines or
    /// check wire compatibility (compiles protos in-process: runs here).
    Contract {
        #[command(subcommand)]
        command: ContractCommand,
    },
    /// Remove test containers of the configured label namespace.
    HarnessClean(HarnessCleanArgs),
    /// Run a custom [tasks.<name>]; without a name, list them.
    Run {
        /// Task name.
        task: Option<String>,
        /// Arguments appended to the task's command.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Create a new sekvent workspace.
    New(NewArgs),
    /// Add sekvent to the existing Cargo workspace.
    Init {
        /// Flavour suggested for the first service.
        #[arg(long, value_enum, default_value_t = Kind::Http)]
        kind: Kind,
        /// Replace existing files.
        #[arg(long)]
        force: bool,
    },
    /// Add a service crate to this workspace.
    Add {
        /// Crate name, kebab-case.
        name: String,
        /// Service flavour.
        #[arg(long, value_enum)]
        kind: Kind,
        /// Render over existing files.
        #[arg(long)]
        force: bool,
    },
    /// Compare or align [workspace.dependencies] with sekvent's pins.
    Deps {
        #[command(subcommand)]
        command: DepsCommand,
    },
    /// The sekvent revision this project is locked to.
    Sdk {
        #[command(subcommand)]
        command: SdkCommand,
    },
    /// CI templates.
    Ci {
        #[command(subcommand)]
        command: CiCommand,
    },
    /// Insert or refresh the sekvent section of AGENTS.md.
    Agents {
        /// The file to update (default: AGENTS.md in the project root).
        #[arg(long)]
        file: Option<PathBuf>,
    },
    /// Agent skills shipped with sekvent.
    Skills {
        #[command(subcommand)]
        command: SkillsCommand,
    },
    /// Replace this binary with the latest release build.
    SelfUpdate,
    /// The project configuration.
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },
    #[command(external_subcommand)]
    External(Vec<String>),
}

#[derive(Debug, Args)]
struct HarnessCleanArgs {
    #[command(flatten)]
    scope: CleanArgs,
    /// Confirm --all.
    #[arg(long)]
    yes: bool,
}

#[derive(Debug, Args)]
#[group(required = true, multiple = false)]
struct CleanArgs {
    /// Remove one run's containers.
    #[arg(long, value_name = "ID")]
    run: Option<String>,
    /// Remove containers older than `[harness].stale_after`.
    #[arg(long)]
    stale: bool,
    /// Remove every container of the namespace (needs --yes).
    #[arg(long)]
    all: bool,
}

#[derive(Debug, Args)]
struct NewArgs {
    /// Project name, kebab-case; also the first service's name.
    name: String,
    /// Flavour of the first service.
    #[arg(long, value_enum, default_value_t = Kind::Http)]
    kind: Kind,
    /// Target directory (default: ./<name>).
    #[arg(long, value_name = "PATH")]
    dir: Option<PathBuf>,
    /// Depend on a local sekvent checkout instead of git.
    #[arg(long, value_name = "PATH")]
    sekvent_path: Option<PathBuf>,
    /// CI template to add.
    #[arg(long, value_enum, default_value_t = Ci::Github)]
    ci: Ci,
    /// Skip `git init`.
    #[arg(long)]
    no_git: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum Kind {
    Grpc,
    Http,
    Worker,
}

impl From<Kind> for ServiceKind {
    fn from(kind: Kind) -> Self {
        match kind {
            Kind::Grpc => Self::Grpc,
            Kind::Http => Self::Http,
            Kind::Worker => Self::Worker,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum Ci {
    Github,
    Bitbucket,
    None,
}

impl Ci {
    fn provider(self) -> Option<CiProvider> {
        match self {
            Self::Github => Some(CiProvider::Github),
            Self::Bitbucket => Some(CiProvider::Bitbucket),
            Self::None => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum Provider {
    Github,
    Bitbucket,
}

impl From<Provider> for CiProvider {
    fn from(provider: Provider) -> Self {
        match provider {
            Provider::Github => Self::Github,
            Provider::Bitbucket => Self::Bitbucket,
        }
    }
}

#[derive(Debug, Subcommand)]
enum DepsCommand {
    /// Report older, newer, feature-mismatch and missing pins.
    Check {
        /// Exit 1 when a pin is older or missing.
        #[arg(long)]
        strict: bool,
    },
    /// Rewrite versions and add missing features and dependencies.
    Sync {
        /// Limit to these dependencies.
        #[arg(long, value_delimiter = ',', value_name = "A,B")]
        only: Vec<String>,
    },
}

#[derive(Debug, Clone, Copy, Subcommand)]
enum SdkCommand {
    /// Show the locked sekvent revision.
    Status {
        /// Compare with the remote branch head.
        #[arg(long)]
        remote: bool,
    },
    /// `cargo update` the sekvent crates (writes Cargo.lock, no compile).
    Update,
}

#[derive(Debug, Subcommand)]
enum CiCommand {
    /// Render the gate-only CI template.
    Generate {
        /// CI system.
        #[arg(value_enum)]
        provider: Provider,
        /// Replace existing files.
        #[arg(long)]
        force: bool,
    },
}

#[derive(Debug, Subcommand)]
enum SkillsCommand {
    /// Copy the sekvent skills into ~/.kodein/skills (and ~/.claude/skills).
    Install {
        /// Install into this directory only.
        #[arg(long, value_name = "DIR")]
        dest: Option<PathBuf>,
    },
}

#[derive(Debug, Subcommand)]
enum ConfigCommand {
    /// Print the effective sekvent.toml with defaults filled in.
    Show,
}

#[derive(Debug, Subcommand)]
enum ContractCommand {
    /// Write the canonical baselines of every service, or of those named.
    Emit {
        /// Full service names, e.g. `billing.v1.Billing` (default: all).
        services: Vec<String>,
    },
    /// Compare the services with their baselines; exit 1 on a breaking
    /// change or a missing baseline.
    Check {
        /// Full service names, e.g. `billing.v1.Billing` (default: all).
        services: Vec<String>,
    },
}

/// What every handler gets.
struct Env<'a> {
    cwd: PathBuf,
    original: &'a [String],
    runner: &'a dyn Runner,
    sweeper: &'a dyn Sweeper,
}

impl Env<'_> {
    fn project(&self) -> anyhow::Result<Project> {
        Ok(Project::discover(&self.cwd)?)
    }

    fn context(&self) -> anyhow::Result<Context<'_>> {
        Ok(Context::new(
            self.project()?,
            process_env(),
            self.runner,
            self.sweeper,
            install_interrupt_handler(),
        ))
    }

    fn compiling(
        &self,
        local: impl FnOnce(&Context<'_>) -> anyhow::Result<i32>,
    ) -> anyhow::Result<i32> {
        dispatch::compile_or_forward(&self.context()?, self.original, local)
    }

    fn cargo_root(&self) -> anyhow::Result<PathBuf> {
        find_project_root(&self.cwd)
            .or_else(|| find_workspace_root(&self.cwd))
            .context("no Cargo workspace found here or above")
    }

    fn absolute(&self, path: &Path) -> PathBuf {
        self.cwd.join(path)
    }
}

fn main() -> ExitCode {
    let argv = dispatch::normalize_args(
        std::env::args_os()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect(),
    );
    let original = argv.get(1..).map_or_else(Vec::new, <[String]>::to_vec);
    let cli = Cli::parse_from(&argv);
    let result = std::env::current_dir()
        .context("cannot read the current directory")
        .and_then(|cwd| {
            let env = Env {
                cwd,
                original: &original,
                runner: &SystemRunner,
                sweeper: &DockerSweeper,
            };
            run(&env, cli.command)
        });
    match result {
        Ok(code) => u8::try_from(code).map_or(ExitCode::FAILURE, ExitCode::from),
        Err(error) => {
            eprintln!("error: {error:#}");
            ExitCode::FAILURE
        }
    }
}

fn run(env: &Env<'_>, command: Command) -> anyhow::Result<i32> {
    match command {
        Command::Gate => env.compiling(gate::gate),
        Command::Check => env.compiling(|ctx| gate::single(ctx, &SingleStep::Check)),
        Command::Clippy => env.compiling(|ctx| gate::single(ctx, &SingleStep::Clippy)),
        Command::Test { args } => env.compiling(|ctx| gate::single(ctx, &SingleStep::Test(args))),
        Command::Coverage { lcov, misses } => {
            let options = CoverageOptions {
                lcov: lcov.map(|path| env.absolute(&path)),
                misses,
            };
            env.compiling(|ctx| gate::coverage(ctx, &options))
        }
        Command::Boundaries => gate::boundaries(&env.context()?),
        Command::Contract { command } => contract_command(env, command),
        Command::HarnessClean(args) => harness_clean(env, args),
        Command::Run { task: None, .. } => {
            print!("{}", dispatch::list_tasks(&env.project()?.config.tasks));
            Ok(0)
        }
        Command::Run {
            task: Some(task),
            args,
        } => dispatch::run_task(&env.context()?, env.original, &task, &args),
        Command::New(args) => new(env, args),
        Command::Init { kind, force } => {
            scaffold::init(&env.cwd, kind.into(), force)?;
            Ok(0)
        }
        Command::Add { name, kind, force } => {
            let project = env.project()?;
            scaffold::add(
                &project.root,
                &project.config.project.name,
                &name,
                kind.into(),
                force,
            )?;
            Ok(0)
        }
        Command::Deps { command } => deps_command(env, command),
        Command::Sdk { command } => sdk_command(env, command),
        Command::Ci {
            command: CiCommand::Generate { provider, force },
        } => {
            let project = env.project()?;
            scaffold::ci_generate(
                &project.root,
                &project.config.project.name,
                provider.into(),
                force,
            )?;
            Ok(0)
        }
        Command::Agents { file } => {
            let project = env.project()?;
            let file = file.map_or_else(
                || project.root.join(AGENTS_FILE),
                |path| env.absolute(&path),
            );
            scaffold::agents_update(&file, &project.config.project.name)?;
            Ok(0)
        }
        Command::Skills {
            command: SkillsCommand::Install { dest },
        } => {
            skills::install(dest.map(|path| env.absolute(&path)).as_deref())?;
            Ok(0)
        }
        Command::SelfUpdate => {
            self_update::self_update(env.runner)?;
            Ok(0)
        }
        Command::Config {
            command: ConfigCommand::Show,
        } => {
            print!("{}", env.project()?.config.to_toml()?);
            Ok(0)
        }
        Command::External(args) => external(env, &args),
    }
}

fn harness_clean(env: &Env<'_>, args: HarnessCleanArgs) -> anyhow::Result<i32> {
    let scope = match (args.scope.run, args.scope.stale, args.scope.all) {
        (Some(run_id), _, _) => CleanScope::Run(run_id),
        (None, true, _) => CleanScope::Stale,
        (None, false, _) => CleanScope::All,
    };
    if scope == CleanScope::All && !args.yes {
        anyhow::bail!(
            "--all removes every harness container of the namespace; pass --yes to confirm"
        );
    }
    env.compiling(|ctx| {
        let report = harness::clean(&ctx.config.harness, ctx.sweeper, &scope, args.yes)?;
        println!(
            "harness-clean: removed {} container(s)",
            report.removed.len()
        );
        Ok(0)
    })
}

fn contract_command(env: &Env<'_>, command: ContractCommand) -> anyhow::Result<i32> {
    let project = env.project()?;
    let config = &project.config.contract;
    match command {
        ContractCommand::Emit { services } => contract::run_emit(&project.root, config, &services),
        ContractCommand::Check { services } => {
            contract::run_check(&project.root, config, &services)
        }
    }
}

fn new(env: &Env<'_>, args: NewArgs) -> anyhow::Result<i32> {
    let options = NewOptions {
        name: args.name,
        kind: args.kind.into(),
        dir: args.dir,
        sekvent_path: args.sekvent_path,
        ci: args.ci.provider(),
        git: !args.no_git,
    };
    scaffold::new_project(env.runner, &env.cwd, &options)?;
    Ok(0)
}

fn deps_command(env: &Env<'_>, command: DepsCommand) -> anyhow::Result<i32> {
    let manifest_path = env.cargo_root()?.join("Cargo.toml");
    let text = std::fs::read_to_string(&manifest_path)
        .with_context(|| format!("cannot read {}", manifest_path.display()))?;
    let pins = deps::embedded_pins();
    match command {
        DepsCommand::Check { strict } => {
            Ok(deps::print_findings(&deps::check(&text, &pins)?, strict))
        }
        DepsCommand::Sync { only } => {
            let only = (!only.is_empty()).then_some(only.as_slice());
            let (updated, changes) = deps::sync(&text, &pins, only)?;
            if changes.is_empty() {
                println!("deps: nothing to change");
                return Ok(0);
            }
            std::fs::write(&manifest_path, updated)?;
            for change in &changes {
                println!("~ {change}");
            }
            println!("next: `cargo update --workspace` refreshes Cargo.lock without compiling");
            Ok(0)
        }
    }
}

fn sdk_command(env: &Env<'_>, command: SdkCommand) -> anyhow::Result<i32> {
    let root = env.cargo_root()?;
    let lock_path = root.join("Cargo.lock");
    let lock = std::fs::read_to_string(&lock_path)
        .with_context(|| format!("cannot read {}", lock_path.display()))?;
    match command {
        SdkCommand::Status { remote } => {
            println!("{}", sdk::status(env.runner, &lock, remote)?);
            Ok(0)
        }
        SdkCommand::Update => {
            let packages = sdk::parse_lock(&lock)?;
            let Some(cmd) = sdk::update_command(&root, &packages) else {
                println!("sdk: no sekvent crates in Cargo.lock");
                return Ok(0);
            };
            println!("==> sdk: {cmd}");
            Ok(env.runner.status(&cmd)?)
        }
    }
}

fn external(env: &Env<'_>, args: &[String]) -> anyhow::Result<i32> {
    let name = args.first().map_or("", String::as_str);
    if dispatch::is_reserved(name) {
        eprintln!("{}", dispatch::reserved_message(name));
        return Ok(dispatch::RESERVED_EXIT);
    }
    let project = match Project::discover(&env.cwd) {
        Ok(project) => project,
        Err(ConfigError::NotFound { .. }) => return help(name),
        Err(error) => return Err(error.into()),
    };
    let meta = Metadata::load(env.runner, &project.root, false)?;
    match dispatch::classify_external(name, dispatch::has_xtask(&meta)) {
        External::Reserved => Ok(dispatch::RESERVED_EXIT),
        External::Help => help(name),
        External::Xtask => env.compiling(|ctx| {
            let cmd = dispatch::xtask_command(&ctx.root, args);
            println!("==> xtask: {}", args.join(" "));
            Ok(ctx.runner.status(&cmd)?)
        }),
    }
}

fn help(name: &str) -> anyhow::Result<i32> {
    eprintln!("unknown command `{name}`\n");
    Cli::command().print_help()?;
    Ok(2)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Cli, clap::Error> {
        let owned: Vec<String> = args.iter().map(|arg| (*arg).to_owned()).collect();
        Cli::try_parse_from(dispatch::normalize_args(owned))
    }

    #[test]
    fn the_command_definition_is_consistent() {
        Cli::command().debug_assert();
    }

    #[test]
    fn cargo_passes_the_subcommand_name_first() {
        let cli = parse(&["cargo-sekvent", "sekvent", "gate"]).unwrap();
        assert!(matches!(cli.command, Command::Gate));
        let cli = parse(&["cargo-sekvent", "check"]).unwrap();
        assert!(matches!(cli.command, Command::Check));
    }

    #[test]
    fn test_arguments_follow_a_double_dash() {
        let cli = parse(&["cargo-sekvent", "sekvent", "test", "--", "--nocapture", "x"]).unwrap();
        let Command::Test { args } = cli.command else {
            panic!("expected test");
        };
        assert_eq!(args, ["--nocapture", "x"]);
    }

    #[test]
    fn run_passes_trailing_arguments_through() {
        let cli = parse(&["cargo-sekvent", "run", "seed", "--rows", "5"]).unwrap();
        let Command::Run { task, args } = cli.command else {
            panic!("expected run");
        };
        assert_eq!(task.as_deref(), Some("seed"));
        assert_eq!(args, ["--rows", "5"]);
    }

    #[test]
    fn reserved_and_unknown_names_are_external() {
        for name in dispatch::RESERVED.into_iter().chain(["release"]) {
            let cli = parse(&["cargo-sekvent", name, "--flag"]).unwrap();
            let Command::External(args) = cli.command else {
                panic!("expected external for {name}");
            };
            assert_eq!(args, [name, "--flag"]);
        }
    }

    #[test]
    fn contract_is_a_command_with_emit_and_check() {
        let cli = parse(&["cargo-sekvent", "sekvent", "contract", "emit"]).unwrap();
        let Command::Contract {
            command: ContractCommand::Emit { services },
        } = cli.command
        else {
            panic!("expected contract emit");
        };
        assert!(services.is_empty());

        let cli = parse(&[
            "cargo-sekvent",
            "contract",
            "check",
            "billing.v1.Billing",
            "orders.v1.Orders",
        ])
        .unwrap();
        let Command::Contract {
            command: ContractCommand::Check { services },
        } = cli.command
        else {
            panic!("expected contract check");
        };
        assert_eq!(services, ["billing.v1.Billing", "orders.v1.Orders"]);

        assert!(parse(&["cargo-sekvent", "contract"]).is_err());
        assert!(parse(&["cargo-sekvent", "contract", "diff"]).is_err());
    }

    #[test]
    fn harness_clean_needs_exactly_one_scope() {
        assert!(parse(&["cargo-sekvent", "harness-clean"]).is_err());
        assert!(parse(&["cargo-sekvent", "harness-clean", "--stale", "--all"]).is_err());
        let cli = parse(&["cargo-sekvent", "harness-clean", "--all", "--yes"]).unwrap();
        let Command::HarnessClean(args) = cli.command else {
            panic!("expected harness-clean");
        };
        assert!(args.scope.all && args.yes);
    }

    #[test]
    fn new_and_deps_options_parse() {
        let cli = parse(&[
            "cargo-sekvent",
            "new",
            "orders",
            "--kind",
            "grpc",
            "--ci",
            "none",
            "--no-git",
            "--sekvent-path",
            "../sekvent",
        ])
        .unwrap();
        let Command::New(args) = cli.command else {
            panic!("expected new");
        };
        assert_eq!(args.kind, Kind::Grpc);
        assert_eq!(args.ci.provider(), None);
        assert!(args.no_git);
        assert_eq!(args.sekvent_path, Some(PathBuf::from("../sekvent")));

        let cli = parse(&["cargo-sekvent", "deps", "sync", "--only", "tokio,serde"]).unwrap();
        let Command::Deps {
            command: DepsCommand::Sync { only },
        } = cli.command
        else {
            panic!("expected deps sync");
        };
        assert_eq!(only, ["tokio", "serde"]);
        assert!(parse(&["cargo-sekvent", "ci", "generate", "gitlab"]).is_err());
    }
}
