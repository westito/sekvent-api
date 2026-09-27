//! `skills install`: copy the embedded agent skills into skill directories.
//!
//! Only skill directories whose name starts with `sekvent` are installed, and
//! only those are ever replaced; other skills in the destination are left
//! alone.

use std::io;
use std::path::{Path, PathBuf};

use include_dir::Dir;

use crate::embedded::SKILLS;
use crate::template::set_executable;

/// Prefix of the skill directories sekvent owns.
pub const OWNED_PREFIX: &str = "sekvent";

fn dir_name(dir: &Dir<'_>) -> String {
    dir.path()
        .file_name()
        .map_or_else(String::new, |name| name.to_string_lossy().into_owned())
}

/// The sekvent-owned top-level skill directories of `root`.
pub fn owned_skills<'a>(root: &Dir<'a>) -> Vec<&'a Dir<'a>> {
    root.dirs()
        .filter(|dir| dir_name(dir).starts_with(OWNED_PREFIX))
        .collect()
}

fn write_tree(dir: &Dir<'_>, base: &Path, target: &Path) -> io::Result<()> {
    for file in dir.files() {
        let relative = file.path().strip_prefix(base).unwrap_or(file.path());
        let path = target.join(relative);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, file.contents())?;
        let in_bin = relative
            .parent()
            .is_some_and(|parent| parent.ends_with("bin"));
        let script = relative
            .extension()
            .is_some_and(|extension| extension == "sh");
        if in_bin || script {
            set_executable(&path)?;
        }
    }
    for child in dir.dirs() {
        write_tree(child, base, target)?;
    }
    Ok(())
}

/// Install every owned skill of `root` into `dest`, replacing earlier
/// copies. Returns the installed skill names.
pub fn install_into(root: &Dir<'_>, dest: &Path) -> io::Result<Vec<String>> {
    std::fs::create_dir_all(dest)?;
    let mut installed = Vec::new();
    for skill in owned_skills(root) {
        let name = dir_name(skill);
        let target = dest.join(&name);
        match std::fs::symlink_metadata(&target) {
            Ok(meta) if meta.is_dir() => std::fs::remove_dir_all(&target)?,
            Ok(_) => std::fs::remove_file(&target)?,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        write_tree(skill, skill.path(), &target)?;
        installed.push(name);
    }
    Ok(installed)
}

/// `~/.kodein/skills`, plus `~/.claude/skills` when that directory exists.
pub fn default_destinations(home: &Path) -> Vec<PathBuf> {
    let mut out = vec![home.join(".kodein/skills")];
    let claude = home.join(".claude/skills");
    if claude.is_dir() {
        out.push(claude);
    }
    out
}

/// `cargo sekvent skills install`.
pub fn install(dest: Option<&Path>) -> anyhow::Result<()> {
    let destinations = if let Some(dest) = dest {
        vec![dest.to_owned()]
    } else {
        let home = dirs::home_dir()
            .ok_or_else(|| anyhow::anyhow!("cannot find the home directory; pass --dest"))?;
        default_destinations(&home)
    };
    for destination in destinations {
        let installed = install_into(&SKILLS, &destination)?;
        println!(
            "installed {} into {}",
            if installed.is_empty() {
                "no skills".to_owned()
            } else {
                installed.join(", ")
            },
            destination.display()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use include_dir::{DirEntry, File};

    use super::*;

    static BIN: [DirEntry<'static>; 1] = [DirEntry::File(File::new(
        "sekvent-gate/bin/run",
        b"#!/bin/sh\n",
    ))];
    static GATE: [DirEntry<'static>; 2] = [
        DirEntry::File(File::new("sekvent-gate/SKILL.md", b"new")),
        DirEntry::Dir(Dir::new("sekvent-gate/bin", &BIN)),
    ];
    static OTHER: [DirEntry<'static>; 1] =
        [DirEntry::File(File::new("other/SKILL.md", b"not ours"))];
    static TOP: [DirEntry<'static>; 3] = [
        DirEntry::Dir(Dir::new("sekvent-gate", &GATE)),
        DirEntry::Dir(Dir::new("other", &OTHER)),
        DirEntry::File(File::new("README.md", b"index")),
    ];
    static ROOT: Dir<'static> = Dir::new("", &TOP);

    #[test]
    fn only_owned_skills_are_installed_and_replaced() {
        let dest = tempfile::tempdir().unwrap();
        let stale = dest.path().join("sekvent-gate/old.md");
        std::fs::create_dir_all(stale.parent().unwrap()).unwrap();
        std::fs::write(&stale, "old").unwrap();
        std::fs::create_dir_all(dest.path().join("other")).unwrap();
        std::fs::write(dest.path().join("other/SKILL.md"), "user skill").unwrap();

        let installed = install_into(&ROOT, dest.path()).unwrap();
        assert_eq!(installed, ["sekvent-gate"]);
        assert!(!stale.exists());
        assert_eq!(
            std::fs::read_to_string(dest.path().join("sekvent-gate/SKILL.md")).unwrap(),
            "new"
        );
        assert_eq!(
            std::fs::read_to_string(dest.path().join("other/SKILL.md")).unwrap(),
            "user skill"
        );
        assert!(!dest.path().join("README.md").exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dest.path().join("sekvent-gate/bin/run"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o755);
        }
    }

    #[test]
    fn claude_skills_are_added_only_when_present() {
        let home = tempfile::tempdir().unwrap();
        assert_eq!(
            default_destinations(home.path()),
            [home.path().join(".kodein/skills")]
        );
        std::fs::create_dir_all(home.path().join(".claude/skills")).unwrap();
        assert_eq!(default_destinations(home.path()).len(), 2);
    }

    #[test]
    fn the_embedded_skills_are_all_owned() {
        let dest = tempfile::tempdir().unwrap();
        let installed = install_into(&SKILLS, dest.path()).unwrap();
        assert!(installed.iter().all(|name| name.starts_with(OWNED_PREFIX)));
    }
}
