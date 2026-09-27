//! HS256 JSON Web Tokens, verified against a caller-supplied time.
//!
//! `jsonwebtoken`'s own `exp`/`nbf` checks read the system clock, so they
//! are switched off: [`JwtKeys::verify`] checks the signature through the
//! library, then checks `exp`, `nbf` and `iat` itself against the
//! `now_unix_secs` it is given.
//!
//! Every token carries a `kid` header naming the key that signed it. A
//! [`JwtKeys`] signs with one key and accepts any of its verification keys,
//! which is how a key is rotated: add the new key for verification
//! everywhere, switch signing to it, and drop the old key once the last
//! token signed with it has expired. Tokens without a `kid` are refused.

use std::collections::HashSet;
use std::fmt;

use jsonwebtoken::errors::ErrorKind;
use jsonwebtoken::{Algorithm, DecodingKey, EncodingKey, Header};
use sekvent_config::Secret;
use sekvent_error::{AppError, ErrorCode};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

/// Minimum length of an HS256 secret, in bytes.
pub const MIN_SECRET_LEN: usize = 32;

/// The published secret behind [`JwtKeys::development`].
///
/// It is in the source code of a public crate: anyone can sign tokens with
/// it. It exists so a service starts on a laptop without configuration and
/// must never be used anywhere a token grants real access.
pub const DEVELOPMENT_SECRET: &str = "sekvent-development-secret-never-use-in-production";

/// The key id used by [`JwtKeys::development`].
pub const DEVELOPMENT_KID: &str = "development";

/// Default clock-skew allowance for [`Validation`], in seconds.
pub const DEFAULT_LEEWAY_SECS: u64 = 30;

/// The caller-safe message for every rejected token.
const REJECTED_MESSAGE: &str = "invalid or expired credentials";

/// Placeholder for tokens without custom claims.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NoClaims {}

/// Registered claims plus application-specific ones.
///
/// The custom claims `T` are flattened into the same JSON object, so their
/// field names must not collide with the registered ones below. `aud` is a
/// single string; a token whose `aud` is an array is rejected as malformed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Claims<T = NoClaims> {
    /// Subject: who the token is about.
    pub sub: String,
    /// Issued at, seconds since the Unix epoch.
    pub iat: u64,
    /// Expiry, seconds since the Unix epoch. The token is refused from this
    /// second on (plus leeway).
    pub exp: u64,
    /// Not before, seconds since the Unix epoch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nbf: Option<u64>,
    /// Issuer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub iss: Option<String>,
    /// Audience.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub aud: Option<String>,
    /// Token id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub jti: Option<String>,
    /// Application claims.
    #[serde(flatten)]
    pub custom: T,
}

impl<T> Claims<T> {
    /// Claims for `sub`, issued at `now_unix_secs` and valid for `ttl_secs`.
    pub fn new(sub: impl Into<String>, now_unix_secs: u64, ttl_secs: u64, custom: T) -> Self {
        Self {
            sub: sub.into(),
            iat: now_unix_secs,
            exp: now_unix_secs.saturating_add(ttl_secs),
            nbf: None,
            iss: None,
            aud: None,
            jti: None,
            custom,
        }
    }

    /// Set `nbf`.
    #[must_use]
    pub fn with_not_before(mut self, nbf_unix_secs: u64) -> Self {
        self.nbf = Some(nbf_unix_secs);
        self
    }

    /// Set `iss`.
    #[must_use]
    pub fn with_issuer(mut self, iss: impl Into<String>) -> Self {
        self.iss = Some(iss.into());
        self
    }

    /// Set `aud`.
    #[must_use]
    pub fn with_audience(mut self, aud: impl Into<String>) -> Self {
        self.aud = Some(aud.into());
        self
    }

    /// Set `jti`.
    #[must_use]
    pub fn with_jti(mut self, jti: impl Into<String>) -> Self {
        self.jti = Some(jti.into());
        self
    }
}

/// What [`JwtKeys::verify`] checks beyond the signature.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Validation {
    /// Clock skew tolerated on `exp`, `nbf` and `iat`, in seconds.
    pub leeway_secs: u64,
    /// When set, `iss` must equal it.
    pub issuer: Option<String>,
    /// When set, `aud` must equal it.
    pub audience: Option<String>,
}

impl Default for Validation {
    fn default() -> Self {
        Self {
            leeway_secs: DEFAULT_LEEWAY_SECS,
            issuer: None,
            audience: None,
        }
    }
}

impl Validation {
    /// [`DEFAULT_LEEWAY_SECS`] of leeway, any issuer, any audience.
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the leeway.
    #[must_use]
    pub fn with_leeway(mut self, secs: u64) -> Self {
        self.leeway_secs = secs;
        self
    }

    /// Require this issuer.
    #[must_use]
    pub fn with_issuer(mut self, issuer: impl Into<String>) -> Self {
        self.issuer = Some(issuer.into());
        self
    }

    /// Require this audience.
    #[must_use]
    pub fn with_audience(mut self, audience: impl Into<String>) -> Self {
        self.audience = Some(audience.into());
        self
    }
}

/// Why a token was refused.
///
/// The variant is for the server's logs. Callers only ever see
/// [`user_message`](Self::user_message), which is the same for every
/// variant: telling a client which check failed helps an attacker.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum TokenRejected {
    /// Not a well-formed JWT, an unsupported algorithm, or claims that do
    /// not deserialize.
    #[error("token is malformed")]
    Malformed,
    /// The signature does not verify with the key named by `kid`.
    #[error("token signature does not verify")]
    BadSignature,
    /// `exp` has passed.
    #[error("token has expired")]
    Expired,
    /// `nbf` or `iat` is in the future.
    #[error("token is not yet valid")]
    NotYetValid,
    /// `iss` does not match [`Validation::issuer`].
    #[error("token issuer is not accepted")]
    WrongIssuer,
    /// `aud` does not match [`Validation::audience`].
    #[error("token audience is not accepted")]
    WrongAudience,
    /// `kid` is missing or names no known verification key.
    #[error("token signing key is unknown")]
    UnknownKey,
}

impl TokenRejected {
    /// Always [`ErrorCode::Unauthenticated`].
    pub fn error_code(self) -> ErrorCode {
        match self {
            Self::Malformed
            | Self::BadSignature
            | Self::Expired
            | Self::NotYetValid
            | Self::WrongIssuer
            | Self::WrongAudience
            | Self::UnknownKey => ErrorCode::Unauthenticated,
        }
    }

    /// The single caller-safe message shared by every variant.
    pub fn user_message(self) -> &'static str {
        match self {
            Self::Malformed
            | Self::BadSignature
            | Self::Expired
            | Self::NotYetValid
            | Self::WrongIssuer
            | Self::WrongAudience
            | Self::UnknownKey => REJECTED_MESSAGE,
        }
    }
}

impl From<TokenRejected> for AppError {
    fn from(rejected: TokenRejected) -> Self {
        AppError::new(rejected.error_code(), rejected.user_message())
    }
}

/// HS256 signing and verification keys, identified by `kid`.
#[derive(Clone)]
pub struct JwtKeys {
    signing_kid: String,
    signing: EncodingKey,
    verifying: Vec<(String, DecodingKey)>,
}

impl JwtKeys {
    /// Keys that sign and verify with `secret` under `kid`.
    ///
    /// Fails when `kid` is blank or `secret` is shorter than
    /// [`MIN_SECRET_LEN`] bytes. Errors name the key id, never the secret.
    pub fn hs256(kid: impl Into<String>, secret: &Secret) -> Result<Self, AppError> {
        let kid = kid.into();
        check_key(&kid, secret)?;
        let bytes = secret.expose().as_bytes();
        Ok(Self {
            signing: EncodingKey::from_secret(bytes),
            verifying: vec![(kid.clone(), DecodingKey::from_secret(bytes))],
            signing_kid: kid,
        })
    }

    /// Also accept tokens signed with `secret` under `kid`.
    ///
    /// Fails on the same conditions as [`hs256`](Self::hs256), and when
    /// `kid` is already present.
    pub fn with_verification_key(
        mut self,
        kid: impl Into<String>,
        secret: &Secret,
    ) -> Result<Self, AppError> {
        let kid = kid.into();
        check_key(&kid, secret)?;
        if self.verifying.iter().any(|(known, _)| *known == kid) {
            return Err(AppError::invalid_argument(format!(
                "JWT key id `{kid}` is configured twice"
            )));
        }
        self.verifying
            .push((kid, DecodingKey::from_secret(secret.expose().as_bytes())));
        Ok(self)
    }

    /// Keys built from the published [`DEVELOPMENT_SECRET`].
    ///
    /// Logs a warning every time it is called. For local development only:
    /// anyone can forge tokens these keys accept.
    pub fn development() -> Self {
        tracing::warn!(
            kid = DEVELOPMENT_KID,
            "JWT KEYS USE THE PUBLISHED DEVELOPMENT SECRET: ANYONE CAN FORGE TOKENS. \
             Configure a real secret before this service handles real users."
        );
        Self::hs256(DEVELOPMENT_KID, &Secret::new(DEVELOPMENT_SECRET))
            .expect("the development secret satisfies the key rules")
    }

    /// The key id new tokens are signed with.
    pub fn signing_kid(&self) -> &str {
        &self.signing_kid
    }

    /// Every key id accepted on verification.
    pub fn verification_kids(&self) -> impl Iterator<Item = &str> {
        self.verifying.iter().map(|(kid, _)| kid.as_str())
    }

    /// Sign `claims` with the signing key, setting `kid` in the header.
    pub fn issue<T: Serialize>(&self, claims: &Claims<T>) -> Result<String, AppError> {
        let mut header = Header::new(Algorithm::HS256);
        header.kid = Some(self.signing_kid.clone());
        jsonwebtoken::encode(&header, claims, &self.signing)
            .map_err(|error| AppError::internal(error.to_string()))
    }

    /// Verify `token` at `now_unix_secs`: signature, `exp`, `nbf`, `iat`,
    /// then issuer and audience when `validation` asks for them.
    ///
    /// `exp` passes while `now < exp + leeway`; `nbf` and `iat` pass while
    /// they are at most `now + leeway`.
    pub fn verify<T: DeserializeOwned>(
        &self,
        token: &str,
        now_unix_secs: u64,
        validation: &Validation,
    ) -> Result<Claims<T>, TokenRejected> {
        let header = jsonwebtoken::decode_header(token).map_err(|_| TokenRejected::Malformed)?;
        if header.alg != Algorithm::HS256 {
            return Err(TokenRejected::BadSignature);
        }
        let kid = header.kid.ok_or(TokenRejected::UnknownKey)?;
        let key = self
            .verifying
            .iter()
            .find_map(|(known, key)| (*known == kid).then_some(key))
            .ok_or(TokenRejected::UnknownKey)?;
        let claims = jsonwebtoken::decode::<Claims<T>>(token, key, &signature_only())
            .map_err(|error| match error.kind() {
                ErrorKind::InvalidSignature | ErrorKind::InvalidAlgorithm => {
                    TokenRejected::BadSignature
                }
                _ => TokenRejected::Malformed,
            })?
            .claims;
        check_times(&claims, now_unix_secs, validation.leeway_secs)?;
        if let Some(issuer) = &validation.issuer
            && claims.iss.as_deref() != Some(issuer.as_str())
        {
            return Err(TokenRejected::WrongIssuer);
        }
        if let Some(audience) = &validation.audience
            && claims.aud.as_deref() != Some(audience.as_str())
        {
            return Err(TokenRejected::WrongAudience);
        }
        Ok(claims)
    }
}

impl fmt::Debug for JwtKeys {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("JwtKeys")
            .field("signing_kid", &self.signing_kid)
            .field(
                "verification_kids",
                &self.verification_kids().collect::<Vec<_>>(),
            )
            .finish_non_exhaustive()
    }
}

fn check_key(kid: &str, secret: &Secret) -> Result<(), AppError> {
    if kid.trim().is_empty() {
        return Err(AppError::invalid_argument("JWT key id must not be blank"));
    }
    if secret.expose().len() < MIN_SECRET_LEN {
        return Err(AppError::invalid_argument(format!(
            "JWT key `{kid}` is too short: HS256 needs at least {MIN_SECRET_LEN} bytes"
        )));
    }
    Ok(())
}

/// Library validation reduced to the signature: no clock, no required claims.
fn signature_only() -> jsonwebtoken::Validation {
    let mut validation = jsonwebtoken::Validation::new(Algorithm::HS256);
    validation.required_spec_claims = HashSet::new();
    validation.validate_exp = false;
    validation.validate_nbf = false;
    validation.validate_aud = false;
    validation.leeway = 0;
    validation
}

fn check_times<T>(claims: &Claims<T>, now: u64, leeway: u64) -> Result<(), TokenRejected> {
    if now >= claims.exp.saturating_add(leeway) {
        return Err(TokenRejected::Expired);
    }
    let latest_start = now.saturating_add(leeway);
    if claims.nbf.is_some_and(|nbf| nbf > latest_start) || claims.iat > latest_start {
        return Err(TokenRejected::NotYetValid);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET_A: &str = "0123456789abcdef0123456789abcdef";
    const SECRET_B: &str = "fedcba9876543210fedcba9876543210";

    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    struct Profile {
        name: String,
        roles: Vec<String>,
    }

    fn keys(kid: &str, secret: &str) -> JwtKeys {
        JwtKeys::hs256(kid, &Secret::new(secret)).unwrap()
    }

    fn strict() -> Validation {
        Validation::new().with_leeway(0)
    }

    fn b64url(bytes: &[u8]) -> String {
        const ALPHABET: &[u8; 64] =
            b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
        let mut out = String::new();
        for chunk in bytes.chunks(3) {
            let padded = [
                chunk[0],
                chunk.get(1).copied().unwrap_or(0),
                chunk.get(2).copied().unwrap_or(0),
            ];
            let n =
                (u32::from(padded[0]) << 16) | (u32::from(padded[1]) << 8) | u32::from(padded[2]);
            for i in 0..=chunk.len() {
                out.push(char::from(ALPHABET[((n >> (18 - 6 * i)) & 63) as usize]));
            }
        }
        out
    }

    fn segments(token: &str) -> Vec<String> {
        token.split('.').map(str::to_owned).collect()
    }

    #[test]
    fn round_trip_with_custom_claims() {
        let keys = keys("k1", SECRET_A);
        let claims = Claims::new(
            "user-1",
            1_000,
            600,
            Profile {
                name: "Ada".into(),
                roles: vec!["admin".into()],
            },
        )
        .with_issuer("orders")
        .with_audience("web")
        .with_jti("t-1");
        let token = keys.issue(&claims).unwrap();
        let header = jsonwebtoken::decode_header(&token).unwrap();
        assert_eq!(header.kid.as_deref(), Some("k1"));
        assert_eq!(header.alg, Algorithm::HS256);

        let verified: Claims<Profile> = keys
            .verify(
                &token,
                1_100,
                &Validation::new().with_issuer("orders").with_audience("web"),
            )
            .unwrap();
        assert_eq!(verified, claims);
    }

    #[test]
    fn round_trip_without_custom_claims() {
        let keys = keys("k1", SECRET_A);
        let claims = Claims::new("user-1", 1_000, 60, NoClaims {});
        let token = keys.issue(&claims).unwrap();
        let payload = segments(&token)[1].clone();
        assert!(!payload.is_empty());
        let verified: Claims = keys.verify(&token, 1_000, &strict()).unwrap();
        assert_eq!(verified, claims);
    }

    #[test]
    fn expiry_boundaries() {
        let keys = keys("k1", SECRET_A);
        let token = keys
            .issue(&Claims::new("u", 1_000, 1_000, NoClaims {}))
            .unwrap();
        let check = |now, leeway| {
            keys.verify::<NoClaims>(&token, now, &Validation::new().with_leeway(leeway))
                .map(|_| ())
        };
        assert_eq!(check(1_999, 0), Ok(()));
        assert_eq!(check(2_000, 0), Err(TokenRejected::Expired));
        assert_eq!(check(2_009, 10), Ok(()));
        assert_eq!(check(2_010, 10), Err(TokenRejected::Expired));
        assert_eq!(check(u64::MAX, u64::MAX), Err(TokenRejected::Expired));
    }

    #[test]
    fn not_before_boundaries() {
        let keys = keys("k1", SECRET_A);
        let token = keys
            .issue(&Claims::new("u", 1_000, 10_000, NoClaims {}).with_not_before(1_500))
            .unwrap();
        let check = |now, leeway| {
            keys.verify::<NoClaims>(&token, now, &Validation::new().with_leeway(leeway))
                .map(|_| ())
        };
        assert_eq!(check(1_499, 0), Err(TokenRejected::NotYetValid));
        assert_eq!(check(1_500, 0), Ok(()));
        assert_eq!(check(1_489, 10), Err(TokenRejected::NotYetValid));
        assert_eq!(check(1_490, 10), Ok(()));
    }

    #[test]
    fn issued_in_the_future_is_not_yet_valid() {
        let keys = keys("k1", SECRET_A);
        let token = keys
            .issue(&Claims::new("u", 1_000, 600, NoClaims {}))
            .unwrap();
        assert_eq!(
            keys.verify::<NoClaims>(&token, 999, &strict()).unwrap_err(),
            TokenRejected::NotYetValid
        );
        assert!(
            keys.verify::<NoClaims>(&token, 990, &Validation::new().with_leeway(10))
                .is_ok()
        );
        assert_eq!(
            keys.verify::<NoClaims>(&token, 989, &Validation::new().with_leeway(10))
                .unwrap_err(),
            TokenRejected::NotYetValid
        );
    }

    #[test]
    fn issuer_and_audience() {
        let keys = keys("k1", SECRET_A);
        let bare = keys
            .issue(&Claims::new("u", 1_000, 600, NoClaims {}))
            .unwrap();
        let tagged = keys
            .issue(
                &Claims::new("u", 1_000, 600, NoClaims {})
                    .with_issuer("orders")
                    .with_audience("web"),
            )
            .unwrap();

        assert!(keys.verify::<NoClaims>(&tagged, 1_000, &strict()).is_ok());
        let wants_issuer = strict().with_issuer("billing");
        assert_eq!(
            keys.verify::<NoClaims>(&tagged, 1_000, &wants_issuer)
                .unwrap_err(),
            TokenRejected::WrongIssuer
        );
        assert_eq!(
            keys.verify::<NoClaims>(&bare, 1_000, &wants_issuer)
                .unwrap_err(),
            TokenRejected::WrongIssuer
        );
        let wants_audience = strict().with_audience("mobile");
        assert_eq!(
            keys.verify::<NoClaims>(&tagged, 1_000, &wants_audience)
                .unwrap_err(),
            TokenRejected::WrongAudience
        );
        assert_eq!(
            keys.verify::<NoClaims>(&bare, 1_000, &wants_audience)
                .unwrap_err(),
            TokenRejected::WrongAudience
        );
    }

    #[test]
    fn rotation_by_kid() {
        let old = keys("k1", SECRET_A);
        let rotated = JwtKeys::hs256("k2", &Secret::new(SECRET_B))
            .unwrap()
            .with_verification_key("k1", &Secret::new(SECRET_A))
            .unwrap();
        assert_eq!(rotated.signing_kid(), "k2");
        assert_eq!(
            rotated.verification_kids().collect::<Vec<_>>(),
            ["k2", "k1"]
        );

        let claims = Claims::new("u", 1_000, 600, NoClaims {});
        let old_token = old.issue(&claims).unwrap();
        let new_token = rotated.issue(&claims).unwrap();

        assert!(
            rotated
                .verify::<NoClaims>(&old_token, 1_000, &strict())
                .is_ok()
        );
        assert!(
            rotated
                .verify::<NoClaims>(&new_token, 1_000, &strict())
                .is_ok()
        );
        assert_eq!(
            old.verify::<NoClaims>(&new_token, 1_000, &strict())
                .unwrap_err(),
            TokenRejected::UnknownKey
        );
    }

    #[test]
    fn same_kid_other_secret_is_bad_signature() {
        let token = keys("k1", SECRET_A)
            .issue(&Claims::new("u", 1_000, 600, NoClaims {}))
            .unwrap();
        assert_eq!(
            keys("k1", SECRET_B)
                .verify::<NoClaims>(&token, 1_000, &strict())
                .unwrap_err(),
            TokenRejected::BadSignature
        );
    }

    #[test]
    fn missing_kid_is_unknown_key() {
        let token = jsonwebtoken::encode(
            &Header::new(Algorithm::HS256),
            &Claims::new("u", 1_000, 600, NoClaims {}),
            &EncodingKey::from_secret(SECRET_A.as_bytes()),
        )
        .unwrap();
        assert_eq!(
            keys("k1", SECRET_A)
                .verify::<NoClaims>(&token, 1_000, &strict())
                .unwrap_err(),
            TokenRejected::UnknownKey
        );
    }

    #[test]
    fn tampered_payload_and_signature() {
        let keys = keys("k1", SECRET_A);
        let user = keys
            .issue(&Claims::new("user", 1_000, 600, NoClaims {}))
            .unwrap();
        let admin = keys
            .issue(&Claims::new("admin", 1_000, 600, NoClaims {}))
            .unwrap();
        let (user, admin) = (segments(&user), segments(&admin));

        let spliced = format!("{}.{}.{}", user[0], admin[1], user[2]);
        assert_eq!(
            keys.verify::<NoClaims>(&spliced, 1_000, &strict())
                .unwrap_err(),
            TokenRejected::BadSignature
        );

        let mut signature = user[2].clone();
        let first = if signature.starts_with('A') { "B" } else { "A" };
        signature.replace_range(0..1, first);
        let flipped = format!("{}.{}.{signature}", user[0], user[1]);
        assert_eq!(
            keys.verify::<NoClaims>(&flipped, 1_000, &strict())
                .unwrap_err(),
            TokenRejected::BadSignature
        );

        let unsigned = format!("{}.{}.", user[0], user[1]);
        assert!(
            keys.verify::<NoClaims>(&unsigned, 1_000, &strict())
                .is_err()
        );
    }

    #[test]
    fn alg_none_is_rejected() {
        let keys = keys("k1", SECRET_A);
        let payload = b64url(
            serde_json::json!({"sub": "admin", "iat": 1_000, "exp": 2_000})
                .to_string()
                .as_bytes(),
        );
        for header in [
            serde_json::json!({"alg": "none", "typ": "JWT", "kid": "k1"}),
            serde_json::json!({"alg": "None", "kid": "k1"}),
        ] {
            let header = b64url(header.to_string().as_bytes());
            for token in [
                format!("{header}.{payload}."),
                format!("{header}.{payload}"),
            ] {
                assert_eq!(
                    keys.verify::<NoClaims>(&token, 1_000, &strict())
                        .unwrap_err(),
                    TokenRejected::Malformed,
                    "{token}"
                );
            }
        }
    }

    #[test]
    fn other_hmac_algorithm_is_bad_signature() {
        let mut header = Header::new(Algorithm::HS512);
        header.kid = Some("k1".into());
        let token = jsonwebtoken::encode(
            &header,
            &Claims::new("u", 1_000, 600, NoClaims {}),
            &EncodingKey::from_secret(SECRET_A.as_bytes()),
        )
        .unwrap();
        assert_eq!(
            keys("k1", SECRET_A)
                .verify::<NoClaims>(&token, 1_000, &strict())
                .unwrap_err(),
            TokenRejected::BadSignature
        );
    }

    #[test]
    fn malformed_tokens() {
        let keys = keys("k1", SECRET_A);
        for token in ["", "abc", "a.b", "a.b.c", "..", "!!.??.**"] {
            assert_eq!(
                keys.verify::<NoClaims>(token, 1_000, &strict())
                    .unwrap_err(),
                TokenRejected::Malformed,
                "{token}"
            );
        }
    }

    #[test]
    fn signed_but_malformed_claims() {
        let mut header = Header::new(Algorithm::HS256);
        header.kid = Some("k1".into());
        let key = EncodingKey::from_secret(SECRET_A.as_bytes());
        let keys = keys("k1", SECRET_A);
        for claims in [
            serde_json::json!({"sub": "u", "iat": 1_000}),
            serde_json::json!({"sub": "u", "iat": 1_000, "exp": "later"}),
            serde_json::json!({"sub": "u", "iat": 1_000, "exp": 2_000, "aud": ["a", "b"]}),
        ] {
            let token = jsonwebtoken::encode(&header, &claims, &key).unwrap();
            assert_eq!(
                keys.verify::<NoClaims>(&token, 1_000, &strict())
                    .unwrap_err(),
                TokenRejected::Malformed
            );
        }
        let token = jsonwebtoken::encode(
            &header,
            &serde_json::json!({"sub": "u", "iat": 1_000, "exp": 2_000}),
            &key,
        )
        .unwrap();
        assert_eq!(
            keys.verify::<Profile>(&token, 1_000, &strict())
                .unwrap_err(),
            TokenRejected::Malformed
        );
    }

    #[test]
    fn key_rules() {
        let short = JwtKeys::hs256("k1", &Secret::new("too-short")).unwrap_err();
        assert_eq!(short.code(), ErrorCode::InvalidArgument);
        assert!(short.message().contains("k1"));
        assert!(!short.message().contains("too-short"));

        assert!(JwtKeys::hs256(" ", &Secret::new(SECRET_A)).is_err());
        assert!(
            keys("k1", SECRET_A)
                .with_verification_key("k1", &Secret::new(SECRET_B))
                .is_err()
        );
        assert!(
            keys("k1", SECRET_A)
                .with_verification_key("k2", &Secret::new("short"))
                .is_err()
        );
    }

    #[test]
    fn debug_shows_kids_only() {
        let debug = format!("{:?}", keys("k1", SECRET_A));
        assert!(debug.contains("k1"));
        assert!(!debug.contains(SECRET_A));
    }

    #[test]
    fn development_keys_work() {
        let keys = JwtKeys::development();
        assert_eq!(keys.signing_kid(), DEVELOPMENT_KID);
        assert!(DEVELOPMENT_SECRET.len() >= MIN_SECRET_LEN);
        let token = keys
            .issue(&Claims::new("dev", 1_000, 60, NoClaims {}))
            .unwrap();
        assert!(keys.verify::<NoClaims>(&token, 1_000, &strict()).is_ok());
    }

    #[test]
    fn rejections_share_one_caller_message() {
        let all = [
            TokenRejected::Malformed,
            TokenRejected::BadSignature,
            TokenRejected::Expired,
            TokenRejected::NotYetValid,
            TokenRejected::WrongIssuer,
            TokenRejected::WrongAudience,
            TokenRejected::UnknownKey,
        ];
        let displays: HashSet<String> = all.iter().map(ToString::to_string).collect();
        assert_eq!(displays.len(), all.len());
        for rejected in all {
            assert_eq!(rejected.error_code(), ErrorCode::Unauthenticated);
            assert_eq!(rejected.user_message(), REJECTED_MESSAGE);
            let error = AppError::from(rejected);
            assert_eq!(error.code(), ErrorCode::Unauthenticated);
            assert_eq!(error.message(), REJECTED_MESSAGE);
        }
    }

    #[test]
    fn validation_defaults() {
        let validation = Validation::default();
        assert_eq!(validation.leeway_secs, DEFAULT_LEEWAY_SECS);
        assert_eq!(validation.issuer, None);
        assert_eq!(validation.audience, None);
    }
}
