use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use http::header::{HeaderName, HeaderValue};
use http::{HeaderMap, Method, StatusCode};
use reqwest::Url;
use sekvent_config::Secret;
use sekvent_context::{CallContext, Clock, SystemClock};
use sekvent_error::AppError;
use sekvent_resilience::{Policy, RetryPolicy, Timeout};

use crate::mapping::{map_status, map_transport_error};
use crate::{BearerSource, HttpResponse, RequestBuilder};

/// Why an [`HttpClient`] (or a token source) could not be built.
///
/// Messages name the offending setting, never its value: a URL may carry
/// credentials.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum BuildError {
    /// The base URL is not an absolute `http` or `https` URL.
    #[error("the base URL is not a valid absolute http(s) URL")]
    InvalidBaseUrl,
    /// The token endpoint URL is not an absolute `http` or `https` URL.
    #[error("the token URL is not a valid absolute http(s) URL")]
    InvalidTokenUrl,
    /// A default header name or value is invalid.
    #[error("the default header {name} is invalid")]
    InvalidHeader {
        /// The header name as given.
        name: String,
    },
    /// The underlying HTTP client could not be created.
    #[error("the HTTP client could not be initialised")]
    Backend(#[source] reqwest::Error),
}

pub(crate) enum Auth {
    None,
    Basic { username: String, password: Secret },
    Bearer(Secret),
    Source(Arc<dyn BearerSource>),
}

pub(crate) struct Inner {
    pub(crate) http: reqwest::Client,
    pub(crate) base_url: Option<Url>,
    pub(crate) request_timeout: Option<Duration>,
    pub(crate) policy: Policy,
    pub(crate) auth: Auth,
    pub(crate) clock: Arc<dyn Clock>,
    pub(crate) propagate_context: bool,
}

/// An outbound HTTP client. Cheap to clone; clones share the connection
/// pool and the resilience state.
#[derive(Clone)]
pub struct HttpClient {
    pub(crate) inner: Arc<Inner>,
}

impl fmt::Debug for HttpClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HttpClient")
            .field(
                "host",
                &self.inner.base_url.as_ref().and_then(Url::host_str),
            )
            .field("policy", &self.inner.policy.name())
            .finish_non_exhaustive()
    }
}

impl HttpClient {
    /// Start configuring a client.
    pub fn builder() -> HttpClientBuilder {
        HttpClientBuilder::default()
    }

    /// A request with any method. `path` is relative to the base URL, or an
    /// absolute URL when no base URL is configured.
    pub fn request(&self, method: Method, path: &str) -> RequestBuilder {
        RequestBuilder::new(self.clone(), method, path)
    }
    /// A `GET` request (idempotent).
    pub fn get(&self, path: &str) -> RequestBuilder {
        self.request(Method::GET, path)
    }
    /// A `HEAD` request (idempotent).
    pub fn head(&self, path: &str) -> RequestBuilder {
        self.request(Method::HEAD, path)
    }
    /// A `POST` request (not idempotent unless marked).
    pub fn post(&self, path: &str) -> RequestBuilder {
        self.request(Method::POST, path)
    }
    /// A `PUT` request (idempotent).
    pub fn put(&self, path: &str) -> RequestBuilder {
        self.request(Method::PUT, path)
    }
    /// A `PATCH` request (not idempotent unless marked).
    pub fn patch(&self, path: &str) -> RequestBuilder {
        self.request(Method::PATCH, path)
    }
    /// A `DELETE` request (idempotent).
    pub fn delete(&self, path: &str) -> RequestBuilder {
        self.request(Method::DELETE, path)
    }

    /// The resilience policy applied to requests.
    pub fn policy(&self) -> &Policy {
        &self.inner.policy
    }

    /// Resolve `path` against the base URL. The result must stay on the
    /// base URL's origin, so a path can never redirect credentials elsewhere.
    pub(crate) fn resolve(&self, path: &str) -> Result<Url, AppError> {
        let invalid = || AppError::invalid_argument("the request path is not a valid URL");
        let Some(base) = &self.inner.base_url else {
            let url = Url::parse(path).map_err(|_| invalid())?;
            return if matches!(url.scheme(), "http" | "https") {
                Ok(url)
            } else {
                Err(invalid())
            };
        };
        let url = base
            .join(path.trim_start_matches('/'))
            .map_err(|_| invalid())?;
        if url.origin() == base.origin() {
            Ok(url)
        } else {
            Err(AppError::invalid_argument(
                "the request path must stay on the base URL's host",
            ))
        }
    }

    /// One attempt: send, re-authenticate once on `401` when a bearer source
    /// is configured, read the body and map failures.
    pub(crate) async fn attempt(
        &self,
        ctx: &CallContext,
        method: &Method,
        url: &Url,
        headers: &HeaderMap,
        body: Option<&Bytes>,
    ) -> Result<HttpResponse, AppError> {
        let mut headers = headers.clone();
        if self.inner.propagate_context {
            sekvent_context::headers::inject(ctx, &mut headers);
        }
        let mut response = self.send_once(ctx, method, url, &headers, body).await?;
        if response.status() == StatusCode::UNAUTHORIZED
            && let Auth::Source(source) = &self.inner.auth
        {
            tracing::debug!("upstream answered 401; refreshing the bearer token once");
            source.invalidate();
            response = self.send_once(ctx, method, url, &headers, body).await?;
        }
        let status = response.status();
        tracing::Span::current().record("status", status.as_u16());
        let response_headers = response.headers().clone();
        let bytes = response.bytes().await.map_err(map_transport_error)?;
        if status.is_success() || status.is_redirection() {
            Ok(HttpResponse::new(status, response_headers, bytes))
        } else {
            Err(map_status(
                status,
                &response_headers,
                &bytes,
                self.inner.clock.now(),
            ))
        }
    }

    async fn send_once(
        &self,
        ctx: &CallContext,
        method: &Method,
        url: &Url,
        headers: &HeaderMap,
        body: Option<&Bytes>,
    ) -> Result<reqwest::Response, AppError> {
        let mut request = self
            .inner
            .http
            .request(method.clone(), url.clone())
            .headers(headers.clone());
        request = match &self.inner.auth {
            Auth::None => request,
            Auth::Basic { username, password } => {
                request.basic_auth(username, Some(password.expose()))
            }
            Auth::Bearer(token) => request.bearer_auth(token.expose()),
            Auth::Source(source) => request.bearer_auth(source.token(ctx).await?.expose()),
        };
        if let Some(body) = body {
            request = request.body(body.clone());
        }
        if let Some(limit) = effective_timeout(self.inner.request_timeout, ctx) {
            request = request.timeout(limit);
        }
        request.send().await.map_err(map_transport_error)
    }
}

/// `min(request timeout, time left before the deadline)`.
fn effective_timeout(configured: Option<Duration>, ctx: &CallContext) -> Option<Duration> {
    match (configured, sekvent_resilience::remaining(ctx)) {
        (Some(limit), Some(left)) => Some(limit.min(left)),
        (limit, left) => limit.or(left),
    }
}

/// Configures an [`HttpClient`].
pub struct HttpClientBuilder {
    base_url: Option<String>,
    connect_timeout: Option<Duration>,
    request_timeout: Option<Duration>,
    read_timeout: Option<Duration>,
    user_agent: String,
    default_headers: Vec<(String, String)>,
    policy: Option<Policy>,
    auth: Auth,
    clock: Arc<dyn Clock>,
    propagate_context: bool,
}

impl fmt::Debug for HttpClientBuilder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HttpClientBuilder")
            .field("connect_timeout", &self.connect_timeout)
            .field("request_timeout", &self.request_timeout)
            .field("read_timeout", &self.read_timeout)
            .finish_non_exhaustive()
    }
}

impl Default for HttpClientBuilder {
    fn default() -> Self {
        Self {
            base_url: None,
            connect_timeout: Some(Duration::from_secs(10)),
            request_timeout: Some(Duration::from_secs(30)),
            read_timeout: None,
            user_agent: concat!("sekvent-client/", env!("CARGO_PKG_VERSION")).to_owned(),
            default_headers: Vec::new(),
            policy: None,
            auth: Auth::None,
            clock: Arc::new(SystemClock),
            propagate_context: true,
        }
    }
}

impl HttpClientBuilder {
    /// Base URL request paths are resolved against, e.g.
    /// `https://billing.example/api` (a trailing slash is implied).
    #[must_use]
    pub fn base_url(mut self, url: impl Into<String>) -> Self {
        self.base_url = Some(url.into());
        self
    }
    /// Connection establishment timeout (default 10 s).
    #[must_use]
    pub fn connect_timeout(mut self, timeout: Duration) -> Self {
        self.connect_timeout = Some(timeout);
        self
    }
    /// Whole-request timeout per attempt (default 30 s); always capped by
    /// the call deadline.
    #[must_use]
    pub fn request_timeout(mut self, timeout: Duration) -> Self {
        self.request_timeout = Some(timeout);
        self
    }
    /// Longest pause between reads of the response.
    #[must_use]
    pub fn read_timeout(mut self, timeout: Duration) -> Self {
        self.read_timeout = Some(timeout);
        self
    }
    /// The `User-Agent` header.
    #[must_use]
    pub fn user_agent(mut self, agent: impl Into<String>) -> Self {
        self.user_agent = agent.into();
        self
    }
    /// A header sent with every request.
    #[must_use]
    pub fn default_header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.default_headers.push((name.into(), value.into()));
        self
    }
    /// Replace the default policy (request timeout plus
    /// [`RetryPolicy::default`] for idempotent requests).
    #[must_use]
    pub fn policy(mut self, policy: Policy) -> Self {
        self.policy = Some(policy);
        self
    }
    /// HTTP Basic credentials.
    #[must_use]
    pub fn basic_auth(mut self, username: impl Into<String>, password: Secret) -> Self {
        self.auth = Auth::Basic {
            username: username.into(),
            password,
        };
        self
    }
    /// A fixed bearer token.
    #[must_use]
    pub fn bearer_token(mut self, token: Secret) -> Self {
        self.auth = Auth::Bearer(token);
        self
    }
    /// Bearer tokens from `source`, refreshed once after a `401`.
    #[must_use]
    pub fn with_bearer_source(mut self, source: Arc<dyn BearerSource>) -> Self {
        self.auth = Auth::Source(source);
        self
    }
    /// The clock used to interpret `Retry-After` dates.
    #[must_use]
    pub fn clock(mut self, clock: Arc<dyn Clock>) -> Self {
        self.clock = clock;
        self
    }
    /// Whether to send the call context (request id, `grpc-timeout`,
    /// trace, identity headers) upstream. On by default; turn it off for
    /// third-party APIs that should not see internal identifiers.
    #[must_use]
    pub fn propagate_context(mut self, propagate: bool) -> Self {
        self.propagate_context = propagate;
        self
    }

    /// Build the client.
    pub fn build(self) -> Result<HttpClient, BuildError> {
        let base_url = self.base_url.as_deref().map(parse_base_url).transpose()?;

        let mut headers = HeaderMap::new();
        for (name, value) in &self.default_headers {
            let invalid = || BuildError::InvalidHeader { name: name.clone() };
            let header_name = HeaderName::from_bytes(name.as_bytes()).map_err(|_| invalid())?;
            let mut header_value = HeaderValue::from_str(value).map_err(|_| invalid())?;
            header_value.set_sensitive(true);
            headers.append(header_name, header_value);
        }
        HeaderValue::from_str(&self.user_agent).map_err(|_| BuildError::InvalidHeader {
            name: "user-agent".to_owned(),
        })?;

        let mut http = reqwest::Client::builder()
            .tls_backend_rustls()
            .user_agent(self.user_agent)
            .default_headers(headers);
        if let Some(timeout) = self.connect_timeout {
            http = http.connect_timeout(timeout);
        }
        if let Some(timeout) = self.read_timeout {
            http = http.read_timeout(timeout);
        }
        let http = http.build().map_err(BuildError::Backend)?;

        let policy = self.policy.unwrap_or_else(|| {
            Policy::new("http")
                .with_timeout(
                    self.request_timeout
                        .map_or_else(Timeout::deadline_only, Timeout::new),
                )
                .with_retry(RetryPolicy::default())
        });

        Ok(HttpClient {
            inner: Arc::new(Inner {
                http,
                base_url,
                request_timeout: self.request_timeout,
                policy,
                auth: self.auth,
                clock: self.clock,
                propagate_context: self.propagate_context,
            }),
        })
    }
}

/// Parse an absolute http(s) URL and make its path end with `/` so relative
/// paths extend it instead of replacing its last segment.
pub(crate) fn parse_base_url(raw: &str) -> Result<Url, BuildError> {
    let mut url = Url::parse(raw).map_err(|_| BuildError::InvalidBaseUrl)?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return Err(BuildError::InvalidBaseUrl);
    }
    if !url.path().ends_with('/') {
        let path = format!("{}/", url.path());
        url.set_path(&path);
    }
    Ok(url)
}

#[cfg(test)]
mod tests {
    use sekvent_error::ErrorCode;

    use super::*;

    #[test]
    fn base_urls_are_validated() {
        assert!(matches!(
            parse_base_url("not a url"),
            Err(BuildError::InvalidBaseUrl)
        ));
        assert!(matches!(
            parse_base_url("ftp://files.example"),
            Err(BuildError::InvalidBaseUrl)
        ));
        assert_eq!(
            parse_base_url("http://h.example/api").unwrap().path(),
            "/api/"
        );
        assert_eq!(
            parse_base_url("http://h.example/api/").unwrap().path(),
            "/api/"
        );
        assert_eq!(parse_base_url("http://h.example").unwrap().path(), "/");
        let error = HttpClient::builder()
            .base_url("http://user:pw@")
            .build()
            .unwrap_err();
        assert!(!error.to_string().contains("pw"));
    }

    #[test]
    fn paths_resolve_under_the_base() {
        let client = HttpClient::builder()
            .base_url("http://h.example/api")
            .build()
            .unwrap();
        assert_eq!(
            client.resolve("/orders/7").unwrap().as_str(),
            "http://h.example/api/orders/7"
        );
        assert_eq!(
            client.resolve("orders").unwrap().as_str(),
            "http://h.example/api/orders"
        );
        let escaped = client
            .resolve("http://elsewhere.example/steal")
            .unwrap_err();
        assert_eq!(escaped.code(), ErrorCode::InvalidArgument);
        let scheme_relative = client.resolve("//elsewhere.example/x").unwrap();
        assert_eq!(scheme_relative.host_str(), Some("h.example"));

        let bare = HttpClient::builder().build().unwrap();
        assert_eq!(bare.resolve("https://h.example/x").unwrap().path(), "/x");
        assert_eq!(
            bare.resolve("/relative").unwrap_err().code(),
            ErrorCode::InvalidArgument
        );
        assert_eq!(
            bare.resolve("file:///etc/passwd").unwrap_err().code(),
            ErrorCode::InvalidArgument
        );
    }

    #[test]
    fn headers_are_validated() {
        let error = HttpClient::builder()
            .default_header("bad name", "x")
            .build()
            .unwrap_err();
        assert_eq!(error.to_string(), "the default header bad name is invalid");
        let error = HttpClient::builder()
            .default_header("x-ok", "line\nbreak")
            .build()
            .unwrap_err();
        assert!(matches!(error, BuildError::InvalidHeader { .. }));
        let error = HttpClient::builder()
            .user_agent("bad\nagent")
            .build()
            .unwrap_err();
        assert!(matches!(error, BuildError::InvalidHeader { name } if name == "user-agent"));
    }

    #[test]
    fn default_policy_times_out_and_retries() {
        let client = HttpClient::builder()
            .request_timeout(Duration::from_secs(5))
            .build()
            .unwrap();
        assert_eq!(
            client.policy().timeout().limit(),
            Some(Duration::from_secs(5))
        );
        assert_eq!(client.policy().retry().max_attempts(), 3);
        let debug = format!("{client:?}");
        assert!(debug.contains("HttpClient"));
        let builder = HttpClient::builder().read_timeout(Duration::from_secs(1));
        assert!(format!("{builder:?}").contains("read_timeout"));
    }

    #[tokio::test]
    async fn effective_timeout_respects_the_deadline() {
        let ctx = CallContext::new().with_timeout(Duration::from_secs(2));
        let limit = effective_timeout(Some(Duration::from_secs(30)), &ctx).unwrap();
        assert!(limit <= Duration::from_secs(2));
        assert_eq!(
            effective_timeout(Some(Duration::from_secs(1)), &ctx),
            Some(Duration::from_secs(1))
        );
        assert_eq!(effective_timeout(None, &CallContext::new()), None);
        assert!(effective_timeout(None, &ctx).is_some());
    }
}
