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
/// Postgres accepts other counts; exactly 4096 is this crate's policy.
pub const SCRAM_ITERATIONS: u32 = 4096;

/// The mechanism name that opens every verifier.
const HEAD: &str = "SCRAM-SHA-256$";

/// The bytes of a random salt.
const SALT_BYTES: usize = 16;

/// The bytes of a decoded `StoredKey` or `ServerKey`: one SHA-256.
const KEY_BYTES: usize = 32;

/// The characters of an encoded `StoredKey` or `ServerKey`: 32 bytes in
/// padded standard base64 are 43 characters and one `=`.
const KEY_CHARS: usize = 44;

/// The most characters an encoded salt may have. Postgres sets no bound; this
/// crate makes 16-byte salts (24 characters) and refuses a longer salt than
/// 1024 characters before decoding it, so a check never decodes more than
/// [`MAX_DECODED_BYTES`].
const MAX_SALT_CHARS: usize = 1024;

/// The most bytes a part within [`MAX_SALT_CHARS`] decodes to.
const MAX_DECODED_BYTES: usize = MAX_SALT_CHARS / 4 * 3;

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

    /// A verifier made elsewhere, after the check of [`is_scram_verifier`]:
    /// its grammar with exactly 4096 iterations, a salt of at most
    /// 1024 characters that decodes, and two keys of 32 bytes each.
    /// `INTERNAL` otherwise, naming nothing of the text.
    pub fn parse(text: Secret) -> Result<Self, AppError> {
        if is_scram_verifier(text.expose()) {
            Ok(Self { text })
        } else {
            Err(AppError::internal("a SCRAM verifier failed the check"))
        }
    }

    /// The verifier text, `SCRAM-SHA-256$4096:<salt>$<StoredKey>:<ServerKey>`,
    /// as [`Secret::expose`] gives it: for storing or comparing it, never for
    /// a log or an error. `Debug` still prints nothing of it.
    pub fn expose(&self) -> &str {
        self.text.expose()
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
/// Postgres stores a text as a SCRAM verifier only when it parses as one: the
/// salt decodes and both keys decode to 32 bytes. It hashes any other text as
/// a plaintext password, so a text that fails that part here would not act as
/// a verifier there. This check therefore decodes every part (standard
/// base64, canonical padding and trailing bits): the salt must give at least
/// one byte and both keys exactly 32.
///
/// The rest is this crate's own policy, stricter than Postgres: exactly
/// [`SCRAM_ITERATIONS`] iterations spelled `4096`, canonical standard base64,
/// and a salt of at most 1024 characters. The lengths are checked before any
/// decoding, so an oversized part is refused without being decoded, and the
/// decoded bytes sit in a buffer wiped on drop.
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
    if salt.len() > MAX_SALT_CHARS || stored.len() != KEY_CHARS || server.len() != KEY_CHARS {
        return false;
    }
    if ![salt, stored, server].into_iter().all(is_base64_part) {
        return false;
    }
    decoded_len(salt).is_some_and(|len| len > 0)
        && decoded_len(stored) == Some(KEY_BYTES)
        && decoded_len(server) == Some(KEY_BYTES)
}

/// The bytes `part` decodes to, `None` when it does not decode. `part` has at
/// most [`MAX_SALT_CHARS`] characters; the bytes land in a stack buffer that
/// is wiped on drop, also when the decoding fails halfway.
fn decoded_len(part: &str) -> Option<usize> {
    let mut buffer = Zeroizing::new([0u8; MAX_DECODED_BYTES]);
    STANDARD.decode_slice(part, &mut buffer[..]).ok()
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
    const RFC_STORED: &str = "WG5d8oPm3OtcPnkdi4Uo7BkeZkBFzpcXkuLmtbsT4qY=";
    const RFC_SERVER: &str = "wfPLwcE6nTWhTAmQ7tl2KeoiWGPlZqQxSrmfPwDl2dU=";
    const RFC_VERIFIER: &str = "SCRAM-SHA-256$4096:W22ZaJ0SNY7soEsUEjb6gQ==$\
        WG5d8oPm3OtcPnkdi4Uo7BkeZkBFzpcXkuLmtbsT4qY=:wfPLwcE6nTWhTAmQ7tl2KeoiWGPlZqQxSrmfPwDl2dU=";
    const DERIVED_PASSWORD: &str = "OYf0aWaTG6covLgwbEBL-T_7D3AQj2pe2UOV6g1yxFo";
    const DERIVED_VERIFIER: &str = "SCRAM-SHA-256$4096:AAECAwQFBgcICQoLDA0ODw==$\
        98uW697Y8lEucGv5BzeBdlex6wb8cVY3PbJ/KqnRVrA=:m1OgrUNfxaT6SaLMHrRbz5EIbQTeQ7sD4+gsA35RlMM=";
    /// 31, 32 and 33 bytes, standard base64.
    const KEY_31: &str = "AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHg==";
    const KEY_32: &str = "AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8=";
    const KEY_33: &str = "AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8g";
    /// [`KEY_32`] without its padding: 43 characters.
    const KEY_32_UNPADDED: &str = "AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8";
    /// [`KEY_32`] with a set trailing bit: 44 characters of the alphabet that
    /// do not decode canonically.
    const KEY_32_TRAILING_BITS: &str = "AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh9=";

    fn verifier(salt: &str, stored: &str, server: &str) -> String {
        format!("SCRAM-SHA-256$4096:{salt}${stored}:{server}")
    }

    /// The RFC 7677 verifier's parts under another head and iteration count.
    fn rfc_with(head: &str, iterations: &str) -> String {
        format!("{head}{iterations}:{RFC_SALT}${RFC_STORED}:{RFC_SERVER}")
    }

    /// `part` with the ASCII character at `at` replaced by `with`.
    fn swap(part: &str, at: usize, with: &str) -> String {
        format!("{}{with}{}", &part[..at], &part[at + 1..])
    }

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
    fn the_check_accepts_verifiers_of_its_grammar_with_32_byte_keys() {
        for text in [
            RFC_VERIFIER.to_owned(),
            DERIVED_VERIFIER.to_owned(),
            // Salts of 1, 3, 5 and 12 bytes: any decodable salt will do.
            verifier("AA==", KEY_32, KEY_32),
            verifier("a2V5", KEY_32, KEY_32),
            verifier("AAECAwQ=", KEY_32, KEY_32),
            verifier("c2FsdHNhbHRzYWx0", KEY_32, KEY_32),
            verifier("c2FsdHNhbHRzYWx0c2FsdA==", KEY_32, KEY_32),
            verifier(RFC_SALT, KEY_32, KEY_32),
            // Salts of 1024 characters, the bound: 768 and 766 bytes.
            verifier(&"AAAA".repeat(256), KEY_32, KEY_32),
            verifier(
                &format!("{}AA==", "AAAA".repeat(255)),
                RFC_STORED,
                RFC_SERVER,
            ),
        ] {
            assert!(is_scram_verifier(&text), "{text}");
        }
    }

    #[test]
    fn the_check_refuses_parts_that_do_not_decode_to_their_lengths() {
        for text in [
            // Short keys: the grammar holds, the encoded lengths do not.
            verifier("a2V5", "a2V5", "a2V5"),
            verifier("c2FsdHNhbHRzYWx0c2FsdA==", "a2V5", "a2V5"),
            verifier("a+/=", "Zg==", "Z"),
            // 44 characters each, decoding to 31 or 33 bytes.
            verifier("a2V5", KEY_31, KEY_32),
            verifier("a2V5", KEY_32, KEY_31),
            verifier("a2V5", KEY_33, KEY_32),
            verifier("a2V5", KEY_32, KEY_33),
            // Each key of the wrong encoded length on its own: 4, 43 and 45
            // characters.
            verifier("a2V5", "a2V5", KEY_32),
            verifier("a2V5", KEY_32, "a2V5"),
            verifier("a2V5", KEY_32_UNPADDED, KEY_32),
            verifier("a2V5", KEY_32, KEY_32_UNPADDED),
            verifier("a2V5", &format!("{KEY_32}A"), KEY_32),
            verifier("a2V5", KEY_32, &format!("{KEY_32}A")),
            verifier(RFC_SALT, &format!("{RFC_STORED}="), RFC_SERVER),
            // The alphabet and the lengths hold, the decoding fails.
            verifier("a2V", KEY_32, KEY_32),
            verifier("Zg=", KEY_32, KEY_32),
            verifier("Zh==", KEY_32, KEY_32),
            verifier("a2V5", KEY_32_TRAILING_BITS, KEY_32),
            verifier("a2V5", KEY_32, KEY_32_TRAILING_BITS),
            // A salt over 1024 characters, though it would decode.
            verifier(
                &format!("{}AA==", "AAAA".repeat(256)),
                RFC_STORED,
                RFC_SERVER,
            ),
            verifier(&"AAAA".repeat(512), KEY_32, KEY_32),
        ] {
            assert!(!is_scram_verifier(&text), "{text}");
        }
    }

    /// Each case is the RFC 7677 verifier with one thing changed; the lengths
    /// stay right, so each is refused by its own rule.
    #[test]
    #[allow(clippy::too_many_lines)]
    fn the_check_admits_nothing_else() {
        let head = "SCRAM-SHA-256$";
        assert!(is_scram_verifier(&rfc_with(head, "4096")));
        for text in [
            String::new(),
            head.to_owned(),
            // The mechanism.
            rfc_with("SCRAM-SHA-1$", "4096"),
            rfc_with("scram-sha-256$", "4096"),
            rfc_with("md5$", "4096"),
            rfc_with(" SCRAM-SHA-256$", "4096"),
            rfc_with("xSCRAM-SHA-256$", "4096"),
            // The iterations.
            rfc_with(head, "4095"),
            rfc_with(head, "40960"),
            rfc_with(head, "04096"),
            rfc_with(head, "+4096"),
            rfc_with(head, "4096 "),
            rfc_with(head, ""),
            // Missing parts and separators.
            "SCRAM-SHA-256$4096".to_owned(),
            format!("SCRAM-SHA-256$4096:{RFC_SALT}"),
            format!("SCRAM-SHA-256$4096:{RFC_SALT}${RFC_STORED}"),
            format!("SCRAM-SHA-256$4096:{RFC_SALT}{RFC_STORED}:{RFC_SERVER}"),
            // Empty parts.
            verifier("", RFC_STORED, RFC_SERVER),
            verifier("==", RFC_STORED, RFC_SERVER),
            // Padding: three `=`, or `=` inside a part.
            verifier(&format!("{RFC_SALT}="), RFC_STORED, RFC_SERVER),
            verifier(RFC_SALT, &format!("{}===", &RFC_STORED[..41]), RFC_SERVER),
            verifier(RFC_SALT, RFC_STORED, &format!("{}===", &RFC_SERVER[..41])),
            verifier(&swap(RFC_SALT, 4, "="), RFC_STORED, RFC_SERVER),
            verifier(RFC_SALT, RFC_STORED, &swap(RFC_SERVER, 20, "=")),
            // Quotes and statements.
            verifier(&swap(RFC_SALT, 2, "'"), RFC_STORED, RFC_SERVER),
            verifier(RFC_SALT, RFC_STORED, &swap(RFC_SERVER, 43, "'")),
            verifier(
                &format!("{RFC_SALT}';DROP ROLE x;--"),
                RFC_STORED,
                RFC_SERVER,
            ),
            verifier(
                RFC_SALT,
                RFC_STORED,
                &format!("{}';DROP ROLE x;--", &RFC_SERVER[..28]),
            ),
            // Backslashes.
            verifier(&swap(RFC_SALT, 2, "\\"), RFC_STORED, RFC_SERVER),
            verifier(RFC_SALT, &swap(RFC_STORED, 10, "\\"), RFC_SERVER),
            // Whitespace.
            verifier(&format!(" {RFC_SALT}"), RFC_STORED, RFC_SERVER),
            verifier(&format!("{RFC_SALT}\n"), RFC_STORED, RFC_SERVER),
            verifier(RFC_SALT, RFC_STORED, &swap(RFC_SERVER, 43, " ")),
            verifier(RFC_SALT, RFC_STORED, &swap(RFC_SERVER, 43, "\n")),
            verifier(RFC_SALT, &swap(RFC_STORED, 0, "\t"), RFC_SERVER),
            // Separators inside a part.
            verifier(&swap(RFC_SALT, 4, ":"), RFC_STORED, RFC_SERVER),
            verifier(RFC_SALT, &swap(RFC_STORED, 20, "$"), RFC_SERVER),
            verifier(RFC_SALT, &swap(RFC_STORED, 0, "$"), RFC_SERVER),
            verifier(RFC_SALT, RFC_STORED, &swap(RFC_SERVER, 20, ":")),
            verifier(RFC_SALT, RFC_STORED, &swap(RFC_SERVER, 20, "$")),
            // URL-safe base64 and non-ASCII.
            verifier(&swap(RFC_SALT, 2, "-"), RFC_STORED, RFC_SERVER),
            verifier(&swap(RFC_SALT, 2, "_"), RFC_STORED, RFC_SERVER),
            verifier(RFC_SALT, &swap(RFC_STORED, 10, "-"), RFC_SERVER),
            verifier(RFC_SALT, RFC_STORED, &swap(RFC_SERVER, 10, "_")),
            verifier(&swap(RFC_SALT, 2, "é"), RFC_STORED, RFC_SERVER),
            verifier(
                RFC_SALT,
                &format!("{}é{}", &RFC_STORED[..10], &RFC_STORED[12..]),
                RFC_SERVER,
            ),
        ] {
            assert!(!is_scram_verifier(&text), "{text}");
        }
    }

    #[test]
    fn parse_keeps_a_valid_text_and_refuses_others_without_showing_them() {
        let parsed = ScramVerifier::parse(Secret::new(RFC_VERIFIER)).expect("a valid verifier");
        assert_eq!(parsed.text.expose(), RFC_VERIFIER);

        for refused in [
            format!("{RFC_VERIFIER}'"),
            verifier(RFC_SALT, RFC_STORED, &swap(RFC_SERVER, 43, "'")),
            verifier("a2V5", KEY_31, KEY_32),
            verifier("a2V5", KEY_32, KEY_32_UNPADDED),
        ] {
            let error = ScramVerifier::parse(Secret::new(refused)).expect_err("refused");
            assert_eq!(error.code(), ErrorCode::Internal);
            assert_eq!(error.reason(), None);
            let source = error.source().map(ToString::to_string).unwrap_or_default();
            let shown = format!("{error} {error:?} {source}");
            for part in ["a2V5", "AAECAw", "W22Z", "WG5d", "wfPL"] {
                assert!(!shown.contains(part), "{part}");
            }
        }
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
    fn expose_round_trips_through_parse() {
        let built = ScramVerifier::with_salt(&Secret::new("pencil"), &rfc_salt());
        assert_eq!(built.expose(), RFC_VERIFIER);
        let parsed =
            ScramVerifier::parse(Secret::new(built.expose())).expect("an exposed verifier parses");
        assert_eq!(parsed.expose(), built.expose());
        assert_eq!(parsed.literal().expose(), format!("'{}'", built.expose()));
        assert_eq!(format!("{parsed:?}"), "ScramVerifier(..)");
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
