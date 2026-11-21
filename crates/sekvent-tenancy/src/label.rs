//! Tenant labels: the first DNS label of a tenant's host, or a path segment
//! that stands for it.

use std::sync::Arc;

use sekvent_error::AppError;

use crate::host::{canonical_host, is_ip_address, is_loopback_host};

/// The longest DNS label.
const LABEL_MAX: usize = 63;

/// The product's check for reserved labels.
type Reserved = Arc<dyn Fn(&str) -> bool + Send + Sync>;

/// How a product's tenant labels look.
#[derive(Clone)]
pub struct LabelRules {
    /// The shortest label, at least 1.
    min: usize,
    /// The longest label, at most 63.
    max: usize,
    /// The product's reserved labels, asked with the lowered label.
    reserved: Option<Reserved>,
}

impl LabelRules {
    /// A DNS label: 1 to 63 characters, nothing reserved.
    pub fn new() -> Self {
        Self {
            min: 1,
            max: LABEL_MAX,
            reserved: None,
        }
    }

    /// `min <= max <= 63`, `min >= 1`. `INVALID_ARGUMENT` otherwise.
    pub fn with_length(self, min: usize, max: usize) -> Result<Self, AppError> {
        if min < 1 || min > max || max > LABEL_MAX {
            return Err(AppError::invalid_argument(
                "label lengths need 1 <= min <= max <= 63",
            ));
        }
        Ok(Self { min, max, ..self })
    }

    /// Labels the product keeps for itself; called with the lowered label.
    #[must_use]
    pub fn with_reserved(self, reserved: impl Fn(&str) -> bool + Send + Sync + 'static) -> Self {
        Self {
            reserved: Some(Arc::new(reserved)),
            ..self
        }
    }

    /// The lowered label, or why it is refused.
    ///
    /// The first failure is reported, checked in this order: characters,
    /// length in bytes, a hyphen at either end, `--` at positions 3 and 4,
    /// the reserved labels.
    pub fn canonical(&self, input: &str) -> Result<String, LabelError> {
        if !input
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        {
            return Err(LabelError::Characters);
        }
        if !(self.min..=self.max).contains(&input.len()) {
            return Err(LabelError::Length);
        }
        let label = input.to_ascii_lowercase();
        if label.starts_with('-') || label.ends_with('-') {
            return Err(LabelError::Hyphen);
        }
        if label.get(2..4) == Some("--") {
            return Err(LabelError::Punycode);
        }
        if self
            .reserved
            .as_ref()
            .is_some_and(|reserved| reserved(label.as_str()))
        {
            return Err(LabelError::Reserved);
        }
        Ok(label)
    }
}

impl Default for LabelRules {
    fn default() -> Self {
        Self::new()
    }
}

/// Why a label was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum LabelError {
    /// Shorter or longer than the rules allow.
    Length,
    /// A character other than an ASCII letter, digit or hyphen.
    Characters,
    /// A hyphen at either end.
    Hyphen,
    /// `--` at positions 3 and 4.
    Punycode,
    /// The product reserves it.
    Reserved,
}

/// `<label>.<base>`; no validation.
pub fn label_host(label: &str, base: &str) -> String {
    format!("{label}.{base}")
}

/// The host a path label stands for: the listener host with its first
/// label replaced by `segment`. `None` on loopback, for a malformed segment
/// or a listener host of one label.
///
/// The segment must be a DNS label ([`LabelRules::new`]); it is lowered.
/// The listener host is canonicalised first and must be a host name: an IP
/// address listener gives `None`, so the result is never an IP address. The
/// result must be a valid host itself (at most 253 bytes).
pub fn path_label_host(segment: &str, listener_host: &str) -> Option<String> {
    let listener = canonical_host(listener_host).ok()?;
    if is_loopback_host(&listener) || is_ip_address(&listener) {
        return None;
    }
    let label = LabelRules::new().canonical(segment).ok()?;
    let (_, rest) = listener.split_once('.')?;
    let host = label_host(&label, rest);
    canonical_host(&host).ok()
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use sekvent_error::ErrorCode;

    use super::*;

    #[test]
    fn new_allows_any_dns_label() {
        let rules = LabelRules::new();
        let longest = "a".repeat(LABEL_MAX);
        for (input, expected) in [
            ("a", "a"),
            ("Billing", "billing"),
            ("ORDERS-2", "orders-2"),
            ("9lives", "9lives"),
            ("a-b-c", "a-b-c"),
            ("ab-cd", "ab-cd"),
            ("abc--d", "abc--d"),
            (longest.as_str(), longest.as_str()),
        ] {
            assert_eq!(rules.canonical(input).as_deref(), Ok(expected), "{input:?}");
        }
        let default = LabelRules::default();
        assert_eq!(default.canonical("Inventory").as_deref(), Ok("inventory"));
    }

    #[test]
    fn length_bounds() {
        let rules = LabelRules::new();
        assert_eq!(rules.canonical(""), Err(LabelError::Length));
        assert_eq!(
            rules.canonical(&"a".repeat(LABEL_MAX + 1)),
            Err(LabelError::Length)
        );

        let rules = LabelRules::new().with_length(3, 5).unwrap();
        assert_eq!(rules.canonical("ab"), Err(LabelError::Length));
        assert_eq!(rules.canonical("abc").as_deref(), Ok("abc"));
        assert_eq!(rules.canonical("abcde").as_deref(), Ok("abcde"));
        assert_eq!(rules.canonical("abcdef"), Err(LabelError::Length));

        let exact = LabelRules::new().with_length(LABEL_MAX, LABEL_MAX).unwrap();
        assert!(exact.canonical(&"a".repeat(LABEL_MAX)).is_ok());
        let one = LabelRules::new().with_length(1, 1).unwrap();
        assert_eq!(one.canonical("ab"), Err(LabelError::Length));
    }

    #[test]
    fn with_length_refuses_impossible_bounds() {
        for (min, max) in [
            (0, 10),
            (0, 0),
            (5, 4),
            (1, LABEL_MAX + 1),
            (LABEL_MAX + 1, LABEL_MAX + 1),
        ] {
            let Err(error) = LabelRules::new().with_length(min, max) else {
                panic!("{min}..{max} was accepted");
            };
            assert_eq!(error.code(), ErrorCode::InvalidArgument, "{min}..{max}");
        }
    }

    #[test]
    fn characters_are_letters_digits_and_hyphens() {
        let rules = LabelRules::new();
        for input in [
            "bill ing",
            "bill_ing",
            "bill.ing",
            "b\u{fc}cher",
            "bill\'ing",
            "-",
            "a/b",
        ] {
            let expected = if input == "-" {
                LabelError::Hyphen
            } else {
                LabelError::Characters
            };
            assert_eq!(rules.canonical(input), Err(expected), "{input:?}");
        }
    }

    #[test]
    fn hyphen_at_either_end() {
        let rules = LabelRules::new();
        for input in ["-billing", "billing-", "-", "--", "-a-"] {
            assert_eq!(rules.canonical(input), Err(LabelError::Hyphen), "{input:?}");
        }
    }

    #[test]
    fn double_hyphen_at_positions_three_and_four() {
        let rules = LabelRules::new();
        for input in ["xn--bcher-kva", "XN--abc", "ab--", "ab--c", "a1--b"] {
            let expected = if input == "ab--" {
                LabelError::Hyphen
            } else {
                LabelError::Punycode
            };
            assert_eq!(rules.canonical(input), Err(expected), "{input:?}");
        }
        for input in ["a--b", "abc--d", "a-b-c"] {
            assert!(rules.canonical(input).is_ok(), "{input:?}");
        }
    }

    #[test]
    fn the_first_failure_wins() {
        let rules = LabelRules::new()
            .with_length(3, 5)
            .unwrap()
            .with_reserved(|_| true);
        // Characters before length.
        assert_eq!(rules.canonical("a_bcdefgh"), Err(LabelError::Characters));
        // Length before hyphen.
        assert_eq!(rules.canonical("-a"), Err(LabelError::Length));
        // Hyphen before punycode.
        assert_eq!(rules.canonical("-b--c"), Err(LabelError::Hyphen));
        // Punycode before reservation.
        assert_eq!(rules.canonical("ab--c"), Err(LabelError::Punycode));
        assert_eq!(rules.canonical("abc"), Err(LabelError::Reserved));
    }

    #[test]
    fn the_reserved_function_receives_the_lowered_label() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let record = Arc::clone(&seen);
        let rules = LabelRules::new().with_reserved(move |label| {
            record.lock().unwrap().push(label.to_owned());
            label == "admin"
        });
        assert_eq!(rules.canonical("ADMIN"), Err(LabelError::Reserved));
        assert_eq!(rules.canonical("Billing").as_deref(), Ok("billing"));
        assert_eq!(rules.canonical("bad_label"), Err(LabelError::Characters));
        assert_eq!(*seen.lock().unwrap(), ["admin", "billing"]);

        let cloned = rules.clone();
        assert_eq!(cloned.canonical("Admin"), Err(LabelError::Reserved));
        let widened = rules.with_length(2, 10).unwrap();
        assert_eq!(widened.canonical("admin"), Err(LabelError::Reserved));
    }

    #[test]
    fn label_host_joins_without_validation() {
        assert_eq!(label_host("billing", "example.com"), "billing.example.com");
        assert_eq!(label_host("", ""), ".");
    }

    #[test]
    fn path_label_host_replaces_the_first_label() {
        for (segment, listener, expected) in [
            ("billing", "app.example.com", "billing.example.com"),
            ("Orders", "App.Example.com.", "orders.example.com"),
            ("inventory", "app.example.com:8443", "inventory.example.com"),
            ("a1", "x.y", "a1.y"),
        ] {
            assert_eq!(
                path_label_host(segment, listener).as_deref(),
                Some(expected),
                "{segment:?}"
            );
        }
    }

    #[test]
    fn a_numeric_segment_on_a_host_name_listener_is_a_host_name() {
        assert_eq!(
            path_label_host("10", "app.example.com").as_deref(),
            Some("10.example.com")
        );
    }

    #[test]
    fn path_label_host_refusals() {
        let long_tail = format!("{}.{}.{}", "b".repeat(63), "c".repeat(63), "d".repeat(63));
        let long_listener = format!("app.{long_tail}");
        let long_segment = "a".repeat(63);
        for (segment, listener) in [
            ("billing", "localhost"),
            ("billing", "app.localhost:5173"),
            ("billing", "127.0.0.1"),
            ("billing", "[::1]:8080"),
            ("billing", "example"),
            ("billing", "[2001:db8::1]"),
            ("billing", "10.0.0.1"),
            ("10", "192.0.2.1"),
            ("127", "192.0.2.1"),
            ("198", "203.0.113.7"),
            ("1", "192.0.2.1:8443"),
            ("2001", "[2001:db8::1]"),
            ("billing", "not a host"),
            ("", "app.example.com"),
            ("bill_ing", "app.example.com"),
            ("-billing", "app.example.com"),
            ("xn--bcher-kva", "app.example.com"),
            ("bill.ing", "app.example.com"),
            (long_segment.as_str(), long_listener.as_str()),
        ] {
            assert_eq!(
                path_label_host(segment, listener),
                None,
                "{segment:?} {listener:?}"
            );
        }
    }
}
