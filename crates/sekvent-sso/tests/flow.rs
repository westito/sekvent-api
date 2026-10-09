//! The login and callback routes against an in-process fake provider, on an
//! injected clock.

mod support;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use futures::FutureExt as _;
use futures::future::BoxFuture;
use http::StatusCode;
use sekvent_config::{MapSource, Secret};
use sekvent_context::{CallContext, ManualClock};
use sekvent_error::{AppError, ErrorCode};
use sekvent_sso::{
    AuthorizationRequest, CodeExchange, IdentityProvider, ProviderTokens, Sso, SsoBuilder,
    SsoConfig, SsoErrorCode, SsoIdentity,
};
use sha2::{Digest, Sha256};
use support::{APP, get, state_key};

/// What the fake does at each step.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Fail {
    Nothing,
    AuthorizationUrl,
    BadLocation,
    Exchange,
    Identity,
}

/// A provider that issues `tokens-for-<code>` and checks PKCE.
struct Fake {
    id: &'static str,
    fail: Fail,
    challenge: Mutex<Option<String>>,
}

impl Fake {
    fn new(id: &'static str) -> Self {
        Self::failing(id, Fail::Nothing)
    }

    fn failing(id: &'static str, fail: Fail) -> Self {
        Self {
            id,
            fail,
            challenge: Mutex::new(None),
        }
    }
}

impl IdentityProvider for Fake {
    fn id(&self) -> &str {
        self.id
    }

    fn authorization_url(&self, request: &AuthorizationRequest<'_>) -> Result<String, AppError> {
        match self.fail {
            Fail::AuthorizationUrl => Err(AppError::internal("no url")),
            Fail::BadLocation => Ok("https://idp.example/\n".into()),
            _ => {
                *self.challenge.lock().unwrap() = Some(request.code_challenge.to_owned());
                Ok(format!(
                    "https://idp.example/authorize?state={}&code_challenge={}",
                    request.state, request.code_challenge
                ))
            }
        }
    }

    fn exchange_code<'a>(
        &'a self,
        _ctx: &'a CallContext,
        exchange: CodeExchange<'a>,
    ) -> BoxFuture<'a, Result<ProviderTokens, AppError>> {
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(exchange.code_verifier.as_bytes()));
        let expected = self.challenge.lock().unwrap().clone();
        let result = if self.fail == Fail::Exchange {
            Err(AppError::unavailable("idp down"))
        } else if expected.as_deref() != Some(challenge.as_str()) {
            Err(AppError::internal("PKCE verifier does not match"))
        } else {
            Ok(ProviderTokens::new(Secret::new(format!(
                "tokens-for-{}",
                exchange.code
            ))))
        };
        async move { result }.boxed()
    }

    fn identity<'a>(
        &'a self,
        _ctx: &'a CallContext,
        tokens: &'a ProviderTokens,
    ) -> BoxFuture<'a, Result<SsoIdentity, AppError>> {
        let result = if self.fail == Fail::Identity {
            Err(AppError::permission_denied("not in the group"))
        } else {
            let subject = tokens.access_token.expose().replace("tokens-for-", "user-");
            Ok(SsoIdentity::new(self.id, subject))
        };
        async move { result }.boxed()
    }
}

/// What the hook answers.
#[derive(Clone, Copy)]
enum HookAnswer {
    Admit,
    Deny,
    Fail,
}

struct Harness {
    sso: Sso<String>,
    router: axum::Router,
    clock: ManualClock,
    hook_calls: Arc<AtomicUsize>,
}

fn builder(answer: HookAnswer, calls: Arc<AtomicUsize>) -> SsoBuilder<String> {
    Sso::builder(APP, state_key()).on_login(move |ctx: CallContext, identity: SsoIdentity| {
        calls.fetch_add(1, Ordering::SeqCst);
        async move {
            assert!(
                ctx.deadline().is_some(),
                "the callback timeout narrows the context"
            );
            match answer {
                HookAnswer::Admit => Ok(format!("{}:{}", identity.provider, identity.subject)),
                HookAnswer::Deny => Err(AppError::permission_denied("not invited")),
                HookAnswer::Fail => Err(AppError::internal("database down")),
            }
        }
    })
}

fn harness_with(
    answer: HookAnswer,
    configure: impl FnOnce(SsoBuilder<String>) -> SsoBuilder<String>,
) -> Harness {
    let clock = ManualClock::new(SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000));
    let hook_calls = Arc::new(AtomicUsize::new(0));
    let builder = builder(answer, Arc::clone(&hook_calls)).clock(Arc::new(clock.clone()));
    let sso = configure(builder).build().unwrap();
    Harness {
        router: sso.router(),
        sso,
        clock,
        hook_calls,
    }
}

fn harness() -> Harness {
    harness_with(HookAnswer::Admit, |b| b.provider(Fake::new("fake")))
}

/// Log in and return `(state, cookie pair)`.
async fn start(h: &Harness, provider: &str, redirect: &str) -> (String, String) {
    let answer = get(
        &h.router,
        &format!("/sso/{provider}/login?redirect={}", urlencode(redirect)),
        None,
    )
    .await;
    assert_eq!(answer.status, StatusCode::FOUND, "{answer:?}");
    (answer.param("state").unwrap(), answer.cookie())
}

fn urlencode(value: &str) -> String {
    form_urlencoded::byte_serialize(value.as_bytes()).collect()
}

#[tokio::test]
async fn login_sets_a_signed_cookie_and_redirects_to_the_provider() {
    let h = harness();
    let answer = get(
        &h.router,
        "/sso/fake/login?redirect=%2Forders%3Ftab%3D1",
        None,
    )
    .await;
    assert_eq!(answer.status, StatusCode::FOUND);
    assert!(
        answer
            .location
            .starts_with("https://idp.example/authorize?state=")
    );
    assert_eq!(answer.param("state").unwrap().len(), 43);
    assert_eq!(answer.param("code_challenge").unwrap().len(), 43);
    let set_cookie = answer.set_cookie.as_deref().unwrap();
    assert!(
        set_cookie.starts_with("__Host-sekvent-sso=v1."),
        "{set_cookie}"
    );
    assert!(set_cookie.ends_with("; Path=/; Max-Age=600; HttpOnly; SameSite=Lax; Secure"));
    assert_eq!(answer.cache_control.as_deref(), Some("no-store"));
    assert_eq!(answer.referrer_policy.as_deref(), Some("no-referrer"));
}

#[tokio::test]
async fn a_full_sign_in_hands_off_once() {
    let h = harness();
    let (state, cookie) = start(&h, "fake", "/orders?tab=1").await;
    let answer = get(
        &h.router,
        &format!("/sso/fake/callback?code=c-1&state={state}"),
        Some(&cookie),
    )
    .await;
    assert_eq!(answer.status, StatusCode::FOUND, "{answer:?}");
    assert_eq!(answer.target(), format!("{APP}/orders?tab=1"));
    let (key, code) = answer.fragment();
    assert_eq!(key, "sso_code");
    assert!(!answer.location.contains("tokens-for"));
    assert_eq!(
        answer.set_cookie.as_deref(),
        Some("__Host-sekvent-sso=; Path=/; Max-Age=0; HttpOnly; SameSite=Lax; Secure")
    );
    assert_eq!(h.sso.handoff().len(), 1);
    assert_eq!(h.sso.redeem(&code).as_deref(), Some("fake:user-c-1"));
    assert_eq!(h.sso.redeem(&code), None);
    assert_eq!(h.hook_calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn the_default_redirect_is_used_without_one() {
    let h = harness_with(HookAnswer::Admit, |b| {
        b.provider(Fake::new("fake")).default_redirect("/home")
    });
    let login = get(&h.router, "/sso/fake/login", None).await;
    let state = login.param("state").unwrap();
    let answer = get(
        &h.router,
        &format!("/sso/fake/callback?code=c&state={state}"),
        Some(&login.cookie()),
    )
    .await;
    assert_eq!(answer.target(), format!("{APP}/home"));
}

#[tokio::test]
async fn a_handoff_code_expires() {
    let h = harness();
    let (state, cookie) = start(&h, "fake", "/").await;
    let answer = get(
        &h.router,
        &format!("/sso/fake/callback?code=c&state={state}"),
        Some(&cookie),
    )
    .await;
    let (_, code) = answer.fragment();
    h.clock.advance(Duration::from_secs(60));
    assert_eq!(h.sso.redeem(&code), None);
}

#[tokio::test]
async fn open_redirects_are_refused_at_login() {
    let h = harness();
    for redirect in [
        "//evil.example",
        "https://evil.example/",
        "/\\evil.example",
        "\\\\evil.example",
        "/\t/evil.example",
        "evil",
        "/x#y",
    ] {
        let answer = get(
            &h.router,
            &format!("/sso/fake/login?redirect={}", urlencode(redirect)),
            None,
        )
        .await;
        assert_eq!(answer.error_to("/"), "invalid_request", "{redirect:?}");
        assert_eq!(answer.set_cookie, None);
    }
    let repeated = get(&h.router, "/sso/fake/login?redirect=/a&redirect=/b", None).await;
    assert_eq!(repeated.error_to("/"), "invalid_request");
}

#[tokio::test]
async fn a_missing_cookie_is_invalid_state() {
    let h = harness();
    let (state, _) = start(&h, "fake", "/orders").await;
    let answer = get(
        &h.router,
        &format!("/sso/fake/callback?code=c&state={state}"),
        None,
    )
    .await;
    assert_eq!(answer.error_to("/"), "invalid_state");
    assert!(answer.set_cookie.as_deref().unwrap().contains("Max-Age=0"));
    assert_eq!(h.hook_calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn a_tampered_cookie_is_invalid_state() {
    let h = harness();
    let (state, cookie) = start(&h, "fake", "/orders").await;
    let (name, sealed) = cookie.split_once('=').unwrap();
    let mut bytes = sealed.as_bytes().to_vec();
    let middle = bytes.len() / 3;
    bytes[middle] = if bytes[middle] == b'A' { b'B' } else { b'A' };
    let forged = format!("{name}={}", String::from_utf8(bytes).unwrap());
    let answer = get(
        &h.router,
        &format!("/sso/fake/callback?code=c&state={state}"),
        Some(&forged),
    )
    .await;
    assert_eq!(answer.error_to("/"), "invalid_state");

    let garbage = get(
        &h.router,
        &format!("/sso/fake/callback?code=c&state={state}"),
        Some("__Host-sekvent-sso=garbage"),
    )
    .await;
    assert_eq!(garbage.error_to("/"), "invalid_state");
}

#[tokio::test]
async fn an_expired_cookie_is_invalid_state() {
    let h = harness();
    let (state, cookie) = start(&h, "fake", "/orders").await;
    h.clock.advance(Duration::from_secs(600));
    let answer = get(
        &h.router,
        &format!("/sso/fake/callback?code=c&state={state}"),
        Some(&cookie),
    )
    .await;
    assert_eq!(answer.error_to("/"), "invalid_state");
}

#[tokio::test]
async fn a_state_mismatch_is_invalid_state() {
    let h = harness();
    let (_, cookie) = start(&h, "fake", "/orders").await;
    let (other_state, _) = start(&h, "fake", "/orders").await;
    for query in [
        format!("code=c&state={other_state}"),
        "code=c".to_owned(),
        format!("code=c&state={other_state}&state={other_state}"),
    ] {
        let answer = get(
            &h.router,
            &format!("/sso/fake/callback?{query}"),
            Some(&cookie),
        )
        .await;
        assert_eq!(answer.error_to("/"), "invalid_state", "{query}");
    }
}

#[tokio::test]
async fn a_callback_cannot_be_replayed() {
    let h = harness();
    let (state, cookie) = start(&h, "fake", "/orders").await;
    let uri = format!("/sso/fake/callback?code=c&state={state}");
    let first = get(&h.router, &uri, Some(&cookie)).await;
    assert_eq!(first.fragment().0, "sso_code");
    let second = get(&h.router, &uri, Some(&cookie)).await;
    assert_eq!(second.error_to("/"), "invalid_state");
    assert_eq!(h.hook_calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn a_cookie_for_another_provider_is_invalid_state() {
    let h = harness_with(HookAnswer::Admit, |b| {
        b.provider(Fake::new("one")).provider(Fake::new("two"))
    });
    let (state, cookie) = start(&h, "one", "/orders").await;
    let answer = get(
        &h.router,
        &format!("/sso/two/callback?code=c&state={state}"),
        Some(&cookie),
    )
    .await;
    assert_eq!(answer.error_to("/"), "invalid_state");
}

#[tokio::test]
async fn a_valid_cookie_is_found_among_planted_ones() {
    let h = harness();
    let (state, cookie) = start(&h, "fake", "/orders").await;
    let both = format!("__Host-sekvent-sso=planted; theme=dark; {cookie}");
    let answer = get(
        &h.router,
        &format!("/sso/fake/callback?code=c&state={state}"),
        Some(&both),
    )
    .await;
    assert_eq!(answer.fragment().0, "sso_code");
}

#[tokio::test]
async fn provider_errors_become_constant_codes_at_the_return_path() {
    let h = harness();
    for (error, expected) in [
        ("access_denied", "access_denied"),
        ("temporarily_unavailable", "temporarily_unavailable"),
        ("server_error", "server_error"),
        ("Some%20upstream%20text%3Cscript%3E", "server_error"),
    ] {
        let (state, cookie) = start(&h, "fake", "/orders").await;
        let answer = get(
            &h.router,
            &format!("/sso/fake/callback?error={error}&error_description=secret&state={state}"),
            Some(&cookie),
        )
        .await;
        assert_eq!(answer.error_to("/orders"), expected);
        assert!(!answer.location.contains("secret"));
    }
    let (state, cookie) = start(&h, "fake", "/orders").await;
    let repeated = get(
        &h.router,
        &format!("/sso/fake/callback?error=a&error=b&state={state}"),
        Some(&cookie),
    )
    .await;
    assert_eq!(repeated.error_to("/orders"), "server_error");
    assert_eq!(h.hook_calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn a_callback_without_a_code_is_invalid_request() {
    let h = harness();
    let (state, cookie) = start(&h, "fake", "/orders").await;
    let answer = get(
        &h.router,
        &format!("/sso/fake/callback?state={state}"),
        Some(&cookie),
    )
    .await;
    assert_eq!(answer.error_to("/orders"), "invalid_request");
}

#[tokio::test]
async fn provider_failures_map_to_codes() {
    for (fail, expected) in [
        (Fail::Exchange, "temporarily_unavailable"),
        (Fail::Identity, "access_denied"),
    ] {
        let h = harness_with(HookAnswer::Admit, |b| {
            b.provider(Fake::failing("fake", fail))
        });
        let (state, cookie) = start(&h, "fake", "/orders").await;
        let answer = get(
            &h.router,
            &format!("/sso/fake/callback?code=c&state={state}"),
            Some(&cookie),
        )
        .await;
        assert_eq!(answer.error_to("/orders"), expected);
        assert_eq!(h.hook_calls.load(Ordering::SeqCst), 0);
    }
}

#[tokio::test]
async fn login_failures_redirect_with_server_error_or_fail_plainly() {
    let h = harness_with(HookAnswer::Admit, |b| {
        b.provider(Fake::failing("fake", Fail::AuthorizationUrl))
    });
    let answer = get(&h.router, "/sso/fake/login?redirect=/orders", None).await;
    assert_eq!(answer.error_to("/"), "server_error");
    assert_eq!(answer.set_cookie, None);

    let h = harness_with(HookAnswer::Admit, |b| {
        b.provider(Fake::failing("fake", Fail::BadLocation))
    });
    let answer = get(&h.router, "/sso/fake/login", None).await;
    assert_eq!(answer.status, StatusCode::INTERNAL_SERVER_ERROR);
}

#[tokio::test]
async fn the_hook_decides_admission() {
    for (answer, expected) in [
        (HookAnswer::Deny, "access_denied"),
        (HookAnswer::Fail, "server_error"),
    ] {
        let h = harness_with(answer, |b| b.provider(Fake::new("fake")));
        let (state, cookie) = start(&h, "fake", "/orders").await;
        let answer = get(
            &h.router,
            &format!("/sso/fake/callback?code=c&state={state}"),
            Some(&cookie),
        )
        .await;
        assert_eq!(answer.error_to("/orders"), expected);
        assert!(h.sso.handoff().is_empty());
    }
}

#[tokio::test]
async fn a_full_handoff_store_is_temporarily_unavailable() {
    let h = harness_with(HookAnswer::Admit, |b| {
        b.provider(Fake::new("fake")).handoff_capacity(1)
    });
    let mut answers = Vec::new();
    for _ in 0..2 {
        let (state, cookie) = start(&h, "fake", "/orders").await;
        answers.push(
            get(
                &h.router,
                &format!("/sso/fake/callback?code=c&state={state}"),
                Some(&cookie),
            )
            .await,
        );
    }
    assert_eq!(answers[0].fragment().0, "sso_code");
    assert_eq!(answers[1].error_to("/orders"), "temporarily_unavailable");
}

#[tokio::test]
async fn insecure_cookies_and_a_custom_path() {
    let h = harness_with(HookAnswer::Admit, |b| {
        b.provider(Fake::new("fake"))
            .secure_cookies(false)
            .path("/auth/v1")
            .state_ttl(Duration::from_secs(120))
    });
    let answer = get(&h.router, "/auth/v1/fake/login", None).await;
    let set_cookie = answer.set_cookie.unwrap();
    assert!(set_cookie.starts_with("sekvent-sso=v1."));
    assert!(set_cookie.ends_with("; Path=/; Max-Age=120; HttpOnly; SameSite=Lax"));
    assert_eq!(
        get(&h.router, "/sso/fake/login", None).await.status,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn unknown_providers_are_not_routed() {
    let h = harness();
    assert_eq!(
        get(&h.router, "/sso/other/login", None).await.status,
        StatusCode::NOT_FOUND
    );
}

fn build_error(configure: impl FnOnce(SsoBuilder<String>) -> SsoBuilder<String>) -> AppError {
    let builder = builder(HookAnswer::Admit, Arc::new(AtomicUsize::new(0)));
    configure(builder).build().unwrap_err()
}

type Configure = Box<dyn FnOnce(SsoBuilder<String>) -> SsoBuilder<String>>;

fn case(
    expected: &'static str,
    configure: impl FnOnce(SsoBuilder<String>) -> SsoBuilder<String> + 'static,
) -> (&'static str, Configure) {
    (expected, Box::new(configure))
}

#[test]
fn build_validates_every_setting() {
    let fake = || Fake::new("fake");
    let cases = [
        case("at least one identity provider", |b| b),
        case("invalid id", |b| b.provider(Fake::new("Bad"))),
        case("registered twice", move |b| {
            b.provider(fake()).provider(fake())
        }),
        case("state TTL", move |b| {
            b.provider(fake()).state_ttl(Duration::ZERO)
        }),
        case("state TTL", move |b| {
            b.provider(fake()).state_ttl(Duration::from_secs(601))
        }),
        case("state TTL", move |b| {
            b.provider(fake()).state_ttl(Duration::from_millis(1500))
        }),
        case("callback timeout", move |b| {
            b.provider(fake()).callback_timeout(Duration::ZERO)
        }),
        case("SSO path", move |b| b.provider(fake()).path("sso")),
        case("SSO path", move |b| b.provider(fake()).path("/sso/")),
        case("SSO path", move |b| b.provider(fake()).path("/s so")),
        case("default redirect", move |b| {
            b.provider(fake()).default_redirect("//x")
        }),
        case("handoff TTL", move |b| {
            b.provider(fake()).handoff_ttl(Duration::ZERO)
        }),
        case("handoff capacity", move |b| {
            b.provider(fake()).handoff_capacity(0)
        }),
    ];
    for (expected, configure) in cases {
        let error = build_error(configure);
        assert_eq!(error.code(), ErrorCode::InvalidArgument);
        assert!(
            error.message().contains(expected),
            "{expected}: {}",
            error.message()
        );
    }

    let error = Sso::<String>::builder("not a url", state_key())
        .build()
        .unwrap_err();
    assert!(error.message().contains("application URL"));
    let error = Sso::<String>::builder(APP, Secret::new("short"))
        .build()
        .unwrap_err();
    assert!(error.message().contains("32 bytes"));
    let error = Sso::<String>::builder(APP, state_key())
        .provider(Fake::new("fake"))
        .build()
        .unwrap_err();
    assert!(error.message().contains("login hook"));
}

#[test]
fn shared_providers_and_debug_output() {
    let shared: Arc<dyn IdentityProvider> = Arc::new(Fake::new("fake"));
    let builder = builder(HookAnswer::Admit, Arc::new(AtomicUsize::new(0))).provider_arc(shared);
    assert!(format!("{builder:?}").contains("fake"));
    let sso = builder.build().unwrap();
    let debug = format!("{:?}", sso.clone());
    assert!(debug.contains("fake") && debug.contains(APP));
    assert!(!debug.contains("0123456789abcdef"));
}

#[test]
fn config_defaults_and_from_config() {
    let source = MapSource::new()
        .with("APP_URL", APP)
        .with("STATE_KEY", "0123456789abcdef0123456789abcdef");
    let config = <SsoConfig as sekvent_config::FromConfig>::from_config(&source).unwrap();
    assert!(config.secure_cookies);
    assert_eq!(config.state_ttl, Duration::from_secs(600));
    assert_eq!(config.handoff_ttl, Duration::from_secs(60));
    assert_eq!(config.handoff_capacity, 10_000);
    assert_eq!(config.callback_timeout, Duration::from_secs(30));
    assert!(!format!("{config:?}").contains("0123456789abcdef"));
    let sso = Sso::<String>::from_config(config)
        .provider(Fake::new("fake"))
        .on_login(|_: CallContext, identity: SsoIdentity| async move {
            Ok::<_, AppError>(identity.subject)
        })
        .build()
        .unwrap();
    assert_eq!(sso.handoff().ttl(), Duration::from_secs(60));
}

#[test]
fn error_codes() {
    let all = [
        (SsoErrorCode::AccessDenied, "access_denied"),
        (SsoErrorCode::InvalidRequest, "invalid_request"),
        (SsoErrorCode::InvalidState, "invalid_state"),
        (SsoErrorCode::ServerError, "server_error"),
        (
            SsoErrorCode::TemporarilyUnavailable,
            "temporarily_unavailable",
        ),
    ];
    for (code, text) in all {
        assert_eq!(code.as_str(), text);
        assert_eq!(code.to_string(), text);
    }
    let for_code = |code| SsoErrorCode::for_error(&AppError::new(code, "x"));
    assert_eq!(
        for_code(ErrorCode::PermissionDenied),
        SsoErrorCode::AccessDenied
    );
    assert_eq!(
        for_code(ErrorCode::Unauthenticated),
        SsoErrorCode::AccessDenied
    );
    assert_eq!(
        for_code(ErrorCode::DeadlineExceeded),
        SsoErrorCode::TemporarilyUnavailable
    );
    assert_eq!(
        for_code(ErrorCode::InvalidArgument),
        SsoErrorCode::ServerError
    );
}
