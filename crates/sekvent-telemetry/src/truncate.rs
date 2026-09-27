use std::borrow::Cow;

/// Shorten `s` to at most `max_chars` characters for logging.
///
/// The cut always falls on a character boundary, and a cut string ends with
/// `…(+N chars)` naming how many characters were dropped. Text that already
/// fits is returned borrowed. Use it for upstream bodies and other untrusted
/// text of unbounded length; it does not make secrets safe to log.
pub fn truncate_for_log(s: &str, max_chars: usize) -> Cow<'_, str> {
    match s.char_indices().nth(max_chars) {
        None => Cow::Borrowed(s),
        Some((cut, _)) => {
            let dropped = s[cut..].chars().count();
            Cow::Owned(format!("{}…(+{dropped} chars)", &s[..cut]))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_text_is_borrowed() {
        assert!(matches!(truncate_for_log("abc", 3), Cow::Borrowed("abc")));
        assert!(matches!(truncate_for_log("", 0), Cow::Borrowed("")));
    }

    #[test]
    fn long_text_is_cut_and_annotated() {
        assert_eq!(truncate_for_log("abcdef", 4), "abcd…(+2 chars)");
        assert_eq!(truncate_for_log("abc", 0), "…(+3 chars)");
    }

    #[test]
    fn cuts_on_char_boundaries() {
        assert_eq!(truncate_for_log("héllo wörld", 7), "héllo w…(+4 chars)");
        assert_eq!(truncate_for_log("🦀🦀🦀", 1), "🦀…(+2 chars)");
    }
}
