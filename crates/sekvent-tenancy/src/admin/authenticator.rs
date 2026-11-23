//! The end-user authenticator of an admin App: the bearer token verified for the routed RPC.

use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use http::request::Parts;
use sekvent_component::{BearerShape, EndUserAuthenticator, EndUserRejection};
use sekvent_config::Secret;
use sekvent_context::{Clock, EndUser};
use sekvent_error::AppError;
use serde::de::DeserializeOwned;

use super::claims::NoExtra;
use super::guard;
use super::verifier::Verifier;
use crate::reasons;

/// The authenticator of an admin App. `Debug` prints the services only.
pub struct AdminAuthenticator<X = NoExtra> {
    verifier: Arc<Verifier<X>>,
    services: Vec<String>,
    clock: Arc<dyn Clock>,
}

impl<X> fmt::Debug for AdminAuthenticator<X> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AdminAuthenticator")
            .field("services", &self.services)
            .finish_non_exhaustive()
    }
}

/// The authenticator for the admin services `services` (`<package>.<Service>`): the routed path
/// must be `/<service>/<Rpc>` of one of them; the token is `BearerShape::token`'s; the claims,
/// verified for the path without its leading slash, become the extension of an `EndUser` whose
/// subject is `sub` and which names no tenant (the admin App has no scope resolver). Every
/// refusal is an `EndUserRejection` with `ADMIN_TOKEN_INVALID` and the verifier's admin domain.
/// `INVALID_ARGUMENT` for no service or a service name outside `[A-Za-z0-9_.]`.
///
/// The end user names no tenant on purpose: a scope that named one would refuse an end user
/// without the same tenant, and the tenant a call addresses is checked by the guard against the
/// claims instead.
pub fn authenticator<X>(
    verifier: Verifier<X>,
    services: &[&str],
    clock: Arc<dyn Clock>,
) -> Result<AdminAuthenticator<X>, AppError>
where
    X: DeserializeOwned + Clone + Send + Sync + 'static,
{
    if services.is_empty() {
        return Err(AppError::invalid_argument(
            "an admin authenticator needs at least one service",
        ));
    }
    for (index, service) in services.iter().enumerate() {
        if !is_service_name(service) {
            return Err(AppError::invalid_argument(format!(
                "admin service {} is not a service name of letters, digits, `_` and `.`",
                index + 1
            )));
        }
    }
    Ok(AdminAuthenticator {
        verifier: Arc::new(verifier),
        services: services.iter().copied().map(str::to_owned).collect(),
        clock,
    })
}

impl<X> AdminAuthenticator<X>
where
    X: DeserializeOwned + Clone + Send + Sync + 'static,
{
    /// The operation a routed path names, `<service>/<Rpc>` without the leading slash, when the
    /// service is one of this App's and the RPC name is well formed.
    fn operation<'p>(&self, path: &'p str) -> Option<&'p str> {
        let op = path.strip_prefix('/')?;
        let (service, rpc) = op.split_once('/')?;
        let known = self.services.iter().any(|known| known == service);
        (known && is_rpc_name(rpc)).then_some(op)
    }

    /// The end user of `request`, or the reason it was refused (a fixed text for the log).
    fn end_user(&self, request: &Parts) -> Result<EndUser, &'static str> {
        let op = self
            .operation(request.uri.path())
            .ok_or("the path is not an RPC of an admin service")?;
        let token = BearerShape::token(&request.headers).ok_or("no bearer token")?;
        let claims = self
            .verifier
            .verify(&Secret::new(token.to_owned()), op, self.clock.now())
            .map_err(|_| "the token was refused")?;
        Ok(guard::end_user(claims))
    }

    /// The one refusal of this authenticator.
    fn rejection(&self) -> AppError {
        EndUserRejection::new(reasons::ADMIN_TOKEN_INVALID)
            .with_domain(self.verifier.domains().admin)
            .into()
    }
}

impl<X> EndUserAuthenticator for AdminAuthenticator<X>
where
    X: DeserializeOwned + Clone + Send + Sync + 'static,
{
    fn authenticate<'a>(
        &'a self,
        request: &'a Parts,
    ) -> Pin<Box<dyn Future<Output = Result<EndUser, AppError>> + Send + 'a>> {
        let answer = self.end_user(request).map_err(|cause| {
            tracing::debug!(cause, "admin call refused");
            self.rejection()
        });
        Box::pin(std::future::ready(answer))
    }
}

/// `[A-Za-z0-9_.]+`: a full service name.
fn is_service_name(text: &str) -> bool {
    !text.is_empty()
        && text
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'.')
}

/// `[A-Za-z0-9_]+`: an RPC name.
fn is_rpc_name(text: &str) -> bool {
    !text.is_empty()
        && text
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::{Duration, SystemTime};

    use http::request::Parts;
    use sekvent_auth::{Claims, EdDsaSigningKey};
    use sekvent_component::{END_USER_REJECTED_MESSAGE, EndUserAuthenticator, EndUserRejection};
    use sekvent_context::{ManualClock, SystemClock};
    use sekvent_error::{AppError, ErrorCode};
    use serde_json::{Map, Value, json};
    use uuid::Uuid;

    use super::{AdminAuthenticator, authenticator};
    use crate::admin::claims::{AdminClaims, NoExtra};
    use crate::admin::verifier::Verifier;
    use crate::{ErrorDomains, TenantId, reasons};

    const ISSUER: &str = "ops-console";
    const AUDIENCE: &str = "billing-eu";
    const SERVICE: &str = "billing.admin.v1.TenantAdmin";
    const OTHER_SERVICE: &str = "billing.admin.v1.Reports";
    const OP: &str = "billing.admin.v1.TenantAdmin/Suspend";
    const TENANT: &str = "6f1c2a8e-3b4d-4e5f-8a9b-0c1d2e3f4a5b";
    const NOW: u64 = 1_700_000_000;

    fn signer() -> EdDsaSigningKey {
        EdDsaSigningKey::from_seed("k1", &[7; 32]).unwrap()
    }

    fn clock() -> Arc<ManualClock> {
        Arc::new(ManualClock::new(
            SystemTime::UNIX_EPOCH + Duration::from_secs(NOW),
        ))
    }

    fn admin(domains: ErrorDomains) -> AdminAuthenticator {
        let verifier =
            Verifier::new(ISSUER, AUDIENCE, signer().verification_keys()).with_domains(domains);
        authenticator(verifier, &[SERVICE, OTHER_SERVICE], clock()).unwrap()
    }

    fn token(op: &str) -> String {
        let mut custom = Map::new();
        custom.insert("tid".into(), json!(TENANT));
        custom.insert("op".into(), Value::String(op.to_owned()));
        let claims = Claims::new("operator:ada", NOW, 60, custom)
            .with_issuer(ISSUER)
            .with_audience(AUDIENCE)
            .with_jti(Uuid::new_v4().to_string());
        signer().issue(&claims).unwrap()
    }

    fn request(path: &str, authorization: Option<&str>) -> Parts {
        let mut request = http::Request::builder().uri(path);
        if let Some(value) = authorization {
            request = request.header(http::header::AUTHORIZATION, value);
        }
        request.body(()).unwrap().into_parts().0
    }

    fn assert_rejected(error: &AppError, domain: &'static str) {
        assert_eq!(error.code(), ErrorCode::Unauthenticated);
        assert_eq!(error.message(), END_USER_REJECTED_MESSAGE);
        assert_eq!(error.reason(), Some(reasons::ADMIN_TOKEN_INVALID));
        assert_eq!(error.domain(), Some(domain));
        let source = std::error::Error::source(error)
            .and_then(|source| source.downcast_ref::<EndUserRejection>())
            .copied();
        assert_eq!(
            source,
            Some(EndUserRejection::new(reasons::ADMIN_TOKEN_INVALID).with_domain(domain))
        );
    }

    #[tokio::test]
    async fn a_token_for_the_routed_rpc_is_an_end_user_with_its_claims() {
        let admin = admin(ErrorDomains::new());
        let bearer = format!("Bearer {}", token(OP));
        let user = admin
            .authenticate(&request(&format!("/{OP}"), Some(&bearer)))
            .await
            .unwrap();
        assert_eq!(user.subject(), "operator:ada");
        assert_eq!(user.tenant(), None);
        let claims = user.extension::<AdminClaims<NoExtra>>().unwrap();
        assert_eq!(claims.op, OP);
        assert_eq!(claims.tenant, TenantId::parse(TENANT));

        let other = format!("{OTHER_SERVICE}/Export_2");
        let bearer = format!("Bearer {}", token(&other));
        let user = admin
            .authenticate(&request(&format!("/{other}"), Some(&bearer)))
            .await
            .unwrap();
        assert_eq!(user.extension::<AdminClaims<NoExtra>>().unwrap().op, other);
    }

    #[tokio::test]
    async fn every_refusal_is_the_one_rejection() {
        let admin = admin(ErrorDomains::new().with_admin("billing.admin.v1"));
        let good = format!("Bearer {}", token(OP));
        let cases = [
            // No token, or not a bearer token.
            (format!("/{OP}"), None),
            (format!("/{OP}"), Some("Basic dXNlcg==".to_owned())),
            // A token for another RPC of the same service.
            (format!("/{SERVICE}/Resume"), Some(good.clone())),
            // A token for the same RPC name of another service.
            (format!("/{OTHER_SERVICE}/Suspend"), Some(good.clone())),
            // A path outside the admin services, or not `/<service>/<Rpc>`.
            (
                "/billing.orders.v1.Orders/Suspend".to_owned(),
                Some(good.clone()),
            ),
            (format!("/{SERVICE}/"), Some(good.clone())),
            (format!("/{SERVICE}"), Some(good.clone())),
            (format!("/{OP}/extra"), Some(good.clone())),
            (format!("/{SERVICE}/Sus-pend"), Some(good.clone())),
            (format!("//{OP}"), Some(good.clone())),
            ("/".to_owned(), Some(good.clone())),
            // A token that does not verify.
            (format!("/{OP}"), Some("Bearer not.a.token".to_owned())),
        ];
        for (path, authorization) in cases {
            let error = admin
                .authenticate(&request(&path, authorization.as_deref()))
                .await
                .unwrap_err();
            assert_rejected(&error, "billing.admin.v1");
        }
    }

    #[tokio::test]
    async fn the_clock_decides_expiry() {
        let clock = clock();
        let verifier = Verifier::<NoExtra>::new(ISSUER, AUDIENCE, signer().verification_keys());
        let admin = authenticator(verifier, &[SERVICE], clock.clone()).unwrap();
        let bearer = format!("Bearer {}", token(OP));
        let path = format!("/{OP}");
        assert!(
            admin
                .authenticate(&request(&path, Some(&bearer)))
                .await
                .is_ok()
        );
        clock.advance(Duration::from_secs(60));
        let error = admin
            .authenticate(&request(&path, Some(&bearer)))
            .await
            .unwrap_err();
        assert_rejected(&error, ErrorDomains::DEFAULT_ADMIN);
    }

    #[test]
    fn the_services_are_checked() {
        let build = |services: &[&str]| {
            let verifier = Verifier::<NoExtra>::refusing();
            authenticator(verifier, services, Arc::new(SystemClock))
        };
        assert!(build(&[SERVICE, "Plain_Service2"]).is_ok());
        for (services, message) in [
            (&[][..], "an admin authenticator needs at least one service"),
            (&[""][..], "admin service 1 is not a service name"),
            (
                &[SERVICE, "billing/admin"][..],
                "admin service 2 is not a service name",
            ),
            (
                &[SERVICE, "billing admin"][..],
                "admin service 2 is not a service name",
            ),
            (
                &[SERVICE, "billing-admin"][..],
                "admin service 2 is not a service name",
            ),
        ] {
            let error = build(services).unwrap_err();
            assert_eq!(error.code(), ErrorCode::InvalidArgument);
            assert!(error.message().starts_with(message), "{}", error.message());
        }
    }

    #[test]
    fn debug_prints_the_services_only() {
        let admin = admin(ErrorDomains::new());
        assert_eq!(
            format!("{admin:?}"),
            format!(r#"AdminAuthenticator {{ services: ["{SERVICE}", "{OTHER_SERVICE}"], .. }}"#)
        );
    }
}
