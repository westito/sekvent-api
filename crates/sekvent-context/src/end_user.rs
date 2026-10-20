//! The end user behind a call, as a serving boundary authenticated them.

use std::fmt;

/// An end user authenticated at a serving boundary, for example from a
/// session token a browser sent.
///
/// It reaches handlers through [`CallContext::end_user`](crate::CallContext::end_user)
/// when the end user called directly; components called further down see
/// only its subject and tenant, as a trusted link would assert them.
///
/// Besides subject, tenant and roles it holds typed *extensions*: whatever
/// else the authenticator learned and the handlers need, such as the scope
/// of an API token, at most one value per type
/// ([`with_extension`](Self::with_extension), [`extension`](Self::extension)).
/// Extensions stay in this process with the end user: they are never
/// encoded in headers, never reach the components a handler calls, and
/// `Debug` shows only how many there are.
///
/// Equality compares subject, tenant and roles; extensions are ignored, since
/// their types need not implement `PartialEq`.
#[derive(Clone)]
pub struct EndUser {
    subject: String,
    tenant: Option<String>,
    roles: Vec<String>,
    extensions: http::Extensions,
}

impl EndUser {
    /// An end user identified by `subject`, without tenant, roles or
    /// extensions.
    pub fn new(subject: impl Into<String>) -> Self {
        Self {
            subject: subject.into(),
            tenant: None,
            roles: Vec::new(),
            extensions: http::Extensions::new(),
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

    /// Attach `value` as the extension of type `T`, replacing an earlier
    /// value of the same type. Use a type of your own (a newtype), not a
    /// shared one such as `String`, so extensions from different sources
    /// cannot collide.
    #[must_use]
    pub fn with_extension<T: Clone + Send + Sync + 'static>(mut self, value: T) -> Self {
        self.extensions.insert(value);
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

    /// The extension of type `T`, if one was attached.
    pub fn extension<T: Send + Sync + 'static>(&self) -> Option<&T> {
        self.extensions.get::<T>()
    }
}

impl PartialEq for EndUser {
    fn eq(&self, other: &Self) -> bool {
        self.subject == other.subject && self.tenant == other.tenant && self.roles == other.roles
    }
}

impl Eq for EndUser {}

impl fmt::Debug for EndUser {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EndUser")
            .field("subject", &self.subject)
            .field("tenant", &self.tenant)
            .field("roles", &self.roles)
            .field("extensions", &self.extensions.len())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::CallContext;

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

    /// The scope of an API token, as an authenticator might attach it.
    #[derive(Debug, Clone, PartialEq)]
    struct TokenScope {
        permissions: Vec<&'static str>,
    }

    /// A value that must never show up in logs or headers.
    #[derive(Debug, Clone, PartialEq)]
    struct TokenId(&'static str);

    fn agent() -> EndUser {
        EndUser::new("agent-3")
            .with_tenant("tenant-a")
            .with_roles(["developer"])
            .with_extension(TokenScope {
                permissions: vec!["issues:write-scope"],
            })
            .with_extension(TokenId("token-id-9f2c"))
    }

    #[test]
    fn extensions_are_typed() {
        let user = agent();
        assert_eq!(
            user.extension::<TokenScope>(),
            Some(&TokenScope {
                permissions: vec!["issues:write-scope"],
            })
        );
        assert_eq!(user.extension::<TokenId>(), Some(&TokenId("token-id-9f2c")));
        assert_eq!(user.extension::<String>(), None, "absent type");
        assert_eq!(EndUser::new("user-7").extension::<TokenId>(), None);

        let replaced = user.with_extension(TokenId("token-id-0000"));
        assert_eq!(
            replaced.extension::<TokenId>(),
            Some(&TokenId("token-id-0000")),
            "one value per type, the last one wins"
        );
    }

    #[test]
    fn clones_keep_extensions() {
        let user = agent();
        let clone = user.clone();
        drop(user);
        assert_eq!(
            clone.extension::<TokenId>(),
            Some(&TokenId("token-id-9f2c"))
        );
        assert!(clone.extension::<TokenScope>().is_some());
    }

    #[test]
    fn equality_ignores_extensions() {
        let plain = EndUser::new("agent-3")
            .with_tenant("tenant-a")
            .with_roles(["developer"]);
        assert_eq!(agent(), plain);
        assert_ne!(agent(), plain.with_roles(["admin"]));
    }

    #[test]
    fn debug_counts_extensions_without_their_values() {
        let shown = format!("{:?}", agent());
        assert!(shown.contains("agent-3"), "{shown}");
        assert!(shown.contains("extensions: 2"), "{shown}");
        assert!(!shown.contains("token-id-9f2c"), "{shown}");
        assert!(!shown.contains("write-scope"), "{shown}");
        assert!(!shown.contains("TokenId"), "{shown}");
        assert!(format!("{:?}", EndUser::new("u")).contains("extensions: 0"));
    }

    #[test]
    fn extensions_stay_at_the_serving_boundary() {
        let cx = CallContext::new().with_end_user(agent());
        assert_eq!(
            cx.end_user().and_then(EndUser::extension::<TokenId>),
            Some(&TokenId("token-id-9f2c"))
        );

        // Calls further down get neither the end user nor its extensions.
        assert_eq!(cx.child().end_user(), None);
        assert_eq!(cx.detached().end_user(), None);

        // Nothing of them goes on the wire or into a log line.
        let mut wire = http::HeaderMap::new();
        crate::headers::inject(&cx, &mut wire);
        crate::headers::propagate(&cx.child(), &mut wire);
        crate::headers::propagate_external(&cx, &mut wire);
        for (name, value) in &wire {
            let value = value.to_str().unwrap();
            for secret in ["token-id-9f2c", "write-scope"] {
                assert!(!value.contains(secret), "{name}: {value}");
            }
        }
        let shown = format!("{cx:?}");
        assert!(!shown.contains("token-id-9f2c"), "{shown}");
        assert!(!shown.contains("write-scope"), "{shown}");
    }
}
