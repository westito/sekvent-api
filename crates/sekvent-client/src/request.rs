use std::fmt;
use std::time::Instant;

use bytes::Bytes;
use http::header::{
    AUTHORIZATION, CONTENT_TYPE, COOKIE, HeaderName, HeaderValue, PROXY_AUTHORIZATION,
};
use http::{HeaderMap, Method};
use reqwest::Url;
use sekvent_context::CallContext;
use sekvent_error::{AppError, ErrorCode};
use sekvent_resilience::Policy;
use serde::Serialize;
use serde::de::DeserializeOwned;
use tracing::Instrument;

use crate::{HttpClient, HttpResponse};

/// A request being built. Nothing is sent until [`RequestBuilder::send`].
///
/// Mistakes made while building (an invalid header, an unserializable body,
/// a path off the base URL) surface as an `AppError` from `send`.
pub struct RequestBuilder {
    client: HttpClient,
    method: Method,
    url: Result<Url, AppError>,
    headers: HeaderMap,
    body: Option<Bytes>,
    idempotent: bool,
    policy: Option<Policy>,
    error: Option<AppError>,
}

impl fmt::Debug for RequestBuilder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RequestBuilder")
            .field("method", &self.method)
            .field("path", &self.url.as_ref().map(Url::path).ok())
            .field("idempotent", &self.idempotent)
            .finish_non_exhaustive()
    }
}

impl RequestBuilder {
    pub(crate) fn new(client: HttpClient, method: Method, path: &str) -> Self {
        let url = client.resolve(path);
        let idempotent = matches!(
            method,
            Method::GET
                | Method::HEAD
                | Method::PUT
                | Method::DELETE
                | Method::OPTIONS
                | Method::TRACE
        );
        Self {
            client,
            method,
            url,
            headers: HeaderMap::new(),
            body: None,
            idempotent,
            policy: None,
            error: None,
        }
    }

    fn fail(&mut self, error: AppError) {
        if self.error.is_none() {
            self.error = Some(error);
        }
    }

    /// Serialize `body` as the JSON request body.
    #[must_use]
    pub fn json<T: Serialize + ?Sized>(mut self, body: &T) -> Self {
        match serde_json::to_vec(body) {
            Ok(bytes) => {
                self.headers
                    .insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
                self.body = Some(Bytes::from(bytes));
            }
            Err(error) => self.fail(
                AppError::new(
                    ErrorCode::Internal,
                    "the request body could not be serialized",
                )
                .with_source(error),
            ),
        }
        self
    }

    /// A raw request body with its content type.
    #[must_use]
    pub fn body(mut self, body: impl Into<Bytes>, content_type: &str) -> Self {
        match HeaderValue::from_str(content_type) {
            Ok(value) => {
                self.headers.insert(CONTENT_TYPE, value);
                self.body = Some(body.into());
            }
            Err(_) => self.fail(AppError::invalid_argument("invalid content type")),
        }
        self
    }

    /// Append query parameters (percent-encoded).
    #[must_use]
    pub fn query<K: AsRef<str>, V: AsRef<str>>(mut self, pairs: &[(K, V)]) -> Self {
        if let Ok(url) = &mut self.url {
            let mut query = url.query_pairs_mut();
            for (key, value) in pairs {
                query.append_pair(key.as_ref(), value.as_ref());
            }
        }
        self
    }

    /// Add a header. Credential-bearing headers are marked sensitive.
    #[must_use]
    pub fn header(mut self, name: &str, value: &str) -> Self {
        let parsed = HeaderName::from_bytes(name.as_bytes())
            .ok()
            .zip(HeaderValue::from_str(value).ok());
        match parsed {
            Some((name, mut value)) => {
                if name == AUTHORIZATION || name == PROXY_AUTHORIZATION || name == COOKIE {
                    value.set_sensitive(true);
                }
                self.headers.append(name, value);
            }
            None => self.fail(AppError::invalid_argument("invalid request header")),
        }
        self
    }

    /// Declare whether sending this request twice is safe. Defaults to
    /// `true` for `GET`, `HEAD`, `PUT`, `DELETE`, `OPTIONS` and `TRACE`,
    /// `false` otherwise. Only idempotent requests are retried.
    #[must_use]
    pub fn idempotent(mut self, idempotent: bool) -> Self {
        self.idempotent = idempotent;
        self
    }

    /// Use `policy` for this request instead of the client's.
    #[must_use]
    pub fn policy(mut self, policy: Policy) -> Self {
        self.policy = Some(policy);
        self
    }

    /// Send the request under the policy and return a successful response;
    /// any non-success status is an `AppError`.
    pub async fn send(self, ctx: &CallContext) -> Result<HttpResponse, AppError> {
        if let Some(error) = self.error {
            return Err(error);
        }
        let url = self.url?;
        let span = tracing::debug_span!(
            "http_client_request",
            method = %self.method,
            host = url.host_str().unwrap_or_default(),
            path = url.path(),
            status = tracing::field::Empty,
            elapsed_ms = tracing::field::Empty,
        );
        let started = Instant::now();
        let client = &self.client;
        let policy = self.policy.as_ref().unwrap_or(&client.inner.policy);
        let (method, headers, body) = (&self.method, &self.headers, self.body.as_ref());
        let result = policy
            .call(ctx, self.idempotent, || {
                client.attempt(ctx, method, &url, headers, body)
            })
            .instrument(span.clone())
            .await;
        let elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        span.record("elapsed_ms", elapsed_ms);
        match &result {
            Ok(_) => tracing::debug!(parent: &span, "http request completed"),
            Err(error) => {
                tracing::debug!(parent: &span, code = %error.code(), "http request failed");
            }
        }
        result
    }

    /// [`RequestBuilder::send`], then decode the body as JSON.
    pub async fn send_json<R: DeserializeOwned>(self, ctx: &CallContext) -> Result<R, AppError> {
        self.send(ctx).await?.json()
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    fn client() -> HttpClient {
        HttpClient::builder()
            .base_url("http://h.example/api")
            .build()
            .unwrap()
    }

    #[test]
    fn idempotency_defaults_follow_the_method() {
        let client = client();
        assert!(client.get("/x").idempotent);
        assert!(client.head("/x").idempotent);
        assert!(client.put("/x").idempotent);
        assert!(client.delete("/x").idempotent);
        assert!(!client.post("/x").idempotent);
        assert!(!client.patch("/x").idempotent);
        assert!(client.post("/x").idempotent(true).idempotent);
    }

    #[test]
    fn query_and_headers_are_encoded() {
        let request = client()
            .get("/search")
            .query(&[("q", "a b&c"), ("page", "2")])
            .header("authorization", "Bearer secret")
            .header("x-trace", "1");
        let url = request.url.as_ref().unwrap();
        assert_eq!(url.query(), Some("q=a+b%26c&page=2"));
        assert!(request.headers["authorization"].is_sensitive());
        assert!(!request.headers["x-trace"].is_sensitive());
        let debug = format!("{request:?}");
        assert!(debug.contains("/api/search"));
        assert!(!debug.contains("secret"));
        assert!(!debug.contains("a+b"));
    }

    #[tokio::test]
    async fn builder_mistakes_surface_on_send() {
        let ctx = CallContext::new();
        let error = client()
            .get("/x")
            .header("bad header", "v")
            .send(&ctx)
            .await
            .unwrap_err();
        assert_eq!(error.code(), ErrorCode::InvalidArgument);
        let error = client()
            .post("/x")
            .body("x", "bad\ntype")
            .send(&ctx)
            .await
            .unwrap_err();
        assert_eq!(error.code(), ErrorCode::InvalidArgument);
        let mut not_json = BTreeMap::new();
        not_json.insert(vec![1_u8], 1);
        let error = client()
            .post("/x")
            .json(&not_json)
            .send(&ctx)
            .await
            .unwrap_err();
        assert_eq!(error.code(), ErrorCode::Internal);
        let error = client()
            .get("http://other.example/")
            .query(&[("a", "b")])
            .send(&ctx)
            .await
            .unwrap_err();
        assert_eq!(error.code(), ErrorCode::InvalidArgument);
    }
}
