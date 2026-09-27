//! Pure construction of the cargo commands behind each gate step.
//!
//! Nothing here runs a process; [`crate::gate`] executes what these functions
//! return, and the tests assert on the exact argv.

use std::path::Path;

use sekvent_testing::DOCKER_TESTS_ENV;
use thiserror::Error;

use crate::config::{Config, FeatureSet, Features, GateConfig, HarnessConfig};
use crate::location::EnvMap;
use crate::metadata::Metadata;
use crate::process::Cmd;

/// `RUST_TEST_THREADS`, set from `[gate].test_threads` unless already set.
pub const TEST_THREADS_ENV: &str = "RUST_TEST_THREADS";

/// Libtest flag that runs `#[ignore]`d tests along with the others.
pub const INCLUDE_IGNORED: &str = "--include-ignored";

/// A package selection that does not match the workspace.
#[derive(Debug, Error, PartialEq, Eq)]
#[non_exhaustive]
pub enum PlanError {
    /// `[gate].exclude` names a package that is not a workspace member.
    #[error("`gate.exclude` names `{0}`, which is not a workspace member")]
    UnknownExclude(String),
    /// Every member is excluded, so there is nothing to check.
    #[error("`gate.exclude` excludes every workspace member")]
    NothingSelected,
}

/// The workspace members the gate covers and the ones it leaves out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Selection {
    /// Selected members, sorted.
    pub members: Vec<String>,
    /// Excluded members, in configuration order.
    pub exclude: Vec<String>,
}

impl Selection {
    /// Apply `gate.exclude` to the workspace members in `meta`.
    pub fn new(meta: &Metadata, gate: &GateConfig) -> Result<Self, PlanError> {
        let all = meta.member_names();
        if let Some(unknown) = gate.exclude.iter().find(|name| !all.contains(name)) {
            return Err(PlanError::UnknownExclude(unknown.clone()));
        }
        let members: Vec<String> = all
            .into_iter()
            .filter(|name| !gate.exclude.contains(name))
            .collect();
        if members.is_empty() {
            return Err(PlanError::NothingSelected);
        }
        Ok(Self {
            members,
            exclude: gate.exclude.clone(),
        })
    }

    /// `--workspace --exclude a --exclude b`.
    pub fn workspace_args(&self) -> Vec<String> {
        let mut args = vec!["--workspace".to_owned()];
        for name in &self.exclude {
            args.push("--exclude".to_owned());
            args.push(name.clone());
        }
        args
    }

    /// Package arguments for `cargo fmt`, which has no `--exclude`: `--all`
    /// when nothing is excluded, else `-p` for each selected member.
    pub fn fmt_args(&self) -> Vec<String> {
        if self.exclude.is_empty() {
            return vec!["--all".to_owned()];
        }
        self.members
            .iter()
            .flat_map(|name| ["-p".to_owned(), name.clone()])
            .collect()
    }
}

/// The cargo feature flags for `features`.
pub fn feature_args(features: &Features) -> Vec<String> {
    match features {
        Features::Set(FeatureSet::All) => vec!["--all-features".to_owned()],
        Features::List(list) if !list.is_empty() => {
            vec!["--features".to_owned(), list.join(",")]
        }
        Features::Set(FeatureSet::Default) | Features::List(_) => Vec::new(),
    }
}

fn cargo(root: &Path, subcommand: &str) -> Cmd {
    Cmd::new("cargo").arg(subcommand).cwd(root)
}

/// `cargo fmt --check` over the selected members.
pub fn fmt_command(root: &Path, selection: &Selection) -> Cmd {
    cargo(root, "fmt").arg("--check").args(selection.fmt_args())
}

/// `cargo check` over the selection, all targets.
pub fn check_command(root: &Path, selection: &Selection, gate: &GateConfig) -> Cmd {
    cargo(root, "check")
        .args(selection.workspace_args())
        .arg("--all-targets")
        .args(feature_args(&gate.features))
        .arg("--locked")
}

/// `cargo clippy … -- -D warnings <clippy_args>`.
pub fn clippy_command(root: &Path, selection: &Selection, gate: &GateConfig) -> Cmd {
    cargo(root, "clippy")
        .args(selection.workspace_args())
        .args(["--no-deps", "--all-targets"])
        .args(feature_args(&gate.features))
        .arg("--locked")
        .args(["--", "-D", "warnings"])
        .args(gate.clippy_args.iter().cloned())
}

/// Environment of a test run: `RUST_TEST_THREADS` from `[gate].test_threads`
/// and, with `[harness].docker_tests`, `SEKVENT_DOCKER_TESTS=1`; a variable
/// `env` already sets is left to the environment.
pub fn test_env(config: &Config, env: &EnvMap) -> Vec<(String, String)> {
    let mut vars = Vec::new();
    if !env.contains_key(TEST_THREADS_ENV) {
        vars.push((
            TEST_THREADS_ENV.to_owned(),
            config.gate.test_threads.to_string(),
        ));
    }
    if config.harness.docker_tests && !env.contains_key(DOCKER_TESTS_ENV) {
        vars.push((DOCKER_TESTS_ENV.to_owned(), "1".to_owned()));
    }
    vars
}

/// Arguments for the test binaries: `extra`, plus [`INCLUDE_IGNORED`] with
/// `[harness].docker_tests` unless `extra` already selects ignored tests
/// (libtest rejects `--ignored` together with `--include-ignored`).
pub fn test_binary_args(harness: &HarnessConfig, extra: &[String]) -> Vec<String> {
    let mut out = extra.to_vec();
    let selects_ignored = extra
        .iter()
        .any(|arg| arg == INCLUDE_IGNORED || arg == "--ignored");
    if harness.docker_tests && !selects_ignored {
        out.push(INCLUDE_IGNORED.to_owned());
    }
    out
}

fn with_binary_args(cmd: Cmd, args: &[String]) -> Cmd {
    if args.is_empty() {
        cmd
    } else {
        cmd.arg("--").args(args.iter().cloned())
    }
}

/// `cargo test` over the selection; [`test_binary_args`] go after `--`.
pub fn test_command(
    root: &Path,
    selection: &Selection,
    config: &Config,
    env: &EnvMap,
    extra: &[String],
) -> Cmd {
    let cmd = cargo(root, "test")
        .args(selection.workspace_args())
        .args(feature_args(&config.gate.features))
        .arg("--locked")
        .envs(&test_env(config, env));
    with_binary_args(cmd, &test_binary_args(&config.harness, extra))
}

/// `cargo doc --no-deps` with warnings denied.
pub fn doc_command(root: &Path, selection: &Selection, gate: &GateConfig) -> Cmd {
    cargo(root, "doc")
        .args(selection.workspace_args())
        .arg("--no-deps")
        .args(feature_args(&gate.features))
        .arg("--locked")
        .env("RUSTDOCFLAGS", "-Dwarnings")
}

/// A hook argv, run in the project root.
pub fn hook_command(root: &Path, argv: &[String]) -> Option<Cmd> {
    Cmd::from_argv(argv).map(|cmd| cmd.cwd(root))
}

/// One step of a gate or coverage run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    /// Run a command; a non-zero exit fails the step.
    Run {
        /// Banner name, e.g. `clippy`.
        name: String,
        /// The command.
        cmd: Cmd,
    },
    /// Check `[[gate.boundaries]]` against the dependency graph.
    Boundaries,
}

impl Step {
    /// The banner name.
    pub fn name(&self) -> &str {
        match self {
            Self::Run { name, .. } => name,
            Self::Boundaries => "boundaries",
        }
    }

    fn run(name: impl Into<String>, cmd: Cmd) -> Self {
        Self::Run {
            name: name.into(),
            cmd,
        }
    }
}

/// Hook steps named `<label>[<n>]`.
pub fn hook_steps(
    root: &Path,
    label: &str,
    hooks: &[Vec<String>],
    env: &[(String, String)],
) -> Vec<Step> {
    hooks
        .iter()
        .enumerate()
        .filter_map(|(index, argv)| {
            let cmd = hook_command(root, argv)?.envs(env);
            Some(Step::run(format!("{label}[{index}]"), cmd))
        })
        .collect()
}

/// The full gate: pre hooks, fmt, clippy, test, doc, boundaries, post hooks.
///
/// `harness_env` (run id and label namespace) is added to every command.
pub fn gate_steps(
    root: &Path,
    config: &Config,
    selection: &Selection,
    env: &EnvMap,
    harness_env: &[(String, String)],
) -> Vec<Step> {
    let gate = &config.gate;
    let mut steps = hook_steps(root, "pre_gate", &config.hooks.pre_gate, harness_env);
    if gate.fmt {
        steps.push(Step::run(
            "fmt",
            fmt_command(root, selection).envs(harness_env),
        ));
    }
    steps.push(Step::run(
        "clippy",
        clippy_command(root, selection, gate).envs(harness_env),
    ));
    steps.push(Step::run(
        "test",
        test_command(root, selection, config, env, &[]).envs(harness_env),
    ));
    if gate.doc {
        steps.push(Step::run(
            "doc",
            doc_command(root, selection, gate).envs(harness_env),
        ));
    }
    if !gate.boundaries.is_empty() {
        steps.push(Step::Boundaries);
    }
    steps.extend(hook_steps(
        root,
        "post_gate",
        &config.hooks.post_gate,
        harness_env,
    ));
    steps
}

/// `cargo llvm-cov clean --workspace`.
pub fn coverage_clean_command(root: &Path) -> Cmd {
    cargo(root, "llvm-cov").args(["clean", "--workspace"])
}

/// `cargo llvm-cov … --no-report`: build and run the tests instrumented.
pub fn coverage_run_command(
    root: &Path,
    selection: &Selection,
    config: &Config,
    env: &EnvMap,
) -> Cmd {
    let cmd = cargo(root, "llvm-cov")
        .args(selection.workspace_args())
        .args(feature_args(&config.gate.features))
        .args(["--locked", "--no-report"])
        .envs(&test_env(config, env));
    with_binary_args(cmd, &test_binary_args(&config.harness, &[]))
}

/// `cargo llvm-cov report --json` into `output`; `summary_only` drops the
/// per-line segments.
pub fn coverage_json_command(
    root: &Path,
    ignore_regex: &str,
    output: &Path,
    summary_only: bool,
) -> Cmd {
    let cmd = cargo(root, "llvm-cov").args(["report", "--json"]);
    let cmd = if summary_only {
        cmd.arg("--summary-only")
    } else {
        cmd
    };
    cmd.args(["--ignore-filename-regex", ignore_regex])
        .arg("--output-path")
        .arg(output.display().to_string())
}

/// `cargo llvm-cov report --lcov` into `output`.
pub fn coverage_lcov_command(root: &Path, ignore_regex: &str, output: &Path) -> Cmd {
    cargo(root, "llvm-cov")
        .args(["report", "--lcov", "--ignore-filename-regex", ignore_regex])
        .arg("--output-path")
        .arg(output.display().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Boundary;
    use crate::fixtures::METADATA;

    fn meta() -> Metadata {
        Metadata::parse(METADATA).unwrap()
    }

    fn gate(exclude: &[&str]) -> GateConfig {
        GateConfig {
            exclude: exclude.iter().map(|name| (*name).to_owned()).collect(),
            ..GateConfig::default()
        }
    }

    fn lines(steps: &[Step]) -> Vec<String> {
        steps
            .iter()
            .map(|step| match step {
                Step::Run { name, cmd } => format!("{name}: {cmd}"),
                Step::Boundaries => "boundaries".to_owned(),
            })
            .collect()
    }

    #[test]
    fn excludes_leave_the_rest_selected() {
        let selection = Selection::new(&meta(), &gate(&["vendored"])).unwrap();
        assert_eq!(
            selection.members,
            ["orders-api", "orders-db", "orders-domain"]
        );
        assert_eq!(
            selection.workspace_args(),
            ["--workspace", "--exclude", "vendored"]
        );
        assert_eq!(
            selection.fmt_args(),
            ["-p", "orders-api", "-p", "orders-db", "-p", "orders-domain"]
        );
    }

    #[test]
    fn no_excludes_formats_everything() {
        let selection = Selection::new(&meta(), &gate(&[])).unwrap();
        assert_eq!(selection.members.len(), 4);
        assert_eq!(selection.fmt_args(), ["--all"]);
        assert_eq!(selection.workspace_args(), ["--workspace"]);
    }

    #[test]
    fn bad_excludes_are_errors() {
        assert_eq!(
            Selection::new(&meta(), &gate(&["serde"])),
            Err(PlanError::UnknownExclude("serde".into()))
        );
        assert_eq!(
            Selection::new(
                &meta(),
                &gate(&["orders-api", "orders-db", "orders-domain", "vendored"])
            ),
            Err(PlanError::NothingSelected)
        );
    }

    #[test]
    fn feature_selections_map_to_flags() {
        assert_eq!(
            feature_args(&Features::Set(FeatureSet::All)),
            ["--all-features"]
        );
        assert!(feature_args(&Features::Set(FeatureSet::Default)).is_empty());
        assert!(feature_args(&Features::List(Vec::new())).is_empty());
        assert_eq!(
            feature_args(&Features::List(vec!["a".into(), "b".into()])),
            ["--features", "a,b"]
        );
    }

    #[test]
    fn the_full_gate_is_built_in_order() {
        let mut config = Config::with_name("orders");
        config.gate = GateConfig {
            exclude: vec!["vendored".into()],
            clippy_args: vec!["-A".into(), "clippy::xyz".into()],
            doc: true,
            boundaries: vec![Boundary {
                from: "orders-api".into(),
                to: "orders-db".into(),
            }],
            ..GateConfig::default()
        };
        config.hooks.pre_gate = vec![vec!["echo".into(), "pre".into()]];
        config.hooks.post_gate = vec![vec!["echo".into(), "post".into()]];
        let selection = Selection::new(&meta(), &config.gate).unwrap();
        let harness = vec![("SEKVENT_TEST_RUN_ID".to_owned(), "r1".to_owned())];
        let steps = gate_steps(
            Path::new("/work"),
            &config,
            &selection,
            &EnvMap::new(),
            &harness,
        );
        assert_eq!(
            lines(&steps),
            [
                "pre_gate[0]: echo pre",
                "fmt: cargo fmt --check -p orders-api -p orders-db -p orders-domain",
                "clippy: cargo clippy --workspace --exclude vendored --no-deps --all-targets \
                 --all-features --locked -- -D warnings -A clippy::xyz",
                "test: cargo test --workspace --exclude vendored --all-features --locked",
                "doc: cargo doc --workspace --exclude vendored --no-deps --all-features --locked",
                "boundaries",
                "post_gate[0]: echo post",
            ]
        );
        for step in &steps {
            if let Step::Run { cmd, .. } = step {
                assert_eq!(cmd.env_value("SEKVENT_TEST_RUN_ID"), Some("r1"), "{cmd}");
                assert_eq!(cmd.cwd.as_deref(), Some(Path::new("/work")));
            }
        }
        let Step::Run { cmd: test, .. } = &steps[3] else {
            panic!("test step")
        };
        assert_eq!(test.env_value("RUST_TEST_THREADS"), Some("8"));
        let Step::Run { cmd: doc, .. } = &steps[4] else {
            panic!("doc step")
        };
        assert_eq!(doc.env_value("RUSTDOCFLAGS"), Some("-Dwarnings"));
    }

    #[test]
    fn a_minimal_gate_skips_optional_steps() {
        let mut config = Config::with_name("orders");
        config.gate.fmt = false;
        config.gate.features = Features::Set(FeatureSet::Default);
        let selection = Selection::new(&meta(), &config.gate).unwrap();
        let steps = gate_steps(Path::new("/w"), &config, &selection, &EnvMap::new(), &[]);
        let names: Vec<&str> = steps.iter().map(Step::name).collect();
        assert_eq!(names, ["clippy", "test"]);
        assert_eq!(
            lines(&steps)[0],
            "clippy: cargo clippy --workspace --no-deps --all-targets --locked -- -D warnings"
        );
    }

    #[test]
    fn test_threads_respect_the_environment() {
        let selection = Selection::new(&meta(), &gate(&[])).unwrap();
        let mut env = EnvMap::new();
        env.insert("RUST_TEST_THREADS".into(), "2".into());
        let cmd = test_command(
            Path::new("/w"),
            &selection,
            &Config::with_name("orders"),
            &env,
            &["--nocapture".into()],
        );
        assert_eq!(cmd.env_value("RUST_TEST_THREADS"), None);
        assert_eq!(cmd.env_value("SEKVENT_DOCKER_TESTS"), None);
        assert_eq!(
            cmd.to_string(),
            "cargo test --workspace --all-features --locked -- --nocapture"
        );
    }

    #[test]
    fn check_and_coverage_commands() {
        let selection = Selection::new(&meta(), &gate(&["vendored"])).unwrap();
        let root = Path::new("/w");
        assert_eq!(
            check_command(root, &selection, &gate(&["vendored"])).to_string(),
            "cargo check --workspace --exclude vendored --all-targets --all-features --locked"
        );
        assert_eq!(
            coverage_clean_command(root).to_string(),
            "cargo llvm-cov clean --workspace"
        );
        let mut config = Config::with_name("orders");
        config.gate = gate(&["vendored"]);
        let run = coverage_run_command(root, &selection, &config, &EnvMap::new());
        assert_eq!(
            run.to_string(),
            "cargo llvm-cov --workspace --exclude vendored --all-features --locked --no-report"
        );
        assert_eq!(run.env_value("RUST_TEST_THREADS"), Some("8"));
        assert_eq!(run.env_value("SEKVENT_DOCKER_TESTS"), None);
        assert_eq!(
            coverage_json_command(root, "x", Path::new("/tmp/c.json"), true).to_string(),
            "cargo llvm-cov report --json --summary-only --ignore-filename-regex x --output-path /tmp/c.json"
        );
        assert_eq!(
            coverage_json_command(root, "x", Path::new("/tmp/c.json"), false).args,
            [
                "llvm-cov",
                "report",
                "--json",
                "--ignore-filename-regex",
                "x",
                "--output-path",
                "/tmp/c.json"
            ]
        );
        assert_eq!(
            coverage_lcov_command(root, "x", Path::new("lcov.info")).to_string(),
            "cargo llvm-cov report --lcov --ignore-filename-regex x --output-path lcov.info"
        );
    }

    #[test]
    fn empty_hooks_are_skipped() {
        let steps = hook_steps(Path::new("/w"), "pre", &[Vec::new(), vec!["a".into()]], &[]);
        assert_eq!(lines(&steps), ["pre[1]: a"]);
    }

    fn docker_config() -> Config {
        let mut config = Config::with_name("orders");
        config.harness.docker_tests = true;
        config
    }

    #[test]
    fn docker_tests_opt_in_to_ignored_tests_in_the_gate() {
        let config = docker_config();
        let selection = Selection::new(&meta(), &config.gate).unwrap();
        let steps = gate_steps(Path::new("/w"), &config, &selection, &EnvMap::new(), &[]);
        let Some(Step::Run { cmd: test, .. }) = steps.iter().find(|step| step.name() == "test")
        else {
            panic!("test step")
        };
        assert_eq!(
            test.to_string(),
            "cargo test --workspace --all-features --locked -- --include-ignored"
        );
        assert_eq!(test.env_value("SEKVENT_DOCKER_TESTS"), Some("1"));
        assert_eq!(test.env_value("RUST_TEST_THREADS"), Some("8"));
    }

    #[test]
    fn docker_tests_merge_with_user_test_arguments() {
        let config = docker_config();
        let selection = Selection::new(&meta(), &config.gate).unwrap();
        let cmd = test_command(
            Path::new("/w"),
            &selection,
            &config,
            &EnvMap::new(),
            &["orders::".into(), "--nocapture".into()],
        );
        assert_eq!(
            cmd.to_string(),
            "cargo test --workspace --all-features --locked -- orders:: --nocapture \
             --include-ignored"
        );
        for selects_ignored in ["--ignored", "--include-ignored"] {
            let cmd = test_command(
                Path::new("/w"),
                &selection,
                &config,
                &EnvMap::new(),
                &[selects_ignored.into()],
            );
            assert_eq!(
                cmd.to_string(),
                format!("cargo test --workspace --all-features --locked -- {selects_ignored}")
            );
        }
    }

    #[test]
    fn docker_tests_respect_an_explicit_environment_value() {
        let config = docker_config();
        let mut env = EnvMap::new();
        env.insert("SEKVENT_DOCKER_TESTS".into(), "0".into());
        env.insert("RUST_TEST_THREADS".into(), "2".into());
        assert!(test_env(&config, &env).is_empty());
        assert_eq!(
            test_env(&config, &EnvMap::new()),
            [
                ("RUST_TEST_THREADS".to_owned(), "8".to_owned()),
                ("SEKVENT_DOCKER_TESTS".to_owned(), "1".to_owned()),
            ]
        );
    }

    #[test]
    fn docker_tests_reach_the_coverage_run() {
        let config = docker_config();
        let selection = Selection::new(&meta(), &config.gate).unwrap();
        let run = coverage_run_command(Path::new("/w"), &selection, &config, &EnvMap::new());
        assert_eq!(
            run.to_string(),
            "cargo llvm-cov --workspace --all-features --locked --no-report -- --include-ignored"
        );
        assert_eq!(run.env_value("SEKVENT_DOCKER_TESTS"), Some("1"));
    }

    #[test]
    fn without_docker_tests_no_binary_arguments_are_added() {
        let harness = HarnessConfig::default();
        assert!(test_binary_args(&harness, &[]).is_empty());
        assert_eq!(test_binary_args(&harness, &["x".into()]), ["x"]);
    }
}
