use std::fs;
use std::path::{Path, PathBuf};

use crate::ProtoBuildError;

/// Every `*.proto` file below `root`, sorted, skipping hidden directories,
/// plus every directory visited (for `rerun-if-changed`, so adding a file
/// triggers a rebuild).
pub(crate) fn discover(root: &Path) -> Result<(Vec<PathBuf>, Vec<PathBuf>), ProtoBuildError> {
    let mut files = Vec::new();
    let mut dirs = Vec::new();
    walk(root, &mut files, &mut dirs)?;
    files.sort();
    dirs.sort();
    Ok((files, dirs))
}

fn walk(
    dir: &Path,
    files: &mut Vec<PathBuf>,
    dirs: &mut Vec<PathBuf>,
) -> Result<(), ProtoBuildError> {
    dirs.push(dir.to_path_buf());
    let entries = fs::read_dir(dir).map_err(|source| ProtoBuildError::Io {
        path: dir.to_path_buf(),
        source,
    })?;
    for entry in entries {
        let entry = entry.map_err(|source| ProtoBuildError::Io {
            path: dir.to_path_buf(),
            source,
        })?;
        let path = entry.path();
        let hidden = entry.file_name().to_string_lossy().starts_with('.');
        if path.is_dir() {
            if !hidden {
                walk(&path, files, dirs)?;
            }
        } else if path.extension().is_some_and(|ext| ext == "proto") {
            files.push(path);
        }
    }
    Ok(())
}

/// The `package` declared by a `.proto` source, if any.
///
/// Comments and string literals are blanked first, so a `package` inside
/// either is not mistaken for the declaration.
pub(crate) fn package_of(source: &str) -> Result<Option<String>, &'static str> {
    let cleaned = strip_comments_and_strings(source);
    for statement in cleaned.split(';') {
        let statement = statement.trim();
        let Some(rest) = statement.strip_prefix("package") else {
            continue;
        };
        if !rest.is_empty() && !rest.starts_with(char::is_whitespace) {
            continue;
        }
        let name = rest.trim();
        if is_package_name(name) {
            return Ok(Some(name.to_owned()));
        }
        return Err("malformed package declaration");
    }
    Ok(None)
}

fn is_package_name(name: &str) -> bool {
    !name.is_empty()
        && name.split('.').all(|segment| {
            let mut chars = segment.chars();
            chars
                .next()
                .is_some_and(|first| first.is_ascii_alphabetic() || first == '_')
                && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
        })
}

fn strip_comments_and_strings(source: &str) -> String {
    let mut out = String::with_capacity(source.len());
    let mut chars = source.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '/' if chars.peek() == Some(&'/') => {
                for next in chars.by_ref() {
                    if next == '\n' {
                        out.push('\n');
                        break;
                    }
                }
            }
            '/' if chars.peek() == Some(&'*') => {
                chars.next();
                let mut previous = '\0';
                for next in chars.by_ref() {
                    if previous == '*' && next == '/' {
                        break;
                    }
                    previous = next;
                }
                out.push(' ');
            }
            '"' | '\'' => {
                let quote = c;
                let mut escaped = false;
                for next in chars.by_ref() {
                    if escaped {
                        escaped = false;
                    } else if next == '\\' {
                        escaped = true;
                    } else if next == quote {
                        break;
                    }
                }
                out.push_str("\"\"");
            }
            _ => out.push(c),
        }
    }
    out
}

/// Rust keywords that cannot be module names without `r#`. (`crate`, `self`,
/// `Self` and `super` cannot be raw identifiers at all; a package segment
/// with one of those names is not supported.)
const KEYWORDS: [&str; 47] = [
    "as", "async", "await", "break", "const", "continue", "dyn", "else", "enum", "extern", "false",
    "fn", "for", "if", "impl", "in", "let", "loop", "match", "mod", "move", "mut", "pub", "ref",
    "return", "static", "struct", "trait", "true", "type", "unsafe", "use", "where", "while",
    "abstract", "become", "box", "do", "final", "macro", "override", "priv", "typeof", "unsized",
    "virtual", "yield", "try",
];

/// A package segment as a Rust module name.
pub(crate) fn module_ident(segment: &str) -> String {
    if KEYWORDS.contains(&segment) {
        format!("r#{segment}")
    } else {
        segment.to_owned()
    }
}

/// The Rust path of `package` inside the crate at `base`:
/// `("::billing_proto", "billing.v1")` becomes `::billing_proto::billing::v1`.
pub(crate) fn rust_path(base: &str, package: &str) -> String {
    let base = base.trim_end_matches(':');
    let modules: Vec<String> = package.split('.').map(module_ident).collect();
    format!("{base}::{}", modules.join("::"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_package_declaration_is_found() {
        let source = "syntax = \"proto3\";\n\npackage billing.v1;\n\nmessage Invoice {}\n";
        assert_eq!(package_of(source).unwrap().as_deref(), Some("billing.v1"));
    }

    #[test]
    fn comments_and_strings_do_not_count() {
        let source = "// package fake.one;\n\
                      /* package fake.two; */\n\
                      option java_package = \"package fake.three;\";\n\
                      option x = 'it\\'s; package fake.four';\n\
                      package\treal.v2 ;\n";
        assert_eq!(package_of(source).unwrap().as_deref(), Some("real.v2"));
    }

    #[test]
    fn files_without_a_package_have_none() {
        assert_eq!(
            package_of("syntax = \"proto3\"; message A {}").unwrap(),
            None
        );
        assert_eq!(package_of("packages_are_not_this;").unwrap(), None);
        assert_eq!(package_of("").unwrap(), None);
    }

    #[test]
    fn malformed_packages_are_errors() {
        assert!(package_of("package ;").is_err());
        assert!(package_of("package 1abc;").is_err());
        assert!(package_of("package a..b;").is_err());
        assert!(package_of("package a-b;").is_err());
    }

    #[test]
    fn an_unterminated_block_comment_swallows_the_rest() {
        assert_eq!(package_of("/* package a.b;").unwrap(), None);
    }

    #[test]
    fn extern_paths_follow_the_package_segments() {
        assert_eq!(
            rust_path("::billing_proto", "billing.v1"),
            "::billing_proto::billing::v1"
        );
        assert_eq!(
            rust_path("crate::proto::", "orders.v2"),
            "crate::proto::orders::v2"
        );
        assert_eq!(rust_path("::p", "acme.type.v1"), "::p::acme::r#type::v1");
    }

    #[test]
    fn keywords_become_raw_identifiers() {
        assert_eq!(module_ident("match"), "r#match");
        assert_eq!(module_ident("orders"), "orders");
    }

    #[test]
    fn discovery_walks_sorted_and_skips_hidden_directories() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().to_path_buf();
        fs::create_dir_all(root.join("b/v1")).unwrap();
        fs::create_dir_all(root.join("a")).unwrap();
        fs::create_dir_all(root.join(".git")).unwrap();
        fs::write(root.join("b/v1/b.proto"), "").unwrap();
        fs::write(root.join("a/a.proto"), "").unwrap();
        fs::write(root.join("a/readme.md"), "").unwrap();
        fs::write(root.join(".git/x.proto"), "").unwrap();

        let (files, dirs) = discover(&root).unwrap();
        assert_eq!(files, [root.join("a/a.proto"), root.join("b/v1/b.proto")]);
        assert!(dirs.contains(&root));
        assert!(dirs.contains(&root.join("b/v1")));
        assert!(!dirs.contains(&root.join(".git")));
    }

    #[test]
    fn discovering_a_missing_root_names_it() {
        let missing = Path::new("/nonexistent/sekvent/protos");
        let error = discover(missing).unwrap_err();
        assert!(error.to_string().contains("/nonexistent/sekvent/protos"));
    }
}
