use std::fmt;

/// A policy was constructed with parameters that cannot work, e.g. a rate
/// gate with zero permits or a backoff multiplier below one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PolicyError {
    parameter: &'static str,
    reason: &'static str,
}

impl PolicyError {
    pub(crate) fn new(parameter: &'static str, reason: &'static str) -> Self {
        Self { parameter, reason }
    }

    /// The offending parameter, e.g. `rate_gate.permits`.
    pub fn parameter(&self) -> &'static str {
        self.parameter
    }

    /// Why the value was rejected.
    pub fn reason(&self) -> &'static str {
        self.reason
    }
}

impl fmt::Display for PolicyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "invalid resilience parameter {}: {}",
            self.parameter, self.reason
        )
    }
}

impl std::error::Error for PolicyError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_names_the_parameter() {
        let error = PolicyError::new("rate_gate.permits", "must be at least 1");
        assert_eq!(error.parameter(), "rate_gate.permits");
        assert_eq!(error.reason(), "must be at least 1");
        assert_eq!(
            error.to_string(),
            "invalid resilience parameter rate_gate.permits: must be at least 1"
        );
    }
}
