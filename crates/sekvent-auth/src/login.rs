//! A login check that does not reveal whether an account exists.
//!
//! [`authenticate`] always performs one password verification: against the
//! stored hash when the account exists, against
//! [`PasswordHasher::dummy_verify`] when it does not. An unknown account
//! and a wrong password produce the same [`LoginRejected::InvalidCredentials`]
//! after the same amount of work. An account's disabled state is only
//! reported once the password has been proven, so it cannot be probed
//! either.

use sekvent_error::{AppError, ErrorCode};

use crate::password::PasswordHasher;

/// Result of [`authenticate`].
pub type LoginOutcome<U> = Result<Authenticated<U>, LoginRejected>;

/// A successful login.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Authenticated<U> {
    /// The account that logged in.
    pub user: U,
    /// The stored hash is legacy or below the configured cost: replace it
    /// with [`PasswordHasher::hash`] of the password just checked.
    pub needs_rehash: bool,
}

/// Why a login was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum LoginRejected {
    /// Unknown account or wrong password; deliberately indistinguishable.
    #[error("invalid credentials")]
    InvalidCredentials,
    /// The password is right but the account is disabled.
    #[error("account disabled")]
    Disabled,
    /// The account is locked, e.g. after too many failed attempts.
    /// [`authenticate`] never returns it; lockout policy belongs to the caller.
    #[error("account locked")]
    Locked,
}

impl LoginRejected {
    /// `UNAUTHENTICATED` for bad credentials, `PERMISSION_DENIED` otherwise.
    pub fn error_code(self) -> ErrorCode {
        match self {
            Self::InvalidCredentials => ErrorCode::Unauthenticated,
            Self::Disabled | Self::Locked => ErrorCode::PermissionDenied,
        }
    }

    /// A message safe to show the caller.
    pub fn user_message(self) -> &'static str {
        match self {
            Self::InvalidCredentials => "invalid username or password",
            Self::Disabled => "this account is disabled",
            Self::Locked => "this account is locked",
        }
    }

    /// Machine-readable reason for [`AppError::with_reason`].
    pub fn reason(self) -> &'static str {
        match self {
            Self::InvalidCredentials => "INVALID_CREDENTIALS",
            Self::Disabled => "ACCOUNT_DISABLED",
            Self::Locked => "ACCOUNT_LOCKED",
        }
    }
}

impl From<LoginRejected> for AppError {
    fn from(rejected: LoginRejected) -> Self {
        AppError::new(rejected.error_code(), rejected.user_message()).with_reason(rejected.reason())
    }
}

/// Check `password` for the account found by the caller's lookup.
///
/// `lookup` is `None` for an unknown account, otherwise the account, its
/// stored password hash and whether it is enabled. Exactly one password
/// verification runs either way.
pub fn authenticate<U>(
    hasher: &PasswordHasher,
    lookup: Option<(U, &str, bool)>,
    password: &str,
) -> LoginOutcome<U> {
    let Some((user, stored_hash, enabled)) = lookup else {
        hasher.dummy_verify(password);
        return Err(LoginRejected::InvalidCredentials);
    };
    let verification = hasher.verify(password, stored_hash);
    if !verification.is_valid() {
        return Err(LoginRejected::InvalidCredentials);
    }
    if !enabled {
        return Err(LoginRejected::Disabled);
    }
    Ok(Authenticated {
        user,
        needs_rehash: verification.needs_rehash(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::password::PasswordParams;

    fn hasher() -> PasswordHasher {
        PasswordHasher::new(PasswordParams {
            memory_kib: 8,
            iterations: 1,
            parallelism: 1,
        })
        .unwrap()
    }

    #[test]
    fn success() {
        let hasher = hasher();
        let stored = hasher.hash("pw").unwrap();
        let outcome = authenticate(&hasher, Some(("ada", stored.as_str(), true)), "pw");
        assert_eq!(
            outcome,
            Ok(Authenticated {
                user: "ada",
                needs_rehash: false
            })
        );
    }

    #[test]
    fn legacy_hash_asks_for_rehash() {
        let hasher = hasher();
        let stored = bcrypt::hash("pw", 4).unwrap();
        let outcome = authenticate(&hasher, Some((7_u32, stored.as_str(), true)), "pw").unwrap();
        assert_eq!(outcome.user, 7);
        assert!(outcome.needs_rehash);
    }

    #[test]
    fn unknown_user_and_wrong_password_look_the_same() {
        let hasher = hasher();
        let stored = hasher.hash("pw").unwrap();
        let unknown = authenticate::<&str>(&hasher, None, "pw");
        let wrong = authenticate(&hasher, Some(("ada", stored.as_str(), true)), "nope");
        assert_eq!(unknown, Err(LoginRejected::InvalidCredentials));
        assert_eq!(wrong, Err(LoginRejected::InvalidCredentials));
    }

    #[test]
    fn disabled_only_after_the_password_is_proven() {
        let hasher = hasher();
        let stored = hasher.hash("pw").unwrap();
        assert_eq!(
            authenticate(&hasher, Some(("ada", stored.as_str(), false)), "pw"),
            Err(LoginRejected::Disabled)
        );
        assert_eq!(
            authenticate(&hasher, Some(("ada", stored.as_str(), false)), "nope"),
            Err(LoginRejected::InvalidCredentials)
        );
    }

    #[test]
    fn malformed_stored_hash_is_invalid_credentials() {
        assert_eq!(
            authenticate(&hasher(), Some(("ada", "garbage", true)), "pw"),
            Err(LoginRejected::InvalidCredentials)
        );
    }

    #[test]
    fn rejection_mapping() {
        for (rejected, code, reason) in [
            (
                LoginRejected::InvalidCredentials,
                ErrorCode::Unauthenticated,
                "INVALID_CREDENTIALS",
            ),
            (
                LoginRejected::Disabled,
                ErrorCode::PermissionDenied,
                "ACCOUNT_DISABLED",
            ),
            (
                LoginRejected::Locked,
                ErrorCode::PermissionDenied,
                "ACCOUNT_LOCKED",
            ),
        ] {
            assert_eq!(rejected.error_code(), code);
            assert_eq!(rejected.reason(), reason);
            assert!(!rejected.to_string().is_empty());
            let error = AppError::from(rejected);
            assert_eq!(error.code(), code);
            assert_eq!(error.message(), rejected.user_message());
            assert_eq!(error.reason(), Some(reason));
        }
    }
}
