//! The reasons this crate answers with and the domains they are sent under.
//!
//! Reasons and their codes are wire contract; the messages are not. Cluster
//! operations attach a reason without a domain, because they know no
//! product. The tenant cache (`Tenants`) adds the serving domain taken from
//! the product's [`ErrorDomains`]; the admin guard gives a reasoned error
//! without a domain the admin domain. Lifecycle operations answer without
//! a domain, like the cluster operations.

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
    /// `UNAVAILABLE`, serving: no lookup permit is free, a caller joined
    /// another caller's lookup twice without getting an answer, or the
    /// tenant cache is full; the retry hint is one second.
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

/// The serving errors of this crate, without a domain.
///
/// Only the tenant cache (`Tenants`) adds the product's serving domain, with
/// [`in_domain`](serving::in_domain), where a request sees them; the
/// resolver selects and opens through `Tenants` and passes its errors on
/// unchanged.
#[cfg(feature = "serving")]
pub(crate) mod serving {
    use std::time::Duration;

    use sekvent_error::AppError;

    use super::{ErrorDomains, reasons};
    use crate::TenantState;

    /// The retry hint of `TENANT_BUSY`.
    pub(crate) const BUSY_RETRY_AFTER: Duration = Duration::from_secs(1);

    /// The reasons a request to a tenant can see.
    const REASONS: [&str; 8] = [
        reasons::TENANT_NOT_HERE,
        reasons::TENANT_PROVISIONING,
        reasons::TENANT_SUSPENDED,
        reasons::TENANT_UNAVAILABLE,
        reasons::TENANT_BUSY,
        reasons::TENANT_SELECTOR_MISSING,
        reasons::TENANT_MISMATCH,
        reasons::ORIGIN_NOT_ALLOWED,
    ];

    /// `NOT_FOUND` / `TENANT_NOT_HERE`.
    pub(crate) fn not_here() -> AppError {
        AppError::not_found("this tenant is not served here").with_reason(reasons::TENANT_NOT_HERE)
    }

    /// `UNAVAILABLE` / `TENANT_PROVISIONING`.
    pub(crate) fn provisioning() -> AppError {
        AppError::unavailable("this tenant is being set up")
            .with_reason(reasons::TENANT_PROVISIONING)
    }

    /// `FAILED_PRECONDITION` / `TENANT_SUSPENDED`.
    pub(crate) fn suspended() -> AppError {
        AppError::failed_precondition("this tenant is suspended")
            .with_reason(reasons::TENANT_SUSPENDED)
    }

    /// `UNAVAILABLE` / `TENANT_UNAVAILABLE`.
    pub(crate) fn unavailable() -> AppError {
        AppError::unavailable("this tenant is unavailable, try again later")
            .with_reason(reasons::TENANT_UNAVAILABLE)
    }

    /// `UNAVAILABLE` / `TENANT_BUSY`, retry hint [`BUSY_RETRY_AFTER`].
    pub(crate) fn busy() -> AppError {
        AppError::unavailable("the server is busy, try again shortly")
            .with_reason(reasons::TENANT_BUSY)
            .with_retry_after(BUSY_RETRY_AFTER)
    }

    /// `INVALID_ARGUMENT` / `TENANT_SELECTOR_MISSING`.
    pub(crate) fn selector_missing() -> AppError {
        AppError::invalid_argument("the request names no tenant")
            .with_reason(reasons::TENANT_SELECTOR_MISSING)
    }

    /// `INVALID_ARGUMENT` / `TENANT_MISMATCH`.
    pub(crate) fn mismatch() -> AppError {
        AppError::invalid_argument("the request names two different tenants")
            .with_reason(reasons::TENANT_MISMATCH)
    }

    /// `PERMISSION_DENIED` / `ORIGIN_NOT_ALLOWED`.
    pub(crate) fn origin_not_allowed() -> AppError {
        AppError::permission_denied("this origin may not call this tenant")
            .with_reason(reasons::ORIGIN_NOT_ALLOWED)
    }

    /// The refusal a row state means for a request: `None` for `Active`;
    /// [`provisioning`], [`suspended`]; [`not_here`] for `Purging` and
    /// `Purged`.
    pub(crate) fn for_state(state: TenantState) -> Option<AppError> {
        match state {
            TenantState::Active => None,
            TenantState::Provisioning => Some(provisioning()),
            TenantState::Suspended => Some(suspended()),
            TenantState::Purging | TenantState::Purged => Some(not_here()),
        }
    }

    /// Whether `error` carries one of the eight serving reasons above.
    pub(crate) fn is_serving(error: &AppError) -> bool {
        error
            .reason()
            .is_some_and(|reason| REASONS.contains(&reason))
    }

    /// `error` with `domains.serving` when it carries a serving reason and no
    /// domain; otherwise unchanged.
    pub(crate) fn in_domain(error: AppError, domains: ErrorDomains) -> AppError {
        if error.domain().is_none() && is_serving(&error) {
            error.with_domain(domains.serving)
        } else {
            error
        }
    }
}

/// `FAILED_PRECONDITION` / `TENANT_STATE` ("the tenant's state does not allow
/// this"), no domain: a host, role or database name another tenant holds.
#[cfg(feature = "serving")]
pub(crate) fn tenant_state() -> sekvent_error::AppError {
    sekvent_error::AppError::failed_precondition("the tenant's state does not allow this")
        .with_reason(reasons::TENANT_STATE)
}

/// `NOT_FOUND` / `TENANT_NOT_FOUND` ("no such tenant"), no domain: a
/// lifecycle call for an id without a control row.
#[cfg(feature = "lifecycle")]
pub(crate) fn tenant_not_found() -> sekvent_error::AppError {
    sekvent_error::AppError::not_found("no such tenant").with_reason(reasons::TENANT_NOT_FOUND)
}

/// `RESOURCE_EXHAUSTED` / `TENANT_LIMIT_REACHED` ("the tenant limit is
/// reached"), no domain: provisioning a new id at the product's cap.
#[cfg(feature = "lifecycle")]
pub(crate) fn tenant_limit_reached() -> sekvent_error::AppError {
    sekvent_error::AppError::resource_exhausted("the tenant limit is reached")
        .with_reason(reasons::TENANT_LIMIT_REACHED)
}

/// The admin surface's two reasons, in the product's admin domain.
#[cfg(feature = "admin")]
pub(crate) mod admin {
    use sekvent_component::END_USER_REJECTED_MESSAGE;
    use sekvent_error::AppError;

    use super::reasons;

    /// `UNAUTHENTICATED` / `ADMIN_TOKEN_INVALID` with the component layer's
    /// constant rejection message, in `domain`: every failure of an admin
    /// token, whatever the cause.
    pub(crate) fn token_invalid(domain: &'static str) -> AppError {
        AppError::unauthenticated(END_USER_REJECTED_MESSAGE)
            .with_reason(reasons::ADMIN_TOKEN_INVALID)
            .with_domain(domain)
    }

    /// `ALREADY_EXISTS` / `ADMIN_TOKEN_REPLAYED` ("the admin token was used
    /// before"), in `domain`.
    pub(crate) fn token_replayed(domain: &'static str) -> AppError {
        AppError::already_exists("the admin token was used before")
            .with_reason(reasons::ADMIN_TOKEN_REPLAYED)
            .with_domain(domain)
    }
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

    #[cfg(feature = "serving")]
    mod serving {
        use std::time::Duration;

        use sekvent_error::{AppError, ErrorCode};

        use super::super::{ErrorDomains, reasons, serving, tenant_state};
        use crate::TenantState;

        fn assert_error(error: &AppError, code: ErrorCode, reason: &str, message: &str) {
            assert_eq!(error.code(), code);
            assert_eq!(error.reason(), Some(reason));
            assert_eq!(error.message(), message);
            assert_eq!(error.domain(), None);
        }

        fn serving_errors() -> [AppError; 8] {
            [
                serving::not_here(),
                serving::provisioning(),
                serving::suspended(),
                serving::unavailable(),
                serving::busy(),
                serving::selector_missing(),
                serving::mismatch(),
                serving::origin_not_allowed(),
            ]
        }

        #[test]
        fn not_here_is_not_found_with_its_reason() {
            let error = serving::not_here();
            assert_error(
                &error,
                ErrorCode::NotFound,
                reasons::TENANT_NOT_HERE,
                "this tenant is not served here",
            );
            assert_eq!(error.retry_after(), None);
        }

        #[test]
        fn provisioning_is_unavailable_with_its_reason() {
            let error = serving::provisioning();
            assert_error(
                &error,
                ErrorCode::Unavailable,
                reasons::TENANT_PROVISIONING,
                "this tenant is being set up",
            );
            assert_eq!(error.retry_after(), None);
        }

        #[test]
        fn suspended_is_a_failed_precondition_with_its_reason() {
            let error = serving::suspended();
            assert_error(
                &error,
                ErrorCode::FailedPrecondition,
                reasons::TENANT_SUSPENDED,
                "this tenant is suspended",
            );
            assert_eq!(error.retry_after(), None);
        }

        #[test]
        fn unavailable_is_unavailable_with_its_reason() {
            let error = serving::unavailable();
            assert_error(
                &error,
                ErrorCode::Unavailable,
                reasons::TENANT_UNAVAILABLE,
                "this tenant is unavailable, try again later",
            );
            assert_eq!(error.retry_after(), None);
        }

        #[test]
        fn busy_is_unavailable_and_asks_for_a_retry_after_one_second() {
            let error = serving::busy();
            assert_error(
                &error,
                ErrorCode::Unavailable,
                reasons::TENANT_BUSY,
                "the server is busy, try again shortly",
            );
            assert_eq!(error.retry_after(), Some(Duration::from_secs(1)));
            assert_eq!(serving::BUSY_RETRY_AFTER, Duration::from_secs(1));
        }

        #[test]
        fn selector_missing_is_an_invalid_argument_with_its_reason() {
            let error = serving::selector_missing();
            assert_error(
                &error,
                ErrorCode::InvalidArgument,
                reasons::TENANT_SELECTOR_MISSING,
                "the request names no tenant",
            );
            assert_eq!(error.retry_after(), None);
        }

        #[test]
        fn mismatch_is_an_invalid_argument_with_its_reason() {
            let error = serving::mismatch();
            assert_error(
                &error,
                ErrorCode::InvalidArgument,
                reasons::TENANT_MISMATCH,
                "the request names two different tenants",
            );
            assert_eq!(error.retry_after(), None);
        }

        #[test]
        fn origin_not_allowed_is_permission_denied_with_its_reason() {
            let error = serving::origin_not_allowed();
            assert_error(
                &error,
                ErrorCode::PermissionDenied,
                reasons::ORIGIN_NOT_ALLOWED,
                "this origin may not call this tenant",
            );
            assert_eq!(error.retry_after(), None);
        }

        #[test]
        fn tenant_state_is_a_failed_precondition_without_a_domain() {
            let error = tenant_state();
            assert_error(
                &error,
                ErrorCode::FailedPrecondition,
                reasons::TENANT_STATE,
                "the tenant's state does not allow this",
            );
            assert_eq!(error.retry_after(), None);
        }

        #[test]
        fn each_state_maps_to_its_refusal() {
            let table = [
                (TenantState::Active, None),
                (
                    TenantState::Provisioning,
                    Some(reasons::TENANT_PROVISIONING),
                ),
                (TenantState::Suspended, Some(reasons::TENANT_SUSPENDED)),
                (TenantState::Purging, Some(reasons::TENANT_NOT_HERE)),
                (TenantState::Purged, Some(reasons::TENANT_NOT_HERE)),
            ];
            for (state, reason) in table {
                let refusal = serving::for_state(state);
                assert_eq!(
                    refusal.as_ref().and_then(AppError::reason),
                    reason,
                    "{state:?}"
                );
                assert_eq!(refusal.as_ref().and_then(AppError::domain), None);
            }
        }

        #[test]
        fn only_the_eight_serving_reasons_are_serving_errors() {
            for error in serving_errors() {
                assert!(serving::is_serving(&error), "{:?}", error.reason());
            }
            assert!(!serving::is_serving(&tenant_state()));
            let capacity = AppError::resource_exhausted("the cache is full")
                .with_reason(sekvent_db::reasons::KEYED_CAPACITY);
            assert!(!serving::is_serving(&capacity));
            assert!(!serving::is_serving(&AppError::unavailable(
                "no reason at all"
            )));
        }

        #[test]
        fn in_domain_adds_the_serving_domain_to_a_serving_reason_only() {
            let domains = ErrorDomains::new().with_serving("orders.tenants.v1");
            for error in serving_errors() {
                let code = error.code();
                let message = error.message().to_owned();
                let reason = error.reason().map(str::to_owned);
                let retry = error.retry_after();
                let placed = serving::in_domain(error, domains);
                assert_eq!(placed.domain(), Some("orders.tenants.v1"));
                assert_eq!(placed.code(), code);
                assert_eq!(placed.message(), message);
                assert_eq!(placed.reason().map(str::to_owned), reason);
                assert_eq!(placed.retry_after(), retry);
            }

            let state = serving::in_domain(tenant_state(), domains);
            assert_eq!(state.domain(), None);
            assert_eq!(state.reason(), Some(reasons::TENANT_STATE));

            let plain = serving::in_domain(AppError::unavailable("no reason at all"), domains);
            assert_eq!(plain.domain(), None);
            assert_eq!(plain.reason(), None);
        }

        #[test]
        fn in_domain_leaves_an_existing_domain_alone() {
            let product = serving::not_here().with_domain("billing.v1");
            let kept = serving::in_domain(product, ErrorDomains::new());
            assert_eq!(kept.domain(), Some("billing.v1"));
            assert_eq!(kept.reason(), Some(reasons::TENANT_NOT_HERE));

            let defaulted = serving::in_domain(serving::busy(), ErrorDomains::default());
            assert_eq!(defaulted.domain(), Some(ErrorDomains::DEFAULT_SERVING));
        }
    }
}
