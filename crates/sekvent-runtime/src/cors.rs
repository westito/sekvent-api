//! Cross-origin rules for browsers calling the server.

use std::fmt;
use std::fmt::Write as _;
use std::net::{Ipv4Addr, Ipv6Addr};
use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::State;
use axum::middleware::{Next, from_fn_with_state};
use axum::response::Response;
use http::header::{
    ACCESS_CONTROL_ALLOW_CREDENTIALS, ACCESS_CONTROL_ALLOW_HEADERS, ACCESS_CONTROL_ALLOW_METHODS,
    ACCESS_CONTROL_ALLOW_ORIGIN, ACCESS_CONTROL_EXPOSE_HEADERS, ACCESS_CONTROL_MAX_AGE,
    ACCESS_CONTROL_REQUEST_METHOD, ORIGIN, VARY,
};
use http::request::Parts;
use http::{HeaderMap, HeaderName, HeaderValue, Method, Request};
use sekvent_config::{ConfigError, ConfigSource};
use sekvent_error::AppError;
use sekvent_telemetry::truncate_for_log;

/// Request headers a browser may send unless more are added.
const DEFAULT_ALLOW_HEADERS: [&str; 8] = [
    "authorization",
    "content-type",
    "grpc-timeout",
    "idempotency-key",
    "traceparent",
    "x-grpc-web",
    "x-request-id",
    "x-user-agent",
];

/// Response headers a browser script may read unless more are added.
const DEFAULT_EXPOSE_HEADERS: [&str; 5] = [
    "content-disposition",
    "grpc-message",
    "grpc-status",
    "grpc-status-details-bin",
    "x-request-id",
];

/// How long a browser may cache a preflight answer by default.
const DEFAULT_MAX_AGE: Duration = Duration::from_secs(3600);

/// Key suffixes [`Cors::from_config`] reads.
const KEYS: [&str; 5] = [
    "ORIGINS",
    "CREDENTIALS",
    "MAX_AGE",
    "ALLOW_HEADERS",
    "EXPOSE_HEADERS",
];

/// Longest piece of a rejected value quoted in an error.
const MAX_QUOTED: usize = 200;

/// Cross-origin rules for browsers calling the server (REST and gRPC-Web).
///
/// Built with [`any_origin`](Self::any_origin) or a validated, normalized
/// origin list ([`origins`](Self::origins), whose `http(s)` entries may leave
/// the port open with `:*` and whose custom-scheme entries, such as
/// `capacitor://localhost`, match exactly), a function deciding each origin
/// ([`origin_predicate`](Self::origin_predicate), also next to a list with
/// [`or_origin_predicate`](Self::or_origin_predicate)), or read from
/// configuration ([`from_config`](Self::from_config)), then handed to
/// [`ServerBuilder::cors`](crate::ServerBuilder::cors).
///
/// | Setting | Default |
/// |---|---|
/// | methods | `GET`, `HEAD`, `POST`, `PUT`, `PATCH`, `DELETE` |
/// | request headers | `authorization`, `content-type`, `grpc-timeout`, `idempotency-key`, `traceparent`, `x-grpc-web`, `x-request-id`, `x-user-agent` |
/// | exposed headers | `content-disposition`, `grpc-message`, `grpc-status`, `grpc-status-details-bin`, `x-request-id` |
/// | max age | 1 h |
/// | credentials | off |
///
/// A preflight (`OPTIONS` with `Origin` and `Access-Control-Request-Method`)
/// is answered by the server itself with `200`, before authentication and
/// handlers; any other `OPTIONS` request reaches the application. A request
/// without `Origin`, or from an origin that is not allowed, gets no
/// `access-control-*` headers at all, so the browser keeps the page from
/// reading the answer; the request itself is still served. A list or a
/// function answers an allowed origin with that origin (never `*`) and adds
/// `Vary: Origin` to every response.
///
/// These rules are authoritative: `access-control-*` headers set by
/// application layers or handlers are removed before the server adds its
/// own, so an inner layer can never widen them.
///
/// Two values are equal when their rules are; a function equals only
/// itself and its clones.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cors {
    origins: Origins,
    /// Decides the origins the list does not hold; always `None` under
    /// `Origins::Any`, which never consults it.
    predicate: Option<OriginPredicate>,
    credentials: bool,
    methods: Vec<Method>,
    allow_headers: Vec<String>,
    expose_headers: Vec<String>,
    max_age: Duration,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Origins {
    Any,
    List(Vec<String>),
}

/// The signature of an origin function.
type OriginFn = dyn Fn(&str, &Parts) -> bool + Send + Sync;

/// An origin function, compared by identity so [`Cors`] keeps its derives.
#[derive(Clone)]
struct OriginPredicate(Arc<OriginFn>);

impl OriginPredicate {
    /// Whether `allowed` admits `origin`. `null` (in any case) and an
    /// `Origin` that is not visible ASCII never reach it; a panic in it
    /// counts as "not allowed".
    fn allows(&self, origin: &HeaderValue, head: &Parts) -> bool {
        let Ok(text) = origin.to_str() else {
            return false;
        };
        if text.trim().eq_ignore_ascii_case("null") {
            return false;
        }
        if let Ok(allowed) = std::panic::catch_unwind(AssertUnwindSafe(|| (self.0)(text, head))) {
            allowed
        } else {
            tracing::error!("the CORS origin function panicked; the origin is not allowed");
            false
        }
    }
}

impl fmt::Debug for OriginPredicate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("OriginPredicate(..)")
    }
}

impl PartialEq for OriginPredicate {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl Eq for OriginPredicate {}
impl Cors {
    /// Every origin (`Access-Control-Allow-Origin: *`).
    pub fn any_origin() -> Self {
        Self::with_origins(Origins::Any)
    }

    /// Exactly these origins, normalized to the form browsers send:
    /// lowercase scheme and host, no default port, no trailing slash, an
    /// IPv4 host in dotted decimal and an IPv6 host in its shortest form
    /// (`http://[0:0::1]` becomes `http://[::1]`; an IPv4-mapped address is
    /// written in hex, `[::ffff:102:304]`, as browsers do).
    ///
    /// Each entry is a scheme, `://`, a host and an optional port.
    /// For `http` and `https` the port `*` (`http://localhost:*`) matches
    /// that scheme and host on any port, the default one (no port in
    /// `Origin`) included; for a development server that picks a fresh port
    /// on every run. The host is always exact: there are no host wildcards.
    ///
    /// Any other scheme is a custom one, such as `capacitor://localhost`, the
    /// origin of an app served from a mobile web view. Such an entry matches
    /// one `Origin` exactly, ignoring ASCII case (`Capacitor://LocalHost`
    /// matches); it has no default port and never takes the port `*`.
    ///
    /// `INVALID_ARGUMENT` for a path, query, fragment or user info, `*` (use
    /// [`any_origin`](Self::any_origin)), `null`, the schemes `file`,
    /// `data`, `blob`, `javascript` and `about` (their documents send
    /// `null`) and `ftp`, `ws` and `wss` (they serve no page), a malformed
    /// scheme, host or port (a `*` in the host, a port such as `*x`, or `*`
    /// on a custom scheme), and for an empty list. The error names the entry
    /// by its position, counting from 1 (`origin #2`), never by its text,
    /// which may hold a credential.
    pub fn origins<I, S>(origins: I) -> Result<Self, AppError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut list = Vec::new();
        for (index, origin) in origins.into_iter().enumerate() {
            push_origin(&mut list, index + 1, origin.as_ref())
                .map_err(|reason| AppError::invalid_argument(format!("the CORS {reason}")))?;
        }
        if list.is_empty() {
            return Err(AppError::invalid_argument(NO_ORIGIN));
        }
        Ok(Self::with_origins(Origins::List(list)))
    }

    /// Origins decided per request by `allowed`, with no fixed list.
    ///
    /// `allowed` gets the `Origin` header's text as the browser sent it (not
    /// normalized) and the head of the request it came on: for a preflight
    /// the `OPTIONS` request with its `Access-Control-Request-*` headers.
    /// The head carries the TCP peer (`ConnectInfo<SocketAddr>`, when the
    /// server's own listener serves it) but no `CallContext`, and its path
    /// still has the server's prefix. The
    /// preflight and the request are decided alike, each by its own call.
    /// An HTTP/2 request names its host in the URI authority
    /// (`head.uri.authority()`), not in `Host`: read the URI first, then
    /// `Host`.
    ///
    /// A request without `Origin`, an `Origin` that is not visible ASCII
    /// and `null` (in any case) never reach `allowed`; the last two are not
    /// allowed. A panic in `allowed` counts as "not allowed" and is logged
    /// at `error`, without the origin, but the process panic hook (stderr
    /// by default) still prints the panic message: never panic with the
    /// origin or a secret in it, and never on every call, which turns each
    /// anonymous request into a stderr dump. An allowed origin is echoed
    /// (never `*`), every answer carries `Vary: Origin`, and
    /// [`allow_credentials`](Self::allow_credentials) is accepted: the
    /// function is then the only guard.
    ///
    /// Responses vary on `Origin` only. When `allowed` decides on anything
    /// else in the head (a cookie, the language, the peer address), make
    /// those responses uncacheable or add a `Vary` for what it reads.
    ///
    /// `allowed` runs for every cross-origin request and preflight, before
    /// authentication, so it must be cheap: compare the whole origin with
    /// known values (an in-memory snapshot refreshed out of band), never
    /// `ends_with` or `contains`, which allow look-alike hosts. CORS never
    /// stops a request from executing; it only decides what a browser page
    /// may read.
    pub fn origin_predicate<F>(allowed: F) -> Self
    where
        F: Fn(&str, &Parts) -> bool + Send + Sync + 'static,
    {
        Self::with_origins(Origins::List(Vec::new())).or_origin_predicate(allowed)
    }

    /// Also allow the origins `allowed` accepts, next to this list. A
    /// listed origin never reaches `allowed`. Under
    /// [`any_origin`](Self::any_origin) it would never be called, so it is
    /// dropped at once with whatever it captures, and the rules stay equal
    /// to plain `any_origin`. A later call replaces the function. See
    /// [`origin_predicate`](Self::origin_predicate) for what it gets.
    #[must_use]
    pub fn or_origin_predicate<F>(mut self, allowed: F) -> Self
    where
        F: Fn(&str, &Parts) -> bool + Send + Sync + 'static,
    {
        self.predicate = match self.origins {
            Origins::Any => None,
            Origins::List(_) => Some(OriginPredicate(Arc::new(allowed))),
        };
        self
    }

    /// Read the rules from `<prefix>ORIGINS` (`*` or a comma-separated
    /// list, entries as in [`origins`](Self::origins), `:*` ports and
    /// custom schemes included),
    /// `<prefix>CREDENTIALS` (bool, default false), `<prefix>MAX_AGE`
    /// (duration, default `1h`), `<prefix>ALLOW_HEADERS` and
    /// `<prefix>EXPOSE_HEADERS` (comma-separated, added to the defaults).
    ///
    /// `Ok(None)` when `<prefix>ORIGINS` is unset or blank: CORS stays off.
    /// Every problem is reported at once, each naming its key; `*` with
    /// credentials is invalid under `<prefix>CREDENTIALS`. An invalid origin
    /// is named by its position in the list, counting from 1 (blank entries
    /// count too), never by its text.
    pub fn from_config(
        source: &dyn ConfigSource,
        prefix: &str,
    ) -> Result<Option<Self>, ConfigError> {
        let key = |suffix: &str| format!("{prefix}{suffix}");
        let origins_key = key("ORIGINS");
        let Some(raw) = source
            .get(&origins_key)
            .filter(|raw| !raw.trim().is_empty())
        else {
            return Ok(None);
        };
        let mut errors = Vec::new();
        let any = raw.trim() == "*";
        let base = if any {
            Some(Self::any_origin())
        } else {
            take(
                &mut errors,
                config_origins(&raw)
                    .map(Self::with_origins)
                    .map_err(|reason| ConfigError::Invalid {
                        key: source.describe(&origins_key),
                        reason,
                    }),
            )
        };
        let credentials = take(
            &mut errors,
            sekvent_config::opt_bool(source, &key("CREDENTIALS"), false),
        )
        .unwrap_or(false);
        let max_age = take(
            &mut errors,
            sekvent_config::opt_duration(source, &key("MAX_AGE"), DEFAULT_MAX_AGE),
        )
        .unwrap_or(DEFAULT_MAX_AGE);
        let allow = header_list(source, &key("ALLOW_HEADERS"), &mut errors);
        let expose = header_list(source, &key("EXPOSE_HEADERS"), &mut errors);
        if credentials && any {
            errors.push(ConfigError::Invalid {
                key: source.describe(&key("CREDENTIALS")),
                reason: format!(
                    "CORS credentials need an explicit origin list in {origins_key}, not *"
                ),
            });
        }
        sekvent_config::collect(errors.into_iter().map(Err))?;
        let allow: Vec<&str> = allow.iter().map(String::as_str).collect();
        let expose: Vec<&str> = expose.iter().map(String::as_str).collect();
        // `base` is set whenever no error was recorded.
        let cors = base
            .unwrap_or_else(Self::any_origin)
            .allow_credentials(credentials)
            .max_age(max_age)
            .allow_headers(&allow)
            .expose_headers(&expose);
        Ok(Some(cors))
    }

    /// Every key [`from_config`](Self::from_config) reads under `prefix`.
    pub fn config_keys(prefix: &str) -> Vec<String> {
        KEYS.iter()
            .map(|suffix| format!("{prefix}{suffix}"))
            .collect()
    }

    /// Whether browsers may send cookies and `Authorization` (default off).
    /// Needs an origin list or an origin function.
    #[must_use]
    pub fn allow_credentials(mut self, allowed: bool) -> Self {
        self.credentials = allowed;
        self
    }

    /// Replace the default methods.
    #[must_use]
    pub fn methods(mut self, methods: &[Method]) -> Self {
        self.methods.clear();
        for method in methods {
            if !self.methods.contains(method) {
                self.methods.push(method.clone());
            }
        }
        self
    }

    /// Add to the default request headers.
    #[must_use]
    pub fn allow_headers(mut self, names: &[&str]) -> Self {
        add_names(&mut self.allow_headers, names.iter().copied());
        self
    }

    /// Add to the default exposed headers.
    #[must_use]
    pub fn expose_headers(mut self, names: &[&str]) -> Self {
        add_names(&mut self.expose_headers, names.iter().copied());
        self
    }

    /// How long a browser may cache a preflight answer (default 1 h).
    #[must_use]
    pub fn max_age(mut self, max_age: Duration) -> Self {
        self.max_age = max_age;
        self
    }

    /// The checks [`ServerBuilder::bind`](crate::ServerBuilder::bind) runs,
    /// each `INVALID_ARGUMENT`: credentials with
    /// [`any_origin`](Self::any_origin), no method, the method `*`, and an
    /// invalid or `*` header name.
    pub fn validate(&self) -> Result<(), AppError> {
        if self.credentials && self.origins == Origins::Any {
            return Err(AppError::invalid_argument(
                "CORS credentials need an explicit origin list",
            ));
        }
        if self.methods.is_empty() {
            return Err(AppError::invalid_argument(
                "CORS needs at least one allowed method",
            ));
        }
        if self.methods.iter().any(|method| method.as_str() == "*") {
            return Err(AppError::invalid_argument(
                "the CORS method * is not allowed; list the methods",
            ));
        }
        if let Some(name) = self
            .allow_headers
            .iter()
            .chain(&self.expose_headers)
            .find(|name| !is_header_name(name))
        {
            return Err(AppError::invalid_argument(format!(
                "the CORS header name {:?} is invalid",
                truncate_for_log(name, MAX_QUOTED)
            )));
        }
        Ok(())
    }

    /// Wrap `router` in the CORS layer. Call only after
    /// [`validate`](Self::validate).
    pub(crate) fn apply(&self, router: Router) -> Router {
        router.layer(from_fn_with_state(Arc::new(Policy::new(self)), answer))
    }

    fn with_origins(origins: Origins) -> Self {
        Self {
            origins,
            predicate: None,
            credentials: false,
            methods: vec![
                Method::GET,
                Method::HEAD,
                Method::POST,
                Method::PUT,
                Method::PATCH,
                Method::DELETE,
            ],
            allow_headers: DEFAULT_ALLOW_HEADERS
                .into_iter()
                .map(str::to_owned)
                .collect(),
            expose_headers: DEFAULT_EXPOSE_HEADERS
                .into_iter()
                .map(str::to_owned)
                .collect(),
            max_age: DEFAULT_MAX_AGE,
        }
    }
}

const NO_ORIGIN: &str = "CORS needs at least one origin";

/// The origins of a configured list; the error names an entry by its
/// position among the comma-separated items, counting from 1.
fn config_origins(raw: &str) -> Result<Origins, String> {
    let mut list = Vec::new();
    for (index, entry) in raw.split(',').enumerate() {
        if !entry.trim().is_empty() {
            push_origin(&mut list, index + 1, entry)?;
        }
    }
    if list.is_empty() {
        return Err(NO_ORIGIN.to_owned());
    }
    Ok(Origins::List(list))
}

/// Normalize `raw` and add it unless already listed. The error names the
/// entry by `position` only.
fn push_origin(list: &mut Vec<String>, position: usize, raw: &str) -> Result<(), String> {
    let normalized = normalize_origin(raw)
        .map_err(|reason| format!("origin #{position} is invalid: {reason}"))?;
    if !list.contains(&normalized) {
        list.push(normalized);
    }
    Ok(())
}

/// Port of a normalized origin entry that matches every port.
const ANY_PORT: &str = ":*";

/// The rules of a validated [`Cors`] as ready-made header values.
#[derive(Debug)]
struct Policy {
    /// `None` for every origin, else the normalized list.
    origins: Option<OriginList>,
    /// Asked about an origin the list does not hold.
    predicate: Option<OriginPredicate>,
    credentials: bool,
    methods: Option<HeaderValue>,
    allow_headers: Option<HeaderValue>,
    expose_headers: Option<HeaderValue>,
    max_age: HeaderValue,
}

/// A normalized origin list, split for matching.
#[derive(Debug, Default)]
struct OriginList {
    /// Origins matched byte for byte.
    exact: Vec<HeaderValue>,
    /// `scheme://host` of the entries with the port `*`.
    any_port: Vec<String>,
    /// Custom-scheme origins, lowercase, matched ignoring ASCII case: a web
    /// view may keep the case of a non-special URL's host.
    custom: Vec<String>,
}

impl OriginList {
    fn new(list: &[String]) -> Self {
        let mut split = Self::default();
        for origin in list {
            if !is_web_origin(origin) {
                split.custom.push(origin.clone());
            } else if let Some(base) = origin.strip_suffix(ANY_PORT) {
                split.any_port.push(base.to_owned());
            } else if let Ok(value) = HeaderValue::from_str(origin) {
                split.exact.push(value);
            }
        }
        split
    }

    /// Whether a request's `Origin` is listed.
    fn matches(&self, origin: &HeaderValue) -> bool {
        let origin = origin.as_bytes();
        self.exact.iter().any(|listed| listed.as_bytes() == origin)
            || self.any_port.iter().any(|base| on_any_port(base, origin))
            || self
                .custom
                .iter()
                .any(|listed| listed.as_bytes().eq_ignore_ascii_case(origin))
    }
}

/// Whether `origin` is `base` itself (the default port) or `base:<port>`
/// with a port written as browsers write it.
fn on_any_port(base: &str, origin: &[u8]) -> bool {
    match origin.strip_prefix(base.as_bytes()) {
        Some([]) => true,
        Some([b':', port @ ..]) => std::str::from_utf8(port)
            .ok()
            .and_then(|port| parse_port(port).ok().map(|parsed| (port, parsed)))
            .is_some_and(|(port, parsed)| parsed.to_string() == port),
        _ => false,
    }
}

impl Policy {
    fn new(cors: &Cors) -> Self {
        let origins = match &cors.origins {
            Origins::Any => None,
            Origins::List(list) => Some(OriginList::new(list)),
        };
        Self {
            origins,
            predicate: cors.predicate.clone(),
            credentials: cors.credentials,
            methods: joined(cors.methods.iter().map(Method::as_str)),
            allow_headers: joined(cors.allow_headers.iter().map(String::as_str)),
            expose_headers: joined(cors.expose_headers.iter().map(String::as_str)),
            max_age: HeaderValue::from(cors.max_age.as_secs()),
        }
    }

    /// The `Access-Control-Allow-Origin` value for the `Origin` of the
    /// request with head `head`, or `None` when the request may not read the
    /// answer. The list is asked first, then the function.
    fn allow_origin(&self, head: &Parts) -> Option<HeaderValue> {
        let origin = head.headers.get(ORIGIN)?;
        let Some(list) = &self.origins else {
            return Some(HeaderValue::from_static("*"));
        };
        let allowed = list.matches(origin)
            || self
                .predicate
                .as_ref()
                .is_some_and(|predicate| predicate.allows(origin, head));
        allowed.then(|| origin.clone())
    }

    /// The answer to a preflight.
    fn preflight(&self, allowed: Option<HeaderValue>) -> Response {
        let mut response = Response::new(Body::empty());
        let headers = response.headers_mut();
        if let Some(origin) = allowed {
            self.allow(headers, origin);
            if let Some(methods) = &self.methods {
                headers.insert(ACCESS_CONTROL_ALLOW_METHODS, methods.clone());
            }
            if let Some(names) = &self.allow_headers {
                headers.insert(ACCESS_CONTROL_ALLOW_HEADERS, names.clone());
            }
            headers.insert(ACCESS_CONTROL_MAX_AGE, self.max_age.clone());
        }
        self.vary(headers);
        response
    }

    /// Replace whatever CORS headers the application set with this policy's.
    fn decorate(&self, headers: &mut HeaderMap, allowed: Option<HeaderValue>) {
        strip_cors_headers(headers, self.origins.is_some());
        if let Some(origin) = allowed {
            self.allow(headers, origin);
            if let Some(names) = &self.expose_headers {
                headers.insert(ACCESS_CONTROL_EXPOSE_HEADERS, names.clone());
            }
        }
        self.vary(headers);
    }

    fn allow(&self, headers: &mut HeaderMap, origin: HeaderValue) {
        headers.insert(ACCESS_CONTROL_ALLOW_ORIGIN, origin);
        if self.credentials {
            headers.insert(
                ACCESS_CONTROL_ALLOW_CREDENTIALS,
                HeaderValue::from_static("true"),
            );
        }
    }

    /// A list answers differently per origin, so caches must key on it.
    fn vary(&self, headers: &mut HeaderMap) {
        if self.origins.is_some() {
            headers.append(VARY, HeaderValue::from_static("origin"));
        }
    }
}

/// The CORS middleware: preflights are answered here, everything else
/// passes through and gets this policy's headers.
async fn answer(State(policy): State<Arc<Policy>>, request: Request<Body>, next: Next) -> Response {
    let (head, body) = request.into_parts();
    let allowed = policy.allow_origin(&head);
    if is_preflight(&head) {
        return policy.preflight(allowed);
    }
    let mut response = next.run(Request::from_parts(head, body)).await;
    policy.decorate(response.headers_mut(), allowed);
    response
}

fn is_preflight(head: &Parts) -> bool {
    head.method == Method::OPTIONS
        && head.headers.contains_key(ORIGIN)
        && head.headers.contains_key(ACCESS_CONTROL_REQUEST_METHOD)
}

/// Remove every `access-control-*` header and the CORS entries of `Vary`
/// (`access-control-request-*`, and `origin` when the policy adds its own).
fn strip_cors_headers(headers: &mut HeaderMap, origin_too: bool) {
    let names: Vec<HeaderName> = headers
        .keys()
        .filter(|name| name.as_str().starts_with("access-control-"))
        .cloned()
        .collect();
    for name in names {
        headers.remove(name);
    }
    let vary: Vec<HeaderValue> = headers.get_all(VARY).iter().cloned().collect();
    if vary.is_empty() {
        return;
    }
    headers.remove(VARY);
    for value in vary {
        let Ok(text) = value.to_str() else {
            headers.append(VARY, value);
            continue;
        };
        let kept: Vec<&str> = text
            .split(',')
            .map(str::trim)
            .filter(|token| {
                let dropped = token.is_empty()
                    || token.eq_ignore_ascii_case("access-control-request-method")
                    || token.eq_ignore_ascii_case("access-control-request-headers")
                    || (origin_too && token.eq_ignore_ascii_case("origin"));
                !dropped
            })
            .collect();
        if !kept.is_empty()
            && let Ok(value) = HeaderValue::from_str(&kept.join(", "))
        {
            headers.append(VARY, value);
        }
    }
}

/// `items` joined with commas, as one header value.
fn joined<'a>(items: impl Iterator<Item = &'a str>) -> Option<HeaderValue> {
    let text = items.collect::<Vec<_>>().join(",");
    HeaderValue::from_str(&text).ok()
}

/// Schemes whose documents have an opaque origin: a browser sends
/// `Origin: null` for them, never their own text.
const OPAQUE_SCHEMES: [&str; 5] = ["about", "blob", "data", "file", "javascript"];

/// Special schemes that never serve a page, so never send an `Origin`.
const NON_PAGE_SCHEMES: [&str; 3] = ["ftp", "ws", "wss"];

const NO_SCHEME: &str = "an origin is a scheme followed by :// and a host";
const OPAQUE: &str = "file, data, blob, javascript and about URLs have the opaque origin null, which is never allowed";
const NON_PAGE: &str = "ftp, ws and wss URLs are never a page's origin; use http or https";
const CUSTOM_ANY_PORT: &str = "only http and https origins may leave the port open with *";

/// The canonical form of one origin entry. `http` and `https` drop their
/// default port and may end in `:*`; any other scheme (`capacitor://localhost`
/// for an app's web view) is kept as written, lowercased, with an explicit
/// port only.
fn normalize_origin(raw: &str) -> Result<String, &'static str> {
    let origin = raw.trim();
    if origin == "*" {
        return Err("* is only allowed alone (Cors::any_origin)");
    }
    if origin.eq_ignore_ascii_case("null") {
        return Err("the opaque origin null is never allowed");
    }
    let Some((scheme, rest)) = origin
        .split_once(':')
        .filter(|(scheme, _)| is_scheme(scheme))
    else {
        return Err(NO_SCHEME);
    };
    let scheme = scheme.to_ascii_lowercase();
    if OPAQUE_SCHEMES.contains(&scheme.as_str()) {
        return Err(OPAQUE);
    }
    if NON_PAGE_SCHEMES.contains(&scheme.as_str()) {
        return Err(NON_PAGE);
    }
    let Some(rest) = rest.strip_prefix("//") else {
        return Err(NO_SCHEME);
    };
    let default_port = match scheme.as_str() {
        "http" => Some(80),
        "https" => Some(443),
        _ => None,
    };
    let authority = rest.strip_suffix('/').unwrap_or(rest);
    if authority.contains(['/', '?', '#']) {
        return Err("an origin has no path, query or fragment");
    }
    if authority.contains('@') {
        return Err("an origin has no user info");
    }
    let (host, port) = split_host_port(authority)?;
    if port == Some("*") {
        if default_port.is_none() {
            return Err(CUSTOM_ANY_PORT);
        }
        return Ok(format!("{scheme}://{host}{ANY_PORT}"));
    }
    let port = port.map(parse_port).transpose()?;
    Ok(match port.filter(|&port| Some(port) != default_port) {
        Some(port) => format!("{scheme}://{host}:{port}"),
        None => format!("{scheme}://{host}"),
    })
}

/// Whether `scheme` is a URL scheme: a letter, then letters, digits, `+`,
/// `-` and `.`.
fn is_scheme(scheme: &str) -> bool {
    let mut chars = scheme.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphabetic())
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
}

/// Whether a normalized entry is `http` or `https`, which browsers send in
/// canonical form and are matched byte for byte.
fn is_web_origin(origin: &str) -> bool {
    origin.starts_with("http://") || origin.starts_with("https://")
}

/// The lowercase host (IPv6 in brackets) and the raw port text, if any.
fn split_host_port(authority: &str) -> Result<(String, Option<&str>), &'static str> {
    if let Some(bracketed) = authority.strip_prefix('[') {
        let Some((inside, tail)) = bracketed.split_once(']') else {
            return Err("an IPv6 host needs a closing bracket");
        };
        let address: Ipv6Addr = inside.parse().map_err(|_| "the IPv6 host is malformed")?;
        let port = if tail.is_empty() {
            None
        } else {
            Some(
                tail.strip_prefix(':')
                    .ok_or("only a port may follow an IPv6 host")?,
            )
        };
        return Ok((ipv6_host(address), port));
    }
    let (host, port) = match authority.rsplit_once(':') {
        Some((host, port)) => (host, Some(port)),
        None => (authority, None),
    };
    if host.is_empty() {
        return Err("an origin needs a host");
    }
    if host.contains('*') {
        return Err("the host must be exact; only the port may be *");
    }
    let valid = host.split('.').all(|label| {
        !label.is_empty() && label.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
    });
    if !valid {
        return Err(
            "the host may only hold letters, digits, hyphens and dots (punycode for international names)",
        );
    }
    let host = host.to_ascii_lowercase();
    if ends_in_number(&host) {
        let address: Ipv4Addr = host
            .parse()
            .map_err(|_| "a numeric host must be an IPv4 address in dotted decimal")?;
        return Ok((address.to_string(), port));
    }
    Ok((host, port))
}

/// Whether browsers would read `host` as an IPv4 address: its last label is
/// a decimal or `0x` hex number.
fn ends_in_number(host: &str) -> bool {
    let last = host.rsplit('.').next().unwrap_or(host);
    last.bytes().all(|b| b.is_ascii_digit())
        || last
            .strip_prefix("0x")
            .is_some_and(|hex| hex.bytes().all(|b| b.is_ascii_hexdigit()))
}

/// `address` in brackets as browsers serialize it: lowercase hex pieces
/// without leading zeros, the first longest run of two or more zero pieces
/// written as `::`, and no dotted IPv4 tail.
fn ipv6_host(address: Ipv6Addr) -> String {
    let pieces = address.segments();
    let (mut run_start, mut run_len) = (0, 0);
    let mut index = 0;
    while index < pieces.len() {
        let start = index;
        while index < pieces.len() && pieces[index] == 0 {
            index += 1;
        }
        if index - start > run_len {
            (run_start, run_len) = (start, index - start);
        }
        index += 1;
    }
    let mut host = String::from("[");
    let mut index = 0;
    while index < pieces.len() {
        if run_len >= 2 && index == run_start {
            host.push_str(if index == 0 { "::" } else { ":" });
            index += run_len;
            continue;
        }
        let _ = write!(host, "{:x}", pieces[index]);
        if index + 1 < pieces.len() {
            host.push(':');
        }
        index += 1;
    }
    host.push(']');
    host
}

const BAD_PORT: &str = "the port must be a number from 1 to 65535, or * for any port";

fn parse_port(port: &str) -> Result<u16, &'static str> {
    if port.is_empty() || port.len() > 5 || !port.bytes().all(|b| b.is_ascii_digit()) {
        return Err(BAD_PORT);
    }
    match port.parse::<u16>() {
        Ok(port) if port > 0 => Ok(port),
        _ => Err(BAD_PORT),
    }
}

fn add_names<'a>(list: &mut Vec<String>, names: impl IntoIterator<Item = &'a str>) {
    for name in names {
        let name = name.trim().to_ascii_lowercase();
        if !list.contains(&name) {
            list.push(name);
        }
    }
}

fn is_header_name(name: &str) -> bool {
    name != "*" && HeaderName::from_bytes(name.as_bytes()).is_ok()
}

/// The names of a comma-separated header list; an invalid name is recorded
/// against `key`.
fn header_list(source: &dyn ConfigSource, key: &str, errors: &mut Vec<ConfigError>) -> Vec<String> {
    let Some(raw) = source.get(key) else {
        return Vec::new();
    };
    let mut names = Vec::new();
    add_names(
        &mut names,
        raw.split(',')
            .map(str::trim)
            .filter(|name| !name.is_empty()),
    );
    if let Some(bad) = names.iter().find(|name| !is_header_name(name)) {
        errors.push(ConfigError::Invalid {
            key: source.describe(key),
            reason: format!(
                "{:?} is not a valid header name",
                truncate_for_log(bad, MAX_QUOTED)
            ),
        });
        return Vec::new();
    }
    names
}

fn take<T>(errors: &mut Vec<ConfigError>, result: Result<T, ConfigError>) -> Option<T> {
    match result {
        Ok(value) => Some(value),
        Err(error) => {
            errors.push(error);
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use axum::body::Body;
    use axum::routing::get;
    use http::header::HOST;
    use http::{Request, StatusCode};
    use sekvent_config::MapSource;
    use sekvent_error::ErrorCode;
    use sekvent_telemetry::LogBuffer;
    use tower::ServiceExt;
    use tracing::Level;
    use tracing_subscriber::Registry;
    use tracing_subscriber::layer::SubscriberExt;

    use super::*;

    fn list(cors: &Cors) -> Vec<String> {
        match &cors.origins {
            Origins::List(list) => list.clone(),
            Origins::Any => vec!["*".into()],
        }
    }

    #[test]
    fn defaults() {
        let cors = Cors::any_origin();
        assert_eq!(cors.origins, Origins::Any);
        assert!(!cors.credentials);
        assert_eq!(
            cors.methods,
            [
                Method::GET,
                Method::HEAD,
                Method::POST,
                Method::PUT,
                Method::PATCH,
                Method::DELETE
            ]
        );
        assert_eq!(cors.allow_headers, DEFAULT_ALLOW_HEADERS);
        assert_eq!(cors.expose_headers, DEFAULT_EXPOSE_HEADERS);
        assert_eq!(cors.max_age, Duration::from_secs(3600));
        assert!(cors.validate().is_ok());
    }

    #[test]
    fn origins_are_normalized_and_deduplicated() {
        let cors = Cors::origins([
            "HTTPS://App.Example.COM:443",
            "https://app.example.com/",
            " http://localhost:3000 ",
            "http://Example.com:80",
            "https://example.com:8443",
            "http://[::1]:8080",
            "http://[FE80::1]",
            "http://10.0.0.1:443",
            "http://[0:0:0:0:0:0:0:1]:8080",
            "http://[0:0:0:0:0:0:0:1]:3000",
            "http://[::FFFF:1.2.3.4]",
            "http://[2001:DB8:0:0:1:0:0:1]",
            "http://[1:0:0:2:0:0:0:3]",
            "http://[1::]",
            "http://[::]",
            "http://[1:2:3:4:5:6:7:8]",
            "http://[1:0:2:3:4:5:6:7]",
            "http://LOCALHOST",
            "HTTP://LocalHost:*",
            "http://localhost:*/",
            "http://127.0.0.1:*",
            "https://[0:0::1]:*",
        ])
        .unwrap();
        assert_eq!(
            list(&cors),
            [
                "https://app.example.com",
                "http://localhost:3000",
                "http://example.com",
                "https://example.com:8443",
                "http://[::1]:8080",
                "http://[fe80::1]",
                "http://10.0.0.1:443",
                "http://[::1]:3000",
                "http://[::ffff:102:304]",
                "http://[2001:db8::1:0:0:1]",
                "http://[1:0:0:2::3]",
                "http://[1::]",
                "http://[::]",
                "http://[1:2:3:4:5:6:7:8]",
                "http://[1:0:2:3:4:5:6:7]",
                "http://localhost",
                "http://localhost:*",
                "http://127.0.0.1:*",
                "https://[::1]:*",
            ]
        );
    }

    #[test]
    fn every_rejected_origin_shape_names_the_position_only() {
        for bad in [
            "https://app.example.com/path",
            "https://app.example.com/a/",
            "https://app.example.com?x=1",
            "https://app.example.com#top",
            "https://user@app.example.com",
            "*",
            "null",
            "NULL",
            "ftp://app.example.com",
            "app.example.com",
            "https://",
            "https://:443",
            "https://app..example.com",
            "https://app_example.com",
            "https://exämple.com",
            "https://app.example.com:",
            "https://app.example.com:0",
            "https://app.example.com:65536",
            "https://app.example.com:+80",
            "https://app.example.com:123456",
            "http://[::1",
            "http://[]",
            "http://[zz::1]",
            "http://[::1]x",
            "http://[:::]",
            "http://[1::2::3]",
            "http://[fe80::1%25eth0]",
            "http://[1:2:3:4:5:6:7:8:9]",
            "http://010.0.0.1",
            "http://1.2.3",
            "http://256.0.0.1",
            "http://0x7f.0.0.1",
            "http://a.0x1f",
            "http://*:80",
            "http://*",
            "http://*:*",
            "https://*.example.com",
            "https://*.example.com:*",
            "http://localhost:*x",
            "http://localhost:**",
            "http://localhost:x*",
            "http://localhost:*/path",
            "http://localhost:*?x=1",
            "http://localhost:*#top",
            "http://[::1]*",
            "*:*",
        ] {
            let error = Cors::origins(["https://ok.example.com", bad]).unwrap_err();
            assert_eq!(error.code(), ErrorCode::InvalidArgument, "{bad}");
            let message = error.message();
            assert!(
                message.starts_with("the CORS origin #2 is invalid: "),
                "{bad}: {message}"
            );
            assert!(bad.len() < 6 || !message.contains(bad), "{bad}: {message}");
        }
        let empty: [&str; 0] = [];
        let error = Cors::origins(empty).unwrap_err();
        assert_eq!(error.message(), "CORS needs at least one origin");
        assert!(Cors::origins(["https://ok.example.com", "*"]).is_err());
        assert!(Cors::origins(["http://1.2.3.4", "http://a.b4"]).is_ok());
    }

    #[test]
    fn wildcard_ports_are_reported_precisely() {
        let reason = |raw: &str| normalize_origin(raw).unwrap_err();
        assert_eq!(
            reason("http://*:80"),
            "the host must be exact; only the port may be *"
        );
        assert_eq!(
            reason("https://*.example.com"),
            "the host must be exact; only the port may be *"
        );
        assert_eq!(reason("http://localhost:*x"), BAD_PORT);
        assert_eq!(
            reason("http://localhost:*/path"),
            "an origin has no path, query or fragment"
        );
        assert_eq!(
            normalize_origin("http://localhost:*").unwrap(),
            "http://localhost:*"
        );
    }

    #[test]
    fn any_port_entries_match_that_scheme_and_host_only() {
        let list = OriginList::new(&[
            "http://localhost:*".to_owned(),
            "https://[::1]:*".to_owned(),
            "https://app.example.com".to_owned(),
        ]);
        assert_eq!(list.exact.len(), 1);
        assert_eq!(list.any_port, ["http://localhost", "https://[::1]"]);
        let matches = |origin: &str| list.matches(&HeaderValue::from_str(origin).unwrap());
        for allowed in [
            "http://localhost",
            "http://localhost:1",
            "http://localhost:80",
            "http://localhost:5173",
            "http://localhost:65535",
            "https://[::1]",
            "https://[::1]:8443",
            "https://app.example.com",
        ] {
            assert!(matches(allowed), "{allowed}");
        }
        for refused in [
            "https://localhost:5173",
            "http://localhost:",
            "http://localhost:0",
            "http://localhost:05173",
            "http://localhost:65536",
            "http://localhost:123456",
            "http://localhost:+80",
            "http://localhost:*",
            "http://localhost:5173/",
            "http://localhost.evil.example",
            "http://localhost.:5173",
            "http://localhostx:5173",
            "http://localhost:5173@evil.example",
            "http://127.0.0.1:5173",
            "http://[::1]:8443",
            "https://app.example.com:8443",
            "null",
        ] {
            assert!(!matches(refused), "{refused}");
        }
        let invalid = HeaderValue::from_bytes(b"http://localhost:\xff").unwrap();
        assert!(!list.matches(&invalid));
    }

    #[test]
    fn credentials_in_a_rejected_origin_are_never_quoted() {
        for bad in [
            "https://admin:s3cret@app.example.com",
            "https://app.example.com/?token=s3cret",
            "https://app.example.com/s3cret",
        ] {
            let message = Cors::origins([bad]).unwrap_err().message().to_owned();
            assert!(!message.contains("s3cret"), "{message}");
            assert!(message.contains("#1"), "{message}");
            let value = format!(" , https://ok.example.com,{bad}");
            let error = read(&[("CORS_ORIGINS", value.as_str())]).unwrap_err();
            let text = error.to_string();
            assert!(!text.contains("s3cret"), "{text}");
            assert!(text.contains("CORS_ORIGINS"), "{text}");
            assert!(text.contains("origin #3 is invalid"), "{text}");
        }
    }

    #[test]
    fn validation_fails_closed() {
        let error = Cors::any_origin()
            .allow_credentials(true)
            .validate()
            .unwrap_err();
        assert_eq!(error.code(), ErrorCode::InvalidArgument);
        assert_eq!(
            error.message(),
            "CORS credentials need an explicit origin list"
        );
        let listed = Cors::origins(["https://app.example.com"])
            .unwrap()
            .allow_credentials(true);
        assert!(listed.validate().is_ok());

        assert!(Cors::any_origin().methods(&[]).validate().is_err());
        let star = Method::from_bytes(b"*").unwrap();
        assert!(Cors::any_origin().methods(&[star]).validate().is_err());
        for bad in ["*", "bad name", ""] {
            let error = Cors::any_origin()
                .allow_headers(&[bad])
                .validate()
                .unwrap_err();
            assert!(error.message().contains("header name"), "{bad}");
            assert!(
                Cors::any_origin()
                    .expose_headers(&[bad])
                    .validate()
                    .is_err()
            );
        }
    }

    #[test]
    fn builders_add_and_replace() {
        let cors = Cors::any_origin()
            .methods(&[Method::GET, Method::GET, Method::POST])
            .allow_headers(&["X-Tenant", "x-tenant", "authorization"])
            .expose_headers(&["X-Total"])
            .max_age(Duration::from_secs(60));
        assert_eq!(cors.methods, [Method::GET, Method::POST]);
        assert_eq!(cors.allow_headers.len(), DEFAULT_ALLOW_HEADERS.len() + 1);
        assert!(cors.allow_headers.contains(&"x-tenant".to_owned()));
        assert_eq!(cors.expose_headers.last().unwrap(), "x-total");
        assert_eq!(cors.max_age, Duration::from_secs(60));
        assert!(cors.validate().is_ok());
        assert_eq!(cors.clone(), cors);
        assert!(format!("{cors:?}").contains("Any"));
    }

    #[test]
    fn config_keys_follow_the_prefix() {
        assert_eq!(
            Cors::config_keys("API_CORS_"),
            [
                "API_CORS_ORIGINS",
                "API_CORS_CREDENTIALS",
                "API_CORS_MAX_AGE",
                "API_CORS_ALLOW_HEADERS",
                "API_CORS_EXPOSE_HEADERS",
            ]
        );
    }

    fn read(pairs: &[(&str, &str)]) -> Result<Option<Cors>, ConfigError> {
        let source: MapSource = pairs.iter().copied().collect();
        Cors::from_config(&source, "CORS_")
    }

    #[test]
    fn from_config_reads_every_key() {
        assert_eq!(read(&[]).unwrap(), None);
        assert_eq!(read(&[("CORS_ORIGINS", "  ")]).unwrap(), None);
        assert_eq!(
            read(&[("CORS_ORIGINS", "*")]).unwrap(),
            Some(Cors::any_origin())
        );
        let cors = read(&[
            (
                "CORS_ORIGINS",
                "https://a.example.com, ,HTTP://B.example.com:80",
            ),
            ("CORS_CREDENTIALS", "true"),
            ("CORS_MAX_AGE", "10m"),
            ("CORS_ALLOW_HEADERS", "x-tenant, X-Trace"),
            ("CORS_EXPOSE_HEADERS", "x-total"),
        ])
        .unwrap()
        .unwrap();
        assert_eq!(
            list(&cors),
            ["https://a.example.com", "http://b.example.com"]
        );
        assert!(cors.credentials);
        assert_eq!(cors.max_age, Duration::from_secs(600));
        assert!(
            cors.allow_headers
                .ends_with(&["x-tenant".into(), "x-trace".into()])
        );
        assert_eq!(cors.expose_headers.last().unwrap(), "x-total");
        assert!(cors.validate().is_ok());
    }

    #[test]
    fn from_config_reports_every_problem_by_key() {
        let error = read(&[("CORS_ORIGINS", "*"), ("CORS_CREDENTIALS", "yes")]).unwrap_err();
        assert!(
            matches!(&error, ConfigError::Invalid { key, .. } if key == "CORS_CREDENTIALS"),
            "{error:?}"
        );

        let error = read(&[
            ("CORS_ORIGINS", "https://a.example.com/path"),
            ("CORS_CREDENTIALS", "maybe"),
            ("CORS_MAX_AGE", "soon"),
            ("CORS_ALLOW_HEADERS", "bad name"),
            ("CORS_EXPOSE_HEADERS", "*"),
        ])
        .unwrap_err();
        let ConfigError::Multiple(errors) = &error else {
            panic!("{error:?}");
        };
        let keys: Vec<&str> = errors
            .iter()
            .map(|error| match error {
                ConfigError::Invalid { key, .. } | ConfigError::Malformed { key, .. } => {
                    key.as_str()
                }
                other => panic!("{other:?}"),
            })
            .collect();
        assert_eq!(
            keys,
            [
                "CORS_ORIGINS",
                "CORS_CREDENTIALS",
                "CORS_MAX_AGE",
                "CORS_ALLOW_HEADERS",
                "CORS_EXPOSE_HEADERS",
            ]
        );
        let text = error.to_string();
        assert!(!text.contains("https://a.example.com/path"), "{text}");
        assert!(text.contains("origin #1 is invalid"), "{text}");
        assert!(text.contains("\"bad name\""), "{text}");

        let error = read(&[("CORS_ORIGINS", "*, https://a.example.com")]).unwrap_err();
        assert!(
            matches!(&error, ConfigError::Invalid { key, .. } if key == "CORS_ORIGINS"),
            "{error:?}"
        );
        assert!(read(&[("CORS_ORIGINS", ",,")]).is_err());

        let error = read(&[("CORS_ORIGINS", "https://a.example.com, http://*:80")]).unwrap_err();
        let text = error.to_string();
        assert!(text.contains("CORS_ORIGINS"), "{text}");
        assert!(text.contains("origin #2 is invalid"), "{text}");
    }

    #[test]
    fn any_port_entries_are_read_from_config_with_credentials() {
        let cors = read(&[
            ("CORS_ORIGINS", "http://localhost:*, http://127.0.0.1:*"),
            ("CORS_CREDENTIALS", "true"),
        ])
        .unwrap()
        .unwrap();
        assert_eq!(list(&cors), ["http://localhost:*", "http://127.0.0.1:*"]);
        assert!(cors.validate().is_ok(), "a pattern is an explicit list");
    }

    /// A handler that sets its own, wider CORS headers.
    async fn reflecting(request: Request<Body>) -> Response {
        let mut response = Response::new(Body::from("reflected"));
        let headers = response.headers_mut();
        if let Some(origin) = request.headers().get(ORIGIN) {
            headers.insert(ACCESS_CONTROL_ALLOW_ORIGIN, origin.clone());
        }
        headers.insert(
            ACCESS_CONTROL_ALLOW_CREDENTIALS,
            HeaderValue::from_static("true"),
        );
        headers.insert(ACCESS_CONTROL_MAX_AGE, HeaderValue::from_static("1"));
        headers.append(VARY, HeaderValue::from_static("Origin, Accept-Encoding"));
        response
    }

    async fn call(cors: &Cors, request: Request<Body>) -> Response {
        let router = Router::new()
            .route(
                "/echo",
                get(|| async { "ok" }).options(|| async { "options handled" }),
            )
            .route("/reflect", get(reflecting));
        cors.apply(router).oneshot(request).await.unwrap()
    }

    fn cors_names(response: &Response) -> Vec<&str> {
        response
            .headers()
            .keys()
            .map(HeaderName::as_str)
            .filter(|name| name.starts_with("access-control-"))
            .collect()
    }

    fn options(path: &str, origin: Option<&str>, preflight: bool) -> Request<Body> {
        let mut request = Request::options(path);
        if let Some(origin) = origin {
            request = request.header("origin", origin);
        }
        if preflight {
            request = request.header("access-control-request-method", "POST");
        }
        request.body(Body::empty()).unwrap()
    }

    async fn text(response: Response) -> String {
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        String::from_utf8_lossy(&body).into_owned()
    }

    #[tokio::test]
    async fn the_layer_answers_listed_origins_only() {
        let cors = Cors::origins(["https://app.example.com"])
            .unwrap()
            .allow_credentials(true);
        let allowed = Request::get("/echo")
            .header("origin", "https://app.example.com")
            .body(Body::empty())
            .unwrap();
        let response = call(&cors, allowed).await;
        assert_eq!(response.status(), StatusCode::OK);
        let headers = response.headers();
        assert_eq!(
            headers["access-control-allow-origin"],
            "https://app.example.com"
        );
        assert_eq!(headers["access-control-allow-credentials"], "true");
        assert_eq!(headers["vary"], "origin");
        assert!(
            headers["access-control-expose-headers"]
                .to_str()
                .unwrap()
                .contains("grpc-status")
        );

        for origin in [Some("https://evil.example.com"), None] {
            let mut request = Request::get("/echo");
            if let Some(origin) = origin {
                request = request.header("origin", origin);
            }
            let response = call(&cors, request.body(Body::empty()).unwrap()).await;
            assert_eq!(response.status(), StatusCode::OK);
            assert!(cors_names(&response).is_empty(), "{:?}", response.headers());
            assert_eq!(response.headers()["vary"], "origin");
        }
    }

    #[tokio::test]
    async fn any_port_entries_reflect_the_origin() {
        let cors = Cors::origins(["http://localhost:*"])
            .unwrap()
            .allow_credentials(true);
        assert!(cors.validate().is_ok());
        for origin in ["http://localhost:5173", "http://localhost"] {
            let request = Request::get("/reflect")
                .header("origin", origin)
                .body(Body::empty())
                .unwrap();
            let response = call(&cors, request).await;
            let headers = response.headers();
            assert_eq!(headers["access-control-allow-origin"], origin);
            assert_eq!(headers["access-control-allow-credentials"], "true");
            let vary: Vec<&str> = headers
                .get_all(VARY)
                .iter()
                .map(|value| value.to_str().unwrap())
                .collect();
            assert_eq!(vary, ["Accept-Encoding", "origin"]);

            let response = call(&cors, options("/echo", Some(origin), true)).await;
            let headers = response.headers();
            assert_eq!(headers["access-control-allow-origin"], origin);
            assert_eq!(headers["access-control-max-age"], "3600");
            assert_eq!(headers["vary"], "origin");
        }
        for origin in [
            "https://localhost:5173",
            "http://localhost.evil.example:5173",
        ] {
            let response = call(&cors, options("/echo", Some(origin), true)).await;
            assert!(cors_names(&response).is_empty(), "{origin}");
            assert_eq!(response.headers()["vary"], "origin");
        }
    }

    #[tokio::test]
    async fn the_configured_rules_override_inner_cors_headers() {
        let cors = Cors::origins(["https://app.example.com"])
            .unwrap()
            .allow_credentials(true);
        let evil = Request::get("/reflect")
            .header("origin", "https://evil.example.com")
            .body(Body::empty())
            .unwrap();
        let response = call(&cors, evil).await;
        assert!(cors_names(&response).is_empty(), "{:?}", response.headers());
        let vary: Vec<&str> = response
            .headers()
            .get_all(VARY)
            .iter()
            .map(|value| value.to_str().unwrap())
            .collect();
        assert_eq!(vary, ["Accept-Encoding", "origin"]);
        assert_eq!(text(response).await, "reflected");

        let allowed = Request::get("/reflect")
            .header("origin", "https://app.example.com")
            .body(Body::empty())
            .unwrap();
        let response = call(&cors, allowed).await;
        let headers = response.headers();
        assert_eq!(
            headers["access-control-allow-origin"],
            "https://app.example.com"
        );
        assert_eq!(headers["access-control-allow-credentials"], "true");
        assert!(!headers.contains_key("access-control-max-age"));

        let any = Cors::any_origin();
        let evil = Request::get("/reflect")
            .header("origin", "https://evil.example.com")
            .body(Body::empty())
            .unwrap();
        let response = call(&any, evil).await;
        let headers = response.headers();
        assert_eq!(headers["access-control-allow-origin"], "*");
        assert!(!headers.contains_key("access-control-allow-credentials"));
        assert_eq!(headers["vary"], "Origin, Accept-Encoding", "kept as is");
    }

    #[tokio::test]
    async fn only_real_preflights_are_answered_here() {
        let cors = Cors::origins(["https://app.example.com"]).unwrap();

        let response = call(
            &cors,
            options("/echo", Some("https://app.example.com"), true),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let headers = response.headers();
        assert_eq!(
            headers["access-control-allow-origin"],
            "https://app.example.com"
        );
        assert!(headers.contains_key("access-control-allow-methods"));
        assert!(headers.contains_key("access-control-allow-headers"));
        assert_eq!(headers["access-control-max-age"], "3600");
        assert_eq!(headers["vary"], "origin");
        assert_eq!(text(response).await, "");

        let response = call(
            &cors,
            options("/echo", Some("https://evil.example.com"), true),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        assert!(cors_names(&response).is_empty(), "{:?}", response.headers());
        assert_eq!(text(response).await, "", "the handler never ran");

        let response = call(
            &cors,
            options("/echo", Some("https://app.example.com"), false),
        )
        .await;
        assert_eq!(
            response.headers()["access-control-allow-origin"],
            "https://app.example.com"
        );
        assert!(!response.headers().contains_key("access-control-max-age"));
        assert_eq!(text(response).await, "options handled");

        let response = call(&cors, options("/echo", None, true)).await;
        assert_eq!(cors_names(&response), [] as [&str; 0]);
        assert_eq!(text(response).await, "options handled");

        let get_with_method = Request::get("/echo")
            .header("origin", "https://app.example.com")
            .header("access-control-request-method", "POST")
            .body(Body::empty())
            .unwrap();
        let response = call(&cors, get_with_method).await;
        assert_eq!(text(response).await, "ok", "only OPTIONS is a preflight");
    }

    #[test]
    fn inner_vary_keeps_everything_but_the_cors_entries() {
        let mut headers = HeaderMap::new();
        headers.insert("access-control-allow-origin", HeaderValue::from_static("*"));
        headers.insert("x-other", HeaderValue::from_static("1"));
        headers.append(
            VARY,
            HeaderValue::from_static("origin, access-control-request-method"),
        );
        headers.append(
            VARY,
            HeaderValue::from_static("Access-Control-Request-Headers,,accept-encoding"),
        );
        headers.append(VARY, HeaderValue::from_bytes(b"caf\xe9").unwrap());
        let mut list = headers.clone();

        strip_cors_headers(&mut headers, false);
        let vary: Vec<&[u8]> = headers
            .get_all(VARY)
            .iter()
            .map(HeaderValue::as_bytes)
            .collect();
        assert_eq!(vary, [&b"origin"[..], b"accept-encoding", b"caf\xe9"]);
        assert!(!headers.contains_key("access-control-allow-origin"));
        assert_eq!(headers["x-other"], "1");

        strip_cors_headers(&mut list, true);
        assert_eq!(list.get_all(VARY).iter().count(), 2, "{list:?}");

        let mut none = HeaderMap::new();
        strip_cors_headers(&mut none, true);
        assert!(none.is_empty());
    }

    #[test]
    fn custom_scheme_origins_are_lowercased_and_keep_their_port() {
        let cors = Cors::origins([
            "Capacitor://LocalHost",
            "capacitor://localhost/",
            " ionic://localhost ",
            "com.example.app+web-1://Callback:080",
            "x://[0:0::1]:443",
            "https://localhost",
        ])
        .unwrap();
        assert_eq!(
            list(&cors),
            [
                "capacitor://localhost",
                "ionic://localhost",
                "com.example.app+web-1://callback:80",
                "x://[::1]:443",
                "https://localhost",
            ]
        );
        assert!(cors.validate().is_ok());
    }

    #[test]
    fn custom_scheme_entries_match_exactly_ignoring_case() {
        let list = OriginList::new(&[
            "capacitor://localhost".to_owned(),
            "app://localhost:8080".to_owned(),
            "https://localhost".to_owned(),
        ]);
        assert_eq!(
            list.custom,
            ["capacitor://localhost", "app://localhost:8080"]
        );
        assert_eq!(list.exact.len(), 1);
        assert_eq!(list.any_port, Vec::<String>::new());
        let matches = |origin: &str| list.matches(&HeaderValue::from_str(origin).unwrap());
        for allowed in [
            "capacitor://localhost",
            "Capacitor://LocalHost",
            "CAPACITOR://LOCALHOST",
            "app://localhost:8080",
            "https://localhost",
        ] {
            assert!(matches(allowed), "{allowed}");
        }
        for refused in [
            "capacitor://evil",
            "capacitor://localhost:8080",
            "capacitor://localhost/",
            "capacitor://localhost.evil.example",
            "capacitor://localhostx",
            "ionic://localhost",
            "app://localhost",
            "app://localhost:80",
            "http://localhost",
            "HTTPS://localhost",
            "null",
        ] {
            assert!(!matches(refused), "{refused}");
        }
    }

    #[test]
    fn refused_custom_scheme_entries_are_reported_precisely() {
        let reason = |raw: &str| normalize_origin(raw).unwrap_err();
        for (bad, expected) in [
            ("null", "the opaque origin null is never allowed"),
            ("*", "* is only allowed alone (Cors::any_origin)"),
            ("capacitor://localhost:*", CUSTOM_ANY_PORT),
            ("file://x", OPAQUE),
            ("FILE:///etc/hosts", OPAQUE),
            ("data:text/plain,x", OPAQUE),
            ("blob:https://app.example.com/1", OPAQUE),
            ("javascript:alert(1)", OPAQUE),
            ("about:blank", OPAQUE),
            ("ftp://app.example.com", NON_PAGE),
            ("ws://app.example.com", NON_PAGE),
            ("wss://app.example.com", NON_PAGE),
            (
                "capacitor://localhost/path",
                "an origin has no path, query or fragment",
            ),
            (
                "capacitor://localhost?x",
                "an origin has no path, query or fragment",
            ),
            (
                "capacitor://localhost#x",
                "an origin has no path, query or fragment",
            ),
            ("capacitor://user@localhost", "an origin has no user info"),
            ("capacitor://", "an origin needs a host"),
            ("capacitor:localhost", NO_SCHEME),
            ("capacitor:/localhost", NO_SCHEME),
            ("1app://localhost", NO_SCHEME),
            ("ca pacitor://localhost", NO_SCHEME),
            ("://localhost", NO_SCHEME),
            ("localhost", NO_SCHEME),
            (
                "capacitor://*",
                "the host must be exact; only the port may be *",
            ),
            ("capacitor://localhost:0", BAD_PORT),
        ] {
            assert_eq!(reason(bad), expected, "{bad}");
        }
    }

    #[test]
    fn refused_custom_scheme_configuration_names_the_key() {
        for bad in [
            "null",
            "*",
            "capacitor://localhost:*",
            "file://x",
            "data:text/plain,x",
            "blob:https://app.example.com/1",
            "javascript:alert(1)",
            "about:blank",
            "capacitor://localhost/path",
            "capacitor://user@localhost",
            "capacitor://localhost?x",
        ] {
            let value = format!("capacitor://localhost,{bad}");
            let error = read(&[("CORS_ORIGINS", value.as_str())]).unwrap_err();
            assert!(
                matches!(&error, ConfigError::Invalid { key, .. } if key == "CORS_ORIGINS"),
                "{bad}: {error:?}"
            );
            let text = error.to_string();
            assert!(text.contains("CORS_ORIGINS"), "{bad}: {text}");
            assert!(text.contains("origin #2 is invalid"), "{bad}: {text}");
        }
        let cors = read(&[
            ("CORS_ORIGINS", "Capacitor://LocalHost, https://localhost"),
            ("CORS_CREDENTIALS", "true"),
        ])
        .unwrap()
        .unwrap();
        assert_eq!(list(&cors), ["capacitor://localhost", "https://localhost"]);
        assert!(cors.validate().is_ok());
    }

    #[tokio::test]
    async fn custom_scheme_origins_pass_preflights_only_when_listed() {
        let cors = Cors::origins(["capacitor://localhost", "https://localhost"]).unwrap();
        for origin in [
            "capacitor://localhost",
            "Capacitor://LocalHost",
            "https://localhost",
        ] {
            let response = call(&cors, options("/echo", Some(origin), true)).await;
            assert_eq!(response.status(), StatusCode::OK);
            let headers = response.headers();
            assert_eq!(headers["access-control-allow-origin"], origin);
            assert!(headers.contains_key("access-control-allow-methods"));
            assert_eq!(headers["vary"], "origin");

            let request = Request::get("/echo")
                .header("origin", origin)
                .body(Body::empty())
                .unwrap();
            let response = call(&cors, request).await;
            let headers = response.headers();
            assert_eq!(headers["access-control-allow-origin"], origin);
            assert!(
                headers["access-control-expose-headers"]
                    .to_str()
                    .unwrap()
                    .contains("grpc-status")
            );
            assert_eq!(headers["vary"], "origin");
        }
        for origin in ["capacitor://evil", "capacitor://localhost:8080", "null"] {
            let response = call(&cors, options("/echo", Some(origin), true)).await;
            assert!(cors_names(&response).is_empty(), "{origin}");
            assert_eq!(response.headers()["vary"], "origin");
        }

        let unlisted = Cors::origins(["https://app.example.com"]).unwrap();
        let response = call(
            &unlisted,
            options("/echo", Some("capacitor://localhost"), true),
        )
        .await;
        assert!(cors_names(&response).is_empty(), "{:?}", response.headers());
    }

    // ------------------------------------------------- origin functions

    const TENANT: &str = "https://a.example.com";

    fn get_from(path: &str, origin: &str) -> Request<Body> {
        Request::get(path)
            .header("origin", origin)
            .body(Body::empty())
            .unwrap()
    }

    /// A request to `/echo` with a `Host`; an `OPTIONS` one is a preflight.
    fn on_host(method: &Method, host: &str, origin: &str) -> Request<Body> {
        let mut request = Request::builder()
            .method(method.clone())
            .uri("/echo")
            .header(HOST, host)
            .header(ORIGIN, origin);
        if *method == Method::OPTIONS {
            request = request.header(ACCESS_CONTROL_REQUEST_METHOD, "POST");
        }
        request.body(Body::empty()).unwrap()
    }

    fn header(head: &Parts, name: &HeaderName) -> Option<String> {
        head.headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned)
    }

    /// An origin function allowing `allowed` only, counting its calls.
    fn counting(
        calls: &Arc<AtomicUsize>,
        allowed: &'static str,
    ) -> impl Fn(&str, &Parts) -> bool + Send + Sync + 'static {
        let calls = Arc::clone(calls);
        move |origin: &str, _: &Parts| {
            calls.fetch_add(1, Ordering::SeqCst);
            origin == allowed
        }
    }

    fn vary(response: &Response) -> Vec<&str> {
        response
            .headers()
            .get_all(VARY)
            .iter()
            .map(|value| value.to_str().unwrap())
            .collect()
    }

    #[tokio::test]
    async fn an_origin_function_alone_allows_and_refuses() {
        let cors = Cors::origin_predicate(|origin, _| origin == TENANT);
        assert!(cors.validate().is_ok());
        assert_eq!(list(&cors), [] as [String; 0]);

        let response = call(&cors, get_from("/echo", TENANT)).await;
        assert_eq!(response.status(), StatusCode::OK);
        let headers = response.headers();
        assert_eq!(headers["access-control-allow-origin"], TENANT);
        assert!(
            headers["access-control-expose-headers"]
                .to_str()
                .unwrap()
                .contains("grpc-status")
        );
        assert!(!headers.contains_key("access-control-allow-credentials"));
        assert_eq!(vary(&response), ["origin"]);
        assert_eq!(text(response).await, "ok");

        let response = call(&cors, get_from("/echo", "https://b.example.com")).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(cors_names(&response), [] as [&str; 0]);
        assert_eq!(vary(&response), ["origin"]);
        assert_eq!(text(response).await, "ok", "the handler still ran");

        let response = call(&cors, options("/echo", Some("https://b.example.com"), true)).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(cors_names(&response), [] as [&str; 0]);
        assert_eq!(vary(&response), ["origin"]);
        assert_eq!(text(response).await, "", "answered by the layer");
    }

    #[tokio::test]
    async fn the_preflight_and_the_request_are_decided_alike_from_their_head() {
        type Seen = (String, Method, Option<String>, Option<String>);
        let seen: Arc<Mutex<Vec<Seen>>> = Arc::default();
        let record = Arc::clone(&seen);
        let cors = Cors::origin_predicate(move |origin, head| {
            let host = header(head, &HOST);
            record.lock().unwrap().push((
                origin.to_owned(),
                head.method.clone(),
                header(head, &ACCESS_CONTROL_REQUEST_METHOD),
                host.clone(),
            ));
            origin == TENANT && host.as_deref() == Some("a.example.com")
        });

        let response = call(&cors, on_host(&Method::OPTIONS, "a.example.com", TENANT)).await;
        assert_eq!(response.status(), StatusCode::OK);
        let headers = response.headers();
        assert_eq!(headers["access-control-allow-origin"], TENANT);
        assert!(headers.contains_key("access-control-allow-methods"));
        assert!(headers.contains_key("access-control-allow-headers"));
        assert_eq!(headers["access-control-max-age"], "3600");
        assert_eq!(vary(&response), ["origin"]);
        assert_eq!(text(response).await, "", "answered by the layer");

        let response = call(&cors, on_host(&Method::GET, "a.example.com", TENANT)).await;
        assert_eq!(response.headers()["access-control-allow-origin"], TENANT);
        assert_eq!(text(response).await, "ok");

        for method in [Method::OPTIONS, Method::GET] {
            let response = call(&cors, on_host(&method, "b.example.com", TENANT)).await;
            assert_eq!(response.status(), StatusCode::OK);
            assert_eq!(cors_names(&response), [] as [&str; 0], "{method}");
            assert_eq!(vary(&response), ["origin"]);
        }

        let seen = seen.lock().unwrap().clone();
        let entry = |method: Method, preflight: bool, host: &str| {
            (
                TENANT.to_owned(),
                method,
                preflight.then(|| "POST".to_owned()),
                Some(host.to_owned()),
            )
        };
        assert_eq!(
            seen,
            [
                entry(Method::OPTIONS, true, "a.example.com"),
                entry(Method::GET, false, "a.example.com"),
                entry(Method::OPTIONS, true, "b.example.com"),
                entry(Method::GET, false, "b.example.com"),
            ]
        );
    }

    #[tokio::test]
    async fn a_listed_origin_never_reaches_the_function() {
        let calls = Arc::new(AtomicUsize::new(0));
        let cors = Cors::origins([
            "https://app.example.com",
            "http://localhost:*",
            "capacitor://localhost",
        ])
        .unwrap()
        .or_origin_predicate(counting(&calls, TENANT));
        assert!(cors.validate().is_ok());
        for origin in [
            "https://app.example.com",
            "http://localhost:5173",
            "Capacitor://LocalHost",
        ] {
            for request in [
                get_from("/echo", origin),
                options("/echo", Some(origin), true),
            ] {
                let response = call(&cors, request).await;
                assert_eq!(response.headers()["access-control-allow-origin"], origin);
            }
        }
        assert_eq!(calls.load(Ordering::SeqCst), 0, "listed origins");

        let response = call(&cors, get_from("/echo", TENANT)).await;
        assert_eq!(response.headers()["access-control-allow-origin"], TENANT);
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        let response = call(&cors, get_from("/echo", "https://b.example.com")).await;
        assert_eq!(cors_names(&response), [] as [&str; 0]);
        assert_eq!(vary(&response), ["origin"]);
        assert_eq!(text(response).await, "ok");
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn an_allowed_origin_is_echoed_as_sent_with_credentials() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let record = Arc::clone(&seen);
        let cors = Cors::origin_predicate(move |origin, _| {
            record.lock().unwrap().push(origin.to_owned());
            true
        })
        .allow_credentials(true);
        assert!(cors.validate().is_ok());

        let origin = "HTTPS://Tenant.Example.COM:443";
        for request in [
            get_from("/echo", origin),
            options("/echo", Some(origin), true),
        ] {
            let response = call(&cors, request).await;
            let headers = response.headers();
            assert_eq!(headers["access-control-allow-origin"], origin, "never *");
            assert_eq!(headers["access-control-allow-credentials"], "true");
            assert_eq!(vary(&response), ["origin"]);
        }
        assert_eq!(*seen.lock().unwrap(), [origin, origin], "not normalized");
    }

    #[tokio::test]
    async fn null_non_text_and_missing_origins_never_reach_the_function() {
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&calls);
        let cors = Cors::origin_predicate(move |_, _| {
            counter.fetch_add(1, Ordering::SeqCst);
            true
        });
        let origins = [
            HeaderValue::from_static("null"),
            HeaderValue::from_static("NULL"),
            HeaderValue::from_bytes(b"https://\xe9.example.com").unwrap(),
        ];
        for origin in origins {
            let request = Request::get("/echo")
                .header(ORIGIN, origin.clone())
                .body(Body::empty())
                .unwrap();
            let response = call(&cors, request).await;
            assert_eq!(cors_names(&response), [] as [&str; 0], "{origin:?}");
            assert_eq!(vary(&response), ["origin"]);
            assert_eq!(text(response).await, "ok");

            let preflight = Request::options("/echo")
                .header(ORIGIN, origin.clone())
                .header(ACCESS_CONTROL_REQUEST_METHOD, "POST")
                .body(Body::empty())
                .unwrap();
            let response = call(&cors, preflight).await;
            assert_eq!(response.status(), StatusCode::OK);
            assert_eq!(cors_names(&response), [] as [&str; 0], "{origin:?}");
            assert_eq!(text(response).await, "");
        }

        let response = call(&cors, Request::get("/echo").body(Body::empty()).unwrap()).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(cors_names(&response), [] as [&str; 0]);
        assert_eq!(vary(&response), ["origin"]);
        assert_eq!(text(response).await, "ok", "served, undecorated");

        let response = call(&cors, options("/echo", None, true)).await;
        assert_eq!(cors_names(&response), [] as [&str; 0]);
        assert_eq!(text(response).await, "options handled");
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn a_panicking_function_refuses_and_logs_without_the_origin() {
        let buffer = LogBuffer::new(16);
        let _guard = tracing::subscriber::set_default(Registry::default().with(buffer.layer()));
        let cors = Cors::origin_predicate(|origin, _| panic!("refusing {origin}"));
        let origin = "https://secret-tenant.example.com";

        let response = call(&cors, options("/echo", Some(origin), true)).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(cors_names(&response), [] as [&str; 0]);
        assert_eq!(vary(&response), ["origin"]);

        let response = call(&cors, get_from("/echo", origin)).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(cors_names(&response), [] as [&str; 0]);
        assert_eq!(text(response).await, "ok");

        let errors: Vec<_> = buffer
            .snapshot()
            .into_iter()
            .filter(|record| record.level == Level::ERROR)
            .collect();
        assert_eq!(errors.len(), 2, "{errors:?}");
        for record in &errors {
            assert!(record.message.contains("panicked"), "{record:?}");
            assert!(
                !format!("{record:?}").contains("secret-tenant"),
                "{record:?}"
            );
        }
    }

    #[tokio::test]
    async fn any_origin_never_calls_the_function() {
        let calls = Arc::new(AtomicUsize::new(0));
        let cors = Cors::any_origin().or_origin_predicate(counting(&calls, TENANT));
        assert!(cors.validate().is_ok());
        for request in [
            get_from("/echo", TENANT),
            options("/echo", Some(TENANT), true),
        ] {
            let response = call(&cors, request).await;
            assert_eq!(response.headers()["access-control-allow-origin"], "*");
            assert!(!response.headers().contains_key(VARY));
        }
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        let error = cors.allow_credentials(true).validate().unwrap_err();
        assert_eq!(error.code(), ErrorCode::InvalidArgument);
    }

    #[test]
    fn any_origin_drops_the_function_and_what_it_captures() {
        let captured = Arc::new(AtomicUsize::new(0));
        let cors = Cors::any_origin().or_origin_predicate(counting(&captured, TENANT));
        assert_eq!(Arc::strong_count(&captured), 1, "the function was dropped");
        assert_eq!(cors, Cors::any_origin());
        let debug = format!("{cors:?}");
        assert!(debug.contains("predicate: None"), "{debug}");

        let configured = read(&[("CORS_ORIGINS", "*")])
            .unwrap()
            .unwrap()
            .or_origin_predicate(counting(&captured, TENANT));
        assert_eq!(Arc::strong_count(&captured), 1);
        assert_eq!(configured, Cors::any_origin());

        let listed = Cors::origins([TENANT])
            .unwrap()
            .or_origin_predicate(counting(&captured, TENANT));
        assert_eq!(Arc::strong_count(&captured), 2, "a list keeps it");
        drop(listed);
        assert_eq!(Arc::strong_count(&captured), 1);
    }

    #[tokio::test]
    async fn inner_cors_headers_are_replaced_under_a_function() {
        let cors = Cors::origin_predicate(|origin, _| origin == TENANT);

        let response = call(&cors, get_from("/reflect", "https://b.example.com")).await;
        assert_eq!(cors_names(&response), [] as [&str; 0]);
        assert_eq!(vary(&response), ["Accept-Encoding", "origin"]);
        assert_eq!(text(response).await, "reflected");

        let response = call(&cors, get_from("/reflect", TENANT)).await;
        let headers = response.headers();
        assert_eq!(headers["access-control-allow-origin"], TENANT);
        assert!(!headers.contains_key("access-control-allow-credentials"));
        assert!(!headers.contains_key("access-control-max-age"));
        assert_eq!(vary(&response), ["Accept-Encoding", "origin"]);
    }

    #[tokio::test]
    async fn a_second_function_replaces_the_first() {
        let calls = Arc::new(AtomicUsize::new(0));
        let cors =
            Cors::origin_predicate(counting(&calls, TENANT)).or_origin_predicate(|_, _| false);
        let response = call(&cors, get_from("/echo", TENANT)).await;
        assert_eq!(cors_names(&response), [] as [&str; 0]);
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn a_function_equals_only_itself_and_debug_hides_it() {
        fn tenant(origin: &str, _: &Parts) -> bool {
            origin == TENANT
        }
        let one = Cors::origin_predicate(tenant);
        assert!(one.validate().is_ok());
        assert_eq!(one, one.clone());
        assert_ne!(
            one,
            Cors::origin_predicate(tenant),
            "two functions never compare equal"
        );
        assert_ne!(one, one.clone().max_age(Duration::from_secs(5)));
        let listed = Cors::origins([TENANT]).unwrap();
        assert_ne!(listed, listed.clone().or_origin_predicate(tenant));

        let debug = format!("{one:?}");
        assert!(
            debug.contains("predicate: Some(OriginPredicate(..))"),
            "{debug}"
        );
        assert!(!debug.contains("0x"), "{debug}");
        let listed = format!("{listed:?}");
        assert!(listed.contains("predicate: None"), "{listed}");
    }
}
