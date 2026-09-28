//! Per-package line coverage from `cargo llvm-cov report --json`.
//!
//! Every file in the export is attributed to the workspace member whose
//! directory is the longest prefix of its path; files outside the workspace
//! are dropped. Each package is then held to its own floor.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::Path;

use serde::Deserialize;

use crate::config::CoverageConfig;
use crate::metadata::Metadata;
use crate::plan::Selection;

/// Filename regexes always left out of the report, anchored to the
/// workspace root or the target directory so a checkout that happens to sit
/// under a directory named `tests/` or `build/` keeps its files: tests,
/// benches and examples inside the workspace, the target dir and
/// build-script output (`OUT_DIR`). llvm-cov uses POSIX extended syntax, so
/// these avoid non-capturing groups.
pub fn builtin_ignore(workspace_root: &Path, target_dir: &Path) -> Vec<String> {
    let root = escape_ere(&display_dir(workspace_root));
    let target = escape_ere(&display_dir(target_dir));
    vec![
        format!("^{root}/(.*/)?(tests|benches|examples)/"),
        format!("^{target}/"),
        format!("^({root}|{target})/(.*/)?build/[^/]+/out/"),
    ]
}

/// The `--ignore-filename-regex` value: [`builtin_ignore`] plus
/// `[coverage].ignore`, which is used as written.
pub fn ignore_regex(workspace_root: &Path, target_dir: &Path, config: &CoverageConfig) -> String {
    builtin_ignore(workspace_root, target_dir)
        .iter()
        .map(String::as_str)
        .chain(config.ignore.iter().map(String::as_str))
        .map(|pattern| format!("({pattern})"))
        .collect::<Vec<_>>()
        .join("|")
}

fn display_dir(dir: &Path) -> String {
    dir.display().to_string().trim_end_matches('/').to_owned()
}

/// Escape the POSIX extended regex metacharacters of `text` with a
/// backslash; everything else is copied as is.
pub fn escape_ere(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if matches!(
            c,
            '.' | '[' | ']' | '{' | '}' | '(' | ')' | '\\' | '*' | '+' | '?' | '^' | '$' | '|'
        ) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// A `llvm.coverage.json.export` document.
#[derive(Debug, Clone, Deserialize)]
pub struct Export {
    /// One entry per exported object set; cargo-llvm-cov emits one.
    pub data: Vec<ExportData>,
}

/// The files of one export.
#[derive(Debug, Clone, Deserialize)]
pub struct ExportData {
    /// Per-file coverage.
    pub files: Vec<FileCoverage>,
}

/// Coverage of one source file.
#[derive(Debug, Clone, Deserialize)]
pub struct FileCoverage {
    /// Absolute path of the file.
    pub filename: String,
    /// Totals for the file.
    pub summary: FileSummary,
    /// Raw segments; absent with `--summary-only`.
    #[serde(default)]
    pub segments: Vec<Vec<serde_json::Value>>,
}

/// A file's totals.
#[derive(Debug, Clone, Deserialize)]
pub struct FileSummary {
    /// Line totals.
    pub lines: Counts,
}

/// Instrumented and covered counts.
#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
pub struct Counts {
    /// Instrumented lines.
    pub count: u64,
    /// Lines executed at least once.
    pub covered: u64,
}

impl Export {
    /// Parse an export document.
    pub fn parse(json: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(json)
    }

    /// Every file of every export entry.
    pub fn files(&self) -> impl Iterator<Item = &FileCoverage> {
        self.data.iter().flat_map(|data| data.files.iter())
    }
}

/// Line coverage of one package against its floor.
#[derive(Debug, Clone, PartialEq)]
pub struct PackageCoverage {
    /// Package name.
    pub package: String,
    /// Summed line counts over its files.
    pub lines: Counts,
    /// The floor that applies, in percent.
    pub threshold: f64,
}

impl PackageCoverage {
    /// Covered lines in percent; `None` without instrumented lines.
    #[allow(clippy::cast_precision_loss)]
    pub fn percent(&self) -> Option<f64> {
        (self.lines.count > 0).then(|| self.lines.covered as f64 * 100.0 / self.lines.count as f64)
    }

    /// At or above the floor; a package without data passes (see
    /// [`no_data_error`] for the case where no package has any).
    pub fn passes(&self) -> bool {
        self.percent()
            .is_none_or(|percent| percent + 1e-9 >= self.threshold)
    }
}

/// Aggregate the export per selected package, skipping `[coverage].exclude`.
pub fn aggregate(
    export: &Export,
    meta: &Metadata,
    selection: &Selection,
    config: &CoverageConfig,
) -> Vec<PackageCoverage> {
    let mut totals: BTreeMap<String, Counts> = selection
        .members
        .iter()
        .filter(|name| !config.exclude.contains(name))
        .map(|name| (name.clone(), Counts::default()))
        .collect();
    for file in export.files() {
        let Some(package) = meta.member_for_file(Path::new(&file.filename)) else {
            continue;
        };
        if let Some(total) = totals.get_mut(&package.name) {
            total.count += file.summary.lines.count;
            total.covered += file.summary.lines.covered;
        }
    }
    totals
        .into_iter()
        .map(|(package, lines)| PackageCoverage {
            threshold: config.threshold_for(&package),
            package,
            lines,
        })
        .collect()
}

/// An explanation when the export has no instrumented line for any of the
/// `rows` (usually every file was filtered out, or nothing was attributed to
/// a workspace member); `None` when at least one package has data or there
/// are no rows at all.
pub fn no_data_error(rows: &[PackageCoverage], ignore_regex: &str) -> Option<String> {
    if rows.is_empty() || rows.iter().any(|row| row.lines.count > 0) {
        return None;
    }
    let names: Vec<&str> = rows.iter().map(|row| row.package.as_str()).collect();
    Some(format!(
        "the coverage report has no instrumented lines for any selected package ({}); \
         check that the tests ran and that `[coverage].ignore` does not match every \
         source file (the filename filter was `{ignore_regex}`)",
        names.join(", ")
    ))
}

/// Render the per-package table.
pub fn render_table(rows: &[PackageCoverage]) -> String {
    let width = rows
        .iter()
        .map(|row| row.package.len())
        .max()
        .unwrap_or(0)
        .max("package".len());
    let mut out = format!(
        "{:<width$}  {:>7}  {:>7}  {:>7}  {:>7}\n",
        "package", "lines", "covered", "cover", "floor"
    );
    for row in rows {
        let cover = row
            .percent()
            .map_or_else(|| "no data".to_owned(), |percent| format!("{percent:.2}%"));
        let verdict = if row.passes() { "" } else { "  FAIL" };
        let _ = writeln!(
            out,
            "{:<width$}  {:>7}  {:>7}  {:>7}  {:>6.1}%{verdict}",
            row.package, row.lines.count, row.lines.covered, cover, row.threshold
        );
    }
    out
}

/// One llvm-cov segment, decoded from
/// `[line, col, count, has_count, is_region_entry, is_gap_region]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Segment {
    /// 1-based line.
    pub line: u64,
    /// Execution count.
    pub count: u64,
    /// The segment carries a count.
    pub has_count: bool,
    /// The segment starts a region.
    pub is_region_entry: bool,
    /// The segment is a gap region.
    pub is_gap_region: bool,
}

impl Segment {
    /// Decode a raw JSON segment; `None` when it is malformed.
    pub fn from_raw(raw: &[serde_json::Value]) -> Option<Self> {
        Some(Self {
            line: raw.first()?.as_u64()?,
            count: raw.get(2)?.as_u64()?,
            has_count: raw.get(3)?.as_bool()?,
            is_region_entry: raw.get(4)?.as_bool()?,
            is_gap_region: raw
                .get(5)
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false),
        })
    }

    fn starts_region(&self) -> bool {
        !self.is_gap_region && self.has_count && self.is_region_entry
    }
}

/// Lines that are instrumented but never executed, following llvm-cov's own
/// line model: a line is mapped when a counted region starts on it or a
/// counted region wraps into it, and its count is the maximum of those.
pub fn missed_lines(segments: &[Segment]) -> Vec<u64> {
    let (Some(first), Some(last)) = (segments.first(), segments.last()) else {
        return Vec::new();
    };
    let mut missed = Vec::new();
    let mut wrapped: Option<&Segment> = None;
    let mut index = 0;
    for line in first.line..=last.line {
        let start = index;
        while index < segments.len() && segments[index].line == line {
            index += 1;
        }
        let on_line = &segments[start..index];
        let region_starts = on_line
            .iter()
            .filter(|segment| segment.starts_region())
            .count();
        let skipped = on_line
            .first()
            .is_some_and(|segment| !segment.has_count && segment.is_region_entry);
        let mapped =
            !skipped && (wrapped.is_some_and(|segment| segment.has_count) || region_starts > 0);
        if mapped {
            let count = on_line
                .iter()
                .filter(|segment| segment.starts_region())
                .map(|segment| segment.count)
                .chain(wrapped.map(|segment| segment.count))
                .max()
                .unwrap_or(0);
            if count == 0 {
                missed.push(line);
            }
        }
        if let Some(segment) = on_line.last() {
            wrapped = Some(segment);
        }
    }
    missed
}

/// Collapse sorted line numbers into inclusive ranges.
pub fn ranges(lines: &[u64]) -> Vec<(u64, u64)> {
    let mut out: Vec<(u64, u64)> = Vec::new();
    for &line in lines {
        match out.last_mut() {
            Some((_, end)) if *end + 1 == line => *end = line,
            _ => out.push((line, line)),
        }
    }
    out
}

/// Missed line ranges per file of `package`, paths relative to the package.
pub fn misses_for_package(
    export: &Export,
    meta: &Metadata,
    package: &str,
) -> Vec<(String, Vec<(u64, u64)>)> {
    let mut out = Vec::new();
    for file in export.files() {
        let path = Path::new(&file.filename);
        let Some(owner) = meta.member_for_file(path) else {
            continue;
        };
        if owner.name != package {
            continue;
        }
        let segments: Vec<Segment> = file
            .segments
            .iter()
            .filter_map(|raw| Segment::from_raw(raw.as_slice()))
            .collect();
        let missed = ranges(&missed_lines(&segments));
        if !missed.is_empty() {
            let relative = path
                .strip_prefix(owner.dir())
                .unwrap_or(path)
                .display()
                .to_string();
            out.push((relative, missed));
        }
    }
    out.sort();
    out
}

/// Render `misses_for_package` output, one file per line.
pub fn render_misses(misses: &[(String, Vec<(u64, u64)>)]) -> String {
    let mut out = String::new();
    for (file, ranges) in misses {
        let list: Vec<String> = ranges
            .iter()
            .map(|&(start, end)| {
                if start == end {
                    start.to_string()
                } else {
                    format!("{start}-{end}")
                }
            })
            .collect();
        let _ = writeln!(out, "{file}: {}", list.join(", "));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::GateConfig;
    use crate::fixtures::{COVERAGE, METADATA};

    fn setup() -> (Export, Metadata, Selection) {
        let meta = Metadata::parse(METADATA).unwrap();
        let gate = GateConfig {
            exclude: vec!["vendored".into()],
            ..GateConfig::default()
        };
        let selection = Selection::new(&meta, &gate).unwrap();
        (Export::parse(COVERAGE).unwrap(), meta, selection)
    }

    fn seg(line: u64, count: u64, has_count: bool, entry: bool, gap: bool) -> Segment {
        Segment {
            line,
            count,
            has_count,
            is_region_entry: entry,
            is_gap_region: gap,
        }
    }

    #[test]
    fn the_ignore_regex_combines_builtins_and_config() {
        let config = CoverageConfig {
            ignore: vec![r"(^|/)src/main\.rs$".into()],
            ..CoverageConfig::default()
        };
        let combined = ignore_regex(Path::new("/work"), Path::new("/work/target"), &config);
        assert_eq!(
            combined,
            concat!(
                r"(^/work/(.*/)?(tests|benches|examples)/)|(^/work/target/)|",
                r"(^(/work|/work/target)/(.*/)?build/[^/]+/out/)|((^|/)src/main\.rs$)"
            )
        );
        let regex = regex::Regex::new(&combined).unwrap();
        assert!(regex.is_match("/work/crates/a/tests/it.rs"));
        assert!(regex.is_match("/work/tests/it.rs"));
        assert!(regex.is_match("/work/target/debug/build/a-0123/out/gen.rs"));
        assert!(regex.is_match("/work/crates/a/src/main.rs"));
        assert!(!regex.is_match("/work/crates/a/src/lib.rs"));
    }

    #[test]
    fn builtins_ignore_ancestors_of_the_workspace() {
        let root = Path::new("/home/u/tests/build/x/out/my.proj");
        let target = Path::new("/cache/target/");
        let combined = ignore_regex(root, target, &CoverageConfig::default());
        let regex = regex::Regex::new(&combined).unwrap();
        assert!(!regex.is_match("/home/u/tests/build/x/out/my.proj/crates/a/src/lib.rs"));
        assert!(regex.is_match("/home/u/tests/build/x/out/my.proj/crates/a/tests/it.rs"));
        assert!(regex.is_match("/home/u/tests/build/x/out/my.proj/examples/demo.rs"));
        assert!(regex.is_match("/cache/target/llvm-cov-target/debug/build/a-1/out/gen.rs"));
        assert!(!regex.is_match("/home/u/tests/build/x/out/myxproj/crates/a/src/lib.rs"));
        assert!(
            combined.starts_with(r"(^/home/u/tests/build/x/out/my\.proj/"),
            "{combined}"
        );
        assert!(combined.contains("(^/cache/target/)"), "{combined}");
    }

    #[test]
    fn regex_metacharacters_in_paths_are_escaped() {
        assert_eq!(
            escape_ere("/a.b[c]{d}(e)\\f*g+h?i^j$k|l-m"),
            r"/a\.b\[c\]\{d\}\(e\)\\f\*g\+h\?i\^j\$k\|l-m"
        );
        let raw = "/w (1)/a+b";
        let regex = regex::Regex::new(&format!("^{}$", escape_ere(raw))).unwrap();
        assert!(regex.is_match(raw));
        assert_eq!(display_dir(Path::new("/")), "");
        assert_eq!(
            builtin_ignore(Path::new("/"), Path::new("/t"))[0],
            "^/(.*/)?(tests|benches|examples)/"
        );
    }

    #[test]
    fn a_report_without_any_data_is_an_error() {
        let row = |package: &str, count: u64| PackageCoverage {
            package: package.to_owned(),
            lines: Counts { count, covered: 0 },
            threshold: 95.0,
        };
        assert_eq!(no_data_error(&[], "x"), None);
        assert_eq!(no_data_error(&[row("a", 0), row("b", 3)], "x"), None);
        let error = no_data_error(&[row("a", 0), row("b", 0)], "RE").unwrap();
        assert!(error.contains("(a, b)"), "{error}");
        assert!(error.contains("`RE`"), "{error}");
        assert!(render_table(&[row("a", 0)]).contains("no data"));
    }

    #[test]
    fn files_aggregate_per_package() {
        let (export, meta, selection) = setup();
        let mut config = CoverageConfig::default();
        config.thresholds.insert("orders-db".into(), 50.0);
        let rows = aggregate(&export, &meta, &selection, &config);
        let summary: Vec<(&str, u64, u64)> = rows
            .iter()
            .map(|row| (row.package.as_str(), row.lines.count, row.lines.covered))
            .collect();
        assert_eq!(
            summary,
            [
                ("orders-api", 30, 29),
                ("orders-db", 10, 6),
                ("orders-domain", 0, 0)
            ]
        );
        assert!(rows[0].passes());
        assert!((rows[0].percent().unwrap() - 96.666_666).abs() < 1e-3);
        assert!(rows[1].passes());
        assert!((rows[1].threshold - 50.0).abs() < f64::EPSILON);
        assert_eq!(rows[2].percent(), None);
        assert!(rows[2].passes());

        let table = render_table(&rows);
        assert!(table.starts_with("package"), "{table}");
        assert!(table.contains("96.67%"), "{table}");
        assert!(!table.contains("FAIL"), "{table}");
    }

    #[test]
    fn packages_under_their_floor_fail_and_excluded_ones_vanish() {
        let (export, meta, selection) = setup();
        let config = CoverageConfig {
            exclude: vec!["orders-domain".into()],
            ..CoverageConfig::default()
        };
        let rows = aggregate(&export, &meta, &selection, &config);
        let names: Vec<&str> = rows.iter().map(|row| row.package.as_str()).collect();
        assert_eq!(names, ["orders-api", "orders-db"]);
        assert!(rows[0].passes());
        assert!(!rows[1].passes());
        assert!(render_table(&rows).contains("FAIL"));
    }

    #[test]
    fn missed_lines_follow_the_llvm_line_model() {
        let segments = [
            seg(1, 5, true, true, false),
            seg(2, 0, true, true, false),
            seg(3, 5, true, false, false),
            seg(5, 0, false, true, false),
            seg(6, 0, true, true, true),
            seg(7, 1, true, true, false),
            seg(8, 0, true, false, false),
        ];
        // Line 2 starts a zero-count region but a count-5 region wraps into
        // it; line 3 starts nothing and inherits line 2's zero count; line 5
        // is skipped; line 6 holds only a gap region after an uncounted one.
        assert_eq!(missed_lines(&segments), [3]);
        assert!(missed_lines(&[]).is_empty());
    }

    #[test]
    fn ranges_collapse_consecutive_lines() {
        assert_eq!(ranges(&[1, 2, 3, 7, 9, 10]), [(1, 3), (7, 7), (9, 10)]);
        assert!(ranges(&[]).is_empty());
    }

    #[test]
    fn misses_are_listed_per_file() {
        let (export, meta, _) = setup();
        let misses = misses_for_package(&export, &meta, "orders-db");
        assert_eq!(misses, [("src/lib.rs".to_owned(), vec![(3, 4)])]);
        assert_eq!(render_misses(&misses), "src/lib.rs: 3-4\n");
        assert!(misses_for_package(&export, &meta, "orders-api").is_empty());
        let raw = [serde_json::json!(1), serde_json::json!(2)];
        assert_eq!(Segment::from_raw(&raw), None);
    }
}
