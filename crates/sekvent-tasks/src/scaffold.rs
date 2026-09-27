//! Tree-writing commands: `new`, `init`, `add`, `ci generate` and `agents`.
//!
//! All of them render embedded templates and write into the source tree, so
//! they always run on this machine. Existing files are never replaced
//! without `--force`; every command prints a diff-style summary.

use std::path::{Path, PathBuf};

use anyhow::{Context as _, bail};
use toml_edit::{Array, DocumentMut, Item};

use crate::agents;
use crate::config::{CONFIG_FILE, find_workspace_root};
use crate::process::{Cmd, Runner};
use crate::remote_build::{self, BOOTSTRAP, Edit, REMOTE_BUILD_FILE};
use crate::template::{
    self, RenderedFile, SekventSource, ServiceKind, Vars, WriteOutcome, current_year, summary,
    variables,
};

/// File name of the agent instructions.
pub const AGENTS_FILE: &str = "AGENTS.md";

/// CI systems with a gate template.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CiProvider {
    /// GitHub Actions.
    Github,
    /// Bitbucket Pipelines.
    Bitbucket,
}

impl CiProvider {
    /// The template kind directory.
    pub fn template_kind(self) -> &'static str {
        match self {
            Self::Github => "ci-github",
            Self::Bitbucket => "ci-bitbucket",
        }
    }
}

/// Options of `cargo sekvent new`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewOptions {
    /// Project name; also the first service's name.
    pub name: String,
    /// The first service's flavour.
    pub kind: ServiceKind,
    /// Target directory; `./<name>` by default.
    pub dir: Option<PathBuf>,
    /// Local sekvent checkout for path dependencies.
    pub sekvent_path: Option<PathBuf>,
    /// CI template to add.
    pub ci: Option<CiProvider>,
    /// Run `git init`.
    pub git: bool,
}

fn merge(into: &mut Vec<RenderedFile>, more: Vec<RenderedFile>) -> anyhow::Result<()> {
    for file in more {
        if into.iter().any(|existing| existing.path == file.path) {
            bail!("two template kinds render to `{}`", file.path);
        }
        into.push(file);
    }
    Ok(())
}

/// The files of a new project, `AGENTS.md` section included.
pub fn render_new(
    vars: &Vars,
    kind: ServiceKind,
    ci: Option<CiProvider>,
) -> anyhow::Result<Vec<RenderedFile>> {
    let mut files = template::render_kind("workspace", vars)?;
    merge(
        &mut files,
        template::render_kind(kind.template_kind(), vars)?,
    )?;
    if let Some(ci) = ci {
        merge(&mut files, template::render_kind(ci.template_kind(), vars)?)?;
    }
    upsert_agents(&mut files, &agents::section_body(vars)?);
    files.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(files)
}

fn upsert_agents(files: &mut Vec<RenderedFile>, body: &str) {
    match files.iter_mut().find(|file| file.path == AGENTS_FILE) {
        Some(file) => file.contents = agents::upsert(Some(&file.text()), body).into_bytes(),
        None => files.push(RenderedFile {
            path: AGENTS_FILE.to_owned(),
            contents: agents::upsert(None, body).into_bytes(),
            executable: false,
        }),
    }
}

fn sekvent_source(cwd: &Path, path: Option<&Path>) -> anyhow::Result<SekventSource> {
    let Some(path) = path else {
        return Ok(SekventSource::Git);
    };
    let root = cwd
        .join(path)
        .canonicalize()
        .with_context(|| format!("--sekvent-path {} does not exist", path.display()))?;
    if !root.join("crates/sekvent/Cargo.toml").is_file() {
        bail!(
            "--sekvent-path {} is not a sekvent checkout (no crates/sekvent)",
            root.display()
        );
    }
    Ok(SekventSource::Path(root))
}

/// `cargo sekvent new`: create a workspace, optionally `git init` it, and
/// return its directory.
pub fn new_project(
    runner: &dyn Runner,
    cwd: &Path,
    options: &NewOptions,
) -> anyhow::Result<PathBuf> {
    template::validate_name(&options.name)?;
    let target = cwd.join(
        options
            .dir
            .clone()
            .unwrap_or_else(|| PathBuf::from(&options.name)),
    );
    if target.exists()
        && std::fs::read_dir(&target)
            .with_context(|| format!("cannot read {}", target.display()))?
            .next()
            .is_some()
    {
        bail!("{} already exists and is not empty", target.display());
    }
    let source = sekvent_source(cwd, options.sekvent_path.as_deref())?;
    let vars = variables(&options.name, &options.name, &source, current_year())?;
    let files = render_new(&vars, options.kind, options.ci)?;
    let results = template::write_all(&target, &files, false)
        .with_context(|| format!("cannot write into {}", target.display()))?;
    print!("{}", summary(&results));
    if options.git {
        let code = runner.status(&Cmd::new("git").args(["init", "--quiet"]).cwd(&target))?;
        if code != 0 {
            eprintln!("warning: `git init` exited with {code}");
        }
    }
    println!(
        "\ncreated {}\n\nnext steps:\n  cd {}\n  cargo generate-lockfile\n  cargo sekvent gate",
        target.display(),
        target.display()
    );
    Ok(target)
}

/// A crate-name-shaped project name from a directory name.
pub fn name_from_dir(dir: &Path) -> String {
    let raw = dir.file_name().map_or_else(String::new, |name| {
        name.to_string_lossy().to_ascii_lowercase()
    });
    let mut name = String::new();
    for c in raw.chars() {
        if c.is_ascii_alphanumeric() {
            name.push(c);
        } else if !name.is_empty() && !name.ends_with('-') {
            name.push('-');
        }
    }
    let name = name.trim_end_matches('-');
    match name.chars().next() {
        Some(first) if first.is_ascii_lowercase() => name.to_owned(),
        Some(_) => format!("x-{name}"),
        None => "project".to_owned(),
    }
}

fn rendered<'a>(files: &'a [RenderedFile], path: &str) -> anyhow::Result<&'a RenderedFile> {
    files
        .iter()
        .find(|file| file.path == path)
        .with_context(|| format!("the embedded `workspace` template has no `{path}`"))
}

fn print_line(outcome: WriteOutcome, path: &str) {
    print!("{}", summary(&[(path.to_owned(), outcome)]));
}

/// `cargo sekvent init`: add sekvent to the Cargo workspace at or above
/// `cwd`. Returns the workspace root.
pub fn init(cwd: &Path, kind: ServiceKind, force: bool) -> anyhow::Result<PathBuf> {
    let root = find_workspace_root(cwd)
        .context("no Cargo workspace found here or above; use `cargo sekvent new` instead")?;
    let name = name_from_dir(&root);
    let vars = variables(&name, &name, &SekventSource::Git, current_year())?;
    let files = template::render_kind("workspace", &vars)?;

    for path in [CONFIG_FILE, BOOTSTRAP] {
        let file = rendered(&files, path)?;
        let outcome =
            template::write_file(&root.join(path), &file.contents, file.executable, force)?;
        print_line(outcome, path);
    }

    let remote_path = root.join(REMOTE_BUILD_FILE);
    match std::fs::read_to_string(&remote_path) {
        Ok(existing) => match remote_build::ensure_command(&existing, force)? {
            Edit::Unchanged => print_line(WriteOutcome::Unchanged, REMOTE_BUILD_FILE),
            Edit::Updated(text) => {
                std::fs::write(&remote_path, text)?;
                println!("~ {REMOTE_BUILD_FILE}  (added [run.commands].sekvent)");
            }
            Edit::Conflict(current) => println!(
                "! {REMOTE_BUILD_FILE}  ([run.commands].sekvent is {current}; --force to replace)"
            ),
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let text = rendered(&files, REMOTE_BUILD_FILE)?.text();
            let text = match remote_build::ensure_command(&text, true)? {
                Edit::Updated(updated) => updated,
                Edit::Unchanged | Edit::Conflict(_) => text,
            };
            std::fs::write(&remote_path, text)?;
            print_line(WriteOutcome::Created, REMOTE_BUILD_FILE);
        }
        Err(error) => {
            return Err(error).with_context(|| format!("cannot read {}", remote_path.display()));
        }
    }

    let agents_path = root.join(AGENTS_FILE);
    let existed = agents_path.exists();
    let changed = agents::update_file(&agents_path, &agents::section_body(&vars)?)?;
    let outcome = match (existed, changed) {
        (false, _) => WriteOutcome::Created,
        (true, true) => WriteOutcome::Overwritten,
        (true, false) => WriteOutcome::Unchanged,
    };
    print_line(outcome, AGENTS_FILE);
    let flavour = kind.template_kind().trim_start_matches("service-");
    println!("\nnext: cargo sekvent add <name> --kind {flavour}");
    Ok(root)
}

/// The sekvent source an existing workspace uses, read from its root
/// `[workspace.dependencies].sekvent`.
pub fn workspace_sekvent_source(manifest: &str) -> SekventSource {
    let path = manifest.parse::<DocumentMut>().ok().and_then(|doc| {
        doc.get("workspace")?
            .get("dependencies")?
            .get("sekvent")?
            .get("path")?
            .as_str()
            .map(str::to_owned)
    });
    match path {
        Some(path) => {
            let path = PathBuf::from(path);
            let root = path
                .parent()
                .and_then(Path::parent)
                .map_or_else(|| path.clone(), Path::to_path_buf);
            SekventSource::Path(root)
        }
        None => SekventSource::Git,
    }
}

/// `members` patterns cover `dir` (exact, or `parent/*`).
pub fn members_cover(patterns: &[String], dir: &str) -> bool {
    let parent = dir.rsplit_once('/').map_or("", |(parent, _)| parent);
    patterns.iter().any(|pattern| {
        pattern == dir
            || pattern
                .strip_suffix("/*")
                .is_some_and(|prefix| prefix == parent)
            || (pattern == "*" && parent.is_empty())
    })
}

/// Add each of `dirs` to `[workspace].members` unless a pattern covers it.
pub fn add_members(manifest: &str, dirs: &[String]) -> anyhow::Result<Option<String>> {
    let mut doc: DocumentMut = manifest
        .parse()
        .context("cannot parse the root Cargo.toml")?;
    let workspace = doc
        .get_mut("workspace")
        .and_then(Item::as_table_like_mut)
        .context("the root Cargo.toml has no [workspace]")?;
    if !workspace.contains_key("members") {
        workspace.insert("members", toml_edit::value(Array::new()));
    }
    let members = workspace
        .get_mut("members")
        .and_then(Item::as_array_mut)
        .context("`workspace.members` is not an array")?;
    let patterns: Vec<String> = members
        .iter()
        .filter_map(toml_edit::Value::as_str)
        .map(str::to_owned)
        .collect();
    let mut changed = false;
    for dir in dirs {
        if !members_cover(&patterns, dir) {
            members.push(dir.as_str());
            changed = true;
        }
    }
    Ok(changed.then(|| doc.to_string()))
}

/// `cargo sekvent add`: render a service kind into an existing workspace.
pub fn add(
    root: &Path,
    project_name: &str,
    name: &str,
    kind: ServiceKind,
    force: bool,
) -> anyhow::Result<()> {
    let manifest_path = root.join("Cargo.toml");
    let manifest = std::fs::read_to_string(&manifest_path)
        .with_context(|| format!("cannot read {}", manifest_path.display()))?;
    let source = workspace_sekvent_source(&manifest);
    let vars = variables(project_name, name, &source, current_year())?;
    let files = template::render_kind(kind.template_kind(), &vars)?;
    let crate_dirs: Vec<String> = files
        .iter()
        .filter_map(|file| file.path.strip_suffix("/Cargo.toml").map(str::to_owned))
        .collect();
    if !force && let Some(existing) = crate_dirs.iter().find(|dir| root.join(dir).exists()) {
        bail!("{existing} already exists; pass --force to render over it");
    }
    let results = template::write_all(root, &files, force)?;
    print!("{}", summary(&results));
    if let Some(updated) = add_members(&manifest, &crate_dirs)? {
        std::fs::write(&manifest_path, updated)?;
        println!("~ Cargo.toml  (workspace members)");
    }
    Ok(())
}

/// `cargo sekvent ci generate`.
pub fn ci_generate(
    root: &Path,
    project_name: &str,
    provider: CiProvider,
    force: bool,
) -> anyhow::Result<Vec<(String, WriteOutcome)>> {
    let vars = variables(
        project_name,
        project_name,
        &SekventSource::Git,
        current_year(),
    )?;
    let files = template::render_kind(provider.template_kind(), &vars)?;
    let results = template::write_all(root, &files, force)?;
    print!("{}", summary(&results));
    Ok(results)
}

/// `cargo sekvent agents`: refresh the managed section of `file`.
pub fn agents_update(file: &Path, project_name: &str) -> anyhow::Result<bool> {
    let vars = variables(
        project_name,
        project_name,
        &SekventSource::Git,
        current_year(),
    )?;
    let changed = agents::update_file(file, &agents::section_body(&vars)?)?;
    println!("{} {}", if changed { '~' } else { '=' }, file.display());
    Ok(changed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::template::{KINDS, embedded_kinds};

    fn vars() -> Vars {
        variables("orders", "orders", &SekventSource::Git, 2026).unwrap()
    }

    #[test]
    fn every_embedded_kind_renders_with_the_full_variable_set() {
        for kind in KINDS {
            let files = template::render_kind(kind, &vars())
                .unwrap_or_else(|error| panic!("kind `{kind}`: {error}"));
            assert!(!files.is_empty(), "kind `{kind}` is empty");
            for file in &files {
                if Path::new(&file.path)
                    .extension()
                    .is_some_and(|extension| extension == "toml")
                {
                    let text = file.text();
                    text.parse::<toml::Table>()
                        .unwrap_or_else(|error| panic!("{kind}/{}: {error}\n{text}", file.path));
                }
            }
        }
        for kind in embedded_kinds() {
            assert!(
                KINDS.contains(&kind.as_str()),
                "unexpected template kind `{kind}`"
            );
        }
    }

    #[test]
    fn the_workspace_template_carries_the_cli_contract() {
        let files = template::render_kind("workspace", &vars()).unwrap();
        let config = rendered(&files, CONFIG_FILE).unwrap().text();
        let config = Config::parse(&config, Path::new(CONFIG_FILE)).unwrap();
        assert_eq!(config.project.name, "orders");
        let bootstrap = rendered(&files, BOOTSTRAP).unwrap();
        assert!(bootstrap.executable);
        let remote = rendered(&files, REMOTE_BUILD_FILE).unwrap().text();
        assert_eq!(
            remote_build::ensure_command(&remote, false).unwrap(),
            Edit::Unchanged
        );
        agents::section_body(&vars()).unwrap();
    }

    #[test]
    fn every_service_kind_combines_with_the_workspace() {
        for kind in [ServiceKind::Grpc, ServiceKind::Http, ServiceKind::Worker] {
            for ci in [None, Some(CiProvider::Github), Some(CiProvider::Bitbucket)] {
                let files = render_new(&vars(), kind, ci)
                    .unwrap_or_else(|error| panic!("{kind:?} {ci:?}: {error}"));
                let agents = rendered(&files, AGENTS_FILE).unwrap().text();
                assert_eq!(agents.matches(agents::BEGIN).count(), 1, "{agents}");
            }
        }
    }

    #[test]
    fn new_creates_a_project_directory() {
        let dir = tempfile::tempdir().unwrap();
        let runner = crate::process::fake::FakeRunner::default();
        let options = NewOptions {
            name: "orders".into(),
            kind: ServiceKind::Http,
            dir: None,
            sekvent_path: None,
            ci: Some(CiProvider::Github),
            git: true,
        };
        let target = new_project(&runner, dir.path(), &options).unwrap();
        assert_eq!(target, dir.path().join("orders"));
        assert!(target.join(CONFIG_FILE).is_file());
        assert!(target.join(AGENTS_FILE).is_file());
        assert_eq!(runner.lines(), ["git init --quiet"]);
        assert!(new_project(&runner, dir.path(), &options).is_err());
        let bad = NewOptions {
            name: "Bad".into(),
            ..options
        };
        assert!(new_project(&runner, dir.path(), &bad).is_err());
    }

    #[test]
    fn init_adds_sekvent_to_an_existing_workspace() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("Billing Service");
        std::fs::create_dir_all(root.join("crates")).unwrap();
        std::fs::write(
            root.join("Cargo.toml"),
            "[workspace]\nmembers = [\"crates/*\"]\n",
        )
        .unwrap();
        std::fs::write(
            root.join(REMOTE_BUILD_FILE),
            "# mine\n[project]\nname = \"billing\"\n\n[run.commands]\ncheck = [\"cargo\", \"check\"]\n",
        )
        .unwrap();
        std::fs::write(root.join(AGENTS_FILE), "# Billing\n").unwrap();

        let found = init(&root.join("crates"), ServiceKind::Http, false).unwrap();
        assert_eq!(found, root);
        let config = Config::load(&root.join(CONFIG_FILE)).unwrap();
        assert_eq!(config.project.name, "billing-service");
        let remote = std::fs::read_to_string(root.join(REMOTE_BUILD_FILE)).unwrap();
        assert!(remote.starts_with("# mine\n"), "{remote}");
        assert!(
            remote.contains("sekvent = [\".sekvent/run.sh\"]"),
            "{remote}"
        );
        let agents = std::fs::read_to_string(root.join(AGENTS_FILE)).unwrap();
        assert!(
            agents.starts_with("# Billing\n\n<!-- sekvent:begin -->"),
            "{agents}"
        );

        std::fs::write(root.join(CONFIG_FILE), "[project]\nname = \"kept\"\n").unwrap();
        init(&root, ServiceKind::Http, false).unwrap();
        assert_eq!(
            std::fs::read_to_string(root.join(CONFIG_FILE)).unwrap(),
            "[project]\nname = \"kept\"\n"
        );
        assert!(init(dir.path(), ServiceKind::Http, false).is_err());
    }

    #[test]
    fn project_names_come_from_directories() {
        assert_eq!(
            name_from_dir(Path::new("/x/Billing Service")),
            "billing-service"
        );
        assert_eq!(name_from_dir(Path::new("/x/orders_v2")), "orders-v2");
        assert_eq!(name_from_dir(Path::new("/x/2fa")), "x-2fa");
        assert_eq!(name_from_dir(Path::new("/")), "project");
    }

    #[test]
    fn members_are_added_only_when_uncovered() {
        let patterns = vec!["crates/*".to_owned(), "tools/xtask".to_owned()];
        assert!(members_cover(&patterns, "crates/orders"));
        assert!(members_cover(&patterns, "tools/xtask"));
        assert!(!members_cover(&patterns, "services/orders"));
        assert!(!members_cover(&patterns, "crates/a/b"));

        let manifest = "[workspace]\n# ours\nmembers = [\"crates/*\"]\n";
        assert_eq!(
            add_members(manifest, &["crates/orders".into()]).unwrap(),
            None
        );
        let updated = add_members(manifest, &["services/orders".into()])
            .unwrap()
            .unwrap();
        assert_eq!(
            updated,
            "[workspace]\n# ours\nmembers = [\"crates/*\", \"services/orders\"]\n"
        );
        assert!(add_members("[package]\n", &[]).is_err());
    }

    #[test]
    fn path_sources_are_detected() {
        let manifest =
            "[workspace.dependencies]\nsekvent = { path = \"/src/sekvent/crates/sekvent\" }\n";
        assert_eq!(
            workspace_sekvent_source(manifest),
            SekventSource::Path("/src/sekvent".into())
        );
        assert_eq!(
            workspace_sekvent_source("[workspace.dependencies]\nsekvent = { git = \"x\" }\n"),
            SekventSource::Git
        );
        assert_eq!(
            workspace_sekvent_source(
                "[workspace.dependencies]\nsekvent = { path = \"sekvent\" }\n"
            ),
            SekventSource::Path("sekvent".into())
        );
        assert_eq!(workspace_sekvent_source("not toml ["), SekventSource::Git);
    }

    fn file(path: &str) -> RenderedFile {
        RenderedFile {
            path: path.to_owned(),
            contents: Vec::new(),
            executable: false,
        }
    }

    #[test]
    fn two_kinds_rendering_the_same_path_are_rejected() {
        let mut files = vec![file("a")];
        merge(&mut files, vec![file("b")]).unwrap();
        let error = merge(&mut files, vec![file("a")]).unwrap_err();
        assert!(error.to_string().contains("`a`"), "{error}");
        assert_eq!(files.len(), 2);
    }

    #[test]
    fn the_agents_section_is_added_when_no_template_renders_one() {
        let mut files = vec![file("README.md")];
        upsert_agents(&mut files, "body");
        let agents = rendered(&files, AGENTS_FILE).unwrap().text();
        assert_eq!(agents.matches(agents::BEGIN).count(), 1, "{agents}");
        assert!(agents.contains("body"), "{agents}");
    }

    #[test]
    fn a_sekvent_path_must_be_a_checkout() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            sekvent_source(dir.path(), None).unwrap(),
            SekventSource::Git
        );
        let missing = sekvent_source(dir.path(), Some(Path::new("missing"))).unwrap_err();
        assert!(missing.to_string().contains("does not exist"), "{missing}");
        std::fs::create_dir_all(dir.path().join("checkout/crates/sekvent")).unwrap();
        let bare = sekvent_source(dir.path(), Some(Path::new("checkout"))).unwrap_err();
        assert!(
            bare.to_string().contains("not a sekvent checkout"),
            "{bare}"
        );
        std::fs::write(
            dir.path().join("checkout/crates/sekvent/Cargo.toml"),
            "[package]\n",
        )
        .unwrap();
        assert_eq!(
            sekvent_source(dir.path(), Some(Path::new("checkout"))).unwrap(),
            SekventSource::Path(dir.path().join("checkout").canonicalize().unwrap())
        );
    }

    #[test]
    fn new_honours_the_directory_path_and_git_options() {
        let dir = tempfile::tempdir().unwrap();
        let checkout = dir.path().join("sekvent");
        std::fs::create_dir_all(checkout.join("crates/sekvent")).unwrap();
        std::fs::write(checkout.join("crates/sekvent/Cargo.toml"), "[package]\n").unwrap();
        std::fs::create_dir_all(dir.path().join("empty")).unwrap();

        let runner = crate::process::fake::FakeRunner::default();
        let options = NewOptions {
            name: "billing".into(),
            kind: ServiceKind::Worker,
            dir: Some(PathBuf::from("empty")),
            sekvent_path: Some(PathBuf::from("sekvent")),
            ci: None,
            git: false,
        };
        let target = new_project(&runner, dir.path(), &options).unwrap();
        assert_eq!(target, dir.path().join("empty"));
        assert!(target.join(CONFIG_FILE).is_file());
        assert!(!target.join(".github").exists());
        let manifest = std::fs::read_to_string(target.join("Cargo.toml")).unwrap();
        assert!(manifest.contains("path = "), "{manifest}");
        assert!(runner.calls().is_empty());

        let failing = crate::process::fake::FakeRunner::with_codes(&[128]);
        let options = NewOptions {
            dir: Some(PathBuf::from("second")),
            sekvent_path: None,
            git: true,
            ..options
        };
        new_project(&failing, dir.path(), &options).unwrap();
        assert_eq!(failing.lines(), ["git init --quiet"]);
    }

    fn workspace(dir: &Path) -> PathBuf {
        let root = dir.join("billing");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("Cargo.toml"), "[workspace]\nmembers = []\n").unwrap();
        root
    }

    #[test]
    fn init_creates_missing_files_and_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let root = workspace(dir.path());
        init(&root, ServiceKind::Grpc, false).unwrap();
        let remote = std::fs::read_to_string(root.join(REMOTE_BUILD_FILE)).unwrap();
        assert_eq!(
            remote_build::ensure_command(&remote, false).unwrap(),
            Edit::Unchanged
        );
        assert!(root.join(AGENTS_FILE).is_file());
        assert!(root.join(BOOTSTRAP).is_file());

        init(&root, ServiceKind::Grpc, false).unwrap();
        assert_eq!(
            std::fs::read_to_string(root.join(REMOTE_BUILD_FILE)).unwrap(),
            remote
        );
    }

    #[test]
    fn init_keeps_a_conflicting_remote_command_without_force() {
        let dir = tempfile::tempdir().unwrap();
        let root = workspace(dir.path());
        let conflicting = "[run.commands]\nsekvent = [\"other.sh\"]\n";
        std::fs::write(root.join(REMOTE_BUILD_FILE), conflicting).unwrap();
        init(&root, ServiceKind::Http, false).unwrap();
        assert_eq!(
            std::fs::read_to_string(root.join(REMOTE_BUILD_FILE)).unwrap(),
            conflicting
        );
        init(&root, ServiceKind::Http, true).unwrap();
        let replaced = std::fs::read_to_string(root.join(REMOTE_BUILD_FILE)).unwrap();
        assert!(replaced.contains(BOOTSTRAP), "{replaced}");
    }

    #[test]
    fn init_reports_an_unreadable_remote_build_file() {
        let dir = tempfile::tempdir().unwrap();
        let root = workspace(dir.path());
        std::fs::create_dir_all(root.join(REMOTE_BUILD_FILE)).unwrap();
        let error = init(&root, ServiceKind::Http, false).unwrap_err();
        assert!(error.to_string().starts_with("cannot read"), "{error:#}");
    }

    #[test]
    fn add_renders_a_service_and_registers_it() {
        let dir = tempfile::tempdir().unwrap();
        assert!(add(dir.path(), "billing", "ledger", ServiceKind::Http, false).is_err());

        let root = workspace(dir.path());
        add(&root, "billing", "ledger", ServiceKind::Http, false).unwrap();
        assert!(root.join("crates/ledger/Cargo.toml").is_file());
        let manifest = std::fs::read_to_string(root.join("Cargo.toml")).unwrap();
        assert!(manifest.contains("\"crates/ledger\""), "{manifest}");

        let error = add(&root, "billing", "ledger", ServiceKind::Http, false).unwrap_err();
        assert!(error.to_string().contains("--force"), "{error}");
        add(&root, "billing", "ledger", ServiceKind::Http, true).unwrap();
        assert_eq!(
            std::fs::read_to_string(root.join("Cargo.toml")).unwrap(),
            manifest
        );
    }

    #[test]
    fn ci_templates_and_the_agents_section_are_written_once() {
        let dir = tempfile::tempdir().unwrap();
        let first = ci_generate(dir.path(), "billing", CiProvider::Bitbucket, false).unwrap();
        assert!(!first.is_empty());
        assert!(
            first
                .iter()
                .all(|(_, outcome)| *outcome == WriteOutcome::Created)
        );
        let again = ci_generate(dir.path(), "billing", CiProvider::Bitbucket, false).unwrap();
        assert!(
            again
                .iter()
                .all(|(_, outcome)| *outcome == WriteOutcome::Unchanged)
        );

        let agents = dir.path().join(AGENTS_FILE);
        assert!(agents_update(&agents, "billing").unwrap());
        assert!(!agents_update(&agents, "billing").unwrap());
    }

    #[test]
    fn members_are_created_or_rejected_when_malformed() {
        let updated = add_members("[workspace]\n", &["crates/a".into()])
            .unwrap()
            .unwrap();
        assert!(updated.contains("\"crates/a\""), "{updated}");
        assert!(add_members("[workspace]\nmembers = \"x\"\n", &[]).is_err());
        assert!(add_members("[workspace\n", &[]).is_err());
        assert!(members_cover(&["*".to_owned()], "orders"));
    }
}
