//! A built frontend served next to the API: `StaticSite` over embedded and
//! on-disk files, mounted with `ServerBuilder::site` and driven through
//! `Server::router` with `oneshot`.
#![cfg(feature = "vfs-embed")]

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use axum::Router;
use axum::body::Body;
use axum::middleware::{Next, from_fn};
use axum::response::Response;
use axum::routing::get;
use bytes::Bytes;
use fixture::Fixture;
use http::header::{
    ACCEPT, ACCEPT_ENCODING, CACHE_CONTROL, CONTENT_ENCODING, CONTENT_LENGTH, CONTENT_TYPE, ETAG,
    IF_NONE_MATCH, LOCATION, X_FRAME_OPTIONS,
};
use http::{HeaderMap, HeaderValue, Method, Request, StatusCode, Version};
use http_body_util::BodyExt;
use sekvent_context::CallContext;
use sekvent_error::ErrorCode;
use sekvent_runtime::vfs::{Directory, Embedded, StaticSite, Vfs, VfsPath};
use sekvent_runtime::{Cors, Ctx, HealthRegistry, Server, ServerBuilder};
use tower::ServiceExt;

#[allow(
    dead_code,
    unreachable_pub,
    reason = "a marker type; the derive adds functions this test does not call"
)]
mod fixture {
    use sekvent_runtime::vfs::RustEmbed;

    /// The fixture site, embedded through the re-exported `rust-embed`.
    #[derive(RustEmbed)]
    #[crate_path = "sekvent_runtime::vfs::rust_embed"]
    #[folder = "tests/fixtures/site"]
    pub(crate) struct Fixture;
}

const INDEX: &str = "<!doctype html>\n<title>fixture</title>\n<div id=\"root\"></div>\n";
const SCRIPT: &str = "/assets/app-3f2a1c.js";
const ORIGIN: &str = "https://app.example.com";

fn localhost() -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], 0))
}

fn embedded() -> StaticSite {
    StaticSite::new(Arc::new(Embedded::<Fixture>::new()))
}

fn api() -> Router {
    Router::new().route(
        "/hello",
        get(|Ctx(ctx): Ctx| async move { format!("hello {}", ctx.request_id()) }),
    )
}

async fn router(builder: ServerBuilder) -> Router {
    builder
        .bind(localhost())
        .await
        .unwrap()
        .router(&HealthRegistry::new())
}

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

/// A browser navigation.
fn page(path: &str) -> Request<Body> {
    Request::get(path)
        .header(ACCEPT, "text/html,application/xhtml+xml,*/*;q=0.8")
        .body(Body::empty())
        .unwrap()
}

fn grpc_check(path: &str) -> Request<Body> {
    Request::post(path)
        .version(Version::HTTP_2)
        .header(CONTENT_TYPE, "application/grpc")
        .body(Body::from(vec![0_u8, 0, 0, 0, 0]))
        .unwrap()
}

#[tokio::test]
async fn embedded_files_are_served_with_caching_and_variants() {
    let site = Router::new().fallback_service(embedded());

    let index = send(&site, page("/")).await;
    assert_eq!(index.status, StatusCode::OK);
    assert_eq!(index.text(), INDEX);
    assert_eq!(index.header("cache-control"), Some("no-cache"));

    let script = send(&site, page(SCRIPT)).await;
    assert_eq!(script.text(), "console.log(\"fixture\");\n");
    assert_eq!(
        script.header("cache-control"),
        Some("public, max-age=31536000, immutable")
    );
    let etag = script.header("etag").unwrap().to_owned();

    let revalidate = Request::get(SCRIPT)
        .header(IF_NONE_MATCH, etag.as_str())
        .body(Body::empty())
        .unwrap();
    let answer = send(&site, revalidate).await;
    assert_eq!(answer.status, StatusCode::NOT_MODIFIED);
    assert!(answer.body.is_empty());

    for (accept, body, coding) in [
        ("gzip, br", "pretend-brotli-bytes\n", "br"),
        ("gzip", "pretend-gzip-bytes\n", "gzip"),
    ] {
        let request = Request::get(SCRIPT)
            .header(ACCEPT_ENCODING, accept)
            .body(Body::empty())
            .unwrap();
        let answer = send(&site, request).await;
        assert_eq!(answer.text(), body);
        assert_eq!(answer.header("content-encoding"), Some(coding));
        assert!(
            answer
                .header("content-type")
                .unwrap()
                .contains("javascript")
        );
        assert_ne!(answer.header("etag"), Some(etag.as_str()));
        assert_eq!(answer.header("vary"), Some("Accept-Encoding"));
    }

    let route = send(&site, page("/clusters/prod/nodes")).await;
    assert_eq!(route.status, StatusCode::OK);
    assert_eq!(route.text(), INDEX);
    let dotted = send(&site, page("/clusters/prod/nodes/10.0.0.1")).await;
    assert_eq!(dotted.status, StatusCode::OK, "dots in a client route");
    assert_eq!(dotted.text(), INDEX);
    let redirect = send(&site, page("/docs?tab=api")).await;
    assert_eq!(redirect.status, StatusCode::PERMANENT_REDIRECT);
    assert_eq!(redirect.header("location"), Some("/docs/?tab=api"));
    let docs = send(&site, page("/docs/")).await;
    assert_eq!(docs.text(), "<!doctype html>\n<title>docs</title>\n");
    let missing = send(&site, page("/assets/app-0000.js")).await;
    assert_eq!(missing.status, StatusCode::NOT_FOUND);
    let traversal = send(&site, page("/assets/%2e%2e/%2e%2e/Cargo.toml")).await;
    assert_eq!(traversal.status, StatusCode::NOT_FOUND);
    let fetch = Request::get("/clusters/10.0.0.1")
        .header(ACCEPT, "*/*")
        .body(Body::empty())
        .unwrap();
    assert_eq!(send(&site, fetch).await.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn embedded_and_directory_files_agree() {
    let root = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/site");
    let directory = Directory::new(root).unwrap();
    let embedded = Embedded::<Fixture>::new();
    for raw in [
        "index.html",
        "robots.txt",
        "docs/index.html",
        "assets/app-3f2a1c.js",
        "assets/app-3f2a1c.js.br",
        "assets/style-9b8e7d.css",
    ] {
        let path = VfsPath::new(raw).unwrap();
        let on_disk = directory.get(&path).await.unwrap().unwrap();
        let compiled_in = embedded.get(&path).await.unwrap().unwrap();
        assert_eq!(on_disk.contents(), compiled_in.contents(), "{raw}");
        assert_eq!(on_disk.etag(), compiled_in.etag(), "{raw}");
        assert_eq!(on_disk.content_type(), compiled_in.content_type(), "{raw}");
    }
}

#[tokio::test]
async fn the_site_serves_everything_outside_the_prefix() {
    let health = HealthRegistry::new();
    let router = Server::builder()
        .prefix("/api")
        .rest(api())
        .grpc_health(false)
        .add_service(health.grpc_service())
        .site(embedded())
        .bind(localhost())
        .await
        .unwrap()
        .router(&health);

    let answer = send(&router, page("/api/hello")).await;
    assert_eq!(answer.status, StatusCode::OK);
    assert!(answer.text().starts_with("hello "), "{}", answer.text());

    for path in ["/api/missing", "/api", "/api/", "/api/assets/app-3f2a1c.js"] {
        let answer = send(&router, page(path)).await;
        assert_eq!(answer.status, StatusCode::NOT_FOUND, "{path}");
        assert_ne!(answer.text(), INDEX, "{path} must stay the API's");
    }

    for path in ["/", "/clusters", "/apiary", "/api-docs/x"] {
        let answer = send(&router, page(path)).await;
        assert_eq!(answer.status, StatusCode::OK, "{path}");
        assert_eq!(answer.text(), INDEX, "{path}");
        assert!(answer.header("x-request-id").is_some(), "{path}");
        assert_eq!(answer.header("x-frame-options"), Some("DENY"), "{path}");
    }
    let answer = send(&router, page(SCRIPT)).await;
    assert_eq!(answer.text(), "console.log(\"fixture\");\n");

    for path in ["/livez", "/readyz", "/healthz"] {
        let answer = send(&router, page(path)).await;
        assert_ne!(answer.text(), INDEX, "{path} is a health endpoint");
    }

    let answer = send(&router, grpc_check("/grpc.health.v1.Health/Check")).await;
    assert_eq!(
        answer.header("grpc-status"),
        Some("0"),
        "native gRPC at the root"
    );
    let answer = send(&router, grpc_check("/api/grpc.health.v1.Health/Check")).await;
    assert_eq!(
        answer.header("grpc-status"),
        Some("0"),
        "gRPC under the prefix"
    );

    let post = Request::post("/clusters")
        .header(CONTENT_TYPE, "application/json")
        .body(Body::from("{}"))
        .unwrap();
    let answer = send(&router, post).await;
    assert_eq!(answer.status, StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(answer.header("allow"), Some("GET, HEAD"));
}

#[tokio::test]
async fn without_grpc_at_the_root_grpc_requests_do_not_reach_the_site() {
    let router = router(
        Server::builder()
            .prefix("/api")
            .grpc_at_root(false)
            .grpc_health(false)
            .site(embedded()),
    )
    .await;
    let answer = send(&router, grpc_check("/grpc.health.v1.Health/Check")).await;
    assert_eq!(answer.status, StatusCode::NOT_FOUND);
    assert_eq!(send(&router, page("/")).await.text(), INDEX);
}

async fn mark(request: Request<Body>, next: Next) -> Response {
    let anonymous = request
        .extensions()
        .get::<CallContext>()
        .is_some_and(|ctx| ctx.caller().is_none());
    let mut response = next.run(request).await;
    response.headers_mut().insert(
        "x-anonymous",
        HeaderValue::from_static(if anonymous { "yes" } else { "no" }),
    );
    response
}

#[tokio::test]
async fn the_site_is_public_but_keeps_the_server_essentials() {
    let authenticated = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&authenticated);
    let router = router(
        Server::builder()
            .prefix("/api")
            .rest(api())
            .authenticator(move |_parts| {
                counter.fetch_add(1, Ordering::SeqCst);
                None
            })
            .layer(from_fn(mark))
            .cors(Cors::origins([ORIGIN]).unwrap())
            .site(embedded()),
    )
    .await;

    let request = Request::get("/")
        .header("origin", ORIGIN)
        .header("x-request-id", "site-1")
        .header(ACCEPT, "text/html")
        .body(Body::empty())
        .unwrap();
    let answer = send(&router, request).await;
    assert_eq!(answer.status, StatusCode::OK, "an anonymous caller");
    assert_eq!(answer.text(), INDEX);
    assert_eq!(answer.header("x-request-id"), Some("site-1"));
    assert_eq!(answer.header("access-control-allow-origin"), Some(ORIGIN));
    assert_eq!(answer.header("x-anonymous"), Some("yes"), "layers apply");
    assert_eq!(authenticated.load(Ordering::SeqCst), 1);

    let preflight = Request::builder()
        .method(Method::OPTIONS)
        .uri("/assets/app-3f2a1c.js")
        .header("origin", ORIGIN)
        .header("access-control-request-method", "GET")
        .body(Body::empty())
        .unwrap();
    let answer = send(&router, preflight).await;
    assert_eq!(
        answer.status,
        StatusCode::OK,
        "CORS answers preflights first"
    );
}

#[tokio::test]
async fn a_site_needs_a_prefix_and_comes_once() {
    let error = Server::builder()
        .site(embedded())
        .bind(localhost())
        .await
        .unwrap_err();
    assert_eq!(error.code(), ErrorCode::InvalidArgument);
    assert!(error.to_string().contains("prefix"), "{error}");

    let error = Server::builder()
        .prefix("/api")
        .site(embedded())
        .site(embedded())
        .bind(localhost())
        .await
        .unwrap_err();
    assert_eq!(error.code(), ErrorCode::InvalidArgument);
    assert!(error.to_string().contains("more than once"), "{error}");

    let builder = Server::builder().prefix("/api").site(embedded());
    assert!(format!("{builder:?}").contains("site: true"));
}

#[tokio::test]
async fn head_and_conditional_requests_pass_through_the_server() {
    let router = router(Server::builder().prefix("/api").site(embedded())).await;
    for path in [SCRIPT, "/", "/clusters/10.0.0.1"] {
        let full = send(&router, page(path)).await;
        assert_eq!(full.status, StatusCode::OK, "{path}");
        let length = full.body.len().to_string();
        assert_eq!(
            full.header("content-length"),
            Some(length.as_str()),
            "{path}"
        );

        let mut head = page(path);
        *head.method_mut() = Method::HEAD;
        let answer = send(&router, head).await;
        assert_eq!(answer.status, StatusCode::OK, "{path}");
        assert!(answer.body.is_empty(), "{path}");
        assert_eq!(
            answer.header("content-length"),
            Some(length.as_str()),
            "{path}: HEAD announces the GET length"
        );

        let mut conditional = page(path);
        conditional
            .headers_mut()
            .insert(IF_NONE_MATCH, full.headers[ETAG].clone());
        let answer = send(&router, conditional).await;
        assert_eq!(answer.status, StatusCode::NOT_MODIFIED, "{path}");
        assert!(answer.body.is_empty(), "{path}");
        assert!(answer.headers.contains_key(CACHE_CONTROL), "{path}");
        assert!(!answer.headers.contains_key(CONTENT_ENCODING), "{path}");
        assert!(
            !answer.headers.contains_key(CONTENT_LENGTH),
            "{path}: a 304 carries no Content-Length"
        );
        assert_eq!(answer.header("x-frame-options"), Some("DENY"), "{path}");
    }
    assert_eq!(
        send(&router, Request::head(SCRIPT).body(Body::empty()).unwrap())
            .await
            .header("content-length"),
        Some("24")
    );
}

#[tokio::test]
async fn extra_headers_and_redirects_pass_through_the_server() {
    let site = StaticSite::builder(Arc::new(Embedded::<Fixture>::new()))
        .header(X_FRAME_OPTIONS, HeaderValue::from_static("SAMEORIGIN"))
        .build()
        .unwrap();
    let router = router(Server::builder().prefix("/api").site(site)).await;
    let answer = send(&router, page("/docs")).await;
    assert_eq!(answer.status, StatusCode::PERMANENT_REDIRECT);
    assert_eq!(answer.headers[LOCATION], "/docs/");
    assert_eq!(answer.header("x-frame-options"), Some("SAMEORIGIN"));
}
