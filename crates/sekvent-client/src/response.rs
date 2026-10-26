use std::fmt;
use std::time::SystemTime;

use bytes::Bytes;
use http::{HeaderMap, StatusCode};
use sekvent_error::{AppError, ErrorCode};
use serde::de::DeserializeOwned;

use crate::mapping::{JsonShape, map_status};

/// A response with its body read: a `2xx` or unfollowed `3xx`, or any
/// other status when the request accepted error statuses
/// ([`RequestBuilder::accept_error_status`](crate::RequestBuilder::accept_error_status)).
///
/// `Debug` shows the status and body length only; headers and body may
/// carry credentials or personal data.
#[derive(Clone)]
pub struct HttpResponse {
    status: StatusCode,
    headers: HeaderMap,
    body: Bytes,
    received_at: SystemTime,
    sekvent_upstream: bool,
}

impl HttpResponse {
    pub(crate) fn new(
        status: StatusCode,
        headers: HeaderMap,
        body: Bytes,
        received_at: SystemTime,
        sekvent_upstream: bool,
    ) -> Self {
        Self {
            status,
            headers,
            body,
            received_at,
            sekvent_upstream,
        }
    }

    /// The status code.
    pub fn status(&self) -> StatusCode {
        self.status
    }

    /// Whether the client would have returned this response as an error:
    /// its status is neither `2xx` nor `3xx`. Only a request that accepted
    /// error statuses can see such a response.
    pub fn is_error(&self) -> bool {
        !(self.status.is_success() || self.status.is_redirection())
    }

    /// This response when it is not an error, otherwise the `AppError` the
    /// client would have returned for it without
    /// [`accept_error_status`](crate::RequestBuilder::accept_error_status):
    /// the same code, message, reason, metadata and `Retry-After` (a date
    /// is measured from when the response arrived). The body never reaches
    /// the error.
    pub fn error_for_status(self) -> Result<Self, AppError> {
        if self.is_error() {
            Err(map_status(
                self.status,
                &self.headers,
                &self.body,
                self.received_at,
                self.sekvent_upstream,
            ))
        } else {
            Ok(self)
        }
    }

    /// The response headers.
    pub fn headers(&self) -> &HeaderMap {
        &self.headers
    }

    /// The raw body.
    pub fn body(&self) -> &Bytes {
        &self.body
    }

    /// Take the raw body.
    pub fn into_body(self) -> Bytes {
        self.body
    }

    /// The body as UTF-8 text.
    pub fn text(&self) -> Result<&str, AppError> {
        std::str::from_utf8(&self.body).map_err(|error| {
            AppError::new(
                ErrorCode::Internal,
                "the upstream response body is not valid UTF-8",
            )
            .with_source(error)
        })
    }

    /// The body decoded from JSON. A mismatch is `INTERNAL`; the source
    /// describes the position of the problem, never the payload.
    pub fn json<T: DeserializeOwned>(&self) -> Result<T, AppError> {
        serde_json::from_slice(&self.body).map_err(|error| {
            AppError::new(
                ErrorCode::Internal,
                "the upstream response body could not be decoded",
            )
            .with_source(JsonShape::new(&error))
        })
    }
}

impl fmt::Debug for HttpResponse {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HttpResponse")
            .field("status", &self.status)
            .field("body_len", &self.body.len())
            .finish_non_exhaustive()
    }
}

/// A non-success response a request accepted, carried as the source of
/// the status-mapped error through the resilience policy, so retries, the
/// retry budget and the breaker see exactly what they see without the
/// opt-in. `RequestBuilder::send` turns it back into the response. Its
/// `Debug` and `Display` show the status and body length only.
#[derive(Debug)]
pub(crate) struct AcceptedResponse(pub(crate) HttpResponse);

impl AcceptedResponse {
    /// The response carried by `error`, if the error is one.
    pub(crate) fn carried_by(error: &AppError) -> Option<&HttpResponse> {
        std::error::Error::source(error)
            .and_then(|source| source.downcast_ref::<Self>())
            .map(|accepted| &accepted.0)
    }
}

impl fmt::Display for AcceptedResponse {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "upstream HTTP {} response with a {}-byte body, accepted by the caller",
            self.0.status.as_u16(),
            self.0.body.len()
        )
    }
}

impl std::error::Error for AcceptedResponse {}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use http::header::RETRY_AFTER;

    use super::*;

    fn response(body: &'static [u8]) -> HttpResponse {
        let mut headers = HeaderMap::new();
        headers.insert("set-cookie", "session=secret".parse().unwrap());
        HttpResponse::new(
            StatusCode::OK,
            headers,
            Bytes::from_static(body),
            SystemTime::UNIX_EPOCH,
            false,
        )
    }

    fn failed(status: StatusCode, headers: HeaderMap, body: &'static [u8]) -> HttpResponse {
        HttpResponse::new(
            status,
            headers,
            Bytes::from_static(body),
            SystemTime::UNIX_EPOCH,
            false,
        )
    }

    #[test]
    fn accessors_and_decoding() {
        let ok = response(br#"{"id":7}"#);
        assert_eq!(ok.status(), StatusCode::OK);
        assert!(ok.headers().contains_key("set-cookie"));
        assert_eq!(ok.text().unwrap(), r#"{"id":7}"#);
        let value: serde_json::Value = ok.json().unwrap();
        assert_eq!(value["id"], 7);
        assert_eq!(ok.body().len(), 8);
        assert_eq!(ok.into_body(), Bytes::from_static(br#"{"id":7}"#));
    }

    #[test]
    fn decoding_errors_are_internal_and_redacted() {
        let bad = response(b"\"secret-value\"");
        let error = bad.json::<u32>().unwrap_err();
        assert_eq!(error.code(), ErrorCode::Internal);
        assert!(!format!("{error:?}").contains("secret-value"));
        let invalid = response(b"\xff\xfe");
        assert_eq!(invalid.text().unwrap_err().code(), ErrorCode::Internal);
    }

    #[test]
    fn debug_hides_headers_and_body() {
        let debug = format!("{:?}", response(b"secret-body"));
        assert!(debug.contains("200"));
        assert!(!debug.contains("secret"));
    }

    #[test]
    fn error_for_status_maps_like_the_client() {
        let ok = response(b"fine");
        assert!(!ok.is_error());
        assert_eq!(ok.error_for_status().unwrap().status(), StatusCode::OK);
        let redirect = failed(StatusCode::SEE_OTHER, HeaderMap::new(), b"");
        assert!(!redirect.is_error());
        assert!(redirect.error_for_status().is_ok());

        let mut headers = HeaderMap::new();
        let at = SystemTime::UNIX_EPOCH + Duration::from_secs(30);
        headers.insert(RETRY_AFTER, httpdate::fmt_http_date(at).parse().unwrap());
        let busy = failed(StatusCode::SERVICE_UNAVAILABLE, headers, b"secret-body");
        assert!(busy.is_error());
        let error = busy.error_for_status().unwrap_err();
        assert_eq!(error.code(), ErrorCode::Unavailable);
        assert_eq!(error.message(), "upstream responded with HTTP 503");
        assert_eq!(error.reason(), Some("UPSTREAM_HTTP_ERROR"));
        assert_eq!(
            error.retry_after(),
            Some(Duration::from_secs(30)),
            "a date is measured from when the response arrived"
        );
        assert!(!format!("{error:?}").contains("secret-body"));

        let refused = failed(StatusCode::UNAUTHORIZED, HeaderMap::new(), b"");
        assert_eq!(
            refused.error_for_status().unwrap_err().code(),
            ErrorCode::Internal
        );
        let peer = HttpResponse::new(
            StatusCode::UNAUTHORIZED,
            HeaderMap::new(),
            Bytes::new(),
            SystemTime::UNIX_EPOCH,
            true,
        );
        assert_eq!(
            peer.error_for_status().unwrap_err().code(),
            ErrorCode::Unauthenticated
        );
    }

    #[test]
    fn accepted_responses_ride_the_error_without_showing_the_body() {
        let accepted = AcceptedResponse(failed(
            StatusCode::BAD_REQUEST,
            HeaderMap::new(),
            b"secret-body",
        ));
        assert_eq!(
            accepted.to_string(),
            "upstream HTTP 400 response with a 11-byte body, accepted by the caller"
        );
        assert!(!format!("{accepted:?}").contains("secret"));
        let error = AppError::invalid_argument("x").with_source(accepted);
        let carried = AcceptedResponse::carried_by(&error).unwrap();
        assert_eq!(carried.body().as_ref(), b"secret-body");
        assert!(AcceptedResponse::carried_by(&AppError::invalid_argument("x")).is_none());
        let other = AppError::invalid_argument("x").with_source(std::fmt::Error);
        assert!(AcceptedResponse::carried_by(&other).is_none());
    }
}
