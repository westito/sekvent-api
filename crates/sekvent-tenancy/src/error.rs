//! The reasons this crate answers with and the domains they are sent under.
//!
//! Reasons and their codes are wire contract; the messages are not. Cluster
//! operations attach a reason without a domain, because they know no
//! product. The serving half adds the serving domain and the admin guard the
//! admin domain, both taken from the product's [`ErrorDomains`].

/// The domains of this crate's reasons.
///
/// Both are `&'static str` because an end-user rejection takes nothing else.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ErrorDomains {
    /// Reasons a request to a tenant can see.
    pub serving: &'static str,
    /// Reasons an admin caller can see.
    pub admin: &'static str,
}

impl ErrorDomains {
    /// The serving domain a product gets unless it picks its own.
    pub const DEFAULT_SERVING: &'static str = "sekvent.tenancy.v1";
    /// The admin domain a product gets unless it picks its own.
    pub const DEFAULT_ADMIN: &'static str = "sekvent.tenancy.admin.v1";

    /// The default domains; the same as [`Default`].
    pub const fn new() -> Self {
        Self {
            serving: Self::DEFAULT_SERVING,
            admin: Self::DEFAULT_ADMIN,
        }
    }

    /// The same domains with `domain` for the reasons of a tenant request.
    #[must_use]
    pub const fn with_serving(self, domain: &'static str) -> Self {
        Self {
            serving: domain,
            ..self
        }
    }

    /// The same domains with `domain` for the reasons of an admin caller.
    #[must_use]
    pub const fn with_admin(self, domain: &'static str) -> Self {
        Self {
            admin: domain,
            ..self
        }
    }
}

impl Default for ErrorDomains {
    fn default() -> Self {
        Self::new()
    }
}

/// The reasons of this crate; each constant's value is its own name.
pub mod reasons {
    /// `NOT_FOUND`, serving: the tenant is unknown, purging or purged, or a
    /// host selects nothing.
    pub const TENANT_NOT_HERE: &str = "TENANT_NOT_HERE";
    /// `UNAVAILABLE`, serving: the tenant is still being set up.
    pub const TENANT_PROVISIONING: &str = "TENANT_PROVISIONING";
    /// `FAILED_PRECONDITION`, serving: the tenant is suspended.
    pub const TENANT_SUSPENDED: &str = "TENANT_SUSPENDED";
    /// `UNAVAILABLE`, serving: the tenant's database cannot be opened.
    pub const TENANT_UNAVAILABLE: &str = "TENANT_UNAVAILABLE";
    /// `UNAVAILABLE`, serving: no lookup permit, or the tenant cache is
    /// full; retried after a second.
    pub const TENANT_BUSY: &str = "TENANT_BUSY";
    /// `INVALID_ARGUMENT`, serving: a selecting host without its selector.
    pub const TENANT_SELECTOR_MISSING: &str = "TENANT_SELECTOR_MISSING";
    /// `INVALID_ARGUMENT`, serving: two selectors disagree, or an id is
    /// malformed.
    pub const TENANT_MISMATCH: &str = "TENANT_MISMATCH";
    /// `PERMISSION_DENIED`, serving: an `Origin` that is not a host of the
    /// selected tenant.
    pub const ORIGIN_NOT_ALLOWED: &str = "ORIGIN_NOT_ALLOWED";
    /// `UNAUTHENTICATED`, admin: any failure of an admin token.
    pub const ADMIN_TOKEN_INVALID: &str = "ADMIN_TOKEN_INVALID";
    /// `ALREADY_EXISTS`, admin: a second call with one token id.
    pub const ADMIN_TOKEN_REPLAYED: &str = "ADMIN_TOKEN_REPLAYED";
    /// `NOT_FOUND`, admin: a lifecycle call for an id without a control row.
    pub const TENANT_NOT_FOUND: &str = "TENANT_NOT_FOUND";
    /// `FAILED_PRECONDITION`, admin: the tenant's state refuses the call, or
    /// a host, role or database name is taken.
    pub const TENANT_STATE: &str = "TENANT_STATE";
    /// `RESOURCE_EXHAUSTED`, admin: provisioning a new id at the product's
    /// cap.
    pub const TENANT_LIMIT_REACHED: &str = "TENANT_LIMIT_REACHED";
    /// `FAILED_PRECONDITION`, admin: a handover check failed; the metadata
    /// key `check` names it.
    pub const ADOPTION_REFUSED: &str = "ADOPTION_REFUSED";
    /// `FAILED_PRECONDITION`, no domain: the control schema is missing or
    /// incomplete; the metadata key `table` or `constraint` names what.
    pub const CONTROL_SCHEMA_MISSING: &str = "CONTROL_SCHEMA_MISSING";
}

#[cfg(test)]
mod tests {
    use super::{ErrorDomains, reasons};

    #[test]
    fn the_defaults_are_the_crate_domains() {
        let domains = ErrorDomains::new();
        assert_eq!(domains.serving, "sekvent.tenancy.v1");
        assert_eq!(domains.admin, "sekvent.tenancy.admin.v1");
        assert_eq!(ErrorDomains::default(), domains);
    }

    #[test]
    fn each_override_changes_its_own_domain_only() {
        let serving = ErrorDomains::new().with_serving("billing.tenants.v1");
        assert_eq!(serving.serving, "billing.tenants.v1");
        assert_eq!(serving.admin, ErrorDomains::DEFAULT_ADMIN);

        let admin = ErrorDomains::new().with_admin("billing.admin.v1");
        assert_eq!(admin.serving, ErrorDomains::DEFAULT_SERVING);
        assert_eq!(admin.admin, "billing.admin.v1");
    }

    #[test]
    fn the_overrides_are_usable_in_a_constant() {
        const DOMAINS: ErrorDomains = ErrorDomains::new()
            .with_serving("orders.v1")
            .with_admin("orders.admin.v1");
        assert_eq!(
            DOMAINS,
            ErrorDomains {
                serving: "orders.v1",
                admin: "orders.admin.v1",
            }
        );
    }

    #[test]
    fn every_reason_is_its_own_name() {
        let all = [
            ("TENANT_NOT_HERE", reasons::TENANT_NOT_HERE),
            ("TENANT_PROVISIONING", reasons::TENANT_PROVISIONING),
            ("TENANT_SUSPENDED", reasons::TENANT_SUSPENDED),
            ("TENANT_UNAVAILABLE", reasons::TENANT_UNAVAILABLE),
            ("TENANT_BUSY", reasons::TENANT_BUSY),
            ("TENANT_SELECTOR_MISSING", reasons::TENANT_SELECTOR_MISSING),
            ("TENANT_MISMATCH", reasons::TENANT_MISMATCH),
            ("ORIGIN_NOT_ALLOWED", reasons::ORIGIN_NOT_ALLOWED),
            ("ADMIN_TOKEN_INVALID", reasons::ADMIN_TOKEN_INVALID),
            ("ADMIN_TOKEN_REPLAYED", reasons::ADMIN_TOKEN_REPLAYED),
            ("TENANT_NOT_FOUND", reasons::TENANT_NOT_FOUND),
            ("TENANT_STATE", reasons::TENANT_STATE),
            ("TENANT_LIMIT_REACHED", reasons::TENANT_LIMIT_REACHED),
            ("ADOPTION_REFUSED", reasons::ADOPTION_REFUSED),
            ("CONTROL_SCHEMA_MISSING", reasons::CONTROL_SCHEMA_MISSING),
        ];
        for (name, value) in all {
            assert_eq!(value, name);
        }
    }
}
