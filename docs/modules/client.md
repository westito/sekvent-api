# Outbound HTTP client (`sekvent::client`)

`sekvent::client` is the outbound HTTP client: a thin layer over `reqwest`
that puts a resilience policy (timeout, retries, optionally a breaker, a
bulkhead and a rate gate) around every request, propagates the call
context (request id, deadline, trace), maps every upstream failure to an
`AppError` without copying a third-party upstream's body into it (only a
declared sekvent upstream's error envelope is adopted), keeps
credentials on the origin they were meant for, and caches OAuth 2.0
client-credentials tokens. It lives in the crate `sekvent-client` and is
re-exported by the facade as `sekvent::client`.

Use it for every call to another HTTP service, whether that is a
third-party API (a payments provider) or another sekvent service. Use a
component binding instead when both sides are sekvent components (see
[components.md](components.md)).

## Enable it

```toml
[dependencies]
sekvent-api = { git = "https://github.com/westito/sekvent-api", branch = "master", features = ["client"] }
```

Add `resilience` as well when you build your own policies (`Policy`,
`PolicySpec`, `RetryPolicy`, …); the `client` feature alone does not
expose the `sekvent::resilience` module:

```toml
sekvent-api = { git = "https://github.com/westito/sekvent-api", branch = "master", features = ["client", "resilience"] }
```

The default facade features (`config`, `error`, `context`, `telemetry`,
`runtime`) give you `Secret`, `AppError` and `CallContext`. A path
dependency on a checkout works the same way; see
[getting-started.md](../getting-started.md) and [features.md](../features.md).

```rust
use sekvent::client::{HttpClient, HttpClientBuilder, RequestBuilder, HttpResponse};
use sekvent::client::{BearerSource, BuildError, RedirectPolicy};
use sekvent::client::{code_for_status, parse_retry_after, reqwest_builder};
use sekvent::client::oauth2::{ClientAuthStyle, ClientCredentials, ClientCredentialsBuilder};
```

The client needs a tokio runtime. It re-exports neither `reqwest` nor
`http`: for `HttpClient::request` you name the method with `http::Method`
(http 1.x, the same type as `reqwest::Method`).

## Quick example

```rust
use std::time::Duration;

use sekvent::client::HttpClient;
use sekvent::config::Secret;
use sekvent::context::CallContext;
use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize)]
struct Invoice {
    id: u64,
    total_cents: i64,
}

#[derive(Serialize)]
struct NewInvoice<'a> {
    order_id: &'a str,
    total_cents: i64,
}

async fn demo(api_key: Secret) -> Result<(), Box<dyn std::error::Error>> {
    // Build once at startup and share; clones are cheap.
    let billing = HttpClient::builder()
        .base_url("https://billing.example/api")
        .bearer_token(api_key)
        .build()?;

    let ctx = CallContext::new().with_timeout(Duration::from_secs(2));

    // GET is idempotent: retried on transient failures within the deadline.
    let invoice: Invoice = billing.get("/invoices/42").send_json(&ctx).await?;

    // POST is not retried unless you say so, and you should only say so
    // when the upstream deduplicates, e.g. by an idempotency key.
    let created: Invoice = billing
        .post("/invoices")
        .json(&NewInvoice { order_id: "o-7", total_cents: 1299 })
        .header("idempotency-key", "o-7-invoice")
        .idempotent(true)
        .send_json(&ctx)
        .await?;

    println!("{} {}", invoice.id, created.total_cents);
    Ok(())
}
```

## Concepts

- **`HttpClient`** owns a `reqwest` connection pool, the base URL, the
  credentials and a resilience `Policy`. It is `Clone`; clones share the
  pool and the policy state (retry budget, breaker, bulkhead, rate gate).
  Build one per upstream at startup and inject it (constructor injection,
  see [components.md](components.md) and [runtime.md](runtime.md)).
- **`RequestBuilder`** describes one request. Nothing is sent until `send`
  or `send_json`; mistakes made while building (a bad header, a body that
  does not serialize, a path off the base URL) are kept and returned by
  `send` as an `AppError`.
- **One logical request, several attempts.** `send` runs the request under
  the policy. Each attempt re-reads the context (the remaining deadline is
  re-encoded), fetches a bearer token if needed, sends, reads the whole
  body and maps the outcome. Only idempotent requests are ever retried, and
  only on transient errors.
- **`HttpResponse`** is a successful response (`2xx`, or a `3xx` that was
  not followed) with the body already read into memory. Every other status
  is an `Err(AppError)`.
- **Third-party versus sekvent upstreams.** By default an upstream is
  treated as a third party: it sees only the "external" part of the
  context, its error bodies are ignored, and a `401`/`403` from it is this
  service's own problem (`INTERNAL`). `sekvent_upstream(true)` declares the
  upstream a sekvent service: it also receives subject, tenant and call
  depth, and its JSON error envelope is adopted as the error.
- **Errors are `AppError`.** By default codes come from a fixed status
  table and the upstream body never reaches the message, the metadata or
  the wire form. The one exception is a `sekvent_upstream(true)` client: a
  valid sekvent error envelope from that upstream is adopted, so its code,
  message, reason, domain, metadata and field violations pass through (see
  [Sekvent JSON error envelopes](#sekvent-json-error-envelopes)). See
  [error.md](error.md).

## How to build a client

```rust
impl HttpClient {
    pub fn builder() -> HttpClientBuilder;
}

impl HttpClientBuilder {
    pub fn base_url(self, url: impl Into<String>) -> Self;
    pub fn connect_timeout(self, timeout: Duration) -> Self;
    pub fn request_timeout(self, timeout: Duration) -> Self;
    pub fn read_timeout(self, timeout: Duration) -> Self;
    pub fn user_agent(self, agent: impl Into<String>) -> Self;
    pub fn default_header(self, name: impl Into<String>, value: impl Into<String>) -> Self;
    pub fn policy(self, policy: Policy) -> Self;
    pub fn basic_auth(self, username: impl Into<String>, password: Secret) -> Self;
    pub fn bearer_token(self, token: Secret) -> Self;
    pub fn with_bearer_source(self, source: Arc<dyn BearerSource>) -> Self;
    pub fn clock(self, clock: Arc<dyn Clock>) -> Self;
    pub fn propagate_context(self, propagate: bool) -> Self;
    pub fn sekvent_upstream(self, sekvent: bool) -> Self;
    pub fn redirects(self, policy: RedirectPolicy) -> Self;
    pub fn build(self) -> Result<HttpClient, BuildError>;
}
```

| Setting | Default | Notes |
|---|---|---|
| `base_url` | none | Absolute `http`/`https` URL with a host; a trailing `/` is implied. |
| `connect_timeout` | 10 s | Connection establishment. Zero is rejected. |
| `request_timeout` | 30 s | Each HTTP send, always capped by the call deadline. An attempt that re-authenticates after a `401` sends twice. Zero is rejected. |
| `read_timeout` | none | Longest pause between reads of the response. Zero is rejected. |
| `user_agent` | `sekvent-client/<version>` | Must be a valid header value. |
| `default_header` | none | Sent with every request; values are marked sensitive. |
| `policy` | timeout + default retries | See [policies](#how-to-choose-a-resilience-policy). |
| auth | none | `basic_auth`, `bearer_token` or `with_bearer_source`; the last call wins. |
| `clock` | `SystemClock` | Only used to turn a `Retry-After` date into a delay. |
| `propagate_context` | `true` | See [context propagation](#how-context-propagates). |
| `sekvent_upstream` | `false` | See [error mapping](#how-upstream-errors-are-mapped). |
| `redirects` | `RedirectPolicy::SameOrigin` | See [redirects](#how-redirects-are-handled). |

`build` fails closed; messages name the setting, never its value (a URL
may carry credentials):

| `BuildError` | Message |
|---|---|
| `InvalidBaseUrl` | `the base URL is not a valid absolute http(s) URL` |
| `InvalidTimeout { name }` | `the {name} must be longer than zero` (`connect timeout`, `request timeout`, `read timeout`) |
| `InvalidHeader { name }` | `the default header {name} is invalid` (also `user-agent`) |
| `Tls(_)` | `the TLS configuration could not be initialised` (e.g. no platform root certificates) |
| `Backend(_)` | `the HTTP client could not be initialised` |

`BuildError` is `#[non_exhaustive]`; `InvalidTokenUrl` and
`InsecureTokenUrl` come from the OAuth 2.0 builder.

### Base URL and paths

With a base URL, request paths are resolved under it: leading slashes are
stripped and the path is joined, so `https://billing.example/api` plus
`/invoices/42` is `https://billing.example/api/invoices/42`. The resolved
URL must stay on the base URL's origin (scheme, host and port); an absolute
URL to another host fails with `INVALID_ARGUMENT` (`the request path must
stay on the base URL's host`). A scheme-relative `//other.example/x` is
treated as a relative path and stays on the base host. Only the origin is
enforced, not the path prefix: `../` can leave the base path on the same
host.

Without a base URL every path must be an absolute `http`/`https` URL;
anything else (a relative path, `file://`) is `INVALID_ARGUMENT`.

### Timeouts

Three limits apply to every attempt, and the call deadline caps all of
them:

1. `connect_timeout` bounds connection setup.
2. `request_timeout` bounds each HTTP send, from connecting until the body
   has been read. It is enforced by `reqwest` per send as
   `min(request_timeout, time left before the deadline)`. An attempt
   normally sends once, but with a `BearerSource` a `401` makes it refresh
   the token and send a second time, so the `reqwest` limit alone allows
   up to twice `request_timeout` (plus the token fetches) per attempt. In
   the default policy the policy's per-attempt `Timeout`, also
   `request_timeout`, bounds the whole attempt, refresh included.
3. `read_timeout`, when set, bounds each pause between reads.

A custom `policy(..)` replaces the policy timeout, but the `reqwest`-level
`request_timeout` (30 s unless you change it) still applies to each send
(two in an attempt that re-authenticates after a `401`); it can be raised but not switched off. Give every call a deadline
(`CallContext::with_timeout`, or the inbound call's context) so the total
time across retries is bounded too.

## How to choose a resilience policy

Without `.policy(..)` the client gets a policy named `http`:

- a per-attempt `Timeout` of `request_timeout`;
- `RetryPolicy::default()`: three attempts in total, exponential backoff
  from 100 ms doubling up to 5 s with full jitter, a retry budget (0.2
  tokens per success, a floor of 10 retries per second, at most 100 banked
  tokens), and `Retry-After` hints honoured up to 30 s;
- no breaker, bulkhead or rate gate.

The policy belongs to the client, so its retry budget is shared by all
clones of that client and by nothing else. `HttpClient::policy(&self) ->
&Policy` returns it.

A retry happens only when every one of these holds (see
[resilience.md](resilience.md)):

- the request is idempotent (method default or `.idempotent(true)`);
- the error is transient: `UNAVAILABLE`, `DEADLINE_EXCEEDED`,
  `RESOURCE_EXHAUSTED` or `ABORTED`;
- attempts are left;
- the error's `retry_after`, if any, is at most the policy's
  `max_retry_after` (default 30 s); a longer hint returns the error at once;
- the delay, the larger of backoff and `retry_after`, ends before the
  deadline;
- the retry budget has a token.

Build your own policy with the `resilience` feature. From configuration
(the usual way):

```rust
use sekvent::client::HttpClient;
use sekvent::config::EnvSource;
use sekvent::resilience::PolicySpec;

// Reads PAYMENTS_TIMEOUT, PAYMENTS_RETRY_MAX_ATTEMPTS, PAYMENTS_BREAKER_FAILURE_RATE, …
let policy = PolicySpec::from_config(&EnvSource, "PAYMENTS_")?.build("payments")?;
let payments = HttpClient::builder()
    .base_url("https://api.example.com/v1")
    .policy(policy)
    .build()?;
```

Or in code:

```rust
use std::sync::Arc;
use std::time::Duration;

use sekvent::resilience::{
    Backoff, CircuitBreaker, CircuitBreakerConfig, Policy, RetryPolicy, Timeout,
};

let policy = Policy::new("inventory")
    .with_timeout(Timeout::new(Duration::from_secs(2)))
    .with_retry(RetryPolicy::new(4, Backoff::default()))
    .with_breaker(Arc::new(CircuitBreaker::new(
        "inventory",
        CircuitBreakerConfig::default(),
    )?));
```

`PolicySpec::build` creates fresh state (budget, breaker, …): build once
per upstream and share the client. `Policy::new(name)` alone only enforces
the call deadline and never retries.

A single request can use a different policy:

```rust
let report = billing
    .get("/reports/monthly")
    .policy(Policy::new("billing-reports").with_timeout(Timeout::new(Duration::from_secs(60))))
    .send(&ctx)
    .await?;
```

The per-request policy replaces the client's for that request only;
remember that the client's `request_timeout` still caps each send.

## How to send requests and read responses

```rust
impl HttpClient {
    pub fn request(&self, method: Method, path: &str) -> RequestBuilder;
    pub fn get(&self, path: &str) -> RequestBuilder;     // idempotent
    pub fn head(&self, path: &str) -> RequestBuilder;    // idempotent
    pub fn post(&self, path: &str) -> RequestBuilder;    // not idempotent
    pub fn put(&self, path: &str) -> RequestBuilder;     // idempotent
    pub fn patch(&self, path: &str) -> RequestBuilder;   // not idempotent
    pub fn delete(&self, path: &str) -> RequestBuilder;  // idempotent
    pub fn policy(&self) -> &Policy;
}

impl RequestBuilder {
    pub fn json<T: Serialize + ?Sized>(self, body: &T) -> Self;
    pub fn body(self, body: impl Into<Bytes>, content_type: &str) -> Self;
    pub fn query<K: AsRef<str>, V: AsRef<str>>(self, pairs: &[(K, V)]) -> Self;
    pub fn header(self, name: &str, value: &str) -> Self;
    pub fn idempotent(self, idempotent: bool) -> Self;
    pub fn policy(self, policy: Policy) -> Self;
    pub async fn send(self, ctx: &CallContext) -> Result<HttpResponse, AppError>;
    pub async fn send_json<R: DeserializeOwned>(self, ctx: &CallContext) -> Result<R, AppError>;
}

impl HttpResponse {
    pub fn status(&self) -> StatusCode;
    pub fn headers(&self) -> &HeaderMap;
    pub fn body(&self) -> &Bytes;
    pub fn into_body(self) -> Bytes;
    pub fn text(&self) -> Result<&str, AppError>;
    pub fn json<T: DeserializeOwned>(&self) -> Result<T, AppError>;
}
```

- `json` serializes the body and sets `content-type: application/json`; a
  serialization failure is `INTERNAL` (`the request body could not be
  serialized`) at `send`.
- `body` sets a raw body and its content type; an invalid content type is
  `INVALID_ARGUMENT` (`invalid content type`).
- `query` appends percent-encoded pairs (`("q", "a b&c")` becomes
  `q=a+b%26c`); call it as often as you like.
- `header` appends a header; an invalid name or value is
  `INVALID_ARGUMENT` (`invalid request header`). `authorization`,
  `proxy-authorization` and `cookie` are marked sensitive.
- `idempotent` overrides the method default (`GET`, `HEAD`, `PUT`,
  `DELETE`, `OPTIONS`, `TRACE` are idempotent; `POST`, `PATCH` and others
  are not). Only idempotent requests are retried.
- Builder mistakes are returned by `send`, before anything is sent. Among
  `json`, `body` and `header` the first mistake wins, and any of them wins
  over a bad path or URL given to `get`/`request`, which `send` reports
  only when the builder calls themselves were fine.

`send` returns the response for `2xx` and for a `3xx` that was not
followed; every other status is an error (see below). `send_json` is `send`
followed by `HttpResponse::json`.

Reading the body:

```rust
use http::Method;

let response = inventory
    .request(Method::OPTIONS, "/stock")
    .send(&ctx)
    .await?;
let allowed = response.headers().get("allow").cloned();

let csv = billing.get("/exports/latest.csv").send(&ctx).await?;
let text: &str = csv.text()?; // INTERNAL if not UTF-8
```

`HttpResponse::json` and `text` fail with `INTERNAL` (`the upstream
response body could not be decoded`, `the upstream response body is not
valid UTF-8`). The JSON error's source describes only the position
(`JSON data error at line 1 column 7`), never the payload.

The whole body is read into memory before `send` returns; there is no
streaming API and no size cap. Do not point the client at endpoints that
can return unbounded bodies.

## How redirects are handled

```rust
#[non_exhaustive]
pub enum RedirectPolicy {
    SameOrigin, // default
    None,
}
```

Default headers, credentials and identity headers travel with every
followed redirect, so the client never follows one off the original
origin:

- `SameOrigin` follows up to five redirects that keep scheme, host and
  port of the first request. A redirect elsewhere, or a sixth one, is not
  followed: the `3xx` response is returned as `Ok(HttpResponse)`.
- `None` never follows; every `3xx` is returned as is.

Check `response.status()` when a `3xx` is possible.

## How to authenticate to the upstream

```rust
// HTTP Basic
let client = HttpClient::builder()
    .base_url("https://api.example.com")
    .basic_auth("orders-service", password)
    .build()?;

// A fixed bearer token or API key
let client = HttpClient::builder()
    .base_url("https://api.example.com")
    .bearer_token(api_key)
    .build()?;

// A header-based API key
let client = HttpClient::builder()
    .base_url("https://api.example.com")
    .default_header("x-api-key", api_key.expose())
    .build()?;
```

Credentials are `Secret`s (see [config.md](config.md)); `Debug` of the
client shows only the base host and the policy name.

For rotating tokens, implement `BearerSource`:

```rust
pub trait BearerSource: Send + Sync + 'static {
    fn token<'a>(&'a self, ctx: &'a CallContext) -> BoxFuture<'a, Result<Secret, AppError>>;
    fn invalidate(&self);
}
```

`BoxFuture` is `futures::future::BoxFuture`. The client calls `token`
before every send and puts the result into `Authorization: Bearer …`. When
the upstream answers `401`, it calls `invalidate`, fetches a token again
and resends the request **once**, within the same attempt and also for
non-idempotent methods (a `401` means the request was not processed). A
second `401` is mapped like any other status (`INTERNAL` for a third-party
upstream). An error from `token` fails the attempt with that error.

`oauth2::ClientCredentials` is the built-in implementation; see
[OAuth 2.0](#how-to-use-oauth-20-client-credentials).

## How context propagates

Each attempt adds the call context to the request headers unless
`propagate_context(false)` is set. What is sent depends on whether the
upstream is declared a sekvent service:

| Header | Third-party upstream (default) | `sekvent_upstream(true)` |
|---|---|---|
| `x-request-id` | sent | sent |
| `grpc-timeout` (time left before the deadline) | sent when the context has a deadline | sent when the context has a deadline |
| `traceparent` | sent when set | sent when set |
| `x-sekvent-subject` | never | sent when set |
| `x-sekvent-tenant` | never | sent when set |
| `x-sekvent-hops` (component call depth) | never | sent when above 0 |
| `idempotency-key` | never | never |

Third-party upstreams get only the external headers
(`sekvent::context::headers::propagate_external`): request id, trace
context and timeout, never the end-user identity or the hop count. A
sekvent peer gets the full set (`sekvent::context::headers::propagate`).
See [context.md](context.md).

Rules:

- A header you set on the request wins over the context, so
  `.header("x-request-id", "…")` is sent as given.
- `grpc-timeout` is the exception: a value you set that is longer than the
  time the context has left, or that does not parse, is replaced by the
  remaining time. A callee never gets more time than its caller has.
- The remaining time is recomputed for every attempt.
- The context's idempotency key is never sent, not even one set with
  `CallContext::with_idempotency_key` for this call. A key identifies one
  request to one service; set it per request with
  `.header("idempotency-key", …)` when the upstream wants one.
- `propagate_context(false)` sends none of these headers, for APIs that
  should not see even the request id. The deadline is still enforced
  locally.
- Requests to an OAuth 2.0 token endpoint carry no context headers.

## How upstream errors are mapped

### Status codes

A response that is neither `2xx` nor `3xx` becomes an `AppError`. The plain
status table is public:

```rust
pub fn code_for_status(status: StatusCode) -> ErrorCode;
```

| Upstream status | `ErrorCode` | Transient (retried when idempotent) |
|---|---|---|
| 400, 413, 414, 415, 422, 431 | `INVALID_ARGUMENT` | no |
| 401 | `INTERNAL` (third party) / `UNAUTHENTICATED` (sekvent upstream) | no |
| 403 | `INTERNAL` (third party) / `PERMISSION_DENIED` (sekvent upstream) | no |
| 404, 410 | `NOT_FOUND` | no |
| 405, 501 | `UNIMPLEMENTED` | no |
| 408, 504 | `DEADLINE_EXCEEDED` | yes |
| 409 | `ALREADY_EXISTS` | no |
| 416 | `OUT_OF_RANGE` | no |
| 429 | `RESOURCE_EXHAUSTED` | yes |
| 499 | `CANCELLED` | no |
| 502, 503 | `UNAVAILABLE` | yes |
| other 4xx | `FAILED_PRECONDITION` | no |
| other 5xx | `INTERNAL` | no |
| anything else | `UNKNOWN` | no |

`code_for_status` itself returns `UNAUTHENTICATED` and `PERMISSION_DENIED`
for 401 and 403 (and `OK` for 200–399); the client refines 401/403 as
shown. A `409` is `ALREADY_EXISTS` because a conflict is about the request,
so the same request is not repeated.

**Why `401`/`403` become `INTERNAL`.** When a third-party upstream rejects
a request as unauthenticated or forbidden, it is rejecting this service's
own credentials or configuration, not the end user. Passing
`UNAUTHENTICATED` up would tell your caller to log in again, which cannot
help. `INTERNAL` is not transient, so it is not retried (apart from the
single re-authentication with a `BearerSource`). With
`sekvent_upstream(true)` the codes are kept, because a sekvent peer
receives the end user's subject and may really be refusing them.

### The error you get

For a status-mapped error:

- message: `upstream responded with HTTP {status}` (never the body);
- reason: `UPSTREAM_HTTP_ERROR`;
- metadata: `upstream_status` = the status number as a string;
- `retry_after`: from the `Retry-After` header, when present and valid;
- source (internal only, never on the wire): the shape of the response,
  e.g. `upstream HTTP 503 response with a 4013-byte body of type
  text/plain`. The content type is cut to its essence (no parameters) and
  to 64 characters with `truncate_for_log`.

### Sekvent JSON error envelopes

With `sekvent_upstream(true)`, a body in the sekvent JSON error format
(what an `AppError` renders over HTTP, see [error.md](error.md)) is adopted
as the error: code, message, reason, domain, metadata and field violations
reach your callers unchanged. The envelope is accepted only when its code
is known, is not `OK`, and its conventional HTTP status equals the
response status; otherwise the response is mapped from its status as
above. When the envelope carries no `retry_after_ms`, the `Retry-After`
header is used.

Without `sekvent_upstream(true)` every error body is ignored, including a
well-formed sekvent envelope: an undeclared upstream is not trusted to
choose your error messages.

### Retry-After

```rust
pub fn parse_retry_after(value: &str, now: SystemTime) -> Option<Duration>;
```

The value may be delay seconds (surrounding whitespace allowed) or an
HTTP-date measured from `now`; the client passes its `clock`. A date in the
past gives zero, garbage (including negative numbers) gives `None`, and a
number too large for `u64` saturates to `u64::MAX` seconds, so the retry
policy treats it as "too long to wait" rather than "no hint". The parsed
value is set as the error's `retry_after`, which the retry policy honours up
to `max_retry_after` (30 s by default) and never past the deadline. Your
own callers see it too (e.g. as `Retry-After` when the error is rendered
over HTTP).

### Transport and other failures

| Failure | Code | Message |
|---|---|---|
| Attempt timed out (`reqwest`) | `DEADLINE_EXCEEDED` | `the upstream request timed out` |
| Policy timeout fired | `DEADLINE_EXCEEDED` | `the call did not complete in time` |
| Connection refused or failed to connect | `UNAVAILABLE` | `could not connect to the upstream` |
| Connection broke while sending or reading | `UNAVAILABLE` | `the upstream connection failed` |
| Request could not be built, response could not be decoded | `INTERNAL` | `the upstream request could not be processed` |
| Context already cancelled or past its deadline | `CANCELLED` / `DEADLINE_EXCEEDED` | from the policy, before anything is sent |

The transport error is attached as the source with its URL stripped (a
query may carry secrets).

## How to use OAuth 2.0 client credentials

`oauth2::ClientCredentials` fetches tokens with the client-credentials
grant (RFC 6749 §4.4), caches them and implements `BearerSource`.

```rust
use std::sync::Arc;
use std::time::Duration;

use sekvent::client::HttpClient;
use sekvent::client::oauth2::{ClientAuthStyle, ClientCredentials};
use sekvent::config::Secret;

let creds = ClientCredentials::builder(
    "https://auth.example.com/oauth/token",
    "orders-service",
    client_secret, // Secret
)
.scope("payments:read payments:write")
.audience("https://api.example.com")
.auth_style(ClientAuthStyle::Basic)
.refresh_skew(Duration::from_secs(30))
.build()?;

let payments = HttpClient::builder()
    .base_url("https://api.example.com/v1")
    .with_bearer_source(Arc::new(creds))
    .build()?;
```

```rust
impl ClientCredentials {
    pub fn builder(
        token_url: impl Into<String>,
        client_id: impl Into<String>,
        client_secret: Secret,
    ) -> ClientCredentialsBuilder;
    pub async fn token(&self, ctx: &CallContext) -> Result<Secret, AppError>;
    pub fn invalidate(&self);
}

impl ClientCredentialsBuilder {
    pub fn scope(self, scope: impl Into<String>) -> Self;
    pub fn audience(self, audience: impl Into<String>) -> Self;
    pub fn auth_style(self, style: ClientAuthStyle) -> Self;
    pub fn refresh_skew(self, skew: Duration) -> Self;
    pub fn default_ttl(self, ttl: Duration) -> Self;
    pub fn timeout(self, timeout: Duration) -> Self;
    pub fn clock(self, clock: Arc<dyn Clock>) -> Self;
    pub fn build(self) -> Result<ClientCredentials, BuildError>;
}

#[non_exhaustive]
pub enum ClientAuthStyle {
    Basic,       // default: client_secret_basic
    RequestBody, // client_secret_post
}
```

| Setting | Default | Notes |
|---|---|---|
| `scope` | none | Space-separated scopes, sent as `scope`. |
| `audience` | none | Sent as `audience` (a common extension). |
| `auth_style` | `Basic` | `Basic`: HTTP Basic with id and secret form-encoded first (RFC 6749 §2.3.1). `RequestBody`: `client_id` and `client_secret` in the form. |
| `refresh_skew` | 30 s | Refresh this long before expiry. |
| `default_ttl` | 5 min | Lifetime assumed when the response has no `expires_in`. |
| `timeout` | 10 s | One token request, also capped by the call deadline. Zero is rejected. |
| `clock` | `SystemClock` | Decides when a cached token is due. |

`build` validates the token URL:

- not an absolute `http`/`https` URL with a host: `BuildError::InvalidTokenUrl`;
- plain `http` to anything but a loopback host (`localhost`,
  `127.0.0.0/8`, `::1`): `BuildError::InsecureTokenUrl`, because the
  client secret and tokens would travel in clear text;
- zero timeout: `BuildError::InvalidTimeout { name: "token request timeout" }`.

### The token request

A `POST` to the token URL with `content-type:
application/x-www-form-urlencoded`, `accept: application/json` and the
form `grant_type=client_credentials` plus `scope` and `audience` when set
(and the credentials with `RequestBody`). The token endpoint is never
redirected to: a `3xx` is a failure, so the secret is never replayed to
another URL. No call-context headers are sent.

The response must be JSON with a non-blank `access_token`. `token_type`
may be absent; if present it must be `bearer` (any case). `expires_in` may
be a number or a numeric string.

### Caching and refresh

- A token is served from the cache for `expires_in − refresh_skew`, but at
  least half of `expires_in`, so a token that lives shorter than the skew is
  still reused. Lifetimes above one day are treated as one day.
  - 60 s token, 10 s skew: refreshed after 50 s.
  - 60 s token, 90 s skew: refreshed after 30 s.
  - `expires_in: 0`: never cached; every request fetches.
- **Single flight**: concurrent callers that find no valid token wait for
  one request to the endpoint and all get its token. A failed fetch is not
  cached; the next waiting caller tries the endpoint itself.
- `invalidate` drops the cached token. `HttpClient` calls it after a `401`
  and resends once with a fresh token.
- Tokens are `Secret`s; `Debug` of `ClientCredentials` shows the token
  host, the client id and the style, never the secret or a token. A debug
  event records only the token lifetime.

### Token failures

| Situation | Code | Reason |
|---|---|---|
| Endpoint answers 408, 429, 502, 503, 504 | `DEADLINE_EXCEEDED`, `RESOURCE_EXHAUSTED`, `UNAVAILABLE` (transient) | `TOKEN_REQUEST_FAILED` |
| Endpoint answers any other non-`2xx` (including 400/401 for bad credentials, and redirects) | `INTERNAL` | `TOKEN_REQUEST_FAILED` |
| Body is not the expected JSON | `INTERNAL` | `TOKEN_REQUEST_FAILED` |
| `token_type` is not bearer | `INTERNAL` | `TOKEN_REQUEST_FAILED` |
| Empty `access_token` | `INTERNAL` | `TOKEN_REQUEST_FAILED` |
| Timeout or connection failure | `DEADLINE_EXCEEDED` / `UNAVAILABLE` | none |

A rejection at the token endpoint means this service's credentials or
configuration are wrong, so it is `INTERNAL`, never the end user's
`UNAUTHENTICATED`. The message is `the token endpoint responded with HTTP
{status}`; the endpoint's `error_description` is never copied. When the
token fetch fails inside `HttpClient::send`, the attempt fails with that
error, and a transient one is retried like any other for idempotent
requests.

## What is logged (and what never is)

- Each `send` opens a debug-level span `http_client_request` with `method`,
  `host`, `path` (no query), `status` and `elapsed_ms`, and ends with a
  debug event `http request completed` or `http request failed` (with
  `code`). The retry policy logs each retry at debug level.
- Headers, query strings and bodies are never logged. Default headers are
  marked sensitive; so are `authorization`, `proxy-authorization` and
  `cookie` set on a request.
- Status-mapped errors never contain the upstream body: only the status,
  content type and length reach the internal source chain, and JSON
  decoding failures report only line and column. The exception is an
  adopted envelope from a `sekvent_upstream(true)` peer, whose code,
  message, reason, domain, metadata and field violations are passed
  through as the error (see
  [Sekvent JSON error envelopes](#sekvent-json-error-envelopes)); declare
  only upstreams you trust to write your callers' error messages.
- `Debug` of `HttpClient`, `RequestBuilder` and `HttpResponse` shows the
  host and policy name, method/path/idempotency, and status/body length
  respectively.

If you must log something taken from a response yourself, bound it with
`sekvent::telemetry::truncate_for_log(s: &str, max_chars: usize) ->
Cow<'_, str>` (see [telemetry.md](telemetry.md)). Truncation limits size;
it does not make secrets or personal data safe to log.

## Configuration

`sekvent-client` reads no configuration and defines no `SEKVENT_*` keys:
the service decides its keys and passes values to the builders. A typical
setup with `EnvConfig` (see [config.md](config.md)) and a `PolicySpec`
(see [resilience.md](resilience.md)):

```rust
use std::sync::Arc;
use std::time::Duration;

use sekvent::client::HttpClient;
use sekvent::client::oauth2::ClientCredentials;
use sekvent::config::EnvSource;
use sekvent::prelude::*;
use sekvent::resilience::PolicySpec;

/// Settings of the payments provider client.
#[derive(Debug, EnvConfig)]
#[config(prefix = "PAYMENTS_")]
pub struct PaymentsConfig {
    /// Base URL of the payments API.
    pub base_url: String,
    /// OAuth 2.0 token endpoint.
    pub token_url: String,
    /// OAuth 2.0 client id.
    pub client_id: String,
    /// OAuth 2.0 client secret.
    pub client_secret: Secret,
    /// Requested scopes, space-separated.
    pub scope: Option<String>,
    /// Per-attempt timeout.
    #[config(default = "10s")]
    pub request_timeout: Duration,
}

fn payments_client() -> Result<HttpClient, Box<dyn std::error::Error>> {
    let config: PaymentsConfig = sekvent::config::from_env()?;
    let mut creds = ClientCredentials::builder(
        config.token_url,
        config.client_id,
        config.client_secret,
    );
    if let Some(scope) = config.scope {
        creds = creds.scope(scope);
    }
    let policy = PolicySpec::from_config(&EnvSource, "PAYMENTS_")?.build("payments")?;
    Ok(HttpClient::builder()
        .base_url(config.base_url)
        .request_timeout(config.request_timeout)
        .policy(policy)
        .with_bearer_source(Arc::new(creds.build()?))
        .build()?)
}
```

This reads `PAYMENTS_BASE_URL`, `PAYMENTS_TOKEN_URL`,
`PAYMENTS_CLIENT_ID`, `PAYMENTS_CLIENT_SECRET`, `PAYMENTS_SCOPE` (optional)
and `PAYMENTS_REQUEST_TIMEOUT` (default `10s`) from the struct, and the
policy keys `PolicySpec::CONFIG_KEYS` under the same prefix:
`PAYMENTS_TIMEOUT`, `PAYMENTS_RETRY_MAX_ATTEMPTS`,
`PAYMENTS_RETRY_INITIAL_BACKOFF`, `PAYMENTS_RETRY_MAX_BACKOFF`,
`PAYMENTS_RETRY_MULTIPLIER`, `PAYMENTS_RETRY_JITTER`,
`PAYMENTS_RETRY_BUDGET_RATIO`, `PAYMENTS_RETRY_BUDGET_MIN_PER_SEC`,
`PAYMENTS_RETRY_MAX_RETRY_AFTER`, `PAYMENTS_BULKHEAD_MAX_CONCURRENT`,
`PAYMENTS_BULKHEAD_MAX_QUEUE`, `PAYMENTS_BULKHEAD_QUEUE_TIMEOUT`,
`PAYMENTS_BREAKER_ENABLED`, `PAYMENTS_BREAKER_FAILURE_RATE`,
`PAYMENTS_BREAKER_WINDOW`, `PAYMENTS_BREAKER_MIN_CALLS`,
`PAYMENTS_BREAKER_WAIT_IN_OPEN`, `PAYMENTS_BREAKER_PERMITTED_IN_HALF_OPEN`,
`PAYMENTS_RATE_LIMIT_PERMITS`, `PAYMENTS_RATE_LIMIT_WINDOW`. Their
defaults and validation are in [resilience.md](resilience.md). Note that a
policy built from a spec without `RETRY_MAX_ATTEMPTS` above 1 does not
retry, unlike the client's default policy.

Validation happens at startup: config errors name the key, builder errors
name the setting (`BuildError`), and none of them print a value.

## How to use reqwest directly, and TLS

`HttpClient` and `ClientCredentials` use rustls with the ring crypto
provider passed explicitly, the platform certificate verifier (the
operating system's trust store) and ALPN offering HTTP/2 before
HTTP/1.1. Building them neither needs nor installs a process-wide rustls
`CryptoProvider`. There are no TLS knobs on `HttpClient` (no custom roots,
no client certificates); the preconfigured builder it uses is internal.

sekvent pins `reqwest` without a built-in crypto provider
(`rustls-no-provider`), so a bare `reqwest::Client::new()` panics. When you
need a plain `reqwest` client, for tests or for an upstream that needs its
own TLS settings, start from:

```rust
pub fn reqwest_builder() -> reqwest::ClientBuilder;
```

It installs ring as the process-wide default rustls provider unless one is
installed already (an installed provider is kept), then returns `reqwest`'s
own rustls builder, so every `reqwest` TLS setting (`tls_certs_only`,
`tls_version_min`, `identity`, `http1_only`, …) applies as documented:

```rust
let raw = sekvent::client::reqwest_builder()
    .tls_version_min(reqwest::tls::Version::TLS_1_3)
    .build()?;
```

To name `reqwest` types, depend on the same version sekvent pins without
its default features, e.g. `reqwest = { version = "0.13", default-features
= false, features = ["rustls-no-provider"] }`. A client built this way gets
none of `HttpClient`'s behaviour: no policy, no context propagation, no
error mapping, no redaction, and `reqwest`'s own redirect policy, which
follows redirects to other hosts.

## Error codes and reasons

| Where | Code | Reason | Message |
|---|---|---|---|
| Upstream non-success status | from the status table | `UPSTREAM_HTTP_ERROR` (metadata `upstream_status`) | `upstream responded with HTTP {status}` |
| Adopted sekvent envelope | the envelope's | the envelope's | the envelope's |
| Transport | `DEADLINE_EXCEEDED`, `UNAVAILABLE`, `INTERNAL` | none | see [transport failures](#transport-and-other-failures) |
| Token endpoint | `INTERNAL` or the transient status code | `TOKEN_REQUEST_FAILED` | `the token endpoint …` |
| Path off the base origin, bad path | `INVALID_ARGUMENT` | none | `the request path must stay on the base URL's host`, `the request path is not a valid URL` |
| Bad header, bad content type | `INVALID_ARGUMENT` | none | `invalid request header`, `invalid content type` |
| Body serialization | `INTERNAL` | none | `the request body could not be serialized` |
| Response decoding | `INTERNAL` | none | `the upstream response body could not be decoded`, `… is not valid UTF-8` |

The reasons are plain strings; the crate exports no constants for them.
Match on `error.code()` first and on `error.reason()` only when you need to
tell an upstream status (`UPSTREAM_HTTP_ERROR`) from a token problem
(`TOKEN_REQUEST_FAILED`).

## Testing

The crate's own tests run the client against a real axum server on an
ephemeral loopback port, which is the pattern to copy (add `axum` and
`tokio` as dev-dependencies):

```rust
use axum::Router;

/// Serve `router` on 127.0.0.1:0 and return its base URL.
async fn serve(router: Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    format!("http://{address}")
}
```

The listener is bound before `serve` returns, so the first request queues
in the accept backlog instead of racing the server start.

```rust
use axum::http::StatusCode;
use axum::routing::get;
use sekvent::client::HttpClient;
use sekvent::context::CallContext;
use sekvent::error::ErrorCode;
use sekvent::resilience::Policy;

#[tokio::test]
async fn a_503_is_unavailable() {
    let base = serve(Router::new().route("/flaky", get(|| async { StatusCode::SERVICE_UNAVAILABLE }))).await;
    let client = HttpClient::builder()
        .base_url(&base)
        .policy(Policy::new("test")) // deadline only, no retries
        .build()
        .unwrap();
    let error = client.get("/flaky").send(&CallContext::new()).await.unwrap_err();
    assert_eq!(error.code(), ErrorCode::Unavailable);
}
```

Tips:

- `Policy::new("test")` turns retries off. To test retries without
  waiting, use `RetryPolicy::new(3, Backoff::constant(Duration::from_millis(1)).with_jitter(Jitter::None))`
  and count hits on the server with an `AtomicUsize`.
- Inject `sekvent::context::ManualClock` with `.clock(..)` to test
  `Retry-After` dates, and into `ClientCredentials` to test token expiry:
  `clock.advance(..)` moves the cache forward with no sleeping.
- An OAuth 2.0 token endpoint on loopback may use `http`; anything else
  must be `https`.
- For "connection refused", use `http://127.0.0.1:1`, never a port you
  bound and dropped.
- Socket tests run on the real clock; do not use `tokio::time::pause` with
  them. Assert lower bounds and structure, and guard against hangs with a
  30 s `tokio::time::timeout`.
- To test single flight, gate the token handler on a `Semaphore`, wait on a
  `Notify` until the first request arrives, then release it and assert one
  hit.
- A test that checks the process-wide rustls provider belongs in its own
  test binary.

See [testing.md](testing.md) for containers and `await_until!`.

## Pitfalls and security

- **Do not mark a request idempotent unless repeating it is safe.**
  Retrying a `POST` that charges a card charges it twice. Mark it only with
  an idempotency key the upstream honours.
- **Give every call a deadline.** Without one, the total time is bounded
  only by attempts × `request_timeout` (under a custom policy without
  a per-attempt timeout, twice that for an attempt that re-authenticates
  after a `401`) plus backoff and token fetches.
- **Declare `sekvent_upstream(true)` only for sekvent services you
  operate.** It sends end-user subject and tenant to that host and lets it
  choose your error messages.
- **Expect `INTERNAL` for upstream `401`/`403`.** That is your
  configuration (expired API key, missing scope), not the user's; alert on
  it rather than asking users to log in.
- **Do not forward an inbound idempotency key.** The client never does; set
  a key per outbound request.
- **Check `3xx` responses.** A redirect that leaves the origin, or the
  sixth one, is returned as `Ok` with the `3xx` status.
- **Bodies are unbounded and buffered.** Only call endpoints whose response
  size you trust.
- **Never put secrets in the URL path.** The path goes into the debug span
  (the query does not), and upstreams and proxies log URLs. Use
  `bearer_token`, `basic_auth` or a default header.
- **Build clients once.** Each `build` creates a connection pool and, with
  the default policy, its own retry budget; building per request defeats
  both.
- **A custom policy does not remove `request_timeout`.** Raise it when an
  endpoint is legitimately slow.
- **Use `reqwest_builder`, not `reqwest::Client::new()`**, when you need
  raw `reqwest`, and remember it has none of the safeguards above.

## See also

- [resilience.md](resilience.md): `Policy`, `PolicySpec`, retries, breakers, budgets
- [context.md](context.md): `CallContext`, deadlines, header propagation
- [error.md](error.md): `ErrorCode`, `AppError`, the HTTP error envelope
- [config.md](config.md): `EnvConfig`, `Secret`
- [telemetry.md](telemetry.md): tracing, `truncate_for_log`
- [link.md](link.md): service-to-service tokens between sekvent services
- [components.md](components.md) and [../component-model.md](../component-model.md): calling other components instead of raw HTTP
- [server.md](server.md), [runtime.md](runtime.md): the inbound side
- [testing.md](testing.md): test helpers
- [../features.md](../features.md), [../getting-started.md](../getting-started.md), [../README.md](../README.md)
