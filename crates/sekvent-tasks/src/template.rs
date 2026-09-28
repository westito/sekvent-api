//! The template renderer and the embedded template kinds.
//!
//! # Contract
//!
//! - Kinds are the top-level directories of the repository's `templates/`:
//!   `workspace`, `service-grpc`, `service-http`, `service-worker`,
//!   `ci-github`, `ci-bitbucket` and `agents`.
//! - Paths inside a kind are relative to the project root and may contain
//!   placeholders in any segment.
//! - A trailing `.tmpl` is stripped and the content rendered; other files are
//!   copied byte for byte (their paths are still rendered).
//! - Placeholders are `{{name}}` with no inner spaces. An unknown name is an
//!   error; a literal `{{` is written `{{{{`.
//! - Files named `*.sh` (after stripping `.tmpl`) are made executable.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use include_dir::Dir;
use thiserror::Error;

use crate::deps::pinned_block;
use crate::embedded::{EDITION, ROOT_CARGO_TOML, RRB_IMAGE, TEMPLATES, rust_version};
use crate::sdk::{SDK_PACKAGES, SEKVENT_BRANCH, SEKVENT_GIT_URL};

/// Every template kind the CLI renders.
pub const KINDS: [&str; 7] = [
    "workspace",
    "service-grpc",
    "service-http",
    "service-worker",
    "ci-github",
    "ci-bitbucket",
    "agents",
];

/// Every variable a template may use.
pub const VARIABLES: [&str; 13] = [
    "project_name",
    "crate_name",
    "crate_ident",
    "service_pascal",
    "proto_package",
    "proto_package_path",
    "rust_version",
    "edition",
    "rrb_image",
    "sekvent_git_url",
    "sekvent_deps",
    "pinned_dependencies",
    "year",
];

/// Why a template could not be rendered.
#[derive(Debug, Error, PartialEq, Eq)]
#[non_exhaustive]
pub enum TemplateError {
    /// `{{name}}` names no known variable.
    #[error("{file}: unknown placeholder `{{{{{name}}}}}`")]
    UnknownPlaceholder {
        /// Template file.
        file: String,
        /// The placeholder name (truncated).
        name: String,
    },
    /// `{{` without a closing `}}`.
    #[error("{file}: `{{{{` without a closing `}}}}` (write a literal `{{{{` as `{{{{{{{{`)")]
    Unterminated {
        /// Template file.
        file: String,
    },
    /// A `.tmpl` file is not UTF-8.
    #[error("{file}: a .tmpl file must be UTF-8")]
    NotUtf8 {
        /// Template file.
        file: String,
    },
    /// A rendered path escapes the project or is malformed.
    #[error("{file}: rendered path `{path}` is not a safe relative path")]
    BadPath {
        /// Template file.
        file: String,
        /// The rendered path.
        path: String,
    },
    /// Two template files render to the same path.
    #[error("two templates render to `{0}`")]
    Duplicate(String),
    /// The kind is not in the embedded templates.
    #[error("template kind `{0}` is not embedded in this build")]
    MissingKind(String),
    /// A project or crate name is not usable.
    #[error("`{name}` is not a valid crate name: {reason}")]
    BadName {
        /// The rejected name.
        name: String,
        /// Why.
        reason: &'static str,
    },
}

/// Template variables.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Vars(BTreeMap<String, String>);

impl Vars {
    /// No variables.
    pub fn new() -> Self {
        Self::default()
    }

    /// Set `name`.
    pub fn insert(&mut self, name: &str, value: impl Into<String>) {
        self.0.insert(name.to_owned(), value.into());
    }

    /// The value of `name`.
    pub fn get(&self, name: &str) -> Option<&str> {
        self.0.get(name).map(String::as_str)
    }

    /// Variable names, sorted.
    pub fn names(&self) -> Vec<&str> {
        self.0.keys().map(String::as_str).collect()
    }
}

/// Render `input`, substituting `{{name}}` and unescaping `{{{{`. `file`
/// only labels errors.
pub fn render_str(input: &str, vars: &Vars, file: &str) -> Result<String, TemplateError> {
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(start) = rest.find("{{") {
        out.push_str(&rest[..start]);
        let after = &rest[start..];
        if let Some(tail) = after.strip_prefix("{{{{") {
            out.push_str("{{");
            rest = tail;
            continue;
        }
        let inner = &after[2..];
        let Some(end) = inner.find("}}") else {
            return Err(TemplateError::Unterminated {
                file: file.to_owned(),
            });
        };
        let name = &inner[..end];
        match vars.get(name) {
            Some(value) => out.push_str(value),
            None => {
                return Err(TemplateError::UnknownPlaceholder {
                    file: file.to_owned(),
                    name: name.chars().take(40).collect(),
                });
            }
        }
        rest = &inner[end + 2..];
    }
    out.push_str(rest);
    Ok(out)
}

/// One template file: its path inside the kind and its bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TemplateFile<'a> {
    /// `/`-separated path relative to the kind directory.
    pub path: String,
    /// Raw contents.
    pub contents: &'a [u8],
}

/// A rendered file, ready to write under the project root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderedFile {
    /// `/`-separated path relative to the project root.
    pub path: String,
    /// Contents.
    pub contents: Vec<u8>,
    /// Write with mode 0755.
    pub executable: bool,
}

impl RenderedFile {
    /// The contents as text (lossy).
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.contents).into_owned()
    }
}

fn safe_relative(path: &str) -> bool {
    !path.is_empty()
        && !path.starts_with('/')
        && !path.contains('\\')
        && path
            .split('/')
            .all(|segment| !segment.is_empty() && segment != "." && segment != "..")
}

/// Render a set of template files.
pub fn render_files(
    files: &[TemplateFile<'_>],
    vars: &Vars,
) -> Result<Vec<RenderedFile>, TemplateError> {
    let mut out: Vec<RenderedFile> = Vec::with_capacity(files.len());
    for file in files {
        let (raw_path, is_template) = match file.path.strip_suffix(".tmpl") {
            Some(stripped) => (stripped, true),
            None => (file.path.as_str(), false),
        };
        let path = render_str(raw_path, vars, &file.path)?;
        if !safe_relative(&path) {
            return Err(TemplateError::BadPath {
                file: file.path.clone(),
                path,
            });
        }
        let contents = if is_template {
            let text = std::str::from_utf8(file.contents).map_err(|_| TemplateError::NotUtf8 {
                file: file.path.clone(),
            })?;
            render_str(text, vars, &file.path)?.into_bytes()
        } else {
            file.contents.to_vec()
        };
        let executable = Path::new(&path)
            .extension()
            .is_some_and(|extension| extension == "sh");
        out.push(RenderedFile {
            path,
            contents,
            executable,
        });
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    if let Some(pair) = out.windows(2).find(|pair| pair[0].path == pair[1].path) {
        return Err(TemplateError::Duplicate(pair[0].path.clone()));
    }
    Ok(out)
}

fn slash_path(path: &Path) -> String {
    path.components()
        .map(|component| component.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("/")
}

fn collect<'a>(dir: &Dir<'a>, base: &Path, out: &mut Vec<TemplateFile<'a>>) {
    for file in dir.files() {
        let relative = file.path().strip_prefix(base).unwrap_or(file.path());
        out.push(TemplateFile {
            path: slash_path(relative),
            contents: file.contents(),
        });
    }
    for child in dir.dirs() {
        collect(child, base, out);
    }
}

/// The files of `kind` in the tree `root`.
pub fn kind_files_in<'a>(
    root: &Dir<'a>,
    kind: &str,
) -> Result<Vec<TemplateFile<'a>>, TemplateError> {
    let dir = root
        .dirs()
        .find(|dir| dir.path() == Path::new(kind))
        .ok_or_else(|| TemplateError::MissingKind(kind.to_owned()))?;
    let mut files = Vec::new();
    collect(dir, dir.path(), &mut files);
    files.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(files)
}

/// The files of `kind` in the embedded templates.
pub fn kind_files(kind: &str) -> Result<Vec<TemplateFile<'static>>, TemplateError> {
    kind_files_in(&TEMPLATES, kind)
}

/// Render the embedded `kind`.
pub fn render_kind(kind: &str, vars: &Vars) -> Result<Vec<RenderedFile>, TemplateError> {
    render_files(&kind_files(kind)?, vars)
}

/// The kinds present in the embedded templates.
pub fn embedded_kinds() -> Vec<String> {
    TEMPLATES.dirs().map(|dir| slash_path(dir.path())).collect()
}

/// The service flavours `new` and `add` create.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceKind {
    /// A tonic RPC service plus its proto crate.
    Grpc,
    /// An axum HTTP service.
    Http,
    /// A background worker.
    Worker,
}

impl ServiceKind {
    /// The template kind directory.
    pub fn template_kind(self) -> &'static str {
        match self {
            Self::Grpc => "service-grpc",
            Self::Http => "service-http",
            Self::Worker => "service-worker",
        }
    }
}

/// Where generated projects take sekvent from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SekventSource {
    /// The git repository, `master` branch.
    Git,
    /// A local checkout, for developing sekvent itself.
    Path(PathBuf),
}

/// Reject names that are not lowercase kebab-case crate names.
pub fn validate_name(name: &str) -> Result<(), TemplateError> {
    let bad = |reason| {
        Err(TemplateError::BadName {
            name: name.to_owned(),
            reason,
        })
    };
    if name.is_empty() || name.len() > 64 {
        return bad("must be 1 to 64 characters");
    }
    if !name.starts_with(|c: char| c.is_ascii_lowercase()) {
        return bad("must start with a lowercase letter");
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    {
        return bad("may only contain lowercase letters, digits and '-'");
    }
    if name.ends_with('-') || name.contains("--") {
        return bad("must not end with '-' or contain '--'");
    }
    Ok(())
}

/// `orders-api` → `orders_api`.
pub fn snake_case(name: &str) -> String {
    name.replace('-', "_")
}

/// `orders-api` → `OrdersApi`.
pub fn pascal_case(name: &str) -> String {
    name.split(['-', '_'])
        .filter(|part| !part.is_empty())
        .map(|part| {
            let mut chars = part.chars();
            chars.next().map_or_else(String::new, |first| {
                first.to_ascii_uppercase().to_string() + chars.as_str()
            })
        })
        .collect()
}

fn toml_string(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

/// The `sekvent_deps` block: one `[workspace.dependencies]` line per sekvent
/// crate a project uses.
pub fn sekvent_deps_block(source: &SekventSource) -> String {
    SDK_PACKAGES
        .iter()
        .map(|name| match source {
            SekventSource::Git => format!(
                "{name} = {{ git = {}, branch = {} }}",
                toml_string(SEKVENT_GIT_URL),
                toml_string(SEKVENT_BRANCH)
            ),
            SekventSource::Path(root) => format!(
                "{name} = {{ path = {} }}",
                toml_string(&root.join("crates").join(name).display().to_string())
            ),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The full variable set for a project and one of its services.
pub fn variables(
    project_name: &str,
    service_name: &str,
    source: &SekventSource,
    year: i64,
) -> Result<Vars, TemplateError> {
    validate_name(project_name)?;
    validate_name(service_name)?;
    let ident = snake_case(service_name);
    let mut vars = Vars::new();
    vars.insert("project_name", project_name);
    vars.insert("crate_name", service_name);
    vars.insert("crate_ident", ident.clone());
    vars.insert("service_pascal", pascal_case(service_name));
    vars.insert("proto_package", format!("{ident}.v1"));
    vars.insert("proto_package_path", format!("{ident}/v1"));
    vars.insert("rust_version", rust_version());
    vars.insert("edition", EDITION);
    vars.insert("rrb_image", RRB_IMAGE);
    vars.insert("sekvent_git_url", SEKVENT_GIT_URL);
    vars.insert("sekvent_deps", sekvent_deps_block(source));
    vars.insert("pinned_dependencies", pinned_block(ROOT_CARGO_TOML));
    vars.insert("year", year.to_string());
    Ok(vars)
}

/// The calendar year (UTC) of `now`.
pub fn year_of(now: SystemTime) -> i64 {
    let seconds = match now.duration_since(UNIX_EPOCH) {
        Ok(elapsed) => i64::try_from(elapsed.as_secs()).unwrap_or(i64::MAX),
        Err(before) => -i64::try_from(before.duration().as_secs()).unwrap_or(i64::MAX),
    };
    let days = seconds.div_euclid(86_400);
    // Civil-from-days (proleptic Gregorian), eras of 400 years.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let year = year_of_era + era * 400;
    if month_index >= 10 { year + 1 } else { year }
}

/// The current calendar year (UTC).
pub fn current_year() -> i64 {
    year_of(SystemTime::now())
}

/// What happened to one file when writing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteOutcome {
    /// The file did not exist.
    Created,
    /// The file existed and was replaced (`--force`).
    Overwritten,
    /// The file existed with identical contents.
    Unchanged,
    /// The file existed with other contents and was left alone.
    Skipped,
}

impl WriteOutcome {
    /// The summary marker: `+`, `~`, `=` or `!`.
    pub fn marker(self) -> char {
        match self {
            Self::Created => '+',
            Self::Overwritten => '~',
            Self::Unchanged => '=',
            Self::Skipped => '!',
        }
    }
}

/// Refuse to write `relative` under `root` through a symbolic link.
///
/// Walks every component of `relative` that already exists and fails on the
/// first symlink, naming it, so a checked-in or planted link (say
/// `.sekvent -> ~/.ssh`) cannot redirect a scaffolded file outside the
/// project. `root` itself is trusted. Components that do not exist yet end
/// the walk; `relative` must be a plain relative path.
pub fn refuse_symlinks(root: &Path, relative: &Path) -> io::Result<()> {
    let mut current = root.to_path_buf();
    let components = relative
        .components()
        .filter(|component| *component != Component::CurDir);
    for component in components {
        match component {
            Component::Normal(part) => current.push(part),
            Component::CurDir
            | Component::ParentDir
            | Component::RootDir
            | Component::Prefix(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("`{}` is not a plain relative path", relative.display()),
                ));
            }
        }
        match std::fs::symlink_metadata(&current) {
            Ok(meta) if meta.file_type().is_symlink() => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!(
                        "refusing to write through the symbolic link {}",
                        current.display()
                    ),
                ));
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

/// Write `contents` to `relative` under `root`, never replacing different
/// contents unless `force` and never writing through a symbolic link.
///
/// A new file is created exclusively (`O_EXCL`), so a file or link that
/// appears between the check and the write is left alone and reported as
/// [`WriteOutcome::Skipped`]. With `force` an existing file is removed (a
/// link is removed, not followed) and created afresh.
pub fn write_file(
    root: &Path,
    relative: &Path,
    contents: &[u8],
    executable: bool,
    force: bool,
) -> io::Result<WriteOutcome> {
    refuse_symlinks(root, relative)?;
    let path = root.join(relative);
    let outcome = match std::fs::read(&path) {
        Ok(existing) if existing == contents => return Ok(WriteOutcome::Unchanged),
        Ok(_) if !force => return Ok(WriteOutcome::Skipped),
        Ok(_) => WriteOutcome::Overwritten,
        Err(error) if error.kind() == io::ErrorKind::NotFound => WriteOutcome::Created,
        Err(error) => return Err(error),
    };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    refuse_symlinks(root, relative)?;
    if outcome == WriteOutcome::Overwritten {
        std::fs::remove_file(&path)?;
    }
    if create_new(&path, contents, executable)? {
        Ok(outcome)
    } else {
        Ok(WriteOutcome::Skipped)
    }
}

/// Create `path` exclusively with `contents`; `false` when something already
/// exists there (it is not touched).
pub fn create_new(path: &Path, contents: &[u8], executable: bool) -> io::Result<bool> {
    use std::io::Write as _;

    let mut file = match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => return Ok(false),
        Err(error) => return Err(error),
    };
    file.write_all(contents)?;
    if executable {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(std::fs::Permissions::from_mode(0o755))?;
        }
    }
    Ok(true)
}

/// Make `path` mode 0755 (a no-op off Unix).
pub fn set_executable(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

/// Write rendered files under `root` (see [`write_file`]).
pub fn write_all(
    root: &Path,
    files: &[RenderedFile],
    force: bool,
) -> io::Result<Vec<(String, WriteOutcome)>> {
    files
        .iter()
        .map(|file| {
            let outcome = write_file(
                root,
                Path::new(&file.path),
                &file.contents,
                file.executable,
                force,
            )?;
            Ok((file.path.clone(), outcome))
        })
        .collect()
}

/// A diff-style summary: `+ created`, `~ overwritten`, `= unchanged`,
/// `! skipped`.
pub fn summary(results: &[(String, WriteOutcome)]) -> String {
    let mut out = String::new();
    for (path, outcome) in results {
        let note = if *outcome == WriteOutcome::Skipped {
            "  (exists with other contents; --force to replace)"
        } else {
            ""
        };
        let _ = writeln!(out, "{} {path}{note}", outcome.marker());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars() -> Vars {
        variables("orders", "orders-api", &SekventSource::Git, 2026).unwrap()
    }

    fn file<'a>(path: &str, contents: &'a [u8]) -> TemplateFile<'a> {
        TemplateFile {
            path: path.to_owned(),
            contents,
        }
    }

    #[test]
    fn placeholders_are_substituted() {
        let text = render_str(
            "name = \"{{crate_name}}\" ident {{crate_ident}}",
            &vars(),
            "f",
        )
        .unwrap();
        assert_eq!(text, "name = \"orders-api\" ident orders_api");
        assert_eq!(
            render_str("plain } { text", &vars(), "f").unwrap(),
            "plain } { text"
        );
    }

    #[test]
    fn a_doubled_brace_pair_is_a_literal() {
        assert_eq!(
            render_str("${{{{ secrets.TOKEN }}", &vars(), "f").unwrap(),
            "${{ secrets.TOKEN }}"
        );
        assert_eq!(
            render_str("{{{{crate_name}} vs {{crate_name}}", &vars(), "f").unwrap(),
            "{{crate_name}} vs orders-api"
        );
    }

    #[test]
    fn unknown_or_unterminated_placeholders_are_errors() {
        assert_eq!(
            render_str("x {{nope}} y", &vars(), "a.tmpl"),
            Err(TemplateError::UnknownPlaceholder {
                file: "a.tmpl".into(),
                name: "nope".into()
            })
        );
        assert!(matches!(
            render_str("{{ crate_name }}", &vars(), "f"),
            Err(TemplateError::UnknownPlaceholder { .. })
        ));
        assert_eq!(
            render_str("x {{crate_name", &vars(), "f"),
            Err(TemplateError::Unterminated { file: "f".into() })
        );
        let error = render_str("{{bad}}", &vars(), "f").unwrap_err();
        assert_eq!(error.to_string(), "f: unknown placeholder `{{bad}}`");
    }

    #[test]
    fn tmpl_is_stripped_and_paths_are_rendered() {
        let files = [
            file(
                "crates/{{crate_name}}/src/lib.rs.tmpl",
                b"//! {{service_pascal}}\n",
            ),
            file("crates/{{crate_name}}/raw.txt", b"{{kept}}"),
            file(".sekvent/run.sh.tmpl", b"#!/bin/sh\n"),
            file("scripts/tool.sh", b"#!/bin/sh\n"),
        ];
        let rendered = render_files(&files, &vars()).unwrap();
        let summary: Vec<(&str, bool)> = rendered
            .iter()
            .map(|file| (file.path.as_str(), file.executable))
            .collect();
        assert_eq!(
            summary,
            [
                (".sekvent/run.sh", true),
                ("crates/orders-api/raw.txt", false),
                ("crates/orders-api/src/lib.rs", false),
                ("scripts/tool.sh", true),
            ]
        );
        assert_eq!(rendered[1].text(), "{{kept}}");
        assert_eq!(rendered[2].text(), "//! OrdersApi\n");
    }

    #[test]
    fn bad_files_are_rejected() {
        assert!(matches!(
            render_files(&[file("../x.tmpl", b"")], &vars()),
            Err(TemplateError::BadPath { .. })
        ));
        assert!(matches!(
            render_files(&[file("a.tmpl", &[0xff, 0xfe])], &vars()),
            Err(TemplateError::NotUtf8 { .. })
        ));
        assert_eq!(
            render_files(&[file("a.tmpl", b"1"), file("a", b"2")], &vars()),
            Err(TemplateError::Duplicate("a".into()))
        );
        assert!(matches!(
            render_files(&[file("{{nope}}/a", b"")], &vars()),
            Err(TemplateError::UnknownPlaceholder { .. })
        ));
    }

    #[test]
    fn names_and_cases() {
        for good in ["orders", "orders-api", "a1-b2"] {
            validate_name(good).unwrap();
        }
        for bad in ["", "Orders", "1x", "a_b", "a-", "a--b", "a b"] {
            assert!(validate_name(bad).is_err(), "{bad}");
        }
        assert_eq!(snake_case("orders-api"), "orders_api");
        assert_eq!(pascal_case("orders-api-v2"), "OrdersApiV2");
        assert_eq!(pascal_case("x"), "X");
    }

    #[test]
    fn the_variable_set_is_complete() {
        let vars = vars();
        let mut expected = VARIABLES.to_vec();
        expected.sort_unstable();
        assert_eq!(vars.names(), expected);
        assert_eq!(vars.get("proto_package"), Some("orders_api.v1"));
        assert_eq!(vars.get("proto_package_path"), Some("orders_api/v1"));
        assert_eq!(vars.get("edition"), Some("2024"));
        assert_eq!(vars.get("year"), Some("2026"));
        assert_eq!(vars.get("project_name"), Some("orders"));
        assert!(vars.get("pinned_dependencies").unwrap().contains("tokio"));
        assert!(variables("Orders", "x", &SekventSource::Git, 1).is_err());
    }

    #[test]
    fn sekvent_deps_follow_the_source() {
        assert_eq!(
            sekvent_deps_block(&SekventSource::Git),
            "sekvent = { git = \"https://github.com/westito/sekvent\", branch = \"master\" }\n\
             sekvent-testing = { git = \"https://github.com/westito/sekvent\", branch = \"master\" }\n\
             sekvent-proto-build = { git = \"https://github.com/westito/sekvent\", branch = \"master\" }"
        );
        let local = sekvent_deps_block(&SekventSource::Path("/src/sekvent".into()));
        assert!(
            local.starts_with("sekvent = { path = \"/src/sekvent/crates/sekvent\" }\n"),
            "{local}"
        );
        assert!(toml::from_str::<toml::Table>(&local).is_ok());
    }

    #[test]
    fn years_follow_the_gregorian_calendar() {
        let at = |seconds: u64| year_of(UNIX_EPOCH + std::time::Duration::from_secs(seconds));
        assert_eq!(at(0), 1970);
        assert_eq!(at(951_782_400), 2000); // 2000-02-29
        assert_eq!(at(1_798_761_599), 2026); // 2026-12-31T23:59:59
        assert_eq!(at(1_798_761_600), 2027);
        assert_eq!(
            year_of(UNIX_EPOCH - std::time::Duration::from_secs(1)),
            1969
        );
        assert!(current_year() >= 2026);
    }

    #[test]
    fn writing_never_clobbers_without_force() {
        let dir = tempfile::tempdir().unwrap();
        let files = vec![
            RenderedFile {
                path: "a/b.txt".into(),
                contents: b"one".to_vec(),
                executable: false,
            },
            RenderedFile {
                path: "run.sh".into(),
                contents: b"#!/bin/sh\n".to_vec(),
                executable: true,
            },
        ];
        let first = write_all(dir.path(), &files, false).unwrap();
        assert_eq!(first[0].1, WriteOutcome::Created);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dir.path().join("run.sh"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o755);
        }
        assert_eq!(
            write_all(dir.path(), &files, false).unwrap()[0].1,
            WriteOutcome::Unchanged
        );
        std::fs::write(dir.path().join("a/b.txt"), "local edit").unwrap();
        let skipped = write_all(dir.path(), &files, false).unwrap();
        assert_eq!(skipped[0].1, WriteOutcome::Skipped);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("a/b.txt")).unwrap(),
            "local edit"
        );
        assert!(summary(&skipped).starts_with("! a/b.txt  (exists"));
        let forced = write_all(dir.path(), &files, true).unwrap();
        assert_eq!(forced[0].1, WriteOutcome::Overwritten);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("a/b.txt")).unwrap(),
            "one"
        );
    }

    #[test]
    fn a_file_that_appears_before_the_write_is_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.txt");
        assert!(create_new(&path, b"first", false).unwrap());
        assert!(!create_new(&path, b"second", true).unwrap());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "first");
        assert!(create_new(&dir.path().join("missing/x"), b"", false).is_err());
    }

    #[test]
    fn only_plain_relative_paths_are_written() {
        let dir = tempfile::tempdir().unwrap();
        for bad in ["../x", "/abs"] {
            let error = write_file(dir.path(), Path::new(bad), b"", false, false).unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::InvalidInput, "{bad}");
            assert!(error.to_string().contains("plain relative path"), "{error}");
        }
        refuse_symlinks(dir.path(), Path::new("./a/b")).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn scaffolding_never_writes_through_a_symlink() {
        use std::os::unix::fs::symlink;

        let project = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        symlink(outside.path(), project.path().join(".sekvent")).unwrap();
        let files = vec![RenderedFile {
            path: ".sekvent/run.sh".into(),
            contents: b"#!/bin/sh\n".to_vec(),
            executable: true,
        }];
        for force in [false, true] {
            let error = write_all(project.path(), &files, force).unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
            let message = error.to_string();
            assert!(message.contains("symbolic link"), "{message}");
            assert!(message.ends_with(".sekvent"), "{message}");
        }
        assert!(!outside.path().join("run.sh").exists());

        let target = outside.path().join("victim");
        std::fs::write(&target, "keep").unwrap();
        symlink(&target, project.path().join("file.txt")).unwrap();
        for force in [false, true] {
            let error =
                write_file(project.path(), Path::new("file.txt"), b"x", false, force).unwrap_err();
            assert!(error.to_string().contains("file.txt"), "{error}");
        }
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "keep");

        let directory = project.path().join("a-dir");
        std::fs::create_dir(&directory).unwrap();
        std::fs::write(directory.join("f"), "x").unwrap();
        assert!(write_file(project.path(), Path::new("a-dir"), b"x", false, true).is_err());
    }
}
