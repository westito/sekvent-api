//! axum extractors: [`Bearer`] verifies `Authorization: Bearer <jwt>`,
//! [`RequireRole`] additionally demands a role.
//!
//! Both read a [`BearerAuth`] from the router state through [`FromRef`]:
//!
//! ```ignore
//! let auth = BearerAuth::new(keys, Validation::new(), Arc::new(SystemClock));
//! let app = Router::new().route("/me", get(me)).with_state(auth);
//!
//! async fn me(Bearer(claims): Bearer<Profile>) -> String {
//!     claims.sub
//! }
//! ```
//!
//! Rejections are [`AppError`]s: `UNAUTHENTICATED` for a missing or invalid
//! token, `PERMISSION_DENIED` for a missing role.

use std::marker::PhantomData;

use ::axum::extract::{FromRef, FromRequestParts};
use ::http::header::AUTHORIZATION;
use ::http::request::Parts;
use sekvent_error::AppError;
use serde::de::DeserializeOwned;

use crate::BearerAuth;
use crate::jwt::{Claims, NoClaims};
use crate::roles::{HasRoles, require_any_role};

/// The verified claims of the request's bearer token.
#[derive(Debug, Clone)]
pub struct Bearer<T = NoClaims>(pub Claims<T>);

impl<S, T> FromRequestParts<S> for Bearer<T>
where
    S: Send + Sync,
    BearerAuth: FromRef<S>,
    T: DeserializeOwned + Send,
{
    type Rejection = AppError;

    fn from_request_parts(
        parts: &mut Parts,
        state: &S,
    ) -> impl Future<Output = Result<Self, Self::Rejection>> + Send {
        let auth = BearerAuth::from_ref(state);
        let header = parts
            .headers
            .get(AUTHORIZATION)
            .and_then(|value| value.to_str().ok());
        std::future::ready(auth.authenticate_header(header).map(Bearer))
    }
}

/// A fixed set of roles, any one of which admits the caller.
pub trait RoleSet {
    /// The accepted roles. An empty list admits nobody.
    const ROLES: &'static [&'static str];
}

/// Like [`Bearer`], and the claims grant at least one of `R::ROLES`.
///
/// ```ignore
/// struct Admins;
/// impl RoleSet for Admins {
///     const ROLES: &'static [&'static str] = &["admin"];
/// }
///
/// async fn purge(guard: RequireRole<Admins, Profile>) { /* … */ }
/// ```
pub struct RequireRole<R, T = NoClaims> {
    /// The verified claims.
    pub claims: Claims<T>,
    roles: PhantomData<fn() -> R>,
}

impl<R, T> RequireRole<R, T> {
    /// The verified claims.
    pub fn into_claims(self) -> Claims<T> {
        self.claims
    }
}

impl<R, T: std::fmt::Debug> std::fmt::Debug for RequireRole<R, T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RequireRole")
            .field("claims", &self.claims)
            .finish_non_exhaustive()
    }
}

impl<S, R, T> FromRequestParts<S> for RequireRole<R, T>
where
    S: Send + Sync,
    BearerAuth: FromRef<S>,
    R: RoleSet,
    T: DeserializeOwned + HasRoles + Send,
{
    type Rejection = AppError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let Bearer(claims) = Bearer::<T>::from_request_parts(parts, state).await?;
        require_any_role(&claims, R::ROLES)?;
        Ok(Self {
            claims,
            roles: PhantomData,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::{Duration, SystemTime};

    use ::axum::Router;
    use ::axum::body::{Body, to_bytes};
    use ::axum::routing::get;
    use ::http::{Request, StatusCode};
    use sekvent_config::Secret;
    use sekvent_context::ManualClock;
    use serde::{Deserialize, Serialize};
    use tower::ServiceExt;

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

    struct Admins;
    impl RoleSet for Admins {
        const ROLES: &'static [&'static str] = &["admin"];
    }

    async fn me(Bearer(claims): Bearer<Profile>) -> String {
        claims.sub
    }

    async fn anyone(Bearer(claims): Bearer) -> String {
        claims.sub
    }

    async fn admin(guard: RequireRole<Admins, Profile>) -> String {
        assert!(format!("{guard:?}").starts_with("RequireRole"));
        guard.into_claims().sub
    }

    struct Fixture {
        keys: JwtKeys,
        clock: ManualClock,
        app: Router,
    }

    fn fixture() -> Fixture {
        let keys = JwtKeys::hs256("k1", &Secret::new("0123456789abcdef0123456789abcdef")).unwrap();
        let clock = ManualClock::new(SystemTime::UNIX_EPOCH + Duration::from_secs(1_000));
        let auth = BearerAuth::new(
            keys.clone(),
            Validation::new().with_leeway(0),
            Arc::new(clock.clone()),
        );
        let app = Router::new()
            .route("/me", get(me))
            .route("/anyone", get(anyone))
            .route("/admin", get(admin))
            .with_state(auth);
        Fixture { keys, clock, app }
    }

    fn token(keys: &JwtKeys, sub: &str, roles: &[&str]) -> String {
        keys.issue(&Claims::new(
            sub,
            1_000,
            60,
            Profile {
                roles: roles.iter().map(|role| (*role).to_owned()).collect(),
            },
        ))
        .unwrap()
    }

    async fn call(app: &Router, path: &str, authorization: Option<&str>) -> (StatusCode, String) {
        let mut request = Request::builder().uri(path);
        if let Some(value) = authorization {
            request = request.header(AUTHORIZATION, value);
        }
        let response = app
            .clone()
            .oneshot(request.body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let body = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
        (status, String::from_utf8(body.to_vec()).unwrap())
    }

    #[tokio::test]
    async fn valid_token_is_extracted() {
        let fixture = fixture();
        let bearer = format!("Bearer {}", token(&fixture.keys, "ada", &[]));
        assert_eq!(
            call(&fixture.app, "/me", Some(&bearer)).await,
            (StatusCode::OK, "ada".to_owned())
        );
        assert_eq!(
            call(&fixture.app, "/anyone", Some(&bearer)).await,
            (StatusCode::OK, "ada".to_owned())
        );
    }

    #[tokio::test]
    async fn missing_or_invalid_token_is_unauthenticated() {
        let fixture = fixture();
        let valid = token(&fixture.keys, "ada", &[]);
        for authorization in [
            None,
            Some("Basic YWRhOnB3".to_owned()),
            Some("Bearer not-a-jwt".to_owned()),
            Some(format!("Bearer {valid}x")),
        ] {
            let (status, _) = call(&fixture.app, "/me", authorization.as_deref()).await;
            assert_eq!(status, StatusCode::UNAUTHORIZED, "{authorization:?}");
        }
    }

    #[tokio::test]
    async fn expiry_follows_the_injected_clock() {
        let fixture = fixture();
        let bearer = format!("Bearer {}", token(&fixture.keys, "ada", &[]));
        assert_eq!(
            call(&fixture.app, "/me", Some(&bearer)).await.0,
            StatusCode::OK
        );
        fixture.clock.advance(Duration::from_secs(60));
        assert_eq!(
            call(&fixture.app, "/me", Some(&bearer)).await.0,
            StatusCode::UNAUTHORIZED
        );
    }

    #[tokio::test]
    async fn role_guard() {
        let fixture = fixture();
        let admin_token = format!("Bearer {}", token(&fixture.keys, "root", &["admin"]));
        assert_eq!(
            call(&fixture.app, "/admin", Some(&admin_token)).await,
            (StatusCode::OK, "root".to_owned())
        );

        let reader = format!("Bearer {}", token(&fixture.keys, "ada", &["reader"]));
        assert_eq!(
            call(&fixture.app, "/admin", Some(&reader)).await.0,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            call(&fixture.app, "/admin", None).await.0,
            StatusCode::UNAUTHORIZED
        );
    }
}
