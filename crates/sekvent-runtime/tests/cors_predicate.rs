//! CORS origins decided by a function over a set that changes while the
//! server runs, on a real listener.

use std::collections::HashSet;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use axum::Router;
use axum::extract::ConnectInfo;
use axum::routing::get;
use bytes::Bytes;
use http::header::HOST;
use http::{HeaderMap, Method, Request, StatusCode};
use http_body_util::{BodyExt, Full};
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::TokioExecutor;
use sekvent_runtime::{Cors, Runtime, Server, Stage, UnitPolicy};

type HttpClient = Client<HttpConnector, Full<Bytes>>;

const ORIGIN: &str = "https://tenant.example.com";

/// What the origin function saw of a request head: its `Host` and whether
/// the TCP peer was attached.
type Seen = Arc<Mutex<Vec<(Option<String>, Option<SocketAddr>)>>>;

/// An answer's status, headers and body text.
struct Answer {
    status: StatusCode,
    headers: HeaderMap,
    body: String,
}

impl Answer {
    fn has_cors_headers(&self) -> bool {
        self.headers
            .keys()
            .any(|name| name.as_str().starts_with("access-control-"))
    }

    fn allow_origin(&self) -> Option<&str> {
        self.headers
            .get("access-control-allow-origin")
            .and_then(|value| value.to_str().ok())
    }

    fn varies_on_origin(&self) -> bool {
        self.headers
            .get_all("vary")
            .iter()
            .filter_map(|value| value.to_str().ok())
            .any(|value| value.eq_ignore_ascii_case("origin"))
    }
}

async fn send(http: &HttpClient, request: Request<Full<Bytes>>) -> Answer {
    let response = http.request(request).await.unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    Answer {
        status,
        headers,
        body: String::from_utf8_lossy(&body).into_owned(),
    }
}

fn plain(url: &str) -> Request<Full<Bytes>> {
    Request::get(url).body(Full::new(Bytes::new())).unwrap()
}

fn from_origin(url: &str) -> Request<Full<Bytes>> {
    Request::get(url)
        .header("origin", ORIGIN)
        .body(Full::new(Bytes::new()))
        .unwrap()
}

fn preflight(url: &str) -> Request<Full<Bytes>> {
    Request::builder()
        .method(Method::OPTIONS)
        .uri(url)
        .header("origin", ORIGIN)
        .header("access-control-request-method", "POST")
        .header("access-control-request-headers", "content-type")
        .body(Full::new(Bytes::new()))
        .unwrap()
}

/// Requests without `Origin` are served undecorated, whatever the set holds.
async fn served_without_origin(http: &HttpClient, url: &str, calls: &AtomicUsize) {
    let before = calls.load(Ordering::SeqCst);
    let answer = send(http, plain(url)).await;
    assert_eq!(answer.status, StatusCode::OK);
    assert_eq!(answer.body, "echo");
    assert!(!answer.has_cors_headers(), "{:?}", answer.headers);
    assert_eq!(calls.load(Ordering::SeqCst), before + 1);
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn an_origin_added_to_the_set_is_allowed_without_a_restart() {
    let run = async {
        let allowed: Arc<RwLock<HashSet<String>>> = Arc::default();
        let seen: Seen = Arc::default();
        let calls = Arc::new(AtomicUsize::new(0));

        let snapshot = Arc::clone(&allowed);
        let record = Arc::clone(&seen);
        let cors = Cors::origin_predicate(move |origin, head| {
            let host = head
                .headers
                .get(HOST)
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned);
            let peer = head
                .extensions
                .get::<ConnectInfo<SocketAddr>>()
                .map(|ConnectInfo(peer)| *peer);
            record.lock().unwrap().push((host, peer));
            snapshot.read().unwrap().contains(origin)
        });
        let counter = Arc::clone(&calls);
        let rest = Router::new().route(
            "/echo",
            get(move || {
                counter.fetch_add(1, Ordering::SeqCst);
                async { "echo" }
            }),
        );
        let server = Server::builder()
            .prefix("/api")
            .rest(rest)
            .cors(cors)
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
        let http: HttpClient = Client::builder(TokioExecutor::new()).build_http();
        let url = format!("http://{addr}/api/echo");

        served_without_origin(&http, &url, &calls).await;

        // Not in the set: the preflight is answered without CORS headers and
        // the request still runs its handler, undecorated.
        let answer = send(&http, preflight(&url)).await;
        assert_eq!(answer.status, StatusCode::OK);
        assert!(!answer.has_cors_headers(), "{:?}", answer.headers);
        assert!(answer.varies_on_origin(), "{:?}", answer.headers);
        assert_eq!(calls.load(Ordering::SeqCst), 1, "the layer answered");

        let answer = send(&http, from_origin(&url)).await;
        assert_eq!(answer.status, StatusCode::OK);
        assert_eq!(answer.body, "echo");
        assert!(!answer.has_cors_headers(), "{:?}", answer.headers);
        assert!(answer.varies_on_origin(), "{:?}", answer.headers);
        assert_eq!(calls.load(Ordering::SeqCst), 2);

        allowed.write().unwrap().insert(ORIGIN.to_owned());

        let answer = send(&http, preflight(&url)).await;
        assert_eq!(answer.status, StatusCode::OK);
        assert_eq!(answer.allow_origin(), Some(ORIGIN));
        assert!(answer.headers.contains_key("access-control-allow-methods"));
        assert!(answer.headers.contains_key("access-control-max-age"));
        assert!(answer.varies_on_origin(), "{:?}", answer.headers);
        assert_eq!(calls.load(Ordering::SeqCst), 2, "the layer answered");

        let answer = send(&http, from_origin(&url)).await;
        assert_eq!(answer.status, StatusCode::OK);
        assert_eq!(answer.body, "echo");
        assert_eq!(answer.allow_origin(), Some(ORIGIN));
        assert!(answer.varies_on_origin(), "{:?}", answer.headers);
        assert_eq!(calls.load(Ordering::SeqCst), 3);

        served_without_origin(&http, &url, &calls).await;

        // Two calls each before and after the insert; the requests without
        // `Origin` never reached the function. Each head had the `Host` the
        // client sent and the TCP peer.
        let seen = seen.lock().unwrap().clone();
        assert_eq!(seen.len(), 4, "{seen:?}");
        for (host, peer) in seen {
            assert_eq!(host.as_deref(), Some(addr.to_string().as_str()));
            let peer = peer.expect("the TCP peer is attached");
            assert!(peer.ip().is_loopback(), "{peer}");
        }

        drop(http);
        handle.shutdown();
        handle.wait().await.unwrap();
    };
    tokio::time::timeout(Duration::from_secs(30), run)
        .await
        .expect("the exchange finished within 30 s");
}
