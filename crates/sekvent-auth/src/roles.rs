//! Role checks on verified claims.

use sekvent_error::AppError;

use crate::jwt::Claims;

/// The caller-safe message when a role check fails.
const DENIED_MESSAGE: &str = "insufficient permissions";

/// Claims that grant roles.
///
/// Implement it on the custom claims type; [`Claims<T>`] delegates to `T`.
pub trait HasRoles {
    /// Whether `role` is granted.
    fn has_role(&self, role: &str) -> bool;
}

impl<T: HasRoles> HasRoles for Claims<T> {
    fn has_role(&self, role: &str) -> bool {
        self.custom.has_role(role)
    }
}

impl HasRoles for [String] {
    fn has_role(&self, role: &str) -> bool {
        self.iter().any(|granted| granted == role)
    }
}

impl HasRoles for Vec<String> {
    fn has_role(&self, role: &str) -> bool {
        self.as_slice().has_role(role)
    }
}

/// Succeed when `claims` grant at least one of `roles`.
///
/// Fails closed: an empty `roles` list is always denied. The error is
/// `PERMISSION_DENIED` with a generic message that does not name the roles.
pub fn require_any_role<C: HasRoles + ?Sized>(claims: &C, roles: &[&str]) -> Result<(), AppError> {
    if roles.iter().any(|role| claims.has_role(role)) {
        Ok(())
    } else {
        Err(AppError::permission_denied(DENIED_MESSAGE))
    }
}

#[cfg(test)]
mod tests {
    use sekvent_error::ErrorCode;

    use super::*;

    #[derive(Debug, Clone)]
    struct Staff {
        roles: Vec<String>,
    }

    impl HasRoles for Staff {
        fn has_role(&self, role: &str) -> bool {
            self.roles.has_role(role)
        }
    }

    fn claims(roles: &[&str]) -> Claims<Staff> {
        Claims::new(
            "u",
            0,
            60,
            Staff {
                roles: roles.iter().map(|role| (*role).to_owned()).collect(),
            },
        )
    }

    #[test]
    fn any_granted_role_passes() {
        let claims = claims(&["reader", "admin"]);
        assert!(require_any_role(&claims, &["admin"]).is_ok());
        assert!(require_any_role(&claims, &["owner", "reader"]).is_ok());
    }

    #[test]
    fn missing_role_is_denied() {
        let error = require_any_role(&claims(&["reader"]), &["admin"]).unwrap_err();
        assert_eq!(error.code(), ErrorCode::PermissionDenied);
        assert_eq!(error.message(), DENIED_MESSAGE);
    }

    #[test]
    fn empty_requirement_is_denied() {
        assert!(require_any_role(&claims(&["admin"]), &[]).is_err());
    }

    #[test]
    fn slices_and_vectors_grant_roles() {
        let roles = vec!["admin".to_owned()];
        assert!(require_any_role(&roles, &["admin"]).is_ok());
        assert!(require_any_role(roles.as_slice(), &["admin"]).is_ok());
        assert!(require_any_role(roles.as_slice(), &["other"]).is_err());
    }
}
