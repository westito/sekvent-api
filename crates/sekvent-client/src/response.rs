use std::fmt;

use bytes::Bytes;
use http::{HeaderMap, StatusCode};
use sekvent_error::{AppError, ErrorCode};
use serde::de::DeserializeOwned;

use crate::mapping::JsonShape;

/// A successful (`2xx` or unfollowed `3xx`) response with its body read.
///
/// `Debug` shows the status and body length only; headers and body may
/// carry credentials or personal data.
#[derive(Clone)]
pub struct HttpResponse {
    status: StatusCode,
    headers: HeaderMap,
    body: Bytes,
}

impl HttpResponse {
    pub(crate) fn new(status: StatusCode, headers: HeaderMap, body: Bytes) -> Self {
        Self {
            status,
            headers,
            body,
        }
    }

    /// The status code.
    pub fn status(&self) -> StatusCode {
        self.status
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

#[cfg(test)]
mod tests {
    use super::*;

    fn response(body: &'static [u8]) -> HttpResponse {
        let mut headers = HeaderMap::new();
        headers.insert("set-cookie", "session=secret".parse().unwrap());
        HttpResponse::new(StatusCode::OK, headers, Bytes::from_static(body))
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
}
