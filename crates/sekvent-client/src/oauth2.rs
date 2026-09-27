//! OAuth 2.0 client-credentials tokens (RFC 6749 §4.4) with caching and
//! single-flight refresh.

use std::fmt;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, SystemTime};

use futures::future::BoxFuture;
use http::header::{ACCEPT, CONTENT_TYPE};
use reqwest::Url;
use sekvent_config::Secret;
use sekvent_context::{CallContext, Clock, SystemClock};
use sekvent_error::{AppError, ErrorCode};
use serde::{Deserialize, Deserializer};

use crate::mapping::{JsonShape, UpstreamBody, code_for_status, map_transport_error};
use crate::{BearerSource, BuildError};

/// How the client authenticates to the token endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum ClientAuthStyle {
    /// HTTP Basic with the form-encoded id and secret (`client_secret_basic`).
    #[default]
    Basic,
    /// `client_id` and `client_secret` in the form body (`client_secret_post`).
    RequestBody,
}

struct CachedToken {
    token: Secret,
    refresh_at: SystemTime,
}

/// Client-credentials token source.
///
/// A token is cached until `expires_in` minus the refresh skew. Concurrent
/// callers needing a new token wait on a single request to the endpoint.
/// [`ClientCredentials::invalidate`] drops the cached token (the HTTP client
/// calls it after a `401`). Time comes from the injected [`Clock`].
pub struct ClientCredentials {
    http: reqwest::Client,
    token_url: Url,
    client_id: String,
    client_secret: Secret,
    scope: Option<String>,
    audience: Option<String>,
    style: ClientAuthStyle,
    refresh_skew: Duration,
    default_ttl: Duration,
    timeout: Duration,
    clock: Arc<dyn Clock>,
    cache: Mutex<Option<CachedToken>>,
    refresh: tokio::sync::Mutex<()>,
}

impl fmt::Debug for ClientCredentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ClientCredentials")
            .field("token_host", &self.token_url.host_str())
            .field("client_id", &self.client_id)
            .field("style", &self.style)
            .finish_non_exhaustive()
    }
}

impl ClientCredentials {
    /// Start configuring a token source for `token_url`.
    pub fn builder(
        token_url: impl Into<String>,
        client_id: impl Into<String>,
        client_secret: Secret,
    ) -> ClientCredentialsBuilder {
        ClientCredentialsBuilder {
            token_url: token_url.into(),
            client_id: client_id.into(),
            client_secret,
            scope: None,
            audience: None,
            style: ClientAuthStyle::Basic,
            refresh_skew: Duration::from_secs(30),
            default_ttl: Duration::from_secs(300),
            timeout: Duration::from_secs(10),
            clock: Arc::new(SystemClock),
        }
    }

    /// A valid access token, fetching one if none is cached or it is due
    /// for refresh.
    pub async fn token(&self, ctx: &CallContext) -> Result<Secret, AppError> {
        if let Some(token) = self.cached() {
            return Ok(token);
        }
        let _flight = self.refresh.lock().await;
        if let Some(token) = self.cached() {
            return Ok(token);
        }
        let (token, ttl) = self.fetch(ctx).await?;
        let refresh_at = self.clock.now() + ttl.saturating_sub(self.refresh_skew);
        *self.lock_cache() = Some(CachedToken {
            token: token.clone(),
            refresh_at,
        });
        Ok(token)
    }

    /// Drop the cached token.
    pub fn invalidate(&self) {
        *self.lock_cache() = None;
    }

    fn lock_cache(&self) -> std::sync::MutexGuard<'_, Option<CachedToken>> {
        self.cache.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn cached(&self) -> Option<Secret> {
        let now = self.clock.now();
        self.lock_cache()
            .as_ref()
            .filter(|cached| now < cached.refresh_at)
            .map(|cached| cached.token.clone())
    }

    async fn fetch(&self, ctx: &CallContext) -> Result<(Secret, Duration), AppError> {
        let mut form: Vec<(&str, &str)> = vec![("grant_type", "client_credentials")];
        if let Some(scope) = &self.scope {
            form.push(("scope", scope.as_str()));
        }
        if let Some(audience) = &self.audience {
            form.push(("audience", audience.as_str()));
        }
        let mut request = self
            .http
            .post(self.token_url.clone())
            .header(CONTENT_TYPE, "application/x-www-form-urlencoded")
            .header(ACCEPT, "application/json");
        match self.style {
            ClientAuthStyle::Basic => {
                request = request.basic_auth(
                    form_encode(&self.client_id),
                    Some(form_encode(self.client_secret.expose())),
                );
            }
            ClientAuthStyle::RequestBody => {
                form.push(("client_id", self.client_id.as_str()));
                form.push(("client_secret", self.client_secret.expose()));
            }
        }
        let timeout =
            sekvent_resilience::remaining(ctx).map_or(self.timeout, |left| left.min(self.timeout));
        let response = request
            .body(encode_form(&form))
            .timeout(timeout)
            .send()
            .await
            .map_err(map_transport_error)?;
        let status = response.status();
        let bytes = response.bytes().await.map_err(map_transport_error)?;
        if !status.is_success() {
            let code = match code_for_status(status) {
                code if code.is_transient() => code,
                _ => ErrorCode::Unauthenticated,
            };
            return Err(AppError::new(
                code,
                format!("the token endpoint responded with HTTP {}", status.as_u16()),
            )
            .with_reason("TOKEN_REQUEST_FAILED")
            .with_source(UpstreamBody::new(status, &bytes)));
        }
        let parsed: TokenResponse = serde_json::from_slice(&bytes).map_err(|error| {
            AppError::unauthenticated("the token endpoint returned an unreadable response")
                .with_source(JsonShape::new(&error))
        })?;
        if parsed
            .token_type
            .as_deref()
            .is_some_and(|kind| !kind.eq_ignore_ascii_case("bearer"))
        {
            return Err(AppError::unauthenticated(
                "the token endpoint returned a token that is not a bearer token",
            ));
        }
        if parsed.access_token.is_blank() {
            return Err(AppError::unauthenticated(
                "the token endpoint returned an empty token",
            ));
        }
        let ttl = parsed
            .expires_in
            .map_or(self.default_ttl, Duration::from_secs);
        tracing::debug!(ttl_secs = ttl.as_secs(), "fetched an OAuth2 access token");
        Ok((parsed.access_token, ttl))
    }
}

impl BearerSource for ClientCredentials {
    fn token<'a>(&'a self, ctx: &'a CallContext) -> BoxFuture<'a, Result<Secret, AppError>> {
        Box::pin(ClientCredentials::token(self, ctx))
    }

    fn invalidate(&self) {
        ClientCredentials::invalidate(self);
    }
}

#[derive(Deserialize)]
struct TokenResponse {
    #[serde(deserialize_with = "secret")]
    access_token: Secret,
    #[serde(default)]
    token_type: Option<String>,
    #[serde(default, deserialize_with = "seconds")]
    expires_in: Option<u64>,
}

fn secret<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Secret, D::Error> {
    String::deserialize(deserializer).map(Secret::new)
}

/// `expires_in` as a number or, as some servers send it, a numeric string.
fn seconds<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<u64>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Repr {
        Number(u64),
        Text(String),
    }
    match Option::<Repr>::deserialize(deserializer)? {
        None => Ok(None),
        Some(Repr::Number(secs)) => Ok(Some(secs)),
        Some(Repr::Text(text)) => text
            .trim()
            .parse()
            .map(Some)
            .map_err(|_| serde::de::Error::custom("expires_in is not a number")),
    }
}

/// `application/x-www-form-urlencoded` encoding of one value.
fn form_encode(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'*' | b'-' | b'.' | b'_' => {
                encoded.push(char::from(byte));
            }
            b' ' => encoded.push('+'),
            _ => {
                const HEX: &[u8; 16] = b"0123456789ABCDEF";
                encoded.push('%');
                encoded.push(char::from(HEX[usize::from(byte >> 4)]));
                encoded.push(char::from(HEX[usize::from(byte & 0x0f)]));
            }
        }
    }
    encoded
}

fn encode_form(pairs: &[(&str, &str)]) -> String {
    pairs
        .iter()
        .map(|(key, value)| format!("{}={}", form_encode(key), form_encode(value)))
        .collect::<Vec<_>>()
        .join("&")
}

/// Configures [`ClientCredentials`].
pub struct ClientCredentialsBuilder {
    token_url: String,
    client_id: String,
    client_secret: Secret,
    scope: Option<String>,
    audience: Option<String>,
    style: ClientAuthStyle,
    refresh_skew: Duration,
    default_ttl: Duration,
    timeout: Duration,
    clock: Arc<dyn Clock>,
}

impl fmt::Debug for ClientCredentialsBuilder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ClientCredentialsBuilder")
            .field("client_id", &self.client_id)
            .field("style", &self.style)
            .finish_non_exhaustive()
    }
}

impl ClientCredentialsBuilder {
    /// Requested scope(s), space-separated.
    #[must_use]
    pub fn scope(mut self, scope: impl Into<String>) -> Self {
        self.scope = Some(scope.into());
        self
    }
    /// Requested audience (a common extension parameter).
    #[must_use]
    pub fn audience(mut self, audience: impl Into<String>) -> Self {
        self.audience = Some(audience.into());
        self
    }
    /// How to present the client credentials (default Basic).
    #[must_use]
    pub fn auth_style(mut self, style: ClientAuthStyle) -> Self {
        self.style = style;
        self
    }
    /// Refresh this long before expiry (default 30 s).
    #[must_use]
    pub fn refresh_skew(mut self, skew: Duration) -> Self {
        self.refresh_skew = skew;
        self
    }
    /// Lifetime assumed when the response has no `expires_in` (default 5 min).
    #[must_use]
    pub fn default_ttl(mut self, ttl: Duration) -> Self {
        self.default_ttl = ttl;
        self
    }
    /// Timeout of one token request (default 10 s; also capped by the call deadline).
    #[must_use]
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }
    /// The clock deciding when a token is due for refresh.
    #[must_use]
    pub fn clock(mut self, clock: Arc<dyn Clock>) -> Self {
        self.clock = clock;
        self
    }

    /// Build the token source.
    pub fn build(self) -> Result<ClientCredentials, BuildError> {
        let token_url = Url::parse(&self.token_url).map_err(|_| BuildError::InvalidTokenUrl)?;
        if !matches!(token_url.scheme(), "http" | "https") || token_url.host_str().is_none() {
            return Err(BuildError::InvalidTokenUrl);
        }
        let http = reqwest::Client::builder()
            .tls_backend_rustls()
            .user_agent(concat!("sekvent-client/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(BuildError::Backend)?;
        Ok(ClientCredentials {
            http,
            token_url,
            client_id: self.client_id,
            client_secret: self.client_secret,
            scope: self.scope,
            audience: self.audience,
            style: self.style,
            refresh_skew: self.refresh_skew,
            default_ttl: self.default_ttl,
            timeout: self.timeout,
            clock: self.clock,
            cache: Mutex::new(None),
            refresh: tokio::sync::Mutex::new(()),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn form_encoding_follows_the_html_rules() {
        assert_eq!(form_encode("AZaz09*-._"), "AZaz09*-._");
        assert_eq!(form_encode("a b"), "a+b");
        assert_eq!(form_encode("p@ss:w/rd&=é"), "p%40ss%3Aw%2Frd%26%3D%C3%A9");
        assert_eq!(
            encode_form(&[
                ("grant_type", "client_credentials"),
                ("scope", "read write")
            ]),
            "grant_type=client_credentials&scope=read+write"
        );
    }

    #[test]
    fn token_responses_parse_leniently() {
        let parsed: TokenResponse =
            serde_json::from_str(r#"{"access_token":"t","token_type":"Bearer","expires_in":"60"}"#)
                .unwrap();
        assert_eq!(parsed.access_token.expose(), "t");
        assert_eq!(parsed.expires_in, Some(60));
        let parsed: TokenResponse = serde_json::from_str(r#"{"access_token":"t"}"#).unwrap();
        assert_eq!(parsed.expires_in, None);
        assert!(parsed.token_type.is_none());
        assert!(
            serde_json::from_str::<TokenResponse>(r#"{"access_token":"t","expires_in":"soon"}"#)
                .is_err()
        );
    }

    #[test]
    fn token_urls_are_validated() {
        let bad = |url: &str| {
            ClientCredentials::builder(url, "id", Secret::new("s"))
                .build()
                .unwrap_err()
        };
        assert!(matches!(bad("nope"), BuildError::InvalidTokenUrl));
        assert!(matches!(
            bad("mailto:a@b.example"),
            BuildError::InvalidTokenUrl
        ));
        let ok =
            ClientCredentials::builder("https://auth.example/token", "id", Secret::new("s3cr3t"))
                .build()
                .unwrap();
        let debug = format!("{ok:?}");
        assert!(debug.contains("auth.example"));
        assert!(!debug.contains("s3cr3t"));
        let builder = ClientCredentials::builder("x", "id", Secret::new("hidden"));
        assert!(!format!("{builder:?}").contains("hidden"));
    }
}
