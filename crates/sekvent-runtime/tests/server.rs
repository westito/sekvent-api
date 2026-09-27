//! One real listener on port 0 serving native gRPC, gRPC-Web, REST and
//! health, driven by the runtime, then shut down gracefully.

use std::net::SocketAddr;

use axum::Router;
use axum::routing::get;
use bytes::Bytes;
use http::request::Parts;
use http::{Request, StatusCode};
use http_body_util::{BodyExt, Empty, Full};
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;
use prost::Message;
use sekvent_context::ServiceIdentity;
use sekvent_runtime::{Ctx, Runtime, Server, Stage, UnitExit, UnitPolicy};
use tonic_health::pb::health_check_response::ServingStatus;
use tonic_health::pb::health_client::HealthClient;
use tonic_health::pb::{HealthCheckRequest, HealthCheckResponse};

fn whoami() -> Router {
    Router::new().route(
        "/whoami",
        get(|Ctx(ctx): Ctx| async move {
            format!("{}|{}", ctx.request_id(), ctx.subject().unwrap_or("-"))
        }),
    )
}

/// Split a gRPC-Web response body into its message and its trailer block.
fn grpc_web_frames(body: &[u8]) -> (Vec<u8>, String) {
    let mut message = Vec::new();
    let mut trailers = String::new();
    let mut rest = body;
    while rest.len() >= 5 {
        let flag = rest[0];
        let len =
            usize::try_from(u32::from_be_bytes([rest[1], rest[2], rest[3], rest[4]])).unwrap();
        let payload = &rest[5..5 + len];
        if flag & 0x80 == 0 {
            message.extend_from_slice(payload);
        } else {
            trailers.push_str(&String::from_utf8_lossy(payload));
        }
        rest = &rest[5 + len..];
    }
    (message, trailers)
}

type HttpClient = Client<hyper_util::client::legacy::connect::HttpConnector, Full<Bytes>>;

async fn text(response: http::Response<hyper::body::Incoming>) -> (StatusCode, String) {
    let status = response.status();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8(body.to_vec()).unwrap())
}

async fn get_path(
    http: &HttpClient,
    addr: SocketAddr,
    path: &str,
    link: bool,
) -> (StatusCode, String) {
    let mut request = Request::get(format!("http://{addr}{path}"))
        .header("x-request-id", "req-42")
        .header("x-sekvent-subject", "user-1");
    if link {
        request = request.header("x-test-link", "1");
    }
    let response = http
        .request(request.body(Full::new(Bytes::new())).unwrap())
        .await
        .unwrap();
    text(response).await
}

/// Native gRPC at the root, as a tonic client sends it.
async fn native_grpc_is_served(addr: SocketAddr) {
    let channel = tonic::transport::Endpoint::from_shared(format!("http://{addr}"))
        .unwrap()
        .connect()
        .await
        .unwrap();
    let response = HealthClient::new(channel)
        .check(HealthCheckRequest {
            service: String::new(),
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(response.status(), ServingStatus::Serving);
}

/// The same call over gRPC-Web (HTTP/1.1) under the prefix.
async fn grpc_web_is_served_under_the_prefix(http: &HttpClient, addr: SocketAddr) {
    let request = Request::post(format!("http://{addr}/api/grpc.health.v1.Health/Check"))
        .header("content-type", "application/grpc-web+proto")
        .header("x-grpc-web", "1")
        .body(Full::new(Bytes::from_static(&[0, 0, 0, 0, 0])))
        .unwrap();
    let response = http.request(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let content_type = response.headers()["content-type"]
        .to_str()
        .unwrap()
        .to_owned();
    assert!(
        content_type.starts_with("application/grpc-web"),
        "{content_type}"
    );
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let (message, trailers) = grpc_web_frames(&body);
    let decoded = HealthCheckResponse::decode(message.as_slice()).unwrap();
    assert_eq!(decoded.status(), ServingStatus::Serving);
    assert!(trailers.contains("grpc-status:0"), "{trailers}");
}

/// REST under the prefix with the call context; health at the root only.
async fn rest_and_health_are_routed(http: &HttpClient, addr: SocketAddr) {
    let trusted = get_path(http, addr, "/api/whoami", true).await;
    assert_eq!(trusted, (StatusCode::OK, "req-42|user-1".into()));
    let anonymous = get_path(http, addr, "/api/whoami", false).await;
    assert_eq!(anonymous, (StatusCode::OK, "req-42|-".into()));

    let (status, body) = get_path(http, addr, "/readyz", false).await;
    assert_eq!(
        (status, body.as_str()),
        (StatusCode::OK, r#"{"status":"ready"}"#)
    );
    let (status, _) = get_path(http, addr, "/api/readyz", false).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = get_path(http, addr, "/whoami", false).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn one_listener_serves_every_protocol_and_drains() {
    let server = Server::builder()
        .prefix("/api")
        .rest(whoami())
        .authenticator(|parts: &Parts| {
            parts
                .headers
                .contains_key("x-test-link")
                .then(|| ServiceIdentity::trusted("billing"))
        })
        .bind(SocketAddr::from(([127, 0, 0, 1], 0)))
        .await
        .unwrap();
    let addr = server.local_addr();
    let handle = Runtime::builder()
        .without_signals()
        .unit(
            "api",
            Stage::Ingress,
            UnitPolicy::Critical,
            server.into_unit(),
        )
        .build()
        .unwrap()
        .start()
        .await
        .unwrap();

    native_grpc_is_served(addr).await;
    let http: HttpClient = Client::builder(TokioExecutor::new()).build_http();
    grpc_web_is_served_under_the_prefix(&http, addr).await;
    rest_and_health_are_routed(&http, addr).await;
    drop(http);

    // Graceful shutdown releases the port.
    handle.shutdown();
    let report = handle.wait().await.unwrap();
    assert_eq!(report.unit("api").unwrap().exit, UnitExit::Completed);
    tokio::net::TcpListener::bind(addr)
        .await
        .expect("the listener was released");

    // Nothing answers any more.
    let closed = Client::builder(TokioExecutor::new())
        .build_http::<Empty<Bytes>>()
        .request(
            Request::get(format!("http://{addr}/livez"))
                .body(Empty::new())
                .unwrap(),
        )
        .await;
    assert!(closed.is_err());
}
