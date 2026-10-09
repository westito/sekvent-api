//! The signed state cookie, PKCE and random tokens.

use std::fmt;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use hmac::{Hmac, KeyInit, Mac};
use http::HeaderMap;
use sekvent_config::Secret;
use sekvent_error::AppError;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

type HmacSha256 = Hmac<Sha256>;

/// Shortest accepted state key, in bytes.
pub const MIN_STATE_KEY_LEN: usize = 32;

/// Longest cookie value [`StateKey::open`] looks at.
const MAX_COOKIE_LEN: usize = 4096;

/// Domain separation for the derived MAC key.
const KEY_LABEL: &[u8] = b"sekvent-sso/state-cookie/v1";

/// Format tag at the start of every sealed value.
const FORMAT: &str = "v1";

/// 32 bytes from the operating system's random source, base64url without
/// padding (43 characters).
pub(crate) fn random_token() -> Result<String, AppError> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes)
        .map_err(|_| AppError::internal("the operating system random source failed"))?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

/// The PKCE S256 challenge of `verifier` (RFC 7636 §4.2).
pub(crate) fn code_challenge(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

/// SHA-256 of `value`, for keys that must not be the secret itself.
pub(crate) fn digest(value: &str) -> [u8; 32] {
    Sha256::digest(value.as_bytes()).into()
}

/// One sign-in in flight, kept in the browser between login and callback.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct FlowState {
    /// Provider id.
    #[serde(rename = "p")]
    pub(crate) provider: String,
    /// The `state` sent to the provider.
    #[serde(rename = "s")]
    pub(crate) state: String,
    /// The PKCE code verifier.
    #[serde(rename = "k")]
    pub(crate) verifier: String,
    /// The validated relative path to return to.
    #[serde(rename = "r")]
    pub(crate) redirect: String,
    /// Expiry, Unix seconds.
    #[serde(rename = "e")]
    pub(crate) expires_at: u64,
}

impl fmt::Debug for FlowState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FlowState")
            .field("provider", &self.provider)
            .field("redirect", &self.redirect)
            .field("expires_at", &self.expires_at)
            .finish_non_exhaustive()
    }
}

/// Why a state cookie was refused. For server logs only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StateRejected {
    /// No cookie with the configured name.
    Missing,
    /// Not in the sealed format, or a payload that does not decode.
    Malformed,
    /// The MAC does not verify: tampered, or sealed with another key.
    BadSignature,
    /// Past its expiry.
    Expired,
    /// Sealed for another provider.
    WrongProvider,
    /// The `state` query parameter differs from the sealed one.
    Mismatch,
    /// This state was already used for a callback.
    Replayed,
}

impl fmt::Display for StateRejected {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Missing => "no state cookie",
            Self::Malformed => "malformed state cookie",
            Self::BadSignature => "state cookie signature does not verify",
            Self::Expired => "state cookie expired",
            Self::WrongProvider => "state cookie belongs to another provider",
            Self::Mismatch => "state parameter does not match the cookie",
            Self::Replayed => "state was already used",
        })
    }
}

/// The MAC key for state cookies, derived from the configured secret.
#[derive(Clone)]
pub(crate) struct StateKey {
    key: [u8; 32],
}

impl fmt::Debug for StateKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("StateKey([redacted])")
    }
}

impl StateKey {
    /// Derive the key; the secret must have at least
    /// [`MIN_STATE_KEY_LEN`] bytes.
    pub(crate) fn new(secret: &Secret) -> Result<Self, AppError> {
        if secret.expose().len() < MIN_STATE_KEY_LEN {
            return Err(AppError::invalid_argument(format!(
                "the SSO state key must be at least {MIN_STATE_KEY_LEN} bytes"
            )));
        }
        let mut mac = HmacSha256::new_from_slice(secret.expose().as_bytes())
            .expect("HMAC accepts keys of any length");
        mac.update(KEY_LABEL);
        Ok(Self {
            key: mac.finalize().into_bytes().into(),
        })
    }

    fn mac(&self) -> HmacSha256 {
        HmacSha256::new_from_slice(&self.key).expect("HMAC accepts keys of any length")
    }

    /// `v1.<payload>.<mac>`, both parts base64url without padding.
    pub(crate) fn seal(&self, flow: &FlowState) -> String {
        let json = serde_json::to_vec(flow).expect("a flow state always serializes");
        let payload = URL_SAFE_NO_PAD.encode(json);
        let signed = format!("{FORMAT}.{payload}");
        let mut mac = self.mac();
        mac.update(signed.as_bytes());
        let tag = URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes());
        format!("{signed}.{tag}")
    }

    /// Verify and decode a sealed value at `now_unix_secs`.
    pub(crate) fn open(
        &self,
        sealed: &str,
        now_unix_secs: u64,
    ) -> Result<FlowState, StateRejected> {
        if sealed.len() > MAX_COOKIE_LEN {
            return Err(StateRejected::Malformed);
        }
        let (signed, tag) = sealed.rsplit_once('.').ok_or(StateRejected::Malformed)?;
        let (format, payload) = signed.split_once('.').ok_or(StateRejected::Malformed)?;
        if format != FORMAT {
            return Err(StateRejected::Malformed);
        }
        let tag = URL_SAFE_NO_PAD
            .decode(tag)
            .map_err(|_| StateRejected::Malformed)?;
        let mut mac = self.mac();
        mac.update(signed.as_bytes());
        mac.verify_slice(&tag)
            .map_err(|_| StateRejected::BadSignature)?;
        let json = URL_SAFE_NO_PAD
            .decode(payload)
            .map_err(|_| StateRejected::Malformed)?;
        let flow: FlowState =
            serde_json::from_slice(&json).map_err(|_| StateRejected::Malformed)?;
        if now_unix_secs >= flow.expires_at {
            return Err(StateRejected::Expired);
        }
        Ok(flow)
    }
}

/// Every value of the cookie `name` in the request's `Cookie` headers.
pub(crate) fn cookie_values<'a>(headers: &'a HeaderMap, name: &'a str) -> Vec<&'a str> {
    headers
        .get_all(http::header::COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|line| line.split(';'))
        .filter_map(|pair| pair.trim().split_once('='))
        .filter(|(key, _)| *key == name)
        .map(|(_, value)| value.trim_matches('"'))
        .collect()
}

/// Cookie attributes shared by setting and clearing.
#[derive(Debug, Clone)]
pub(crate) struct CookieSpec {
    pub(crate) name: String,
    pub(crate) secure: bool,
}

impl CookieSpec {
    /// The `Set-Cookie` value that stores `value` for `max_age_secs`.
    pub(crate) fn set(&self, value: &str, max_age_secs: u64) -> String {
        format!(
            "{}={value}; Path=/; Max-Age={max_age_secs}; HttpOnly; SameSite=Lax{}",
            self.name,
            if self.secure { "; Secure" } else { "" }
        )
    }

    /// The `Set-Cookie` value that deletes the cookie.
    pub(crate) fn clear(&self) -> String {
        self.set("", 0)
    }
}

#[cfg(test)]
mod tests {
    use http::HeaderValue;

    use super::*;

    fn key() -> StateKey {
        StateKey::new(&Secret::new("k".repeat(32))).unwrap()
    }

    fn flow() -> FlowState {
        FlowState {
            provider: "bitbucket".into(),
            state: "state-1".into(),
            verifier: "verifier-1".into(),
            redirect: "/orders".into(),
            expires_at: 1_000,
        }
    }

    #[test]
    fn a_short_key_is_refused() {
        let error = StateKey::new(&Secret::new("short")).unwrap_err();
        assert!(error.message().contains("32 bytes"));
        assert!(!error.message().contains("short"));
    }

    #[test]
    fn seal_then_open_round_trips_before_expiry() {
        let sealed = key().seal(&flow());
        assert!(sealed.starts_with("v1."));
        assert_eq!(key().open(&sealed, 999).unwrap(), flow());
        assert_eq!(key().open(&sealed, 1_000), Err(StateRejected::Expired));
    }

    #[test]
    fn tampering_and_other_keys_are_refused() {
        let sealed = key().seal(&flow());
        let other = StateKey::new(&Secret::new("o".repeat(40))).unwrap();
        assert_eq!(other.open(&sealed, 0), Err(StateRejected::BadSignature));

        let (signed, tag) = sealed.rsplit_once('.').unwrap();
        let mut forged = flow();
        forged.redirect = "/admin".into();
        let forged_payload = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&forged).unwrap());
        let swapped = format!("v1.{forged_payload}.{tag}");
        assert_eq!(key().open(&swapped, 0), Err(StateRejected::BadSignature));

        let mut flipped = signed.to_owned();
        flipped.push('A');
        assert_eq!(
            key().open(&format!("{flipped}.{tag}"), 0),
            Err(StateRejected::BadSignature)
        );
    }

    #[test]
    fn malformed_values_are_refused() {
        let k = key();
        for value in [
            "",
            "v1",
            "v1.abc",
            "v2.abc.def",
            "v1.abc.!!!",
            &"a".repeat(MAX_COOKIE_LEN + 1),
        ] {
            assert_eq!(
                k.open(value, 0),
                Err(StateRejected::Malformed),
                "{value:.20}"
            );
        }
        // A valid MAC over a payload that is not base64 or not a flow.
        for payload in ["!!!", "bm90IGpzb24"] {
            let signed = format!("v1.{payload}");
            let mut mac = k.mac();
            mac.update(signed.as_bytes());
            let tag = URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes());
            assert_eq!(
                k.open(&format!("{signed}.{tag}"), 0),
                Err(StateRejected::Malformed)
            );
        }
    }

    #[test]
    fn debug_output_hides_secrets() {
        assert_eq!(format!("{:?}", key()), "StateKey([redacted])");
        let debug = format!("{:?}", flow());
        assert!(!debug.contains("state-1"));
        assert!(!debug.contains("verifier-1"));
        assert!(debug.contains("/orders"));
    }

    #[test]
    fn rejections_have_log_messages() {
        for rejected in [
            StateRejected::Missing,
            StateRejected::Malformed,
            StateRejected::BadSignature,
            StateRejected::Expired,
            StateRejected::WrongProvider,
            StateRejected::Mismatch,
            StateRejected::Replayed,
        ] {
            assert!(!rejected.to_string().is_empty());
        }
    }

    #[test]
    fn random_tokens_are_long_and_distinct() {
        let a = random_token().unwrap();
        let b = random_token().unwrap();
        assert_eq!(a.len(), 43);
        assert_ne!(a, b);
    }

    #[test]
    fn pkce_challenge_matches_rfc_7636_appendix_b() {
        assert_eq!(
            code_challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
        assert_eq!(digest("x").len(), 32);
    }

    #[test]
    fn cookie_values_reads_every_header_and_pair() {
        let mut headers = HeaderMap::new();
        headers.append(
            http::header::COOKIE,
            HeaderValue::from_static("a=1; sso=\"one\"; b=2"),
        );
        headers.append(http::header::COOKIE, HeaderValue::from_static("sso=two"));
        headers.append(
            http::header::COOKIE,
            HeaderValue::from_bytes(b"sso=\xff").unwrap(),
        );
        assert_eq!(cookie_values(&headers, "sso"), ["one", "two"]);
        assert!(cookie_values(&headers, "missing").is_empty());
    }

    #[test]
    fn cookie_attributes() {
        let secure = CookieSpec {
            name: "__Host-sso".into(),
            secure: true,
        };
        assert_eq!(
            secure.set("v", 600),
            "__Host-sso=v; Path=/; Max-Age=600; HttpOnly; SameSite=Lax; Secure"
        );
        assert_eq!(
            secure.clear(),
            "__Host-sso=; Path=/; Max-Age=0; HttpOnly; SameSite=Lax; Secure"
        );
        let plain = CookieSpec {
            name: "sso".into(),
            secure: false,
        };
        assert_eq!(
            plain.set("v", 1),
            "sso=v; Path=/; Max-Age=1; HttpOnly; SameSite=Lax"
        );
    }
}
