//! `HttpClient` against a local axum server.

mod support;

use std::collections::HashMap;
use std::error::Error as _;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, SystemTime};

use axum::Json;
use axum::Router;
use axum::extract::{Path, Query};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::Redirect;
use axum::routing::{any, get, post};
use sekvent_client::{BuildError, HttpClient, RedirectPolicy};
use sekvent_config::Secret;
use sekvent_context::{CallContext, ManualClock};
use sekvent_error::{AppError, ErrorCode};
use sekvent_resilience::{Backoff, Jitter, Policy, RetryPolicy};
use serde_json::{Value, json};
use support::serve;

fn plain_client(base: &str) -> HttpClient {
    HttpClient::builder()
        .base_url(base)
        .policy(Policy::new("test"))
        .build()
        .unwrap()
}

fn fast_retries() -> Policy {
    Policy::new("test").with_retry(RetryPolicy::new(
        3,
        Backoff::constant(Duration::from_millis(1)).with_jitter(Jitter::None),
    ))
}

fn header_text(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
}

#[tokio::test]
async fn get_json_with_query() {
    let router = Router::new().route(
        "/api/orders/{id}",
        get(
            |Path(id): Path<u32>, Query(query): Query<HashMap<String, String>>| async move {
                Json(json!({ "id": id, "expand": query.get("expand") }))
            },
        ),
    );
    let base = serve(router).await;
    let client = HttpClient::builder()
        .base_url(format!("{base}/api"))
        .build()
        .unwrap();
    let body: Value = client
        .get("/orders/7")
        .query(&[("expand", "line items")])
        .send_json(&CallContext::new())
        .await
        .unwrap();
    assert_eq!(body, json!({ "id": 7, "expand": "line items" }));
}

#[tokio::test]
async fn post_json_round_trip() {
    let router = Router::new().route(
        "/orders",
        post(|Json(order): Json<Value>| async move { (StatusCode::CREATED, Json(order)) }),
    );
    let base = serve(router).await;
    let response = plain_client(&base)
        .post("/orders")
        .json(&json!({ "sku": "A-1", "quantity": 2 }))
        .send(&CallContext::new())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    assert_eq!(response.json::<Value>().unwrap()["quantity"], 2);

    let deleted = plain_client(&base)
        .delete("/orders")
        .send(&CallContext::new())
        .await;
    assert_eq!(
        deleted.unwrap_err().code(),
        ErrorCode::Unimplemented,
        "405 from axum"
    );
}

#[tokio::test]
async fn statuses_map_to_codes() {
    let router = Router::new().route(
        "/status/{code}",
        any(|Path(code): Path<u16>| async move {
            (StatusCode::from_u16(code).unwrap(), "plain upstream text")
        }),
    );
    let base = serve(router).await;
    let client = plain_client(&base);
    let table = [
        (400, ErrorCode::InvalidArgument),
        (401, ErrorCode::Internal),
        (403, ErrorCode::Internal),
        (404, ErrorCode::NotFound),
        (408, ErrorCode::DeadlineExceeded),
        (409, ErrorCode::AlreadyExists),
        (412, ErrorCode::FailedPrecondition),
        (422, ErrorCode::InvalidArgument),
        (429, ErrorCode::ResourceExhausted),
        (500, ErrorCode::Internal),
        (501, ErrorCode::Unimplemented),
        (502, ErrorCode::Unavailable),
        (503, ErrorCode::Unavailable),
        (504, ErrorCode::DeadlineExceeded),
    ];
    for (status, code) in table {
        let error = client
            .get(&format!("/status/{status}"))
            .send(&CallContext::new())
            .await
            .unwrap_err();
        assert_eq!(error.code(), code, "HTTP {status}");
        assert_eq!(
            error.message(),
            format!("upstream responded with HTTP {status}")
        );
    }

    let sekvent = HttpClient::builder()
        .base_url(&base)
        .policy(Policy::new("test"))
        .sekvent_upstream(true)
        .build()
        .unwrap();
    for (status, code) in [
        (401, ErrorCode::Unauthenticated),
        (403, ErrorCode::PermissionDenied),
    ] {
        let error = sekvent
            .get(&format!("/status/{status}"))
            .send(&CallContext::new())
            .await
            .unwrap_err();
        assert_eq!(error.code(), code, "HTTP {status} from a sekvent upstream");
    }
}

#[tokio::test]
async fn a_conflict_is_not_retried() {
    let hits = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&hits);
    let router = Router::new().route(
        "/conflict",
        any(move || {
            counter.fetch_add(1, Ordering::SeqCst);
            async { StatusCode::CONFLICT }
        }),
    );
    let base = serve(router).await;
    let client = HttpClient::builder()
        .base_url(&base)
        .policy(fast_retries())
        .build()
        .unwrap();
    let error = client
        .put("/conflict")
        .send(&CallContext::new())
        .await
        .unwrap_err();
    assert_eq!(error.code(), ErrorCode::AlreadyExists);
    assert_eq!(hits.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn retry_after_is_read_from_seconds_and_dates() {
    let now = SystemTime::UNIX_EPOCH + Duration::from_hours(500_000);
    let date = httpdate::fmt_http_date(now + Duration::from_secs(120));
    let router = Router::new()
        .route(
            "/busy",
            get(|| async {
                (
                    StatusCode::TOO_MANY_REQUESTS,
                    [(header::RETRY_AFTER, "7")],
                    "",
                )
            }),
        )
        .route(
            "/later",
            get(move || {
                let date = date.clone();
                async move {
                    (
                        StatusCode::SERVICE_UNAVAILABLE,
                        [(header::RETRY_AFTER, date)],
                        "",
                    )
                }
            }),
        );
    let base = serve(router).await;
    let client = HttpClient::builder()
        .base_url(&base)
        .policy(Policy::new("test"))
        .clock(Arc::new(ManualClock::new(now)))
        .build()
        .unwrap();
    let busy = client
        .get("/busy")
        .send(&CallContext::new())
        .await
        .unwrap_err();
    assert_eq!(busy.code(), ErrorCode::ResourceExhausted);
    assert_eq!(busy.retry_after(), Some(Duration::from_secs(7)));
    let later = client
        .get("/later")
        .send(&CallContext::new())
        .await
        .unwrap_err();
    assert_eq!(later.code(), ErrorCode::Unavailable);
    assert_eq!(later.retry_after(), Some(Duration::from_secs(120)));
}

#[tokio::test]
async fn retries_503_for_get_but_not_post() {
    let hits = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&hits);
    let router = Router::new().route(
        "/flaky",
        any(move || {
            counter.fetch_add(1, Ordering::SeqCst);
            async { StatusCode::SERVICE_UNAVAILABLE }
        }),
    );
    let base = serve(router).await;
    let client = HttpClient::builder()
        .base_url(&base)
        .policy(fast_retries())
        .build()
        .unwrap();
    let ctx = CallContext::new();

    let error = client.get("/flaky").send(&ctx).await.unwrap_err();
    assert_eq!(error.code(), ErrorCode::Unavailable);
    assert_eq!(hits.swap(0, Ordering::SeqCst), 3, "GET is retried");

    client.post("/flaky").send(&ctx).await.unwrap_err();
    assert_eq!(hits.swap(0, Ordering::SeqCst), 1, "POST is not retried");

    client
        .post("/flaky")
        .idempotent(true)
        .send(&ctx)
        .await
        .unwrap_err();
    assert_eq!(
        hits.swap(0, Ordering::SeqCst),
        3,
        "a POST marked idempotent is retried"
    );
}

#[tokio::test]
async fn retry_recovers_from_a_transient_failure() {
    let hits = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&hits);
    let router = Router::new().route(
        "/recovering",
        get(move || {
            let n = counter.fetch_add(1, Ordering::SeqCst);
            async move {
                if n == 0 {
                    (StatusCode::BAD_GATEWAY, "down")
                } else {
                    (StatusCode::OK, "up")
                }
            }
        }),
    );
    let base = serve(router).await;
    let client = HttpClient::builder()
        .base_url(&base)
        .policy(fast_retries())
        .build()
        .unwrap();
    let response = client
        .get("/recovering")
        .send(&CallContext::new())
        .await
        .unwrap();
    assert_eq!(response.text().unwrap(), "up");
    assert_eq!(hits.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn context_is_propagated() {
    let router = Router::new().route(
        "/headers",
        get(|headers: HeaderMap| async move {
            Json(json!({
                "request_id": header_text(&headers, "x-request-id"),
                "grpc_timeout": header_text(&headers, "grpc-timeout"),
                "idempotency_key": header_text(&headers, "idempotency-key"),
            }))
        }),
    );
    let base = serve(router).await;
    let ctx = CallContext::new()
        .with_request_id("req-42")
        .with_idempotency_key("inbound-key")
        .with_timeout(Duration::from_secs(5));

    let seen: Value = plain_client(&base)
        .get("/headers")
        .send_json(&ctx)
        .await
        .unwrap();
    assert_eq!(seen["request_id"], "req-42");
    assert_eq!(seen["idempotency_key"], Value::Null);
    let timeout = sekvent_context::headers::parse_grpc_timeout(
        seen["grpc_timeout"].as_str().expect("grpc-timeout is sent"),
    )
    .unwrap();
    assert!(timeout <= Duration::from_secs(5));
    assert!(timeout > Duration::from_secs(1));

    let quiet = HttpClient::builder()
        .base_url(&base)
        .propagate_context(false)
        .build()
        .unwrap();
    let seen: Value = quiet.get("/headers").send_json(&ctx).await.unwrap();
    assert_eq!(
        seen,
        json!({ "request_id": null, "grpc_timeout": null, "idempotency_key": null })
    );
}

#[tokio::test]
async fn request_headers_win_over_the_context() {
    let router = Router::new().route(
        "/headers",
        post(|headers: HeaderMap| async move {
            Json(json!({
                "request_id": header_text(&headers, "x-request-id"),
                "idempotency_key": header_text(&headers, "idempotency-key"),
            }))
        }),
    );
    let base = serve(router).await;
    let ctx = CallContext::new()
        .with_request_id("req-42")
        .with_idempotency_key("inbound-key");
    let client = plain_client(&base);

    let seen: Value = client.post("/headers").send_json(&ctx).await.unwrap();
    assert_eq!(seen["request_id"], "req-42");
    assert_eq!(
        seen["idempotency_key"],
        Value::Null,
        "the inbound key is not reused upstream"
    );

    let seen: Value = client
        .post("/headers")
        .header("x-request-id", "caller-chosen")
        .header("idempotency-key", "outbound-key")
        .send_json(&ctx)
        .await
        .unwrap();
    assert_eq!(seen["request_id"], "caller-chosen");
    assert_eq!(seen["idempotency_key"], "outbound-key");
}

#[tokio::test]
async fn redirects_never_leave_the_origin() {
    let stolen = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&stolen);
    let elsewhere = serve(Router::new().route(
        "/steal",
        get(move || {
            counter.fetch_add(1, Ordering::SeqCst);
            async { "gotcha" }
        }),
    ))
    .await;
    let hops = Arc::new(AtomicUsize::new(0));
    let hop_counter = Arc::clone(&hops);
    let router = Router::new()
        .route("/hop", get(|| async { Redirect::to("/landing") }))
        .route(
            "/landing",
            get(|headers: HeaderMap| async move {
                header_text(&headers, "x-api-key").unwrap_or_default()
            }),
        )
        .route(
            "/away",
            get(move || {
                let target = format!("{elsewhere}/steal");
                async move { Redirect::to(&target) }
            }),
        )
        .route(
            "/loop/{n}",
            get(move |Path(n): Path<u32>| {
                hop_counter.fetch_add(1, Ordering::SeqCst);
                async move { Redirect::to(&format!("/loop/{}", n + 1)) }
            }),
        );
    let base = serve(router).await;
    let client = HttpClient::builder()
        .base_url(&base)
        .policy(Policy::new("test"))
        .default_header("x-api-key", "k-123")
        .build()
        .unwrap();
    let ctx = CallContext::new();

    let landed = client.get("/hop").send(&ctx).await.unwrap();
    assert_eq!(landed.status(), StatusCode::OK);
    assert_eq!(landed.text().unwrap(), "k-123");

    let stopped = client.get("/away").send(&ctx).await.unwrap();
    assert_eq!(stopped.status(), StatusCode::SEE_OTHER);
    assert_eq!(
        stolen.load(Ordering::SeqCst),
        0,
        "the other origin never saw the key"
    );

    let looping = client.get("/loop/0").send(&ctx).await.unwrap();
    assert_eq!(looping.status(), StatusCode::SEE_OTHER);
    assert_eq!(
        hops.load(Ordering::SeqCst),
        6,
        "the first request and five redirects"
    );

    let manual = HttpClient::builder()
        .base_url(&base)
        .redirects(RedirectPolicy::None)
        .build()
        .unwrap();
    let not_followed = manual.get("/hop").send(&ctx).await.unwrap();
    assert_eq!(not_followed.status(), StatusCode::SEE_OTHER);
}

#[tokio::test]
async fn sekvent_json_errors_decode_losslessly() {
    let router = Router::new().route(
        "/orders/7",
        get(|| async {
            Err::<(), _>(
                AppError::not_found("order 7 does not exist")
                    .with_reason("ORDER_NOT_FOUND")
                    .with_domain("orders")
                    .with_metadata("order_id", "7")
                    .with_field_violation("id", "unknown"),
            )
        }),
    );
    let base = serve(router).await;
    let foreign = plain_client(&base)
        .get("/orders/7")
        .send(&CallContext::new())
        .await
        .unwrap_err();
    assert_eq!(foreign.code(), ErrorCode::NotFound);
    assert_eq!(
        foreign.message(),
        "upstream responded with HTTP 404",
        "an upstream not declared as sekvent is not trusted"
    );
    assert_eq!(foreign.reason(), Some("UPSTREAM_HTTP_ERROR"));

    let error = HttpClient::builder()
        .base_url(&base)
        .policy(Policy::new("test"))
        .sekvent_upstream(true)
        .build()
        .unwrap()
        .get("/orders/7")
        .send(&CallContext::new())
        .await
        .unwrap_err();
    assert_eq!(error.code(), ErrorCode::NotFound);
    assert_eq!(error.message(), "order 7 does not exist");
    assert_eq!(error.reason(), Some("ORDER_NOT_FOUND"));
    assert_eq!(error.domain(), Some("orders"));
    assert_eq!(
        error.metadata().get("order_id").map(String::as_str),
        Some("7")
    );
    assert_eq!(error.field_violations().len(), 1);
}

#[tokio::test]
async fn upstream_bodies_never_reach_the_message() {
    let body = format!("SECRET-{}", "x".repeat(3000));
    let served = body.clone();
    let router = Router::new().route(
        "/boom",
        get(move || {
            let served = served.clone();
            async move { (StatusCode::INTERNAL_SERVER_ERROR, served) }
        }),
    );
    let base = serve(router).await;
    let error = plain_client(&base)
        .get("/boom")
        .send(&CallContext::new())
        .await
        .unwrap_err();
    assert_eq!(error.code(), ErrorCode::Internal);
    assert!(!error.message().contains("SECRET"));
    assert!(!error.to_string().contains("SECRET"));
    assert!(!format!("{error:?}").contains("SECRET"));
    assert!(!format!("{:?}", error.to_wire()).contains("SECRET"));
    let source = error
        .source()
        .expect("the response shape is kept internally")
        .to_string();
    assert!(!source.contains("SECRET"));
    assert!(source.contains(&format!("{}-byte body", body.len())));
}

#[tokio::test]
async fn transport_failures_are_transient() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    drop(listener);
    let refused = plain_client(&format!("http://{address}"))
        .get("/")
        .send(&CallContext::new())
        .await
        .unwrap_err();
    assert_eq!(refused.code(), ErrorCode::Unavailable);
    assert!(refused.is_transient());

    let router = Router::new().route("/slow", get(std::future::pending::<&'static str>));
    let base = serve(router).await;
    let ctx = CallContext::new().with_timeout(Duration::from_millis(100));
    let timed_out = plain_client(&base)
        .get("/slow")
        .send(&ctx)
        .await
        .unwrap_err();
    assert_eq!(timed_out.code(), ErrorCode::DeadlineExceeded);
}

#[tokio::test]
async fn credentials_and_default_headers_are_sent() {
    let router = Router::new().route(
        "/whoami",
        get(|headers: HeaderMap| async move {
            Json(json!({
                "authorization": header_text(&headers, "authorization"),
                "user_agent": header_text(&headers, "user-agent"),
                "app": header_text(&headers, "x-app"),
            }))
        }),
    );
    let base = serve(router).await;
    let ctx = CallContext::new();

    let basic = HttpClient::builder()
        .base_url(&base)
        .basic_auth("svc", Secret::new("pw"))
        .user_agent("orders-service/1")
        .default_header("x-app", "orders")
        .build()
        .unwrap();
    let seen: Value = basic.get("/whoami").send_json(&ctx).await.unwrap();
    assert_eq!(seen["authorization"], "Basic c3ZjOnB3");
    assert_eq!(seen["user_agent"], "orders-service/1");
    assert_eq!(seen["app"], "orders");

    let bearer = HttpClient::builder()
        .base_url(&base)
        .bearer_token(Secret::new("tok"))
        .build()
        .unwrap();
    let seen: Value = bearer.get("/whoami").send_json(&ctx).await.unwrap();
    assert_eq!(seen["authorization"], "Bearer tok");
    assert!(
        seen["user_agent"]
            .as_str()
            .unwrap()
            .starts_with("sekvent-client/")
    );
}

#[tokio::test]
async fn request_level_policy_and_invalid_json() {
    let hits = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&hits);
    let router = Router::new()
        .route(
            "/flaky",
            get(move || {
                counter.fetch_add(1, Ordering::SeqCst);
                async { StatusCode::SERVICE_UNAVAILABLE }
            }),
        )
        .route("/text", get(|| async { "not json" }));
    let base = serve(router).await;
    let client = plain_client(&base);
    client
        .get("/flaky")
        .policy(fast_retries())
        .send(&CallContext::new())
        .await
        .unwrap_err();
    assert_eq!(hits.load(Ordering::SeqCst), 3);

    let error = client
        .get("/text")
        .send_json::<Value>(&CallContext::new())
        .await
        .unwrap_err();
    assert_eq!(error.code(), ErrorCode::Internal);
}

#[test]
fn invalid_base_urls_are_rejected() {
    for bad in ["not a url", "ftp://files.example", "http://"] {
        let error = HttpClient::builder().base_url(bad).build().unwrap_err();
        assert!(matches!(error, BuildError::InvalidBaseUrl), "{bad}");
    }
}
