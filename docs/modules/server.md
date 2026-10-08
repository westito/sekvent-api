# Server (`sekvent::runtime::Server`)

`Server` is one TCP listener that serves native gRPC (tonic services),
gRPC-Web for browsers and REST (an axum `Router`) side by side, optionally
under a path prefix, with the health endpoints built in. Every request gets a
request id and one access-log event; every request except the health
endpoints and CORS preflights also gets a [`CallContext`](context.md) decoded
from its headers. The server applies validated CORS rules, connection and
body limits and your own tower layers, and drains gracefully as a
[runtime](runtime.md) unit. `Download` builds file responses with safe
headers.

## Enable it

| | |
|---|---|
| Facade feature | `runtime` (on by default) |
| gRPC-Web | `runtime-grpc-web`. **Off in the facade's defaults** (the internal crate's `grpc-web` feature is on by default, the facade turns it off). Without it, `ServerBuilder::grpc_web(true)` has no effect. |
| Module | `use sekvent::runtime::{Server, ServerBuilder, Ctx, Cors, Download, Disposition, CacheControl, HealthVisibility};` |
| Prelude | `Server`, `ServerBuilder`, `Ctx` |
| Your manifest | `axum` (0.8) for REST routes and `tonic` (0.14) for gRPC services, at the versions the workspace pins |

## Quick example

```rust
use std::net::SocketAddr;

use axum::Router;
use axum::routing::get;
use sekvent::prelude::*;

fn rest_routes() -> Router {
    Router::new().route(
        "/whoami",
        get(|Ctx(ctx): Ctx| async move { ctx.request_id().to_owned() }),
    )
}

let server = Server::builder()
    .add_service(OrdersServiceServer::new(OrdersApi::new(repo))) // a tonic-generated service
    .rest(rest_routes())
    .prefix("/api")                // REST and gRPC under /api; gRPC (native and gRPC-Web) also at /
    .bind(SocketAddr::from(([0, 0, 0, 0], 8080)))
    .await?;

Runtime::builder()
    .unit("api", Stage::Ingress, UnitPolicy::Critical, server.into_unit())
    .build()?
    .run()
    .await?;
```

`GET /api/whoami` answers the request id; `/livez`, `/readyz`, `/healthz`
and `grpc.health.v1.Health` work without any further code.

## Concepts

### One listener, three protocols

The server splits traffic by **content type**: a request whose
`content-type` starts with `application/grpc` (native gRPC and gRPC-Web) goes
to the tonic routes, everything else to the REST routes. With the
`runtime-grpc-web` feature and `grpc_web(true)` (the default), gRPC-Web
requests are translated before they reach tonic, so the same service answers
browsers over HTTP/1.1 and native clients over HTTP/2. Connections speak
HTTP/1.1 or HTTP/2, detected per connection.

### Prefix and `grpc_at_root`

| Path | No prefix | `prefix("/api")`, `grpc_at_root(true)` (default) | `prefix("/api")`, `grpc_at_root(false)` |
|---|---|---|---|
| `/pkg.Service/Method` (gRPC content type) | tonic | tonic | 404 |
| `/api/pkg.Service/Method` (gRPC content type) | — | tonic, prefix stripped | tonic, prefix stripped |
| `/orders` (REST) | REST routes | 404 | 404 |
| `/api/orders` (REST) | — | REST route `/orders` | REST route `/orders` |
| `/livez`, `/readyz`, `/healthz` | health | health | health |
| `/api/readyz` | — | 404 | 404 |
| `grpc.health.v1.Health` | at the root | at the root and under `/api` | under `/api` only |

The prefix is stripped before routing, so tonic still sees
`/pkg.Service/Method` and REST routes are written without it. Native gRPC
clients rarely support a path prefix, which is why `grpc_at_root` defaults to
on. Routing at the root goes by content type alone, so with `grpc_at_root`
gRPC-Web requests (`application/grpc-web…`) are answered at the root too, not
only under the prefix (translated when gRPC-Web is enabled). The HTTP health endpoints always live at the root.

A prefix must start with `/`, must not end with one, and may only contain
unreserved URL characters (`A–Z a–z 0–9 - _ . ~`) in non-empty segments, e.g.
`/api` or `/api/v1`. It must not be a health path (`/livez`, `/readyz`,
`/healthz`, while health routes are on) nor `/grpc.health.v1.Health` or
anything below it (while gRPC health is on). Anything else fails `bind` with
`INVALID_ARGUMENT`.

### The request stack

`Server::router` documents the full stack. Outermost first:

1. **Request id, `request` span and access log.** A valid incoming
   `x-request-id` (1–128 visible ASCII characters) is kept, anything else is
   replaced with a fresh UUID v7, and the id is echoed on the response.
2. **CORS**, when configured. Preflights are answered here.
3. **Health endpoints**: `/livez`, `/readyz`, `/healthz` and
   `grpc.health.v1.Health`. They never see the layers below.
4. Everything else:
   1. the **call context**: the `Authenticator` (if any) identifies the
      caller from the request head, then the `CallContext` is decoded from
      the headers and put into the request extensions (its request id is the
      one from step 1);
   2. your **application layers** (`ServerBuilder::layer`), the last one
      added outermost;
   3. the **prefix strip** and the split between gRPC (by content type) and
      the REST routes with their body limit.

The authenticator runs on the request **head** before any body is read.

## How to …

### Add gRPC services and REST routes

```rust
let server = Server::builder()
    .add_service(OrdersServiceServer::new(orders))            // one tonic service
    .add_service(InventoryServiceServer::new(inventory)
        .max_decoding_message_size(16 << 20))                 // gRPC limits are per service
    .rest(orders_routes())                                    // axum Router, merged
    .rest(admin_routes())                                     // merged again
    .bind(addr)
    .await?;
```

- `add_service(S)` takes any tonic service (`NamedService + Clone + Send +
  Sync`, `Error = Infallible`); call it as often as needed.
- `grpc_routes(tonic::service::Routes)` hands over a ready `Routes` value
  instead. It may be given **once**, and it must come **before** any
  `add_service`: `add_service` creates the routes itself, so a later
  `grpc_routes` counts as a second set. Either mistake fails `bind` with
  `INVALID_ARGUMENT` (`gRPC routes were given more than once; use add_service
  to add more`). Call `add_service` after it for more services.
- `rest(Router)` merges with `Router::merge`, which **panics on overlapping
  routes**.
- REST routes get a route recorder, so the access log's `route` field holds
  the matched template (`/orders/{id}`), prefix included.

### Bind

```rust
pub async fn bind(self, addr: SocketAddr) -> Result<Server, AppError>;
pub fn from_listener(self, listener: tokio::net::TcpListener) -> Result<Server, AppError>;
```

Both validate the configuration first (`INVALID_ARGUMENT` naming the
problem). `bind` fails with `UNAVAILABLE` (`could not listen on <addr>`) when
the address is taken. Port 0 picks a free port; `server.local_addr()` returns
the real one. `from_listener` serves a listener you bound yourself.

Binding happens before the runtime starts, so a taken port fails the process
before any unit runs.

### Run it as a unit

```rust
let builder = Runtime::builder()
    .unit("api", Stage::Ingress, UnitPolicy::Critical, server.into_unit());
```

`into_unit()` returns a unit factory:

- The unit publishes the current health state, logs `listening` and reports
  **ready once it is accepting**.
- When its stage drains it **stops accepting at once** (the port is released
  immediately), then lets in-flight requests finish: HTTP/2 connections get a
  GOAWAY, HTTP/1 connections close after their current response. The unit
  returns when every connection is done, or is aborted at its
  [stop deadline](runtime.md#use-the-unit-context).
- The runtime has already turned readiness and gRPC health to not serving
  before the drain (and waited `shutdown_delay`), so load balancers stop
  sending new traffic first.
- A failed accept that concerns one connection is skipped. Running out of
  file descriptors or memory pauses accepting (100 ms, doubling up to 1 s),
  logged once per burst at `warn`, with an `info` when accepting resumes.
- Only a listener that can no longer accept at all fails the unit
  (`UNAVAILABLE`, `the listener on <addr> failed`); it returns at once and
  its open connections drain in the background for at most the stage grace.
- A restarted unit binds the same address again, so `UnitPolicy::Restart`
  works too. `Server` is cheap to clone.

Ingress is the last stage to start and the first to drain: the listener
only opens once infrastructure, components and workers are up.

### Read the call context in handlers

axum handlers take the `Ctx` extractor:

```rust
use sekvent::runtime::Ctx;

async fn get_order(Ctx(ctx): Ctx, Path(id): Path<String>) -> Result<Json<Order>, AppError> {
    let order = orders.get(&ctx, &id).await?;
    Ok(Json(order))
}
```

tonic handlers read it from the request extensions:

```rust
async fn get_order(&self, request: tonic::Request<GetOrderRequest>) -> Result<tonic::Response<Order>, tonic::Status> {
    let ctx = request
        .extensions()
        .get::<CallContext>()
        .cloned()
        .ok_or_else(|| tonic::Status::internal("no call context"))?;
    // …
}
```

The context carries the request id, the deadline from `grpc-timeout`, the
`traceparent`, the idempotency key, the hop count and, only from a trusted
caller, the end-user subject and tenant. See [Call context](context.md) for
the header rules.

`Ctx` rejects with `INTERNAL` (`the request has no call context`) when the
router runs without the server's context layer, for example a bare axum
router in a test. That is deliberate: a handler never runs with a made-up
context.

### Authenticate callers

```rust
pub type Authenticator = Arc<dyn Fn(&http::request::Parts) -> Option<ServiceIdentity> + Send + Sync>;

let auth = sekvent::link::authenticator(link.inbound().clone());
let server = Server::builder()
    .authenticator(move |parts| auth(parts))
    // …
```

The authenticator sees the request head and returns the calling service, or
`None` for an anonymous caller. Its answer feeds
`sekvent::context::headers::from_headers`: only a `ServiceIdentity` that is
trusted may assert `subject` and `tenant`; for anyone else those headers are
dropped. It does **not** reject requests by itself: an anonymous request
still reaches your handlers with `ctx.caller() == None`. Reject in a layer or
handler where that matters; [Service links](link.md) has the middleware.

For tests, any closure works:

```rust
.authenticator(|parts: &Parts| {
    parts.headers.contains_key("x-test-link").then(|| ServiceIdentity::trusted("billing"))
})
```

### Add application layers

```rust
use http::{HeaderValue, header};
use tower_http::set_header::SetResponseHeaderLayer;

let server = Server::builder()
    .rest(routes())
    .layer(SetResponseHeaderLayer::overriding(               // any Clone tower layer
        header::X_FRAME_OPTIONS,
        HeaderValue::from_static("DENY"),
    ))
    .layer(axum::middleware::from_fn(audit))                // wraps the one above
    .bind(addr)
    .await?;
```

(`SetResponseHeaderLayer` needs `tower-http` with its `set-header` feature in
your manifest.) The built-in [access log](#use-the-access-log) already logs every request
without its query string; do not add `tower_http::trace::TraceLayer` for
request logging. Its default span records the full URI, query included, so
credentials passed as query parameters (`?code=…`, `?token=…`) would end up
in your logs.

- A layer wraps every REST, gRPC and gRPC-Web request, inside the call
  context (so it can read `CallContext` from the extensions) and outside
  routing. It never wraps the HTTP or gRPC health endpoints.
- The bounds are those of `axum::Router::layer`, and the layer must be
  `Clone`. A later `.layer(..)` wraps the earlier ones.
- Component calls served on this listener pass through the layers too. An
  end-user authentication layer must let link-authenticated component paths
  through, or be applied to the REST router or tonic service it protects
  instead.

### Limit bodies, messages and connections

| Setting | Default | Notes |
|---|---|---|
| `rest_body_limit(bytes)` | 2 MiB (axum's default) | REST extractors refuse larger bodies with `413`. Must be positive. |
| per-route body limit | — | `.layer(DefaultBodyLimit::max(16 * 1024 * 1024))` on one method router raises it for that route only. |
| raw `Body` handlers | unbounded | A handler reading `axum::body::Body` directly must bound it itself, e.g. `http_body_util::Limited`. |
| gRPC message size | tonic: 4 MiB decoding | Set per service: `OrdersServer::new(svc).max_decoding_message_size(16 << 20)`. |
| `header_read_timeout(Some(d))` | 30 s | Longest a new connection may stay silent, and an HTTP/1 client may take to send its headers, before it is closed. `None` waits forever; zero is invalid. |
| `max_connections(Some(n))` | 10 000 | Beyond it, new connections wait in the OS accept queue until one closes. `None` removes the limit; 0 is invalid. |

### Configure CORS

CORS is **off** by default. It covers REST and gRPC-Web.

```rust
use std::time::Duration;
use sekvent::config::EnvSource;
use sekvent::runtime::Cors;

// In code:
let cors = Cors::origins(["https://app.example.com", "http://localhost:5173"])?
    .allow_credentials(true)
    .allow_headers(&["x-tenant"])
    .expose_headers(&["x-total-count"])
    .max_age(Duration::from_secs(600));
let builder = Server::builder().cors(cors);

// From configuration: ORDERS_CORS_ORIGINS, _CREDENTIALS, _MAX_AGE, _ALLOW_HEADERS, _EXPOSE_HEADERS
let mut builder = Server::builder();
if let Some(cors) = Cors::from_config(&EnvSource, "ORDERS_CORS_")? {
    builder = builder.cors(cors);
}
```

| Method | Effect |
|---|---|
| `Cors::any_origin()` | `Access-Control-Allow-Origin: *` |
| `Cors::origins(iter)?` | exactly these origins, normalized; `INVALID_ARGUMENT` on a bad entry or an empty list |
| `Cors::from_config(source, prefix)?` | `Ok(None)` when `<prefix>ORIGINS` is unset or blank |
| `Cors::config_keys(prefix)` | every key `from_config` reads, e.g. for an env template |
| `.allow_credentials(bool)` | cookies and `Authorization` from the browser; needs an origin list |
| `.methods(&[Method])` | **replaces** the default methods |
| `.allow_headers(&[&str])` | **adds** to the default request headers |
| `.expose_headers(&[&str])` | **adds** to the default exposed headers |
| `.max_age(Duration)` | preflight cache lifetime |
| `.validate()` | the checks `bind` runs |

Defaults:

| Setting | Default |
|---|---|
| methods | `GET`, `HEAD`, `POST`, `PUT`, `PATCH`, `DELETE` |
| request headers | `authorization`, `content-type`, `grpc-timeout`, `idempotency-key`, `traceparent`, `x-grpc-web`, `x-request-id`, `x-user-agent` |
| exposed headers | `content-disposition`, `grpc-message`, `grpc-status`, `grpc-status-details-bin`, `x-request-id` |
| max age | 1 h |
| credentials | off |

Configuration keys (`<prefix>` is yours, e.g. `ORDERS_CORS_`):

| Key | Value | Default |
|---|---|---|
| `<prefix>ORIGINS` | `*` or a comma-separated origin list | unset: CORS off |
| `<prefix>CREDENTIALS` | bool | `false` |
| `<prefix>MAX_AGE` | duration (`10m`, `3600`) | `1h` |
| `<prefix>ALLOW_HEADERS` | comma-separated header names, added to the defaults | — |
| `<prefix>EXPOSE_HEADERS` | comma-separated header names, added to the defaults | — |

`from_config` reports every problem at once, each naming its key; `*` with
credentials is reported under `<prefix>CREDENTIALS`.

The rules:

- **Real preflights only.** An `OPTIONS` request with both `Origin` and
  `Access-Control-Request-Method` is answered by the server itself with
  `200`, before the authenticator, your layers and handlers. Any other
  `OPTIONS` request reaches your routes.
- **Allowed or nothing.** A request without `Origin`, or from an origin not in
  the list, gets no `access-control-*` headers at all, so the browser blocks
  it. With an origin list every response also carries `Vary: origin`.
- **Authoritative.** `access-control-*` headers set by your layers or handlers
  are removed before the server adds its own, so inner code can never widen
  the policy.
- **Origin canonicalization.** Entries are normalized to the form browsers
  send: lowercase scheme and host, no port when it is the scheme's own
  default (`:80` for `http`, `:443` for `https`; `https://host:80` keeps it), no
  trailing slash, IPv4 hosts in dotted decimal and IPv6 hosts in their
  shortest form (`http://[0:0::1]` becomes `http://[::1]`; IPv4-mapped
  addresses in hex, `[::ffff:102:304]`). Duplicates collapse.
- **Rejected entries** (`INVALID_ARGUMENT`): anything but `http://` or
  `https://`, a path, query, fragment or user info, `*` inside a list (use
  `any_origin`), `null`, a malformed host or port, and an empty list. The
  error names the entry by position (`origin #2`, counting from 1; blank
  entries in configuration count too), never by its text, which may hold a
  credential.
- `bind` also rejects credentials with `any_origin`, an empty method list,
  the method `*`, and an invalid or `*` header name.

### Serve health endpoints

| Endpoint | Healthy | Unhealthy |
|---|---|---|
| `GET /livez` | `200`, body `ok\n` | `503`, body `not live\n` (after `mark_fatal`) |
| `GET /readyz`, `GET /healthz` | `200`, JSON | `503`, JSON |
| `grpc.health.v1.Health/Check` and `/Watch`, service `""` | `SERVING` | `NOT_SERVING` |
| the same, a named service | its status from `HealthRegistry::set_status` | |

What readiness means is described in [Runtime: health](runtime.md#health).
`HealthVisibility` decides how much `/readyz` reveals:

- `Minimal` (the default), safe for public exposure:
  `{"status":"ready"}` or `{"status":"not_ready"}`.
- `Full`, for internal networks only: also `live`, `started`, `draining`,
  `version` (when set) and every probe with `name`, `required`, `status`
  (`up`, `down` or `unknown`) and, when down, `failure` (`rejected` or
  `unreachable`) and `detail`.

```rust
use sekvent::runtime::HealthVisibility;

let server = Server::builder()
    .health_visibility(HealthVisibility::Full)   // internal listener only
    .health_routes(true)                         // /livez /readyz /healthz (default on)
    .grpc_health(true)                           // grpc.health.v1 (default on)
    // …
```

Health endpoints bypass the authenticator and your layers (CORS and the
access log still apply), and their traffic is logged at `debug`. To serve
your own gRPC health service, turn `grpc_health(false)` and add yours with
`add_service`; `HealthRegistry::grpc_service()` is the built-in one.

### Use the access log

With `access_log(true)` (the default) each request is logged once on
completion: target `sekvent::access`, message `request completed`, fields
`request_id`, `method`, `path` (never the query), `route`, `protocol`
(`http`, `grpc`, `grpc-web`), `status`, `grpc_status`, `latency_ms` and
`aborted`. Levels: `warn` for HTTP 5xx and gRPC `UNKNOWN`, `INTERNAL`,
`DATA_LOSS`; `debug` for health traffic; `info` otherwise. The access log and
the `request` span never record headers, bodies, query strings or peer
addresses, but they do record paths, so keep secrets out of paths. This
holds for the built-in logger only: layers and handlers you add log whatever
they log.

`SEKVENT_LOG=info,sekvent::access=warn` keeps only failures.
`access_log(false)` drops the event but keeps request ids and the `request`
span. Details: [Telemetry](telemetry.md).

### Send files with `Download`

```rust
use sekvent::runtime::{CacheControl, Disposition, Download};

async fn invoice_pdf(Ctx(ctx): Ctx, Path(id): Path<String>) -> Result<Download, AppError> {
    let pdf: Vec<u8> = invoices.render(&ctx, &id).await?;
    Ok(Download::bytes(pdf)
        .filename(&format!("invoice-{id}.pdf"))
        .disposition(Disposition::Inline)
        .cache(CacheControl::NoStore))
}

// Text formats are never sniffed: set the type.
let csv = Download::bytes(rows).filename("orders.csv").content_type("text/csv; charset=utf-8")?;
// A stream, with its length when known.
let export = Download::stream(axum::body::Body::from_stream(chunks), Some(len)).filename("orders.ndjson");
```

| Builder | Notes |
|---|---|
| `Download::bytes(impl Into<Bytes>)` | in memory; `Content-Length` set |
| `Download::stream(Body, Option<u64>)` | `Content-Length` only when the length is given |
| `.filename(&str)` | passed through `sanitize_filename` |
| `.disposition(Disposition)` | `Attachment` (default) or `Inline` |
| `.content_type(&str)?` | `type/subtype[; name=value]`, token characters, quoted parameter values allowed; else `INVALID_ARGUMENT` |
| `.cache(CacheControl)` | `NoStore` → `no-store`; `Private(Duration::ZERO)` (default) → `private, no-cache`; `Private(d)` → `private, max-age=<s>`; `Public(d)` → `public, max-age=<s>` |

The response is `200 OK` with `Content-Type`, `Content-Disposition`,
`Content-Length` (when known), `Cache-Control` and `X-Content-Type-Options:
nosniff`.

Safety rules:

- **Content type**: the one you set; else, for a bytes body, sniffed from the
  magic bytes by `sniff_content_type`; else `application/octet-stream`. A
  stream is never sniffed. Text formats (CSV, JSON, plain text, HTML) are
  never guessed.
- **Inline only for safe types**: `application/pdf`, `image/png`,
  `image/jpeg`, `image/gif`, `image/webp`, `image/avif`, `text/plain`,
  `audio/*` and `video/*`. Anything else (HTML, SVG, XML, unknown) is sent as
  an attachment, so a stored file cannot run script in your origin.
- **Filenames** (`sanitize_filename`): keep the part after the last `/` or
  `\`; drop control characters (U+0000–U+001F, U+007F–U+009F) and
  bidirectional overrides (U+202A–U+202E, U+2066–U+2069); collapse
  whitespace runs to one space; trim spaces and dots at both ends; cap at 200
  UTF-8 bytes on a character boundary, keeping an extension of up to 16
  bytes. An empty result, `.` or `..` becomes `download`.
- **`Content-Disposition`** (`content_disposition`, RFC 6266 / RFC 5987 and
  8187): `attachment; filename="<ascii>"` where non-ASCII characters, `"`,
  `\` and `%` become `_`, plus `filename*=UTF-8''<percent-encoded>` when the
  real name differs from that fallback. Without a filename, just the
  disposition.
- `Download`'s `Debug` shows name, type, disposition, cache policy and body
  kind, plus the length for an in-memory body (a stream prints as `Stream`
  without its length); never the bytes.

`sniff_content_type(head, filename)` recognizes PDF, PNG, JPEG, GIF, WebP,
AVIF, MP4 (`ftyp`), Ogg, MP3 (ID3), gzip, ZIP and the OLE container; the
filename's extension only refines ZIP (`docx`, `xlsx`, `pptx`, `odt`, `ods`,
`odp`) and OLE (`doc`, `xls`, `ppt`). Use it for uploads too: decide the
type from the first bytes, never from the client's `Content-Type`, and store
under a name the server chooses.

### Serve the router elsewhere

`server.router(&health)` returns the complete `axum::Router` (every layer
above, health included) for serving it yourself or testing without a
socket. Pass the runtime's registry (`builder.health()`) so health reflects
the lifecycle.

## Errors and defaults at a glance

| Situation | Result |
|---|---|
| invalid prefix, zero header timeout, connection limit 0 or too large, zero body limit, invalid CORS | `bind`/`from_listener`: `INVALID_ARGUMENT` |
| `grpc_routes` twice, or after `add_service` | `bind`/`from_listener`: `INVALID_ARGUMENT`, `gRPC routes were given more than once; use add_service to add more` |
| address in use | `bind`: `UNAVAILABLE`, `could not listen on <addr>` |
| listener broke while serving | unit fails: `UNAVAILABLE`, `the listener on <addr> failed` |
| `Ctx` without a call context | `INTERNAL`, `the request has no call context` |
| REST body above the limit | `413` |
| no route | `404` |

Builder defaults: no prefix, `grpc_at_root(true)`, `grpc_web(true)`,
`health_routes(true)`, `grpc_health(true)`, `HealthVisibility::Minimal`, no
authenticator, `header_read_timeout(Some(30 s))`,
`max_connections(Some(10_000))`, no CORS, `rest_body_limit(2 MiB)`, no
layers, `access_log(true)`.

## Testing

- **In process, no socket**: build the server (binding `127.0.0.1:0` is
  cheap), take `server.router(&HealthRegistry::new())` and drive it with
  `tower::ServiceExt::oneshot`. This covers routing, CORS, limits, layers
  and the call context:

  ```rust
  use tower::ServiceExt;

  #[tokio::test]
  async fn whoami_echoes_the_request_id() {
      let server = Server::builder()
          .rest(rest_routes())
          .bind(SocketAddr::from(([127, 0, 0, 1], 0)))
          .await
          .unwrap();
      let router = server.router(&HealthRegistry::new());
      let request = http::Request::get("/whoami")
          .header("x-request-id", "r-1")
          .body(axum::body::Body::empty())
          .unwrap();
      let response = router.oneshot(request).await.unwrap();
      assert_eq!(response.status(), http::StatusCode::OK);
  }
  ```

  A fresh `HealthRegistry` was never started, so `/readyz` answers `503`
  there; `/livez` answers `200`.
- **gRPC in process**: send `POST /pkg.Service/Method` with
  `content-type: application/grpc`, HTTP/2 and a framed message (an empty
  message is the five bytes `[0, 0, 0, 0, 0]`); collect the body to read
  `grpc-status` from the trailers. gRPC-Web uses
  `content-type: application/grpc-web+proto` and `x-grpc-web: 1`.
- **Over a real socket**: bind `127.0.0.1:0`, run `server.into_unit()` in a
  runtime built with `without_signals()`, `start()` it, and talk to
  `server.local_addr()`. Socket tests run on the real clock (not
  `start_paused`), assert lower time bounds rather than exact times, and
  guard every wait with a 30 s `tokio::time::timeout`. After
  `handle.shutdown()` and `handle.wait()`, binding the same address again
  proves the port was released.
- An address nobody listens on is `127.0.0.1:1`, never a port you bound and
  dropped.

## Pitfalls

- **gRPC-Web silently missing**: the facade needs `runtime-grpc-web`;
  without it browsers get no translation even though `grpc_web` defaults to
  `true`.
- **REST at the root with a prefix**: once `prefix` is set, REST routes are
  served only under it; the root answers only gRPC and health.
- **Overlapping `rest(..)` routers panic** at build time, like
  `Router::merge`.
- **An authenticator is not an authorization check.** Anonymous requests
  still reach handlers; reject them where needed.
- **Layers see component traffic** on the same listener. Do not put an
  end-user login check in `.layer(..)` if components are served there.
- **CORS headers from handlers are discarded** once CORS is configured; set
  extra exposed headers with `expose_headers`.
- **`Full` health visibility leaks topology** (probe names, version); keep it
  for internal listeners.
- **Raw body handlers** bypass `rest_body_limit`; bound them yourself.
- **Upload limits**: raise `DefaultBodyLimit` on the upload route only, not
  globally with `rest_body_limit`.

## See also

- [Runtime](runtime.md): units, stages, shutdown and the health registry.
- [Call context](context.md): which headers become context, and when they are trusted.
- [Service links](link.md): service tokens and the authenticator.
- [Telemetry](telemetry.md): request ids and the access log.
- [Components](components.md): serving components over gRPC on this listener.
- [Errors](error.md): how `AppError` becomes HTTP and gRPC responses.
