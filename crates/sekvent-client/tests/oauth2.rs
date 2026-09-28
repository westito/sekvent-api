//! Client-credentials token source and bearer handling against a local server.

mod support;

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use axum::Json;
use axum::Router;
use axum::extract::{Form, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Redirect};
use axum::routing::{get, post};
use sekvent_client::oauth2::{ClientAuthStyle, ClientCredentials};
use sekvent_client::{BearerSource, HttpClient};
use sekvent_config::Secret;
use sekvent_context::{CallContext, ManualClock};
use sekvent_error::ErrorCode;
use serde_json::json;
use support::serve;
use tokio::sync::{Notify, Semaphore};

/// The `Authorization` header and form body of one token request.
type SeenRequest = (Option<String>, HashMap<String, String>);

#[derive(Default)]
struct TokenServer {
    token_hits: AtomicUsize,
    api_hits: AtomicUsize,
    entered: Notify,
    gate: Option<Semaphore>,
    seen: Mutex<Vec<SeenRequest>>,
}

async fn issue(
    State(server): State<Arc<TokenServer>>,
    headers: HeaderMap,
    Form(form): Form<HashMap<String, String>>,
) -> impl IntoResponse {
    let n = server.token_hits.fetch_add(1, Ordering::SeqCst) + 1;
    let authorization = headers
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    server.seen.lock().unwrap().push((authorization, form));
    server.entered.notify_one();
    if let Some(gate) = &server.gate {
        gate.acquire().await.unwrap().forget();
    }
    Json(json!({ "access_token": format!("token-{n}"), "token_type": "Bearer", "expires_in": 60 }))
}

async fn secure(State(server): State<Arc<TokenServer>>, headers: HeaderMap) -> StatusCode {
    server.api_hits.fetch_add(1, Ordering::SeqCst);
    match headers
        .get("authorization")
        .and_then(|value| value.to_str().ok())
    {
        Some("Bearer token-2") => StatusCode::OK,
        _ => StatusCode::UNAUTHORIZED,
    }
}

async fn start(server: TokenServer) -> (Arc<TokenServer>, String) {
    let server = Arc::new(server);
    let router = Router::new()
        .route("/token", post(issue))
        .route("/secure", get(secure).post(secure))
        .route(
            "/rejecting",
            post(|| async {
                (
                    StatusCode::BAD_REQUEST,
                    Json(
                        json!({ "error": "invalid_client", "error_description": "SECRET-DETAIL" }),
                    ),
                )
            }),
        )
        .route(
            "/overloaded",
            post(|| async { StatusCode::SERVICE_UNAVAILABLE }),
        )
        .route("/garbage", post(|| async { "not json" }))
        .route(
            "/mac",
            post(|| async { Json(json!({ "access_token": "t", "token_type": "mac" })) }),
        )
        .route("/moved", post(|| async { Redirect::temporary("/token") }))
        .route(
            "/forever",
            post(|State(server): State<Arc<TokenServer>>| async move {
                let n = server.token_hits.fetch_add(1, Ordering::SeqCst) + 1;
                Json(json!({ "access_token": format!("forever-{n}"), "expires_in": u64::MAX }))
            }),
        )
        .with_state(Arc::clone(&server));
    let base = serve(router).await;
    (server, base)
}

fn start_time() -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_hours(500_000)
}

fn credentials(base: &str, path: &str, clock: &ManualClock) -> ClientCredentials {
    ClientCredentials::builder(format!("{base}{path}"), "svc", Secret::new("p@ss"))
        .scope("read write")
        .refresh_skew(Duration::from_secs(10))
        .clock(Arc::new(clock.clone()))
        .build()
        .unwrap()
}

#[tokio::test]
async fn tokens_are_cached_and_sent_with_basic_auth() {
    let (server, base) = start(TokenServer::default()).await;
    let clock = ManualClock::new(start_time());
    let creds = credentials(&base, "/token", &clock);
    let ctx = CallContext::new();
    assert_eq!(creds.token(&ctx).await.unwrap().expose(), "token-1");
    assert_eq!(creds.token(&ctx).await.unwrap().expose(), "token-1");
    assert_eq!(
        server.token_hits.load(Ordering::SeqCst),
        1,
        "the second call is a cache hit"
    );

    let seen = server.seen.lock().unwrap();
    let (authorization, form) = &seen[0];
    assert_eq!(authorization.as_deref(), Some("Basic c3ZjOnAlNDBzcw=="));
    assert_eq!(
        form.get("grant_type").map(String::as_str),
        Some("client_credentials")
    );
    assert_eq!(form.get("scope").map(String::as_str), Some("read write"));
    assert!(!form.contains_key("client_secret"));
}

#[tokio::test]
async fn tokens_refresh_before_expiry() {
    let (server, base) = start(TokenServer::default()).await;
    let clock = ManualClock::new(start_time());
    let creds = credentials(&base, "/token", &clock);
    let ctx = CallContext::new();
    assert_eq!(creds.token(&ctx).await.unwrap().expose(), "token-1");
    clock.advance(Duration::from_secs(49));
    assert_eq!(creds.token(&ctx).await.unwrap().expose(), "token-1");
    clock.advance(Duration::from_secs(1));
    assert_eq!(
        creds.token(&ctx).await.unwrap().expose(),
        "token-2",
        "60 s minus 10 s skew"
    );
    assert_eq!(server.token_hits.load(Ordering::SeqCst), 2);

    creds.invalidate();
    assert_eq!(creds.token(&ctx).await.unwrap().expose(), "token-3");
}

#[tokio::test]
async fn concurrent_callers_share_one_fetch() {
    let (server, base) = start(TokenServer {
        gate: Some(Semaphore::new(0)),
        ..TokenServer::default()
    })
    .await;
    let clock = ManualClock::new(start_time());
    let creds = Arc::new(credentials(&base, "/token", &clock));

    let callers: Vec<_> = (0..16)
        .map(|_| {
            let creds = Arc::clone(&creds);
            tokio::spawn(async move { creds.token(&CallContext::new()).await })
        })
        .collect();
    server.entered.notified().await;
    server.gate.as_ref().unwrap().add_permits(64);
    for caller in callers {
        assert_eq!(caller.await.unwrap().unwrap().expose(), "token-1");
    }
    assert_eq!(server.token_hits.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn request_body_style_sends_the_secret_in_the_form() {
    let (server, base) = start(TokenServer::default()).await;
    let creds = ClientCredentials::builder(format!("{base}/token"), "svc", Secret::new("p@ss"))
        .auth_style(ClientAuthStyle::RequestBody)
        .audience("https://orders.example")
        .build()
        .unwrap();
    creds.token(&CallContext::new()).await.unwrap();
    let seen = server.seen.lock().unwrap();
    let (authorization, form) = &seen[0];
    assert_eq!(authorization, &None);
    assert_eq!(form.get("client_id").map(String::as_str), Some("svc"));
    assert_eq!(form.get("client_secret").map(String::as_str), Some("p@ss"));
    assert_eq!(
        form.get("audience").map(String::as_str),
        Some("https://orders.example")
    );
}

#[tokio::test]
async fn a_401_invalidates_and_retries_once() {
    let (server, base) = start(TokenServer::default()).await;
    let clock = ManualClock::new(start_time());
    let creds: Arc<dyn BearerSource> = Arc::new(credentials(&base, "/token", &clock));
    let client = HttpClient::builder()
        .base_url(&base)
        .with_bearer_source(Arc::clone(&creds))
        .build()
        .unwrap();
    let ctx = CallContext::new();

    let response = client.get("/secure").send(&ctx).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        server.token_hits.load(Ordering::SeqCst),
        2,
        "token-1 rejected, token-2 fetched"
    );
    assert_eq!(server.api_hits.load(Ordering::SeqCst), 2);

    client.post("/secure").send(&ctx).await.unwrap();
    assert_eq!(
        server.api_hits.load(Ordering::SeqCst),
        3,
        "the cached token-2 is reused"
    );

    creds.invalidate();
    let error = client.post("/secure").send(&ctx).await.unwrap_err();
    assert_eq!(
        error.code(),
        ErrorCode::Internal,
        "only one re-authentication per request; our credentials are not the user's problem"
    );
    assert_eq!(server.token_hits.load(Ordering::SeqCst), 4);
    assert_eq!(server.api_hits.load(Ordering::SeqCst), 5);
}

#[tokio::test]
async fn token_endpoint_failures_are_mapped() {
    let (_server, base) = start(TokenServer::default()).await;
    let clock = ManualClock::new(start_time());
    let ctx = CallContext::new();

    let rejected = credentials(&base, "/rejecting", &clock)
        .token(&ctx)
        .await
        .unwrap_err();
    assert_eq!(rejected.code(), ErrorCode::Internal);
    assert_eq!(rejected.reason(), Some("TOKEN_REQUEST_FAILED"));
    assert!(!rejected.to_string().contains("SECRET-DETAIL"));
    assert!(!format!("{rejected:?}").contains("SECRET-DETAIL"));

    let overloaded = credentials(&base, "/overloaded", &clock)
        .token(&ctx)
        .await
        .unwrap_err();
    assert_eq!(overloaded.code(), ErrorCode::Unavailable);

    let garbage = credentials(&base, "/garbage", &clock)
        .token(&ctx)
        .await
        .unwrap_err();
    assert_eq!(garbage.code(), ErrorCode::Internal);

    let mac = credentials(&base, "/mac", &clock)
        .token(&ctx)
        .await
        .unwrap_err();
    assert_eq!(mac.code(), ErrorCode::Internal);
}

#[tokio::test]
async fn the_token_endpoint_is_never_redirected() {
    let (server, base) = start(TokenServer::default()).await;
    let clock = ManualClock::new(start_time());
    let error = credentials(&base, "/moved", &clock)
        .token(&CallContext::new())
        .await
        .unwrap_err();
    assert_eq!(error.code(), ErrorCode::Internal);
    assert_eq!(
        server.token_hits.load(Ordering::SeqCst),
        0,
        "the client secret was not replayed to the redirect target"
    );
}

#[tokio::test]
async fn absurd_lifetimes_are_clamped_to_a_day() {
    let (server, base) = start(TokenServer::default()).await;
    let clock = ManualClock::new(start_time());
    let creds = credentials(&base, "/forever", &clock);
    let ctx = CallContext::new();
    assert_eq!(creds.token(&ctx).await.unwrap().expose(), "forever-1");
    clock.advance(Duration::from_hours(23));
    assert_eq!(creds.token(&ctx).await.unwrap().expose(), "forever-1");
    clock.advance(Duration::from_hours(1));
    assert_eq!(creds.token(&ctx).await.unwrap().expose(), "forever-2");
    assert_eq!(server.token_hits.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn a_lifetime_shorter_than_the_skew_is_still_cached() {
    let (server, base) = start(TokenServer::default()).await;
    let clock = ManualClock::new(start_time());
    let creds = ClientCredentials::builder(format!("{base}/token"), "svc", Secret::new("p@ss"))
        .refresh_skew(Duration::from_secs(90))
        .clock(Arc::new(clock.clone()))
        .build()
        .unwrap();
    let ctx = CallContext::new();
    assert_eq!(creds.token(&ctx).await.unwrap().expose(), "token-1");
    clock.advance(Duration::from_secs(29));
    assert_eq!(
        creds.token(&ctx).await.unwrap().expose(),
        "token-1",
        "half of the 60 s lifetime"
    );
    clock.advance(Duration::from_secs(1));
    assert_eq!(creds.token(&ctx).await.unwrap().expose(), "token-2");
    assert_eq!(server.token_hits.load(Ordering::SeqCst), 2);
}
