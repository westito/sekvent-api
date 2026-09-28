//! The sekvent section of a project's `AGENTS.md`, kept between markers so
//! it can be refreshed without touching the rest of the file.

use std::path::Path;

use thiserror::Error;

use crate::template::{self, TemplateError, Vars};

/// Opening marker of the managed section.
pub const BEGIN: &str = "<!-- sekvent:begin -->";

/// Closing marker of the managed section.
pub const END: &str = "<!-- sekvent:end -->";

/// The template kind and file that hold the section body.
pub const SECTION_KIND: &str = "agents";

/// The section body rendered from the `agents` template, without markers.
pub fn section_body(vars: &Vars) -> Result<String, TemplateError> {
    let files = template::render_kind(SECTION_KIND, vars)?;
    let file = files
        .iter()
        .find(|file| file.path == "section.md")
        .ok_or_else(|| TemplateError::MissingKind(format!("{SECTION_KIND}/section.md.tmpl")))?;
    Ok(strip_markers(&file.text()))
}

/// Remove marker lines a template may already carry.
pub fn strip_markers(body: &str) -> String {
    body.lines()
        .filter(|line| {
            let trimmed = line.trim();
            trimmed != BEGIN && trimmed != END
        })
        .collect::<Vec<_>>()
        .join("\n")
        .trim_matches('\n')
        .to_owned()
}

/// The file has an opening marker with no closing marker after it, so the
/// extent of the managed section is unknown.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error(
    "line {line}: `<!-- sekvent:begin -->` has no matching `<!-- sekvent:end -->` after it; \
     add the closing marker where the sekvent section ends (or remove the opening one) and \
     run again"
)]
pub struct UnclosedSection {
    /// 1-based line of the opening marker.
    pub line: usize,
}

/// Insert or replace the managed section in `existing` (`None`: new file).
///
/// Everything outside the markers is kept. An opening marker without a
/// closing one is an error rather than a guess, since replacing up to the
/// end of the file would delete the user's text after it.
pub fn upsert(existing: Option<&str>, body: &str) -> Result<String, UnclosedSection> {
    let block = format!("{BEGIN}\n{}\n{END}\n", body.trim_matches('\n'));
    let Some(existing) = existing else {
        return Ok(block);
    };
    if let Some(start) = existing.find(BEGIN) {
        let Some(end_offset) = existing[start..].find(END) else {
            let line = existing[..start].matches('\n').count() + 1;
            return Err(UnclosedSection { line });
        };
        let end = start + end_offset + END.len();
        let after = existing[end..]
            .strip_prefix('\n')
            .unwrap_or(&existing[end..]);
        return Ok(format!("{}{block}{after}", &existing[..start]));
    }
    let mut out = existing.trim_end_matches('\n').to_owned();
    if !out.is_empty() {
        out.push_str("\n\n");
    }
    out.push_str(&block);
    Ok(out)
}

/// Update the section in `file`, creating the file if needed. Returns
/// whether the file changed. An [`UnclosedSection`] leaves the file alone
/// and is reported as [`std::io::ErrorKind::InvalidData`] naming the file
/// and line.
pub fn update_file(file: &Path, body: &str) -> std::io::Result<bool> {
    let existing = match std::fs::read_to_string(file) {
        Ok(text) => Some(text),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error),
    };
    let updated = upsert(existing.as_deref(), body).map_err(|error| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("{}: {error}", file.display()),
        )
    })?;
    if existing.as_deref() == Some(updated.as_str()) {
        return Ok(false);
    }
    if let Some(parent) = file.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(file, updated)?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_new_file_gets_just_the_section() {
        assert_eq!(
            upsert(None, "hello\n").unwrap(),
            format!("{BEGIN}\nhello\n{END}\n")
        );
    }

    #[test]
    fn the_section_is_appended_once_and_then_replaced() {
        let original = "# Project\n\nOwn rules.\n";
        let first = upsert(Some(original), "v1").unwrap();
        assert_eq!(
            first,
            format!("# Project\n\nOwn rules.\n\n{BEGIN}\nv1\n{END}\n")
        );
        let tail = format!("{first}\n## Later\n");
        let second = upsert(Some(&tail), "v2").unwrap();
        assert_eq!(
            second,
            format!("# Project\n\nOwn rules.\n\n{BEGIN}\nv2\n{END}\n\n## Later\n")
        );
        assert_eq!(upsert(Some(&second), "v2").unwrap(), second);
    }

    #[test]
    fn template_markers_are_not_doubled() {
        assert_eq!(strip_markers(&format!("{BEGIN}\nbody\n{END}\n")), "body");
        assert_eq!(strip_markers("\nkeep\n  indented\n"), "keep\n  indented");
    }

    #[test]
    fn files_are_created_and_left_alone_when_current() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("AGENTS.md");
        assert!(update_file(&file, "body").unwrap());
        assert!(!update_file(&file, "body").unwrap());
        assert!(update_file(&file, "other").unwrap());
        let text = std::fs::read_to_string(&file).unwrap();
        assert_eq!(text, format!("{BEGIN}\nother\n{END}\n"));
    }

    #[test]
    fn an_opening_marker_without_a_closing_one_is_refused() {
        let text = format!("# Project\n\nintro\n{BEGIN}\nold\n\n## Mine\nkeep me\n");
        let error = upsert(Some(&text), "new").unwrap_err();
        assert_eq!(error, UnclosedSection { line: 4 });
        assert!(error.to_string().starts_with("line 4: "), "{error}");
        assert!(error.to_string().contains(BEGIN), "{error}");
        assert!(error.to_string().contains(END), "{error}");

        let before_begin = format!("{END}\n{BEGIN}\nrest\n");
        assert_eq!(
            upsert(Some(&before_begin), "x"),
            Err(UnclosedSection { line: 2 })
        );

        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("AGENTS.md");
        std::fs::write(&file, &text).unwrap();
        let error = update_file(&file, "new").unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("AGENTS.md: line 4"), "{error}");
        assert_eq!(std::fs::read_to_string(&file).unwrap(), text);
    }

    #[test]
    fn unreadable_files_are_errors() {
        let dir = tempfile::tempdir().unwrap();
        assert!(update_file(dir.path(), "body").is_err());
    }
}
