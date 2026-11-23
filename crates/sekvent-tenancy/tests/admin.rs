//! The admin surface on a real control database: the guard's order and its audit row, and (with
//! feature `manager`, for its HTTP client) an admin App served over gRPC-Web on loopback with the
//! admin authenticator and a guarded RPC body.
#![cfg(feature = "admin")]

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use chrono::{DateTime, Utc};
use sekvent_context::CallContext;
use sekvent_error::{AppError, ErrorCode};
use sekvent_tenancy::admin::{AdminAudit, AdminClaims, NoExtra, guard};
use sekvent_tenancy::{ErrorDomains, TenantId, reasons};
use sqlx::PgPool;
use uuid::Uuid;

const OP: &str = "billing.admin.v1.TenantAdmin/Suspend";
const OTHER_OP: &str = "billing.admin.v1.TenantAdmin/Resume";
const SUBJECT: &str = "operator:ada";
const DOMAIN: &str = "billing.admin.v1";

/// Claims for [`OP`] addressing `tenant`, with a fresh token id.
fn claims_for(tenant: Option<TenantId>) -> AdminClaims {
    AdminClaims {
        token_id: Uuid::new_v4(),
        subject: SUBJECT.to_owned(),
        tenant,
        op: OP.to_owned(),
        expires_at: common::fixture_time() + Duration::from_secs(60),
        extra: NoExtra {},
    }
}

/// The call context the authenticator makes of `claims`.
fn context(claims: &AdminClaims) -> CallContext {
    CallContext::new().with_end_user(guard::end_user(claims.clone()))
}

/// The control schema, a pool on it and an audit answering in [`DOMAIN`] at the fixture's time.
async fn audit_of(cluster: &common::Cluster) -> (AdminAudit, PgPool) {
    let clock = Arc::new(common::manual_clock());
    let (_registry, control) = cluster.registry(clock.clone()).await;
    let audit = AdminAudit::new(control.clone(), clock)
        .with_domains(ErrorDomains::new().with_admin(DOMAIN));
    (audit, control)
}

/// The audit row of `token_id`: operation, actor, outcome, tenant.
async fn row_of(
    control: &PgPool,
    token_id: Uuid,
) -> Option<(String, String, String, Option<Uuid>)> {
    sqlx::query_as(
        "SELECT operation, actor, outcome, tenant_id FROM admin_audit WHERE token_id = $1",
    )
    .bind(token_id)
    .fetch_optional(control)
    .await
    .expect("the audit row was read")
}

async fn audit_rows(control: &PgPool) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM admin_audit")
        .fetch_one(control)
        .await
        .expect("the audit rows were counted")
}

/// [`guard::run`] with a body that counts its runs and answers `answer()`.
async fn guarded(
    audit: &AdminAudit,
    cx: &CallContext,
    op: &str,
    tenant: Option<TenantId>,
    runs: &AtomicUsize,
    answer: fn() -> Result<(), AppError>,
) -> Result<(), AppError> {
    guard::run(audit, cx, op, tenant, |_claims: AdminClaims| async move {
        runs.fetch_add(1, Ordering::SeqCst);
        answer()
    })
    .await
}

fn assert_invalid(error: &AppError) {
    assert_eq!(error.code(), ErrorCode::Unauthenticated);
    assert_eq!(error.reason(), Some(reasons::ADMIN_TOKEN_INVALID));
    assert_eq!(error.domain(), Some(DOMAIN));
}

#[tokio::test]
#[ignore = "needs Docker"]
async fn the_guard_refuses_before_it_records() {
    let Some(cluster) = common::Cluster::start().await else {
        return;
    };
    common::guard(async {
        let (audit, control) = audit_of(&cluster).await;
        let tenant = common::new_tenant();
        let claims = claims_for(Some(tenant));
        let cx = context(&claims);
        let runs = AtomicUsize::new(0);
        let ok: fn() -> Result<(), AppError> = || Ok(());

        // Another operation, another tenant, no tenant, a tenant for an untenanted call.
        let cases = [
            (OTHER_OP, Some(tenant)),
            (OP, Some(common::new_tenant())),
            (OP, None),
        ];
        for (op, called) in cases {
            let error = guarded(&audit, &cx, op, called, &runs, ok)
                .await
                .unwrap_err();
            assert_invalid(&error);
        }
        let untenanted = claims_for(None);
        let error = guarded(&audit, &context(&untenanted), OP, Some(tenant), &runs, ok)
            .await
            .unwrap_err();
        assert_invalid(&error);

        // No end user, or an end user whose admin claims have another extra type.
        let error = guarded(&audit, &CallContext::new(), OP, Some(tenant), &runs, ok)
            .await
            .unwrap_err();
        assert_invalid(&error);
        let error = guard::run(&audit, &cx, OP, Some(tenant), |_: AdminClaims<u8>| async {
            Ok::<_, AppError>(())
        })
        .await
        .unwrap_err();
        assert_invalid(&error);

        assert_eq!(runs.load(Ordering::SeqCst), 0);
        assert_eq!(audit_rows(&control).await, 0);
    })
    .await;
}

#[tokio::test]
#[ignore = "needs Docker"]
async fn a_token_runs_its_body_once() {
    let Some(cluster) = common::Cluster::start().await else {
        return;
    };
    common::guard(async {
        let (audit, control) = audit_of(&cluster).await;
        let tenant = common::new_tenant();
        let claims = claims_for(Some(tenant));
        let cx = context(&claims);

        let seen = guard::run(
            &audit,
            &cx,
            OP,
            Some(tenant),
            |claims: AdminClaims| async move { Ok::<_, AppError>(claims) },
        )
        .await
        .unwrap();
        assert_eq!(seen, claims);
        assert_eq!(
            row_of(&control, claims.token_id).await,
            Some((
                OP.to_owned(),
                SUBJECT.to_owned(),
                "ok".to_owned(),
                Some(Uuid::from(tenant))
            ))
        );
        let occurred_at: DateTime<Utc> =
            sqlx::query_scalar("SELECT occurred_at FROM admin_audit WHERE token_id = $1")
                .bind(claims.token_id)
                .fetch_one(&control)
                .await
                .unwrap();
        assert_eq!(occurred_at, DateTime::<Utc>::from(common::fixture_time()));

        // The same token again: refused before the body, the stored outcome unchanged.
        let runs = AtomicUsize::new(0);
        let error = guarded(&audit, &cx, OP, Some(tenant), &runs, || Ok(()))
            .await
            .unwrap_err();
        assert_eq!(error.code(), ErrorCode::AlreadyExists);
        assert_eq!(error.reason(), Some(reasons::ADMIN_TOKEN_REPLAYED));
        assert_eq!(error.domain(), Some(DOMAIN));
        assert_eq!(runs.load(Ordering::SeqCst), 0);
        assert_eq!(audit_rows(&control).await, 1);
        assert_eq!(
            row_of(&control, claims.token_id).await.map(|row| row.2),
            Some("ok".to_owned())
        );
    })
    .await;
}

#[tokio::test]
#[ignore = "needs Docker"]
async fn a_failed_body_is_stored_and_its_reason_is_placed_in_the_admin_domain() {
    let Some(cluster) = common::Cluster::start().await else {
        return;
    };
    common::guard(async {
        let (audit, control) = audit_of(&cluster).await;
        let runs = AtomicUsize::new(0);

        // A reason without a domain gets the admin domain.
        let claims = claims_for(None);
        let error = guarded(&audit, &context(&claims), OP, None, &runs, || {
            Err(AppError::failed_precondition("not now").with_reason("PLAN_LIMIT"))
        })
        .await
        .unwrap_err();
        assert_eq!(error.code(), ErrorCode::FailedPrecondition);
        assert_eq!(error.reason(), Some("PLAN_LIMIT"));
        assert_eq!(error.domain(), Some(DOMAIN));
        assert_eq!(
            row_of(&control, claims.token_id).await,
            Some((OP.to_owned(), SUBJECT.to_owned(), "failed".to_owned(), None))
        );

        // A domain of the body's own is kept.
        let claims = claims_for(None);
        let error = guarded(&audit, &context(&claims), OP, None, &runs, || {
            Err(AppError::failed_precondition("not now")
                .with_reason("PLAN_LIMIT")
                .with_domain("billing.plans.v1"))
        })
        .await
        .unwrap_err();
        assert_eq!(error.domain(), Some("billing.plans.v1"));

        // An error without a reason passes unchanged.
        let claims = claims_for(None);
        let error = guarded(&audit, &context(&claims), OP, None, &runs, || {
            Err(AppError::unavailable("try again"))
        })
        .await
        .unwrap_err();
        assert_eq!(error.code(), ErrorCode::Unavailable);
        assert_eq!(error.reason(), None);
        assert_eq!(error.domain(), None);
        assert_eq!(
            row_of(&control, claims.token_id).await.map(|row| row.2),
            Some("failed".to_owned())
        );

        assert_eq!(runs.load(Ordering::SeqCst), 3);
        assert_eq!(audit_rows(&control).await, 3);
    })
    .await;
}

#[tokio::test]
#[ignore = "needs Docker"]
async fn record_and_finish_audit_a_body_run_outside_the_guard() {
    let Some(cluster) = common::Cluster::start().await else {
        return;
    };
    common::guard(async {
        let (audit, control) = audit_of(&cluster).await;
        let tenant = common::new_tenant();
        let claims = claims_for(Some(tenant));

        audit.record(&claims).await.expect("the first record");
        assert_eq!(
            row_of(&control, claims.token_id).await,
            Some((
                OP.to_owned(),
                SUBJECT.to_owned(),
                "started".to_owned(),
                Some(Uuid::from(tenant))
            ))
        );
        let replayed = audit.record(&claims).await.expect_err("a replay");
        assert_eq!(replayed.code(), ErrorCode::AlreadyExists);
        assert_eq!(replayed.reason(), Some(reasons::ADMIN_TOKEN_REPLAYED));
        assert_eq!(replayed.domain(), Some(DOMAIN));

        audit.finish(claims.token_id, false).await;
        audit.finish(claims.token_id, true).await;
        assert_eq!(
            row_of(&control, claims.token_id).await.map(|row| row.2),
            Some("failed".to_owned()),
            "only a started row takes an outcome"
        );
        audit.finish(Uuid::new_v4(), true).await;
        assert_eq!(
            audit_rows(&control).await,
            1,
            "an unknown token writes nothing"
        );

        let untenanted = claims_for(None);
        audit
            .record(&untenanted)
            .await
            .expect("a call without a tenant");
        audit.finish(untenanted.token_id, true).await;
        assert_eq!(
            row_of(&control, untenanted.token_id).await,
            Some((OP.to_owned(), SUBJECT.to_owned(), "ok".to_owned(), None))
        );
    })
    .await;
}

/// The test's admin service, as sekvent-proto-build would declare it for
/// `billing.admin.v1.TenantAdmin` with one RPC taking and answering `google.protobuf.Empty`.
#[cfg(feature = "manager")]
#[allow(dead_code, missing_docs, unreachable_pub)]
mod tenant_admin {
    use sekvent_component::{AppError, CallContext};

    pub mod proto {
        #[allow(non_upper_case_globals, clippy::type_complexity)]
        pub const __sekvent_service_TenantAdmin: (&str, &[(&str, &str, &str, bool, bool)]) = (
            "billing.admin.v1.TenantAdmin",
            &[(
                "Suspend",
                "google.protobuf.Empty",
                "google.protobuf.Empty",
                false,
                false,
            )],
        );
        #[allow(non_camel_case_types)]
        pub type __sekvent_rpc_TenantAdmin__Suspend = ((), ());
    }

    /// Tenant administration for an operator service.
    #[sekvent_component::component(
        name = "tenant_admin",
        package = "billing.admin.v1",
        proto = "crate::tenant_admin::proto"
    )]
    pub trait TenantAdmin: Send + Sync + 'static {
        /// Suspend the tenant the token names.
        #[call(timeout = "5s")]
        async fn suspend(&self, cx: &CallContext, req: ()) -> Result<(), AppError>;
    }
}

/// An admin App served over gRPC-Web on loopback, called with tokens minted in the test.
#[cfg(feature = "manager")]
mod served {
    use std::net::SocketAddr;
    use std::sync::{Arc, Mutex, PoisonError};
    use std::time::Duration;

    use base64::Engine as _;
    use base64::engine::general_purpose::STANDARD_NO_PAD;
    use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
    use sekvent_auth::{Claims, EdDsaSigningKey};
    use sekvent_component::{App, AppError, CallContext, END_USER_REJECTED_MESSAGE};
    use sekvent_config::MapSource;
    use sekvent_runtime::{Runtime, RuntimeHandle, Server, Stage, UnitPolicy};
    use sekvent_tenancy::admin::{self, AdminAudit, AdminClaims, NoExtra, Verifier, guard};
    use sekvent_tenancy::{ErrorDomains, TenantId, reasons};
    use serde_json::{Map, Value, json};
    use uuid::Uuid;

    use super::tenant_admin::{TenantAdmin, TenantAdminHandle};
    use super::{DOMAIN, OP, OTHER_OP, SUBJECT, common};

    const ISSUER: &str = "ops-console";
    const AUDIENCE: &str = "billing-eu";
    const SERVICE: &str = "billing.admin.v1.TenantAdmin";
    /// The listener's path prefix; the component sees the path below it.
    const PREFIX: &str = "/admin";
    /// gRPC `OK`, `UNAUTHENTICATED` and `ALREADY_EXISTS`.
    const OK: i32 = 0;
    const UNAUTHENTICATED: i32 = 16;
    const ALREADY_EXISTS: i32 = 6;

    /// The `google.rpc.Status` of `grpc-status-details-bin`, as far as the test reads it.
    #[allow(dead_code)]
    #[derive(Clone, PartialEq, prost::Message)]
    struct RpcStatus {
        #[prost(int32, tag = "1")]
        code: i32,
        #[prost(string, tag = "2")]
        message: String,
        #[prost(message, repeated, tag = "3")]
        details: Vec<Any>,
    }

    #[derive(Clone, PartialEq, prost::Message)]
    struct Any {
        #[prost(string, tag = "1")]
        type_url: String,
        #[prost(bytes = "vec", tag = "2")]
        value: Vec<u8>,
    }

    #[derive(Clone, PartialEq, prost::Message)]
    struct ErrorInfo {
        #[prost(string, tag = "1")]
        reason: String,
        #[prost(string, tag = "2")]
        domain: String,
    }

    /// The admin component: the body records the claims it ran with.
    struct Admin {
        audit: AdminAudit,
        tenant: TenantId,
        seen: Arc<Mutex<Vec<AdminClaims>>>,
    }

    impl TenantAdmin for Admin {
        async fn suspend(&self, cx: &CallContext, (): ()) -> Result<(), AppError> {
            guard::run(
                &self.audit,
                cx,
                OP,
                Some(self.tenant),
                |claims: AdminClaims| async move {
                    self.seen
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .push(claims);
                    Ok(())
                },
            )
            .await
        }
    }

    struct Served {
        runtime: RuntimeHandle,
        addr: SocketAddr,
        seen: Arc<Mutex<Vec<AdminClaims>>>,
    }

    impl Served {
        fn seen(&self) -> Vec<AdminClaims> {
            self.seen
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone()
        }

        async fn stop(self) {
            self.runtime.shutdown();
            self.runtime.wait().await.unwrap();
        }
    }

    fn signer() -> EdDsaSigningKey {
        EdDsaSigningKey::from_seed("k1", &[7; 32]).unwrap()
    }

    fn now_secs() -> u64 {
        common::fixture_time()
            .duration_since(std::time::SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs()
    }

    /// A token for `op` addressing `tenant`, minted at the fixture's time.
    fn mint(op: &str, tenant: TenantId) -> (String, Uuid) {
        let token_id = Uuid::new_v4();
        let mut custom = Map::new();
        custom.insert("tid".into(), json!(tenant.to_string()));
        custom.insert("op".into(), Value::String(op.to_owned()));
        let claims = Claims::new(SUBJECT, now_secs(), 60, custom)
            .with_issuer(ISSUER)
            .with_audience(AUDIENCE)
            .with_jti(token_id.to_string());
        (signer().issue(&claims).unwrap(), token_id)
    }

    async fn serve(cluster: &common::Cluster, tenant: TenantId) -> Served {
        let clock = Arc::new(common::manual_clock());
        let (_registry, control) = cluster.registry(clock.clone()).await;
        let domains = ErrorDomains::new().with_admin(DOMAIN);
        let audit = AdminAudit::new(control, clock.clone()).with_domains(domains);
        let verifier = Verifier::<NoExtra>::new(ISSUER, AUDIENCE, signer().verification_keys())
            .with_domains(domains);
        let authenticator = admin::authenticator(verifier, &[SERVICE], clock).unwrap();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let component = Admin {
            audit,
            tenant,
            seen: Arc::clone(&seen),
        };

        let keys: MapSource = [
            ("SEKVENT_COMPONENT_TENANT_ADMIN_SERVE", "grpc"),
            ("SEKVENT_COMPONENT_TENANT_ADMIN_SERVE_AUTH", "bearer"),
        ]
        .into_iter()
        .collect();
        let mut builder = App::builder(&keys);
        builder.end_user_authenticator(authenticator).unwrap();
        TenantAdminHandle::install(&mut builder, move |_| Ok(component)).unwrap();
        let app = builder.build().unwrap();

        let server = Server::builder()
            .prefix(PREFIX)
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
                "admin-grpc",
                Stage::Ingress,
                UnitPolicy::Critical,
                server.into_unit(),
            )
            .build()
            .unwrap()
            .start()
            .await
            .unwrap();
        Served {
            runtime,
            addr,
            seen,
        }
    }

    /// What a gRPC-Web call answered.
    #[derive(Debug)]
    struct Answer {
        status: i32,
        message: String,
        reason: Option<String>,
        domain: Option<String>,
    }

    /// The reason and domain of the `ErrorInfo` in `grpc-status-details-bin`.
    fn error_info(value: &HeaderValue) -> (Option<String>, Option<String>) {
        let text = value.to_str().unwrap().trim_end_matches('=');
        let bytes = STANDARD_NO_PAD.decode(text).unwrap();
        let status = <RpcStatus as prost::Message>::decode(bytes.as_slice()).unwrap();
        status
            .details
            .iter()
            .find(|detail| detail.type_url.ends_with("google.rpc.ErrorInfo"))
            .map(|detail| {
                let info = <ErrorInfo as prost::Message>::decode(detail.value.as_slice()).unwrap();
                (Some(info.reason), Some(info.domain))
            })
            .unwrap_or_default()
    }

    /// A unary gRPC-Web call of `path` with an empty request, over HTTP/1.1.
    async fn call(
        client: &reqwest::Client,
        addr: SocketAddr,
        path: &str,
        token: Option<&str>,
    ) -> Answer {
        let mut request = client
            .post(format!("http://{addr}{path}"))
            .header("content-type", "application/grpc-web+proto")
            .header("x-grpc-web", "1")
            .body(vec![0_u8, 0, 0, 0, 0]);
        if let Some(token) = token {
            request = request.header("authorization", format!("Bearer {token}"));
        }
        let response = request.send().await.unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        let mut headers: HeaderMap = response.headers().clone();
        let body = response.bytes().await.unwrap().to_vec();
        let mut rest = body.as_slice();
        while rest.len() >= 5 {
            let flag = rest[0];
            let length =
                usize::try_from(u32::from_be_bytes([rest[1], rest[2], rest[3], rest[4]])).unwrap();
            let (payload, after) = rest[5..].split_at(length);
            rest = after;
            if flag & 0x80 == 0 {
                continue;
            }
            for line in std::str::from_utf8(payload).unwrap().split("\r\n") {
                if let Some((name, value)) = line.split_once(':') {
                    headers.insert(
                        HeaderName::from_bytes(name.trim().to_ascii_lowercase().as_bytes())
                            .unwrap(),
                        value.trim().parse().unwrap(),
                    );
                }
            }
        }
        let status = headers["grpc-status"].to_str().unwrap().parse().unwrap();
        let message = headers
            .get("grpc-message")
            .map(|value| value.to_str().unwrap().to_owned())
            .unwrap_or_default();
        let (reason, domain) = headers
            .get("grpc-status-details-bin")
            .map(error_info)
            .unwrap_or_default();
        Answer {
            status,
            message,
            reason,
            domain,
        }
    }

    fn client() -> reqwest::Client {
        sekvent_client::reqwest_builder()
            .http1_only()
            .build()
            .unwrap()
    }

    fn assert_invalid(answer: &Answer) {
        assert_eq!(answer.status, UNAUTHENTICATED, "{answer:?}");
        assert_eq!(answer.reason.as_deref(), Some(reasons::ADMIN_TOKEN_INVALID));
        assert_eq!(answer.domain.as_deref(), Some(DOMAIN));
    }

    #[tokio::test]
    #[ignore = "needs Docker"]
    async fn a_minted_token_reaches_the_guarded_body_once() {
        let Some(cluster) = common::Cluster::start().await else {
            return;
        };
        common::guard(async {
            let tenant = common::new_tenant();
            let served = serve(&cluster, tenant).await;
            let client = client();
            let path = format!("{PREFIX}/{OP}");

            let (token, token_id) = mint(OP, tenant);
            let answer = call(&client, served.addr, &path, Some(&token)).await;
            assert_eq!(answer.status, OK, "{answer:?}");
            let seen = served.seen();
            assert_eq!(seen.len(), 1);
            assert_eq!(seen[0].token_id, token_id);
            assert_eq!(seen[0].subject, SUBJECT);
            assert_eq!(seen[0].tenant, Some(tenant));
            assert_eq!(seen[0].op, OP);

            // The same token again is a replay, refused before the body.
            let answer = call(&client, served.addr, &path, Some(&token)).await;
            assert_eq!(answer.status, ALREADY_EXISTS, "{answer:?}");
            assert_eq!(
                answer.reason.as_deref(),
                Some(reasons::ADMIN_TOKEN_REPLAYED)
            );
            assert_eq!(answer.domain.as_deref(), Some(DOMAIN));
            assert_eq!(served.seen().len(), 1);

            served.stop().await;
        })
        .await;
    }

    #[tokio::test]
    #[ignore = "needs Docker"]
    async fn a_wrong_or_missing_token_is_refused_before_the_body() {
        let Some(cluster) = common::Cluster::start().await else {
            return;
        };
        common::guard(async {
            let tenant = common::new_tenant();
            let served = serve(&cluster, tenant).await;
            let client = client();
            let path = format!("{PREFIX}/{OP}");

            // Minted for another RPC, for an RPC of another service, for another tenant
            // (the authenticator passes it, the guard refuses it), or not verifiable.
            let (other_op, _) = mint(OTHER_OP, tenant);
            let (other_service, _) = mint("billing.admin.v1.Reports/Suspend", tenant);
            let (other_tenant, _) = mint(OP, common::new_tenant());
            for token in [
                other_op.as_str(),
                other_service.as_str(),
                other_tenant.as_str(),
                "not.a.token",
            ] {
                let answer = call(&client, served.addr, &path, Some(token)).await;
                assert_invalid(&answer);
                assert_eq!(
                    answer.message.replace("%20", " "),
                    END_USER_REJECTED_MESSAGE
                );
            }

            // No bearer token at all: the App's default precheck refuses it with the constant
            // answer, before the authenticator, so no reason is attached.
            let answer = call(&client, served.addr, &path, None).await;
            assert_eq!(answer.status, UNAUTHENTICATED, "{answer:?}");
            assert_eq!(answer.reason, None);

            assert_eq!(served.seen(), [] as [AdminClaims; 0]);
            served.stop().await;
        })
        .await;
    }
}
