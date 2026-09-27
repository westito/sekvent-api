//! The subset of `cargo metadata --format-version 1` the tasks need.
//!
//! Only the fields read here are modelled; the rest of the document is
//! ignored, so newer cargo versions keep parsing.

use std::path::{Path, PathBuf};

use anyhow::{Context as _, bail};
use serde::Deserialize;

use crate::process::{Cmd, Runner};

/// A `cargo metadata` document.
#[derive(Debug, Clone, Deserialize)]
pub struct Metadata {
    /// Every package in the graph (only the workspace with `--no-deps`).
    pub packages: Vec<Package>,
    /// Ids of the workspace members.
    pub workspace_members: Vec<String>,
    /// The resolved dependency graph; absent with `--no-deps`.
    #[serde(default)]
    pub resolve: Option<Resolve>,
    /// The workspace root directory.
    pub workspace_root: PathBuf,
}

/// One package.
#[derive(Debug, Clone, Deserialize)]
pub struct Package {
    /// Package name.
    pub name: String,
    /// Opaque package id.
    pub id: String,
    /// Package version.
    pub version: String,
    /// Path of its `Cargo.toml`.
    pub manifest_path: PathBuf,
}

impl Package {
    /// The directory holding the manifest.
    pub fn dir(&self) -> &Path {
        self.manifest_path.parent().unwrap_or(Path::new(""))
    }
}

/// The resolved graph.
#[derive(Debug, Clone, Deserialize)]
pub struct Resolve {
    /// One node per resolved package.
    pub nodes: Vec<Node>,
}

/// A resolved package and its dependency edges.
#[derive(Debug, Clone, Deserialize)]
pub struct Node {
    /// The package id.
    pub id: String,
    /// Its dependency edges.
    #[serde(default)]
    pub deps: Vec<NodeDep>,
}

/// One dependency edge.
#[derive(Debug, Clone, Deserialize)]
pub struct NodeDep {
    /// The dependency's name as seen by the dependent (after renames).
    pub name: String,
    /// The dependency's package id.
    pub pkg: String,
    /// The kinds this edge is used as.
    #[serde(default)]
    pub dep_kinds: Vec<DepKindInfo>,
}

impl NodeDep {
    /// The edge is a normal (not dev, not build) dependency on some target.
    pub fn is_normal(&self) -> bool {
        self.dep_kinds.is_empty() || self.dep_kinds.iter().any(|kind| kind.kind.is_none())
    }
}

/// How an edge is used.
#[derive(Debug, Clone, Deserialize)]
pub struct DepKindInfo {
    /// `None` for normal, `"dev"` or `"build"` otherwise.
    #[serde(default)]
    pub kind: Option<String>,
    /// A `cfg(...)` or triple the edge is limited to.
    #[serde(default)]
    pub target: Option<String>,
}

impl Metadata {
    /// Parse a `cargo metadata` JSON document.
    pub fn parse(json: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(json)
    }

    /// The `cargo metadata` command run in `root`.
    pub fn command(root: &Path, with_deps: bool) -> Cmd {
        let cmd = Cmd::new("cargo")
            .args(["metadata", "--format-version", "1"])
            .cwd(root);
        if with_deps { cmd } else { cmd.arg("--no-deps") }
    }

    /// Run `cargo metadata` in `root` and parse it. It resolves but never
    /// compiles, so it is safe on any machine.
    pub fn load(runner: &dyn Runner, root: &Path, with_deps: bool) -> anyhow::Result<Self> {
        let cmd = Self::command(root, with_deps);
        let output = runner.output(&cmd, None)?;
        if !output.success() {
            bail!("`{cmd}` failed:\n{}", output.stderr.trim());
        }
        Self::parse(&output.stdout).context("cannot parse `cargo metadata` output")
    }

    /// The workspace members, sorted by name.
    pub fn members(&self) -> Vec<&Package> {
        let mut members: Vec<&Package> = self
            .packages
            .iter()
            .filter(|package| self.workspace_members.contains(&package.id))
            .collect();
        members.sort_by(|a, b| a.name.cmp(&b.name));
        members
    }

    /// Names of the workspace members, sorted.
    pub fn member_names(&self) -> Vec<String> {
        self.members()
            .into_iter()
            .map(|package| package.name.clone())
            .collect()
    }

    /// The workspace member named `name`.
    pub fn member(&self, name: &str) -> Option<&Package> {
        self.members()
            .into_iter()
            .find(|package| package.name == name)
    }

    /// Any package in the graph with this id.
    pub fn package_by_id(&self, id: &str) -> Option<&Package> {
        self.packages.iter().find(|package| package.id == id)
    }

    /// The workspace member whose directory is the longest prefix of `file`.
    pub fn member_for_file(&self, file: &Path) -> Option<&Package> {
        self.members()
            .into_iter()
            .filter(|package| file.starts_with(package.dir()))
            .max_by_key(|package| package.dir().components().count())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixtures::METADATA as FIXTURE;
    use crate::process::fake::FakeRunner;

    #[test]
    fn members_are_sorted_and_exclude_dependencies() {
        let meta = Metadata::parse(FIXTURE).unwrap();
        assert_eq!(
            meta.member_names(),
            ["orders-api", "orders-db", "orders-domain", "vendored"]
        );
        assert!(meta.member("serde").is_none());
        assert!(
            meta.package_by_id("registry+https://github.com/rust-lang/crates.io-index#serde@1.0.0")
                .is_some()
        );
    }

    #[test]
    fn files_map_to_the_longest_manifest_dir() {
        let meta = Metadata::parse(FIXTURE).unwrap();
        let lookup = |path: &str| {
            meta.member_for_file(Path::new(path))
                .map(|package| package.name.clone())
        };
        assert_eq!(
            lookup("/work/crates/api/src/lib.rs").as_deref(),
            Some("orders-api")
        );
        assert_eq!(
            lookup("/work/crates/api/domain/src/lib.rs").as_deref(),
            Some("orders-domain")
        );
        assert_eq!(lookup("/work/crates/api-extra/src/lib.rs"), None);
        assert_eq!(lookup("/elsewhere/lib.rs"), None);
    }

    #[test]
    fn edges_without_kinds_or_with_a_null_kind_are_normal() {
        let meta = Metadata::parse(FIXTURE).unwrap();
        let resolve = meta.resolve.unwrap();
        let api = resolve
            .nodes
            .iter()
            .find(|node| node.id.starts_with("path+file:///work/crates/api#"))
            .unwrap();
        let kinds: Vec<(String, bool)> = api
            .deps
            .iter()
            .map(|dep| (dep.name.clone(), dep.is_normal()))
            .collect();
        assert_eq!(
            kinds,
            [
                ("orders_domain".to_owned(), true),
                ("orders_db".to_owned(), false)
            ]
        );
    }

    #[test]
    fn load_runs_cargo_metadata_and_reports_failures() {
        let runner = FakeRunner::default();
        runner.push_output(FIXTURE);
        let meta = Metadata::load(&runner, Path::new("/work"), false).unwrap();
        assert_eq!(meta.workspace_root, PathBuf::from("/work"));
        assert_eq!(
            runner.lines(),
            ["cargo metadata --format-version 1 --no-deps"]
        );
        assert_eq!(runner.calls()[0].cwd, Some(PathBuf::from("/work")));

        let failing = FakeRunner::default();
        failing
            .outputs
            .borrow_mut()
            .push_back(Ok(crate::process::CmdOutput {
                code: 101,
                stdout: String::new(),
                stderr: "error: no manifest".into(),
            }));
        let error = Metadata::load(&failing, Path::new("/work"), true).unwrap_err();
        assert!(error.to_string().contains("no manifest"), "{error}");
        assert_eq!(failing.lines(), ["cargo metadata --format-version 1"]);
    }
}
