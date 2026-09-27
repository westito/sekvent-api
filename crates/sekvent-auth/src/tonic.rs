//! tonic support: [`BearerInterceptor`] verifies `authorization: Bearer <jwt>`
//! and stores the claims in the request extensions; [`verified_claims`] and
//! [`require_any_role`] read them back in a handler.
//!
//! ```ignore
//! let auth = BearerAuth::new(keys, Validation::new(), Arc::new(SystemClock));
//! let service = OrdersServer::with_interceptor(orders, BearerInterceptor::<Profile>::new(auth));
//!
//! async fn cancel(&self, request: Request<CancelOrder>) -> Result<Response<()>, Status> {
//!     let claims = require_any_role::<Profile, _>(&request, &["admin"])?;
//!     // …
//! }
//! ```
//!
//! Failures become `Status` through `sekvent-error`: `UNAUTHENTICATED` for a
//! missing or invalid token, `PERMISSION_DENIED` for a missing role.

use std::fmt;
use std::marker::PhantomData;

use ::tonic::service::Interceptor;
use ::tonic::{Request, Status};
use sekvent_error::AppError;
use serde::de::DeserializeOwned;

use crate::BearerAuth;
use crate::jwt::{Claims, NoClaims, TokenRejected};
use crate::roles::{HasRoles, require_any_role as check_roles};

/// Verifies the bearer token of each request and inserts its
/// [`Claims<T>`] into the request extensions.
pub struct BearerInterceptor<T = NoClaims> {
    auth: BearerAuth,
    claims: PhantomData<fn() -> T>,
}

impl<T> BearerInterceptor<T> {
    /// An interceptor verifying with `auth`.
    pub fn new(auth: BearerAuth) -> Self {
        Self {
            auth,
            claims: PhantomData,
        }
    }
}

impl<T> Clone for BearerInterceptor<T> {
    fn clone(&self) -> Self {
        Self::new(self.auth.clone())
    }
}

impl<T> fmt::Debug for BearerInterceptor<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BearerInterceptor")
            .field("auth", &self.auth)
            .finish_non_exhaustive()
    }
}

impl<T> Interceptor for BearerInterceptor<T>
where
    T: DeserializeOwned + Clone + Send + Sync + 'static,
{
    fn call(&mut self, mut request: Request<()>) -> Result<Request<()>, Status> {
        let header = request
            .metadata()
            .get("authorization")
            .and_then(|value| value.to_str().ok());
        let claims = self.auth.authenticate_header::<T>(header)?;
        request.extensions_mut().insert(claims);
        Ok(request)
    }
}

/// The claims [`BearerInterceptor`] stored on `request`.
///
/// Fails closed with `UNAUTHENTICATED` when they are absent, e.g. because
/// the interceptor was not installed on this service.
pub fn verified_claims<T, M>(request: &Request<M>) -> Result<&Claims<T>, Status>
where
    T: Send + Sync + 'static,
{
    request
        .extensions()
        .get::<Claims<T>>()
        .ok_or_else(|| Status::from(AppError::from(TokenRejected::Malformed)))
}

/// The stored claims, provided they grant at least one of `roles`
/// (`PERMISSION_DENIED` otherwise).
pub fn require_any_role<'r, T, M>(
    request: &'r Request<M>,
    roles: &[&str],
) -> Result<&'r Claims<T>, Status>
where
    T: HasRoles + Send + Sync + 'static,
{
    let claims = verified_claims::<T, M>(request)?;
    check_roles(claims, roles)?;
    Ok(claims)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::{Duration, SystemTime};

    use ::tonic::Code;
    use sekvent_config::Secret;
    use sekvent_context::ManualClock;
    use serde::{Deserialize, Serialize};

    use super::*;
    use crate::jwt::{JwtKeys, Validation};

    #[derive(Debug, Clone, Serialize, Deserialize)]
    struct Profile {
        roles: Vec<String>,
    }

    impl HasRoles for Profile {
        fn has_role(&self, role: &str) -> bool {
            self.roles.has_role(role)
        }
    }

    fn setup() -> (JwtKeys, ManualClock, BearerInterceptor<Profile>) {
        let keys = JwtKeys::hs256("k1", &Secret::new("0123456789abcdef0123456789abcdef")).unwrap();
        let clock = ManualClock::new(SystemTime::UNIX_EPOCH + Duration::from_secs(1_000));
        let auth = BearerAuth::new(
            keys.clone(),
            Validation::new().with_leeway(0),
            Arc::new(clock.clone()),
        );
        (keys, clock, BearerInterceptor::new(auth))
    }

    fn request(authorization: Option<&str>) -> Request<()> {
        let mut request = Request::new(());
        if let Some(value) = authorization {
            request
                .metadata_mut()
                .insert("authorization", value.parse().unwrap());
        }
        request
    }

    fn token(keys: &JwtKeys, roles: &[&str]) -> String {
        keys.issue(&Claims::new(
            "ada",
            1_000,
            60,
            Profile {
                roles: roles.iter().map(|role| (*role).to_owned()).collect(),
            },
        ))
        .unwrap()
    }

    #[test]
    fn valid_token_inserts_claims() {
        let (keys, _, mut interceptor) = setup();
        let bearer = format!("Bearer {}", token(&keys, &["admin"]));
        let request = interceptor.call(request(Some(&bearer))).unwrap();
        assert_eq!(verified_claims::<Profile, _>(&request).unwrap().sub, "ada");
        assert!(require_any_role::<Profile, _>(&request, &["admin"]).is_ok());
        let denied = require_any_role::<Profile, _>(&request, &["owner"]).unwrap_err();
        assert_eq!(denied.code(), Code::PermissionDenied);
    }

    #[test]
    fn invalid_or_missing_token_is_unauthenticated() {
        let (keys, clock, mut interceptor) = setup();
        let valid = token(&keys, &[]);
        for authorization in [
            None,
            Some("Basic YWRhOnB3".to_owned()),
            Some("Bearer garbage".to_owned()),
            Some(format!("Bearer {valid}x")),
        ] {
            let status = interceptor
                .call(request(authorization.as_deref()))
                .unwrap_err();
            assert_eq!(status.code(), Code::Unauthenticated, "{authorization:?}");
        }

        let bearer = format!("Bearer {valid}");
        assert!(interceptor.call(request(Some(&bearer))).is_ok());
        clock.advance(Duration::from_secs(60));
        let status = interceptor.call(request(Some(&bearer))).unwrap_err();
        assert_eq!(status.code(), Code::Unauthenticated);
    }

    #[test]
    fn claims_absent_without_interceptor() {
        let request = request(None);
        assert_eq!(
            verified_claims::<Profile, _>(&request).unwrap_err().code(),
            Code::Unauthenticated
        );
        assert_eq!(
            require_any_role::<Profile, _>(&request, &["admin"])
                .unwrap_err()
                .code(),
            Code::Unauthenticated
        );
    }

    #[test]
    fn clone_and_debug() {
        let (_, _, interceptor) = setup();
        let debug = format!("{:?}", interceptor.clone());
        assert!(debug.starts_with("BearerInterceptor"));
        assert!(!debug.contains("0123456789abcdef"));
    }
}
