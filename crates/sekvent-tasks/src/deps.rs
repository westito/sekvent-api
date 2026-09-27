//! Third-party version pins shared with sekvent.
//!
//! sekvent pins every third-party dependency once in its root
//! `[workspace.dependencies]`. A project that uses the same crates should
//! agree on version and on the features sekvent relies on; `deps check`
//! reports drift and `deps sync` fixes it in place with `toml_edit`, keeping
//! formatting and comments. Project-only dependencies and extra features the
//! project enabled are never touched.

use std::cmp::Ordering;
use std::fmt;

use anyhow::{Context as _, bail};
use toml_edit::{Array, DocumentMut, InlineTable, Item, Table, TableLike, Value};

use crate::embedded::ROOT_CARGO_TOML;

/// One third-party pin from sekvent's root manifest.
#[derive(Debug, Clone)]
pub struct Pin {
    /// Dependency key.
    pub name: String,
    /// Pinned version requirement.
    pub version: String,
    /// Features sekvent enables.
    pub features: Vec<String>,
    /// The original TOML item, inserted verbatim by `deps sync`.
    pub item: Item,
}

fn is_internal(name: &str) -> bool {
    name == "sekvent" || name.starts_with("sekvent-")
}

fn dependencies(doc: &DocumentMut) -> Option<&dyn TableLike> {
    doc.get("workspace")?.get("dependencies")?.as_table_like()
}

fn string_list(item: Option<&Item>) -> Vec<String> {
    item.and_then(Item::as_array)
        .map_or_else(Vec::new, |array| {
            array
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
}

/// Version and features of a dependency entry; `None` for git or path
/// entries without a version.
fn version_and_features(item: &Item) -> Option<(String, Vec<String>)> {
    if let Some(version) = item.as_str() {
        return Some((version.to_owned(), Vec::new()));
    }
    let table = item.as_table_like()?;
    let version = table.get("version")?.as_str()?.to_owned();
    Some((version, string_list(table.get("features"))))
}

/// The third-party pins of a root manifest.
pub fn parse_pins(manifest: &str) -> anyhow::Result<Vec<Pin>> {
    let doc: DocumentMut = manifest
        .parse()
        .context("cannot parse the pinned manifest")?;
    let Some(deps) = dependencies(&doc) else {
        bail!("the pinned manifest has no [workspace.dependencies]");
    };
    Ok(deps
        .iter()
        .filter(|(name, _)| !is_internal(name))
        .filter_map(|(name, item)| {
            let (version, features) = version_and_features(item)?;
            Some(Pin {
                name: name.to_owned(),
                version,
                features,
                item: item.clone(),
            })
        })
        .collect())
}

/// The pins embedded from sekvent's own root `Cargo.toml`.
pub fn embedded_pins() -> Vec<Pin> {
    parse_pins(ROOT_CARGO_TOML).unwrap_or_default()
}

/// The third-party lines of sekvent's `[workspace.dependencies]`, verbatim,
/// with their section comments, for the `pinned_dependencies` template
/// variable.
pub fn pinned_block(manifest: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    let mut pending: Vec<&str> = Vec::new();
    let mut in_section = false;
    let mut depth: i32 = 0;
    let mut keep_continuation = false;
    for line in manifest.lines() {
        let trimmed = line.trim();
        if depth > 0 {
            if keep_continuation {
                out.push(line);
            }
            depth += bracket_balance(line);
            continue;
        }
        if trimmed.starts_with('[') {
            in_section = trimmed == "[workspace.dependencies]";
            pending.clear();
            continue;
        }
        if !in_section {
            continue;
        }
        if trimmed.is_empty() {
            pending.clear();
            if out.last().is_some_and(|last| !last.trim().is_empty()) {
                out.push("");
            }
            continue;
        }
        if trimmed.starts_with('#') {
            pending.push(line);
            continue;
        }
        let key = trimmed
            .split('=')
            .next()
            .unwrap_or("")
            .trim()
            .trim_matches('"');
        keep_continuation = !is_internal(key);
        if keep_continuation {
            out.append(&mut pending);
            out.push(line);
        } else {
            pending.clear();
        }
        depth = bracket_balance(line);
    }
    while out.last().is_some_and(|last| last.trim().is_empty()) {
        out.pop();
    }
    while out.first().is_some_and(|first| first.trim().is_empty()) {
        out.remove(0);
    }
    out.join("\n")
}

fn bracket_balance(line: &str) -> i32 {
    let mut balance = 0;
    let mut in_string = false;
    for c in line.chars() {
        match c {
            '"' => in_string = !in_string,
            '#' if !in_string => break,
            '[' | '{' if !in_string => balance += 1,
            ']' | '}' if !in_string => balance -= 1,
            _ => {}
        }
    }
    balance
}

/// A version read leniently: `1`, `1.2`, `^1.2.3`, `=0.7.4`. Ranges and
/// wildcards are not comparable and yield `None`.
pub fn lenient_version(requirement: &str) -> Option<semver::Version> {
    let text = requirement
        .trim()
        .trim_start_matches(['^', '=', '~'])
        .trim();
    if text.is_empty() || text.contains([',', '*', '<', '>', ' ']) {
        return None;
    }
    let split = text.find(['-', '+']).unwrap_or(text.len());
    let (core, rest) = text.split_at(split);
    let mut parts: Vec<&str> = core.split('.').collect();
    if parts.is_empty() || parts.len() > 3 {
        return None;
    }
    while parts.len() < 3 {
        parts.push("0");
    }
    semver::Version::parse(&format!("{}{rest}", parts.join("."))).ok()
}

/// What kind of drift a finding reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum FindingKind {
    /// The project pins an older version.
    Older,
    /// The project pins a newer version.
    Newer,
    /// The project lacks features sekvent enables.
    FeatureMismatch,
    /// The project does not declare the dependency.
    Missing,
    /// The project's requirement is not a plain version.
    Unparsed,
}

impl fmt::Display for FindingKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Older => "older",
            Self::Newer => "newer",
            Self::FeatureMismatch => "feature-mismatch",
            Self::Missing => "missing",
            Self::Unparsed => "unparsed",
        })
    }
}

/// One difference between the project and the pins.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    /// Dependency key.
    pub name: String,
    /// The kind of drift.
    pub kind: FindingKind,
    /// Human-readable detail.
    pub detail: String,
}

/// Compare the project's `[workspace.dependencies]` with `pins`.
pub fn check(project_manifest: &str, pins: &[Pin]) -> anyhow::Result<Vec<Finding>> {
    let doc: DocumentMut = project_manifest
        .parse()
        .context("cannot parse the project Cargo.toml")?;
    let deps = dependencies(&doc);
    let mut findings = Vec::new();
    for pin in pins {
        let Some(item) = deps.and_then(|deps| deps.get(&pin.name)) else {
            findings.push(Finding {
                name: pin.name.clone(),
                kind: FindingKind::Missing,
                detail: format!("sekvent pins {}", pin.version),
            });
            continue;
        };
        let Some((version, features)) = version_and_features(item) else {
            continue;
        };
        match (lenient_version(&version), lenient_version(&pin.version)) {
            (Some(ours), Some(pinned)) => {
                let kind = match ours.cmp(&pinned) {
                    Ordering::Less => Some(FindingKind::Older),
                    Ordering::Greater => Some(FindingKind::Newer),
                    Ordering::Equal => None,
                };
                if let Some(kind) = kind {
                    findings.push(Finding {
                        name: pin.name.clone(),
                        kind,
                        detail: format!("{version} (sekvent pins {})", pin.version),
                    });
                }
            }
            _ => findings.push(Finding {
                name: pin.name.clone(),
                kind: FindingKind::Unparsed,
                detail: format!("`{version}` cannot be compared with {}", pin.version),
            }),
        }
        let missing: Vec<&str> = pin
            .features
            .iter()
            .filter(|feature| !features.contains(feature))
            .map(String::as_str)
            .collect();
        if !missing.is_empty() {
            findings.push(Finding {
                name: pin.name.clone(),
                kind: FindingKind::FeatureMismatch,
                detail: format!("lacks features {}", missing.join(", ")),
            });
        }
    }
    Ok(findings)
}

/// Print findings and return the exit code: 1 in `strict` mode when a pin is
/// older or missing, else 0.
pub fn print_findings(findings: &[Finding], strict: bool) -> i32 {
    if findings.is_empty() {
        println!("deps: in line with sekvent's pins");
        return 0;
    }
    let width = findings.iter().map(|f| f.name.len()).max().unwrap_or(0);
    for finding in findings {
        println!(
            "{:<width$}  {:<16}  {}",
            finding.name,
            finding.kind.to_string(),
            finding.detail
        );
    }
    let failing = findings
        .iter()
        .any(|f| matches!(f.kind, FindingKind::Older | FindingKind::Missing));
    i32::from(strict && failing)
}

fn with_decor(mut value: Value, old: &Value) -> Value {
    *value.decor_mut() = old.decor().clone();
    value
}

fn clean_item(item: &Item) -> Item {
    let mut item = item.clone();
    if let Some(value) = item.as_value_mut() {
        value.decor_mut().clear();
    }
    item
}

/// Bring `project_manifest` in line with `pins`, limited to `only` when given.
///
/// Returns the new text and one line per change. Versions are set to the
/// pin, missing features appended, missing dependencies added verbatim;
/// nothing is removed.
pub fn sync(
    project_manifest: &str,
    pins: &[Pin],
    only: Option<&[String]>,
) -> anyhow::Result<(String, Vec<String>)> {
    let mut doc: DocumentMut = project_manifest
        .parse()
        .context("cannot parse the project Cargo.toml")?;
    if let Some(only) = only
        && let Some(unknown) = only
            .iter()
            .find(|name| !pins.iter().any(|pin| &pin.name == *name))
    {
        bail!("`{unknown}` is not a sekvent pin");
    }
    let root = doc.as_table_mut();
    if !root.contains_key("workspace") {
        let mut workspace = Table::new();
        workspace.set_implicit(true);
        root.insert("workspace", Item::Table(workspace));
    }
    let workspace = root
        .get_mut("workspace")
        .and_then(Item::as_table_like_mut)
        .context("`workspace` is not a table")?;
    if !workspace.contains_key("dependencies") {
        workspace.insert("dependencies", Item::Table(Table::new()));
    }
    let deps = workspace
        .get_mut("dependencies")
        .and_then(Item::as_table_like_mut)
        .context("`workspace.dependencies` is not a table")?;

    let mut changes = Vec::new();
    for pin in pins {
        if only.is_some_and(|only| !only.contains(&pin.name)) {
            continue;
        }
        let Some(item) = deps.get_mut(&pin.name) else {
            deps.insert(&pin.name, clean_item(&pin.item));
            changes.push(format!("added {}", pin.name));
            continue;
        };
        sync_entry(item, pin, &mut changes);
    }
    Ok((doc.to_string(), changes))
}

fn sync_entry(item: &mut Item, pin: &Pin, changes: &mut Vec<String>) {
    let comparable = |version: &str| {
        lenient_version(version).is_some() && lenient_version(&pin.version).is_some()
    };
    if let Some(version) = item.as_str().map(str::to_owned) {
        let Some(value) = item.as_value_mut() else {
            return;
        };
        if pin.features.is_empty() {
            if version != pin.version && comparable(&version) {
                *value = with_decor(Value::from(pin.version.as_str()), value);
                changes.push(format!("{}: {version} -> {}", pin.name, pin.version));
            }
            return;
        }
        let target = if comparable(&version) {
            pin.version.as_str()
        } else {
            version.as_str()
        };
        let mut table = InlineTable::new();
        table.insert("version", Value::from(target));
        table.insert(
            "features",
            Value::Array(pin.features.iter().map(String::as_str).collect::<Array>()),
        );
        *value = with_decor(Value::InlineTable(table), value);
        changes.push(format!(
            "{}: {version} -> {target} with features {}",
            pin.name,
            pin.features.join(", ")
        ));
        return;
    }
    let Some(table) = item.as_table_like_mut() else {
        return;
    };
    let Some(version) = table
        .get("version")
        .and_then(Item::as_str)
        .map(str::to_owned)
    else {
        return;
    };
    if version != pin.version
        && comparable(&version)
        && let Some(value) = table.get_mut("version").and_then(Item::as_value_mut)
    {
        *value = with_decor(Value::from(pin.version.as_str()), value);
        changes.push(format!("{}: {version} -> {}", pin.name, pin.version));
    }
    let present = string_list(table.get("features"));
    let missing: Vec<&String> = pin
        .features
        .iter()
        .filter(|feature| !present.contains(feature))
        .collect();
    if missing.is_empty() {
        return;
    }
    if let Some(array) = table.get_mut("features").and_then(Item::as_array_mut) {
        for feature in &missing {
            array.push(feature.as_str());
        }
    } else {
        let array: Array = missing.iter().map(|feature| feature.as_str()).collect();
        table.insert("features", toml_edit::value(array));
    }
    changes.push(format!(
        "{}: added features {}",
        pin.name,
        missing
            .iter()
            .map(|feature| feature.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    ));
}

#[cfg(test)]
mod tests {
    use super::*;

    const PINS: &str = r#"[workspace]
members = ["crates/*"]

[workspace.dependencies]
# --- internal ---
sekvent-config = { path = "crates/sekvent-config" }

# --- async ---
tokio = { version = "1.53.1", features = ["full"] }
futures = "0.3.34"
# One TLS stack.
reqwest = { version = "0.13.5", default-features = false, features = ["rustls", "json"] }
tower = { version = "0.5.3", features = [
    "util",
    "timeout",
] }
rust_decimal = "1"

[workspace.lints.rust]
unsafe_code = "deny"
"#;

    const PROJECT: &str = r#"[workspace]
members = ["crates/*"]

[workspace.dependencies]
# Runtime; keep in step with sekvent.
tokio = { version = "1.40", features = ["macros"] } # trailing note
futures = "0.3.40"   # newer on purpose
reqwest = { version = "0.13.5", default-features = false, features = ["rustls", "json", "stream"] }
tower = "0.5.3"
rust_decimal = ">=1, <2"
anyhow = "1"
"#;

    fn pins() -> Vec<Pin> {
        parse_pins(PINS).unwrap()
    }

    #[test]
    fn pins_skip_internal_crates() {
        let names: Vec<String> = pins().into_iter().map(|pin| pin.name).collect();
        assert_eq!(
            names,
            ["tokio", "futures", "reqwest", "tower", "rust_decimal"]
        );
        let tokio = &pins()[0];
        assert_eq!(tokio.version, "1.53.1");
        assert_eq!(tokio.features, ["full"]);
        assert!(!embedded_pins().is_empty());
        assert!(
            embedded_pins()
                .iter()
                .all(|pin| !pin.name.starts_with("sekvent"))
        );
    }

    #[test]
    fn the_pinned_block_is_verbatim_without_internal_lines() {
        assert_eq!(
            pinned_block(PINS),
            "# --- async ---\n\
             tokio = { version = \"1.53.1\", features = [\"full\"] }\n\
             futures = \"0.3.34\"\n\
             # One TLS stack.\n\
             reqwest = { version = \"0.13.5\", default-features = false, features = [\"rustls\", \"json\"] }\n\
             tower = { version = \"0.5.3\", features = [\n    \"util\",\n    \"timeout\",\n] }\n\
             rust_decimal = \"1\""
        );
        let embedded = pinned_block(ROOT_CARGO_TOML);
        assert!(embedded.contains("tokio = "), "{embedded}");
        assert!(!embedded.contains("sekvent-config"), "{embedded}");
        assert!(!embedded.contains("[workspace"), "{embedded}");
    }

    #[test]
    fn versions_are_read_leniently() {
        let v = |text: &str| lenient_version(text).map(|version| version.to_string());
        assert_eq!(v("1").as_deref(), Some("1.0.0"));
        assert_eq!(v("^0.7").as_deref(), Some("0.7.0"));
        assert_eq!(v("=1.2.3").as_deref(), Some("1.2.3"));
        assert_eq!(v("1.0.0-rc.1").as_deref(), Some("1.0.0-rc.1"));
        assert_eq!(v("2.0.0-alpha").as_deref(), Some("2.0.0-alpha"));
        assert_eq!(v(">=1, <2"), None);
        assert_eq!(v("*"), None);
        assert_eq!(v("1.2.3.4"), None);
        assert_eq!(v("x"), None);
    }

    #[test]
    fn check_reports_each_kind_of_drift() {
        let findings = check(PROJECT, &pins()).unwrap();
        let summary: Vec<(&str, FindingKind)> = findings
            .iter()
            .map(|finding| (finding.name.as_str(), finding.kind))
            .collect();
        assert_eq!(
            summary,
            [
                ("tokio", FindingKind::Older),
                ("tokio", FindingKind::FeatureMismatch),
                ("futures", FindingKind::Newer),
                ("tower", FindingKind::FeatureMismatch),
                ("rust_decimal", FindingKind::Unparsed),
            ]
        );
        assert_eq!(findings[1].detail, "lacks features full");
        assert_eq!(print_findings(&findings, false), 0);
        assert_eq!(print_findings(&findings, true), 1);
        assert_eq!(print_findings(&[], true), 0);

        let empty = check("[workspace]\n", &pins()).unwrap();
        assert!(
            empty
                .iter()
                .all(|finding| finding.kind == FindingKind::Missing)
        );
        assert_eq!(empty.len(), 5);
    }

    #[test]
    fn sync_updates_in_place_and_keeps_comments() {
        let (text, changes) = sync(PROJECT, &pins(), None).unwrap();
        assert_eq!(
            changes,
            [
                "tokio: 1.40 -> 1.53.1",
                "tokio: added features full",
                "futures: 0.3.40 -> 0.3.34",
                "tower: 0.5.3 -> 0.5.3 with features util, timeout",
            ]
        );
        assert!(
            text.contains("# Runtime; keep in step with sekvent.\n"),
            "{text}"
        );
        assert!(
            text.contains(
                "tokio = { version = \"1.53.1\", features = [\"macros\", \"full\"] } # trailing note"
            ),
            "{text}"
        );
        assert!(
            text.contains("futures = \"0.3.34\"   # newer on purpose"),
            "{text}"
        );
        assert!(
            text.contains("features = [\"rustls\", \"json\", \"stream\"]"),
            "{text}"
        );
        assert!(
            text.contains("tower = { version = \"0.5.3\", features = [\"util\", \"timeout\"] }"),
            "{text}"
        );
        assert!(text.contains("rust_decimal = \">=1, <2\""), "{text}");
        assert!(text.contains("anyhow = \"1\""), "{text}");
        assert!(
            check(&text, &pins())
                .unwrap()
                .iter()
                .all(|finding| finding.kind == FindingKind::Unparsed)
        );
        let (again, none) = sync(&text, &pins(), None).unwrap();
        assert!(none.is_empty(), "{none:?}");
        assert_eq!(again, text);
    }

    #[test]
    fn sync_adds_missing_entries_and_honours_only() {
        let only = ["futures".to_owned()];
        let (text, changes) = sync("[package]\nname = \"x\"\n", &pins(), Some(&only[..])).unwrap();
        assert_eq!(changes, ["added futures"]);
        assert!(
            text.contains("[workspace.dependencies]\nfutures = \"0.3.34\"\n"),
            "{text}"
        );
        assert!(text.starts_with("[package]\nname = \"x\"\n"), "{text}");
        let nope = ["nope".to_owned()];
        assert!(sync(PROJECT, &pins(), Some(&nope[..])).is_err());

        let reqwest = ["reqwest".to_owned()];
        let (text, _) = sync("[workspace]\n", &pins(), Some(&reqwest[..])).unwrap();
        assert!(
            text.contains(
                "reqwest = { version = \"0.13.5\", default-features = false, features = [\"rustls\", \"json\"] }"
            ),
            "{text}"
        );
    }
}
