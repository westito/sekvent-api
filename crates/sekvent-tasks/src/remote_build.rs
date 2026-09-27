//! The `sekvent` command in a project's `.remote-build.toml`.
//!
//! `rrb run sekvent <args>` runs `[run.commands].sekvent` in the builder with
//! the arguments appended; the command is the `.sekvent/run.sh` bootstrap,
//! which installs the matching `cargo-sekvent` and runs `cargo sekvent "$@"`.

use anyhow::Context as _;
use toml_edit::{Array, DocumentMut, Item, Table};

/// File name of the rrb configuration.
pub const REMOTE_BUILD_FILE: &str = ".remote-build.toml";

/// The `[run.commands]` key compiling commands are forwarded to.
pub const COMMAND_NAME: &str = "sekvent";

/// The bootstrap script, relative to the project root.
pub const BOOTSTRAP: &str = ".sekvent/run.sh";

/// Result of adding the `sekvent` command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Edit {
    /// The command is already there as expected.
    Unchanged,
    /// The new file text.
    Updated(String),
    /// A different `sekvent` command exists; `--force` replaces it.
    Conflict(String),
}

fn expected() -> Array {
    std::iter::once(BOOTSTRAP).collect()
}

fn is_expected(item: &Item) -> bool {
    item.as_array().is_some_and(|array| {
        let values: Vec<Option<&str>> = array.iter().map(toml_edit::Value::as_str).collect();
        values == [Some(BOOTSTRAP)]
    })
}

fn table_mut<'a>(parent: &'a mut Table, key: &str) -> anyhow::Result<&'a mut Table> {
    if !parent.contains_key(key) {
        let mut table = Table::new();
        table.set_implicit(true);
        parent.insert(key, Item::Table(table));
    }
    parent
        .get_mut(key)
        .and_then(Item::as_table_mut)
        .with_context(|| format!("`{key}` in {REMOTE_BUILD_FILE} is not a table"))
}

/// Add `[run.commands].sekvent` to `text`, keeping comments and layout.
pub fn ensure_command(text: &str, force: bool) -> anyhow::Result<Edit> {
    let mut doc: DocumentMut = text
        .parse()
        .with_context(|| format!("cannot parse {REMOTE_BUILD_FILE}"))?;
    let run = table_mut(doc.as_table_mut(), "run")?;
    let commands = table_mut(run, "commands")?;
    if let Some(existing) = commands.get(COMMAND_NAME) {
        if is_expected(existing) {
            return Ok(Edit::Unchanged);
        }
        if !force {
            let shown = existing
                .as_value()
                .map_or_else(|| existing.to_string(), ToString::to_string);
            return Ok(Edit::Conflict(shown.trim().to_owned()));
        }
    }
    commands.insert(COMMAND_NAME, toml_edit::value(expected()));
    Ok(Edit::Updated(doc.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXISTING: &str = r#"# rrb config for orders.
[project]
name = "orders"

[run]
workdir = "/work"
default = "gate"   # the usual one

[run.commands]
# Type-check only.
check = ["cargo", "check"]

[env]
set = {}
"#;

    #[test]
    fn the_command_is_inserted_keeping_comments() {
        let Edit::Updated(text) = ensure_command(EXISTING, false).unwrap() else {
            panic!("expected an update");
        };
        assert!(text.starts_with("# rrb config for orders.\n"), "{text}");
        assert!(
            text.contains("default = \"gate\"   # the usual one\n"),
            "{text}"
        );
        assert!(
            text.contains(
                "# Type-check only.\ncheck = [\"cargo\", \"check\"]\nsekvent = [\".sekvent/run.sh\"]\n"
            ),
            "{text}"
        );
        assert!(text.contains("[env]\nset = {}\n"), "{text}");
        assert_eq!(ensure_command(&text, false).unwrap(), Edit::Unchanged);
    }

    #[test]
    fn a_different_command_is_a_conflict_unless_forced() {
        let text = EXISTING.replace(
            "check = [\"cargo\", \"check\"]",
            "check = [\"cargo\", \"check\"]\nsekvent = [\"custom.sh\"]",
        );
        assert_eq!(
            ensure_command(&text, false).unwrap(),
            Edit::Conflict("[\"custom.sh\"]".into())
        );
        let Edit::Updated(forced) = ensure_command(&text, true).unwrap() else {
            panic!("expected an update");
        };
        assert!(
            forced.contains("sekvent = [\".sekvent/run.sh\"]"),
            "{forced}"
        );
        assert!(!forced.contains("custom.sh"), "{forced}");
    }

    #[test]
    fn missing_tables_are_created() {
        let Edit::Updated(text) = ensure_command("[project]\nname = \"x\"\n", false).unwrap()
        else {
            panic!("expected an update");
        };
        let parsed: toml::Table = text.parse().unwrap();
        assert_eq!(
            parsed["run"]["commands"]["sekvent"],
            toml::Value::Array(vec![toml::Value::String(BOOTSTRAP.into())])
        );
        assert!(ensure_command("run = 1\n", false).is_err());
    }
}
