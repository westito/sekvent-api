# Service links (`sekvent::link`)

Service-to-service authentication with static bearer tokens. Every link
between two services has one token. The caller sends it as
`Authorization: Bearer <token>`. The receiver looks the token up, learns
which service is calling, and learns whether that service may act on
behalf of an end user. Configuration is read once at startup and fails
closed. Errors name keys and links, never tokens. Lookups compare in
constant time.

The crate is `sekvent-link`; the facade re-exports it as `sekvent::link`.

## Enable it

```toml
[dependencies]
sekvent-api = { git = "https://github.com/westito/sekvent-api", branch = "master", features = ["link", "link-axum"] }
```

| Facade feature | What it adds |
|---|---|
| `link` | `LinkConfig`, `TokenMap`, `BearerInjector` (on a `HeaderMap` and as a tower `Layer`), `authenticator`, `authenticate_headers`, token helpers |
| `link-axum` | `require_service`, an axum middleware that rejects unauthenticated callers |
| `link-tonic` | `ServiceTokenInterceptor` (inbound) and a `tonic::service::Interceptor` impl on `BearerInjector` (outbound) |

`link-axum` and `link-tonic` both turn on `link`. The default facade
features (`config`, `error`, `context`, `telemetry`, `runtime`) supply
`EnvSource`, `AppError`, `CallContext` and the combined `Server` used in
the examples below. The facade does not re-export axum or tonic, so add
them yourself at the versions the workspace pins (axum 0.8, tonic 0.14).

```rust
use sekvent::link::{LinkConfig, TokenMap, BearerInjector, ServiceIdentity};
```

## Quick example

The `orders` service accepts calls from `billing` and calls `inventory`:

```sh
# orders' environment
SEKVENT_LINK_INBOUND_BILLING=<token billing presents to orders>
SEKVENT_LINK_OUTBOUND_INVENTORY=<token orders presents to inventory>
SEKVENT_LINK_TRUSTED=billing
```

```rust
use std::net::SocketAddr;

use axum::routing::post;
use axum::{Extension, Router};
use sekvent::config::EnvSource;
use sekvent::link::{self, LinkConfig, ServiceIdentity};
use sekvent::runtime::{Ctx, Server};

async fn create_order(Extension(caller): Extension<ServiceIdentity>, Ctx(cx): Ctx) -> String {
    // `caller.name` is "billing". `cx.subject()` is kept because billing is trusted.
    format!("{} {:?}", caller.name, cx.subject())
}

async fn serve() -> Result<Server, Box<dyn std::error::Error>> {
    // Fails at startup if any link key is malformed. The error names the key.
    let links = LinkConfig::from_source(&EnvSource)?;
    links.check_distinct_tokens()?;

    // Reject callers without a valid token on the internal routes.
    let internal = Router::new()
        .route("/internal/orders", post(create_order))
        .layer(axum::middleware::from_fn_with_state(
            links.inbound().clone(),
            link::require_service,
        ));

    // Feed the caller into every request's CallContext.
    let auth = link::authenticator(links.inbound().clone());
    let server = Server::builder()
        .rest(internal)
        .authenticator(move |parts| auth(parts))
        .bind(SocketAddr::from(([0, 0, 0, 0], 8080)))
        .await?;
    Ok(server)
}
```

## Concepts

### Links and tokens

A **link** is a named, one-directional relationship between a caller and a
callee. If `billing` calls `orders`, the same token appears in two places:

| Process | Key | Role |
|---|---|---|
| `billing` (caller) | `SEKVENT_LINK_OUTBOUND_ORDERS` | token billing **presents** when calling `orders` |
| `orders` (callee) | `SEKVENT_LINK_INBOUND_BILLING` | token `orders` **accepts** from the link `billing` |

On the inbound side the link name identifies the caller, so `orders` names
it `billing`. On the outbound side the name identifies the callee, so
`billing` names it `orders`. One token per link and direction. A call from
`orders` back to `billing` is a separate link and uses a separate token.

Tokens are **canonical**:

- at least `MIN_TOKEN_LEN` (32) characters;
- only `A-Z`, `a-z`, `0-9`, `-` and `_`;
- no whitespace anywhere. Nothing trims a token, so a trailing newline from
  a secrets file is rejected rather than silently stripped.

`random_token()` makes a suitable one: 32 bytes from the OS random source,
base64url-encoded without padding, 43 characters.

### `ServiceIdentity` and trust

A successful lookup yields a `sekvent::link::ServiceIdentity`, which is a
re-export of `sekvent::context::ServiceIdentity`:

```rust
pub struct ServiceIdentity {
    pub name: String,   // the inbound link name, lower-cased, e.g. "billing"
    pub trusted: bool,  // whether this link may assert end-user identity
}
// ServiceIdentity::trusted(name), ServiceIdentity::untrusted(name)
```

A link is `trusted` only if `SEKVENT_LINK_TRUSTED` lists it. Only a
trusted caller may pass on an end user's `x-sekvent-subject` and
`x-sekvent-tenant`. `sekvent::context::headers::from_headers(headers, caller)`
drops those headers from any other caller (see [context.md](context.md)).

The identity reaches your handlers in one of two ways, depending on which
inbound piece you use:

| Inbound piece | Where the identity lands | Unknown or missing token |
|---|---|---|
| `Server::builder().authenticator(…)` with `link::authenticator` | `CallContext::caller()` (axum: `Ctx`; tonic: `request.extensions().get::<CallContext>()`) | request continues with an anonymous (untrusted) caller |
| `link::require_service` (axum) | request extension `ServiceIdentity` | `401 UNAUTHENTICATED` |
| `link::ServiceTokenInterceptor` (tonic) | request extension `ServiceIdentity` | `UNAUTHENTICATED` status |

The server's authenticator **identifies** the caller. The middleware and
the interceptor **enforce** that there is one. Internal-only endpoints
usually need both: the authenticator so the `CallContext` carries the
caller and the end-user fields, and the middleware or interceptor so
anonymous callers are rejected.

## How to …

### Generate a token

```rust
pub fn random_token() -> Result<sekvent::config::Secret, LinkError>;
```

```rust
let token = sekvent::link::random_token()?;   // 43 characters, canonical
// Hand token.expose() to your secret store. Never log it.
```

The only error is `LinkError::Randomness`, which means the OS random
source failed. Provision the same value as the caller's
`SEKVENT_LINK_OUTBOUND_<CALLEE>` and the callee's
`SEKVENT_LINK_INBOUND_<CALLER>`.

### Load the configuration

```rust
impl LinkConfig {
    pub fn from_source(source: &dyn ConfigSource) -> Result<Self, ConfigError>;
    pub fn inbound(&self) -> &Arc<TokenMap>;
    pub fn outbound(&self, name: &str) -> Option<&BearerInjector>;
    pub fn require_outbound(&self, name: &str) -> Result<&BearerInjector, ConfigError>;
    pub fn outbound_names(&self) -> impl Iterator<Item = &str>;
    pub fn check_distinct_tokens(&self) -> Result<(), LinkError>;
    pub fn keys(source: &dyn ConfigSource) -> Vec<String>;
}
```

```rust
use sekvent::config::EnvSource;
use sekvent::link::LinkConfig;

let links = LinkConfig::from_source(&EnvSource)?;
links.check_distinct_tokens()?;                          // optional, recommended
let to_inventory = links.require_outbound("inventory")?; // Missing { key: "SEKVENT_LINK_OUTBOUND_INVENTORY" }
```

- `from_source` reads every `SEKVENT_LINK_*` key, validates all of them,
  and reports **every** problem at once. A single problem comes back as
  `ConfigError::Invalid` or `ConfigError::EmptySecret`; several come back
  as `ConfigError::Multiple`. Each error names the key, never the token.
- A source with no link keys gives an empty config. `inbound()` then
  accepts nothing, and `outbound(…)` returns `None` for every name.
- `outbound(name)` and `require_outbound(name)` ignore case. Use
  `require_outbound` for every link your service actually calls, so a
  missing token fails at startup instead of on the first request.
- `LinkConfig` is `Clone` and its `Debug` output never contains a token.
- `LinkConfig::keys(source)` lists the link keys present in `source`, so
  you can declare them as known when you check the reserved namespace
  yourself with `sekvent::config::check_reserved`. You do not need this
  with `check_unknown_keys` or `sekvent::config::load`, which already
  accept the link keys (see below).
- `Prefixed` sources work: errors from `from_source` report the full key
  the operator set, e.g. `ORDERS_SEKVENT_LINK_INBOUND_BILLING`.

### Configuration keys

| Key | Format | Meaning |
|---|---|---|
| `SEKVENT_LINK_INBOUND_<NAME>` | canonical token | token that link `<name>` presents to this process |
| `SEKVENT_LINK_OUTBOUND_<NAME>` | canonical token | token this process presents when calling `<name>` |
| `SEKVENT_LINK_TRUSTED` | comma-separated link names | inbound links that may assert end-user `subject` and `tenant` |

The constants and helpers below build these names:

```rust
pub const INBOUND_PREFIX: &str = "SEKVENT_LINK_INBOUND_";
pub const OUTBOUND_PREFIX: &str = "SEKVENT_LINK_OUTBOUND_";
pub const TRUSTED_KEY: &str = "SEKVENT_LINK_TRUSTED";
pub fn inbound_key(link: &str) -> String;   // inbound_key("billing") == "SEKVENT_LINK_INBOUND_BILLING"
pub fn outbound_key(link: &str) -> String;  // outbound_key("inventory") == "SEKVENT_LINK_OUTBOUND_INVENTORY"
```

Validation rules:

- `<NAME>` is one or more of `A-Z`, `a-z`, `0-9` and `_`, and becomes the
  lower-cased link name. `SEKVENT_LINK_INBOUND_` alone, or a name with a
  `-`, is `Invalid`.
- Two keys whose names differ only in case, such as `…_INBOUND_BILLING`
  and `…_INBOUND_billing`, collide: "configured twice".
- An empty or whitespace-only token is `EmptySecret`. A short token or a
  non-canonical one is `Invalid`.
- Two **inbound** links that share a token are `Invalid`. The error is
  reported on the second key in sorted order, because a shared token
  could not tell the two callers apart.
- `SEKVENT_LINK_TRUSTED`: entries are trimmed, matched without regard to
  case, and empty entries are skipped. Every name it lists must have an
  inbound token; otherwise the key is `Invalid` ("it names link `x`, which
  has no SEKVENT_LINK_INBOUND_<NAME> token"). Unset means no link is
  trusted.
- There are no defaults. A link is only accepted or called if you
  configure it.

`SEKVENT_LINK_TRUSTED` is in `sekvent::config::FRAMEWORK_KEYS`, and
`SEKVENT_LINK_INBOUND_` and `SEKVENT_LINK_OUTBOUND_` are in
`FRAMEWORK_PREFIXES`. The reserved-namespace check that
`sekvent::config::load` runs therefore accepts them without help (see
[config.md](config.md)). A misspelling such as `SEKVENT_LINKS_TRUSTED` is
still rejected as an unknown `SEKVENT_` key.

### Keep one token per purpose

`TokenMap::new` and `from_source` already refuse a token shared by two
inbound links. Two more checks are available:

```rust
// Inbound and outbound together: no token is both accepted and presented,
// and no two outbound links share one. Errors name "inbound/<link>" or
// "outbound/<link>".
links.check_distinct_tokens()?;

// Several token maps in one process, e.g. two services hosted together.
// Errors name "<map>/<link>".
sekvent::link::validate_unique(&[("orders", &orders_map), ("inventory", &inventory_map)])?;
```

```rust
pub fn validate_unique(maps: &[(&str, &TokenMap)]) -> Result<(), LinkError>;
```

`from_source` does not call `check_distinct_tokens` for you. Call it
yourself. Component configuration does call it whenever it reads link
keys. Both checks compare tokens in constant time.

### Build a `TokenMap` by hand

Use this when tokens come from somewhere other than environment-style
config:

```rust
pub struct InboundLink {
    pub name: String,
    pub token: Secret,
    pub trusted: bool,
}
impl InboundLink {
    pub fn untrusted(name: impl Into<String>, token: Secret) -> Self;
    pub fn trusted(name: impl Into<String>, token: Secret) -> Self;
}

impl TokenMap {
    pub fn new(links: impl IntoIterator<Item = InboundLink>) -> Result<Self, LinkError>;
    pub fn authenticate(&self, token: &str) -> Option<ServiceIdentity>;
    pub fn identities(&self) -> impl Iterator<Item = &ServiceIdentity>;
    pub fn len(&self) -> usize;
    pub fn is_empty(&self) -> bool;
}

pub fn validate_token(link: &str, token: &str) -> Result<(), LinkError>;
pub const MIN_TOKEN_LEN: usize = 32;
```

```rust
use std::sync::Arc;

use sekvent::config::Secret;
use sekvent::link::{InboundLink, TokenMap};

let map = Arc::new(TokenMap::new([
    InboundLink::trusted("billing", Secret::new(billing_token)),
    InboundLink::untrusted("reports", Secret::new(reports_token)),
])?);
```

- `new` refuses a blank name (`BlankName`), a name used twice
  (`DuplicateName`), a blank, short or non-canonical token, and a token
  shared by two links (`DuplicateToken`). `InboundLink` names are used as
  given. `LinkConfig` lower-cases them; when you build by hand, keep them
  lower-case yourself.
- `authenticate` refuses empty input up front. Otherwise it compares the
  presented token with **every** entry in constant time and never stops
  early, so timing does not show which entry matched or how many leading
  characters were right. Comparing tokens of different lengths does show
  that the lengths differ. That is an accepted limitation, and tokens from
  `random_token` all have the same length.
- Matching is exact: no trimming, no case folding. A token with extra
  characters or missing ones is refused.
- `Debug` for `TokenMap` and `InboundLink` shows link names only.

### Authenticate inbound HTTP requests (axum)

Feature `link-axum`.

```rust
pub async fn require_service(
    State(map): State<Arc<TokenMap>>,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Result<axum::response::Response, AppError>;
```

```rust
use axum::{Extension, Router, routing::get};
use sekvent::link::{self, ServiceIdentity};

async fn stock(Extension(caller): Extension<ServiceIdentity>) -> String {
    format!("hello {}", caller.name)
}

let internal = Router::new()
    .route("/internal/stock", get(stock))
    .layer(axum::middleware::from_fn_with_state(
        links.inbound().clone(),
        link::require_service,
    ));
```

When the token is valid, the middleware inserts the `ServiceIdentity` into
the request extensions and calls the handler. When the header is missing,
malformed, or carries an unknown token, it answers `401` and logs
"service token rejected" at `debug` level, without the token.

### Put the caller into `CallContext` (combined server)

```rust
pub type Authenticator = Arc<dyn Fn(&http::request::Parts) -> Option<ServiceIdentity> + Send + Sync>;
pub fn authenticator(map: Arc<TokenMap>) -> Authenticator;
pub fn authenticate_headers(map: &TokenMap, headers: &http::HeaderMap) -> Option<ServiceIdentity>;
```

```rust
let auth = sekvent::link::authenticator(links.inbound().clone());
let server = sekvent::runtime::Server::builder()
    .rest(routes)
    .authenticator(move |parts| auth(parts))
    .bind(addr)
    .await?;
```

The server runs the authenticator on every request except the health
endpoints, and passes the result to `headers::from_headers`. Handlers read
it as `cx.caller()`. An unknown or missing token gives `None`, an
anonymous caller with no subject or tenant. Nothing is rejected at this
stage, so add `require_service` or `ServiceTokenInterceptor` where an
authenticated caller is mandatory. See [server.md](server.md).

`authenticate_headers` is the building block for custom middleware. It
reads `Authorization`, accepts the `Bearer` scheme in any case followed by
exactly one space, and takes the rest verbatim. `Bearer  <token>` (two
spaces), `Basic …`, a bare token and non-ASCII header bytes are all
refused.

### Authenticate inbound gRPC calls (tonic)

Feature `link-tonic`.

```rust
impl ServiceTokenInterceptor {
    pub fn new(map: Arc<TokenMap>) -> Self;
}
impl tonic::service::Interceptor for ServiceTokenInterceptor { /* … */ }
```

```rust
use sekvent::link::ServiceTokenInterceptor;

let interceptor = ServiceTokenInterceptor::new(links.inbound().clone());
let server = sekvent::runtime::Server::builder()
    .add_service(InventoryServer::with_interceptor(api, interceptor))
    .bind(addr)
    .await?;

// In a handler:
let caller = request.extensions().get::<sekvent::link::ServiceIdentity>();
```

The interceptor reads the `authorization` metadata, applies the same
parsing and lookup rules as the axum middleware, and fails with
`Status::unauthenticated` carrying `REJECTED_MESSAGE`. It is `Clone`, and
its `Debug` output shows link names only.

### Attach the token to outbound calls

`BearerInjector` presents one outbound link's token:

```rust
impl BearerInjector {
    pub fn new(link: impl Into<String>, token: &Secret) -> Result<Self, LinkError>;
    pub fn link(&self) -> &str;
    pub fn apply(&self, headers: &mut http::HeaderMap);
}
impl<S> tower::Layer<S> for BearerInjector { type Service = InjectBearer<S>; }
// feature link-tonic:
impl tonic::service::Interceptor for BearerInjector { /* … */ }
```

You normally get one from `links.require_outbound("inventory")?`. To build
one directly, call `BearerInjector::new`, which validates the token. The
header value is marked sensitive, and `Debug` shows the link only.

**tonic client** (feature `link-tonic`):

```rust
let to_inventory = links.require_outbound("inventory")?.clone();
let channel = tonic::transport::Endpoint::from_shared(url)?.connect_lazy();
let client = InventoryClient::with_interceptor(channel, to_inventory);
```

**tower HTTP client stack**. `InjectBearer<S>` wraps any
`Service<http::Request<B>>` and sets the header on every request:

```rust
use tower::Layer;

let authed = links.require_outbound("inventory")?.layer(http_service);
```

**A header map you build yourself**:

```rust
let mut headers = http::HeaderMap::new();
links.require_outbound("inventory")?.apply(&mut headers); // replaces any existing Authorization
```

**`sekvent::client::HttpClient`** (feature `client`). The HTTP client is
built on reqwest, not tower, and `BearerInjector` does not expose its
token. Read the outbound key as a `Secret`, check it, and give it to the
client as a fixed bearer token:

```rust
use sekvent::client::HttpClient;
use sekvent::config::{EnvSource, req_secret};
use sekvent::link::{outbound_key, validate_token};

let token = req_secret(&EnvSource, &outbound_key("inventory"))?;
validate_token("inventory", token.expose())?;
let inventory = HttpClient::builder()
    .base_url("http://inventory:8080")
    .bearer_token(token)
    .sekvent_upstream(true) // the upstream is a sekvent service: propagate subject/tenant, keep its 401 code
    .build()?;
```

See [client.md](client.md) for `sekvent_upstream` and context propagation.

### Rotate a token

Each inbound link holds exactly one token. `TokenMap` refuses a name used
twice, and `LinkConfig` is read only once, at startup. There is no built-in
"old and new" pair. Rotation means changing configuration and restarting:

1. Generate the new token with `random_token()`.
2. Simplest, with a brief window of rejected calls: update the callee's
   `SEKVENT_LINK_INBOUND_<CALLER>` and the caller's
   `SEKVENT_LINK_OUTBOUND_<CALLEE>`, then restart both, callee first.
3. With no rejected calls, use a second, temporary inbound link:
   1. On the callee, add `SEKVENT_LINK_INBOUND_BILLING_NEXT` carrying the
      new token and, if `billing` is trusted, add `billing_next` to
      `SEKVENT_LINK_TRUSTED`. Restart the callee.
   2. Switch the caller's outbound token to the new one and restart it.
   3. On the callee, in **one** configuration update: set
      `SEKVENT_LINK_INBOUND_BILLING` to the new token, remove
      `SEKVENT_LINK_INBOUND_BILLING_NEXT`, and remove `billing_next` from
      `SEKVENT_LINK_TRUSTED`. Then restart the callee. Doing these together
      matters: a `SEKVENT_LINK_TRUSTED` entry without its
      `SEKVENT_LINK_INBOUND_<NAME>` token, or the same token under two
      names, fails `LinkConfig::from_source` at startup.

   While the temporary link exists, calls that use it identify as
   `billing_next`, not `billing`. Any code that checks `caller.name` must
   accept both names during that window.

### Authenticate component calls

Components that cross a process boundary use these same keys. They do not
call this crate's API. In brief (details in [components.md](components.md)
and [../component-model.md](../component-model.md)):

- A component bound `grpc` presents `SEKVENT_LINK_OUTBOUND_<LINK>`.
  `<LINK>` comes from `SEKVENT_COMPONENT_<C>_LINK` and defaults to the
  component name. `SEKVENT_COMPONENT_<C>_AUTH` is `link` (default) or
  `none`.
- A component served over gRPC (`SEKVENT_COMPONENT_<C>_SERVE=grpc`) checks
  the process's inbound tokens unless `SEKVENT_COMPONENT_<C>_SERVE_AUTH=none`.
  The check runs on the request headers before the body is read. Every
  configured inbound link may call every exposed component.
- The `App` build fails, naming the key, when a needed outbound token is
  missing (`Missing { key: "SEKVENT_LINK_OUTBOUND_<LINK>" }`), when
  inbound auth is on but no `SEKVENT_LINK_INBOUND_*` key exists
  (`Missing { key: "SEKVENT_LINK_INBOUND_<CALLER>" }`), when two link keys
  share a token, or when a link is named `local`, which is reserved for
  in-process callers.
- Turning auth off with `none` is logged at `warn` level.

## Errors

### What a rejected caller sees

The response is the same for a missing token, a malformed header and an
unknown token, so a caller cannot tell which one it was:

| Transport | Response |
|---|---|
| HTTP (`require_service`) | `401 Unauthorized`, `Content-Type: application/json`, body `{"error":{"code":"UNAUTHENTICATED","message":"service authentication required"}}` |
| gRPC (`ServiceTokenInterceptor`, component serving) | status `UNAUTHENTICATED`, message `service authentication required` |

```rust
pub const REJECTED_MESSAGE: &str = "service authentication required";
```

The rejection carries no reason code and no metadata. This crate defines
no reason constants; compare against `ErrorCode::Unauthenticated` and, if
you need to, `REJECTED_MESSAGE`. A sekvent `HttpClient` built with
`sekvent_upstream(true)` surfaces the upstream's 401 as `UNAUTHENTICATED`.
Without it, the 401 becomes `INTERNAL`, because a refused service
credential is this service's configuration problem, not the end user's
(see [error.md](error.md)).

### Setup errors

`LinkError` (`#[non_exhaustive]`, `Clone + PartialEq`). Messages name
links, never tokens:

| Variant | Message |
|---|---|
| `BlankName` | a service link name must not be blank |
| `BlankToken { link }` | service link `<link>` has a blank token |
| `TooShort { link }` | service link `<link>` has a token shorter than 32 characters |
| `NonCanonical { link }` | service link `<link>` has a non-canonical token: only A-Z, a-z, 0-9, `-` and `_` are allowed, without whitespace |
| `DuplicateToken { first, second }` | service links `<first>` and `<second>` use the same token |
| `DuplicateName { link }` | service link `<link>` is configured twice |
| `Randomness` | the operating system random source failed |

`LinkConfig::from_source` wraps these in `ConfigError::Invalid { key, reason }`,
so you get the key too.

## Fail-closed rules

- **Startup, inbound.** A malformed name, a blank, short or non-canonical
  token, a duplicate name, a token shared by two inbound links, or a
  trusted name without a token all fail `from_source`. Each error names
  the key.
- **Startup, outbound.** `require_outbound("x")` fails with
  `ConfigError::Missing { key: "SEKVENT_LINK_OUTBOUND_X" }`.
  `BearerInjector::new` refuses a non-canonical token.
- **Request time, inbound.** Only an exact, constant-time match against a
  configured token is accepted. An empty map accepts nothing. Anything
  else is anonymous to the server's authenticator, or rejected by
  `require_service` and `ServiceTokenInterceptor`.
- **Trust.** It is never implied. A link can assert end-user identity only
  if `SEKVENT_LINK_TRUSTED` names it.

## Testing tips

- Build config from `sekvent::config::MapSource` with fixed tokens of at
  least 32 canonical characters, e.g.
  `"billing-token-0123456789abcdefABCDEF"`, or with `random_token()`.
  Never use real secrets.
- Round-trip without a socket. Wrap the axum router in the injector and
  drive it with `tower::ServiceExt::oneshot`:
  ```rust
  use tower::{Layer, ServiceExt};
  let client = BearerInjector::new("orders", &Secret::new(ORDERS))?.layer(app);
  let response = client.oneshot(request).await?;
  ```
- For tonic, call the interceptors directly:
  `inbound.call(outbound.call(tonic::Request::new(()))?)?`, then assert on
  `request.extensions().get::<ServiceIdentity>()`.
- Assert the negative cases too: no header, an unknown token,
  `Bearer ` with nothing after it, and two spaces after `Bearer`. Each
  must give `UNAUTHENTICATED` with `REJECTED_MESSAGE`.
- Assert that error strings and `Debug` output never contain the token:
  `assert!(!error.to_string().contains(TOKEN))`.
- `TokenMap::new` and `LinkConfig::from_source` are synchronous and need no
  runtime. More in [testing.md](testing.md).

## Pitfalls and security

- **Never log, format or return a token.** Keep tokens in `Secret`, whose
  `Debug` and `Display` print `[redacted]` and which zeroes its memory on
  drop. Call `expose()` only where the raw value is actually needed. The
  crate's own types are safe to debug-print.
- **Secret files with a trailing newline** are rejected as non-canonical.
  Strip the newline when you provision the secret; the framework will not.
- **The authenticator alone does not protect a route.** It only labels the
  caller. Add `require_service` or the interceptor to endpoints that must
  not be called anonymously.
- **Health endpoints bypass the authenticator**, by design.
- **Do not reuse a token** across links or directions. Call
  `check_distinct_tokens()`, and `validate_unique` when one process hosts
  several token maps.
- **Trust sparingly.** List a caller in `SEKVENT_LINK_TRUSTED` only if it
  really authenticates end users itself. A trusted link can act as any
  user.
- **Configuration is read once.** Changing a token requires a restart; see
  rotation above.
- **Transport.** Bearer tokens over plaintext HTTP or HTTP/2 can be read by
  anyone on the network path. Run links on a private network or behind
  TLS.

## See also

- [components.md](components.md) and [../component-model.md](../component-model.md): component bindings and gRPC serving with link auth
- [server.md](server.md) and [runtime.md](runtime.md): `Server::builder().authenticator(…)`, `Ctx`
- [context.md](context.md): `CallContext`, `ServiceIdentity`, header propagation
- [config.md](config.md): `ConfigSource`, `Secret`, the reserved `SEKVENT_` namespace
- [error.md](error.md): `AppError`, `ErrorCode::Unauthenticated`, HTTP and gRPC mappings
- [client.md](client.md): outbound HTTP with `bearer_token` and `sekvent_upstream`
- [auth.md](auth.md): end-user authentication (passwords, JWT)
- [testing.md](testing.md), [telemetry.md](telemetry.md)
- [../features.md](../features.md), [../getting-started.md](../getting-started.md), [../README.md](../README.md)
