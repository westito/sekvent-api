use std::fmt;

use zeroize::Zeroizing;

/// A value that must never reach a log line: tokens, passwords, key
/// material, database URLs with credentials.
///
/// `Debug` and `Display` print `[redacted]`; the memory is zeroed on drop.
/// The value is reachable only through [`Secret::expose`], and every call
/// site is a review point. Redaction does not authorize logging the value
/// anywhere else.
#[derive(Clone, Default)]
pub struct Secret(Zeroizing<String>);

impl Secret {
    /// Wrap a value.
    pub fn new(value: impl Into<String>) -> Self {
        Self(Zeroizing::new(value.into()))
    }

    /// The raw value. Never log the result.
    pub fn expose(&self) -> &str {
        &self.0
    }

    /// Whether the value is empty or whitespace-only.
    pub fn is_blank(&self) -> bool {
        self.0.trim().is_empty()
    }
}

impl From<String> for Secret {
    fn from(value: String) -> Self {
        Self::new(value)
    }
}

impl From<&str> for Secret {
    fn from(value: &str) -> Self {
        Self::new(value)
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("[redacted]")
    }
}

impl fmt::Display for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("[redacted]")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacted_formatting() {
        let secret = Secret::new("hunter2");
        assert_eq!(format!("{secret}"), "[redacted]");
        assert_eq!(format!("{secret:?}"), "[redacted]");
        assert_eq!(secret.expose(), "hunter2");
    }

    #[test]
    fn conversions_and_blankness() {
        assert!(Secret::default().is_blank());
        assert!(Secret::from(" \n").is_blank());
        assert!(!Secret::from(String::from("x")).is_blank());
        let cloned = Secret::from("v").clone();
        assert_eq!(cloned.expose(), "v");
    }
}
