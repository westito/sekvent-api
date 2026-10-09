//! The Bitbucket provider end to end against a fake Bitbucket on loopback.
//! Real clock (sockets), every wait guarded.

mod support;

use std::net::SocketAddr;
use std::sync::atomic::{AtomicU16, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::Router;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use sekvent_config::MapSource;
use sekvent_context::CallContext;
use sekvent_error::AppError;
use sekvent_runtime::{HealthRegistry, Server};
use sekvent_sso::{BitbucketProvider, Sso, SsoIdentity};
use serde_json::{Value, json};
use support::{APP, Answer, get as browse, guard, state_key};

const CALLBACK: &str = "https://app.example.com/api/sso/bitbucket/callback";

/// The fake's knobs and what it saw.
struct Fake {
    token_status: AtomicU16,
    token_hangs: AtomicU16,
    member_status: AtomicU16,
    emails: Mutex<Value>,
    token_forms: Mutex<Vec<Vec<(String, String)>>>,
    token_hits: AtomicUsize,
}

impl Fake {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            token_status: AtomicU16::new(200),
            token_hangs: AtomicU16::new(0),
            member_status: AtomicU16::new(200),
            emails: Mutex::new(json!({
                "pagelen": 100,
                "values": [
                    {"type": "email", "email": "old@example.com", "is_primary": false, "is_confirmed": true},
                    {"type": "email", "email": "ada@example.com", "is_primary": true, "is_confirmed": true}
                ]
            })),
            token_forms: Mutex::new(Vec::new()),
            token_hits: AtomicUsize::new(0),
        })
    }
}

fn bearer_ok(headers: &HeaderMap) -> bool {
    headers.get("authorization").and_then(|v| v.to_str().ok()) == Some("Bearer at-1")
}

async fn token(State(fake): State<Arc<Fake>>, headers: HeaderMap, body: String) -> Response {
    fake.token_hits.fetch_add(1, Ordering::SeqCst);
    if fake.token_hangs.load(Ordering::SeqCst) == 1 {
        std::future::pending::<()>().await;
    }
    let basic = format!("Basic {}", STANDARD.encode("consumer-key:consumer-secret"));
    assert_eq!(
        headers.get("authorization").and_then(|v| v.to_str().ok()),
        Some(basic.as_str())
    );
    let form: Vec<(String, String)> = form_urlencoded::parse(body.as_bytes())
        .into_owned()
        .collect();
    fake.token_forms.lock().unwrap().push(form);
    let status = StatusCode::from_u16(fake.token_status.load(Ordering::SeqCst)).unwrap();
    if status != StatusCode::OK {
        return (
            status,
            axum::Json(
                json!({"error": "invalid_grant", "error_description": "upstream secret text"}),
            ),
        )
            .into_response();
    }
    axum::Json(json!({
        "access_token": "at-1",
        "token_type": "bearer",
        "expires_in": 7200,
        "refresh_token": "rt-1",
        "scopes": "account email"
    }))
    .into_response()
}

async fn user(headers: HeaderMap) -> Response {
    if !bearer_ok(&headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    axum::Json(json!({
        "type": "user",
        "uuid": "{11111111-2222-3333-4444-555555555555}",
        "account_id": "557058:abc",
        "display_name": "Ada Lovelace",
        "nickname": "ada"
    }))
    .into_response()
}

async fn permission(
    State(fake): State<Arc<Fake>>,
    Path(workspace): Path<String>,
    headers: HeaderMap,
) -> Response {
    assert_eq!(workspace, "acme");
    if !bearer_ok(&headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let status = StatusCode::from_u16(fake.member_status.load(Ordering::SeqCst)).unwrap();
    if status != StatusCode::OK {
        return (
            status,
            axum::Json(json!({"type": "error", "error": {"message": "no"}})),
        )
            .into_response();
    }
    axum::Json(json!({"type": "workspace_membership", "permission": "member"})).into_response()
}

async fn emails(State(fake): State<Arc<Fake>>, headers: HeaderMap) -> Response {
    if !bearer_ok(&headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    axum::Json(fake.emails.lock().unwrap().clone()).into_response()
}

/// Serve the fake on 127.0.0.1:0 and return its base URL.
async fn serve(fake: Arc<Fake>) -> String {
    let router = Router::new()
        .route("/site/oauth2/access_token", post(token))
        .route("/2.0/user", get(user))
        .route(
            "/2.0/user/workspaces/{workspace}/permission",
            get(permission),
        )
        .route("/2.0/user/emails", get(emails))
        .with_state(fake);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    format!("http://{address}")
}

/// The provider pointed at `base`, with a short timeout.
fn provider(base: &str, timeout: &str) -> BitbucketProvider {
    let source = MapSource::new()
        .with("SSO_BITBUCKET_CLIENT_ID", "consumer-key")
        .with("SSO_BITBUCKET_CLIENT_SECRET", "consumer-secret")
        .with("SSO_BITBUCKET_CALLBACK_URL", CALLBACK)
        .with("SSO_BITBUCKET_WORKSPACE", "acme")
        .with(
            "SSO_BITBUCKET_AUTHORIZE_URL",
            format!("{base}/site/oauth2/authorize"),
        )
        .with(
            "SSO_BITBUCKET_TOKEN_URL",
            format!("{base}/site/oauth2/access_token"),
        )
        .with("SSO_BITBUCKET_API_URL", format!("{base}/2.0"))
        .with("SSO_BITBUCKET_TIMEOUT", timeout);
    BitbucketProvider::from_config(&source, "SSO_BITBUCKET_").unwrap()
}

struct Setup {
    sso: Sso<String>,
    router: Router,
    identities: Arc<Mutex<Vec<SsoIdentity>>>,
}

fn setup(provider: BitbucketProvider) -> Setup {
    let identities = Arc::new(Mutex::new(Vec::new()));
    let seen = Arc::clone(&identities);
    let sso = Sso::builder(APP, state_key())
        .provider(provider)
        .on_login(move |_: CallContext, identity: SsoIdentity| {
            let seen = Arc::clone(&seen);
            async move {
                let subject = identity.subject.clone();
                seen.lock().unwrap().push(identity);
                Ok::<_, AppError>(format!("user:{subject}"))
            }
        })
        .build()
        .unwrap();
    Setup {
        router: sso.router(),
        sso,
        identities,
    }
}

/// Log in, then come back from "Bitbucket" with `code`.
async fn sign_in(setup: &Setup, redirect: &str, code: &str) -> Answer {
    let login = browse(
        &setup.router,
        &format!("/sso/bitbucket/login?redirect={redirect}"),
        None,
    )
    .await;
    assert_eq!(login.status, StatusCode::FOUND);
    let state = login.param("state").unwrap();
    browse(
        &setup.router,
        &format!("/sso/bitbucket/callback?code={code}&state={state}"),
        Some(&login.cookie()),
    )
    .await
}

#[tokio::test]
async fn a_workspace_member_signs_in_end_to_end() {
    guard(async {
        let fake = Fake::new();
        let base = serve(Arc::clone(&fake)).await;
        let setup = setup(provider(&base, "5s"));

        let login = browse(&setup.router, "/sso/bitbucket/login?redirect=/orders", None).await;
        assert!(
            login
                .location
                .starts_with(&format!("{base}/site/oauth2/authorize?"))
        );
        assert_eq!(login.param("client_id").as_deref(), Some("consumer-key"));
        assert_eq!(login.param("response_type").as_deref(), Some("code"));
        assert_eq!(login.param("redirect_uri").as_deref(), Some(CALLBACK));
        assert_eq!(login.param("code_challenge"), None);
        assert_eq!(login.param("scope"), None);
        let state = login.param("state").unwrap();

        let answer = browse(
            &setup.router,
            &format!("/sso/bitbucket/callback?code=auth-code-1&state={state}"),
            Some(&login.cookie()),
        )
        .await;
        assert_eq!(answer.target(), format!("{APP}/orders"));
        let (key, code) = answer.fragment();
        assert_eq!(key, "sso_code");
        assert!(!answer.location.contains("at-1") && !answer.location.contains("rt-1"));

        let forms = fake.token_forms.lock().unwrap().clone();
        assert_eq!(
            forms,
            [vec![
                ("grant_type".to_owned(), "authorization_code".to_owned()),
                ("code".to_owned(), "auth-code-1".to_owned()),
                ("redirect_uri".to_owned(), CALLBACK.to_owned()),
            ]]
        );

        let identity = setup.identities.lock().unwrap()[0].clone();
        assert_eq!(identity.provider, "bitbucket");
        assert_eq!(identity.subject, "{11111111-2222-3333-4444-555555555555}");
        assert_eq!(identity.email.as_deref(), Some("ada@example.com"));
        assert_eq!(identity.display_name.as_deref(), Some("Ada Lovelace"));
        assert_eq!(identity.username.as_deref(), Some("ada"));
        assert_eq!(identity.groups, ["acme"]);
        assert_eq!(identity.attributes["account_id"], "557058:abc");

        assert_eq!(
            setup.sso.redeem(&code).as_deref(),
            Some("user:{11111111-2222-3333-4444-555555555555}")
        );
        assert_eq!(setup.sso.redeem(&code), None);
    })
    .await;
}

#[tokio::test]
async fn an_unconfirmed_or_secondary_email_is_left_out() {
    guard(async {
        let fake = Fake::new();
        *fake.emails.lock().unwrap() = json!({"values": [
            {"email": "primary@example.com", "is_primary": true, "is_confirmed": false},
            {"email": "other@example.com", "is_primary": false, "is_confirmed": true}
        ]});
        let base = serve(Arc::clone(&fake)).await;
        let setup = setup(provider(&base, "5s"));
        let answer = sign_in(&setup, "/", "c").await;
        assert_eq!(answer.fragment().0, "sso_code");
        assert_eq!(setup.identities.lock().unwrap()[0].email, None);
    })
    .await;
}

#[tokio::test]
async fn a_non_member_is_denied() {
    guard(async {
        for status in [403, 404] {
            let fake = Fake::new();
            fake.member_status.store(status, Ordering::SeqCst);
            let base = serve(Arc::clone(&fake)).await;
            let setup = setup(provider(&base, "5s"));
            let answer = sign_in(&setup, "/orders", "c").await;
            assert_eq!(answer.error_to("/orders"), "access_denied", "{status}");
            assert!(setup.identities.lock().unwrap().is_empty());
            assert!(setup.sso.handoff().is_empty());
        }
    })
    .await;
}

#[tokio::test]
async fn a_failed_membership_check_is_a_server_error() {
    guard(async {
        let fake = Fake::new();
        fake.member_status.store(500, Ordering::SeqCst);
        let base = serve(Arc::clone(&fake)).await;
        let setup = setup(provider(&base, "5s"));
        let answer = sign_in(&setup, "/orders", "c").await;
        assert_eq!(answer.error_to("/orders"), "server_error");
    })
    .await;
}

#[tokio::test]
async fn a_failed_token_exchange_carries_no_upstream_text() {
    guard(async {
        let fake = Fake::new();
        fake.token_status.store(400, Ordering::SeqCst);
        let base = serve(Arc::clone(&fake)).await;
        let setup = setup(provider(&base, "5s"));
        let answer = sign_in(&setup, "/orders", "c").await;
        assert_eq!(answer.error_to("/orders"), "server_error");
        assert!(!answer.location.contains("upstream"));
        assert!(setup.identities.lock().unwrap().is_empty());
    })
    .await;
}

#[tokio::test]
async fn a_hanging_token_endpoint_times_out() {
    guard(async {
        let fake = Fake::new();
        fake.token_hangs.store(1, Ordering::SeqCst);
        let base = serve(Arc::clone(&fake)).await;
        let setup = setup(provider(&base, "300ms"));
        let started = Instant::now();
        let answer = sign_in(&setup, "/orders", "c").await;
        assert!(started.elapsed() >= Duration::from_millis(300));
        assert_eq!(answer.error_to("/orders"), "temporarily_unavailable");
        assert_eq!(
            fake.token_hits.load(Ordering::SeqCst),
            1,
            "a POST is never retried"
        );
    })
    .await;
}

#[tokio::test]
async fn an_unreachable_bitbucket_is_temporarily_unavailable() {
    guard(async {
        let setup = setup(provider("http://127.0.0.1:1", "5s"));
        let answer = sign_in(&setup, "/orders", "c").await;
        assert_eq!(answer.error_to("/orders"), "temporarily_unavailable");
    })
    .await;
}

#[tokio::test]
async fn the_routes_work_under_the_server_prefix() {
    guard(async {
        let fake = Fake::new();
        let base = serve(Arc::clone(&fake)).await;
        let setup = setup(provider(&base, "5s"));
        let server = Server::builder()
            .prefix("/api")
            .rest(setup.router.clone())
            .bind(SocketAddr::from(([127, 0, 0, 1], 0)))
            .await
            .unwrap();
        let router = server.router(&HealthRegistry::new());
        let login = browse(&router, "/api/sso/bitbucket/login?redirect=/x", None).await;
        assert_eq!(login.status, StatusCode::FOUND);
        let state = login.param("state").unwrap();
        let answer = browse(
            &router,
            &format!("/api/sso/bitbucket/callback?code=c&state={state}"),
            Some(&login.cookie()),
        )
        .await;
        assert_eq!(answer.target(), format!("{APP}/x"));
        let (_, code) = answer.fragment();
        assert!(setup.sso.redeem(&code).is_some());
    })
    .await;
}
