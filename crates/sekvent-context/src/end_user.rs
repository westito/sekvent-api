//! The end user behind a call, as a serving boundary authenticated them.

/// An end user authenticated at a serving boundary, for example from a
/// session token a browser sent.
///
/// It reaches handlers through [`CallContext::end_user`](crate::CallContext::end_user)
/// when the end user called directly; components called further down see
/// only its subject and tenant, as a trusted link would assert them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EndUser {
    subject: String,
    tenant: Option<String>,
    roles: Vec<String>,
}

impl EndUser {
    /// An end user identified by `subject`, without tenant or roles.
    pub fn new(subject: impl Into<String>) -> Self {
        Self {
            subject: subject.into(),
            tenant: None,
            roles: Vec::new(),
        }
    }

    /// Set the tenant the end user acts in.
    #[must_use]
    pub fn with_tenant(mut self, tenant: impl Into<String>) -> Self {
        self.tenant = Some(tenant.into());
        self
    }

    /// Add roles granted to the end user.
    #[must_use]
    pub fn with_roles<I, S>(mut self, roles: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.roles.extend(roles.into_iter().map(Into::into));
        self
    }

    /// The subject, e.g. a user id.
    pub fn subject(&self) -> &str {
        &self.subject
    }

    /// The tenant, if any.
    pub fn tenant(&self) -> Option<&str> {
        self.tenant.as_deref()
    }

    /// The granted roles, in the order they were added.
    pub fn roles(&self) -> &[String] {
        &self.roles
    }

    /// Whether `role` is granted.
    pub fn has_role(&self, role: &str) -> bool {
        self.roles.iter().any(|granted| granted == role)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builders_and_getters() {
        let user = EndUser::new("user-7");
        assert_eq!(user.subject(), "user-7");
        assert_eq!(user.tenant(), None);
        assert!(user.roles().is_empty());
        assert!(!user.has_role("admin"));

        let user = user
            .with_tenant("tenant-a")
            .with_roles(["reader"])
            .with_roles(vec![String::from("admin")]);
        assert_eq!(user.tenant(), Some("tenant-a"));
        assert_eq!(user.roles(), ["reader", "admin"]);
        assert!(user.has_role("admin"));
        assert!(!user.has_role("owner"));
        assert_ne!(user, EndUser::new("user-7"));
        assert!(format!("{user:?}").contains("user-7"));
    }
}
