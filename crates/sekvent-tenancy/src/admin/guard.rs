//! The guard a product wraps each of its admin RPC bodies in.

use std::future::Future;

use sekvent_context::{CallContext, EndUser};
use sekvent_error::AppError;

use super::audit::AdminAudit;
use super::claims::AdminClaims;
use crate::TenantId;

/// Runs `body` for the admin RPC `op` addressing `tenant`, in this order: the claims from the
/// call's end user (none is `ADMIN_TOKEN_INVALID`); `claims.op == op` and `claims.tenant ==
/// tenant` exactly (else `ADMIN_TOKEN_INVALID`); the token id recorded in `admin_audit` with the
/// time, operation, tenant and subject, outcome `started` (a second record of one id is
/// `ADMIN_TOKEN_REPLAYED` and the body does not run); the body, with the claims; the outcome
/// stored (`ok` or `failed`; a failure to store it is a warning; a cancelled call leaves
/// `started`). An error with a reason and no domain gets the admin domain.
pub async fn run<X, T, F, Fut>(
    audit: &AdminAudit,
    cx: &CallContext,
    op: &str,
    tenant: Option<TenantId>,
    body: F,
) -> Result<T, AppError>
where
    X: Clone + Send + Sync + 'static,
    F: FnOnce(AdminClaims<X>) -> Fut,
    Fut: Future<Output = Result<T, AppError>>,
{
    let admin = audit.domains().admin;
    let claims = match claims::<X>(cx) {
        Some(claims) if claims.op == op && claims.tenant == tenant => claims.clone(),
        _ => return Err(crate::error::admin::token_invalid(admin)),
    };
    audit.record(&claims).await?;
    let token_id = claims.token_id;
    let result = body(claims).await;
    audit.finish(token_id, result.is_ok()).await;
    result.map_err(|error| in_admin_domain(error, admin))
}

/// `error` with `domain` when it carries a reason and no domain; otherwise unchanged.
fn in_admin_domain(error: AppError, domain: &'static str) -> AppError {
    if error.reason().is_some() && error.domain().is_none() {
        error.with_domain(domain)
    } else {
        error
    }
}

/// The admin claims of the call, when its end user carries them.
pub fn claims<X: Send + Sync + 'static>(cx: &CallContext) -> Option<&AdminClaims<X>> {
    cx.end_user().and_then(EndUser::extension::<AdminClaims<X>>)
}

/// The end user the authenticator makes of `claims`: subject `sub`, no tenant, the claims as its
/// extension. For a product's tests of its admin bodies.
pub fn end_user<X: Clone + Send + Sync + 'static>(claims: AdminClaims<X>) -> EndUser {
    EndUser::new(claims.subject.clone()).with_extension(claims)
}

#[cfg(test)]
mod tests {
    use std::time::SystemTime;

    use sekvent_context::CallContext;
    use sekvent_error::AppError;
    use uuid::Uuid;

    use super::{claims, end_user, in_admin_domain};
    use crate::TenantId;
    use crate::admin::claims::{AdminClaims, NoExtra};

    fn sample() -> AdminClaims {
        AdminClaims {
            token_id: Uuid::from_u128(7),
            subject: "operator:ada".into(),
            tenant: Some(TenantId::from_uuid(Uuid::from_u128(9))),
            op: "billing.admin.v1.TenantAdmin/Suspend".into(),
            expires_at: SystemTime::UNIX_EPOCH,
            extra: NoExtra {},
        }
    }

    #[test]
    fn the_end_user_carries_the_claims_and_no_tenant() {
        let user = end_user(sample());
        assert_eq!(user.subject(), "operator:ada");
        assert_eq!(user.tenant(), None);
        let cx = CallContext::new().with_end_user(user);
        assert_eq!(claims::<NoExtra>(&cx), Some(&sample()));
        assert_eq!(claims::<u8>(&cx), None);
        assert_eq!(claims::<NoExtra>(&CallContext::new()), None);
    }

    #[test]
    fn only_a_reasoned_error_without_a_domain_gets_the_admin_domain() {
        let reasoned = in_admin_domain(
            AppError::failed_precondition("no").with_reason("TENANT_STATE"),
            "billing.admin.v1",
        );
        assert_eq!(reasoned.domain(), Some("billing.admin.v1"));
        let own = in_admin_domain(
            AppError::failed_precondition("no")
                .with_reason("PLAN_LIMIT")
                .with_domain("billing.plans.v1"),
            "billing.admin.v1",
        );
        assert_eq!(own.domain(), Some("billing.plans.v1"));
        let bare = in_admin_domain(AppError::unavailable("try again"), "billing.admin.v1");
        assert_eq!(bare.domain(), None);
        assert_eq!(bare.reason(), None);
    }
}
