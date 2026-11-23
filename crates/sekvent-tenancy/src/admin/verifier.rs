//! The admin token verifier: `EdDSA`, a short lifetime, the issuer, the audience, one operation and
//! at most one tenant.

use std::fmt;
use std::marker::PhantomData;
use std::time::{Duration, SystemTime};

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use sekvent_auth::{Claims, EdDsaVerificationKeys, TokenRejected, Validation};
use sekvent_config::Secret;
use sekvent_error::AppError;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use uuid::Uuid;

use super::claims::{AdminClaims, MAX_LIFETIME, NoExtra};
use crate::{ErrorDomains, TenantId, is_key_id};

/// The length of a raw Ed25519 public key.
const KEY_LEN: usize = 32;
/// The characters of [`KEY_LEN`] bytes in unpadded base64url.
const KEY_TEXT_LEN: usize = 43;
/// The length of the lower-case hyphenated form of a UUID.
const UUID_TEXT_LEN: usize = 36;

/// The product's rule on `sub`.
type SubjectCheck = Box<dyn Fn(&str) -> bool + Send + Sync>;

/// The claims of an admin token beside the registered ones; the product's `X` flattened in.
#[derive(Deserialize)]
struct AdminBody<X> {
    #[serde(default)]
    tid: Option<String>,
    op: String,
    #[serde(flatten)]
    extra: X,
}

/// Why a token was refused: for the `debug` log only, never for the caller. Each text is fixed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Cause {
    Refusing,
    BeforeEpoch,
    Token(TokenRejected),
    NullClaim,
    Lifetime,
    Subject,
    TokenId,
    Tenant,
    Operation,
}

impl fmt::Display for Cause {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let text = match self {
            Self::Refusing => "this verifier accepts no admin token",
            Self::BeforeEpoch => "the clock is before the Unix epoch",
            Self::Token(rejected) => return fmt::Display::fmt(rejected, f),
            Self::NullClaim => "a claim is null",
            Self::Lifetime => "the token's lifetime is not between 1 and 60 seconds",
            Self::Subject => "the subject is empty or refused",
            Self::TokenId => "the token id is missing or not a lower-case hyphenated UUID",
            Self::Tenant => "the tenant claim is not a tenant id",
            Self::Operation => "the token was minted for another operation",
        };
        f.write_str(text)
    }
}

/// Verifies the admin tokens of one product instance. `Debug` prints the key ids and whether it
/// refuses everything, never a key.
pub struct Verifier<X = NoExtra> {
    /// The keys and the validation; `None` refuses every token.
    keys: Option<(EdDsaVerificationKeys, Validation)>,
    subject_check: Option<SubjectCheck>,
    domains: ErrorDomains,
    _extra: PhantomData<fn() -> X>,
}

impl<X> fmt::Debug for Verifier<X> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let kids: Vec<&str> = self
            .keys
            .as_ref()
            .map(|(keys, _)| keys.kids().collect())
            .unwrap_or_default();
        f.debug_struct("Verifier")
            .field("kids", &kids)
            .field("refusing", &self.keys.is_none())
            .finish_non_exhaustive()
    }
}

impl<X: DeserializeOwned + Send + Sync + 'static> Verifier<X> {
    /// Accepts tokens of `issuer` for `audience`, signed by one of `keys`.
    pub fn new(issuer: &str, audience: &str, keys: EdDsaVerificationKeys) -> Self {
        let validation = Validation::new()
            .with_leeway(0)
            .with_issuer(issuer)
            .with_audience(audience);
        Self {
            keys: Some((keys, validation)),
            subject_check: None,
            domains: ErrorDomains::new(),
            _extra: PhantomData,
        }
    }

    /// Accepts nothing: what a process that serves a fixed scope installs.
    pub fn refusing() -> Self {
        Self {
            keys: None,
            subject_check: None,
            domains: ErrorDomains::new(),
            _extra: PhantomData,
        }
    }

    /// The product's rule on `sub`, after it is a non-empty string.
    #[must_use]
    pub fn with_subject_check(self, check: impl Fn(&str) -> bool + Send + Sync + 'static) -> Self {
        Self {
            subject_check: Some(Box::new(check)),
            ..self
        }
    }

    /// The domains of the refusal (default `ErrorDomains::new()`; the admin one is used).
    #[must_use]
    pub fn with_domains(self, domains: ErrorDomains) -> Self {
        Self { domains, ..self }
    }

    /// The domains of the refusal.
    pub fn domains(&self) -> ErrorDomains {
        self.domains
    }

    /// The claims of `token` for the RPC `op` at `now`. Every failure is the one answer
    /// `UNAUTHENTICATED` / `ADMIN_TOKEN_INVALID` in the admin domain; the cause is logged at
    /// `debug`, never the token.
    pub fn verify(
        &self,
        token: &Secret,
        op: &str,
        now: SystemTime,
    ) -> Result<AdminClaims<X>, AppError> {
        self.check(token.expose(), op, now).map_err(|cause| {
            tracing::debug!(cause = %cause, "admin token refused");
            crate::error::admin::token_invalid(self.domains.admin)
        })
    }

    /// Every rule of the token, in order; the first one broken is the cause.
    fn check(&self, token: &str, op: &str, now: SystemTime) -> Result<AdminClaims<X>, Cause> {
        let Some((keys, validation)) = &self.keys else {
            return Err(Cause::Refusing);
        };
        let now = now
            .duration_since(SystemTime::UNIX_EPOCH)
            .map_err(|_| Cause::BeforeEpoch)?
            .as_secs();
        let claims: Claims<AdminBody<X>> =
            keys.verify(token, now, validation).map_err(Cause::Token)?;
        if has_null_claim(token) {
            return Err(Cause::NullClaim);
        }
        let short_lived = claims
            .exp
            .checked_sub(claims.iat)
            .is_some_and(|secs| secs > 0 && secs <= MAX_LIFETIME.as_secs());
        if !short_lived {
            return Err(Cause::Lifetime);
        }
        let expires_at = SystemTime::UNIX_EPOCH
            .checked_add(Duration::from_secs(claims.exp))
            .ok_or(Cause::Lifetime)?;
        let subject_passes = !claims.sub.is_empty()
            && self
                .subject_check
                .as_ref()
                .is_none_or(|check| check(&claims.sub));
        if !subject_passes {
            return Err(Cause::Subject);
        }
        let token_id = claims
            .jti
            .as_deref()
            .and_then(parse_token_id)
            .ok_or(Cause::TokenId)?;
        let tenant = match claims.custom.tid.as_deref() {
            None => None,
            Some(text) => Some(
                TenantId::parse(text)
                    .filter(|id| !id.is_local())
                    .ok_or(Cause::Tenant)?,
            ),
        };
        if claims.custom.op != op {
            return Err(Cause::Operation);
        }
        Ok(AdminClaims {
            token_id,
            subject: claims.sub,
            tenant,
            op: claims.custom.op,
            expires_at,
            extra: claims.custom.extra,
        })
    }
}

/// Whether any top-level member of the token's payload is JSON `null`: an optional claim given as
/// `null` would otherwise read as absent. A payload that does not decode counts as one.
fn has_null_claim(token: &str) -> bool {
    let Some(payload) = token.split('.').nth(1) else {
        return true;
    };
    let Ok(bytes) = URL_SAFE_NO_PAD.decode(payload) else {
        return true;
    };
    match serde_json::from_slice::<serde_json::Map<String, serde_json::Value>>(&bytes) {
        Ok(members) => members.values().any(serde_json::Value::is_null),
        Err(_) => true,
    }
}

/// The token id of a `jti`: the lower-case hyphenated form of a UUID only.
fn parse_token_id(text: &str) -> Option<Uuid> {
    if text.len() != UUID_TEXT_LEN {
        return None;
    }
    let id = Uuid::try_parse(text).ok()?;
    let mut buffer = Uuid::encode_buffer();
    (&*id.hyphenated().encode_lower(&mut buffer) == text).then_some(id)
}

/// The verification keys of a list `key_id:<base64url Ed25519 public key>[,…]` (what
/// `Minter::public_key_entry` writes, joined by commas). `INVALID_ARGUMENT` naming an entry's
/// position, never a key id or a key.
pub fn parse_keys(text: &str) -> Result<EdDsaVerificationKeys, AppError> {
    let mut keys: Option<EdDsaVerificationKeys> = None;
    let mut seen: Vec<&str> = Vec::new();
    for (index, entry) in text.split(',').enumerate() {
        let position = index + 1;
        let refuse =
            |what: &str| AppError::invalid_argument(format!("admin key entry {position} {what}"));
        let entry = entry.trim();
        if entry.is_empty() {
            return Err(refuse("is empty"));
        }
        let Some((kid, key)) = entry.split_once(':') else {
            return Err(refuse("has no `:` between its key id and its key"));
        };
        if !is_key_id(kid) {
            return Err(refuse("has an invalid key id"));
        }
        if seen.contains(&kid) {
            return Err(refuse("repeats the key id of an earlier entry"));
        }
        seen.push(kid);
        let mut raw = [0_u8; KEY_LEN];
        let decoded = key.len() == KEY_TEXT_LEN
            && matches!(
                URL_SAFE_NO_PAD.decode_slice(key.as_bytes(), &mut raw),
                Ok(KEY_LEN)
            );
        if !decoded {
            return Err(refuse(
                "is not 32 bytes of unpadded base64url after its key id",
            ));
        }
        let added = match keys {
            None => EdDsaVerificationKeys::from_public_key(kid, &raw),
            Some(keys) => keys.with_public_key(kid, &raw),
        };
        keys = Some(added.map_err(|_| refuse("is not a usable Ed25519 public key"))?);
    }
    keys.ok_or_else(|| AppError::invalid_argument("admin key entry 1 is empty"))
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, SystemTime};

    use base64::Engine as _;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use sekvent_auth::{Claims, EdDsaSigningKey, JwtKeys, TokenRejected};
    use sekvent_component::END_USER_REJECTED_MESSAGE;
    use sekvent_config::Secret;
    use sekvent_error::{AppError, ErrorCode};
    use serde::Deserialize;
    use serde_json::{Map, Value, json};
    use uuid::Uuid;

    use super::{Cause, Verifier, has_null_claim, parse_keys};
    use crate::admin::claims::NoExtra;
    use crate::{ErrorDomains, TenantId, reasons};

    const ISSUER: &str = "ops-console";
    const AUDIENCE: &str = "billing-eu";
    const OP: &str = "billing.admin.v1.TenantAdmin/Suspend";
    const OTHER_OP: &str = "billing.admin.v1.TenantAdmin/Purge";
    const TENANT: &str = "6f1c2a8e-3b4d-4e5f-8a9b-0c1d2e3f4a5b";
    const JTI: &str = "0b7e5c1a-9d2f-4a6b-8c3e-1f2a3b4c5d6e";
    const SUBJECT: &str = "operator:ada";
    /// The verification time; tokens are issued 10 s before it, for 60 s.
    const NOW: u64 = 1_700_000_000;

    type Body = Claims<Map<String, Value>>;

    fn seed(byte: u8) -> [u8; 32] {
        [byte; 32]
    }

    fn signer() -> EdDsaSigningKey {
        EdDsaSigningKey::from_seed("k1", &seed(7)).unwrap()
    }

    fn verifier() -> Verifier {
        Verifier::new(ISSUER, AUDIENCE, signer().verification_keys())
    }

    fn at(secs: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(secs)
    }

    /// A valid token's claims.
    fn valid() -> Body {
        let mut custom = Map::new();
        custom.insert("tid".into(), json!(TENANT));
        custom.insert("op".into(), json!(OP));
        Claims::new(SUBJECT, NOW - 10, 60, custom)
            .with_issuer(ISSUER)
            .with_audience(AUDIENCE)
            .with_jti(JTI)
    }

    fn with(mut claims: Body, key: &str, value: Value) -> Body {
        claims.custom.insert(key.into(), value);
        claims
    }

    fn without(mut claims: Body, key: &str) -> Body {
        claims.custom.remove(key);
        claims
    }

    fn sign(claims: &Body) -> Secret {
        Secret::new(signer().issue(claims).unwrap())
    }

    fn key_text(byte: u8) -> String {
        let signer = EdDsaSigningKey::from_seed("k", &seed(byte)).unwrap();
        URL_SAFE_NO_PAD.encode(signer.public_key())
    }

    fn assert_invalid(error: &AppError, domain: &str) {
        assert_eq!(error.code(), ErrorCode::Unauthenticated);
        assert_eq!(error.reason(), Some(reasons::ADMIN_TOKEN_INVALID));
        assert_eq!(error.domain(), Some(domain));
        assert_eq!(error.message(), END_USER_REJECTED_MESSAGE);
    }

    fn refused<X>(verifier: &Verifier<X>, token: &Secret, now: u64)
    where
        X: serde::de::DeserializeOwned + Send + Sync + 'static,
    {
        let error = verifier.verify(token, OP, at(now)).err().unwrap();
        assert_invalid(&error, ErrorDomains::DEFAULT_ADMIN);
    }

    #[test]
    fn a_valid_token_gives_its_claims() {
        let claims = verifier().verify(&sign(&valid()), OP, at(NOW)).unwrap();
        assert_eq!(claims.token_id, Uuid::parse_str(JTI).unwrap());
        assert_eq!(claims.subject, SUBJECT);
        assert_eq!(claims.tenant, TenantId::parse(TENANT));
        assert_eq!(claims.op, OP);
        assert_eq!(claims.expires_at, at(NOW + 50));
        assert_eq!(claims.extra, NoExtra {});
    }

    #[test]
    fn the_boundaries_of_the_rules_are_accepted() {
        let verifier = verifier();
        let mut sixty = valid();
        sixty.iat = NOW;
        sixty.exp = NOW + 60;
        let accepted = [
            // `iat == now` and a lifetime of exactly 60 s.
            (sixty, NOW),
            // The last second before `exp`.
            (valid(), NOW + 49),
            // `nbf == now`.
            (valid().with_not_before(NOW), NOW),
            // No tenant: a call that addresses none.
            (without(valid(), "tid"), NOW),
            // An extra claim `NoExtra` does not know.
            (with(valid(), "region", json!("eu")), NOW),
        ];
        for (index, (claims, now)) in accepted.into_iter().enumerate() {
            let outcome = verifier.verify(&sign(&claims), OP, at(now));
            assert!(outcome.is_ok(), "row {index}");
        }
        let untenanted = verifier
            .verify(&sign(&without(valid(), "tid")), OP, at(NOW))
            .unwrap();
        assert_eq!(untenanted.tenant, None);
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn every_rule_refuses_with_the_one_answer() {
        let verifier = verifier();
        let hs256 = JwtKeys::hs256("k1", &Secret::new("0123456789abcdef0123456789abcdef"))
            .unwrap()
            .issue(&valid())
            .unwrap();
        let unknown_kid = EdDsaSigningKey::from_seed("k9", &seed(7))
            .unwrap()
            .issue(&valid())
            .unwrap();
        let other_key = EdDsaSigningKey::from_seed("k1", &seed(8))
            .unwrap()
            .issue(&valid())
            .unwrap();
        let signed = sign(&valid());
        let mut tampered: Vec<String> = signed.expose().split('.').map(str::to_owned).collect();
        tampered[1] = URL_SAFE_NO_PAD
            .encode(serde_json::to_vec(&with(valid(), "op", json!(OTHER_OP))).unwrap());
        let mut no_issuer = valid();
        no_issuer.iss = None;
        let mut no_audience = valid();
        no_audience.aud = None;
        let array_audience = with(no_audience.clone(), "aud", json!([AUDIENCE]));
        let mut long = valid();
        long.exp = long.iat + 61;
        let mut zero = valid();
        zero.exp = zero.iat;
        let mut backwards = valid();
        backwards.iat = NOW + 5;
        backwards.exp = NOW;
        let mut empty_subject = valid();
        empty_subject.sub = String::new();
        let mut no_jti = valid();
        no_jti.jti = None;

        let rows: Vec<(&str, Secret, u64)> = vec![
            ("an HS256 token", Secret::new(hs256), NOW),
            ("an unknown key id", Secret::new(unknown_kid), NOW),
            (
                "another key under the known id",
                Secret::new(other_key),
                NOW,
            ),
            (
                "a payload changed after signing",
                Secret::new(tampered.join(".")),
                NOW,
            ),
            ("not a token", Secret::new("not-a-token"), NOW),
            (
                "another issuer",
                sign(&valid().with_issuer("billing-us")),
                NOW,
            ),
            ("no issuer", sign(&no_issuer), NOW),
            (
                "another audience",
                sign(&valid().with_audience("billing-us")),
                NOW,
            ),
            ("no audience", sign(&no_audience), NOW),
            ("an array audience", sign(&array_audience), NOW),
            ("expired: now == exp", sign(&valid()), NOW + 50),
            ("issued after now", sign(&valid()), NOW - 11),
            ("a lifetime of 61 s", sign(&long), NOW),
            ("exp == iat", sign(&zero), NOW),
            ("exp before iat", sign(&backwards), NOW + 5),
            (
                "nbf after now",
                sign(&valid().with_not_before(NOW + 1)),
                NOW,
            ),
            ("an empty subject", sign(&empty_subject), NOW),
            ("no token id", sign(&no_jti), NOW),
            (
                "an upper-case token id",
                sign(&valid().with_jti(JTI.to_uppercase())),
                NOW,
            ),
            (
                "a braced token id",
                sign(&valid().with_jti(format!("{{{JTI}}}"))),
                NOW,
            ),
            (
                "a simple token id",
                sign(&valid().with_jti(JTI.replace('-', ""))),
                NOW,
            ),
            (
                "a token id that is no UUID",
                sign(&valid().with_jti("token-1")),
                NOW,
            ),
            (
                "an upper-case tenant",
                sign(&with(valid(), "tid", json!(TENANT.to_uppercase()))),
                NOW,
            ),
            (
                "a malformed tenant",
                sign(&with(valid(), "tid", json!("tenant-1"))),
                NOW,
            ),
            (
                "a numeric tenant",
                sign(&with(valid(), "tid", json!(7))),
                NOW,
            ),
            (
                "the nil tenant",
                sign(&with(valid(), "tid", json!(Uuid::nil().to_string()))),
                NOW,
            ),
            (
                "another operation",
                sign(&with(valid(), "op", json!(OTHER_OP))),
                NOW,
            ),
            (
                "the operation with its leading slash",
                sign(&with(valid(), "op", json!(format!("/{OP}")))),
                NOW,
            ),
            ("no operation", sign(&without(valid(), "op")), NOW),
        ];
        for (row, token, now) in &rows {
            let error = verifier.verify(token, OP, at(*now)).unwrap_err();
            assert_eq!(error.reason(), Some(reasons::ADMIN_TOKEN_INVALID), "{row}");
            assert_invalid(&error, ErrorDomains::DEFAULT_ADMIN);
        }
    }

    #[test]
    fn a_claim_given_as_null_is_refused() {
        let verifier = verifier();
        for claim in ["nbf", "tid", "region", "jti", "iss", "aud", "op"] {
            let mut claims = with(valid(), claim, Value::Null);
            // `Claims` skips its own absent members, so the map's `null` is the only one.
            match claim {
                "jti" => claims.jti = None,
                "iss" => claims.iss = None,
                "aud" => claims.aud = None,
                _ => {}
            }
            refused(&verifier, &sign(&claims), NOW);
        }
        // Without the member, `nbf` and `tid` are optional and the token passes.
        assert!(
            verifier
                .verify(&sign(&without(valid(), "tid")), OP, at(NOW))
                .is_ok()
        );
        assert!(has_null_claim("only-one-segment"));
        assert!(has_null_claim("a.!!!.c"));
        assert!(has_null_claim(&format!(
            "a.{}.c",
            URL_SAFE_NO_PAD.encode("[1]")
        )));
        let nested = format!("a.{}.c", URL_SAFE_NO_PAD.encode(r#"{"a":{"b":null}}"#));
        assert!(!has_null_claim(&nested));
    }

    #[test]
    fn a_time_before_the_epoch_is_refused() {
        let before = SystemTime::UNIX_EPOCH - Duration::from_secs(1);
        let error = verifier().verify(&sign(&valid()), OP, before).unwrap_err();
        assert_invalid(&error, ErrorDomains::DEFAULT_ADMIN);
    }

    #[test]
    fn the_subject_check_runs_on_a_non_empty_subject() {
        let verifier = verifier().with_subject_check(|subject| subject.starts_with("operator:"));
        assert!(verifier.verify(&sign(&valid()), OP, at(NOW)).is_ok());
        let mut other = valid();
        other.sub = "robot:7".into();
        refused(&verifier, &sign(&other), NOW);

        let accepting = self::verifier().with_subject_check(|_| true);
        let mut empty = valid();
        empty.sub = String::new();
        refused(&accepting, &sign(&empty), NOW);
    }

    #[test]
    fn the_refusal_is_in_the_chosen_admin_domain() {
        let verifier = verifier().with_domains(ErrorDomains::new().with_admin("billing.admin.v1"));
        assert_eq!(verifier.domains().admin, "billing.admin.v1");
        let error = verifier
            .verify(&sign(&valid()), OTHER_OP, at(NOW))
            .unwrap_err();
        assert_invalid(&error, "billing.admin.v1");
        assert_eq!(
            Verifier::<NoExtra>::refusing().domains(),
            ErrorDomains::new()
        );
    }

    #[test]
    fn a_refusing_verifier_accepts_nothing() {
        let refusing = Verifier::<NoExtra>::refusing();
        refused(&refusing, &sign(&valid()), NOW);
        assert_eq!(
            format!("{refusing:?}"),
            "Verifier { kids: [], refusing: true, .. }"
        );
    }

    #[test]
    fn debug_prints_the_key_ids_only() {
        let keys = parse_keys(&format!("k1:{},k2:{}", key_text(7), key_text(8))).unwrap();
        let verifier = Verifier::<NoExtra>::new(ISSUER, AUDIENCE, keys);
        let printed = format!("{verifier:?}");
        assert_eq!(
            printed,
            r#"Verifier { kids: ["k1", "k2"], refusing: false, .. }"#
        );
        assert!(!printed.contains(&key_text(7)));
    }

    #[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
    struct Region {
        region: String,
    }

    #[test]
    fn the_extra_claims_are_flattened() {
        let verifier = Verifier::<Region>::new(ISSUER, AUDIENCE, signer().verification_keys());
        let claims = verifier
            .verify(&sign(&with(valid(), "region", json!("eu-1"))), OP, at(NOW))
            .unwrap();
        assert_eq!(
            claims.extra,
            Region {
                region: "eu-1".into()
            }
        );
        assert_eq!(claims.tenant, TenantId::parse(TENANT));
        for token in [sign(&valid()), sign(&with(valid(), "region", json!(7)))] {
            refused(&verifier, &token, NOW);
        }
    }

    #[test]
    fn two_keys_both_verify_during_a_rotation() {
        let old = EdDsaSigningKey::from_seed("k1", &seed(7)).unwrap();
        let new = EdDsaSigningKey::from_seed("k2", &seed(8)).unwrap();
        let keys = parse_keys(&format!(" k1:{} , k2:{} ", key_text(7), key_text(8))).unwrap();
        let verifier = Verifier::<NoExtra>::new(ISSUER, AUDIENCE, keys);
        for signer in [&old, &new] {
            let token = Secret::new(signer.issue(&valid()).unwrap());
            assert!(
                verifier.verify(&token, OP, at(NOW)).is_ok(),
                "{}",
                signer.kid()
            );
        }
        let retired = EdDsaSigningKey::from_seed("k0", &seed(9)).unwrap();
        refused(
            &verifier,
            &Secret::new(retired.issue(&valid()).unwrap()),
            NOW,
        );
    }

    #[test]
    fn parse_keys_reads_a_list() {
        let keys = parse_keys(&format!("k1:{}", key_text(7))).unwrap();
        assert_eq!(keys.kids().collect::<Vec<_>>(), ["k1"]);
        let keys = parse_keys(&format!("k1:{},k_2-b:{}", key_text(7), key_text(8))).unwrap();
        assert_eq!(keys.kids().collect::<Vec<_>>(), ["k1", "k_2-b"]);
    }

    #[test]
    fn parse_keys_refusals_name_the_position_only() {
        let good = key_text(7);
        let other = key_text(8);
        let padded = format!("{good}=");
        let mut standard = good.clone();
        standard.replace_range(0..1, "+");
        let short = URL_SAFE_NO_PAD.encode([1_u8; 31]);
        let long = URL_SAFE_NO_PAD.encode([1_u8; 33]);
        let weak = URL_SAFE_NO_PAD.encode([0_u8; 32]);
        let mut point = [0_u8; 32];
        point[0] = 2;
        let not_a_point = URL_SAFE_NO_PAD.encode(point);
        let hidden = "hidden-kid-name";
        let cases: Vec<(String, usize)> = vec![
            (String::new(), 1),
            ("   ".to_owned(), 1),
            (format!("k1:{good},"), 2),
            (format!("k1:{good},,k2:{other}"), 2),
            (format!("{hidden}{good}"), 1),
            (format!("k1:{good},:{other}"), 2),
            (format!("k1:{good},{}:{other}", "k".repeat(33)), 2),
            (format!("k1:{good},bad kid:{other}"), 2),
            (format!("k1:{good},k1:{other}"), 2),
            (format!("k1:{padded}"), 1),
            (format!("k1:{standard}"), 1),
            (format!("k1:{short}"), 1),
            (format!("k1:{long}"), 1),
            ("k1:".to_owned(), 1),
            (format!("k1:{good},k2:{weak}"), 2),
            (format!("k1:{good},k2:{good},k3:{not_a_point}"), 3),
        ];
        for (text, position) in cases {
            let error = parse_keys(&text).unwrap_err();
            assert_eq!(error.code(), ErrorCode::InvalidArgument, "{text}");
            let message = error.message();
            assert!(
                message.starts_with(&format!("admin key entry {position} ")),
                "{text}: {message}"
            );
            for leaked in [good.as_str(), other.as_str(), "k1", "k2", "k3", hidden] {
                assert!(!message.contains(leaked), "{text}: {message}");
            }
        }
    }

    #[test]
    fn every_cause_has_a_fixed_text() {
        let causes = [
            Cause::Refusing,
            Cause::BeforeEpoch,
            Cause::Token(TokenRejected::Expired),
            Cause::NullClaim,
            Cause::Lifetime,
            Cause::Subject,
            Cause::TokenId,
            Cause::Tenant,
            Cause::Operation,
        ];
        for cause in causes {
            assert!(!cause.to_string().is_empty(), "{cause:?}");
        }
        assert_eq!(
            Cause::Token(TokenRejected::Expired).to_string(),
            "token has expired"
        );
    }
}
