//! Which sekvent revision a project is locked to, and updating it.
//!
//! Projects depend on sekvent through git (`branch = "master"`), so
//! `Cargo.lock` records the exact revision. `sdk status --remote` compares it
//! with the branch head via `git ls-remote`; the gate does the same as a
//! best-effort, time-boxed hint.

use std::path::Path;
use std::time::Duration;

use serde::Deserialize;

use crate::location::{EnvMap, NO_UPDATE_CHECK_ENV, is_set};
use crate::process::{Cmd, Runner};

/// The sekvent repository.
pub const SEKVENT_GIT_URL: &str = "https://github.com/westito/sekvent";

/// The branch projects track.
pub const SEKVENT_BRANCH: &str = "master";

/// sekvent crates a project may depend on directly.
pub const SDK_PACKAGES: [&str; 3] = ["sekvent", "sekvent-testing", "sekvent-proto-build"];

/// Time box for the gate's update check.
pub const UPDATE_CHECK_TIMEOUT: Duration = Duration::from_secs(5);

/// One `[[package]]` of `Cargo.lock`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct LockedPackage {
    /// Package name.
    pub name: String,
    /// Locked version.
    pub version: String,
    /// Source URL; `None` for path dependencies and workspace members.
    #[serde(default)]
    pub source: Option<String>,
}

#[derive(Debug, Deserialize)]
struct LockFile {
    #[serde(default)]
    package: Vec<LockedPackage>,
}

/// Parse the packages of a `Cargo.lock`.
pub fn parse_lock(text: &str) -> Result<Vec<LockedPackage>, toml::de::Error> {
    toml::from_str::<LockFile>(text).map(|lock| lock.package)
}

/// Where a locked package comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    /// A git checkout.
    Git {
        /// Repository URL without query or fragment.
        url: String,
        /// The tracked branch, if any.
        branch: Option<String>,
        /// The locked commit.
        rev: String,
    },
    /// A local path.
    Path,
    /// A registry or anything else, verbatim.
    Other(String),
}

/// Decode a `Cargo.lock` source string.
pub fn parse_source(source: Option<&str>) -> Source {
    let Some(source) = source else {
        return Source::Path;
    };
    let Some(git) = source.strip_prefix("git+") else {
        return Source::Other(source.to_owned());
    };
    let (base, rev) = git.split_once('#').unwrap_or((git, ""));
    let (url, query) = base.split_once('?').unwrap_or((base, ""));
    let branch = query
        .split('&')
        .find_map(|pair| pair.strip_prefix("branch="))
        .map(str::to_owned);
    Source::Git {
        url: url.to_owned(),
        branch,
        rev: rev.to_owned(),
    }
}

/// The locked `sekvent` package and its source.
pub fn locked_sdk(packages: &[LockedPackage]) -> Option<(&LockedPackage, Source)> {
    packages
        .iter()
        .find(|package| package.name == "sekvent")
        .map(|package| (package, parse_source(package.source.as_deref())))
}

/// `git ls-remote <url> refs/heads/<branch>`, never prompting.
pub fn ls_remote_command(url: &str, branch: &str) -> Cmd {
    Cmd::new("git")
        .args(["ls-remote", url])
        .arg(format!("refs/heads/{branch}"))
        .env("GIT_TERMINAL_PROMPT", "0")
}

/// The commit of the first line of `git ls-remote` output.
pub fn parse_ls_remote(output: &str) -> Option<String> {
    output
        .lines()
        .find_map(|line| line.split_whitespace().next())
        .filter(|sha| sha.len() >= 7 && sha.chars().all(|c| c.is_ascii_hexdigit()))
        .map(str::to_owned)
}

/// Two commit ids name the same commit (one may be abbreviated).
pub fn same_commit(a: &str, b: &str) -> bool {
    !a.is_empty() && !b.is_empty() && (a.starts_with(b) || b.starts_with(a))
}

/// The branch head, or `None` when git fails or times out.
pub fn remote_head(
    runner: &dyn Runner,
    url: &str,
    branch: &str,
    timeout: Option<Duration>,
) -> Option<String> {
    let output = runner
        .output(&ls_remote_command(url, branch), timeout)
        .ok()?;
    if !output.success() {
        return None;
    }
    parse_ls_remote(&output.stdout)
}

/// A one-line hint when the locked sekvent is behind the branch head.
///
/// Best effort: `None` without a lock, with a non-git source, with
/// `SEKVENT_NO_UPDATE_CHECK` set, offline, or after the 5 s time box.
pub fn update_hint(runner: &dyn Runner, root: &Path, env: &EnvMap) -> Option<String> {
    if is_set(env, NO_UPDATE_CHECK_ENV) {
        return None;
    }
    let lock = std::fs::read_to_string(root.join("Cargo.lock")).ok()?;
    let packages = parse_lock(&lock).ok()?;
    let (_, Source::Git { url, branch, rev }) = locked_sdk(&packages)? else {
        return None;
    };
    let branch = branch.unwrap_or_else(|| SEKVENT_BRANCH.to_owned());
    let head = remote_head(runner, &url, &branch, Some(UPDATE_CHECK_TIMEOUT))?;
    (!same_commit(&rev, &head)).then(|| {
        format!(
            "note: sekvent {branch} moved on ({} -> {}); run `cargo sekvent sdk update`",
            short(&rev),
            short(&head)
        )
    })
}

fn short(rev: &str) -> &str {
    rev.get(..10).unwrap_or(rev)
}

/// `sdk status`: describe the locked sekvent; with `remote`, compare it with
/// the branch head.
pub fn status(runner: &dyn Runner, lock_text: &str, remote: bool) -> anyhow::Result<String> {
    let packages = parse_lock(lock_text)?;
    let Some((package, source)) = locked_sdk(&packages) else {
        return Ok("sekvent is not in Cargo.lock".to_owned());
    };
    let mut lines = Vec::new();
    match &source {
        Source::Git { url, branch, rev } => {
            let branch = branch.clone().unwrap_or_else(|| SEKVENT_BRANCH.to_owned());
            lines.push(format!(
                "sekvent {} from {url} ({branch}) at {}",
                package.version,
                short(rev)
            ));
            if remote {
                match remote_head(runner, url, &branch, None) {
                    Some(head) if same_commit(rev, &head) => lines.push("up to date".to_owned()),
                    Some(head) => lines.push(format!(
                        "behind: {branch} is at {}; run `cargo sekvent sdk update`",
                        short(&head)
                    )),
                    None => lines.push(format!("cannot reach {url}")),
                }
            }
        }
        Source::Path => lines.push(format!("sekvent {} from a local path", package.version)),
        Source::Other(other) => lines.push(format!("sekvent {} from {other}", package.version)),
    }
    Ok(lines.join("\n"))
}

/// `cargo update -p …` for the sekvent crates present in the lock; `None`
/// when none is.
pub fn update_command(root: &Path, packages: &[LockedPackage]) -> Option<Cmd> {
    let present: Vec<&str> = SDK_PACKAGES
        .iter()
        .copied()
        .filter(|name| packages.iter().any(|package| package.name == *name))
        .collect();
    if present.is_empty() {
        return None;
    }
    let mut cmd = Cmd::new("cargo").arg("update").cwd(root);
    for name in present {
        cmd = cmd.args(["-p", name]);
    }
    Some(cmd)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::process::fake::FakeRunner;

    const REV: &str = "0123456789abcdef0123456789abcdef01234567";

    fn lock() -> String {
        format!(
            r#"# This file is automatically @generated by Cargo.
version = 4

[[package]]
name = "orders"
version = "0.1.0"
dependencies = ["sekvent"]

[[package]]
name = "sekvent"
version = "0.1.0"
source = "git+https://github.com/westito/sekvent?branch=master#{REV}"

[[package]]
name = "sekvent-testing"
version = "0.1.0"
source = "git+https://github.com/westito/sekvent?branch=master#{REV}"

[[package]]
name = "serde"
version = "1.0.0"
source = "registry+https://github.com/rust-lang/crates.io-index"
checksum = "00"
"#
        )
    }

    #[test]
    fn the_lock_yields_the_git_revision() {
        let packages = parse_lock(&lock()).unwrap();
        let (package, source) = locked_sdk(&packages).unwrap();
        assert_eq!(package.version, "0.1.0");
        assert_eq!(
            source,
            Source::Git {
                url: SEKVENT_GIT_URL.into(),
                branch: Some("master".into()),
                rev: REV.into(),
            }
        );
        assert_eq!(parse_source(None), Source::Path);
        assert_eq!(
            parse_source(Some("registry+https://x")),
            Source::Other("registry+https://x".into())
        );
        assert_eq!(
            parse_source(Some("git+https://h/r?rev=abc#abc")),
            Source::Git {
                url: "https://h/r".into(),
                branch: None,
                rev: "abc".into()
            }
        );
        assert!(locked_sdk(&parse_lock("version = 4\n").unwrap()).is_none());
    }

    #[test]
    fn ls_remote_output_is_parsed() {
        assert_eq!(
            parse_ls_remote(&format!("{REV}\trefs/heads/master\n")).as_deref(),
            Some(REV)
        );
        assert_eq!(parse_ls_remote(""), None);
        assert_eq!(parse_ls_remote("fatal: nope\n"), None);
        assert!(same_commit(REV, &REV[..12]));
        assert!(!same_commit(REV, "fff"));
        assert!(!same_commit("", REV));
    }

    #[test]
    fn status_reports_behind_and_up_to_date() {
        let runner = FakeRunner::default();
        runner.push_output(&format!("{REV}\trefs/heads/master\n"));
        runner.push_output("fedcba9876543210fedcba9876543210fedcba98\trefs/heads/master\n");
        let current = status(&runner, &lock(), true).unwrap();
        assert!(current.ends_with("up to date"), "{current}");
        let behind = status(&runner, &lock(), true).unwrap();
        assert!(
            behind.contains("behind: master is at fedcba9876"),
            "{behind}"
        );
        assert_eq!(
            runner.lines()[0],
            "git ls-remote https://github.com/westito/sekvent refs/heads/master"
        );
        let offline = status(&runner, &lock(), false).unwrap();
        assert_eq!(
            offline,
            "sekvent 0.1.0 from https://github.com/westito/sekvent (master) at 0123456789"
        );
        assert_eq!(runner.calls().len(), 2);
    }

    #[test]
    fn update_targets_only_locked_sdk_crates() {
        let packages = parse_lock(&lock()).unwrap();
        let cmd = update_command(Path::new("/w"), &packages).unwrap();
        assert_eq!(
            cmd.to_string(),
            "cargo update -p sekvent -p sekvent-testing"
        );
        assert!(update_command(Path::new("/w"), &[]).is_none());
    }

    #[test]
    fn the_hint_is_best_effort() {
        let dir = tempfile::tempdir().unwrap();
        let runner = FakeRunner::default();
        let env = EnvMap::new();
        assert_eq!(update_hint(&runner, dir.path(), &env), None);
        std::fs::write(dir.path().join("Cargo.lock"), lock()).unwrap();

        runner.push_output("fedcba9876543210fedcba9876543210fedcba98\trefs/heads/master\n");
        let hint = update_hint(&runner, dir.path(), &env).unwrap();
        assert!(hint.contains("0123456789 -> fedcba9876"), "{hint}");

        runner
            .outputs
            .borrow_mut()
            .push_back(Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "slow",
            )));
        assert_eq!(update_hint(&runner, dir.path(), &env), None);

        let mut off = EnvMap::new();
        off.insert(NO_UPDATE_CHECK_ENV.into(), "1".into());
        let calls = runner.calls().len();
        assert_eq!(update_hint(&runner, dir.path(), &off), None);
        assert_eq!(runner.calls().len(), calls);
    }
}
