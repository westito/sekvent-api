//! State shared by the axum and tonic adapters.

use std::fmt;
use std::sync::Arc;

use sekvent_context::{Clock, EndUser};
use sekvent_error::AppError;
use serde::de::DeserializeOwned;

use crate::jwt::{Claims, JwtKeys, NoClaims, TokenRejected, Validation};
use crate::roles::HasRoles;

/// Custom claims that name an end user's tenant and roles, for
/// [`BearerAuth::end_user`]. Both default to none.
pub trait EndUserClaims {
    /// The tenant the end user acts in.
    fn tenant(&self) -> Option<&str> {
        None
    }

    /// The roles granted to the end user.
    fn roles(&self) -> &[String] {
        &[]
    }
}

impl EndUserClaims for NoClaims {}

impl HasRoles for EndUser {
    fn has_role(&self, role: &str) -> bool {
        EndUser::has_role(self, role)
    }
}

/// What a server needs to verify bearer tokens: keys, validation rules and
/// a clock. Cheap to clone.
///
/// For axum, make it reachable from the router state through `FromRef`;
/// for tonic, hand it to `tonic::BearerInterceptor::new`.
#[derive(Clone)]
pub struct BearerAuth {
    keys: Arc<JwtKeys>,
    validation: Arc<Validation>,
    clock: Arc<dyn Clock>,
}

impl BearerAuth {
    /// Verify with `keys` and `validation`, reading the time from `clock`.
    pub fn new(keys: JwtKeys, validation: Validation, clock: Arc<dyn Clock>) -> Self {
        Self {
            keys: Arc::new(keys),
            validation: Arc::new(validation),
            clock,
        }
    }

    /// The keys, e.g. to issue tokens from the same server.
    pub fn keys(&self) -> &JwtKeys {
        &self.keys
    }

    /// The current time according to the injected clock, in Unix seconds.
    pub fn now_unix_secs(&self) -> u64 {
        self.clock.now_unix_millis() / 1_000
    }

    /// Verify a raw token at the clock's current time.
    pub fn verify<T: DeserializeOwned>(&self, token: &str) -> Result<Claims<T>, TokenRejected> {
        self.keys
            .verify(token, self.now_unix_secs(), &self.validation)
    }

    /// Verify the bearer token of the `authorization` header in `headers`.
    /// Every failure (no header, another scheme, a bad token) is
    /// `UNAUTHENTICATED` with the same caller-safe message; the reason is
    /// logged at `debug`, the token never.
    pub fn verify_headers<T: DeserializeOwned>(
        &self,
        headers: &http::HeaderMap,
    ) -> Result<Claims<T>, AppError> {
        let header = headers
            .get(http::header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok());
        self.authenticate_header(header)
    }

    /// The end user a valid bearer token in `headers` names: `sub` is the
    /// subject, tenant and roles come from the custom claims. Fails as
    /// [`verify_headers`](Self::verify_headers) does.
    ///
    /// Made to back a component end-user authenticator:
    /// `builder.end_user_authenticator(move |request| auth.end_user::<Profile>(&request.headers))`.
    pub fn end_user<T: DeserializeOwned + EndUserClaims>(
        &self,
        headers: &http::HeaderMap,
    ) -> Result<EndUser, AppError> {
        let claims = self.verify_headers::<T>(headers)?;
        let mut user = EndUser::new(claims.sub).with_roles(claims.custom.roles().iter().cloned());
        if let Some(tenant) = claims.custom.tenant() {
            user = user.with_tenant(tenant);
        }
        Ok(user)
    }

    /// Verify the value of an `Authorization` header, if any. Every failure
    /// is `UNAUTHENTICATED` with the same caller-safe message.
    pub(crate) fn authenticate_header<T: DeserializeOwned>(
        &self,
        header: Option<&str>,
    ) -> Result<Claims<T>, AppError> {
        let Some(token) = header.and_then(bearer_token) else {
            tracing::debug!("request without a bearer token");
            return Err(TokenRejected::Malformed.into());
        };
        self.verify(token).map_err(|rejected| {
            tracing::debug!(reason = %rejected, "bearer token rejected");
            AppError::from(rejected)
        })
    }
}

impl fmt::Debug for BearerAuth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BearerAuth")
            .field("keys", &self.keys)
            .field("validation", &self.validation)
            .finish_non_exhaustive()
    }
}

/// The token of an `Authorization: Bearer <token>` value. The scheme is
/// case-insensitive; the token is taken verbatim, never trimmed.
fn bearer_token(value: &str) -> Option<&str> {
    let (scheme, token) = value.split_once(' ')?;
    (scheme.eq_ignore_ascii_case("bearer") && !token.is_empty()).then_some(token)
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, SystemTime};

    use sekvent_config::Secret;
    use sekvent_context::ManualClock;
    use sekvent_error::ErrorCode;

    use super::*;
    use crate::jwt::NoClaims;

    #[test]
    fn bearer_token_parsing() {
        assert_eq!(bearer_token("Bearer abc"), Some("abc"));
        assert_eq!(bearer_token("bearer abc"), Some("abc"));
        assert_eq!(bearer_token("BEARER abc"), Some("abc"));
        assert_eq!(bearer_token("Bearer  abc"), Some(" abc"));
        assert_eq!(bearer_token("Bearer "), None);
        assert_eq!(bearer_token("Bearer"), None);
        assert_eq!(bearer_token("Basic abc"), None);
        assert_eq!(bearer_token(""), None);
    }

    #[test]
    fn clock_drives_verification() {
        let keys = JwtKeys::hs256("k1", &Secret::new("0123456789abcdef0123456789abcdef")).unwrap();
        let token = keys
            .issue(&Claims::new("u", 1_000, 60, NoClaims {}))
            .unwrap();
        let clock = ManualClock::new(SystemTime::UNIX_EPOCH + Duration::from_secs(1_000));
        let auth = BearerAuth::new(
            keys,
            Validation::new().with_leeway(0),
            Arc::new(clock.clone()),
        );
        assert_eq!(auth.now_unix_secs(), 1_000);
        assert_eq!(auth.keys().signing_kid(), "k1");
        assert!(auth.verify::<NoClaims>(&token).is_ok());

        let header = format!("Bearer {token}");
        assert!(auth.authenticate_header::<NoClaims>(Some(&header)).is_ok());

        clock.advance(Duration::from_secs(60));
        assert_eq!(
            auth.verify::<NoClaims>(&token).unwrap_err(),
            TokenRejected::Expired
        );
        let error = auth
            .authenticate_header::<NoClaims>(Some(&header))
            .unwrap_err();
        assert_eq!(error.code(), ErrorCode::Unauthenticated);

        let missing = auth.authenticate_header::<NoClaims>(None).unwrap_err();
        assert_eq!(missing.code(), ErrorCode::Unauthenticated);
        assert_eq!(missing.message(), error.message());

        let debug = format!("{auth:?}");
        assert!(debug.contains("k1"));
        assert!(!debug.contains("0123456789abcdef"));
    }

    #[derive(Debug, serde::Serialize, serde::Deserialize)]
    struct Profile {
        tenant: Option<String>,
        roles: Vec<String>,
    }

    impl EndUserClaims for Profile {
        fn tenant(&self) -> Option<&str> {
            self.tenant.as_deref()
        }

        fn roles(&self) -> &[String] {
            &self.roles
        }
    }

    fn headers(value: &str) -> http::HeaderMap {
        let mut headers = http::HeaderMap::new();
        headers.insert(http::header::AUTHORIZATION, value.parse().unwrap());
        headers
    }

    #[test]
    fn end_users_come_from_verified_headers() {
        let keys = JwtKeys::hs256("k1", &Secret::new("0123456789abcdef0123456789abcdef")).unwrap();
        let profile = Profile {
            tenant: Some("tenant-a".to_owned()),
            roles: vec!["admin".to_owned()],
        };
        let token = keys
            .issue(&Claims::new("user-7", 1_000, 60, profile))
            .unwrap();
        let plain = keys
            .issue(&Claims::new("user-8", 1_000, 60, NoClaims {}))
            .unwrap();
        let clock = ManualClock::new(SystemTime::UNIX_EPOCH + Duration::from_secs(1_000));
        let auth = BearerAuth::new(keys, Validation::new(), Arc::new(clock));

        let user = auth
            .end_user::<Profile>(&headers(&format!("Bearer {token}")))
            .unwrap();
        assert_eq!(user.subject(), "user-7");
        assert_eq!(user.tenant(), Some("tenant-a"));
        assert!(HasRoles::has_role(&user, "admin"));
        assert!(crate::require_any_role(&user, &["admin"]).is_ok());
        assert!(crate::require_any_role(&user, &["owner"]).is_err());

        let user = auth
            .end_user::<NoClaims>(&headers(&format!("bearer {plain}")))
            .unwrap();
        assert_eq!(user.subject(), "user-8");
        assert_eq!(user.tenant(), None);
        assert!(user.roles().is_empty());

        let claims = auth
            .verify_headers::<NoClaims>(&headers(&format!("Bearer {plain}")))
            .unwrap();
        assert_eq!(claims.sub, "user-8");

        let missing = auth
            .end_user::<NoClaims>(&http::HeaderMap::new())
            .unwrap_err();
        let wrong = auth
            .end_user::<NoClaims>(&headers("Bearer not-a-token"))
            .unwrap_err();
        let basic = auth
            .end_user::<NoClaims>(&headers("Basic dXNlcjpwYXNz"))
            .unwrap_err();
        for error in [&missing, &wrong, &basic] {
            assert_eq!(error.code(), ErrorCode::Unauthenticated);
            assert_eq!(error.message(), missing.message());
        }
    }
}
