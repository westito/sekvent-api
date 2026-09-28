use std::fmt;
use std::time::{Duration, SystemTime};

use http::header::{CONTENT_TYPE, RETRY_AFTER};
use http::{HeaderMap, StatusCode};
use sekvent_error::{AppError, ErrorCode};

/// Longest content type kept in an error's internal source.
const CONTENT_TYPE_LIMIT: usize = 64;

/// The [`ErrorCode`] for an upstream HTTP status.
///
/// `408`, `429`, `502`, `503` and `504` map to transient codes; other
/// `4xx` statuses map to the matching caller-error codes, none of them
/// transient. In particular `409` maps to `ALREADY_EXISTS`: a conflict is
/// about the request, so repeating the same request is not retried.
///
/// This is the plain status table. [`HttpClient`](crate::HttpClient) refines
/// it for `401`/`403` unless the upstream is declared a sekvent service; see
/// [`HttpClientBuilder::sekvent_upstream`](crate::HttpClientBuilder::sekvent_upstream).
pub fn code_for_status(status: StatusCode) -> ErrorCode {
    match status.as_u16() {
        200..=399 => ErrorCode::Ok,
        400 | 413 | 414 | 415 | 422 | 431 => ErrorCode::InvalidArgument,
        401 => ErrorCode::Unauthenticated,
        403 => ErrorCode::PermissionDenied,
        404 | 410 => ErrorCode::NotFound,
        405 | 501 => ErrorCode::Unimplemented,
        408 | 504 => ErrorCode::DeadlineExceeded,
        409 => ErrorCode::AlreadyExists,
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
/// from `now`. A date in the past gives zero; garbage gives `None`. A number
/// of seconds too large for a `u64` saturates rather than failing, so a
/// retry policy sees it as "far too long" instead of "no hint".
pub fn parse_retry_after(value: &str, now: SystemTime) -> Option<Duration> {
    let value = value.trim();
    if !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()) {
        let secs = value.parse::<u64>().unwrap_or(u64::MAX);
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

/// The shape of an upstream error response, attached to an error's source
/// chain: status, content type and body length. The body itself is never
/// kept — it may echo credentials or personal data — so neither `Debug`
/// nor `Display` can leak it into a log line.
#[derive(Debug)]
pub(crate) struct UpstreamBody {
    status: u16,
    content_type: Option<String>,
    len: usize,
}

impl UpstreamBody {
    pub(crate) fn new(status: StatusCode, headers: &HeaderMap, body: &[u8]) -> Self {
        let content_type = headers
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .map(|value| {
                let essence = value.split(';').next().unwrap_or_default().trim();
                sekvent_telemetry::truncate_for_log(essence, CONTENT_TYPE_LIMIT).into_owned()
            });
        Self {
            status: status.as_u16(),
            content_type,
            len: body.len(),
        }
    }
}

impl fmt::Display for UpstreamBody {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "upstream HTTP {} response with a {}-byte body",
            self.status, self.len
        )?;
        if let Some(content_type) = &self.content_type {
            write!(f, " of type {content_type}")?;
        }
        Ok(())
    }
}

impl std::error::Error for UpstreamBody {}

/// Map a non-success response to an [`AppError`].
///
/// With `sekvent_upstream`, a sekvent JSON error body (one whose code agrees
/// with the status) is adopted as is. Otherwise the error is mapped from the
/// status alone with a message that only names the status; a `401`/`403`
/// becomes `INTERNAL`, since it is this service's credentials that were
/// refused. The body never reaches the error; only its shape goes to the
/// source chain.
pub(crate) fn map_status(
    status: StatusCode,
    headers: &HeaderMap,
    body: &[u8],
    now: SystemTime,
    sekvent_upstream: bool,
) -> AppError {
    let retry_after = retry_after_header(headers, now);
    if sekvent_upstream
        && let Some(error) = sekvent_error::http::from_json_body(status.as_u16(), body)
    {
        return match retry_after {
            Some(after) if error.retry_after().is_none() => error.with_retry_after(after),
            _ => error,
        };
    }
    let code = match code_for_status(status) {
        ErrorCode::Ok => ErrorCode::Unknown,
        ErrorCode::Unauthenticated | ErrorCode::PermissionDenied if !sekvent_upstream => {
            ErrorCode::Internal
        }
        code => code,
    };
    let error = AppError::new(
        code,
        format!("upstream responded with HTTP {}", status.as_u16()),
    )
    .with_reason("UPSTREAM_HTTP_ERROR")
    .with_metadata("upstream_status", status.as_u16().to_string())
    .with_source(UpstreamBody::new(status, headers, body));
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
            (409, ErrorCode::AlreadyExists),
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
        for status in 100..=599 {
            let code = code_for_status(StatusCode::from_u16(status).unwrap());
            assert_eq!(
                code.is_transient(),
                matches!(status, 408 | 429 | 502 | 503 | 504),
                "{status}"
            );
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
        assert_eq!(parse_retry_after("", now), None);
        assert_eq!(
            parse_retry_after(&"9".repeat(40), now),
            Some(Duration::from_secs(u64::MAX)),
            "an absurd number saturates instead of vanishing"
        );
    }

    #[test]
    fn upstream_bodies_never_reach_the_error() {
        let now = SystemTime::UNIX_EPOCH;
        let mut headers = HeaderMap::new();
        headers.insert(RETRY_AFTER, HeaderValue::from_static("3"));
        headers.insert(
            CONTENT_TYPE,
            HeaderValue::from_static("text/plain; charset=utf-8"),
        );
        let body = format!("secret-token={}", "x".repeat(4000));
        let error = map_status(
            StatusCode::SERVICE_UNAVAILABLE,
            &headers,
            body.as_bytes(),
            now,
            false,
        );
        assert_eq!(error.code(), ErrorCode::Unavailable);
        assert_eq!(error.message(), "upstream responded with HTTP 503");
        assert_eq!(error.retry_after(), Some(Duration::from_secs(3)));
        assert_eq!(
            error.metadata().get("upstream_status").map(String::as_str),
            Some("503")
        );
        assert!(!error.to_string().contains("secret-token"));
        assert!(!format!("{error:?}").contains("secret-token"));
        assert!(!format!("{:?}", error.to_wire()).contains("secret-token"));
        let source = error.source().unwrap();
        assert_eq!(
            source.to_string(),
            "upstream HTTP 503 response with a 4013-byte body of type text/plain"
        );
        assert!(!format!("{source:?}").contains("secret-token"));

        let odd = map_status(
            StatusCode::from_u16(299).unwrap(),
            &HeaderMap::new(),
            b"",
            now,
            false,
        );
        assert_eq!(odd.code(), ErrorCode::Unknown);
        assert_eq!(
            odd.source().unwrap().to_string(),
            "upstream HTTP 299 response with a 0-byte body"
        );
    }

    #[test]
    fn rejected_credentials_are_this_services_problem() {
        let now = SystemTime::UNIX_EPOCH;
        for status in [StatusCode::UNAUTHORIZED, StatusCode::FORBIDDEN] {
            let error = map_status(status, &HeaderMap::new(), b"", now, false);
            assert_eq!(error.code(), ErrorCode::Internal, "{status}");
        }
        let passed = map_status(StatusCode::UNAUTHORIZED, &HeaderMap::new(), b"", now, true);
        assert_eq!(passed.code(), ErrorCode::Unauthenticated);
        let passed = map_status(StatusCode::FORBIDDEN, &HeaderMap::new(), b"", now, true);
        assert_eq!(passed.code(), ErrorCode::PermissionDenied);
    }

    #[test]
    fn error_envelopes_are_adopted_only_from_sekvent_upstreams() {
        let now = SystemTime::UNIX_EPOCH;
        let mut headers = HeaderMap::new();
        headers.insert(RETRY_AFTER, HeaderValue::from_static("4"));
        let envelope =
            br#"{"error":{"code":"NOT_FOUND","message":"order 7 is gone","metadata":{"k":"v"}}}"#;

        let foreign = map_status(StatusCode::NOT_FOUND, &headers, envelope, now, false);
        assert_eq!(foreign.code(), ErrorCode::NotFound);
        assert_eq!(foreign.message(), "upstream responded with HTTP 404");
        assert!(foreign.metadata().get("k").is_none());

        let adopted = map_status(StatusCode::NOT_FOUND, &headers, envelope, now, true);
        assert_eq!(adopted.message(), "order 7 is gone");
        assert_eq!(adopted.retry_after(), Some(Duration::from_secs(4)));
        let hinted = br#"{"error":{"code":"UNAVAILABLE","message":"m","retry_after_ms":1500}}"#;
        let kept = map_status(StatusCode::SERVICE_UNAVAILABLE, &headers, hinted, now, true);
        assert_eq!(kept.retry_after(), Some(Duration::from_millis(1500)));

        let ok_in_500 = br#"{"error":{"code":"OK","message":"all good"}}"#;
        let error = map_status(
            StatusCode::INTERNAL_SERVER_ERROR,
            &HeaderMap::new(),
            ok_in_500,
            now,
            true,
        );
        assert_eq!(error.code(), ErrorCode::Internal);
        assert_eq!(error.message(), "upstream responded with HTTP 500");
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
