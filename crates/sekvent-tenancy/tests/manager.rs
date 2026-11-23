//! The manager kit against a served admin App: `AdminClient` calls a component on
//! `127.0.0.1:0` whose App authenticates admin tokens, and each `CallOutcome` is observed.
#![cfg(all(feature = "manager", feature = "admin"))]

use std::fmt::Debug;
use std::future::Future;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use admin_api::{FailRequest, ProbeReply, ProbeRequest, TenantAdmin, TenantAdminHandle};
use sekvent_auth::EdDsaSigningKey;
use sekvent_component::{App, CallContext};
use sekvent_config::MapSource;
use sekvent_context::{Clock, SystemClock};
use sekvent_error::{AppError, ErrorCode, FieldViolation};
use sekvent_runtime::{Runtime, RuntimeHandle, Server, Stage, UnitPolicy};
use sekvent_tenancy::TenantId;
use sekvent_tenancy::admin::{self, AdminClaims, Verifier};
use sekvent_tenancy::manager::{AdminClient, CallOutcome, Minter};
use serde::{Deserialize, Serialize};
use tokio::io::AsyncReadExt as _;
use tokio::net::TcpListener;
use uuid::Uuid;

const ISSUER: &str = "ops-console";
const AUDIENCE: &str = "billing-eu";
const SERVICE: &str = "billing.admin.v1.TenantAdmin";
const PROBE: &str = "billing.admin.v1.TenantAdmin/Probe";
const FAIL: &str = "billing.admin.v1.TenantAdmin/Fail";
const DEADLINE: Duration = Duration::from_secs(10);

/// The admin service the test product declares in its own proto.
#[allow(dead_code, missing_docs, unreachable_pub)]
mod admin_api {
    use sekvent_component::{AppError, CallContext};

    /// The contract sekvent-proto-build would generate for `billing.admin.v1.TenantAdmin`.
    pub mod proto {
        #[allow(non_upper_case_globals, clippy::type_complexity)]
        pub const __sekvent_service_TenantAdmin: (&str, &[(&str, &str, &str, bool, bool)]) = (
            "billing.admin.v1.TenantAdmin",
            &[
                (
                    "Probe",
                    "billing.admin.v1.ProbeRequest",
                    "billing.admin.v1.ProbeReply",
                    false,
                    false,
                ),
                (
                    "Fail",
                    "billing.admin.v1.FailRequest",
                    "billing.admin.v1.ProbeReply",
                    false,
                    false,
                ),
            ],
        );
        #[allow(non_camel_case_types)]
        pub type __sekvent_rpc_TenantAdmin__Probe = (super::ProbeRequest, super::ProbeReply);
        #[allow(non_camel_case_types)]
        pub type __sekvent_rpc_TenantAdmin__Fail = (super::FailRequest, super::ProbeReply);
    }

    /// A text to echo.
    #[derive(Clone, PartialEq, prost::Message)]
    pub struct ProbeRequest {
        #[prost(string, tag = "1")]
        pub text: String,
    }

    impl prost::Name for ProbeRequest {
        const NAME: &'static str = "ProbeRequest";
        const PACKAGE: &'static str = "billing.admin.v1";
    }

    /// The echo, and what the body saw of the admin claims.
    #[derive(Clone, PartialEq, prost::Message)]
    pub struct ProbeReply {
        #[prost(string, tag = "1")]
        pub echo: String,
        #[prost(string, tag = "2")]
        pub tenant: String,
        #[prost(string, tag = "3")]
        pub subject: String,
        #[prost(string, tag = "4")]
        pub op: String,
        #[prost(string, tag = "5")]
        pub ticket: String,
    }

    impl prost::Name for ProbeReply {
        const NAME: &'static str = "ProbeReply";
        const PACKAGE: &'static str = "billing.admin.v1";
    }

    /// Which error to answer: 1 a refusal with every detail, 2 `UNAVAILABLE` without a reason.
    #[derive(Clone, PartialEq, prost::Message)]
    pub struct FailRequest {
        #[prost(uint32, tag = "1")]
        pub mode: u32,
    }

    impl prost::Name for FailRequest {
        const NAME: &'static str = "FailRequest";
        const PACKAGE: &'static str = "billing.admin.v1";
    }

    /// The test product's admin API.
    #[sekvent_component::component(
        name = "admin",
        package = "billing.admin.v1",
        proto = "crate::admin_api::proto"
    )]
    pub trait TenantAdmin: Send + Sync + 'static {
        /// Echoes the text with the claims the body sees.
        #[call(timeout = "5s")]
        async fn probe(&self, cx: &CallContext, req: ProbeRequest) -> Result<ProbeReply, AppError>;

        /// Answers the error `req.mode` names.
        #[call(timeout = "5s")]
        async fn fail(&self, cx: &CallContext, req: FailRequest) -> Result<ProbeReply, AppError>;
    }
}

/// The product's extra admin claims.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Ticket {
    ticket: String,
}

fn ticket() -> Ticket {
    Ticket {
        ticket: "T-42".into(),
    }
}

/// The refusal the `Fail` RPC answers in mode 1.
fn refusal() -> AppError {
    AppError::failed_precondition("the tenant is not suspended")
        .with_reason("TENANT_STATE")
        .with_domain("billing.admin.v1")
        .with_metadata("state", "active")
        .with_violation(
            FieldViolation::new("tenant_id", "must name a suspended tenant")
                .with_reason("NOT_SUSPENDED")
                .with_metadata("state", "active"),
        )
}

/// The test product's admin bodies.
struct Admin;

impl TenantAdmin for Admin {
    async fn probe(&self, cx: &CallContext, req: ProbeRequest) -> Result<ProbeReply, AppError> {
        let claims: &AdminClaims<Ticket> =
            admin::guard::claims(cx).ok_or_else(|| AppError::unauthenticated("no admin claims"))?;
        Ok(ProbeReply {
            echo: req.text,
            tenant: claims
                .tenant
                .map(|tenant| tenant.to_string())
                .unwrap_or_default(),
            subject: claims.subject.clone(),
            op: claims.op.clone(),
            ticket: claims.extra.ticket.clone(),
        })
    }

    async fn fail(&self, _cx: &CallContext, req: FailRequest) -> Result<ProbeReply, AppError> {
        Err(match req.mode {
            1 => refusal(),
            _ => AppError::unavailable("try again"),
        })
    }
}

fn signing_key() -> EdDsaSigningKey {
    EdDsaSigningKey::from_seed("ops-1", &[11; 32]).unwrap()
}

fn minter() -> Minter {
    Minter::new(ISSUER, signing_key(), Arc::new(SystemClock))
}

/// The admin App, served on `127.0.0.1:0`.
struct AdminSite {
    runtime: RuntimeHandle,
    addr: SocketAddr,
}

impl AdminSite {
    async fn start() -> Self {
        let clock: Arc<dyn Clock> = Arc::new(SystemClock);
        let keys = admin::parse_keys(&minter().public_key_entry()).unwrap();
        let verifier = Verifier::<Ticket>::new(ISSUER, AUDIENCE, keys);
        let authenticator = admin::authenticator(verifier, &[SERVICE], clock).unwrap();
        let config: MapSource = [
            ("SEKVENT_COMPONENT_ADMIN_SERVE", "grpc"),
            ("SEKVENT_COMPONENT_ADMIN_SERVE_AUTH", "bearer"),
        ]
        .into_iter()
        .collect();
        let mut builder = App::builder(&config);
        builder.end_user_authenticator(authenticator).unwrap();
        TenantAdminHandle::install(&mut builder, |_| Ok(Admin)).unwrap();
        let app = builder.build().unwrap();
        let server = Server::builder()
            .grpc_web(true)
            .grpc_routes(app.grpc_routes())
            .bind(SocketAddr::from(([127, 0, 0, 1], 0)))
            .await
            .unwrap();
        let addr = server.local_addr();
        let runtime = app
            .register(
                Runtime::builder()
                    .without_signals()
                    .shutdown_delay(Duration::ZERO),
            )
            .unit(
                "admin",
                Stage::Ingress,
                UnitPolicy::Critical,
                server.into_unit(),
            )
            .build()
            .unwrap()
            .start()
            .await
            .unwrap();
        Self { runtime, addr }
    }

    fn client(&self, audience: &str) -> AdminClient {
        AdminClient::new(&format!("http://{}/", self.addr), audience, minter()).unwrap()
    }

    async fn stop(self) {
        self.runtime.shutdown();
        self.runtime.wait().await.unwrap();
    }
}

/// Runs `test` under the 30 s guard.
async fn guarded<F: Future<Output = ()>>(test: F) {
    tokio::time::timeout(Duration::from_secs(30), test)
        .await
        .expect("the test finished within 30 s");
}

fn replied<T: Debug>(outcome: CallOutcome<T>) -> T {
    match outcome {
        CallOutcome::Replied(reply) => reply,
        other => panic!("not replied: {other:?}"),
    }
}

fn refused<T: Debug>(outcome: CallOutcome<T>) -> AppError {
    match outcome {
        CallOutcome::Refused(error) => error,
        other => panic!("not refused: {other:?}"),
    }
}

fn unknown<T: Debug>(outcome: CallOutcome<T>) -> AppError {
    match outcome {
        CallOutcome::Unknown(error) => error,
        other => panic!("not unknown: {other:?}"),
    }
}

fn not_sent<T: Debug>(outcome: CallOutcome<T>) -> AppError {
    match outcome {
        CallOutcome::NotSent(error) => error,
        other => panic!("not sent: {other:?}"),
    }
}

fn probe(text: &str) -> ProbeRequest {
    ProbeRequest { text: text.into() }
}

#[tokio::test]
async fn an_ok_answer_is_replied_with_the_claims_the_body_saw() {
    guarded(async {
        let site = AdminSite::start().await;
        let client = site.client(AUDIENCE);
        let tenant = TenantId::from_uuid(Uuid::new_v4());

        let outcome: CallOutcome<ProbeReply> = client
            .call(
                PROBE,
                Some(tenant),
                "operator-7",
                &ticket(),
                DEADLINE,
                &probe("hello"),
            )
            .await;
        let reply = replied(outcome);
        assert_eq!(
            reply,
            ProbeReply {
                echo: "hello".into(),
                tenant: tenant.to_string(),
                subject: "operator-7".into(),
                op: PROBE.into(),
                ticket: "T-42".into(),
            }
        );

        // A call that addresses no tenant, and a second call with a fresh token.
        let reply = client
            .call::<_, ProbeReply, _>(PROBE, None, "operator-7", &ticket(), DEADLINE, &probe(""))
            .await
            .into_result()
            .unwrap();
        assert_eq!(reply.tenant, "");
        assert_eq!(reply.echo, "");

        site.stop().await;
    })
    .await;
}

#[tokio::test]
async fn a_product_error_is_refused_with_every_detail() {
    guarded(async {
        let site = AdminSite::start().await;
        let client = site.client(AUDIENCE);

        let outcome = client
            .call::<_, ProbeReply, _>(
                FAIL,
                None,
                "operator-7",
                &ticket(),
                DEADLINE,
                &FailRequest { mode: 1 },
            )
            .await;
        let error = refused(outcome);
        let expected = refusal();
        assert_eq!(error.code(), ErrorCode::FailedPrecondition);
        assert_eq!(error.message(), expected.message());
        assert_eq!(error.reason(), Some("TENANT_STATE"));
        assert_eq!(error.domain(), Some("billing.admin.v1"));
        assert_eq!(error.metadata(), expected.metadata());
        assert_eq!(error.field_violations(), expected.field_violations());

        // A token for another audience is the verifier's one answer.
        let outcome = site
            .client("billing-us")
            .call::<_, ProbeReply, _>(PROBE, None, "operator-7", &ticket(), DEADLINE, &probe("x"))
            .await;
        let error = refused(outcome);
        assert_eq!(error.code(), ErrorCode::Unauthenticated);
        assert_eq!(
            error.reason(),
            Some(sekvent_tenancy::reasons::ADMIN_TOKEN_INVALID)
        );

        site.stop().await;
    })
    .await;
}

#[tokio::test]
async fn unavailable_without_a_reason_is_unknown() {
    guarded(async {
        let site = AdminSite::start().await;
        let outcome = site
            .client(AUDIENCE)
            .call::<_, ProbeReply, _>(
                FAIL,
                None,
                "operator-7",
                &ticket(),
                DEADLINE,
                &FailRequest { mode: 2 },
            )
            .await;
        let error = unknown(outcome);
        assert_eq!(error.code(), ErrorCode::Unavailable);
        assert_eq!(error.message(), "try again");
        assert_eq!(error.reason(), None);
        site.stop().await;
    })
    .await;
}

/// Reads one HTTP/1.1 request (its head and a `content-length` body) from `stream`.
async fn read_request(stream: &mut tokio::net::TcpStream) {
    let mut seen = Vec::new();
    let mut buffer = [0_u8; 4096];
    loop {
        let read = stream.read(&mut buffer).await.unwrap();
        assert_ne!(read, 0, "the client closed before its request was read");
        seen.extend_from_slice(&buffer[..read]);
        let Some(end) = seen.windows(4).position(|window| window == b"\r\n\r\n") else {
            continue;
        };
        let head = String::from_utf8_lossy(&seen[..end]).to_ascii_lowercase();
        let length: usize = head
            .lines()
            .find_map(|line| line.strip_prefix("content-length:"))
            .map_or(0, |value| value.trim().parse().unwrap());
        if seen.len() >= end + 4 + length {
            return;
        }
    }
}

#[tokio::test]
async fn a_connection_closed_after_the_request_is_unknown() {
    guarded(async {
        let listener = TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
            .await
            .unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            read_request(&mut stream).await;
            drop(stream);
        });
        let client = AdminClient::new(&format!("http://{addr}"), AUDIENCE, minter()).unwrap();
        let outcome = client
            .call::<_, ProbeReply, _>(PROBE, None, "operator-7", &ticket(), DEADLINE, &probe("x"))
            .await;
        let error = unknown(outcome);
        assert_eq!(error.code(), ErrorCode::Unknown);
        server.await.unwrap();
    })
    .await;
}

#[tokio::test]
async fn a_server_that_never_answers_runs_out_the_deadline() {
    guarded(async {
        let listener = TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
            .await
            .unwrap();
        let addr = listener.local_addr().unwrap();
        let (release, held) = tokio::sync::oneshot::channel::<()>();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            read_request(&mut stream).await;
            // Hold the connection open, unanswered, until the test is done.
            let _ = held.await;
            drop(stream);
        });
        let client = AdminClient::new(&format!("http://{addr}"), AUDIENCE, minter()).unwrap();
        let outcome = client
            .call::<_, ProbeReply, _>(
                PROBE,
                None,
                "operator-7",
                &ticket(),
                Duration::from_millis(300),
                &probe("x"),
            )
            .await;
        let error = unknown(outcome);
        assert_eq!(error.code(), ErrorCode::DeadlineExceeded);
        release.send(()).unwrap();
        server.await.unwrap();
    })
    .await;
}

#[tokio::test]
async fn no_listener_is_not_sent() {
    guarded(async {
        let client = AdminClient::new("http://127.0.0.1:1", AUDIENCE, minter()).unwrap();
        let outcome = client
            .call::<_, ProbeReply, _>(PROBE, None, "operator-7", &ticket(), DEADLINE, &probe("x"))
            .await;
        let error = not_sent(outcome);
        assert_eq!(error.code(), ErrorCode::Unavailable);
    })
    .await;
}

#[test]
fn plain_http_off_loopback_needs_in_cluster() {
    let url = "http://billing-admin.internal:8081";
    let error = AdminClient::new(url, AUDIENCE, minter()).unwrap_err();
    assert_eq!(error.code(), ErrorCode::InvalidArgument);
    assert!(!error.message().contains("billing-admin"));
    let client = AdminClient::in_cluster(url, AUDIENCE, minter()).unwrap();
    assert!(format!("{client:?}").contains(url));
    assert!(AdminClient::new("https://billing-admin.example.com", AUDIENCE, minter()).is_ok());
}
