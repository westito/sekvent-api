//! Password hashing with argon2id, and verification of argon2 and legacy
//! bcrypt hashes.
//!
//! New hashes are always argon2id PHC strings. Stored values in any of these
//! shapes verify:
//!
//! | Stored value | Result when the password matches |
//! |---|---|
//! | `$argon2id$v=19$…` at or above the configured cost | [`Verification::Valid`] |
//! | `$argon2id$…` below the configured cost, `$argon2i$…`, `$argon2d$…` | [`Verification::ValidNeedsRehash`] |
//! | `{argon2}$argon2…` (delegating-encoder prefix) | [`Verification::ValidNeedsRehash`] |
//! | `$2a$…`, `$2b$…`, `$2y$…` (bcrypt) | [`Verification::ValidNeedsRehash`] |
//! | `{bcrypt}$2a$…` (delegating-encoder prefix) | [`Verification::ValidNeedsRehash`] |
//!
//! Anything else — unknown prefixes, malformed strings, `$2x$` — is
//! [`Verification::Invalid`] and logged at `warn` without the value.
//! Nothing in this module panics on a bad stored value.

use std::fmt;
use std::hint::black_box;

use argon2::password_hash::Error as HashError;
use argon2::{
    Algorithm, Argon2, Params, PasswordHash, PasswordHasher as _, PasswordVerifier as _, Version,
};
use sekvent_error::AppError;

/// Length of the derived hash in bytes.
const OUTPUT_LEN: usize = 32;
/// Length of a generated salt in bytes.
const SALT_LEN: usize = 16;
/// Stored argon2 hashes asking for more memory than this are refused rather
/// than allowed to exhaust the process.
const MAX_VERIFY_MEMORY_KIB: u32 = 1024 * 1024;

const ARGON2_PREFIX: &str = "{argon2}";
const BCRYPT_PREFIX: &str = "{bcrypt}";
const BCRYPT_VERSIONS: [&str; 3] = ["$2a$", "$2b$", "$2y$"];

/// 16 zero bytes, unpadded base64.
const DUMMY_SALT_B64: &str = "AAAAAAAAAAAAAAAAAAAAAA";
/// 32 zero bytes, unpadded base64.
const DUMMY_OUTPUT_B64: &str = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";

/// Argon2id cost parameters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PasswordParams {
    /// Memory cost in KiB (`m`).
    pub memory_kib: u32,
    /// Number of passes (`t`).
    pub iterations: u32,
    /// Degree of parallelism (`p`).
    pub parallelism: u32,
}

impl PasswordParams {
    /// The OWASP password-storage recommendation for argon2id:
    /// 19 MiB of memory, 2 iterations, parallelism 1.
    pub const OWASP: Self = Self {
        memory_kib: 19 * 1024,
        iterations: 2,
        parallelism: 1,
    };
}

impl Default for PasswordParams {
    fn default() -> Self {
        Self::OWASP
    }
}

/// Outcome of checking a password against a stored hash.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[must_use]
pub enum Verification {
    /// The password matches and the stored hash is current.
    Valid,
    /// The password matches, but the stored hash uses a legacy format or
    /// weaker parameters: store [`PasswordHasher::hash`] of the password.
    ValidNeedsRehash,
    /// The password does not match, or the stored value is unusable.
    Invalid,
}

impl Verification {
    /// Whether the password matched.
    pub fn is_valid(self) -> bool {
        !matches!(self, Self::Invalid)
    }

    /// Whether the password matched and the stored hash should be replaced.
    pub fn needs_rehash(self) -> bool {
        matches!(self, Self::ValidNeedsRehash)
    }
}

/// Hashes passwords with argon2id and verifies stored hashes.
///
/// Build it once and share it; it holds no secrets.
#[derive(Clone)]
pub struct PasswordHasher {
    params: PasswordParams,
    argon2: Argon2<'static>,
    dummy: PasswordHash,
}

impl PasswordHasher {
    /// A hasher producing argon2id hashes with `params`.
    ///
    /// Fails when argon2 rejects the parameters (for example less than
    /// 8 KiB of memory per lane, or zero iterations).
    pub fn new(params: PasswordParams) -> Result<Self, AppError> {
        let argon_params = Params::new(
            params.memory_kib,
            params.iterations,
            params.parallelism,
            Some(OUTPUT_LEN),
        )
        .map_err(|error| {
            AppError::invalid_argument(format!("invalid argon2id parameters: {error}"))
        })?;
        // A well-formed hash at the configured cost that no password matches:
        // checking against it costs exactly as much as a real verification.
        let dummy = PasswordHash::new(&format!(
            "$argon2id$v=19$m={},t={},p={}${DUMMY_SALT_B64}${DUMMY_OUTPUT_B64}",
            params.memory_kib, params.iterations, params.parallelism
        ))
        .map_err(|error| AppError::internal(error.to_string()))?;
        Ok(Self {
            params,
            argon2: Argon2::new(Algorithm::Argon2id, Version::V0x13, argon_params),
            dummy,
        })
    }

    /// The configured cost parameters.
    pub fn params(&self) -> PasswordParams {
        self.params
    }

    /// Hash `password` into an argon2id PHC string with a fresh random salt.
    pub fn hash(&self, password: &str) -> Result<String, AppError> {
        let mut salt = [0_u8; SALT_LEN];
        getrandom::fill(&mut salt).map_err(|error| AppError::internal(error.to_string()))?;
        let hash = self
            .argon2
            .hash_password_with_salt(password.as_bytes(), &salt)
            .map_err(|error| AppError::internal(error.to_string()))?;
        Ok(hash.to_string())
    }

    /// Check `password` against a stored hash. See the [module docs](self)
    /// for the accepted formats.
    pub fn verify(&self, password: &str, stored: &str) -> Verification {
        if let Some(rest) = stored.strip_prefix(ARGON2_PREFIX) {
            return self.verify_argon2(password, rest, true);
        }
        if let Some(rest) = stored.strip_prefix(BCRYPT_PREFIX) {
            return verify_bcrypt(password, rest);
        }
        if stored.starts_with("$argon2") {
            return self.verify_argon2(password, stored, false);
        }
        if is_bcrypt(stored) {
            return verify_bcrypt(password, stored);
        }
        rejected("unrecognised format")
    }

    /// Spend the same work as [`verify`](Self::verify) of a current argon2id
    /// hash, then discard the result.
    ///
    /// Call it when there is no stored hash to check (unknown user), so that
    /// "no such account" and "wrong password" take the same time. A stored
    /// hash with a legacy scheme or different cost still takes a different
    /// time; rehashing on login closes that gap over time.
    pub fn dummy_verify(&self, password: &str) {
        let outcome = self
            .argon2
            .verify_password(black_box(password.as_bytes()), &self.dummy);
        black_box(outcome.is_ok());
    }

    fn verify_argon2(&self, password: &str, stored: &str, prefixed: bool) -> Verification {
        let Ok(hash) = PasswordHash::new(stored) else {
            return rejected("malformed argon2 hash");
        };
        let Ok(algorithm) = Algorithm::new(hash.algorithm.as_str()) else {
            return rejected("unknown argon2 variant");
        };
        let Ok(params) = Params::try_from(&hash) else {
            return rejected("invalid argon2 parameters");
        };
        if params.m_cost() > MAX_VERIFY_MEMORY_KIB {
            return rejected("argon2 memory cost above the verification limit");
        }
        match self.argon2.verify_password(password.as_bytes(), &hash) {
            Ok(()) if prefixed || self.is_weaker(algorithm, hash.version, &params) => {
                Verification::ValidNeedsRehash
            }
            Ok(()) => Verification::Valid,
            Err(HashError::PasswordInvalid) => Verification::Invalid,
            Err(_) => rejected("argon2 verification failed"),
        }
    }

    fn is_weaker(&self, algorithm: Algorithm, version: Option<u32>, params: &Params) -> bool {
        !matches!(algorithm, Algorithm::Argon2id)
            || version != Some(u32::from(Version::V0x13))
            || params.m_cost() < self.params.memory_kib
            || params.t_cost() < self.params.iterations
            || params.p_cost() < self.params.parallelism
            || params.output_len().is_none_or(|len| len < OUTPUT_LEN)
    }
}

impl Default for PasswordHasher {
    /// A hasher with [`PasswordParams::OWASP`].
    fn default() -> Self {
        Self::new(PasswordParams::OWASP).expect("the OWASP argon2id parameters are valid")
    }
}

impl fmt::Debug for PasswordHasher {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PasswordHasher")
            .field("params", &self.params)
            .finish_non_exhaustive()
    }
}

fn is_bcrypt(stored: &str) -> bool {
    BCRYPT_VERSIONS
        .iter()
        .any(|version| stored.starts_with(version))
}

fn verify_bcrypt(password: &str, stored: &str) -> Verification {
    if !is_bcrypt(stored) {
        return rejected("unsupported bcrypt version");
    }
    match bcrypt::verify(password, stored) {
        Ok(true) => Verification::ValidNeedsRehash,
        Ok(false) => Verification::Invalid,
        // The bcrypt error can quote parts of the hash; it is not logged.
        Err(_) => rejected("malformed bcrypt hash"),
    }
}

fn rejected(reason: &'static str) -> Verification {
    tracing::warn!(reason, "stored password hash rejected");
    Verification::Invalid
}

#[cfg(test)]
mod tests {
    use super::*;

    const LOW: PasswordParams = PasswordParams {
        memory_kib: 8,
        iterations: 1,
        parallelism: 1,
    };
    const HIGHER: PasswordParams = PasswordParams {
        memory_kib: 16,
        iterations: 1,
        parallelism: 1,
    };

    fn low() -> PasswordHasher {
        PasswordHasher::new(LOW).unwrap()
    }

    fn bcrypt_hash(password: &str, version: bcrypt::Version) -> String {
        bcrypt::hash_with_result(password, 4)
            .unwrap()
            .format_for_version(version)
    }

    #[test]
    fn argon2_round_trip() {
        let hasher = low();
        let hash = hasher.hash("correct horse").unwrap();
        assert!(hash.starts_with("$argon2id$v=19$m=8,t=1,p=1$"), "{hash}");
        assert_eq!(hasher.verify("correct horse", &hash), Verification::Valid);
        assert_eq!(hasher.verify("wrong horse", &hash), Verification::Invalid);
    }

    #[test]
    fn salts_differ() {
        let hasher = low();
        assert_ne!(hasher.hash("same").unwrap(), hasher.hash("same").unwrap());
    }

    #[test]
    fn weaker_parameters_need_rehash() {
        let hash = low().hash("pw").unwrap();
        let stronger = PasswordHasher::new(HIGHER).unwrap();
        assert_eq!(stronger.verify("pw", &hash), Verification::ValidNeedsRehash);
        assert_eq!(stronger.verify("nope", &hash), Verification::Invalid);

        let strong_hash = stronger.hash("pw").unwrap();
        assert_eq!(low().verify("pw", &strong_hash), Verification::Valid);

        let more_iterations = PasswordHasher::new(PasswordParams {
            iterations: 2,
            ..LOW
        })
        .unwrap();
        assert_eq!(
            more_iterations.verify("pw", &hash),
            Verification::ValidNeedsRehash
        );
        let more_lanes = PasswordHasher::new(PasswordParams {
            memory_kib: 16,
            parallelism: 2,
            ..LOW
        })
        .unwrap();
        assert_eq!(
            more_lanes.verify("pw", &strong_hash),
            Verification::ValidNeedsRehash
        );
    }

    #[test]
    fn short_output_needs_rehash() {
        let params = Params::new(8, 1, 1, Some(16)).unwrap();
        let argon2 = Argon2::new(Algorithm::Argon2id, Version::V0x13, params);
        let hash = argon2
            .hash_password_with_salt(b"pw", b"saltsaltsaltsalt")
            .unwrap()
            .to_string();
        assert_eq!(low().verify("pw", &hash), Verification::ValidNeedsRehash);
    }

    #[test]
    fn other_argon2_variants_need_rehash() {
        let params = Params::new(8, 1, 1, Some(OUTPUT_LEN)).unwrap();
        for (algorithm, version) in [
            (Algorithm::Argon2i, Version::V0x13),
            (Algorithm::Argon2d, Version::V0x13),
            (Algorithm::Argon2id, Version::V0x10),
        ] {
            let argon2 = Argon2::new(algorithm, version, params.clone());
            let hash = argon2
                .hash_password_with_salt(b"pw", b"saltsaltsaltsalt")
                .unwrap()
                .to_string();
            assert_eq!(
                low().verify("pw", &hash),
                Verification::ValidNeedsRehash,
                "{hash}"
            );
            assert_eq!(low().verify("other", &hash), Verification::Invalid);
        }
    }

    #[test]
    fn prefixed_argon2_needs_rehash() {
        let hasher = low();
        let stored = format!("{{argon2}}{}", hasher.hash("pw").unwrap());
        assert_eq!(hasher.verify("pw", &stored), Verification::ValidNeedsRehash);
        assert_eq!(hasher.verify("other", &stored), Verification::Invalid);
    }

    #[test]
    fn bcrypt_versions_verify_and_need_rehash() {
        let hasher = low();
        for version in [
            bcrypt::Version::TwoA,
            bcrypt::Version::TwoB,
            bcrypt::Version::TwoY,
        ] {
            let stored = bcrypt_hash("legacy", version);
            assert_eq!(
                hasher.verify("legacy", &stored),
                Verification::ValidNeedsRehash
            );
            assert_eq!(hasher.verify("wrong", &stored), Verification::Invalid);
        }
    }

    #[test]
    fn delegating_bcrypt_prefix() {
        let hasher = low();
        let stored = format!("{{bcrypt}}{}", bcrypt_hash("legacy", bcrypt::Version::TwoA));
        assert_eq!(
            hasher.verify("legacy", &stored),
            Verification::ValidNeedsRehash
        );
        assert_eq!(hasher.verify("wrong", &stored), Verification::Invalid);
    }

    #[test]
    fn bcrypt_2x_is_refused() {
        let stored = bcrypt_hash("legacy", bcrypt::Version::TwoX);
        assert_eq!(low().verify("legacy", &stored), Verification::Invalid);
        let prefixed = format!("{{bcrypt}}{stored}");
        assert_eq!(low().verify("legacy", &prefixed), Verification::Invalid);
    }

    #[test]
    fn malformed_values_are_invalid() {
        let hasher = low();
        for stored in [
            "",
            "plaintext",
            "{noop}plaintext",
            "{argon2}",
            "{argon2}garbage",
            "{bcrypt}",
            "{bcrypt}$2a$04$short",
            "$argon2id$",
            "$argon2id$v=19$m=8,t=1,p=1$!!!$!!!",
            "$argon2x$v=19$m=8,t=1,p=1$AAAAAAAAAAAAAAAAAAAAAA$AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
            "$argon2id$v=19$m=1,t=1,p=1$AAAAAAAAAAAAAAAAAAAAAA$AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
            "$argon2id$v=19$m=4294967295,t=1,p=1$AAAAAAAAAAAAAAAAAAAAAA$AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
            "$argon2id$v=7$m=8,t=1,p=1$AAAAAAAAAAAAAAAAAAAAAA$AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
            "$2b$04$tooshort",
            "$2b$99$abcdefghijklmnopqrstuuabcdefghijklmnopqrstuvwxyz01234",
        ] {
            assert_eq!(
                hasher.verify("pw", stored),
                Verification::Invalid,
                "{stored}"
            );
        }
    }

    #[test]
    fn dummy_hash_never_matches() {
        let hasher = low();
        hasher.dummy_verify("anything");
        let dummy = hasher.dummy.to_string();
        assert_eq!(hasher.verify("anything", &dummy), Verification::Invalid);
        assert_eq!(hasher.verify("", &dummy), Verification::Invalid);
    }

    #[test]
    fn invalid_parameters_are_rejected() {
        let error = PasswordHasher::new(PasswordParams {
            memory_kib: 1,
            ..LOW
        })
        .unwrap_err();
        assert_eq!(error.code(), sekvent_error::ErrorCode::InvalidArgument);
        assert!(
            PasswordHasher::new(PasswordParams {
                iterations: 0,
                ..LOW
            })
            .is_err()
        );
    }

    #[test]
    fn defaults_are_owasp() {
        assert_eq!(PasswordParams::default(), PasswordParams::OWASP);
        let hasher = PasswordHasher::default();
        assert_eq!(hasher.params(), PasswordParams::OWASP);
        assert_eq!(
            format!("{hasher:?}"),
            "PasswordHasher { params: PasswordParams { memory_kib: 19456, iterations: 2, parallelism: 1 }, .. }"
        );
    }

    #[test]
    fn verification_helpers() {
        assert!(Verification::Valid.is_valid());
        assert!(!Verification::Valid.needs_rehash());
        assert!(Verification::ValidNeedsRehash.is_valid());
        assert!(Verification::ValidNeedsRehash.needs_rehash());
        assert!(!Verification::Invalid.is_valid());
        assert!(!Verification::Invalid.needs_rehash());
    }
}
