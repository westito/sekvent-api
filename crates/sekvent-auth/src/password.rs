//! Password hashing with argon2id or bcrypt, and verification of argon2 and
//! bcrypt hashes.
//!
//! The [`PasswordScheme`] of a [`PasswordHasher`] decides how new hashes are
//! written: argon2id PHC strings (the default) or bcrypt, for a store shared
//! with applications that only read bcrypt. Stored values in any of these
//! shapes verify under either scheme:
//!
//! | Stored value (password matches) | argon2id scheme | bcrypt scheme |
//! |---|---|---|
//! | `$argon2id$v=19$…` at or above the configured cost | [`Valid`](Verification::Valid) | [`ValidNeedsRehash`](Verification::ValidNeedsRehash) |
//! | `$argon2id$…` below the configured cost, `$argon2i$…`, `$argon2d$…` | [`ValidNeedsRehash`](Verification::ValidNeedsRehash) | [`ValidNeedsRehash`](Verification::ValidNeedsRehash) |
//! | `{argon2}$argon2…` (delegating-encoder prefix) | [`ValidNeedsRehash`](Verification::ValidNeedsRehash) | [`ValidNeedsRehash`](Verification::ValidNeedsRehash) |
//! | `$2a$…`, `$2b$…`, `$2y$…` or `{bcrypt}$2…` in the configured form at or above the configured cost | [`ValidNeedsRehash`](Verification::ValidNeedsRehash) | [`Valid`](Verification::Valid) |
//! | bcrypt in the other form (prefix or not), or below the configured cost | [`ValidNeedsRehash`](Verification::ValidNeedsRehash) | [`ValidNeedsRehash`](Verification::ValidNeedsRehash) |
//! | bcrypt in any form, and a password longer than [`BCRYPT_MAX_PASSWORD_BYTES`] | [`ValidNeedsRehash`](Verification::ValidNeedsRehash) | [`Valid`](Verification::Valid) |
//!
//! The bcrypt version tag (`2a`, `2b`, `2y`) never asks for a rehash. A
//! password longer than 72 bytes is checked against bcrypt on its first
//! 72 bytes, the part every bcrypt implementation that wrote such a hash
//! read. Under argon2id the match asks for a rehash, so the whole password
//! ends up in an argon2id hash; under bcrypt it never does, because a new
//! bcrypt hash of that password is refused (see [`PasswordHasher::hash`]).
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
//! user) it runs
//! [`PasswordHasher::dummy_verify`] instead, which costs as much as checking
//! a current hash of the configured scheme, so such an account takes as long
//! to reject as a wrong password.

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
/// Highest bcrypt cost [`PasswordHasher::verify`] accepts and a bcrypt
/// hasher may be configured with: 2^14 rounds, around a second on current
/// hardware and four times the common default of 12. Stored hashes above it
/// are refused.
pub const MAX_BCRYPT_COST: u32 = 14;
/// Lowest bcrypt cost a hasher may be configured with.
const MIN_BCRYPT_COST: u32 = 4;
/// Longest password bcrypt reads, in bytes. New bcrypt hashes refuse longer
/// passwords; verification reads only this many bytes.
pub const BCRYPT_MAX_PASSWORD_BYTES: usize = 72;

const ARGON2_PREFIX: &str = "{argon2}";
const BCRYPT_PREFIX: &str = "{bcrypt}";
const BCRYPT_VERSIONS: [&str; 3] = ["$2a$", "$2b$", "$2y$"];

/// 16 zero bytes, unpadded base64.
const DUMMY_SALT_B64: &str = "AAAAAAAAAAAAAAAAAAAAAA";
/// 32 zero bytes, unpadded base64.
const DUMMY_OUTPUT_B64: &str = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
/// A zero salt and a zero digest in bcrypt's base64 alphabet (`.` is zero).
const BCRYPT_DUMMY_TAIL_LEN: usize = 53;

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

/// How [`PasswordHasher::hash`] writes new hashes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum PasswordScheme {
    /// argon2id PHC strings (`$argon2id$v=19$…`), the default.
    Argon2id(PasswordParams),
    /// bcrypt (`$2b$10$…`, or `{bcrypt}$2b$10$…` when prefixed).
    Bcrypt(BcryptParams),
}

impl Default for PasswordScheme {
    /// argon2id with [`PasswordParams::OWASP`].
    fn default() -> Self {
        Self::Argon2id(PasswordParams::OWASP)
    }
}

/// bcrypt settings for new hashes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BcryptParams {
    /// log2 of the rounds, `4..=`[`MAX_BCRYPT_COST`].
    pub cost: u32,
    /// Version tag written into new hashes.
    pub version: BcryptVersion,
    /// Write the `{bcrypt}` delegating-encoder prefix.
    pub prefixed: bool,
}

impl BcryptParams {
    /// `cost`, `$2b$`, no prefix.
    pub const fn new(cost: u32) -> Self {
        Self {
            cost,
            version: BcryptVersion::TwoB,
            prefixed: false,
        }
    }

    /// With the `{bcrypt}` prefix.
    #[must_use]
    pub const fn prefixed(self) -> Self {
        Self {
            prefixed: true,
            ..self
        }
    }

    /// With another version tag.
    #[must_use]
    pub const fn version(self, version: BcryptVersion) -> Self {
        Self { version, ..self }
    }
}

/// The version tag of a bcrypt hash. All three name the same algorithm;
/// readers differ in which they accept.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BcryptVersion {
    /// `$2a$`.
    TwoA,
    /// `$2b$`, the current tag.
    #[default]
    TwoB,
    /// `$2y$`.
    TwoY,
}

impl BcryptVersion {
    fn tag(self) -> bcrypt::Version {
        match self {
            Self::TwoA => bcrypt::Version::TwoA,
            Self::TwoB => bcrypt::Version::TwoB,
            Self::TwoY => bcrypt::Version::TwoY,
        }
    }
}

/// Outcome of checking a password against a stored hash.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[must_use]
pub enum Verification {
    /// The password matches and the stored hash is current.
    Valid,
    /// The password matches, but the stored hash uses another scheme, a
    /// legacy format or weaker parameters: store [`PasswordHasher::hash`]
    /// of the password.
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

/// Hashes passwords with the configured [`PasswordScheme`] and verifies
/// stored hashes.
///
/// Build it once and share it; it holds no secrets.
#[derive(Clone)]
pub struct PasswordHasher {
    scheme: PasswordScheme,
    argon2: Argon2<'static>,
    dummy: Dummy,
}

/// A well-formed hash at the configured cost that no password matches:
/// checking against it costs exactly as much as a real verification.
#[derive(Clone)]
enum Dummy {
    Argon2(Box<PasswordHash>),
    Bcrypt(String),
}

/// Why the stored value is unusable, so no real verification ran; logged
/// at `warn`.
type Unchecked = &'static str;

impl PasswordHasher {
    /// A hasher producing argon2id hashes with `params`.
    ///
    /// Fails when argon2 rejects the parameters (for example less than
    /// 8 KiB of memory per lane, or zero iterations), or when they exceed
    /// [`PasswordParams::VERIFY_LIMIT`] (the hasher could not verify its own
    /// hashes).
    pub fn new(params: PasswordParams) -> Result<Self, AppError> {
        Self::with_scheme(PasswordScheme::Argon2id(params))
    }

    /// A hasher producing bcrypt hashes with `params`.
    ///
    /// Fails with `INVALID_ARGUMENT` for a cost outside
    /// `4..=`[`MAX_BCRYPT_COST`].
    pub fn bcrypt(params: BcryptParams) -> Result<Self, AppError> {
        Self::with_scheme(PasswordScheme::Bcrypt(params))
    }

    /// A hasher writing new hashes with `scheme`. See [`new`](Self::new) and
    /// [`bcrypt`](Self::bcrypt) for the checks.
    pub fn with_scheme(scheme: PasswordScheme) -> Result<Self, AppError> {
        match scheme {
            PasswordScheme::Argon2id(params) => Self::argon2id(params),
            PasswordScheme::Bcrypt(params) => {
                if !(MIN_BCRYPT_COST..=MAX_BCRYPT_COST).contains(&params.cost) {
                    return Err(AppError::invalid_argument(format!(
                        "bcrypt cost must be between {MIN_BCRYPT_COST} and {MAX_BCRYPT_COST}"
                    )));
                }
                let dummy = format!(
                    "$2b${:02}${}",
                    params.cost,
                    ".".repeat(BCRYPT_DUMMY_TAIL_LEN)
                );
                Ok(Self {
                    scheme,
                    argon2: Argon2::default(),
                    dummy: Dummy::Bcrypt(dummy),
                })
            }
        }
    }

    fn argon2id(params: PasswordParams) -> Result<Self, AppError> {
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
        let dummy = PasswordHash::new(&format!(
            "$argon2id$v=19$m={},t={},p={}${DUMMY_SALT_B64}${DUMMY_OUTPUT_B64}",
            params.memory_kib, params.iterations, params.parallelism
        ))
        .map_err(|error| AppError::internal(error.to_string()))?;
        Ok(Self {
            scheme: PasswordScheme::Argon2id(params),
            argon2: Argon2::new(Algorithm::Argon2id, Version::V0x13, argon_params),
            dummy: Dummy::Argon2(Box::new(dummy)),
        })
    }

    /// The configured scheme.
    pub fn scheme(&self) -> PasswordScheme {
        self.scheme
    }

    /// Hash `password` with the configured scheme and a fresh random salt.
    ///
    /// Under bcrypt, a password longer than [`BCRYPT_MAX_PASSWORD_BYTES`]
    /// fails with `INVALID_ARGUMENT` (reason `PASSWORD_TOO_LONG`) instead
    /// of being truncated. [`verify`](Self::verify) does truncate, to check
    /// hashes written by implementations that did.
    pub fn hash(&self, password: &str) -> Result<String, AppError> {
        if matches!(self.scheme, PasswordScheme::Bcrypt(_))
            && password.len() > BCRYPT_MAX_PASSWORD_BYTES
        {
            return Err(AppError::invalid_argument(format!(
                "passwords longer than {BCRYPT_MAX_PASSWORD_BYTES} bytes are not supported"
            ))
            .with_reason("PASSWORD_TOO_LONG"));
        }
        let mut salt = [0_u8; SALT_LEN];
        getrandom::fill(&mut salt).map_err(|error| AppError::internal(error.to_string()))?;
        match self.scheme {
            PasswordScheme::Argon2id(_) => {
                let hash = self
                    .argon2
                    .hash_password_with_salt(password.as_bytes(), &salt)
                    .map_err(|error| AppError::internal(error.to_string()))?;
                Ok(hash.to_string())
            }
            PasswordScheme::Bcrypt(params) => {
                let parts = bcrypt::hash_with_salt(password, params.cost, salt)
                    .map_err(|error| AppError::internal(error.to_string()))?;
                let hash = parts.format_for_version(params.version.tag());
                Ok(if params.prefixed {
                    format!("{BCRYPT_PREFIX}{hash}")
                } else {
                    hash
                })
            }
        }
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
            self.verify_bcrypt(password, rest, true)
        } else if stored.starts_with("$argon2") {
            self.verify_argon2(password, stored, false)
        } else if is_bcrypt(stored) {
            self.verify_bcrypt(password, stored, false)
        } else {
            Err("unrecognised format")
        };
        checked.unwrap_or_else(|reason| {
            self.dummy_verify(password);
            tracing::warn!(reason, "stored password hash rejected");
            Verification::Invalid
        })
    }

    /// Spend the same work as [`verify`](Self::verify) of a current hash of
    /// the configured scheme, then discard the result.
    ///
    /// Call it when there is no stored hash to check (unknown user), so that
    /// "no such account" and "wrong password" take the same time. A stored
    /// hash with another scheme or a different cost still takes a different
    /// time; rehashing on login closes that gap over time.
    pub fn dummy_verify(&self, password: &str) {
        #[cfg(test)]
        DUMMY_RUNS.with(|runs| runs.set(runs.get() + 1));
        let matched = match &self.dummy {
            Dummy::Argon2(hash) => self
                .argon2
                .verify_password(black_box(password.as_bytes()), &**hash)
                .is_ok(),
            Dummy::Bcrypt(hash) => {
                bcrypt::verify(black_box(bcrypt_input(password)), hash).unwrap_or(false)
            }
        };
        black_box(matched);
    }

    /// `Err` means no real verification ran; the caller spends the dummy
    /// work instead.
    fn verify_argon2(
        &self,
        password: &str,
        stored: &str,
        prefixed: bool,
    ) -> Result<Verification, Unchecked> {
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
            Ok(()) => Ok(match self.scheme {
                PasswordScheme::Argon2id(current)
                    if !prefixed && !is_weaker(current, algorithm, hash.version, &params) =>
                {
                    Verification::Valid
                }
                _ => Verification::ValidNeedsRehash,
            }),
            Err(HashError::PasswordInvalid) => Ok(Verification::Invalid),
            // Argon2 validates before it hashes, so other errors did no work.
            Err(_) => Err("argon2 verification failed"),
        }
    }

    /// `Err` means no real verification ran; the caller spends the dummy
    /// work instead.
    fn verify_bcrypt(
        &self,
        password: &str,
        stored: &str,
        prefixed: bool,
    ) -> Result<Verification, Unchecked> {
        if !is_bcrypt(stored) {
            return Err("unsupported bcrypt version");
        }
        let cost = bcrypt_cost(stored).ok_or("malformed bcrypt hash")?;
        if cost > MAX_BCRYPT_COST {
            return Err("bcrypt cost above the verification limit");
        }
        // A new bcrypt hash of a longer password would be refused, so the
        // bcrypt scheme never asks to rehash one.
        let truncated = password.len() > BCRYPT_MAX_PASSWORD_BYTES;
        match bcrypt::verify(bcrypt_input(password), stored) {
            Ok(true) => Ok(match self.scheme {
                PasswordScheme::Bcrypt(current)
                    if truncated || (current.prefixed == prefixed && cost >= current.cost) =>
                {
                    Verification::Valid
                }
                _ => Verification::ValidNeedsRehash,
            }),
            Ok(false) => Ok(Verification::Invalid),
            // The bcrypt error can quote parts of the hash; it is not logged.
            // bcrypt parses and validates before it hashes, so no work was done.
            Err(_) => Err("malformed bcrypt hash"),
        }
    }
}

#[cfg(feature = "tokio")]
impl PasswordHasher {
    /// [`hash`](Self::hash) on tokio's blocking pool.
    pub async fn hash_async(&self, password: sekvent_config::Secret) -> Result<String, AppError> {
        let hasher = self.clone();
        run_blocking(move || hasher.hash(password.expose())).await?
    }

    /// [`verify`](Self::verify) on tokio's blocking pool; `Invalid` if the
    /// task fails.
    pub async fn verify_async(
        &self,
        password: sekvent_config::Secret,
        stored: String,
    ) -> Verification {
        let hasher = self.clone();
        run_blocking(move || hasher.verify(password.expose(), &stored))
            .await
            .unwrap_or(Verification::Invalid)
    }
}

/// Run `work` on tokio's blocking pool.
#[cfg(feature = "tokio")]
pub(crate) async fn run_blocking<T, F>(work: F) -> Result<T, AppError>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|error| join_failed(&error))
}

/// A blocking task that panicked or was cancelled. The panic payload may
/// quote a password and is never logged.
#[cfg(feature = "tokio")]
fn join_failed(error: &tokio::task::JoinError) -> AppError {
    let cause = if error.is_panic() {
        "panicked"
    } else {
        "cancelled"
    };
    tracing::error!(cause, "password task failed");
    AppError::internal(format!("password task {cause}"))
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
            .field("scheme", &self.scheme)
            .finish_non_exhaustive()
    }
}

fn is_weaker(
    current: PasswordParams,
    algorithm: Algorithm,
    version: Option<u32>,
    params: &Params,
) -> bool {
    !matches!(algorithm, Algorithm::Argon2id)
        || version != Some(u32::from(Version::V0x13))
        || params.m_cost() < current.memory_kib
        || params.t_cost() < current.iterations
        || params.p_cost() < current.parallelism
        || params.output_len().is_none_or(|len| len < OUTPUT_LEN)
}

fn is_bcrypt(stored: &str) -> bool {
    BCRYPT_VERSIONS
        .iter()
        .any(|version| stored.starts_with(version))
}

/// The bytes of `password` bcrypt reads: at most the first
/// [`BCRYPT_MAX_PASSWORD_BYTES`], as the implementations that wrote stored
/// hashes truncated.
fn bcrypt_input(password: &str) -> &[u8] {
    let bytes = password.as_bytes();
    &bytes[..bytes.len().min(BCRYPT_MAX_PASSWORD_BYTES)]
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

    fn bcrypt_at(password: &str, cost: u32, version: bcrypt::Version) -> String {
        bcrypt::hash_with_result(password, cost)
            .unwrap()
            .format_for_version(version)
    }

    fn all_versions() -> [(BcryptVersion, &'static str); 3] {
        [
            (BcryptVersion::TwoA, "$2a$"),
            (BcryptVersion::TwoB, "$2b$"),
            (BcryptVersion::TwoY, "$2y$"),
        ]
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
        let bcrypt = PasswordHasher::bcrypt(BcryptParams::new(4)).unwrap();
        assert_ne!(bcrypt.hash("same").unwrap(), bcrypt.hash("same").unwrap());
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
        let Dummy::Argon2(dummy) = &hasher.dummy else {
            panic!("argon2id hasher with a bcrypt dummy");
        };
        let dummy = dummy.to_string();
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
        assert_eq!(
            PasswordScheme::default(),
            PasswordScheme::Argon2id(PasswordParams::OWASP)
        );
        let hasher = PasswordHasher::default();
        assert_eq!(
            hasher.scheme(),
            PasswordScheme::Argon2id(PasswordParams::OWASP)
        );
        assert_eq!(
            format!("{hasher:?}"),
            "PasswordHasher { scheme: Argon2id(PasswordParams { memory_kib: 19456, iterations: 2, parallelism: 1 }), .. }"
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

    #[test]
    fn bcrypt_params_builders() {
        let params = BcryptParams::new(10);
        assert_eq!(
            params,
            BcryptParams {
                cost: 10,
                version: BcryptVersion::TwoB,
                prefixed: false
            }
        );
        assert!(params.prefixed().prefixed);
        assert_eq!(
            params.version(BcryptVersion::TwoY).version,
            BcryptVersion::TwoY
        );
        assert_eq!(BcryptVersion::default(), BcryptVersion::TwoB);
    }

    #[test]
    fn bcrypt_scheme_and_debug() {
        let params = BcryptParams::new(10).prefixed();
        let hasher = PasswordHasher::bcrypt(params).unwrap();
        assert_eq!(hasher.scheme(), PasswordScheme::Bcrypt(params));
        assert_eq!(
            format!("{hasher:?}"),
            "PasswordHasher { scheme: Bcrypt(BcryptParams { cost: 10, version: TwoB, prefixed: true }), .. }"
        );
        let same = PasswordHasher::with_scheme(PasswordScheme::Bcrypt(params)).unwrap();
        assert_eq!(same.scheme(), hasher.scheme());
    }

    #[test]
    fn bcrypt_costs_outside_the_range_are_rejected() {
        for cost in [0, 3, 15, 31] {
            let error = PasswordHasher::bcrypt(BcryptParams::new(cost)).unwrap_err();
            assert_eq!(
                error.code(),
                sekvent_error::ErrorCode::InvalidArgument,
                "{cost}"
            );
        }
        for cost in [4, 14] {
            assert!(PasswordHasher::bcrypt(BcryptParams::new(cost)).is_ok());
        }
    }

    #[test]
    fn bcrypt_round_trips_in_both_forms_and_every_version() {
        for prefixed in [false, true] {
            for (version, tag) in all_versions() {
                let mut params = BcryptParams::new(4).version(version);
                if prefixed {
                    params = params.prefixed();
                }
                let hasher = PasswordHasher::bcrypt(params).unwrap();
                let hash = hasher.hash("correct horse").unwrap();
                let expected = format!("{}{tag}04$", if prefixed { "{bcrypt}" } else { "" });
                assert!(hash.starts_with(&expected), "{hash}");
                assert_eq!(hash.len(), 60 + if prefixed { 8 } else { 0 });
                assert_eq!(hasher.verify("correct horse", &hash), Verification::Valid);
                assert_eq!(hasher.verify("wrong horse", &hash), Verification::Invalid);
            }
        }
    }

    #[test]
    fn the_documented_shared_store_form() {
        let hasher = PasswordHasher::bcrypt(BcryptParams::new(10).prefixed()).unwrap();
        let hash = hasher.hash("pw").unwrap();
        assert!(hash.starts_with("{bcrypt}$2b$10$"), "{hash}");
        assert_eq!(hasher.verify("pw", &hash), Verification::Valid);
    }

    #[test]
    fn the_version_tag_never_asks_for_a_rehash() {
        let hasher = PasswordHasher::bcrypt(BcryptParams::new(4)).unwrap();
        for version in [
            bcrypt::Version::TwoA,
            bcrypt::Version::TwoB,
            bcrypt::Version::TwoY,
        ] {
            let stored = bcrypt_hash("pw", version);
            assert_eq!(hasher.verify("pw", &stored), Verification::Valid);
        }
    }

    #[test]
    fn bcrypt_scheme_rows() {
        let plain = PasswordHasher::bcrypt(BcryptParams::new(5)).unwrap();
        let prefixed = PasswordHasher::bcrypt(BcryptParams::new(5).prefixed()).unwrap();
        let at = bcrypt_at("pw", 5, bcrypt::Version::TwoB);
        let above = bcrypt_at("pw", 6, bcrypt::Version::TwoA);
        let below = bcrypt_at("pw", 4, bcrypt::Version::TwoY);
        let with_prefix = |hash: &str| format!("{BCRYPT_PREFIX}{hash}");

        // The configured form at or above the configured cost.
        assert_eq!(plain.verify("pw", &at), Verification::Valid);
        assert_eq!(plain.verify("pw", &above), Verification::Valid);
        assert_eq!(
            prefixed.verify("pw", &with_prefix(&at)),
            Verification::Valid
        );
        assert_eq!(
            prefixed.verify("pw", &with_prefix(&above)),
            Verification::Valid
        );
        // The other form.
        assert_eq!(
            plain.verify("pw", &with_prefix(&at)),
            Verification::ValidNeedsRehash
        );
        assert_eq!(prefixed.verify("pw", &at), Verification::ValidNeedsRehash);
        // Below the configured cost.
        assert_eq!(plain.verify("pw", &below), Verification::ValidNeedsRehash);
        assert_eq!(
            prefixed.verify("pw", &with_prefix(&below)),
            Verification::ValidNeedsRehash
        );
        // Wrong passwords.
        assert_eq!(plain.verify("nope", &at), Verification::Invalid);
        assert_eq!(
            prefixed.verify("nope", &with_prefix(&at)),
            Verification::Invalid
        );
    }

    #[test]
    fn argon2_hashes_rehash_under_bcrypt() {
        let hasher = PasswordHasher::bcrypt(BcryptParams::new(4)).unwrap();
        let argon = low().hash("pw").unwrap();
        let strong = PasswordHasher::new(HIGHER).unwrap().hash("pw").unwrap();
        for stored in [argon.clone(), strong, format!("{ARGON2_PREFIX}{argon}")] {
            assert_eq!(
                hasher.verify("pw", &stored),
                Verification::ValidNeedsRehash,
                "{stored}"
            );
            assert_eq!(hasher.verify("nope", &stored), Verification::Invalid);
        }
        let params = Params::new(8, 1, 1, Some(OUTPUT_LEN)).unwrap();
        let argon2i = Argon2::new(Algorithm::Argon2i, Version::V0x13, params)
            .hash_password_with_salt(b"pw", b"saltsaltsaltsalt")
            .unwrap()
            .to_string();
        assert_eq!(
            hasher.verify("pw", &argon2i),
            Verification::ValidNeedsRehash
        );
    }

    #[test]
    fn bcrypt_hashes_rehash_under_argon2id() {
        let hasher = low();
        let at = bcrypt_at("pw", 5, bcrypt::Version::TwoB);
        assert_eq!(hasher.verify("pw", &at), Verification::ValidNeedsRehash);
        assert_eq!(
            hasher.verify("pw", &format!("{BCRYPT_PREFIX}{at}")),
            Verification::ValidNeedsRehash
        );
    }

    #[test]
    fn passwords_longer_than_bcrypt_reads() {
        let longest = "x".repeat(BCRYPT_MAX_PASSWORD_BYTES);
        let too_long = "x".repeat(BCRYPT_MAX_PASSWORD_BYTES + 1);

        let bcrypt_hasher = PasswordHasher::bcrypt(BcryptParams::new(4)).unwrap();
        let error = bcrypt_hasher.hash(&too_long).unwrap_err();
        assert_eq!(error.code(), sekvent_error::ErrorCode::InvalidArgument);
        assert_eq!(error.reason(), Some("PASSWORD_TOO_LONG"));
        assert!(!error.message().contains(&too_long));

        let stored = bcrypt_hasher.hash(&longest).unwrap();
        assert_eq!(bcrypt_hasher.verify(&longest, &stored), Verification::Valid);

        // argon2 has no such limit.
        let argon = low().hash(&too_long).unwrap();
        assert_eq!(low().verify(&too_long, &argon), Verification::Valid);
    }

    #[test]
    fn long_passwords_verify_on_their_first_72_bytes() {
        let long = format!("{}tail", "x".repeat(BCRYPT_MAX_PASSWORD_BYTES));
        let other_tail = format!("{}other", "x".repeat(BCRYPT_MAX_PASSWORD_BYTES));
        let other_head = format!("y{}", "x".repeat(BCRYPT_MAX_PASSWORD_BYTES + 3));
        // Written by a writer that truncated, as legacy bcrypt writers did.
        let legacy = bcrypt_at(&long, 4, bcrypt::Version::TwoA);
        let prefixed_legacy = format!("{BCRYPT_PREFIX}{legacy}");

        let argon_hasher = low();
        let same_form = PasswordHasher::bcrypt(BcryptParams::new(4)).unwrap();
        let stronger_prefixed = PasswordHasher::bcrypt(BcryptParams::new(5).prefixed()).unwrap();
        let before = dummy_runs();
        for stored in [&legacy, &prefixed_legacy] {
            for password in [&long, &other_tail] {
                // Under argon2id the whole password moves to argon2id.
                assert_eq!(
                    argon_hasher.verify(password, stored),
                    Verification::ValidNeedsRehash,
                    "{stored}"
                );
                // Under bcrypt a rehash would be refused, so none is asked
                // for, whatever the form or cost of the stored hash.
                for hasher in [&same_form, &stronger_prefixed] {
                    assert_eq!(hasher.verify(password, stored), Verification::Valid);
                }
            }
            for hasher in [&argon_hasher, &same_form, &stronger_prefixed] {
                assert_eq!(hasher.verify(&other_head, stored), Verification::Invalid);
            }
        }
        assert_eq!(dummy_runs(), before, "a long password is checked for real");
        assert!(same_form.hash(&long).is_err());
        assert!(argon_hasher.hash(&long).is_ok());
    }

    #[test]
    fn the_bcrypt_input_is_capped_at_72_bytes() {
        assert_eq!(bcrypt_input("pw"), b"pw");
        let longest = "x".repeat(BCRYPT_MAX_PASSWORD_BYTES);
        assert_eq!(bcrypt_input(&longest).len(), BCRYPT_MAX_PASSWORD_BYTES);
        let long = format!("{longest}tail");
        assert_eq!(bcrypt_input(&long), longest.as_bytes());
    }
    #[test]
    fn bcrypt_dummy_is_a_bcrypt_string_that_never_matches() {
        for cost in [4, 10, 14] {
            let hasher = PasswordHasher::bcrypt(BcryptParams::new(cost)).unwrap();
            let Dummy::Bcrypt(dummy) = &hasher.dummy else {
                panic!("bcrypt hasher with an argon2 dummy");
            };
            assert_eq!(dummy.len(), 60);
            assert!(dummy.starts_with(&format!("$2b${cost:02}$")), "{dummy}");
            assert_eq!(bcrypt_cost(dummy), Some(cost));
            if cost == 4 {
                assert!(matches!(bcrypt::verify("anything", dummy), Ok(false)));
                assert!(matches!(bcrypt::verify("", dummy), Ok(false)));
                assert_eq!(hasher.verify("anything", dummy), Verification::Invalid);
            }
        }
    }

    #[test]
    fn bcrypt_dummy_work_is_counted() {
        let hasher = PasswordHasher::bcrypt(BcryptParams::new(4)).unwrap();
        let before = dummy_runs();
        hasher.dummy_verify("pw");
        hasher.dummy_verify(&"x".repeat(100));
        assert_eq!(hasher.verify("pw", "garbage"), Verification::Invalid);
        assert_eq!(dummy_runs(), before + 3);
    }

    #[cfg(feature = "tokio")]
    mod blocking {
        use sekvent_config::Secret;
        use sekvent_error::ErrorCode;

        use super::*;

        fn explode() -> u8 {
            panic!("boom")
        }

        #[tokio::test]
        async fn hash_and_verify_on_the_blocking_pool() {
            let hasher = PasswordHasher::bcrypt(BcryptParams::new(4)).unwrap();
            let hash = hasher.hash_async(Secret::new("pw")).await.unwrap();
            assert_eq!(
                hasher.verify_async(Secret::new("pw"), hash.clone()).await,
                Verification::Valid
            );
            assert_eq!(
                hasher.verify_async(Secret::new("nope"), hash).await,
                Verification::Invalid
            );
            let error = hasher
                .hash_async(Secret::new("x".repeat(80)))
                .await
                .unwrap_err();
            assert_eq!(error.reason(), Some("PASSWORD_TOO_LONG"));
        }

        #[tokio::test]
        async fn a_panicking_task_is_internal() {
            let error = run_blocking(explode).await.unwrap_err();
            assert_eq!(error.code(), ErrorCode::Internal);
            assert_eq!(error.message(), "internal error");
        }

        #[tokio::test]
        async fn join_failed_maps_panics_and_cancellation() {
            let panicked = tokio::spawn(async { explode() }).await.unwrap_err();
            assert!(panicked.is_panic());
            let error = join_failed(&panicked);
            assert_eq!(error.code(), ErrorCode::Internal);
            assert!(
                std::error::Error::source(&error)
                    .unwrap()
                    .to_string()
                    .contains("panicked")
            );

            let task = tokio::spawn(std::future::pending::<()>());
            task.abort();
            let cancelled = task.await.unwrap_err();
            assert!(cancelled.is_cancelled());
            let error = join_failed(&cancelled);
            assert_eq!(error.code(), ErrorCode::Internal);
            assert!(
                std::error::Error::source(&error)
                    .unwrap()
                    .to_string()
                    .contains("cancelled")
            );
        }
    }
}
