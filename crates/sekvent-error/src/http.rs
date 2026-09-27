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

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
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
/// When the body names a code this version does not know, the code is
/// derived from the HTTP `status` instead.
pub fn from_json_body(status: u16, body: &[u8]) -> Option<AppError> {
    let Envelope { mut error } = serde_json::from_slice(body).ok()?;
    if ErrorCode::parse(&error.code).is_none() {
        code_for_http_status(status)
            .as_str()
            .clone_into(&mut error.code);
    }
    Some(AppError::from_wire(error))
}

/// Whole seconds, rounded up.
fn retry_after_seconds(after: Duration) -> u64 {
    after
        .as_secs()
        .saturating_add(u64::from(after.subsec_nanos() > 0))
}

/// The inverse of [`ErrorCode::http_status`] where it is unambiguous.
fn code_for_http_status(status: u16) -> ErrorCode {
    match status {
        200..=299 => ErrorCode::Ok,
        400 => ErrorCode::InvalidArgument,
        401 => ErrorCode::Unauthenticated,
        403 => ErrorCode::PermissionDenied,
        404 => ErrorCode::NotFound,
        409 => ErrorCode::Aborted,
        429 => ErrorCode::ResourceExhausted,
        499 => ErrorCode::Cancelled,
        500 => ErrorCode::Internal,
        501 => ErrorCode::Unimplemented,
        503 => ErrorCode::Unavailable,
        504 => ErrorCode::DeadlineExceeded,
        _ => ErrorCode::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retry_after_rounds_up_to_whole_seconds() {
        assert_eq!(retry_after_seconds(Duration::ZERO), 0);
        assert_eq!(retry_after_seconds(Duration::from_secs(3)), 3);
        assert_eq!(retry_after_seconds(Duration::from_millis(1)), 1);
        assert_eq!(retry_after_seconds(Duration::from_millis(2_001)), 3);
        assert_eq!(retry_after_seconds(Duration::MAX), u64::MAX);
    }

    #[test]
    fn every_mapped_status_inverts_its_code() {
        for code in ErrorCode::ALL {
            let derived = code_for_http_status(code.http_status());
            assert_eq!(
                derived.http_status(),
                code.http_status(),
                "{code} and {derived} must share a status"
            );
        }
        assert_eq!(code_for_http_status(418), ErrorCode::Unknown);
    }

    #[test]
    fn an_unknown_code_name_falls_back_to_the_status() {
        let body = br#"{"error":{"code":"NOT_A_CODE","message":"gone"}}"#;
        let error = from_json_body(404, body).expect("well-formed envelope");
        assert_eq!(error.code(), ErrorCode::NotFound);
        assert_eq!(error.message(), "gone");
    }
}
