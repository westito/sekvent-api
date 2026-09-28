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
//! Anything else — unknown prefixes, malformed strings, `$2x$`, an empty
//! value — is [`Verification::Invalid`] and logged at `warn` without the
//! value. So is a hash whose cost exceeds the verification limits
//! ([`PasswordParams::VERIFY_LIMIT`] for argon2, [`MAX_BCRYPT_COST`] for
//! bcrypt): a stored value must not be able to make one login consume
//! unbounded memory or time. Nothing in this module panics on a bad stored
//! value.
//!
//! # Uniform work
//!
//! Every [`PasswordHasher::verify`] call performs one expensive
//! verification. When the stored value cannot be checked (unusable, over
//! the limits, an account with no password such as a single-sign-on-only
//! user) it runs [`PasswordHasher::dummy_verify`] instead, so such an
//! account takes as long to reject as a wrong password.

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
/// Highest bcrypt cost [`PasswordHasher::verify`] accepts: 2^14 rounds,
/// around a second on current hardware and four times the common default
/// of 12. Stored hashes above it are refused.
pub const MAX_BCRYPT_COST: u32 = 14;

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
    /// The largest argon2 cost a stored hash may ask for, and a hasher may
    /// be configured with: 1 GiB of memory, 16 iterations, 16 lanes. Stored
    /// hashes above any of these are refused rather than allowed to exhaust
    /// the process.
    pub const VERIFY_LIMIT: Self = Self {
        memory_kib: 1024 * 1024,
        iterations: 16,
        parallelism: 16,
    };

    /// Whether every parameter is within [`VERIFY_LIMIT`](Self::VERIFY_LIMIT).
    pub fn within_verify_limit(self) -> bool {
        self.memory_kib <= Self::VERIFY_LIMIT.memory_kib
            && self.iterations <= Self::VERIFY_LIMIT.iterations
            && self.parallelism <= Self::VERIFY_LIMIT.parallelism
    }

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
    /// 8 KiB of memory per lane, or zero iterations), or when they exceed
    /// [`PasswordParams::VERIFY_LIMIT`] (the hasher could not verify its own
    /// hashes).
    pub fn new(params: PasswordParams) -> Result<Self, AppError> {
        if !params.within_verify_limit() {
            return Err(AppError::invalid_argument(
                "argon2id parameters exceed the verification limit",
            ));
        }
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
    ///
    /// Exactly one expensive verification runs per call: a stored value that
    /// cannot be checked costs a [`dummy_verify`](Self::dummy_verify) before
    /// it is reported [`Verification::Invalid`].
    pub fn verify(&self, password: &str, stored: &str) -> Verification {
        let checked = if let Some(rest) = stored.strip_prefix(ARGON2_PREFIX) {
            self.verify_argon2(password, rest, true)
        } else if let Some(rest) = stored.strip_prefix(BCRYPT_PREFIX) {
            verify_bcrypt(password, rest)
        } else if stored.starts_with("$argon2") {
            self.verify_argon2(password, stored, false)
        } else if is_bcrypt(stored) {
            verify_bcrypt(password, stored)
        } else {
            Err("unrecognised format")
        };
        checked.unwrap_or_else(|reason| {
            self.dummy_verify(password);
            tracing::warn!(reason, "stored password hash rejected");
            Verification::Invalid
        })
    }

    /// Spend the same work as [`verify`](Self::verify) of a current argon2id
    /// hash, then discard the result.
    ///
    /// Call it when there is no stored hash to check (unknown user), so that
    /// "no such account" and "wrong password" take the same time. A stored
    /// hash with a legacy scheme or different cost still takes a different
    /// time; rehashing on login closes that gap over time.
    pub fn dummy_verify(&self, password: &str) {
        #[cfg(test)]
        DUMMY_RUNS.with(|runs| runs.set(runs.get() + 1));
        let outcome = self
            .argon2
            .verify_password(black_box(password.as_bytes()), &self.dummy);
        black_box(outcome.is_ok());
    }

    /// `Err` means no real verification ran; the caller spends the dummy
    /// work instead.
    fn verify_argon2(
        &self,
        password: &str,
        stored: &str,
        prefixed: bool,
    ) -> Result<Verification, &'static str> {
        let hash = PasswordHash::new(stored).map_err(|_| "malformed argon2 hash")?;
        // Without both, argon2 reports a mismatch without hashing anything.
        if hash.salt.is_none() || hash.hash.is_none() {
            return Err("argon2 hash without salt or output");
        }
        let algorithm =
            Algorithm::new(hash.algorithm.as_str()).map_err(|_| "unknown argon2 variant")?;
        let params = Params::try_from(&hash).map_err(|_| "invalid argon2 parameters")?;
        let cost = PasswordParams {
            memory_kib: params.m_cost(),
            iterations: params.t_cost(),
            parallelism: params.p_cost(),
        };
        if !cost.within_verify_limit() {
            return Err("argon2 cost above the verification limit");
        }
        match self.argon2.verify_password(password.as_bytes(), &hash) {
            Ok(()) if prefixed || self.is_weaker(algorithm, hash.version, &params) => {
                Ok(Verification::ValidNeedsRehash)
            }
            Ok(()) => Ok(Verification::Valid),
            Err(HashError::PasswordInvalid) => Ok(Verification::Invalid),
            // Argon2 validates before it hashes, so other errors did no work.
            Err(_) => Err("argon2 verification failed"),
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

/// `Err` means no real verification ran; the caller spends the dummy work
/// instead.
fn verify_bcrypt(password: &str, stored: &str) -> Result<Verification, &'static str> {
    if !is_bcrypt(stored) {
        return Err("unsupported bcrypt version");
    }
    let cost = bcrypt_cost(stored).ok_or("malformed bcrypt hash")?;
    if cost > MAX_BCRYPT_COST {
        return Err("bcrypt cost above the verification limit");
    }
    match bcrypt::verify(password, stored) {
        Ok(true) => Ok(Verification::ValidNeedsRehash),
        Ok(false) => Ok(Verification::Invalid),
        // The bcrypt error can quote parts of the hash; it is not logged.
        // bcrypt parses and validates before it hashes, so no work was done.
        Err(_) => Err("malformed bcrypt hash"),
    }
}

/// The cost field of `$2b$12$…`.
fn bcrypt_cost(stored: &str) -> Option<u32> {
    let cost = stored.get(4..)?.split('$').next()?;
    if cost.is_empty() || !cost.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    cost.parse().ok()
}

#[cfg(test)]
thread_local! {
    /// How many times [`PasswordHasher::dummy_verify`] ran on this thread.
    static DUMMY_RUNS: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

/// Dummy verifications run so far on this thread.
#[cfg(test)]
pub(crate) fn dummy_runs() -> u32 {
    DUMMY_RUNS.with(std::cell::Cell::get)
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
            "$argon2id$v=19$m=8,t=1,p=1",
            "$argon2id$v=19$m=8,t=1,p=1$AAAAAAAAAAAAAAAAAAAAAA",
            "$2b$04$tooshort",
            "$2b$99$abcdefghijklmnopqrstuuabcdefghijklmnopqrstuvwxyz01234",
        ] {
            let before = dummy_runs();
            assert_eq!(
                hasher.verify("pw", stored),
                Verification::Invalid,
                "{stored}"
            );
            assert_eq!(dummy_runs(), before + 1, "no dummy work for {stored}");
        }
    }

    #[test]
    fn checkable_hashes_do_no_dummy_work() {
        let hasher = low();
        let argon = hasher.hash("pw").unwrap();
        let legacy = bcrypt_hash("pw", bcrypt::Version::TwoB);
        let before = dummy_runs();
        for (password, stored) in [
            ("pw", argon.as_str()),
            ("nope", argon.as_str()),
            ("pw", legacy.as_str()),
            ("nope", legacy.as_str()),
        ] {
            let _ = hasher.verify(password, stored);
        }
        assert_eq!(dummy_runs(), before);
    }

    #[test]
    fn an_account_without_a_password_pays_the_dummy_work() {
        let hasher = low();
        let before = dummy_runs();
        assert_eq!(hasher.verify("pw", ""), Verification::Invalid);
        assert_eq!(dummy_runs(), before + 1);
    }

    #[test]
    fn argon2_costs_above_the_limit_are_refused() {
        let hasher = low();
        let hash =
            |cost: &str| format!("$argon2id$v=19${cost}${DUMMY_SALT_B64}${DUMMY_OUTPUT_B64}");
        for cost in ["m=1048577,t=1,p=1", "m=8,t=17,p=1", "m=136,t=1,p=17"] {
            let stored = hash(cost);
            assert!(PasswordHash::new(&stored).is_ok(), "{stored}");
            let before = dummy_runs();
            assert_eq!(hasher.verify("pw", &stored), Verification::Invalid);
            assert_eq!(dummy_runs(), before + 1, "{cost}");
        }
        // At the limit is still checked for real (and does not match).
        let before = dummy_runs();
        assert_eq!(
            hasher.verify("pw", &hash("m=128,t=16,p=16")),
            Verification::Invalid
        );
        assert_eq!(dummy_runs(), before);
    }

    #[test]
    fn bcrypt_costs_above_the_limit_are_refused() {
        let hasher = low();
        let real = bcrypt_hash("pw", bcrypt::Version::TwoB);
        assert!(real.starts_with("$2b$04$"));
        let too_costly = real.replacen("$04$", &format!("${}$", MAX_BCRYPT_COST + 1), 1);
        let before = dummy_runs();
        assert_eq!(hasher.verify("pw", &too_costly), Verification::Invalid);
        assert_eq!(dummy_runs(), before + 1);
        let prefixed = format!("{{bcrypt}}{too_costly}");
        assert_eq!(hasher.verify("pw", &prefixed), Verification::Invalid);
        assert_eq!(dummy_runs(), before + 2);
    }

    #[test]
    fn bcrypt_cost_parsing() {
        assert_eq!(bcrypt_cost("$2b$12$rest"), Some(12));
        assert_eq!(bcrypt_cost("$2b$04"), Some(4));
        assert_eq!(bcrypt_cost("$2b$"), None);
        assert_eq!(bcrypt_cost("$2b$+1$rest"), None);
        assert_eq!(bcrypt_cost("$2b$ab$rest"), None);
        assert_eq!(bcrypt_cost("$2b"), None);
        assert_eq!(bcrypt_cost("$2b$99999999999$rest"), None);
    }

    #[test]
    fn verify_limits() {
        assert!(PasswordParams::OWASP.within_verify_limit());
        assert!(PasswordParams::VERIFY_LIMIT.within_verify_limit());
        for over in [
            PasswordParams {
                memory_kib: PasswordParams::VERIFY_LIMIT.memory_kib + 1,
                ..LOW
            },
            PasswordParams {
                iterations: PasswordParams::VERIFY_LIMIT.iterations + 1,
                ..LOW
            },
            PasswordParams {
                memory_kib: 8 * 17,
                parallelism: PasswordParams::VERIFY_LIMIT.parallelism + 1,
                ..LOW
            },
        ] {
            assert!(!over.within_verify_limit(), "{over:?}");
            let error = PasswordHasher::new(over).unwrap_err();
            assert_eq!(error.code(), sekvent_error::ErrorCode::InvalidArgument);
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
