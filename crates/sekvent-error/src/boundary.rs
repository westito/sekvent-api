//! The server's own record of an error answered at a serving boundary.
//!
//! The caller sees only the wire form; the source chain of a server-side
//! failure would otherwise be lost. One `error` event, target
//! `sekvent::error`, carries the code, the reason and the rendered source
//! chain. The caller-visible message and the metadata are left out: the
//! message is already in the response and metadata may carry identifiers.

use std::error::Error as StdError;
use std::fmt::{self, Write as _};

use crate::{AppError, ErrorCode};

/// Hard cap on the rendered source chain, in bytes, marker included.
const SOURCE_LIMIT: usize = 2048;

/// Appended when the source chain was cut at [`SOURCE_LIMIT`].
const CUT_MARKER: &str = "…";

/// Logs `error` once if its code is a server-side failure (`UNKNOWN`,
/// `INTERNAL`, `DATA_LOSS`); other codes are caller errors and are not
/// logged.
///
/// `From<AppError> for tonic::Status` and `IntoResponse for AppError` call
/// this; a serving boundary that encodes with `grpc::to_status` or
/// [`AppError::to_wire`] directly calls it itself, exactly once per answer.
#[doc(hidden)]
pub fn log_server_side(error: &AppError) {
    let code = error.code();
    if !matches!(
        code,
        ErrorCode::Unknown | ErrorCode::Internal | ErrorCode::DataLoss
    ) {
        return;
    }
    let source = source_chain(error);
    tracing::error!(
        target: "sekvent::error",
        code = code.as_str(),
        reason = error.reason(),
        source = source.as_deref(),
        "request failed"
    );
}

/// The source chain below `error`, joined with `": "` and capped at
/// [`SOURCE_LIMIT`] bytes; `None` without a source.
fn source_chain(error: &AppError) -> Option<String> {
    let mut next = StdError::source(error)?;
    let mut out = Capped::new(SOURCE_LIMIT - CUT_MARKER.len());
    loop {
        if write!(out, "{next}").is_err() {
            break;
        }
        let Some(cause) = next.source() else {
            break;
        };
        if out.write_str(": ").is_err() {
            break;
        }
        next = cause;
    }
    Some(out.finish())
}

/// A string writer that refuses to grow past `limit` bytes, cutting on a
/// char boundary, so an enormous `Display` never materialises in full.
struct Capped {
    text: String,
    limit: usize,
    cut: bool,
}

impl Capped {
    fn new(limit: usize) -> Self {
        Self {
            text: String::new(),
            limit,
            cut: false,
        }
    }

    fn finish(mut self) -> String {
        if self.cut {
            self.text.push_str(CUT_MARKER);
        }
        self.text
    }
}

impl fmt::Write for Capped {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        let room = self.limit - self.text.len();
        if s.len() <= room {
            self.text.push_str(s);
            return Ok(());
        }
        self.text.push_str(&s[..s.floor_char_boundary(room)]);
        self.cut = true;
        Err(fmt::Error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{capture, field};

    #[derive(Debug)]
    struct Layer {
        text: String,
        below: Option<Box<Layer>>,
    }

    impl fmt::Display for Layer {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str(&self.text)
        }
    }

    impl StdError for Layer {
        fn source(&self) -> Option<&(dyn StdError + 'static)> {
            self.below
                .as_deref()
                .map(|below| below as &(dyn StdError + 'static))
        }
    }

    fn nested() -> Layer {
        Layer {
            text: "query failed".into(),
            below: Some(Box::new(Layer {
                text: "connection reset".into(),
                below: None,
            })),
        }
    }

    #[test]
    fn an_internal_error_logs_its_source_chain_once() {
        let error = AppError::internal(nested()).with_reason("DB_FAILURE");
        let events = capture(|| log_server_side(&error));
        assert_eq!(events.len(), 1, "{events:?}");
        let event = &events[0];
        assert_eq!(event.level, tracing::Level::ERROR);
        assert_eq!(field(event, "message"), Some("request failed"));
        assert_eq!(field(event, "code"), Some("INTERNAL"));
        assert_eq!(field(event, "reason"), Some("DB_FAILURE"));
        assert_eq!(
            field(event, "source"),
            Some("query failed: connection reset")
        );
    }

    #[test]
    fn unknown_and_data_loss_are_logged_without_a_source() {
        for code in [ErrorCode::Unknown, ErrorCode::DataLoss] {
            let error = AppError::new(code, "visible to the caller");
            let events = capture(|| log_server_side(&error));
            assert_eq!(events.len(), 1, "{code}: {events:?}");
            assert_eq!(field(&events[0], "code"), Some(code.as_str()));
            assert_eq!(field(&events[0], "source"), None);
            assert_eq!(field(&events[0], "reason"), None);
        }
    }

    #[test]
    fn the_message_and_metadata_are_never_logged() {
        let error = AppError::new(ErrorCode::Internal, "visible message")
            .with_metadata("order_id", "order-4711")
            .with_source(std::io::Error::other("disk full"));
        let events = capture(|| log_server_side(&error));
        assert_eq!(events.len(), 1, "{events:?}");
        let rendered = format!("{:?}", events[0].fields);
        assert!(!rendered.contains("visible message"), "{rendered}");
        assert!(!rendered.contains("order-4711"), "{rendered}");
        assert_eq!(field(&events[0], "source"), Some("disk full"));
    }

    #[test]
    fn caller_errors_are_not_logged() {
        for error in [
            AppError::not_found("no such order"),
            AppError::invalid_argument("bad").with_source(std::io::Error::other("detail")),
            AppError::unavailable("busy"),
            AppError::permission_denied("no"),
        ] {
            let events = capture(|| log_server_side(&error));
            assert!(events.is_empty(), "{}: {events:?}", error.code());
        }
    }

    #[test]
    fn a_long_source_is_capped_on_a_char_boundary() {
        let error = AppError::internal("é".repeat(4_000));
        let events = capture(|| log_server_side(&error));
        let source = field(&events[0], "source").expect("source");
        assert!(source.len() <= SOURCE_LIMIT, "{}", source.len());
        assert!(source.len() > SOURCE_LIMIT - 8, "{}", source.len());
        assert!(source.ends_with(CUT_MARKER));
        assert!(
            source
                .trim_end_matches(CUT_MARKER)
                .chars()
                .all(|c| c == 'é')
        );
    }

    #[test]
    fn a_long_chain_stops_at_the_cap() {
        let error = AppError::internal(Layer {
            text: "x".repeat(SOURCE_LIMIT - CUT_MARKER.len() - 1),
            below: Some(Box::new(Layer {
                text: "never rendered".into(),
                below: None,
            })),
        });
        let chain = source_chain(&error).expect("source");
        assert_eq!(chain.len(), SOURCE_LIMIT);
        assert!(chain.ends_with(&format!("x:{CUT_MARKER}")), "{chain}");
        assert!(!chain.contains("never"));
    }

    #[test]
    fn a_chain_exactly_at_the_cap_is_not_marked() {
        let exact = "y".repeat(SOURCE_LIMIT - CUT_MARKER.len());
        let error = AppError::internal(exact.clone());
        assert_eq!(source_chain(&error), Some(exact));
    }
}
