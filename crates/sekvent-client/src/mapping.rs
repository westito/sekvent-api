use std::fmt;
use std::time::{Duration, SystemTime};

use http::{HeaderMap, StatusCode, header::RETRY_AFTER};
use sekvent_error::{AppError, ErrorCode};

/// Longest upstream body excerpt kept in an error's internal source.
const BODY_EXCERPT: usize = 512;

/// The [`ErrorCode`] for an upstream HTTP status.
///
/// `408`, `429`, `502`, `503` and `504` map to transient codes; other
/// `4xx` statuses map to the matching caller-error codes.
pub fn code_for_status(status: StatusCode) -> ErrorCode {
    match status.as_u16() {
        200..=399 => ErrorCode::Ok,
        400 | 413 | 414 | 415 | 422 | 431 => ErrorCode::InvalidArgument,
        401 => ErrorCode::Unauthenticated,
        403 => ErrorCode::PermissionDenied,
        404 | 410 => ErrorCode::NotFound,
        405 | 501 => ErrorCode::Unimplemented,
        408 | 504 => ErrorCode::DeadlineExceeded,
        409 => ErrorCode::Aborted,
        416 => ErrorCode::OutOfRange,
        429 => ErrorCode::ResourceExhausted,
        499 => ErrorCode::Cancelled,
        502 | 503 => ErrorCode::Unavailable,
        400..=499 => ErrorCode::FailedPrecondition,
        500..=599 => ErrorCode::Internal,
        _ => ErrorCode::Unknown,
    }
}

/// Parse a `Retry-After` value: delay seconds or an HTTP-date, measured
/// from `now`. A date in the past gives zero; garbage gives `None`.
pub fn parse_retry_after(value: &str, now: SystemTime) -> Option<Duration> {
    let value = value.trim();
    if let Ok(secs) = value.parse::<u64>() {
        return Some(Duration::from_secs(secs));
    }
    let at = httpdate::parse_http_date(value).ok()?;
    Some(at.duration_since(now).unwrap_or(Duration::ZERO))
}

fn retry_after_header(headers: &HeaderMap, now: SystemTime) -> Option<Duration> {
    headers
        .get(RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| parse_retry_after(value, now))
}

/// The upstream response body, kept for the server's logs only.
#[derive(Debug)]
pub(crate) struct UpstreamBody {
    status: u16,
    excerpt: String,
}

impl UpstreamBody {
    pub(crate) fn new(status: StatusCode, body: &[u8]) -> Self {
        let text = String::from_utf8_lossy(body);
        Self {
            status: status.as_u16(),
            excerpt: sekvent_telemetry::truncate_for_log(&text, BODY_EXCERPT).into_owned(),
        }
    }
}

impl fmt::Display for UpstreamBody {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "upstream HTTP {} body: {}", self.status, self.excerpt)
    }
}

impl std::error::Error for UpstreamBody {}

/// Map a non-success response to an [`AppError`].
///
/// A sekvent JSON error body is decoded as is. Otherwise the caller-visible
/// message only names the status, and the body goes to the source chain,
/// truncated.
pub(crate) fn map_status(
    status: StatusCode,
    headers: &HeaderMap,
    body: &[u8],
    now: SystemTime,
) -> AppError {
    let retry_after = retry_after_header(headers, now);
    if let Some(error) = sekvent_error::http::from_json_body(status.as_u16(), body) {
        return match retry_after {
            Some(after) if error.retry_after().is_none() => error.with_retry_after(after),
            _ => error,
        };
    }
    let code = match code_for_status(status) {
        ErrorCode::Ok => ErrorCode::Unknown,
        code => code,
    };
    let error = AppError::new(
        code,
        format!("upstream responded with HTTP {}", status.as_u16()),
    )
    .with_reason("UPSTREAM_HTTP_ERROR")
    .with_metadata("upstream_status", status.as_u16().to_string())
    .with_source(UpstreamBody::new(status, body));
    match retry_after {
        Some(after) => error.with_retry_after(after),
        None => error,
    }
}

/// Map a transport failure. The URL (which may carry a query) is stripped
/// from the source before it is attached.
pub(crate) fn map_transport_error(error: reqwest::Error) -> AppError {
    let error = error.without_url();
    let mapped = if error.is_timeout() {
        AppError::deadline_exceeded("the upstream request timed out")
    } else if error.is_connect() {
        AppError::unavailable("could not connect to the upstream")
    } else if error.is_builder() || error.is_decode() {
        AppError::new(
            ErrorCode::Internal,
            "the upstream request could not be processed",
        )
    } else {
        AppError::unavailable("the upstream connection failed")
    };
    mapped.with_source(error)
}

/// A JSON decoding failure described by position only, so no fragment of
/// the payload reaches a log line.
#[derive(Debug)]
pub(crate) struct JsonShape {
    category: &'static str,
    line: usize,
    column: usize,
}

impl JsonShape {
    pub(crate) fn new(error: &serde_json::Error) -> Self {
        let category = match error.classify() {
            serde_json::error::Category::Io => "io",
            serde_json::error::Category::Syntax => "syntax",
            serde_json::error::Category::Data => "data",
            serde_json::error::Category::Eof => "eof",
        };
        Self {
            category,
            line: error.line(),
            column: error.column(),
        }
    }
}

impl fmt::Display for JsonShape {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "JSON {} error at line {} column {}",
            self.category, self.line, self.column
        )
    }
}

impl std::error::Error for JsonShape {}

#[cfg(test)]
mod tests {
    use std::error::Error as _;

    use http::HeaderValue;

    use super::*;

    #[test]
    fn statuses_map_to_codes() {
        let table = [
            (200, ErrorCode::Ok),
            (304, ErrorCode::Ok),
            (400, ErrorCode::InvalidArgument),
            (401, ErrorCode::Unauthenticated),
            (403, ErrorCode::PermissionDenied),
            (404, ErrorCode::NotFound),
            (405, ErrorCode::Unimplemented),
            (408, ErrorCode::DeadlineExceeded),
            (409, ErrorCode::Aborted),
            (410, ErrorCode::NotFound),
            (412, ErrorCode::FailedPrecondition),
            (416, ErrorCode::OutOfRange),
            (422, ErrorCode::InvalidArgument),
            (429, ErrorCode::ResourceExhausted),
            (499, ErrorCode::Cancelled),
            (500, ErrorCode::Internal),
            (501, ErrorCode::Unimplemented),
            (502, ErrorCode::Unavailable),
            (503, ErrorCode::Unavailable),
            (504, ErrorCode::DeadlineExceeded),
            (599, ErrorCode::Internal),
            (100, ErrorCode::Unknown),
        ];
        for (status, code) in table {
            assert_eq!(
                code_for_status(StatusCode::from_u16(status).unwrap()),
                code,
                "{status}"
            );
        }
        for transient in [408, 429, 502, 503, 504] {
            assert!(code_for_status(StatusCode::from_u16(transient).unwrap()).is_transient());
        }
    }

    #[test]
    fn retry_after_accepts_seconds_and_dates() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        assert_eq!(
            parse_retry_after(" 120 ", now),
            Some(Duration::from_secs(120))
        );
        let later = httpdate::fmt_http_date(now + Duration::from_secs(90));
        assert_eq!(
            parse_retry_after(&later, now),
            Some(Duration::from_secs(90))
        );
        let earlier = httpdate::fmt_http_date(now - Duration::from_secs(90));
        assert_eq!(parse_retry_after(&earlier, now), Some(Duration::ZERO));
        assert_eq!(parse_retry_after("soon", now), None);
        assert_eq!(parse_retry_after("-5", now), None);
    }

    #[test]
    fn unknown_bodies_stay_out_of_the_message() {
        let now = SystemTime::UNIX_EPOCH;
        let mut headers = HeaderMap::new();
        headers.insert(RETRY_AFTER, HeaderValue::from_static("3"));
        let body = format!("secret-token={}", "x".repeat(4000));
        let error = map_status(
            StatusCode::SERVICE_UNAVAILABLE,
            &headers,
            body.as_bytes(),
            now,
        );
        assert_eq!(error.code(), ErrorCode::Unavailable);
        assert_eq!(error.message(), "upstream responded with HTTP 503");
        assert_eq!(error.retry_after(), Some(Duration::from_secs(3)));
        assert_eq!(
            error.metadata().get("upstream_status").map(String::as_str),
            Some("503")
        );
        assert!(!error.to_string().contains("secret-token"));
        assert!(!format!("{:?}", error.to_wire()).contains("secret-token"));
        let source = error.source().unwrap().to_string();
        assert!(source.contains("secret-token"));
        assert!(source.len() < 700, "the excerpt is truncated");

        let odd = map_status(
            StatusCode::from_u16(299).unwrap(),
            &HeaderMap::new(),
            b"",
            now,
        );
        assert_eq!(odd.code(), ErrorCode::Unknown);
    }

    #[test]
    fn json_shape_hides_the_payload() {
        let error = serde_json::from_str::<u32>("\"secret\"").unwrap_err();
        let shape = JsonShape::new(&error).to_string();
        assert!(shape.starts_with("JSON data error at line 1"));
        assert!(!shape.contains("secret"));
        let error = serde_json::from_str::<u32>("").unwrap_err();
        assert!(JsonShape::new(&error).to_string().starts_with("JSON eof"));
        let error = serde_json::from_str::<u32>("}").unwrap_err();
        assert!(
            JsonShape::new(&error)
                .to_string()
                .starts_with("JSON syntax")
        );
    }
}
