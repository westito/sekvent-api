use std::fmt;

use sekvent_error::AppError;

use crate::error::reasons;

/// Longest object key, in bytes of UTF-8 (the S3 limit).
pub const MAX_KEY_LEN: usize = 1024;

/// A validated object key.
///
/// A key is 1 to [`MAX_KEY_LEN`] bytes of UTF-8 made of `/`-separated
/// segments. It must not start with `/`, and no segment may be empty, `.`
/// or `..`, so a key never collapses into another one when a URL is
/// normalised. Control characters and `\` are rejected.
///
/// Keys are not secret, but they appear in URLs and logs: build them from
/// identifiers the server chose (`attachments/<issue>/<uuid>`), never from
/// a user's file name. Keep the file name in your database and hand it to
/// [`PresignGet::attachment`](crate::PresignGet::attachment) on download.
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ObjectKey(String);

impl ObjectKey {
    /// Validate `key`. Fails with `INVALID_ARGUMENT`, reason
    /// `INVALID_OBJECT_KEY`; the message never quotes the key.
    pub fn new(key: impl Into<String>) -> Result<Self, AppError> {
        let key = key.into();
        match problem(&key) {
            None => Ok(Self(key)),
            Some(problem) => Err(
                AppError::invalid_argument(format!("the object key {problem}"))
                    .with_reason(reasons::INVALID_OBJECT_KEY),
            ),
        }
    }

    /// The key.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The key as an owned string.
    pub fn into_string(self) -> String {
        self.0
    }
}

fn problem(key: &str) -> Option<&'static str> {
    if key.is_empty() {
        return Some("must not be empty");
    }
    if key.len() > MAX_KEY_LEN {
        return Some("is longer than 1024 bytes");
    }
    if key.chars().any(char::is_control) {
        return Some("must not contain control characters");
    }
    if key.contains('\\') {
        return Some("must not contain a backslash");
    }
    if key.starts_with('/') {
        return Some("must not start with '/'");
    }
    if key
        .split('/')
        .any(|segment| matches!(segment, "" | "." | ".."))
    {
        return Some("must not contain empty, '.' or '..' segments");
    }
    None
}

impl fmt::Display for ObjectKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl fmt::Debug for ObjectKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("ObjectKey").field(&self.0).finish()
    }
}

impl AsRef<str> for ObjectKey {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for ObjectKey {
    type Error = AppError;

    fn try_from(key: String) -> Result<Self, AppError> {
        Self::new(key)
    }
}

impl TryFrom<&str> for ObjectKey {
    type Error = AppError;

    fn try_from(key: &str) -> Result<Self, AppError> {
        Self::new(key)
    }
}

impl std::str::FromStr for ObjectKey {
    type Err = AppError;

    fn from_str(key: &str) -> Result<Self, AppError> {
        Self::new(key)
    }
}

#[cfg(test)]
mod tests {
    use sekvent_error::ErrorCode;

    use super::*;

    #[test]
    fn plain_keys_are_accepted() {
        for key in [
            "a",
            "attachments/42/0190b6b2-7c1e-7d4f-9a51-1b2c3d4e5f60",
            "files/report v2 (final).pdf",
            "unicode/árvíztűrő.txt",
            "dots/.hidden/a..b",
        ] {
            let parsed = ObjectKey::new(key).unwrap();
            assert_eq!(parsed.as_str(), key);
            assert_eq!(parsed.to_string(), key);
            assert_eq!(parsed.as_ref(), key);
        }
        let max = "k".repeat(MAX_KEY_LEN);
        assert_eq!(ObjectKey::new(max.clone()).unwrap().into_string(), max);
    }

    #[test]
    fn unsafe_keys_are_rejected_without_quoting_them() {
        for (key, message) in [
            ("", "must not be empty"),
            ("/abs", "must not start with '/'"),
            ("a//b", "empty, '.' or '..' segments"),
            ("a/", "empty, '.' or '..' segments"),
            ("a/./b", "empty, '.' or '..' segments"),
            ("../etc/passwd", "empty, '.' or '..' segments"),
            ("a/..", "empty, '.' or '..' segments"),
            ("tab\there", "control characters"),
            ("nul\0", "control characters"),
            ("c1\u{85}", "control characters"),
            ("win\\path", "backslash"),
        ] {
            let error = ObjectKey::new(key).unwrap_err();
            assert_eq!(error.code(), ErrorCode::InvalidArgument, "{key:?}");
            assert_eq!(error.reason(), Some(reasons::INVALID_OBJECT_KEY));
            assert!(
                error.message().contains(message),
                "{key:?}: {}",
                error.message()
            );
        }
        let long = "k".repeat(MAX_KEY_LEN + 1);
        let error = ObjectKey::new(long).unwrap_err();
        assert_eq!(error.message(), "the object key is longer than 1024 bytes");
        let secretish = "../token-123";
        assert!(
            !ObjectKey::new(secretish)
                .unwrap_err()
                .message()
                .contains("token")
        );
    }

    #[test]
    fn conversions_validate_too() {
        assert!(ObjectKey::try_from("a/b").is_ok());
        assert!(ObjectKey::try_from(String::from("a/../b")).is_err());
        assert!("x".parse::<ObjectKey>().is_ok());
        assert!("".parse::<ObjectKey>().is_err());
        assert_eq!(
            format!("{:?}", ObjectKey::new("a/b").unwrap()),
            r#"ObjectKey("a/b")"#
        );
    }
}
