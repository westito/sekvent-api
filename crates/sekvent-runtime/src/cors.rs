//! Cross-origin rules for browsers calling the server.

use std::fmt::Write as _;
use std::net::{Ipv4Addr, Ipv6Addr};
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
/// origin list ([`origins`](Self::origins)), or read from configuration
/// ([`from_config`](Self::from_config)), then handed to
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
/// without `Origin`, or from an origin outside the list, gets no
/// `access-control-*` headers at all, so the browser blocks it; a list also
/// adds `Vary: Origin` to every response.
///
/// These rules are authoritative: `access-control-*` headers set by
/// application layers or handlers are removed before the server adds its
/// own, so an inner layer can never widen them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cors {
    origins: Origins,
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
    /// Each entry is `http://` or `https://`, a host and an optional port.
    /// `INVALID_ARGUMENT` for a path, query, fragment or user info, `*` (use
    /// [`any_origin`](Self::any_origin)), `null`, another scheme, a
    /// malformed host or port, and for an empty list. The error names the
    /// entry by its position, counting from 1 (`origin #2`), never by its
    /// text, which may hold a credential.
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

    /// Read the rules from `<prefix>ORIGINS` (`*` or a comma-separated
    /// list), `<prefix>CREDENTIALS` (bool, default false), `<prefix>MAX_AGE`
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
    /// Needs an origin list.
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

/// The rules of a validated [`Cors`] as ready-made header values.
#[derive(Debug)]
struct Policy {
    /// `None` for every origin, else the normalized list.
    origins: Option<Vec<HeaderValue>>,
    credentials: bool,
    methods: Option<HeaderValue>,
    allow_headers: Option<HeaderValue>,
    expose_headers: Option<HeaderValue>,
    max_age: HeaderValue,
}

impl Policy {
    fn new(cors: &Cors) -> Self {
        let origins = match &cors.origins {
            Origins::Any => None,
            Origins::List(list) => Some(
                list.iter()
                    .filter_map(|origin| HeaderValue::from_str(origin).ok())
                    .collect(),
            ),
        };
        Self {
            origins,
            credentials: cors.credentials,
            methods: joined(cors.methods.iter().map(Method::as_str)),
            allow_headers: joined(cors.allow_headers.iter().map(String::as_str)),
            expose_headers: joined(cors.expose_headers.iter().map(String::as_str)),
            max_age: HeaderValue::from(cors.max_age.as_secs()),
        }
    }

    /// The `Access-Control-Allow-Origin` value for a request's `Origin`, or
    /// `None` when the request may not read the answer.
    fn allow_origin(&self, origin: Option<&HeaderValue>) -> Option<HeaderValue> {
        let origin = origin?;
        match &self.origins {
            None => Some(HeaderValue::from_static("*")),
            Some(list) => list
                .iter()
                .any(|listed| listed.as_bytes() == origin.as_bytes())
                .then(|| origin.clone()),
        }
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
    let allowed = policy.allow_origin(request.headers().get(ORIGIN));
    if is_preflight(&request) {
        return policy.preflight(allowed);
    }
    let mut response = next.run(request).await;
    policy.decorate(response.headers_mut(), allowed);
    response
}

fn is_preflight<B>(request: &Request<B>) -> bool {
    request.method() == Method::OPTIONS
        && request.headers().contains_key(ORIGIN)
        && request
            .headers()
            .contains_key(ACCESS_CONTROL_REQUEST_METHOD)
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

fn normalize_origin(raw: &str) -> Result<String, &'static str> {
    let origin = raw.trim();
    if origin == "*" {
        return Err("* is only allowed alone (Cors::any_origin)");
    }
    if origin.eq_ignore_ascii_case("null") {
        return Err("the opaque origin null is never allowed");
    }
    let Some((scheme, rest)) = origin.split_once("://") else {
        return Err("an origin is http:// or https:// followed by a host");
    };
    let scheme = scheme.to_ascii_lowercase();
    let default_port = match scheme.as_str() {
        "http" => 80,
        "https" => 443,
        _ => return Err("only http and https origins are allowed"),
    };
    let authority = rest.strip_suffix('/').unwrap_or(rest);
    if authority.contains(['/', '?', '#']) {
        return Err("an origin has no path, query or fragment");
    }
    if authority.contains('@') {
        return Err("an origin has no user info");
    }
    let (host, port) = split_host_port(authority)?;
    let port = port.map(parse_port).transpose()?;
    Ok(match port.filter(|&port| port != default_port) {
        Some(port) => format!("{scheme}://{host}:{port}"),
        None => format!("{scheme}://{host}"),
    })
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

fn parse_port(port: &str) -> Result<u16, &'static str> {
    if port.is_empty() || port.len() > 5 || !port.bytes().all(|b| b.is_ascii_digit()) {
        return Err("the port must be a number from 1 to 65535");
    }
    match port.parse::<u16>() {
        Ok(port) if port > 0 => Ok(port),
        _ => Err("the port must be a number from 1 to 65535"),
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
    use axum::body::Body;
    use axum::routing::get;
    use http::{Request, StatusCode};
    use sekvent_config::MapSource;
    use sekvent_error::ErrorCode;
    use tower::ServiceExt;

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
        assert!(cors_names(&response).is_empty());
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
}
