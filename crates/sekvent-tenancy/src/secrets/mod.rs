//! Role secrets: a tenant role's password, derived from a master secret by
//! key id and generation and stored nowhere, and (with `lifecycle`) the
//! SCRAM verifier a role is created with.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use sekvent_config::Secret;
use sekvent_error::AppError;
use zeroize::Zeroizing;

use crate::{TenantId, is_key_id};

mod kdf;
#[cfg(feature = "lifecycle")]
mod scram;

#[cfg(feature = "lifecycle")]
pub use scram::{SCRAM_ITERATIONS, ScramVerifier, is_scram_verifier};

/// The fewest bytes a master secret may have.
pub const MASTER_SECRET_MIN_BYTES: usize = 32;

/// The longest context string.
const CONTEXT_MAX: usize = 64;

/// The bytes of HKDF output a role password encodes.
const PASSWORD_BYTES: usize = 32;

/// Which master secret and which generation a role's password comes from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RoleKey {
    /// The id of the master secret, as configured.
    pub key_id: String,
    /// The generation of the password under that secret, from 1.
    pub generation: u32,
}

/// The master secrets by key id, and the product's context string.
///
/// Clones share the keys. `Debug` prints the number of keys.
#[derive(Clone)]
pub struct RoleSecrets {
    /// The master secret of each key id.
    keys: Arc<BTreeMap<String, Secret>>,
    /// The product's context string, part of every derivation's info.
    context: Arc<str>,
}

impl RoleSecrets {
    /// `text` is `key_id:secret[,key_id:secret…]`; `context` is
    /// `^[A-Za-z0-9._-]{1,64}$`. `INVALID_ARGUMENT`, naming the position of
    /// the refused entry and never its key id or value.
    ///
    /// The text is split into entries at every `,`, so a secret cannot
    /// contain a comma. Each entry is trimmed as a whole and split at its
    /// first `:`; later colons belong to the secret. A key id passes
    /// [`is_key_id`] and occurs once; a secret has at least
    /// [`MASTER_SECRET_MIN_BYTES`] bytes, does
    /// not contain `change-me` in any letter case and is not one character
    /// repeated.
    pub fn parse(text: &Secret, context: &str) -> Result<Self, AppError> {
        if !is_context(context) {
            return Err(AppError::invalid_argument(
                "the role-secret context must be 1 to 64 ASCII letters, digits, dots, \
                 underscores or hyphens",
            ));
        }
        let mut keys = BTreeMap::new();
        for (index, entry) in text.expose().split(',').enumerate() {
            let position = index + 1;
            let Some((key_id, secret)) = entry.trim().split_once(':') else {
                return Err(refused(position, "is not of the form key_id:secret"));
            };
            if !is_key_id(key_id) {
                return Err(refused(position, "has an invalid key id"));
            }
            if keys.contains_key(key_id) {
                return Err(refused(position, "repeats the key id of an earlier entry"));
            }
            if secret.len() < MASTER_SECRET_MIN_BYTES {
                return Err(refused(position, "has a secret shorter than 32 bytes"));
            }
            if is_placeholder(secret) {
                return Err(refused(position, "has a placeholder secret"));
            }
            if is_one_character(secret) {
                return Err(refused(position, "has a secret of one repeated character"));
            }
            keys.insert(key_id.to_owned(), Secret::new(secret));
        }
        Ok(Self {
            keys: Arc::new(keys),
            context: Arc::from(context),
        })
    }

    /// Whether a master secret is configured under `key_id`.
    pub fn contains(&self, key_id: &str) -> bool {
        self.keys.contains_key(key_id)
    }

    /// The number of master secrets.
    pub fn len(&self) -> usize {
        self.keys.len()
    }

    /// Whether no master secret is configured.
    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }

    /// The role password of `tenant` under `key`. `INTERNAL` for a key id
    /// that is not configured or generation 0.
    ///
    /// HKDF-SHA-256 with the secret's bytes as input key material, the 16
    /// bytes of the tenant's UUID as salt and `<context>:<key_id>:<generation>`
    /// as info; the 32 output bytes in base64url without padding (43
    /// characters).
    pub fn derive(&self, tenant: TenantId, key: &RoleKey) -> Result<Secret, AppError> {
        if key.generation == 0 {
            return Err(AppError::internal(
                "a role key of generation 0 derives no password",
            ));
        }
        let Some(secret) = self.keys.get(&key.key_id) else {
            return Err(AppError::internal(
                "the role key names a master secret that is not configured",
            ));
        };
        let info = format!("{}:{}:{}", self.context, key.key_id, key.generation);
        let mut okm = Zeroizing::new([0u8; PASSWORD_BYTES]);
        kdf::hkdf_sha256(
            secret.expose().as_bytes(),
            tenant.as_uuid().as_bytes(),
            info.as_bytes(),
            &mut okm[..],
        );
        Ok(Secret::new(URL_SAFE_NO_PAD.encode(&okm[..])))
    }
}

impl fmt::Debug for RoleSecrets {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RoleSecrets")
            .field("keys", &self.keys.len())
            .finish_non_exhaustive()
    }
}

/// `^[A-Za-z0-9._-]{1,64}$`.
fn is_context(text: &str) -> bool {
    (1..=CONTEXT_MAX).contains(&text.len())
        && text
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// Contains `change-me` in any letter case.
fn is_placeholder(secret: &str) -> bool {
    const PLACEHOLDER: &[u8] = b"change-me";
    secret
        .as_bytes()
        .windows(PLACEHOLDER.len())
        .any(|window| window.eq_ignore_ascii_case(PLACEHOLDER))
}

/// One character repeated (an empty text counts too).
fn is_one_character(secret: &str) -> bool {
    let mut chars = secret.chars();
    chars
        .next()
        .is_none_or(|first| chars.all(|other| other == first))
}

/// A refusal of the entry at `position` (from 1); never its key id or value.
fn refused(position: usize, what: &str) -> AppError {
    AppError::invalid_argument(format!("role secrets: entry {position} {what}"))
}

#[cfg(test)]
mod tests {
    use std::error::Error as _;

    use sekvent_error::ErrorCode;
    use uuid::Uuid;

    use super::*;

    const SECRET: &str = "correct horse battery staple 936";
    const OTHER_SECRET: &str = "another secret value for testing";
    const CONTEXT: &str = "example-app.role.v1";
    const TENANT: &str = "0f8fad5b-d9cb-469f-a165-70867728950e";
    const PASSWORD: &str = "OYf0aWaTG6covLgwbEBL-T_7D3AQj2pe2UOV6g1yxFo";

    fn tenant(text: &str) -> TenantId {
        TenantId::from_uuid(Uuid::parse_str(text).expect("a test UUID"))
    }

    fn key(key_id: &str, generation: u32) -> RoleKey {
        RoleKey {
            key_id: key_id.to_owned(),
            generation,
        }
    }

    fn secrets(text: &str, context: &str) -> RoleSecrets {
        RoleSecrets::parse(&Secret::new(text), context).expect("the test secrets parse")
    }

    fn derive(text: &str, context: &str, tenant_id: &str, role_key: &RoleKey) -> String {
        secrets(text, context)
            .derive(tenant(tenant_id), role_key)
            .expect("the password derives")
            .expose()
            .to_owned()
    }

    fn refusal(text: &str) -> AppError {
        RoleSecrets::parse(&Secret::new(text), CONTEXT).expect_err("the text is refused")
    }

    /// Everything an error shows: message, `Display`, `Debug` and source.
    fn shown(error: &AppError) -> String {
        let source = error.source().map(ToString::to_string).unwrap_or_default();
        format!("{} | {error} | {error:?} | {source}", error.message())
    }

    #[test]
    fn the_derivation_matches_the_committed_vector() {
        let password = derive(&format!("k1:{SECRET}"), CONTEXT, TENANT, &key("k1", 1));
        assert_eq!(password, PASSWORD);
        assert_eq!(password.len(), 43);
    }

    #[test]
    fn every_input_changes_the_password() {
        let k1 = format!("k1:{SECRET}");
        let cases = [
            (
                derive(
                    &k1,
                    CONTEXT,
                    "7c9e6679-7425-40de-944b-e07fc1f90ae7",
                    &key("k1", 1),
                ),
                "C2Hr0_j2_Fj1WIKRx1WxUCoyEL_EUE-C7qcgPN8MeOI",
            ),
            (
                derive(&format!("k2:{SECRET}"), CONTEXT, TENANT, &key("k2", 1)),
                "12fYh6Rupx4bAxCCvqPakKqMO7DPRv94vNvrnEf9SFo",
            ),
            (
                derive(&k1, CONTEXT, TENANT, &key("k1", 2)),
                "uGqhZ-kUP9vpXyC3HSAm-JbnKGksS3Iwbr-aGIsRtmQ",
            ),
            (
                derive(
                    &format!("k1:{OTHER_SECRET}"),
                    CONTEXT,
                    TENANT,
                    &key("k1", 1),
                ),
                "leg_mIwtKG2yZCAyPTbmNeybPkPeb0sAQlYj8GvL7Uw",
            ),
            (
                derive(&k1, "example-app.role.v2", TENANT, &key("k1", 1)),
                "-htXNg0BAeDyyq5IBkJjuAMB9hXTAB7SJmhvDjYWyKk",
            ),
        ];
        for (password, expected) in cases {
            assert_eq!(password, expected);
            assert_eq!(password.len(), 43);
            assert_ne!(password, PASSWORD);
        }
    }

    #[test]
    fn each_key_id_uses_its_own_secret() {
        let both = format!("k1:{SECRET},k2:{OTHER_SECRET}");
        assert_eq!(derive(&both, CONTEXT, TENANT, &key("k1", 1)), PASSWORD);
        assert_eq!(
            derive(&both, CONTEXT, TENANT, &key("k2", 1)),
            derive(
                &format!("k2:{OTHER_SECRET}"),
                CONTEXT,
                TENANT,
                &key("k2", 1)
            )
        );
    }

    #[test]
    fn generation_zero_and_an_unknown_key_id_are_internal() {
        let secrets = secrets(&format!("k1:{SECRET}"), CONTEXT);
        for role_key in [key("k1", 0), key("k2", 1), key("", 1)] {
            let error = secrets
                .derive(tenant(TENANT), &role_key)
                .expect_err("no password");
            assert_eq!(error.code(), ErrorCode::Internal);
            assert_eq!(error.reason(), None);
            assert!(!shown(&error).contains(SECRET));
        }
    }

    #[test]
    fn parse_accepts_entries_and_trims_each_as_a_whole() {
        let text = format!("  k1:{SECRET} ,\tk_2-x:{OTHER_SECRET}\n");
        let parsed = secrets(&text, CONTEXT);
        assert_eq!(parsed.len(), 2);
        assert!(!parsed.is_empty());
        assert!(parsed.contains("k1"));
        assert!(parsed.contains("k_2-x"));
        assert!(!parsed.contains("k3"));
        assert_eq!(parsed.keys["k1"].expose(), SECRET);
        assert_eq!(parsed.keys["k_2-x"].expose(), OTHER_SECRET);
        assert_eq!(&*parsed.context, CONTEXT);
    }

    #[test]
    fn colons_after_the_first_and_inner_spaces_belong_to_the_secret() {
        let secret = "billing:orders:inventory:0123456789";
        let parsed = secrets(&format!("k1:{secret}"), CONTEXT);
        assert_eq!(parsed.keys["k1"].expose(), secret);
        let spaced = secrets(&format!("k1: {SECRET}"), CONTEXT);
        assert_eq!(spaced.keys["k1"].expose(), format!(" {SECRET}"));
        assert_ne!(
            spaced
                .derive(tenant(TENANT), &key("k1", 1))
                .unwrap()
                .expose(),
            PASSWORD
        );
    }

    #[test]
    fn parse_refusals_name_the_position_only() {
        let good = format!("k1:{SECRET}");
        let cases = [
            (String::new(), 1, "key_id:secret"),
            (format!("{good},"), 2, "key_id:secret"),
            (format!("{good},k2"), 2, "key_id:secret"),
            (format!("k 1:{SECRET}"), 1, "invalid key id"),
            (format!("k1 :{SECRET}"), 1, "invalid key id"),
            (format!(":{SECRET}"), 1, "invalid key id"),
            (format!("{}:{SECRET}", "k".repeat(33)), 1, "invalid key id"),
            (format!("{good},k1:{OTHER_SECRET}"), 2, "repeats"),
            (
                "k1:0123456789abcdef0123456789abcde".to_owned(),
                1,
                "shorter than 32",
            ),
            (format!("{good},k2:"), 2, "shorter than 32"),
            (format!("k1:{}CHANGE-me", "x1".repeat(12)), 1, "placeholder"),
            (
                format!("{good},k2:change-me{}", "y2".repeat(12)),
                2,
                "placeholder",
            ),
            (
                format!("k1:{}", "z".repeat(40)),
                1,
                "one repeated character",
            ),
            (
                format!("{good},k2:{}", "é".repeat(20)),
                2,
                "one repeated character",
            ),
        ];
        for (text, position, what) in cases {
            let error = refusal(&text);
            assert_eq!(
                error.code(),
                ErrorCode::InvalidArgument,
                "{position} {what}"
            );
            assert_eq!(error.reason(), None);
            assert!(
                error.message().contains(&format!("entry {position} ")),
                "{}",
                error.message()
            );
            assert!(error.message().contains(what), "{}", error.message());
            let shown = shown(&error);
            for part in text.split([',', ':']) {
                let part = part.trim();
                if part.len() > 1 && part != "k" {
                    assert!(
                        !shown.contains(part),
                        "the refusal shows no key id or value"
                    );
                }
            }
        }
    }

    #[test]
    fn secrets_of_two_characters_and_exactly_32_bytes_pass() {
        let parsed = secrets(&format!("k1:{}", "ab".repeat(16)), CONTEXT);
        assert_eq!(parsed.keys["k1"].expose().len(), MASTER_SECRET_MIN_BYTES);
        let multibyte = secrets(&format!("k1:{}", "éa".repeat(11)), CONTEXT);
        assert_eq!(multibyte.len(), 1);
    }

    #[test]
    fn the_context_grammar_is_checked() {
        let text = format!("k1:{SECRET}");
        let longest = "x".repeat(64);
        let too_long = "x".repeat(65);
        for context in ["a", "A.b_c-9", longest.as_str()] {
            assert_eq!(secrets(&text, context).len(), 1);
        }
        for context in [
            "",
            too_long.as_str(),
            "example-app/role/v1",
            "a b",
            "a:b",
            "é",
        ] {
            let error = RoleSecrets::parse(&Secret::new(text.as_str()), context)
                .expect_err("the context is refused");
            assert_eq!(error.code(), ErrorCode::InvalidArgument);
            assert!(!shown(&error).contains(SECRET));
        }
    }

    #[test]
    fn debug_prints_the_number_of_keys_only() {
        let parsed = secrets(&format!("k1:{SECRET},k2:{OTHER_SECRET}"), CONTEXT);
        let debug = format!("{parsed:?}");
        assert_eq!(debug, "RoleSecrets { keys: 2, .. }");
        assert!(!debug.contains("k1"));
        assert!(!debug.contains(SECRET));
    }

    #[test]
    fn clones_share_the_keys() {
        let parsed = secrets(&format!("k1:{SECRET}"), CONTEXT);
        let clone = parsed.clone();
        assert!(Arc::ptr_eq(&parsed.keys, &clone.keys));
        assert!(Arc::ptr_eq(&parsed.context, &clone.context));
    }

    #[test]
    fn the_helpers_follow_their_rules() {
        assert!(is_one_character(""));
        assert!(is_one_character("q"));
        assert!(!is_one_character("qr"));
        assert!(is_placeholder("xxCHANGE-MExx"));
        assert!(!is_placeholder("change_me"));
        assert!(!is_placeholder("change"));
    }
}
