//! `sekvent.toml`, the per-project configuration read from the project root.
//!
//! Every table rejects unknown keys, so a typo is a load error instead of a
//! silently ignored setting. Everything except `[project].name` has a default;
//! `cargo sekvent config show` prints the effective values.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// File name of the project configuration.
pub const CONFIG_FILE: &str = "sekvent.toml";

/// Default location of the `rrb` remote builder front end.
pub const DEFAULT_RRB: &str = "~/.kodein/skills/build-on-rtx/bin/rrb";

/// Default per-package line coverage floor, in percent.
pub const DEFAULT_FAIL_UNDER_LINES: f64 = 95.0;

/// Default `RUST_TEST_THREADS` for gate and coverage test runs.
pub const DEFAULT_TEST_THREADS: u32 = 8;

/// Default age after which another run's harness containers count as stale.
pub const DEFAULT_STALE_AFTER: &str = "6h";

/// Why `sekvent.toml` could not be used.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ConfigError {
    /// The file exists but could not be read.
    #[error("cannot read {}: {source}", .path.display())]
    Read {
        /// The file that failed.
        path: PathBuf,
        /// The underlying I/O error.
        #[source]
        source: std::io::Error,
    },
    /// The file is not valid TOML or does not match the schema.
    #[error("invalid {}: {message}", .path.display())]
    Parse {
        /// The file that failed.
        path: PathBuf,
        /// The parser's message, with line and column.
        message: String,
    },
    /// A value parsed but is out of range or malformed.
    #[error("invalid {}: `{key}` {reason}", .path.display())]
    Invalid {
        /// The file that failed.
        path: PathBuf,
        /// Dotted key of the offending setting.
        key: String,
        /// What is wrong with it.
        reason: String,
    },
    /// No `sekvent.toml` exists in the start directory or any parent.
    #[error(
        "no sekvent.toml found in {} or any parent directory; run `cargo sekvent init`",
        .start.display()
    )]
    NotFound {
        /// Where the search started.
        start: PathBuf,
    },
}

/// The whole of `sekvent.toml`.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// `[project]`: identity of the project.
    pub project: ProjectConfig,
    /// `[remote]`: where compiling commands run.
    #[serde(default)]
    pub remote: RemoteConfig,
    /// `[gate]`: package selection and gate steps.
    #[serde(default)]
    pub gate: GateConfig,
    /// `[coverage]`: per-package line coverage floors.
    #[serde(default)]
    pub coverage: CoverageConfig,
    /// `[harness]`: test container hygiene.
    #[serde(default)]
    pub harness: HarnessConfig,
    /// `[hooks]`: commands run around the gate and coverage.
    #[serde(default)]
    pub hooks: HooksConfig,
    /// `[tasks.<name>]`: custom tasks for `cargo sekvent run <name>`.
    #[serde(default)]
    pub tasks: BTreeMap<String, TaskConfig>,
}

/// `[project]`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectConfig {
    /// Project name, kebab-case (required).
    pub name: String,
}

/// `[remote]`: how compiling commands reach the builder.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct RemoteConfig {
    /// `"rrb"` forwards compiling commands to the remote builder; `"local"`
    /// runs them in place.
    pub mode: RemoteMode,
    /// Path of the `rrb` executable; a leading `~` is the home directory and
    /// a bare name is looked up on `PATH`.
    pub rrb: String,
}

impl Default for RemoteConfig {
    fn default() -> Self {
        Self {
            mode: RemoteMode::Rrb,
            rrb: DEFAULT_RRB.to_owned(),
        }
    }
}

impl RemoteConfig {
    /// The `rrb` path with a leading `~` replaced by `home`.
    pub fn rrb_path(&self, home: Option<&Path>) -> PathBuf {
        expand_tilde(&self.rrb, home)
    }
}

/// `[remote].mode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum RemoteMode {
    /// Forward compiling commands to the remote builder through `rrb`.
    #[default]
    Rrb,
    /// Run compiling commands on this machine.
    Local,
}

/// `[gate]`: which packages the gate covers and how.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct GateConfig {
    /// Workspace packages left out of fmt, clippy, test and coverage
    /// (vendored code, for example).
    pub exclude: Vec<String>,
    /// Feature selection passed to clippy, test, doc and coverage.
    pub features: Features,
    /// `RUST_TEST_THREADS` for test runs, unless the environment sets it.
    pub test_threads: u32,
    /// Extra clippy arguments after `-- -D warnings`.
    pub clippy_args: Vec<String>,
    /// Run `cargo fmt --check`.
    pub fmt: bool,
    /// Also run `cargo doc --no-deps` with `RUSTDOCFLAGS=-Dwarnings`.
    pub doc: bool,
    /// Dependency rules checked by `cargo sekvent boundaries` and the gate.
    pub boundaries: Vec<Boundary>,
}

impl Default for GateConfig {
    fn default() -> Self {
        Self {
            exclude: Vec::new(),
            features: Features::default(),
            test_threads: DEFAULT_TEST_THREADS,
            clippy_args: Vec::new(),
            fmt: true,
            doc: false,
            boundaries: Vec::new(),
        }
    }
}

/// `[gate].features`: `"all"`, `"default"` or a list of feature names.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(untagged)]
pub enum Features {
    /// `"all"` or `"default"`.
    Set(FeatureSet),
    /// An explicit list, passed as `--features a,b`.
    List(Vec<String>),
}

impl Default for Features {
    fn default() -> Self {
        Self::Set(FeatureSet::All)
    }
}

/// The named feature selections.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum FeatureSet {
    /// `--all-features`.
    All,
    /// The packages' default features.
    Default,
}

/// `[[gate.boundaries]]`: package `from` must not depend on `to`.
///
/// Only normal dependencies count, transitively; dev and build dependencies
/// are ignored.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Boundary {
    /// The workspace package the rule constrains.
    pub from: String,
    /// The package it must not reach.
    pub to: String,
}

/// `[coverage]`.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct CoverageConfig {
    /// Line coverage floor for every measured package, in percent.
    pub fail_under_lines: f64,
    /// Filename regexes left out of the report, on top of the built-ins.
    pub ignore: Vec<String>,
    /// Packages built but not measured.
    pub exclude: Vec<String>,
    /// Per-package floors overriding `fail_under_lines`.
    pub thresholds: BTreeMap<String, f64>,
}

impl Default for CoverageConfig {
    fn default() -> Self {
        Self {
            fail_under_lines: DEFAULT_FAIL_UNDER_LINES,
            ignore: Vec::new(),
            exclude: Vec::new(),
            thresholds: BTreeMap::new(),
        }
    }
}

impl CoverageConfig {
    /// The floor that applies to `package`.
    pub fn threshold_for(&self, package: &str) -> f64 {
        self.thresholds
            .get(package)
            .copied()
            .unwrap_or(self.fail_under_lines)
    }
}

/// `[harness]`: labels and cleanup of test containers.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct HarnessConfig {
    /// Label namespace stamped on, and used to select, harness containers.
    pub label_namespace: String,
    /// Age (humantime, e.g. `6h`) after which another run's containers are
    /// swept before a gate.
    pub stale_after: String,
    /// Run the container-backed tests in `gate`, `test` and `coverage`:
    /// `SEKVENT_DOCKER_TESTS=1` is exported (unless the environment already
    /// sets it) and the test binaries get `--include-ignored`.
    pub docker_tests: bool,
}

impl Default for HarnessConfig {
    fn default() -> Self {
        Self {
            label_namespace: sekvent_testing::DEFAULT_NAMESPACE.to_owned(),
            stale_after: DEFAULT_STALE_AFTER.to_owned(),
            docker_tests: false,
        }
    }
}

impl HarnessConfig {
    /// `stale_after` as a duration.
    pub fn stale_after(&self) -> Result<Duration, humantime::DurationError> {
        humantime::parse_duration(&self.stale_after)
    }
}

/// `[hooks]`: argv lists run in the project root.
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct HooksConfig {
    /// Before the first gate step.
    pub pre_gate: Vec<Vec<String>>,
    /// After the last gate step, only when every step passed.
    pub post_gate: Vec<Vec<String>>,
    /// Before the coverage run.
    pub pre_coverage: Vec<Vec<String>>,
    /// After the coverage report.
    pub post_coverage: Vec<Vec<String>>,
}

/// `[tasks.<name>]`: a custom task.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TaskConfig {
    /// The argv to run; extra CLI arguments are appended.
    pub run: Vec<String>,
    /// The task compiles, so it is forwarded to the remote builder.
    #[serde(default)]
    pub remote: bool,
    /// One line shown by `cargo sekvent run` without a task name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

impl Config {
    /// A configuration with every default and the given project name.
    pub fn with_name(name: &str) -> Self {
        Self {
            project: ProjectConfig {
                name: name.to_owned(),
            },
            remote: RemoteConfig::default(),
            gate: GateConfig::default(),
            coverage: CoverageConfig::default(),
            harness: HarnessConfig::default(),
            hooks: HooksConfig::default(),
            tasks: BTreeMap::new(),
        }
    }

    /// Read and validate `path`.
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Read {
            path: path.to_owned(),
            source,
        })?;
        Self::parse(&text, path)
    }

    /// Parse and validate `text`; `path` only labels errors.
    pub fn parse(text: &str, path: &Path) -> Result<Self, ConfigError> {
        let config: Self = toml::from_str(text).map_err(|error| ConfigError::Parse {
            path: path.to_owned(),
            message: error.to_string(),
        })?;
        config.validate(path)?;
        Ok(config)
    }

    /// Render the effective configuration as TOML.
    pub fn to_toml(&self) -> Result<String, toml::ser::Error> {
        toml::to_string_pretty(self)
    }

    fn validate(&self, path: &Path) -> Result<(), ConfigError> {
        let invalid = |key: &str, reason: String| ConfigError::Invalid {
            path: path.to_owned(),
            key: key.to_owned(),
            reason,
        };
        if self.project.name.trim().is_empty() {
            return Err(invalid("project.name", "must not be empty".into()));
        }
        if self.remote.rrb.trim().is_empty() {
            return Err(invalid("remote.rrb", "must not be empty".into()));
        }
        if self.gate.test_threads == 0 {
            return Err(invalid("gate.test_threads", "must be at least 1".into()));
        }
        if let Features::List(list) = &self.gate.features
            && list.iter().any(|feature| feature.trim().is_empty())
        {
            return Err(invalid(
                "gate.features",
                "must not contain empty names".into(),
            ));
        }
        for boundary in &self.gate.boundaries {
            if boundary.from.is_empty() || boundary.to.is_empty() {
                return Err(invalid(
                    "gate.boundaries",
                    "needs both `from` and `to`".into(),
                ));
            }
            if boundary.from == boundary.to {
                return Err(invalid(
                    "gate.boundaries",
                    format!("`{}` cannot be a boundary to itself", boundary.from),
                ));
            }
        }
        check_percent(self.coverage.fail_under_lines)
            .map_err(|reason| invalid("coverage.fail_under_lines", reason))?;
        for (package, value) in &self.coverage.thresholds {
            check_percent(*value)
                .map_err(|reason| invalid(&format!("coverage.thresholds.{package}"), reason))?;
        }
        for pattern in &self.coverage.ignore {
            regex::Regex::new(pattern)
                .map_err(|error| invalid("coverage.ignore", format!("has a bad regex: {error}")))?;
        }
        sekvent_testing::Harness::with_run_id(&self.harness.label_namespace, Some("validate"))
            .map_err(|error| invalid("harness.label_namespace", error.to_string()))?;
        self.harness.stale_after().map_err(|error| {
            invalid("harness.stale_after", format!("is not a duration: {error}"))
        })?;
        for (key, hooks) in [
            ("hooks.pre_gate", &self.hooks.pre_gate),
            ("hooks.post_gate", &self.hooks.post_gate),
            ("hooks.pre_coverage", &self.hooks.pre_coverage),
            ("hooks.post_coverage", &self.hooks.post_coverage),
        ] {
            if hooks
                .iter()
                .any(|argv| argv.first().is_none_or(String::is_empty))
            {
                return Err(invalid(key, "has an empty command".into()));
            }
        }
        for (name, task) in &self.tasks {
            if name.is_empty() || name.contains(char::is_whitespace) {
                return Err(invalid("tasks", format!("has a bad task name `{name}`")));
            }
            if task.run.first().is_none_or(String::is_empty) {
                return Err(invalid(
                    &format!("tasks.{name}.run"),
                    "must not be empty".into(),
                ));
            }
        }
        Ok(())
    }
}

fn check_percent(value: f64) -> Result<(), String> {
    if value.is_finite() && (0.0..=100.0).contains(&value) {
        Ok(())
    } else {
        Err("must be between 0 and 100".into())
    }
}

/// Replace a leading `~` (alone or followed by `/`) with `home`.
pub fn expand_tilde(value: &str, home: Option<&Path>) -> PathBuf {
    match (value.strip_prefix('~'), home) {
        (Some(""), Some(home)) => home.to_owned(),
        (Some(rest), Some(home)) if rest.starts_with('/') => {
            home.join(rest.trim_start_matches('/'))
        }
        _ => PathBuf::from(value),
    }
}

/// The nearest directory at or above `start` that holds `sekvent.toml`.
pub fn find_project_root(start: &Path) -> Option<PathBuf> {
    start
        .ancestors()
        .find(|dir| dir.join(CONFIG_FILE).is_file())
        .map(Path::to_path_buf)
}

/// The nearest directory at or above `start` whose `Cargo.toml` declares a
/// `[workspace]`.
pub fn find_workspace_root(start: &Path) -> Option<PathBuf> {
    start
        .ancestors()
        .find(|dir| {
            std::fs::read_to_string(dir.join("Cargo.toml"))
                .ok()
                .and_then(|text| text.parse::<toml::Table>().ok())
                .is_some_and(|table| table.contains_key("workspace"))
        })
        .map(Path::to_path_buf)
}

/// A project: its root directory and validated configuration.
#[derive(Debug, Clone, PartialEq)]
pub struct Project {
    /// The directory holding `sekvent.toml`.
    pub root: PathBuf,
    /// The parsed configuration.
    pub config: Config,
}

impl Project {
    /// Find `sekvent.toml` at or above `start` and load it.
    pub fn discover(start: &Path) -> Result<Self, ConfigError> {
        let root = find_project_root(start).ok_or_else(|| ConfigError::NotFound {
            start: start.to_owned(),
        })?;
        let config = Config::load(&root.join(CONFIG_FILE))?;
        Ok(Self { root, config })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FULL: &str = r#"
[project]
name = "orders"

[remote]
mode = "local"
rrb = "/opt/rrb"

[gate]
exclude = ["vendored"]
features = ["a", "b"]
test_threads = 4
clippy_args = ["-A", "clippy::xyz"]
fmt = false
doc = true

[[gate.boundaries]]
from = "orders-api"
to = "orders-db"

[coverage]
fail_under_lines = 90.0
ignore = ["(^|/)src/main\\.rs$"]
exclude = ["orders-proto"]
[coverage.thresholds]
"orders-proto" = 0.0

[harness]
label_namespace = "com.example.harness"
stale_after = "2h"
docker_tests = true

[hooks]
pre_gate = [["echo", "pre"]]
post_gate = [["echo", "post"]]
pre_coverage = [["true"]]
post_coverage = [["true"], ["echo", "done"]]

[tasks.seed]
run = ["cargo", "run", "-p", "orders", "--bin", "seed", "--"]
remote = true
description = "Seed the dev database"
"#;

    fn parse(text: &str) -> Result<Config, ConfigError> {
        Config::parse(text, Path::new("sekvent.toml"))
    }

    #[test]
    fn every_section_parses() {
        let config = parse(FULL).unwrap();
        assert_eq!(config.project.name, "orders");
        assert_eq!(config.remote.mode, RemoteMode::Local);
        assert_eq!(config.remote.rrb, "/opt/rrb");
        assert_eq!(config.gate.exclude, ["vendored"]);
        assert_eq!(
            config.gate.features,
            Features::List(vec!["a".into(), "b".into()])
        );
        assert_eq!(config.gate.test_threads, 4);
        assert_eq!(config.gate.clippy_args, ["-A", "clippy::xyz"]);
        assert!(!config.gate.fmt);
        assert!(config.gate.doc);
        assert_eq!(
            config.gate.boundaries,
            [Boundary {
                from: "orders-api".into(),
                to: "orders-db".into()
            }]
        );
        assert!((config.coverage.fail_under_lines - 90.0).abs() < f64::EPSILON);
        assert_eq!(config.coverage.ignore, ["(^|/)src/main\\.rs$"]);
        assert_eq!(config.coverage.exclude, ["orders-proto"]);
        assert!(config.coverage.threshold_for("orders-proto").abs() < f64::EPSILON);
        assert!((config.coverage.threshold_for("other") - 90.0).abs() < f64::EPSILON);
        assert_eq!(config.harness.label_namespace, "com.example.harness");
        assert_eq!(
            config.harness.stale_after().unwrap(),
            Duration::from_secs(7200)
        );
        assert!(config.harness.docker_tests);
        assert_eq!(config.hooks.pre_gate, [["echo", "pre"]]);
        assert_eq!(config.hooks.post_coverage.len(), 2);
        let seed = &config.tasks["seed"];
        assert!(seed.remote);
        assert_eq!(seed.description.as_deref(), Some("Seed the dev database"));
        assert_eq!(seed.run[0], "cargo");
    }

    #[test]
    fn defaults_fill_everything_but_the_name() {
        let config = parse("[project]\nname = \"orders\"\n").unwrap();
        assert_eq!(config, Config::with_name("orders"));
        assert_eq!(config.remote.mode, RemoteMode::Rrb);
        assert_eq!(config.remote.rrb, DEFAULT_RRB);
        assert_eq!(config.gate.features, Features::Set(FeatureSet::All));
        assert_eq!(config.gate.test_threads, 8);
        assert!(config.gate.fmt);
        assert!(!config.gate.doc);
        assert!((config.coverage.fail_under_lines - 95.0).abs() < f64::EPSILON);
        assert_eq!(config.harness.label_namespace, "io.sekvent.harness");
        assert_eq!(
            config.harness.stale_after().unwrap(),
            Duration::from_hours(6)
        );
        assert!(!config.harness.docker_tests);
        assert!(config.tasks.is_empty());
    }

    #[test]
    fn named_feature_sets_parse() {
        let config = parse("[project]\nname = \"x\"\n[gate]\nfeatures = \"default\"\n").unwrap();
        assert_eq!(config.gate.features, Features::Set(FeatureSet::Default));
        assert!(parse("[project]\nname = \"x\"\n[gate]\nfeatures = \"some\"\n").is_err());
    }

    #[test]
    fn unknown_keys_are_rejected_in_every_table() {
        for text in [
            "[project]\nname = \"x\"\nextra = 1\n",
            "[project]\nname = \"x\"\n[remote]\nhost = \"a\"\n",
            "[project]\nname = \"x\"\n[gate]\nexcludes = []\n",
            "[project]\nname = \"x\"\n[[gate.boundaries]]\nfrom = \"a\"\nto = \"b\"\nvia = \"c\"\n",
            "[project]\nname = \"x\"\n[coverage]\nfail_under = 1.0\n",
            "[project]\nname = \"x\"\n[harness]\nnamespace = \"a\"\n",
            "[project]\nname = \"x\"\n[harness]\ndocker_test = true\n",
            "[project]\nname = \"x\"\n[hooks]\npre_test = []\n",
            "[project]\nname = \"x\"\n[tasks.a]\nrun = [\"a\"]\nenv = {}\n",
            "[project]\nname = \"x\"\n[unknown]\n",
        ] {
            let error = parse(text).unwrap_err();
            assert!(
                matches!(error, ConfigError::Parse { .. }),
                "{text}: {error}"
            );
        }
    }

    #[test]
    fn a_missing_project_name_is_an_error() {
        assert!(matches!(parse(""), Err(ConfigError::Parse { .. })));
        assert!(matches!(
            parse("[project]\nname = \" \"\n"),
            Err(ConfigError::Invalid { .. })
        ));
    }

    #[test]
    fn out_of_range_values_name_the_key() {
        for (text, key) in [
            ("[gate]\ntest_threads = 0", "gate.test_threads"),
            ("[gate]\nfeatures = [\"\"]", "gate.features"),
            (
                "[[gate.boundaries]]\nfrom = \"a\"\nto = \"a\"",
                "gate.boundaries",
            ),
            (
                "[coverage]\nfail_under_lines = 101.0",
                "coverage.fail_under_lines",
            ),
            ("[coverage.thresholds]\np = -1.0", "coverage.thresholds.p"),
            ("[coverage]\nignore = [\"(\"]", "coverage.ignore"),
            (
                "[harness]\nlabel_namespace = \"bad ns\"",
                "harness.label_namespace",
            ),
            ("[harness]\nstale_after = \"soon\"", "harness.stale_after"),
            ("[hooks]\npre_gate = [[]]", "hooks.pre_gate"),
            ("[tasks.a]\nrun = []", "tasks.a.run"),
            ("[remote]\nrrb = \"\"", "remote.rrb"),
        ] {
            let text = format!("[project]\nname = \"x\"\n{text}\n");
            match parse(&text) {
                Err(ConfigError::Invalid { key: got, .. }) => assert_eq!(got, key),
                other => panic!("{text}: {other:?}"),
            }
        }
    }

    #[test]
    fn the_effective_config_round_trips() {
        let config = parse(FULL).unwrap();
        let text = config.to_toml().unwrap();
        assert_eq!(parse(&text).unwrap(), config);
        let defaults = Config::with_name("orders").to_toml().unwrap();
        assert!(defaults.contains("stale_after = \"6h\""), "{defaults}");
        assert!(defaults.contains("docker_tests = false"), "{defaults}");
    }

    #[test]
    fn tilde_expands_only_at_the_start() {
        let home = Path::new("/home/dev");
        assert_eq!(
            expand_tilde("~/bin/rrb", Some(home)),
            PathBuf::from("/home/dev/bin/rrb")
        );
        assert_eq!(expand_tilde("~", Some(home)), PathBuf::from("/home/dev"));
        assert_eq!(expand_tilde("~other", Some(home)), PathBuf::from("~other"));
        assert_eq!(expand_tilde("/a/~/b", Some(home)), PathBuf::from("/a/~/b"));
        assert_eq!(expand_tilde("~/x", None), PathBuf::from("~/x"));
        assert_eq!(
            RemoteConfig::default().rrb_path(Some(home)),
            PathBuf::from("/home/dev/.kodein/skills/build-on-rtx/bin/rrb")
        );
    }

    #[test]
    fn roots_are_found_upwards() {
        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join("crates/a/src");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(dir.path().join("Cargo.toml"), "[workspace]\nmembers = []\n").unwrap();
        std::fs::write(
            dir.path().join("crates/a/Cargo.toml"),
            "[package]\nname = \"a\"\n",
        )
        .unwrap();
        assert_eq!(find_project_root(&nested), None);
        assert_eq!(find_workspace_root(&nested).unwrap(), dir.path());
        assert!(matches!(
            Project::discover(&nested),
            Err(ConfigError::NotFound { .. })
        ));

        std::fs::write(dir.path().join(CONFIG_FILE), "[project]\nname = \"a\"\n").unwrap();
        let project = Project::discover(&nested).unwrap();
        assert_eq!(project.root, dir.path());
        assert_eq!(project.config.project.name, "a");
    }
}
