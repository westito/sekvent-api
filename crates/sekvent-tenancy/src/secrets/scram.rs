//! The SCRAM-SHA-256 verifier a tenant role is created and altered with, so
//! that the password never appears in a statement.

use std::fmt;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use sekvent_config::Secret;
use sekvent_error::AppError;
use sha2::{Digest, Sha256};
use zeroize::Zeroizing;

use super::kdf::{hmac_sha256, pbkdf2_sha256_block};

/// The PBKDF2 iteration count of every verifier this crate makes or accepts.
pub const SCRAM_ITERATIONS: u32 = 4096;

/// The mechanism name that opens every verifier.
const HEAD: &str = "SCRAM-SHA-256$";

/// The bytes of a random salt.
const SALT_BYTES: usize = 16;

/// A SCRAM-SHA-256 verifier as Postgres stores it. `Debug` prints the type
/// name only.
#[derive(Clone)]
pub struct ScramVerifier {
    /// `SCRAM-SHA-256$4096:<salt>$<StoredKey>:<ServerKey>`; passed
    /// [`is_scram_verifier`].
    text: Secret,
}

/// The keys RFC 5802 derives from a password and a salt; wiped on drop.
struct Keys {
    client: Zeroizing<[u8; 32]>,
    server: Zeroizing<[u8; 32]>,
}

impl Keys {
    /// `SaltedPassword` is one PBKDF2-HMAC-SHA-256 block of
    /// [`SCRAM_ITERATIONS`]; `ClientKey` and `ServerKey` are HMACs of it.
    ///
    /// The password is used as its bytes: `SASLprep` is the identity on the
    /// ASCII passwords this crate derives.
    fn derive(password: &Secret, salt: &[u8]) -> Self {
        let salted = pbkdf2_sha256_block(password.expose().as_bytes(), salt, SCRAM_ITERATIONS);
        Self {
            client: hmac_sha256(&salted[..], &[b"Client Key"]),
            server: hmac_sha256(&salted[..], &[b"Server Key"]),
        }
    }

    /// `StoredKey`: the SHA-256 of `ClientKey`.
    fn stored(&self) -> Zeroizing<[u8; 32]> {
        Zeroizing::new(Sha256::digest(&self.client[..]).into())
    }
}

impl ScramVerifier {
    /// With a random 16-byte salt. `INTERNAL` when the random source fails.
    ///
    /// 4096 iterations take a moment: run it on the blocking pool.
    pub fn new(password: &Secret) -> Result<Self, AppError> {
        let mut salt = [0u8; SALT_BYTES];
        getrandom::fill(&mut salt)
            .map_err(|err| AppError::internal(format!("the random source failed: {err}")))?;
        Ok(Self::with_salt(password, &salt))
    }

    /// With `salt`; pure.
    pub fn with_salt(password: &Secret, salt: &[u8; 16]) -> Self {
        let keys = Keys::derive(password, salt);
        let text = format!(
            "{HEAD}{SCRAM_ITERATIONS}:{}${}:{}",
            STANDARD.encode(salt),
            STANDARD.encode(&keys.stored()[..]),
            STANDARD.encode(&keys.server[..]),
        );
        Self {
            text: Secret::new(text),
        }
    }

    /// A verifier made elsewhere, after the syntax check of
    /// [`is_scram_verifier`]. `INTERNAL` otherwise.
    pub fn parse(text: Secret) -> Result<Self, AppError> {
        if is_scram_verifier(text.expose()) {
            Ok(Self { text })
        } else {
            Err(AppError::internal(
                "a SCRAM verifier failed the syntax check",
            ))
        }
    }

    /// The verifier as a single-quoted SQL string literal,
    /// `'SCRAM-SHA-256$…'`. Its alphabet holds no quote, so nothing is
    /// escaped; this is the only literal the crate formats into a statement.
    pub(crate) fn literal(&self) -> Secret {
        // Every way to a `ScramVerifier` passes `is_scram_verifier` (`parse`)
        // or builds that grammar (`with_salt`): letters, digits, `+`, `/`,
        // `=`, `-`, `:` and `$` only. No quote and no backslash can occur, so
        // the text cannot end the literal early.
        Secret::new(format!("'{}'", self.text.expose()))
    }
}

impl fmt::Debug for ScramVerifier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ScramVerifier(..)")
    }
}

/// `SCRAM-SHA-256$4096:<salt>$<StoredKey>:<ServerKey>`, each part standard
/// base64 (`[A-Za-z0-9+/]+={0,2}`), nothing before or after.
///
/// A syntax check only: the parts are not decoded and their lengths are not
/// checked.
pub fn is_scram_verifier(text: &str) -> bool {
    let Some(rest) = text.strip_prefix(HEAD) else {
        return false;
    };
    let Some((iterations, rest)) = rest.split_once(':') else {
        return false;
    };
    // Decimal digits without a leading zero or sign: one spelling of 4096.
    let canonical = iterations.bytes().all(|b| b.is_ascii_digit()) && !iterations.starts_with('0');
    if !canonical || iterations.parse::<u32>().ok() != Some(SCRAM_ITERATIONS) {
        return false;
    }
    let Some((salt, keys)) = rest.split_once('$') else {
        return false;
    };
    let Some((stored, server)) = keys.split_once(':') else {
        return false;
    };
    [salt, stored, server].into_iter().all(is_base64_part)
}

/// `[A-Za-z0-9+/]+={0,2}`.
fn is_base64_part(part: &str) -> bool {
    let body = part.trim_end_matches('=');
    let padding = part.len() - body.len();
    !body.is_empty()
        && padding <= 2
        && body
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'+' || b == b'/')
}

#[cfg(test)]
mod tests {
    use std::error::Error as _;

    use sekvent_error::ErrorCode;

    use super::*;

    const RFC_SALT: &str = "W22ZaJ0SNY7soEsUEjb6gQ==";
    const RFC_VERIFIER: &str = "SCRAM-SHA-256$4096:W22ZaJ0SNY7soEsUEjb6gQ==$\
        WG5d8oPm3OtcPnkdi4Uo7BkeZkBFzpcXkuLmtbsT4qY=:wfPLwcE6nTWhTAmQ7tl2KeoiWGPlZqQxSrmfPwDl2dU=";
    const DERIVED_PASSWORD: &str = "OYf0aWaTG6covLgwbEBL-T_7D3AQj2pe2UOV6g1yxFo";
    const DERIVED_VERIFIER: &str = "SCRAM-SHA-256$4096:AAECAwQFBgcICQoLDA0ODw==$\
        98uW697Y8lEucGv5BzeBdlex6wb8cVY3PbJ/KqnRVrA=:m1OgrUNfxaT6SaLMHrRbz5EIbQTeQ7sD4+gsA35RlMM=";

    fn decode(text: &str) -> Vec<u8> {
        STANDARD.decode(text).expect("test base64")
    }

    fn rfc_salt() -> [u8; 16] {
        decode(RFC_SALT).try_into().expect("a 16-byte salt")
    }

    fn counting_salt() -> [u8; 16] {
        std::array::from_fn(|i| u8::try_from(i).expect("below 16"))
    }

    #[test]
    fn the_rfc_7677_exchange_verifies_with_the_derived_keys() {
        let keys = Keys::derive(&Secret::new("pencil"), &rfc_salt());
        assert_eq!(
            STANDARD.encode(&keys.client[..]),
            "pg/JI9Z+hkSpLRa5btpe9GVrDHJcSEN0viVTVXaZbos="
        );
        let stored = keys.stored();
        assert_eq!(
            STANDARD.encode(&stored[..]),
            "WG5d8oPm3OtcPnkdi4Uo7BkeZkBFzpcXkuLmtbsT4qY="
        );
        assert_eq!(
            STANDARD.encode(&keys.server[..]),
            "wfPLwcE6nTWhTAmQ7tl2KeoiWGPlZqQxSrmfPwDl2dU="
        );

        let auth_message = "n=user,r=rOprNGfwEbeRWgbNEkqO,\
            r=rOprNGfwEbeRWgbNEkqO%hvYDpWUa2RaTCAfuxFIlj)hNlF$k0,s=W22ZaJ0SNY7soEsUEjb6gQ==,i=4096,\
            c=biws,r=rOprNGfwEbeRWgbNEkqO%hvYDpWUa2RaTCAfuxFIlj)hNlF$k0";
        let client_signature = hmac_sha256(&stored[..], &[auth_message.as_bytes()]);
        let proof: Vec<u8> = keys
            .client
            .iter()
            .zip(client_signature.iter())
            .map(|(key, signature)| key ^ signature)
            .collect();
        assert_eq!(
            STANDARD.encode(proof),
            "dHzbZapWIk4jUhN+Ute9ytag9zjfMHgsqmmiz7AndVQ="
        );
        let server_signature = hmac_sha256(&keys.server[..], &[auth_message.as_bytes()]);
        assert_eq!(
            STANDARD.encode(&server_signature[..]),
            "6rriTRBi23WpRR/wtup+mMhUZUn/dB5nLTJRsjl95G4="
        );
    }

    #[test]
    fn the_verifier_carries_the_stored_and_server_keys() {
        let verifier = ScramVerifier::with_salt(&Secret::new("pencil"), &rfc_salt());
        assert_eq!(verifier.text.expose(), RFC_VERIFIER);
    }

    #[test]
    fn a_fixed_salt_gives_a_stable_verifier() {
        let password = Secret::new(DERIVED_PASSWORD);
        let first = ScramVerifier::with_salt(&password, &counting_salt());
        let second = ScramVerifier::with_salt(&password, &counting_salt());
        assert_eq!(first.text.expose(), DERIVED_VERIFIER);
        assert_eq!(second.text.expose(), DERIVED_VERIFIER);
        assert!(is_scram_verifier(first.text.expose()));
    }

    #[test]
    fn a_random_salt_gives_a_fresh_verifier() {
        let password = Secret::new(DERIVED_PASSWORD);
        let first = ScramVerifier::new(&password).expect("the random source works");
        let second = ScramVerifier::new(&password).expect("the random source works");
        assert!(is_scram_verifier(first.text.expose()));
        assert_ne!(first.text.expose(), second.text.expose());
        let salt = first
            .text
            .expose()
            .strip_prefix("SCRAM-SHA-256$4096:")
            .and_then(|rest| rest.split_once('$'))
            .map(|(salt, _)| decode(salt))
            .expect("the verifier has a salt");
        assert_eq!(salt.len(), SALT_BYTES);
    }

    #[test]
    fn the_check_accepts_verifiers_of_its_grammar() {
        for text in [
            RFC_VERIFIER,
            DERIVED_VERIFIER,
            "SCRAM-SHA-256$4096:a2V5$a2V5:a2V5",
            "SCRAM-SHA-256$4096:c2FsdHNhbHRzYWx0c2FsdA==$a2V5:a2V5",
            "SCRAM-SHA-256$4096:a+/=$Zg==:Z",
        ] {
            assert!(is_scram_verifier(text), "{text}");
        }
    }

    #[test]
    fn the_check_admits_nothing_else() {
        for text in [
            "",
            "SCRAM-SHA-256$",
            "SCRAM-SHA-1$4096:a2V5$a2V5:a2V5",
            "scram-sha-256$4096:a2V5$a2V5:a2V5",
            "md5$4096:a2V5$a2V5:a2V5",
            "SCRAM-SHA-256$4095:a2V5$a2V5:a2V5",
            "SCRAM-SHA-256$04096:a2V5$a2V5:a2V5",
            "SCRAM-SHA-256$+4096:a2V5$a2V5:a2V5",
            "SCRAM-SHA-256$:a2V5$a2V5:a2V5",
            "SCRAM-SHA-256$4096",
            "SCRAM-SHA-256$4096:a2V5",
            "SCRAM-SHA-256$4096:a2V5$a2V5",
            "SCRAM-SHA-256$4096:a2V5===$a2V5:a2V5",
            "SCRAM-SHA-256$4096:a2V5$a2V5===:a2V5",
            "SCRAM-SHA-256$4096:a2V5$a2V5:a2V5===",
            "SCRAM-SHA-256$4096:$a2V5:a2V5",
            "SCRAM-SHA-256$4096:a2V5$:a2V5",
            "SCRAM-SHA-256$4096:a2V5$a2V5:",
            "SCRAM-SHA-256$4096:==$a2V5:a2V5",
            "SCRAM-SHA-256$4096:a2'V5$a2V5:a2V5",
            "SCRAM-SHA-256$4096:a2V5$a2V5:a2V5'",
            "SCRAM-SHA-256$4096:a2V5$a2V5:a2V5';DROP ROLE x;--",
            "SCRAM-SHA-256$4096:a2\\V5$a2V5:a2V5",
            " SCRAM-SHA-256$4096:a2V5$a2V5:a2V5",
            "xSCRAM-SHA-256$4096:a2V5$a2V5:a2V5",
            "SCRAM-SHA-256$4096:a2V5$a2V5:a2V5 ",
            "SCRAM-SHA-256$4096:a2V5$a2V5:a2V5\n",
            "SCRAM-SHA-256$4096:a2V5$a2V5:a2V5=x",
            "SCRAM-SHA-256$4096:a2V5$a2V5:a2V5$x",
            "SCRAM-SHA-256$4096:a2V5$a2V5:a2V5:x",
            "SCRAM-SHA-256$4096:a2V5$a2V5$a2V5:a2V5",
            "SCRAM-SHA-256$4096:a2-V5$a2V5:a2V5",
            "SCRAM-SHA-256$4096:a2_V5$a2V5:a2V5",
            "SCRAM-SHA-256$4096:a2é$a2V5:a2V5",
        ] {
            assert!(!is_scram_verifier(text), "{text}");
        }
    }

    #[test]
    fn parse_keeps_a_valid_text_and_refuses_others_without_showing_them() {
        let parsed = ScramVerifier::parse(Secret::new(RFC_VERIFIER)).expect("a valid verifier");
        assert_eq!(parsed.text.expose(), RFC_VERIFIER);

        let refused = "SCRAM-SHA-256$4096:a2V5$a2V5:a2V5'";
        let error = ScramVerifier::parse(Secret::new(refused)).expect_err("refused");
        assert_eq!(error.code(), ErrorCode::Internal);
        assert_eq!(error.reason(), None);
        let source = error.source().map(ToString::to_string).unwrap_or_default();
        let shown = format!("{error} {error:?} {source}");
        assert!(!shown.contains("a2V5"));
    }

    #[test]
    fn the_literal_is_quoted_once_and_cannot_be_escaped() {
        let parsed = ScramVerifier::parse(Secret::new(RFC_VERIFIER)).expect("a valid verifier");
        assert_eq!(parsed.literal().expose(), format!("'{RFC_VERIFIER}'"));

        let built = ScramVerifier::with_salt(&Secret::new("it's a 'quoted' password"), &rfc_salt());
        let password = Secret::new(DERIVED_PASSWORD);
        let random = ScramVerifier::new(&password).expect("the random source works");
        for verifier in [parsed, built, random] {
            let literal = verifier.literal();
            let literal = literal.expose();
            let inner = literal
                .strip_prefix('\'')
                .and_then(|rest| rest.strip_suffix('\''))
                .expect("the literal is quoted");
            assert!(is_scram_verifier(inner));
            assert!(
                inner.bytes().all(|b| b.is_ascii_alphanumeric()
                    || matches!(b, b'+' | b'/' | b'=' | b'-' | b':' | b'$')),
                "the literal holds only the verifier alphabet"
            );
            assert_eq!(literal.matches('\'').count(), 2);
            assert!(!literal.contains('\\'));
        }
    }

    #[test]
    fn debug_shows_no_secret_material() {
        let verifier = ScramVerifier::with_salt(&Secret::new("pencil"), &rfc_salt());
        let debug = format!("{verifier:?}");
        assert_eq!(debug, "ScramVerifier(..)");
        assert!(!debug.contains("SCRAM-SHA-256"));
        assert_eq!(format!("{:?}", verifier.literal()), "[redacted]");
        assert_eq!(verifier.literal().to_string(), "[redacted]");
    }
}
