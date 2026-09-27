//! State shared by the axum and tonic adapters.

use std::fmt;
use std::sync::Arc;

use sekvent_context::Clock;
use sekvent_error::AppError;
use serde::de::DeserializeOwned;

use crate::jwt::{Claims, JwtKeys, TokenRejected, Validation};

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
}
