//! `AppError` as an HTTP response: status from [`crate::ErrorCode::http_status`],
//! JSON body `{"error": WireError}`, `Retry-After` header when set.
//!
//! `Retry-After` carries whole seconds rounded up, so a client that honours
//! it never retries before the hint; the body keeps the exact value in
//! milliseconds.

use std::time::Duration;

use axum::response::{IntoResponse, Response};
use http::header::{CONTENT_TYPE, RETRY_AFTER};
use http::{HeaderValue, StatusCode};

use crate::{AppError, ErrorCode, WireError};

/// The JSON envelope of an error body.
#[derive(serde::Serialize, serde::Deserialize)]
struct Envelope {
    error: WireError,
}

/// A body that cannot fail to serialize, used if the real one somehow does.
const FALLBACK_BODY: &str = r#"{"error":{"code":"INTERNAL","message":"internal error"}}"#;

/// The serving-boundary conversion: logs a server-side failure (`UNKNOWN`,
/// `INTERNAL`, `DATA_LOSS`) once with its source chain, target
/// `sekvent::error`, then answers with the caller-visible part only.
impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        crate::log_server_side(&self);
        let status = StatusCode::from_u16(self.code().http_status())
            .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        let body = serde_json::to_vec(&Envelope {
            error: self.to_wire(),
        })
        .unwrap_or_else(|_| FALLBACK_BODY.as_bytes().to_vec());
        let mut response = (status, body).into_response();
        let headers = response.headers_mut();
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        if let Some(after) = self.retry_after() {
            headers.insert(RETRY_AFTER, HeaderValue::from(retry_after_seconds(after)));
        }
        response
    }
}

/// Parse an HTTP error response body produced by this module back into an
/// `AppError`; `None` if the body is not in that shape.
///
/// Only a body this module could have produced is accepted: the code must
/// name a known error (never `OK`) whose conventional status
/// ([`ErrorCode::http_status`]) is `status`. Anything else — an unknown code,
/// `OK`, or a code that disagrees with the status — is treated as a foreign
/// body and yields `None`, so the caller maps the response by its status
/// instead of adopting a message it cannot vouch for.
pub fn from_json_body(status: u16, body: &[u8]) -> Option<AppError> {
    let Envelope { error } = serde_json::from_slice(body).ok()?;
    let code = ErrorCode::parse(&error.code)?;
    if code == ErrorCode::Ok || code.http_status() != status {
        return None;
    }
    Some(AppError::from_wire(error))
}

/// Whole seconds, rounded up.
fn retry_after_seconds(after: Duration) -> u64 {
    after
        .as_secs()
        .saturating_add(u64::from(after.subsec_nanos() > 0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{capture, field};

    #[test]
    fn into_response_logs_an_internal_error_once() {
        let error = AppError::internal(std::io::Error::other("relation does not exist"))
            .with_reason("DB_FAILURE");
        let mut status = None;
        let events = capture(|| status = Some(error.into_response().status()));
        assert_eq!(status, Some(StatusCode::INTERNAL_SERVER_ERROR));
        assert_eq!(events.len(), 1, "{events:?}");
        assert_eq!(field(&events[0], "code"), Some("INTERNAL"));
        assert_eq!(field(&events[0], "reason"), Some("DB_FAILURE"));
        assert_eq!(field(&events[0], "source"), Some("relation does not exist"));
    }

    #[test]
    fn into_response_does_not_log_caller_errors() {
        let events = capture(|| {
            drop(AppError::not_found("no such order").into_response());
            drop(AppError::invalid_argument("bad").into_response());
        });
        assert!(events.is_empty(), "{events:?}");
    }

    #[test]
    fn retry_after_rounds_up_to_whole_seconds() {
        assert_eq!(retry_after_seconds(Duration::ZERO), 0);
        assert_eq!(retry_after_seconds(Duration::from_secs(3)), 3);
        assert_eq!(retry_after_seconds(Duration::from_millis(1)), 1);
        assert_eq!(retry_after_seconds(Duration::from_millis(2_001)), 3);
        assert_eq!(retry_after_seconds(Duration::MAX), u64::MAX);
    }

    #[test]
    fn a_known_code_matching_the_status_is_adopted() {
        let body = br#"{"error":{"code":"NOT_FOUND","message":"gone"}}"#;
        let error = from_json_body(404, body).expect("well-formed envelope");
        assert_eq!(error.code(), ErrorCode::NotFound);
        assert_eq!(error.message(), "gone");
        let conflict = br#"{"error":{"code":"ALREADY_EXISTS","message":"dup"}}"#;
        assert_eq!(
            from_json_body(409, conflict).as_ref().map(AppError::code),
            Some(ErrorCode::AlreadyExists)
        );
    }

    #[test]
    fn foreign_or_inconsistent_envelopes_are_rejected() {
        for (status, body) in [
            (
                404,
                &br#"{"error":{"code":"NOT_A_CODE","message":"gone"}}"#[..],
            ),
            (500, br#"{"error":{"code":"OK","message":"fine"}}"#),
            (200, br#"{"error":{"code":"OK","message":"fine"}}"#),
            (500, br#"{"error":{"code":"NOT_FOUND","message":"gone"}}"#),
            (
                401,
                br#"{"error":{"code":"PERMISSION_DENIED","message":"no"}}"#,
            ),
        ] {
            assert!(
                from_json_body(status, body).is_none(),
                "{status} {}",
                String::from_utf8_lossy(body)
            );
        }
    }
}
