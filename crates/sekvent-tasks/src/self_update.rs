//! `self-update`: replace the running `cargo-sekvent` with the latest
//! release build for this target.
//!
//! The rolling GitHub release `cli-latest` carries one
//! `cargo-sekvent-<target>.tar.gz` per target plus a `.sha256` next to it.
//! Downloads go through `curl`, the checksum through `sha256sum` or
//! `shasum`, and the executable is replaced by renaming a file written next
//! to it, so a crash never leaves a half-written binary.

use std::path::{Path, PathBuf};

use anyhow::{Context as _, bail};

use crate::process::{Cmd, Runner};
use crate::sdk::SEKVENT_GIT_URL;
use crate::template::set_executable;

/// Tag of the rolling CLI release.
pub const RELEASE_TAG: &str = "cli-latest";

/// Name of the binary inside the archive.
pub const BINARY: &str = "cargo-sekvent";

/// The target triple this binary was built for.
pub const BUILD_TARGET: &str = env!("SEKVENT_BUILD_TARGET");

/// Archive name for `target`.
pub fn asset_name(target: &str) -> String {
    format!("{BINARY}-{target}.tar.gz")
}

/// Download URL of the archive for `target`.
pub fn asset_url(target: &str) -> String {
    format!(
        "{SEKVENT_GIT_URL}/releases/download/{RELEASE_TAG}/{}",
        asset_name(target)
    )
}

/// The hex digest of a `.sha256` file (`<hex>` or `<hex>  <name>`).
pub fn parse_checksum(text: &str) -> Option<String> {
    let digest = text.split_whitespace().next()?.to_ascii_lowercase();
    (digest.len() == 64 && digest.chars().all(|c| c.is_ascii_hexdigit())).then_some(digest)
}

/// `curl` fetching `url` into `output`, failing on HTTP errors.
pub fn download_command(url: &str, output: &Path) -> Cmd {
    Cmd::new("curl")
        .args([
            "--fail",
            "--silent",
            "--show-error",
            "--location",
            "--retry",
            "2",
        ])
        .arg("--output")
        .arg(output.display().to_string())
        .arg(url)
}

/// A SHA-256 command available on this machine.
pub fn checksum_command(file: &Path) -> anyhow::Result<Cmd> {
    checksum_command_with(file, |program| which::which(program).is_ok())
}

fn checksum_command_with(file: &Path, installed: impl Fn(&str) -> bool) -> anyhow::Result<Cmd> {
    let path = file.display().to_string();
    if installed("sha256sum") {
        Ok(Cmd::new("sha256sum").arg(path))
    } else if installed("shasum") {
        Ok(Cmd::new("shasum").args(["-a", "256"]).arg(path))
    } else {
        bail!("neither `sha256sum` nor `shasum` is installed")
    }
}

fn find_binary(dir: &Path) -> Option<PathBuf> {
    let names = [BINARY.to_owned(), format!("{BINARY}.exe")];
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if let Some(found) = find_binary(&path) {
                return Some(found);
            }
        } else if path
            .file_name()
            .is_some_and(|name| names.iter().any(|candidate| name == candidate.as_str()))
        {
            return Some(path);
        }
    }
    None
}

fn run(runner: &dyn Runner, cmd: &Cmd) -> anyhow::Result<()> {
    let code = runner.status(cmd)?;
    if code != 0 {
        bail!("`{cmd}` exited with {code}");
    }
    Ok(())
}

/// Download, verify and install the latest release over `exe`.
pub fn update_executable(runner: &dyn Runner, exe: &Path, target: &str) -> anyhow::Result<()> {
    let work = tempfile::tempdir().context("cannot create a temporary directory")?;
    let archive = work.path().join(asset_name(target));
    let checksum = work.path().join(format!("{}.sha256", asset_name(target)));
    let url = asset_url(target);
    println!("==> self-update: downloading {url}");
    run(runner, &download_command(&url, &archive))?;
    run(
        runner,
        &download_command(&format!("{url}.sha256"), &checksum),
    )?;

    let expected = parse_checksum(&std::fs::read_to_string(&checksum)?)
        .context("the .sha256 file holds no SHA-256 digest")?;
    let output = runner.output(&checksum_command(&archive)?, None)?;
    let actual = parse_checksum(&output.stdout).context("cannot read the computed digest")?;
    if actual != expected {
        bail!(
            "checksum mismatch for {}: expected {expected}, got {actual}",
            asset_name(target)
        );
    }

    let unpacked = work.path().join("unpacked");
    std::fs::create_dir_all(&unpacked)?;
    run(
        runner,
        &Cmd::new("tar")
            .arg("-xzf")
            .arg(archive.display().to_string())
            .arg("-C")
            .arg(unpacked.display().to_string()),
    )?;
    let binary = find_binary(&unpacked).context("the archive holds no cargo-sekvent binary")?;

    let dir = exe
        .parent()
        .context("the current executable has no parent directory")?;
    let mut staged = tempfile::Builder::new()
        .prefix(".cargo-sekvent-update-")
        .tempfile_in(dir)
        .with_context(|| format!("cannot write into {}", dir.display()))?;
    std::io::copy(&mut std::fs::File::open(&binary)?, staged.as_file_mut())?;
    staged.as_file().sync_all()?;
    set_executable(staged.path())?;
    staged
        .persist(exe)
        .with_context(|| format!("cannot replace {}", exe.display()))?;
    println!("==> self-update: installed {}", exe.display());
    Ok(())
}

/// `cargo sekvent self-update` for the running executable.
pub fn self_update(runner: &dyn Runner) -> anyhow::Result<()> {
    update_current(runner, std::env::current_exe(), BUILD_TARGET)
}

fn update_current(
    runner: &dyn Runner,
    exe: std::io::Result<PathBuf>,
    target: &str,
) -> anyhow::Result<()> {
    if target.is_empty() {
        bail!("this build does not know its target triple");
    }
    let exe = exe.context("cannot locate the running executable")?;
    let exe = exe.canonicalize().unwrap_or(exe);
    update_executable(runner, &exe, target)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::process::fake::FakeRunner;

    #[test]
    fn asset_names_follow_the_release_layout() {
        assert_eq!(
            asset_url("x86_64-unknown-linux-gnu"),
            "https://github.com/westito/sekvent-api/releases/download/cli-latest/\
             cargo-sekvent-x86_64-unknown-linux-gnu.tar.gz"
        );
        assert!(!BUILD_TARGET.is_empty());
    }

    #[test]
    fn checksum_files_are_parsed() {
        let digest = "a".repeat(64);
        assert_eq!(parse_checksum(&digest).as_deref(), Some(digest.as_str()));
        assert_eq!(
            parse_checksum(&format!("{}  cargo-sekvent.tar.gz\n", "AB".repeat(32))),
            Some("ab".repeat(32))
        );
        assert_eq!(parse_checksum("abc"), None);
        assert_eq!(parse_checksum(""), None);
        assert_eq!(parse_checksum(&"g".repeat(64)), None);
    }

    #[test]
    fn downloads_fail_on_http_errors() {
        assert_eq!(
            download_command("https://x/a", Path::new("/tmp/a")).to_string(),
            "curl --fail --silent --show-error --location --retry 2 --output /tmp/a https://x/a"
        );
    }

    #[test]
    fn a_failed_download_stops_before_touching_the_executable() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("cargo-sekvent");
        std::fs::write(&exe, "old").unwrap();
        let runner = FakeRunner::with_codes(&[22]);
        let error = update_executable(&runner, &exe, "x86_64-unknown-linux-gnu").unwrap_err();
        assert!(error.to_string().contains("exited with 22"), "{error}");
        assert_eq!(std::fs::read_to_string(&exe).unwrap(), "old");
        assert_eq!(runner.calls().len(), 1);
    }

    #[test]
    fn binaries_are_found_in_nested_archives() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(find_binary(dir.path()), None);
        let nested = dir.path().join("cargo-sekvent-x/bin");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(nested.join(BINARY), "bin").unwrap();
        assert_eq!(find_binary(dir.path()), Some(nested.join(BINARY)));
    }

    #[test]
    fn the_checksum_tool_is_picked_by_availability() {
        let file = Path::new("/tmp/a.tar.gz");
        assert_eq!(
            checksum_command_with(file, |_| true).unwrap().to_string(),
            "sha256sum /tmp/a.tar.gz"
        );
        assert_eq!(
            checksum_command_with(file, |program| program == "shasum")
                .unwrap()
                .to_string(),
            "shasum -a 256 /tmp/a.tar.gz"
        );
        let error = checksum_command_with(file, |_| false).unwrap_err();
        assert!(error.to_string().contains("sha256sum"), "{error}");
    }

    const TARGET: &str = "x86_64-unknown-linux-gnu";
    const NEW_BINARY: &str = "new binary";

    /// Serves a release: `curl` writes the archive or the published digest,
    /// the checksum tool prints `computed`, and `tar` unpacks a binary when
    /// `with_binary`.
    struct Release {
        published: String,
        computed: String,
        with_binary: bool,
        calls: std::cell::RefCell<Vec<String>>,
    }

    impl Release {
        fn new() -> Self {
            Self {
                published: format!("{}  {}\n", "ab".repeat(32), asset_name(TARGET)),
                computed: "ab".repeat(32),
                with_binary: true,
                calls: std::cell::RefCell::default(),
            }
        }

        fn last_arg(cmd: &Cmd) -> &str {
            cmd.args.last().map_or("", String::as_str)
        }
    }

    impl Runner for Release {
        fn status(&self, cmd: &Cmd) -> std::io::Result<i32> {
            self.calls.borrow_mut().push(cmd.program.clone());
            match cmd.program.as_str() {
                "curl" => {
                    let output = cmd
                        .args
                        .iter()
                        .skip_while(|arg| *arg != "--output")
                        .nth(1)
                        .expect("curl --output");
                    let body = if Self::last_arg(cmd).ends_with(".sha256") {
                        self.published.as_str()
                    } else {
                        "archive"
                    };
                    std::fs::write(output, body)?;
                    Ok(0)
                }
                "tar" => {
                    if self.with_binary {
                        let nested = Path::new(Self::last_arg(cmd)).join("release");
                        std::fs::create_dir_all(&nested)?;
                        std::fs::write(nested.join(BINARY), NEW_BINARY)?;
                    }
                    Ok(0)
                }
                _ => Ok(127),
            }
        }

        fn output(
            &self,
            cmd: &Cmd,
            _timeout: Option<std::time::Duration>,
        ) -> std::io::Result<crate::process::CmdOutput> {
            self.calls.borrow_mut().push(cmd.program.clone());
            Ok(crate::process::CmdOutput {
                code: 0,
                stdout: format!("{}  {}\n", self.computed, Self::last_arg(cmd)),
                stderr: String::new(),
            })
        }

        fn exec(&self, cmd: &Cmd) -> std::io::Result<i32> {
            self.status(cmd)
        }
    }

    fn installed_exe() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join(BINARY);
        std::fs::write(&exe, "old").unwrap();
        (dir, exe)
    }

    #[test]
    fn a_verified_release_replaces_the_executable() {
        let (dir, exe) = installed_exe();
        let release = Release::new();
        update_executable(&release, &exe, TARGET).unwrap();
        assert_eq!(std::fs::read_to_string(&exe).unwrap(), NEW_BINARY);
        assert_eq!(
            release.calls.borrow()[..2],
            ["curl".to_owned(), "curl".to_owned()]
        );
        assert_eq!(release.calls.borrow().last().unwrap(), "tar");
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(leftovers, [BINARY]);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&exe).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o755);
        }
    }

    #[test]
    fn a_checksum_mismatch_keeps_the_old_executable() {
        let (_dir, exe) = installed_exe();
        let release = Release {
            computed: "cd".repeat(32),
            ..Release::new()
        };
        let error = update_executable(&release, &exe, TARGET).unwrap_err();
        assert!(error.to_string().contains("checksum mismatch"), "{error}");
        assert_eq!(std::fs::read_to_string(&exe).unwrap(), "old");
    }

    #[test]
    fn unreadable_digests_are_errors() {
        let (_dir, exe) = installed_exe();
        let published = Release {
            published: "not found\n".into(),
            ..Release::new()
        };
        let error = update_executable(&published, &exe, TARGET).unwrap_err();
        assert!(error.to_string().contains("no SHA-256 digest"), "{error}");

        let computed = Release {
            computed: "?".into(),
            ..Release::new()
        };
        let error = update_executable(&computed, &exe, TARGET).unwrap_err();
        assert!(error.to_string().contains("computed digest"), "{error}");
        assert_eq!(std::fs::read_to_string(&exe).unwrap(), "old");
    }

    #[test]
    fn an_archive_without_the_binary_is_rejected() {
        let (_dir, exe) = installed_exe();
        let release = Release {
            with_binary: false,
            ..Release::new()
        };
        let error = update_executable(&release, &exe, TARGET).unwrap_err();
        assert!(
            error.to_string().contains("no cargo-sekvent binary"),
            "{error}"
        );
    }

    #[test]
    fn the_executable_needs_a_writable_parent() {
        let error = update_executable(&Release::new(), Path::new("/"), TARGET).unwrap_err();
        assert!(error.to_string().contains("no parent directory"), "{error}");

        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("missing").join(BINARY);
        let error = update_executable(&Release::new(), &exe, TARGET).unwrap_err();
        assert!(
            error.to_string().starts_with("cannot write into"),
            "{error}"
        );
    }

    #[test]
    fn a_failed_checksum_download_stops_the_update() {
        let (_dir, exe) = installed_exe();
        let runner = FakeRunner::with_codes(&[0, 22]);
        let error = update_executable(&runner, &exe, TARGET).unwrap_err();
        assert!(error.to_string().contains(".sha256"), "{error}");
        assert_eq!(runner.calls().len(), 2);
    }

    #[test]
    fn the_running_executable_is_resolved_first() {
        let runner = FakeRunner::default();
        let error = update_current(&runner, Ok(PathBuf::from("/x")), "").unwrap_err();
        assert!(error.to_string().contains("target triple"), "{error}");
        let missing = Err(std::io::Error::other("gone"));
        let error = update_current(&runner, missing, TARGET).unwrap_err();
        assert!(error.to_string().contains("running executable"), "{error}");
        assert!(runner.calls().is_empty());

        let (_dir, exe) = installed_exe();
        update_current(&Release::new(), Ok(exe.clone()), TARGET).unwrap();
        assert_eq!(std::fs::read_to_string(&exe).unwrap(), NEW_BINARY);
    }
}
