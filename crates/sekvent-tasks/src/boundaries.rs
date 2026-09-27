//! `[[gate.boundaries]]`: forbidden dependency paths between packages.
//!
//! A rule `from -> to` fails when `to` is reachable from the workspace member
//! `from` through normal dependencies, directly or transitively. Dev and build
//! dependencies do not count: they never ship in `from`'s artifact.

use std::collections::{BTreeMap, VecDeque};

use thiserror::Error;

use crate::config::Boundary;
use crate::metadata::Metadata;

/// The check could not be evaluated.
#[derive(Debug, Error, PartialEq, Eq)]
#[non_exhaustive]
pub enum BoundaryError {
    /// The metadata was loaded with `--no-deps`.
    #[error("cargo metadata has no resolved dependency graph")]
    NoResolve,
    /// A rule's `from` is not a workspace member.
    #[error("boundary `from = \"{0}\"` is not a workspace member")]
    UnknownFrom(String),
}

/// A broken rule and the dependency path that breaks it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Violation {
    /// The rule.
    pub rule: Boundary,
    /// Package names from `from` to `to`, both included.
    pub path: Vec<String>,
}

/// The result of checking every rule.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BoundaryReport {
    /// Broken rules.
    pub violations: Vec<Violation>,
    /// `to` names that match no package in the graph (possible typos).
    pub unknown_targets: Vec<String>,
}

/// Check `rules` against the resolved graph in `meta`.
pub fn check(meta: &Metadata, rules: &[Boundary]) -> Result<BoundaryReport, BoundaryError> {
    let resolve = meta.resolve.as_ref().ok_or(BoundaryError::NoResolve)?;
    let edges: BTreeMap<&str, Vec<&str>> = resolve
        .nodes
        .iter()
        .map(|node| {
            let normal = node
                .deps
                .iter()
                .filter(|dep| dep.is_normal())
                .map(|dep| dep.pkg.as_str())
                .collect();
            (node.id.as_str(), normal)
        })
        .collect();
    let name_of = |id: &str| {
        meta.package_by_id(id)
            .map_or_else(|| id.to_owned(), |package| package.name.clone())
    };

    let mut report = BoundaryReport::default();
    for rule in rules {
        let from = meta
            .member(&rule.from)
            .ok_or_else(|| BoundaryError::UnknownFrom(rule.from.clone()))?;
        if !meta.packages.iter().any(|package| package.name == rule.to) {
            report.unknown_targets.push(rule.to.clone());
            continue;
        }
        let mut parent: BTreeMap<&str, &str> = BTreeMap::new();
        let mut queue = VecDeque::from([from.id.as_str()]);
        let mut found = None;
        while let Some(id) = queue.pop_front() {
            if id != from.id && name_of(id) == rule.to {
                found = Some(id);
                break;
            }
            for &next in edges.get(id).map_or(&[][..], Vec::as_slice) {
                if next != from.id && !parent.contains_key(next) {
                    parent.insert(next, id);
                    queue.push_back(next);
                }
            }
        }
        if let Some(mut id) = found {
            let mut path = vec![name_of(id)];
            while let Some(&previous) = parent.get(id) {
                path.push(name_of(previous));
                id = previous;
            }
            path.reverse();
            report.violations.push(Violation {
                rule: rule.clone(),
                path,
            });
        }
    }
    Ok(report)
}

/// Print `report` and return the exit code: 0 when every rule holds.
pub fn print_report(report: &BoundaryReport) -> i32 {
    for target in &report.unknown_targets {
        eprintln!("warning: boundary target `{target}` matches no package in the graph");
    }
    if report.violations.is_empty() {
        println!("boundaries: ok");
        return 0;
    }
    for violation in &report.violations {
        eprintln!(
            "boundary violated: `{}` must not depend on `{}`: {}",
            violation.rule.from,
            violation.rule.to,
            violation.path.join(" -> ")
        );
    }
    1
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixtures::METADATA;

    fn rule(from: &str, to: &str) -> Boundary {
        Boundary {
            from: from.into(),
            to: to.into(),
        }
    }

    fn meta() -> Metadata {
        Metadata::parse(METADATA).unwrap()
    }

    #[test]
    fn transitive_normal_dependencies_violate() {
        let report = check(&meta(), &[rule("orders-api", "orders-db")]).unwrap();
        assert_eq!(
            report.violations,
            [Violation {
                rule: rule("orders-api", "orders-db"),
                path: vec![
                    "orders-api".into(),
                    "orders-domain".into(),
                    "orders-db".into()
                ],
            }]
        );
        assert_eq!(print_report(&report), 1);
    }

    #[test]
    fn third_party_targets_are_checked_too() {
        let report = check(&meta(), &[rule("orders-api", "serde")]).unwrap();
        assert_eq!(report.violations.len(), 1);
        assert_eq!(report.violations[0].path.last().unwrap(), "serde");
    }

    #[test]
    fn dev_and_build_edges_and_reverse_edges_are_fine() {
        let report = check(
            &meta(),
            &[
                rule("orders-api", "cc"),
                rule("orders-db", "orders-api"),
                rule("vendored", "serde"),
            ],
        )
        .unwrap();
        assert_eq!(report, BoundaryReport::default());
        assert_eq!(print_report(&report), 0);
    }

    #[test]
    fn unknown_names_are_reported() {
        let report = check(&meta(), &[rule("orders-api", "orders-dbx")]).unwrap();
        assert_eq!(report.unknown_targets, ["orders-dbx"]);
        assert!(report.violations.is_empty());
        assert_eq!(
            check(&meta(), &[rule("nope", "serde")]),
            Err(BoundaryError::UnknownFrom("nope".into()))
        );
        let mut no_deps = meta();
        no_deps.resolve = None;
        assert_eq!(check(&no_deps, &[]), Err(BoundaryError::NoResolve));
    }
}
