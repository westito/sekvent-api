//! The login and callback routes, the login hook and their settings.

use std::borrow::Cow;
use std::error::Error as StdError;
use std::fmt::{self, Write as _};
use std::future::Future;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use axum::Router;
use axum::extract::Request;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use futures::FutureExt as _;
use futures::future::BoxFuture;
use http::{HeaderValue, StatusCode, header};
use sekvent_config::{EnvConfig, Secret};
use sekvent_context::{CallContext, Clock, SystemClock};
use sekvent_error::AppError;
use subtle::ConstantTimeEq as _;

use crate::handoff::{DEFAULT_HANDOFF_CAPACITY, DEFAULT_HANDOFF_TTL, Expiring, HandoffStore};
use crate::provider::{
    AuthorizationRequest, CodeExchange, IdentityProvider, SsoIdentity, valid_provider_id,
};
use crate::redirect::{is_safe_redirect, parse_app_url};
use crate::state::{
    CookieSpec, FlowState, StateKey, StateRejected, code_challenge, cookie_values, digest,
    random_token,
};

/// Default lifetime of a sign-in between login and callback.
pub const DEFAULT_STATE_TTL: Duration = Duration::from_secs(600);
/// Longest accepted lifetime of a sign-in between login and callback.
pub const MAX_STATE_TTL: Duration = Duration::from_secs(600);
/// Default bound on the callback's calls to the provider and the hook.
pub const DEFAULT_CALLBACK_TIMEOUT: Duration = Duration::from_secs(30);
/// Name of the state cookie with secure cookies (the `__Host-` prefix pins
/// it to the exact origin, `Path=/` and `Secure`).
pub const SECURE_COOKIE_NAME: &str = "__Host-sekvent-sso";
/// Name of the state cookie with `secure_cookies(false)`.
pub const INSECURE_COOKIE_NAME: &str = "sekvent-sso";
/// Fragment parameter carrying the handoff code on success.
pub const HANDOFF_PARAM: &str = "sso_code";
/// Fragment parameter carrying an [`SsoErrorCode`] on failure.
pub const ERROR_PARAM: &str = "sso_error";

/// Why a sign-in did not complete, as the browser learns it: a constant
/// code in the fragment (`#sso_error=access_denied`), never upstream text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum SsoErrorCode {
    /// The user cancelled at the provider, the provider's rules refused
    /// them (not a workspace member), or the login hook refused them.
    AccessDenied,
    /// The login request was unusable (an unsafe `redirect`), or the
    /// callback had no code.
    InvalidRequest,
    /// The state cookie was missing, tampered with, expired, already used
    /// or did not match the callback's `state`.
    InvalidState,
    /// The provider or the application failed.
    ServerError,
    /// A transient failure (timeout, provider unavailable); retrying may
    /// work.
    TemporarilyUnavailable,
}

impl SsoErrorCode {
    /// The code as it appears in the fragment.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::AccessDenied => "access_denied",
            Self::InvalidRequest => "invalid_request",
            Self::InvalidState => "invalid_state",
            Self::ServerError => "server_error",
            Self::TemporarilyUnavailable => "temporarily_unavailable",
        }
    }

    /// The code for an error from a provider or the login hook:
    /// `PERMISSION_DENIED` and `UNAUTHENTICATED` are `access_denied`,
    /// transient codes `temporarily_unavailable`, anything else
    /// `server_error`.
    pub fn for_error(error: &AppError) -> Self {
        use sekvent_error::ErrorCode;
        match error.code() {
            ErrorCode::PermissionDenied | ErrorCode::Unauthenticated => Self::AccessDenied,
            code if code.is_transient() => Self::TemporarilyUnavailable,
            _ => Self::ServerError,
        }
    }

    fn for_provider_error(error: &str) -> Self {
        match error {
            "access_denied" => Self::AccessDenied,
            "temporarily_unavailable" => Self::TemporarilyUnavailable,
            _ => Self::ServerError,
        }
    }
}

impl fmt::Display for SsoErrorCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The application's part of a sign-in: provision or look up the user for
/// an [`SsoIdentity`] and return what the handoff code should carry
/// (typically the application's own user id and roles).
///
/// An error refuses the sign-in: `PERMISSION_DENIED` or `UNAUTHENTICATED`
/// sends the browser back with `access_denied`, a transient code with
/// `temporarily_unavailable`, anything else with `server_error`.
///
/// Implemented for closures `Fn(CallContext, SsoIdentity) -> impl Future`.
pub trait LoginHook<O>: Send + Sync + 'static {
    /// Admit `identity` and produce the outcome to hand off.
    fn on_login(
        &self,
        ctx: CallContext,
        identity: SsoIdentity,
    ) -> BoxFuture<'static, Result<O, AppError>>;
}

impl<O, F, Fut> LoginHook<O> for F
where
    F: Fn(CallContext, SsoIdentity) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<O, AppError>> + Send + 'static,
{
    fn on_login(
        &self,
        ctx: CallContext,
        identity: SsoIdentity,
    ) -> BoxFuture<'static, Result<O, AppError>> {
        self(ctx, identity).boxed()
    }
}

/// Router settings from configuration. The struct has no prefix of its
/// own; read it with `SsoConfig::from_config(&Prefixed::new(&EnvSource,
/// "SSO_"))` for `SSO_APP_URL`, `SSO_STATE_KEY`, ….
#[derive(Debug, EnvConfig)]
pub struct SsoConfig {
    /// Base URL of the application the browser returns to, e.g.
    /// `https://app.example.com`.
    pub app_url: String,
    /// Key that signs the state cookie, at least 32 bytes. Use a key of its
    /// own, not the session JWT secret.
    pub state_key: Secret,
    /// Mark the state cookie `Secure` (and name it `__Host-…`). Turn off
    /// only for plain-http development.
    #[config(default = "on")]
    pub secure_cookies: bool,
    /// Lifetime of a sign-in between login and callback (at most 10 min).
    #[config(default = "10m")]
    pub state_ttl: Duration,
    /// Lifetime of a handoff code (at most 10 min).
    #[config(default = "60s")]
    pub handoff_ttl: Duration,
    /// Handoff codes that may wait for redemption at once.
    #[config(default = "10000")]
    pub handoff_capacity: usize,
    /// Bound on the callback's calls to the provider and the hook.
    #[config(default = "30s")]
    pub callback_timeout: Duration,
}

/// Builds an [`Sso`]. Every setting is validated by
/// [`build`](Self::build).
#[must_use]
pub struct SsoBuilder<O> {
    app_url: String,
    state_key: Secret,
    providers: Vec<Arc<dyn IdentityProvider>>,
    hook: Option<Arc<dyn LoginHook<O>>>,
    clock: Arc<dyn Clock>,
    secure_cookies: bool,
    state_ttl: Duration,
    handoff_ttl: Duration,
    handoff_capacity: usize,
    callback_timeout: Duration,
    path: String,
    default_redirect: String,
}

impl<O> fmt::Debug for SsoBuilder<O> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SsoBuilder")
            .field("app_url", &self.app_url)
            .field(
                "providers",
                &self.providers.iter().map(|p| p.id()).collect::<Vec<_>>(),
            )
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

impl<O: Send + 'static> SsoBuilder<O> {
    /// Add a provider; its routes are `<path>/<id>/login` and
    /// `<path>/<id>/callback`.
    pub fn provider<P: IdentityProvider>(mut self, provider: P) -> Self {
        self.providers.push(Arc::new(provider));
        self
    }

    /// Add a shared provider.
    pub fn provider_arc(mut self, provider: Arc<dyn IdentityProvider>) -> Self {
        self.providers.push(provider);
        self
    }

    /// The application's login hook. Required.
    pub fn on_login<H: LoginHook<O>>(mut self, hook: H) -> Self {
        self.hook = Some(Arc::new(hook));
        self
    }

    /// The clock for cookie and handoff expiry (default [`SystemClock`]).
    pub fn clock(mut self, clock: Arc<dyn Clock>) -> Self {
        self.clock = clock;
        self
    }

    /// Mark the state cookie `Secure` and name it [`SECURE_COOKIE_NAME`]
    /// (default `true`). With `false` it is [`INSECURE_COOKIE_NAME`], for
    /// plain-http development only.
    pub fn secure_cookies(mut self, secure: bool) -> Self {
        self.secure_cookies = secure;
        self
    }

    /// Lifetime of a sign-in between login and callback: whole seconds,
    /// 1 s to 10 min (default 10 min).
    pub fn state_ttl(mut self, ttl: Duration) -> Self {
        self.state_ttl = ttl;
        self
    }

    /// Lifetime of a handoff code (default 60 s, at most 10 min).
    pub fn handoff_ttl(mut self, ttl: Duration) -> Self {
        self.handoff_ttl = ttl;
        self
    }

    /// Handoff codes that may wait at once (default 10 000).
    pub fn handoff_capacity(mut self, capacity: usize) -> Self {
        self.handoff_capacity = capacity;
        self
    }

    /// Bound on the callback's calls to the provider and the hook
    /// (default 30 s). It narrows the request's call context.
    pub fn callback_timeout(mut self, timeout: Duration) -> Self {
        self.callback_timeout = timeout;
        self
    }

    /// Path the routes live under (default `/sso`), below the server's
    /// prefix.
    pub fn path(mut self, path: impl Into<String>) -> Self {
        self.path = path.into();
        self
    }

    /// Return path when the login request names none (default `/`).
    pub fn default_redirect(mut self, path: impl Into<String>) -> Self {
        self.default_redirect = path.into();
        self
    }

    /// Validate the settings and build.
    pub fn build(self) -> Result<Sso<O>, AppError> {
        let app_url = parse_app_url(&self.app_url)?;
        let key = StateKey::new(&self.state_key)?;
        let hook = self
            .hook
            .ok_or_else(|| AppError::invalid_argument("an SSO login hook is required"))?;
        if self.providers.is_empty() {
            return Err(AppError::invalid_argument(
                "SSO needs at least one identity provider",
            ));
        }
        for (index, provider) in self.providers.iter().enumerate() {
            let id = provider.id();
            if !valid_provider_id(id) {
                return Err(AppError::invalid_argument(format!(
                    "SSO provider #{} has an invalid id; use lowercase letters, digits and -",
                    index + 1
                )));
            }
            if self.providers[..index].iter().any(|other| other.id() == id) {
                return Err(AppError::invalid_argument(format!(
                    "SSO provider {id} is registered twice"
                )));
            }
        }
        if self.state_ttl < Duration::from_secs(1)
            || self.state_ttl > MAX_STATE_TTL
            || self.state_ttl.subsec_nanos() != 0
        {
            return Err(AppError::invalid_argument(
                "the SSO state TTL must be whole seconds from 1 s to 10 minutes",
            ));
        }
        if self.callback_timeout.is_zero() {
            return Err(AppError::invalid_argument(
                "the SSO callback timeout must be longer than zero",
            ));
        }
        if !valid_path(&self.path) {
            return Err(AppError::invalid_argument(
                "the SSO path must start with /, not end with /, and use unreserved characters",
            ));
        }
        if !is_safe_redirect(&self.default_redirect) {
            return Err(AppError::invalid_argument(
                "the SSO default redirect must be a relative path like /",
            ));
        }
        let handoff = HandoffStore::new(
            self.handoff_ttl,
            self.handoff_capacity,
            Arc::clone(&self.clock),
        )?;
        let cookie = CookieSpec {
            name: if self.secure_cookies {
                SECURE_COOKIE_NAME
            } else {
                INSECURE_COOKIE_NAME
            }
            .to_owned(),
            secure: self.secure_cookies,
        };
        Ok(Sso {
            inner: Arc::new(Inner {
                app_url,
                providers: self.providers,
                hook,
                key,
                cookie,
                state_ttl_secs: self.state_ttl.as_secs(),
                callback_timeout: self.callback_timeout,
                path: self.path,
                default_redirect: self.default_redirect,
                consumed: Mutex::new(Expiring::new(self.handoff_capacity)),
                handoff,
                clock: self.clock,
            }),
        })
    }
}

/// `/a/b`: starts with `/`, no trailing `/`, non-empty segments of RFC 3986
/// unreserved characters.
fn valid_path(path: &str) -> bool {
    path.strip_prefix('/').is_some_and(|rest| {
        rest.split('/').all(|segment| {
            !segment.is_empty()
                && segment
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~'))
        })
    })
}

/// Browser single sign-on: the routes, the login hook and the handoff
/// store.
///
/// ```text
/// GET <path>/<provider>/login?redirect=/orders
///     302 to the provider; sets the signed state cookie
/// GET <path>/<provider>/callback?code=…&state=…
///     302 to <app_url>/orders#sso_code=<one-time code>
///      or <app_url>/orders#sso_error=<SsoErrorCode>
/// ```
///
/// The application's frontend reads the fragment and posts the code to one
/// of its own anonymous endpoints, which calls [`redeem`](Self::redeem) and
/// mints the application's session. Clones share everything.
pub struct Sso<O> {
    inner: Arc<Inner<O>>,
}

impl<O> Clone for Sso<O> {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl<O> fmt::Debug for Sso<O> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Sso")
            .field("app_url", &self.inner.app_url)
            .field(
                "providers",
                &self
                    .inner
                    .providers
                    .iter()
                    .map(|p| p.id())
                    .collect::<Vec<_>>(),
            )
            .field("path", &self.inner.path)
            .finish_non_exhaustive()
    }
}

struct Inner<O> {
    app_url: String,
    providers: Vec<Arc<dyn IdentityProvider>>,
    hook: Arc<dyn LoginHook<O>>,
    key: StateKey,
    cookie: CookieSpec,
    state_ttl_secs: u64,
    callback_timeout: Duration,
    path: String,
    default_redirect: String,
    consumed: Mutex<Expiring<()>>,
    handoff: HandoffStore<O>,
    clock: Arc<dyn Clock>,
}

impl<O: Send + 'static> Sso<O> {
    /// A builder. `app_url` is where browsers return to; `state_key` (at
    /// least 32 bytes) signs the state cookie.
    pub fn builder(app_url: impl Into<String>, state_key: Secret) -> SsoBuilder<O> {
        SsoBuilder {
            app_url: app_url.into(),
            state_key,
            providers: Vec::new(),
            hook: None,
            clock: Arc::new(SystemClock),
            secure_cookies: true,
            state_ttl: DEFAULT_STATE_TTL,
            handoff_ttl: DEFAULT_HANDOFF_TTL,
            handoff_capacity: DEFAULT_HANDOFF_CAPACITY,
            callback_timeout: DEFAULT_CALLBACK_TIMEOUT,
            path: "/sso".to_owned(),
            default_redirect: "/".to_owned(),
        }
    }

    /// A builder with every setting from `config`.
    pub fn from_config(config: SsoConfig) -> SsoBuilder<O> {
        Self::builder(config.app_url, config.state_key)
            .secure_cookies(config.secure_cookies)
            .state_ttl(config.state_ttl)
            .handoff_ttl(config.handoff_ttl)
            .handoff_capacity(config.handoff_capacity)
            .callback_timeout(config.callback_timeout)
    }

    /// The routes: `GET <path>/<id>/login` and `GET <path>/<id>/callback`
    /// for every provider. Mount them with `ServerBuilder::rest`.
    pub fn router(&self) -> Router {
        let mut router = Router::new();
        for provider in &self.inner.providers {
            let base = format!("{}/{}", self.inner.path, provider.id());
            let (inner, login_provider) = (Arc::clone(&self.inner), Arc::clone(provider));
            router = router.route(
                &format!("{base}/login"),
                get(move |request: Request| {
                    let inner = Arc::clone(&inner);
                    let provider = Arc::clone(&login_provider);
                    async move { inner.login(provider.as_ref(), &request) }
                }),
            );
            let (inner, callback_provider) = (Arc::clone(&self.inner), Arc::clone(provider));
            router = router.route(
                &format!("{base}/callback"),
                get(move |request: Request| {
                    let inner = Arc::clone(&inner);
                    let provider = Arc::clone(&callback_provider);
                    async move { inner.callback(provider.as_ref(), request).await }
                }),
            );
        }
        router
    }

    /// The handoff store, e.g. for an endpoint that only redeems.
    pub fn handoff(&self) -> HandoffStore<O> {
        self.inner.handoff.clone()
    }

    /// Take the outcome stored under a handoff code; `None` for an unknown,
    /// expired, already redeemed or malformed code.
    pub fn redeem(&self, code: &str) -> Option<O> {
        self.inner.handoff.redeem(code)
    }
}

/// The single value of `name` in a query; `Err` when it is repeated.
fn single<'a>(
    pairs: &'a [(Cow<'a, str>, Cow<'a, str>)],
    name: &str,
) -> Result<Option<&'a str>, ()> {
    let mut found = pairs
        .iter()
        .filter(|(key, _)| key == name)
        .map(|(_, v)| v.as_ref());
    let first = found.next();
    if found.next().is_some() {
        return Err(());
    }
    Ok(first)
}

/// Where a finished request sends the browser.
enum Outcome {
    Code(Secret),
    Error(SsoErrorCode),
}

impl<O: Send + 'static> Inner<O> {
    fn now_secs(&self) -> u64 {
        self.clock.now_unix_millis() / 1000
    }

    fn login(&self, provider: &dyn IdentityProvider, request: &Request) -> Response {
        let pairs: Vec<_> =
            form_urlencoded::parse(request.uri().query().unwrap_or("").as_bytes()).collect();
        let redirect = match single(&pairs, "redirect") {
            Ok(None) => self.default_redirect.clone(),
            Ok(Some(path)) if is_safe_redirect(path) => path.to_owned(),
            _ => {
                tracing::info!(
                    target: "sekvent::sso",
                    provider = provider.id(),
                    "sso login refused: unsafe or repeated redirect"
                );
                return self.finish(
                    &self.default_redirect,
                    &Outcome::Error(SsoErrorCode::InvalidRequest),
                    false,
                );
            }
        };
        match self.start(provider, redirect) {
            Ok((location, cookie)) => redirect_response(&location, Some(&cookie)),
            Err(error) => {
                log_failure(provider.id(), "login", &error);
                self.finish(
                    &self.default_redirect,
                    &Outcome::Error(SsoErrorCode::ServerError),
                    false,
                )
            }
        }
    }

    /// The provider URL and the `Set-Cookie` value for a new sign-in.
    fn start(
        &self,
        provider: &dyn IdentityProvider,
        redirect: String,
    ) -> Result<(String, String), AppError> {
        let state = random_token()?;
        let verifier = random_token()?;
        let location = provider.authorization_url(&AuthorizationRequest::new(
            &state,
            &code_challenge(&verifier),
        ))?;
        let flow = FlowState {
            provider: provider.id().to_owned(),
            state,
            verifier,
            redirect,
            expires_at: self.now_secs().saturating_add(self.state_ttl_secs),
        };
        let cookie = self.cookie.set(&self.key.seal(&flow), self.state_ttl_secs);
        Ok((location, cookie))
    }

    async fn callback(&self, provider: &dyn IdentityProvider, request: Request) -> Response {
        let ctx = request
            .extensions()
            .get::<CallContext>()
            .cloned()
            .unwrap_or_default()
            .with_timeout(self.callback_timeout);
        let query = request.uri().query().unwrap_or("").to_owned();
        let pairs: Vec<_> = form_urlencoded::parse(query.as_bytes()).collect();
        let flow = match self.verify_state(provider.id(), request.headers(), &pairs) {
            Ok(flow) => flow,
            Err(rejected) => {
                tracing::info!(
                    target: "sekvent::sso",
                    provider = provider.id(),
                    reason = %rejected,
                    "sso callback refused"
                );
                return self.finish(
                    &self.default_redirect,
                    &Outcome::Error(SsoErrorCode::InvalidState),
                    true,
                );
            }
        };
        let provider_error = single(&pairs, "error");
        if provider_error != Ok(None) {
            let error = error_token(provider_error.ok().flatten().unwrap_or_default());
            let code = SsoErrorCode::for_provider_error(error);
            tracing::info!(
                target: "sekvent::sso",
                provider = provider.id(),
                provider_error = error,
                "sso sign-in refused at the provider"
            );
            return self.finish(&flow.redirect, &Outcome::Error(code), true);
        }
        let Ok(Some(code)) = single(&pairs, "code") else {
            tracing::info!(
                target: "sekvent::sso",
                provider = provider.id(),
                "sso callback without a single code"
            );
            return self.finish(
                &flow.redirect,
                &Outcome::Error(SsoErrorCode::InvalidRequest),
                true,
            );
        };
        let outcome = match self.complete(provider, &ctx, code, &flow.verifier).await {
            Ok(handoff) => Outcome::Code(handoff),
            Err((step, error)) => {
                let code = SsoErrorCode::for_error(&error);
                if code == SsoErrorCode::AccessDenied {
                    tracing::info!(
                        target: "sekvent::sso",
                        provider = provider.id(),
                        step,
                        reason = error.reason(),
                        "sso sign-in denied"
                    );
                } else {
                    log_failure(provider.id(), step, &error);
                }
                Outcome::Error(code)
            }
        };
        self.finish(&flow.redirect, &outcome, true)
    }

    /// Check the state cookie against the route's provider and the
    /// callback's `state`, and mark the state used.
    fn verify_state(
        &self,
        provider: &str,
        headers: &http::HeaderMap,
        pairs: &[(Cow<'_, str>, Cow<'_, str>)],
    ) -> Result<FlowState, StateRejected> {
        let now = self.now_secs();
        let mut first_error = StateRejected::Missing;
        let mut opened = None;
        for (index, value) in cookie_values(headers, &self.cookie.name)
            .into_iter()
            .enumerate()
        {
            match self.key.open(value, now) {
                Ok(flow) => {
                    opened = Some(flow);
                    break;
                }
                Err(rejected) if index == 0 => first_error = rejected,
                Err(_) => {}
            }
        }
        let flow = opened.ok_or(first_error)?;
        if flow.provider != provider {
            return Err(StateRejected::WrongProvider);
        }
        let state = single(pairs, "state")
            .ok()
            .flatten()
            .ok_or(StateRejected::Mismatch)?;
        if !bool::from(state.as_bytes().ct_eq(flow.state.as_bytes())) {
            return Err(StateRejected::Mismatch);
        }
        let key = digest(&flow.state);
        let now_ms = self.clock.now_unix_millis();
        let mut consumed = self.consumed.lock().unwrap_or_else(PoisonError::into_inner);
        consumed.sweep(now_ms);
        if consumed.contains(&key, now_ms) {
            return Err(StateRejected::Replayed);
        }
        consumed.insert_evicting(key, (), flow.expires_at.saturating_mul(1000));
        Ok(flow)
    }

    /// Exchange, fetch the identity, run the hook and issue a handoff code.
    /// The error names the step that failed.
    async fn complete(
        &self,
        provider: &dyn IdentityProvider,
        ctx: &CallContext,
        code: &str,
        verifier: &str,
    ) -> Result<Secret, (&'static str, AppError)> {
        let tokens = provider
            .exchange_code(ctx, CodeExchange::new(code, verifier))
            .await
            .map_err(|error| ("token exchange", error))?;
        let identity = provider
            .identity(ctx, &tokens)
            .await
            .map_err(|error| ("identity", error))?;
        drop(tokens);
        let outcome = self
            .hook
            .on_login(ctx.clone(), identity)
            .await
            .map_err(|error| ("login hook", error))?;
        self.handoff
            .issue(outcome)
            .map_err(|error| ("handoff", error))
    }

    /// Redirect to the application with the outcome in the fragment,
    /// clearing the state cookie when asked.
    fn finish(&self, redirect: &str, outcome: &Outcome, clear_cookie: bool) -> Response {
        let fragment = match outcome {
            Outcome::Code(code) => format!("{HANDOFF_PARAM}={}", code.expose()),
            Outcome::Error(error) => format!("{ERROR_PARAM}={error}"),
        };
        let location = format!("{}{redirect}#{fragment}", self.app_url);
        let clear = clear_cookie.then(|| self.cookie.clear());
        redirect_response(&location, clear.as_deref())
    }
}

/// The provider's `error` parameter if it is a plain token, for logs.
fn error_token(error: &str) -> &str {
    if !error.is_empty()
        && error.len() <= 64
        && error.bytes().all(|b| b.is_ascii_lowercase() || b == b'_')
    {
        error
    } else {
        "other"
    }
}

/// One `warn` event for a server-side failure: step, code, reason and the
/// source chain (upstream sources carry no bodies or URLs).
fn log_failure(provider: &str, step: &str, error: &AppError) {
    let mut source = String::new();
    let mut next = StdError::source(error);
    while let Some(cause) = next {
        if !source.is_empty() {
            source.push_str(": ");
        }
        let _ = write!(source, "{cause}");
        next = cause.source();
    }
    tracing::warn!(
        target: "sekvent::sso",
        provider,
        step,
        code = error.code().as_str(),
        reason = error.reason(),
        source = source.as_str(),
        "sso sign-in failed"
    );
}

/// `302 Found` to `location`, never cached, never leaking the URL as a
/// referrer.
fn redirect_response(location: &str, set_cookie: Option<&str>) -> Response {
    let Ok(location) = HeaderValue::try_from(location) else {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    };
    let mut response = StatusCode::FOUND.into_response();
    let headers = response.headers_mut();
    headers.insert(header::LOCATION, location);
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    if let Some(cookie) = set_cookie.and_then(|c| HeaderValue::try_from(c).ok()) {
        headers.insert(header::SET_COOKIE, cookie);
    }
    response
}
