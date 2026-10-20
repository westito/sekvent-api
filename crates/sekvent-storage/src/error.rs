//! Error reasons and the mapping of object-store responses to [`AppError`].

use std::fmt;
use std::time::SystemTime;

use reqwest::StatusCode;
use reqwest::header::HeaderMap;
use sekvent_error::{AppError, ErrorCode};

/// Machine-readable reasons set on the errors of this crate.
pub mod reasons {
    /// The object key failed validation (`INVALID_ARGUMENT`).
    pub const INVALID_OBJECT_KEY: &str = "INVALID_OBJECT_KEY";
    /// A presign request has a bad content type, expiry or file name
    /// (`INVALID_ARGUMENT`).
    pub const INVALID_PRESIGN_REQUEST: &str = "INVALID_PRESIGN_REQUEST";
    /// The object a copy reads from does not exist (`NOT_FOUND`).
    pub const OBJECT_NOT_FOUND: &str = "OBJECT_NOT_FOUND";
    /// The store refused the service itself: credentials, permissions,
    /// region, endpoint or a missing bucket (`INTERNAL`). This is a
    /// configuration problem, never the end user's.
    pub const STORAGE_REJECTED: &str = "STORAGE_REJECTED";
    /// The store answered with another error status; the code follows the
    /// status (`UNAVAILABLE` for `502`/`503`, …).
    pub const STORAGE_HTTP_ERROR: &str = "STORAGE_HTTP_ERROR";
}

/// Longest S3 error code kept in an error's internal source.
const S3_CODE_LIMIT: usize = 64;

/// The shape of a failed response for the internal source chain: status
/// and the S3 error code, never the body.
#[derive(Debug)]
pub(crate) struct UpstreamFailure {
    status: u16,
    s3_code: Option<String>,
}

impl fmt::Display for UpstreamFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "object store HTTP {}", self.status)?;
        if let Some(code) = &self.s3_code {
            write!(f, " ({code})")?;
        }
        Ok(())
    }
}

impl std::error::Error for UpstreamFailure {}

/// The `<Code>` of an S3 XML error body, when it is a short ASCII
/// identifier. Anything else is dropped rather than quoted.
pub(crate) fn s3_error_code(body: &[u8]) -> Option<String> {
    let text = std::str::from_utf8(body).ok()?;
    let start = text.find("<Code>")? + "<Code>".len();
    let len = text[start..].find("</Code>")?;
    let code = &text[start..start + len];
    let plain = !code.is_empty()
        && code.len() <= S3_CODE_LIMIT
        && code.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'.');
    plain.then(|| code.to_owned())
}

/// The error for a response the operation does not accept.
pub(crate) fn status_error(
    status: StatusCode,
    headers: &HeaderMap,
    body: &[u8],
    now: SystemTime,
) -> AppError {
    let s3_code = s3_error_code(body);
    let rejected = status.is_redirection()
        || matches!(status.as_u16(), 400 | 401 | 403)
        || s3_code.as_deref() == Some("NoSuchBucket");
    let (code, reason) = if rejected {
        (ErrorCode::Internal, reasons::STORAGE_REJECTED)
    } else {
        let code = match sekvent_client::code_for_status(status) {
            ErrorCode::Ok => ErrorCode::Unknown,
            code => code,
        };
        (code, reasons::STORAGE_HTTP_ERROR)
    };
    let mut error = AppError::new(
        code,
        format!("the object store responded with HTTP {}", status.as_u16()),
    )
    .with_reason(reason)
    .with_metadata("upstream_status", status.as_u16().to_string())
    .with_source(UpstreamFailure {
        status: status.as_u16(),
        s3_code,
    });
    if let Some(after) = headers
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| sekvent_client::parse_retry_after(value, now))
    {
        error = error.with_retry_after(after);
    }
    error
}

/// The error for a transport failure; the URL is stripped from the source.
pub(crate) fn transport_error(error: reqwest::Error) -> AppError {
    let error = error.without_url();
    let mapped = if error.is_timeout() {
        AppError::deadline_exceeded("the object store request timed out")
    } else if error.is_connect() {
        AppError::unavailable("could not connect to the object store")
    } else if error.is_builder() {
        AppError::new(
            ErrorCode::Internal,
            "the object store request could not be built",
        )
    } else {
        AppError::unavailable("the object store connection failed")
    };
    mapped.with_source(error)
}

/// An `INVALID_ARGUMENT` for a bad presign request.
pub(crate) fn invalid_presign(message: impl Into<String>) -> AppError {
    AppError::invalid_argument(message).with_reason(reasons::INVALID_PRESIGN_REQUEST)
}

#[cfg(test)]
mod tests {
    use std::error::Error as _;
    use std::time::Duration;

    use super::*;

    fn error_for(status: u16, body: &str) -> AppError {
        status_error(
            StatusCode::from_u16(status).unwrap(),
            &HeaderMap::new(),
            body.as_bytes(),
            SystemTime::UNIX_EPOCH,
        )
    }

    #[test]
    fn s3_codes_are_read_only_when_plain() {
        let body =
            "<?xml version=\"1.0\"?><Error><Code>NoSuchKey</Code><Message>gone</Message></Error>";
        assert_eq!(s3_error_code(body.as_bytes()).as_deref(), Some("NoSuchKey"));
        assert_eq!(s3_error_code(b"<Code>has space</Code>"), None);
        assert_eq!(s3_error_code(b"<Code></Code>"), None);
        assert_eq!(s3_error_code(b"<Code>unterminated"), None);
        assert_eq!(s3_error_code(b"no code"), None);
        assert_eq!(s3_error_code(&[0xff, 0xfe]), None);
        let long = format!("<Code>{}</Code>", "A".repeat(65));
        assert_eq!(s3_error_code(long.as_bytes()), None);
    }

    #[test]
    fn refusals_of_the_service_are_internal() {
        for status in [301, 307, 400, 401, 403] {
            let error = error_for(status, "");
            assert_eq!(error.code(), ErrorCode::Internal, "{status}");
            assert_eq!(error.reason(), Some(reasons::STORAGE_REJECTED));
        }
        let error = error_for(404, "<Error><Code>NoSuchBucket</Code></Error>");
        assert_eq!(error.code(), ErrorCode::Internal);
        assert_eq!(error.reason(), Some(reasons::STORAGE_REJECTED));
    }

    #[test]
    fn other_statuses_follow_the_status_table() {
        let error = error_for(503, "<Error><Code>SlowDown</Code></Error>");
        assert_eq!(error.code(), ErrorCode::Unavailable);
        assert_eq!(error.reason(), Some(reasons::STORAGE_HTTP_ERROR));
        assert_eq!(error.message(), "the object store responded with HTTP 503");
        assert_eq!(error.metadata()["upstream_status"], "503");
        assert_eq!(
            error.source().unwrap().to_string(),
            "object store HTTP 503 (SlowDown)"
        );
        assert_eq!(error_for(500, "").code(), ErrorCode::Internal);
        assert_eq!(
            error_for(500, "").source().unwrap().to_string(),
            "object store HTTP 500"
        );
        assert_eq!(error_for(404, "").code(), ErrorCode::NotFound);
        assert_eq!(error_for(200, "").code(), ErrorCode::Unknown);
    }

    #[test]
    fn retry_after_is_honoured() {
        let mut headers = HeaderMap::new();
        headers.insert(reqwest::header::RETRY_AFTER, "3".parse().unwrap());
        let error = status_error(
            StatusCode::SERVICE_UNAVAILABLE,
            &headers,
            b"",
            SystemTime::UNIX_EPOCH,
        );
        assert_eq!(error.retry_after(), Some(Duration::from_secs(3)));
    }

    #[test]
    fn messages_never_quote_the_body() {
        let error = error_for(
            403,
            "<Error><Code>AccessDenied</Code><Message>secret-ish detail</Message></Error>",
        );
        let shown = format!("{error:?} {error}");
        assert!(!shown.contains("secret-ish"));
    }
}
