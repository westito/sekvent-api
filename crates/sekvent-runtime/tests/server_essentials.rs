//! Server essentials through `Server::router` and `oneshot`: CORS, the REST
//! body limit, application layers, request ids and the access log.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use axum::Router;
use axum::body::Body;
use axum::extract::{DefaultBodyLimit, State};
use axum::middleware::{Next, from_fn, from_fn_with_state};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use bytes::Bytes;
use http::header::CONTENT_TYPE;
use http::{HeaderMap, HeaderValue, Method, Request, StatusCode, Version};
use http_body_util::BodyExt;
use sekvent_context::CallContext;
use sekvent_error::ErrorCode;
use sekvent_runtime::{Cors, Ctx, HealthRegistry, Server, ServerBuilder};
use sekvent_telemetry::{LogBuffer, LogRecord};
use tonic::server::NamedService;
use tower::ServiceExt;
use tracing::Level;
use tracing::subscriber::DefaultGuard;
use tracing_subscriber::Registry;
use tracing_subscriber::layer::SubscriberExt;

const ORIGIN: &str = "https://app.example.com";
const MIB: usize = 1024 * 1024;

fn localhost() -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], 0))
}

async fn router(builder: ServerBuilder) -> Router {
    builder
        .bind(localhost())
        .await
        .unwrap()
        .router(&HealthRegistry::new())
}

/// A response with its trailers folded into the headers.
struct Answer {
    status: StatusCode,
    headers: HeaderMap,
    body: Bytes,
}

impl Answer {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|value| value.to_str().ok())
    }

    fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    fn has_cors_headers(&self) -> bool {
        self.headers
            .keys()
            .any(|name| name.as_str().starts_with("access-control-"))
    }
}

async fn send(router: &Router, request: Request<Body>) -> Answer {
    let response = router.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let mut headers = response.headers().clone();
    let collected = response.into_body().collect().await.unwrap();
    if let Some(trailers) = collected.trailers() {
        headers.extend(trailers.clone());
    }
    Answer {
        status,
        headers,
        body: collected.to_bytes(),
    }
}

fn get_request(path: &str) -> Request<Body> {
    Request::get(path).body(Body::empty()).unwrap()
}

fn with_origin(path: &str, origin: &str) -> Request<Body> {
    Request::get(path)
        .header("origin", origin)
        .body(Body::empty())
        .unwrap()
}

/// Native gRPC as a tonic client sends it (HTTP/2, an empty message).
fn grpc(path: &str) -> Request<Body> {
    Request::post(path)
        .version(Version::HTTP_2)
        .header(CONTENT_TYPE, "application/grpc")
        .body(Body::from(vec![0_u8; 5]))
        .unwrap()
}

/// gRPC-Web as a browser sends it (HTTP/1.1, an empty message).
fn grpc_web(path: &str, origin: Option<&str>) -> Request<Body> {
    let mut request = Request::post(path)
        .header(CONTENT_TYPE, "application/grpc-web+proto")
        .header("x-grpc-web", "1");
    if let Some(origin) = origin {
        request = request.header("origin", origin);
    }
    request.body(Body::from(vec![0_u8; 5])).unwrap()
}

fn json(bytes: usize) -> String {
    let mut text = String::with_capacity(bytes + 2);
    text.push('"');
    text.extend(std::iter::repeat_n('a', bytes));
    text.push('"');
    text
}

fn post_json(path: &str, bytes: usize) -> Request<Body> {
    Request::post(path)
        .header(CONTENT_TYPE, "application/json")
        .body(Body::from(json(bytes)))
        .unwrap()
}

/// The application's REST routes; `calls` counts handler runs.
fn routes(calls: Arc<AtomicUsize>) -> Router {
    Router::new()
        .route(
            "/echo",
            get(move |Ctx(ctx): Ctx| {
                calls.fetch_add(1, Ordering::SeqCst);
                async move { ctx.request_id().to_owned() }
            })
            .post(|_: axum::Json<serde_json::Value>| async { "stored" })
            .options(|| async { "options handled" }),
        )
        .route(
            "/upload",
            post(|_: axum::Json<serde_json::Value>| async { "uploaded" })
                .layer(DefaultBodyLimit::max(16 * MIB)),
        )
        .route("/orders/{id}", get(|| async { "order" }))
        .route("/fail", get(|| async { StatusCode::INTERNAL_SERVER_ERROR }))
        .route(
            "/traced",
            get(|| async {
                tracing::info!("handled");
                "traced"
            }),
        )
}

fn rest() -> Router {
    routes(Arc::default())
}

/// A hand-written gRPC service answering trailers-only with `code`, and the
/// request id of the `CallContext` it saw in `x-ctx-id`.
#[derive(Clone)]
struct Probe {
    code: &'static str,
}

impl NamedService for Probe {
    const NAME: &'static str = "test.Probe";
}

impl tower::Service<Request<tonic::body::Body>> for Probe {
    type Response = Response;
    type Error = Infallible;
    type Future = std::future::Ready<Result<Response, Infallible>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Infallible>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, request: Request<tonic::body::Body>) -> Self::Future {
        let id = request
            .extensions()
            .get::<CallContext>()
            .map(|ctx| ctx.request_id().to_owned())
            .unwrap_or_default();
        let response = Response::builder()
            .header(CONTENT_TYPE, "application/grpc")
            .header("grpc-status", self.code)
            .header("x-ctx-id", id)
            .body(Body::empty())
            .unwrap();
        std::future::ready(Ok(response))
    }
}

fn capture() -> (LogBuffer, DefaultGuard) {
    let buffer = LogBuffer::new(256);
    let guard = tracing::subscriber::set_default(Registry::default().with(buffer.layer()));
    (buffer, guard)
}

fn access(buffer: &LogBuffer) -> Vec<LogRecord> {
    buffer
        .snapshot()
        .into_iter()
        .filter(|record| record.target == "sekvent::access")
        .collect()
}

/// The only access event logged since the last `clear`.
fn last_access(buffer: &LogBuffer) -> LogRecord {
    let mut records = access(buffer);
    assert_eq!(records.len(), 1, "{records:?}");
    buffer.clear();
    records.remove(0)
}

// ---------------------------------------------------------------- CORS

#[tokio::test]
async fn a_preflight_is_answered_before_authentication_and_handlers() {
    let calls = Arc::new(AtomicUsize::new(0));
    let authenticated = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&authenticated);
    let router = router(
        Server::builder()
            .prefix("/api")
            .rest(routes(Arc::clone(&calls)))
            .authenticator(move |_parts| {
                counter.fetch_add(1, Ordering::SeqCst);
                None
            })
            .cors(Cors::origins([ORIGIN]).unwrap()),
    )
    .await;

    let preflight = Request::builder()
        .method(Method::OPTIONS)
        .uri("/api/echo")
        .header("origin", ORIGIN)
        .header("access-control-request-method", "POST")
        .header(
            "access-control-request-headers",
            "content-type,authorization",
        )
        .body(Body::empty())
        .unwrap();
    let answer = send(&router, preflight).await;
    assert_eq!(answer.status, StatusCode::OK);
    assert_eq!(answer.header("access-control-allow-origin"), Some(ORIGIN));
    let methods = answer.header("access-control-allow-methods").unwrap();
    for method in ["GET", "HEAD", "POST", "PUT", "PATCH", "DELETE"] {
        assert!(methods.contains(method), "{methods}");
    }
    let headers = answer.header("access-control-allow-headers").unwrap();
    for name in [
        "authorization",
        "content-type",
        "x-grpc-web",
        "x-user-agent",
    ] {
        assert!(headers.contains(name), "{headers}");
    }
    assert_eq!(answer.header("access-control-max-age"), Some("3600"));
    assert!(answer.header("x-request-id").is_some());
    assert_eq!(calls.load(Ordering::SeqCst), 0, "the handler never ran");
    assert_eq!(
        authenticated.load(Ordering::SeqCst),
        0,
        "nor authentication"
    );

    let rest = send(&router, with_origin("/api/echo", ORIGIN)).await;
    assert_eq!(rest.status, StatusCode::OK);
    assert_eq!(rest.header("access-control-allow-origin"), Some(ORIGIN));
    assert!(
        rest.header("access-control-expose-headers")
            .unwrap()
            .contains("grpc-status")
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(authenticated.load(Ordering::SeqCst), 1);

    let web = send(
        &router,
        grpc_web("/api/grpc.health.v1.Health/Check", Some(ORIGIN)),
    )
    .await;
    assert_eq!(web.status, StatusCode::OK);
    assert_eq!(web.header("access-control-allow-origin"), Some(ORIGIN));
    let exposed = web.header("access-control-expose-headers").unwrap();
    assert!(exposed.contains("grpc-status"), "{exposed}");
    assert!(exposed.contains("grpc-message"), "{exposed}");

    let refused = send(
        &router,
        with_origin("/api/echo", "https://evil.example.com"),
    )
    .await;
    assert_eq!(refused.status, StatusCode::OK, "the browser blocks it");
    assert!(!refused.has_cors_headers(), "{:?}", refused.headers);
    let refused_preflight = Request::builder()
        .method(Method::OPTIONS)
        .uri("/api/echo")
        .header("origin", "https://evil.example.com")
        .header("access-control-request-method", "POST")
        .body(Body::empty())
        .unwrap();
    let refused = send(&router, refused_preflight).await;
    assert!(!refused.has_cors_headers(), "{:?}", refused.headers);
    assert_eq!(
        calls.load(Ordering::SeqCst),
        2,
        "the refused GET ran, the refused preflight did not"
    );

    let plain_options = Request::builder()
        .method(Method::OPTIONS)
        .uri("/api/echo")
        .header("origin", ORIGIN)
        .body(Body::empty())
        .unwrap();
    let answer = send(&router, plain_options).await;
    assert_eq!(answer.text(), "options handled", "not a preflight");
    assert_eq!(answer.header("access-control-allow-origin"), Some(ORIGIN));
    assert_eq!(authenticated.load(Ordering::SeqCst), 3);
}

/// An application layer with its own, wider CORS answer.
async fn reflect_cors(request: Request<Body>, next: Next) -> Response {
    let origin = request.headers().get("origin").cloned();
    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    if let Some(origin) = origin {
        headers.insert("access-control-allow-origin", origin);
    }
    headers.insert(
        "access-control-allow-credentials",
        HeaderValue::from_static("true"),
    );
    headers.insert(
        "access-control-allow-headers",
        HeaderValue::from_static("*"),
    );
    headers.append("vary", HeaderValue::from_static("origin"));
    response
}

#[tokio::test]
async fn the_server_cors_rules_override_an_inner_cors_layer() {
    let router = router(
        Server::builder()
            .rest(rest())
            .layer(from_fn(reflect_cors))
            .cors(Cors::origins([ORIGIN]).unwrap().allow_credentials(true)),
    )
    .await;
    let refused = send(&router, with_origin("/echo", "https://evil.example.com")).await;
    assert_eq!(refused.status, StatusCode::OK);
    assert!(!refused.has_cors_headers(), "{:?}", refused.headers);
    assert_eq!(refused.headers.get_all("vary").iter().count(), 1);
    assert_eq!(refused.header("vary"), Some("origin"));

    let allowed = send(&router, with_origin("/echo", ORIGIN)).await;
    assert_eq!(allowed.header("access-control-allow-origin"), Some(ORIGIN));
    assert_eq!(
        allowed.header("access-control-allow-credentials"),
        Some("true")
    );
    assert!(allowed.header("access-control-allow-headers").is_none());
}

#[tokio::test]
async fn credentials_need_a_list_and_vary_by_origin() {
    let router = router(
        Server::builder()
            .rest(rest())
            .cors(Cors::origins([ORIGIN]).unwrap().allow_credentials(true)),
    )
    .await;
    let answer = send(&router, with_origin("/echo", ORIGIN)).await;
    assert_eq!(answer.header("access-control-allow-origin"), Some(ORIGIN));
    assert_eq!(
        answer.header("access-control-allow-credentials"),
        Some("true")
    );
    assert_eq!(answer.header("vary"), Some("origin"));

    let any = router_with_any_origin().await;
    let answer = send(&any, with_origin("/echo", "https://anywhere.example.com")).await;
    assert_eq!(answer.header("access-control-allow-origin"), Some("*"));
    assert!(answer.header("vary").is_none());

    let error = Server::builder()
        .cors(Cors::any_origin().allow_credentials(true))
        .bind(localhost())
        .await
        .unwrap_err();
    assert_eq!(error.code(), ErrorCode::InvalidArgument);
    assert!(
        error.message().contains("explicit origin list"),
        "{}",
        error.message()
    );
}

async fn router_with_any_origin() -> Router {
    router(Server::builder().rest(rest()).cors(Cors::any_origin())).await
}

#[tokio::test]
async fn without_cors_there_are_no_cors_headers() {
    let router = router(Server::builder().rest(rest())).await;
    let answer = send(&router, with_origin("/echo", ORIGIN)).await;
    assert_eq!(answer.status, StatusCode::OK);
    assert!(!answer.has_cors_headers());
}

// ---------------------------------------------------------- body limit

#[tokio::test]
async fn the_rest_body_limit_defaults_to_two_mib() {
    let router = router(Server::builder().rest(rest())).await;
    let answer = send(&router, post_json("/echo", 3 * MIB)).await;
    assert_eq!(answer.status, StatusCode::PAYLOAD_TOO_LARGE);
    let answer = send(&router, post_json("/echo", MIB)).await;
    assert_eq!(
        (answer.status, answer.text()),
        (StatusCode::OK, "stored".into())
    );
}

#[tokio::test]
async fn the_rest_body_limit_can_be_raised() {
    let router = router(Server::builder().rest(rest()).rest_body_limit(4 * MIB)).await;
    let answer = send(&router, post_json("/echo", 3 * MIB)).await;
    assert_eq!(answer.status, StatusCode::OK);
}

#[tokio::test]
async fn a_route_can_raise_its_own_limit() {
    let router = router(Server::builder().prefix("/api").rest(rest())).await;
    let answer = send(&router, post_json("/api/upload", 10 * MIB)).await;
    assert_eq!(
        (answer.status, answer.text()),
        (StatusCode::OK, "uploaded".into())
    );
    let answer = send(&router, post_json("/api/echo", 3 * MIB)).await;
    assert_eq!(answer.status, StatusCode::PAYLOAD_TOO_LARGE);
}

#[tokio::test]
async fn a_zero_body_limit_fails_bind() {
    let error = Server::builder()
        .rest_body_limit(0)
        .bind(localhost())
        .await
        .unwrap_err();
    assert_eq!(error.code(), ErrorCode::InvalidArgument);
    assert!(
        error.message().contains("body limit"),
        "{}",
        error.message()
    );
}

// -------------------------------------------------------------- layers

#[derive(Clone)]
struct Tag {
    name: &'static str,
    order: Arc<Mutex<Vec<&'static str>>>,
}

async fn tag(State(tag): State<Tag>, request: Request<Body>, next: Next) -> Response {
    tag.order.lock().unwrap().push(tag.name);
    let mut response = next.run(request).await;
    response
        .headers_mut()
        .append("x-layer", HeaderValue::from_static(tag.name));
    response
}

async fn read_context(request: Request<Body>, next: Next) -> Response {
    let id = request
        .extensions()
        .get::<CallContext>()
        .map_or_else(|| "none".to_owned(), |ctx| ctx.request_id().to_owned());
    let mut response = next.run(request).await;
    response
        .headers_mut()
        .insert("x-layer-ctx", HeaderValue::from_str(&id).unwrap());
    response
}

#[tokio::test]
async fn layers_wrap_every_protocol_but_not_health() {
    let order = Arc::new(Mutex::new(Vec::new()));
    let tagged = |name| Tag {
        name,
        order: Arc::clone(&order),
    };
    let router = router(
        Server::builder()
            .prefix("/api")
            .rest(rest())
            .add_service(Probe { code: "0" })
            .layer(from_fn(read_context))
            .layer(from_fn_with_state(tagged("inner"), tag))
            .layer(from_fn_with_state(tagged("outer"), tag)),
    )
    .await;

    let answer = send(&router, get_request("/api/echo")).await;
    assert_eq!(answer.status, StatusCode::OK);
    assert_eq!(answer.headers.get_all("x-layer").iter().count(), 2);
    assert_eq!(*order.lock().unwrap(), ["outer", "inner"]);
    let id = answer.header("x-request-id").unwrap();
    assert_eq!(
        answer.header("x-layer-ctx"),
        Some(id),
        "the layer read the context"
    );
    assert_eq!(answer.text(), id);

    let native = send(&router, grpc("/test.Probe/Get")).await;
    assert_eq!(native.header("grpc-status"), Some("0"));
    assert_eq!(native.headers.get_all("x-layer").iter().count(), 2);

    let web = send(&router, grpc_web("/api/test.Probe/Get", None)).await;
    assert_eq!(web.status, StatusCode::OK);
    assert_eq!(web.headers.get_all("x-layer").iter().count(), 2);
    assert!(web.header("x-layer-ctx").is_some());

    order.lock().unwrap().clear();
    for request in [
        get_request("/readyz"),
        grpc("/grpc.health.v1.Health/Check"),
        grpc("/api/grpc.health.v1.Health/Check"),
        grpc_web("/api/grpc.health.v1.Health/Check", None),
    ] {
        let path = request.uri().path().to_owned();
        let health = send(&router, request).await;
        assert!(health.header("x-layer").is_none(), "{path}");
        assert!(health.header("x-layer-ctx").is_none(), "{path}");
        assert!(health.header("x-request-id").is_some(), "{path}");
    }
    assert!(order.lock().unwrap().is_empty());

    let debug = format!(
        "{:?}",
        Server::builder()
            .layer(from_fn(read_context))
            .cors(Cors::any_origin())
            .rest_body_limit(MIB)
            .access_log(false)
    );
    for part in [
        "layers: 1",
        "rest_body_limit: 1048576",
        "access_log: false",
        "cors: Some(",
    ] {
        assert!(debug.contains(part), "{debug}");
    }
}

async fn reject(_request: Request<Body>, _next: Next) -> Response {
    StatusCode::UNAUTHORIZED.into_response()
}

#[tokio::test]
async fn a_rejecting_layer_never_fails_grpc_health() {
    let authenticated = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&authenticated);
    let router = router(
        Server::builder()
            .prefix("/api")
            .rest(rest())
            .add_service(Probe { code: "0" })
            .authenticator(move |_parts| {
                counter.fetch_add(1, Ordering::SeqCst);
                None
            })
            .layer(from_fn(reject)),
    )
    .await;

    for path in [
        "/grpc.health.v1.Health/Check",
        "/api/grpc.health.v1.Health/Check",
    ] {
        let native = send(&router, grpc(path)).await;
        assert_eq!(native.status, StatusCode::OK, "{path}");
        assert_eq!(native.header("grpc-status"), Some("0"), "{path}");
    }
    let web = send(&router, grpc_web("/api/grpc.health.v1.Health/Check", None)).await;
    assert_eq!(web.status, StatusCode::OK);
    let trailer = b"grpc-status:0";
    assert!(
        web.body
            .windows(trailer.len())
            .any(|window| window == trailer),
        "{:?}",
        web.body
    );
    assert_eq!(authenticated.load(Ordering::SeqCst), 0);

    let plain = send(&router, get_request("/grpc.health.v1.Health/Check")).await;
    assert_eq!(
        plain.status,
        StatusCode::NOT_FOUND,
        "gRPC by content type only"
    );

    let refused = send(&router, get_request("/api/echo")).await;
    assert_eq!(refused.status, StatusCode::UNAUTHORIZED);
    let probe = send(&router, grpc("/test.Probe/Get")).await;
    assert_eq!(probe.status, StatusCode::UNAUTHORIZED);
    assert_eq!(authenticated.load(Ordering::SeqCst), 2);

    let only_prefixed = router_without_root_grpc().await;
    let prefixed = send(&only_prefixed, grpc("/api/grpc.health.v1.Health/Check")).await;
    assert_eq!(prefixed.header("grpc-status"), Some("0"));
    let root = send(&only_prefixed, grpc("/grpc.health.v1.Health/Check")).await;
    assert_ne!(root.status, StatusCode::OK, "no native gRPC at the root");
    assert!(root.header("grpc-status").is_none());
}

async fn router_without_root_grpc() -> Router {
    router(
        Server::builder()
            .prefix("/api")
            .grpc_at_root(false)
            .layer(from_fn(reject)),
    )
    .await
}

// ----------------------------------------------------------------- ids

#[tokio::test]
async fn the_response_id_is_the_context_id() {
    let router = router(
        Server::builder()
            .rest(rest())
            .add_service(Probe { code: "0" }),
    )
    .await;

    let request = Request::get("/echo")
        .header("x-request-id", "req-1")
        .body(Body::empty())
        .unwrap();
    let answer = send(&router, request).await;
    assert_eq!(answer.header("x-request-id"), Some("req-1"));
    assert_eq!(answer.text(), "req-1");

    let request = Request::get("/echo")
        .header("x-request-id", "has space")
        .body(Body::empty())
        .unwrap();
    let answer = send(&router, request).await;
    let id = answer.header("x-request-id").unwrap();
    assert_ne!(id, "has space");
    assert_eq!(id.len(), 36, "a fresh UUID: {id}");
    assert_eq!(answer.text(), id);

    let native = send(&router, grpc("/test.Probe/Get")).await;
    let id = native.header("x-request-id").unwrap();
    assert_eq!(native.header("x-ctx-id"), Some(id));

    for path in ["/livez", "/readyz", "/healthz"] {
        let health = send(&router, get_request(path)).await;
        assert!(health.header("x-request-id").is_some(), "{path}");
    }
}

// ---------------------------------------------------------- access log

#[tokio::test]
async fn one_event_per_request_with_every_field_and_no_secrets() {
    let (buffer, _guard) = capture();
    let router = router(Server::builder().prefix("/api").rest(rest())).await;

    let request = Request::get("/api/orders/7?token=s3cret")
        .header("authorization", "Bearer t0psecret")
        .body(Body::empty())
        .unwrap();
    let answer = send(&router, request).await;
    assert_eq!(answer.status, StatusCode::OK);
    for record in buffer.snapshot() {
        let text = format!("{record:?}");
        assert!(!text.contains("s3cret"), "{text}");
        assert!(!text.contains("t0psecret"), "{text}");
    }
    let record = last_access(&buffer);
    assert_eq!(record.level, Level::INFO);
    assert_eq!(record.message, "request completed");
    assert_eq!(record.fields["method"], "GET");
    assert_eq!(record.fields["path"], "/api/orders/7");
    assert_eq!(record.fields["route"], "/api/orders/{id}");
    assert_eq!(record.fields["protocol"], "http");
    assert_eq!(record.fields["status"], "200");
    assert_eq!(record.fields["aborted"], "false");
    assert!(record.fields["latency_ms"].parse::<u64>().is_ok());
    assert_eq!(
        Some(record.fields["request_id"].as_str()),
        answer.header("x-request-id")
    );
    assert!(!record.fields.contains_key("grpc_status"));
    send(&router, get_request("/api/missing")).await;
    let record = last_access(&buffer);
    assert_eq!(record.fields["status"], "404");
    assert_eq!(record.fields["route"], "");
}

#[tokio::test]
async fn routes_without_a_prefix_are_logged_as_declared() {
    let (buffer, _guard) = capture();
    let router = router(Server::builder().rest(rest())).await;
    send(&router, get_request("/orders/7")).await;
    assert_eq!(last_access(&buffer).fields["route"], "/orders/{id}");
    send(&router, get_request("/livez")).await;
    assert_eq!(last_access(&buffer).fields["route"], "/livez");
}

#[tokio::test]
async fn levels_follow_the_outcome() {
    let (buffer, _guard) = capture();
    let router = router(
        Server::builder()
            .rest(rest())
            .add_service(Probe { code: "5" }),
    )
    .await;

    send(&router, get_request("/readyz")).await;
    let record = last_access(&buffer);
    assert_eq!(record.level, Level::DEBUG, "health is quiet");
    assert_eq!(record.fields["status"], "503");

    send(&router, get_request("/fail")).await;
    let record = last_access(&buffer);
    assert_eq!(record.level, Level::WARN);
    assert_eq!(record.fields["status"], "500");

    let answer = send(&router, grpc("/grpc.health.v1.Health/Check")).await;
    assert_eq!(answer.header("grpc-status"), Some("0"));
    let record = last_access(&buffer);
    assert_eq!(record.fields["grpc_status"], "0", "from the trailers");
    assert_eq!(record.fields["protocol"], "grpc");
    assert_eq!(record.level, Level::DEBUG);

    send(&router, grpc("/test.Probe/Get")).await;
    let record = last_access(&buffer);
    assert_eq!(record.fields["grpc_status"], "5", "trailers-only");
    assert_eq!(record.fields["route"], "/test.Probe/Get");
    assert_eq!(record.level, Level::INFO);

    let web = send(&router, grpc_web("/grpc.health.v1.Health/Check", None)).await;
    assert_eq!(web.status, StatusCode::OK);
    let record = last_access(&buffer);
    assert_eq!(record.fields["protocol"], "grpc-web");
    assert_eq!(record.fields["grpc_status"], "0", "from the trailer frame");
}

#[tokio::test]
async fn health_is_quiet_wherever_it_is_mounted() {
    let (buffer, _guard) = capture();
    let app = router(Server::builder().prefix("/api").rest(rest())).await;

    for request in [
        grpc("/grpc.health.v1.Health/Check"),
        grpc("/api/grpc.health.v1.Health/Check"),
        grpc_web("/api/grpc.health.v1.Health/Check", None),
        get_request("/livez"),
    ] {
        let path = request.uri().path().to_owned();
        send(&app, request).await;
        let record = last_access(&buffer);
        assert_eq!(record.level, Level::DEBUG, "{path}");
        assert_eq!(record.fields["aborted"], "false", "{path}");
    }
    let head = Request::head("/livez").body(Body::empty()).unwrap();
    let answer = send(&app, head).await;
    assert_eq!(answer.status, StatusCode::OK);
    let record = last_access(&buffer);
    assert_eq!(
        record.fields["aborted"], "false",
        "a HEAD answer has no body"
    );
    assert_eq!(record.fields["method"], "HEAD");
    assert_eq!(record.level, Level::DEBUG);

    send(&app, get_request("/api/echo")).await;
    assert_eq!(last_access(&buffer).level, Level::INFO);

    let only_prefixed = router(
        Server::builder()
            .prefix("/api")
            .grpc_at_root(false)
            .rest(rest()),
    )
    .await;
    send(&only_prefixed, grpc("/grpc.health.v1.Health/Check")).await;
    assert_eq!(
        last_access(&buffer).level,
        Level::INFO,
        "nothing is mounted at the root"
    );
    send(&only_prefixed, grpc("/api/grpc.health.v1.Health/Check")).await;
    assert_eq!(last_access(&buffer).level, Level::DEBUG);
}

#[tokio::test]
async fn the_request_id_is_on_failures_logged_under_a_warn_filter() {
    let buffer = LogBuffer::new(16);
    let _guard = tracing::subscriber::set_default(
        Registry::default()
            .with(tracing_subscriber::filter::LevelFilter::WARN)
            .with(buffer.layer()),
    );
    let router = router(Server::builder().rest(rest())).await;
    let request = Request::get("/fail")
        .header("x-request-id", "fail-1")
        .body(Body::empty())
        .unwrap();
    send(&router, request).await;
    send(&router, get_request("/echo")).await;
    let record = last_access(&buffer);
    assert_eq!(record.level, Level::WARN);
    assert_eq!(record.fields["request_id"], "fail-1");
}

#[tokio::test]
async fn without_the_access_log_ids_and_the_span_stay() {
    let (buffer, _guard) = capture();
    let router = router(Server::builder().rest(rest()).access_log(false)).await;
    let answer = send(&router, get_request("/traced")).await;
    assert_eq!(answer.text(), "traced");
    let id = answer.header("x-request-id").unwrap();
    assert!(access(&buffer).is_empty());
    let handled = buffer
        .snapshot()
        .into_iter()
        .find(|record| record.message == "handled")
        .expect("the handler logged");
    assert_eq!(handled.fields["request_id"], id);
    assert_eq!(handled.fields["path"], "/traced");
}
