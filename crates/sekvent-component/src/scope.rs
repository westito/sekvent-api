//! The request scope: what a serving boundary resolves from the request
//! head before it authenticates the caller, and the tenant rule that
//! applies once it has.

use std::future::Future;
use std::pin::Pin;

use http::request::Parts;
use sekvent_context::{CallContext, EndUser, RequestScope};
use sekvent_error::{AppError, ErrorCode};
use tokio::time::Instant;

use crate::{END_USER_REJECTED_MESSAGE, reasons};

/// Resolves the [`RequestScope`] of a request before its caller is
/// authenticated: the tenant the request belongs to (from the host, say)
/// and typed values its handlers need (that tenant's database).
///
/// Register one per App with
/// [`AppBuilder::scope_resolver`](crate::AppBuilder::scope_resolver). A
/// sync closure `Fn(&http::request::Parts, &CallContext) -> Result<RequestScope, AppError>`
/// is a resolver; implement the trait for resolution that needs I/O.
///
/// It runs once for every call served over gRPC or gRPC-Web that no link
/// token authenticated (end users, anonymous and optional-end-user methods,
/// any caller under `SERVE_AUTH=none`), after routing and the
/// address-keyed client limit and before the end-user authenticator, on
/// the request head alone; never for a link caller or an in-process call.
/// `request.uri.path()` is the routed path `/<package>.<Service>/<Rpc>`,
/// so a resolver can return [`RequestScope::new`] for methods that must
/// not open a tenant's resources before the caller is authorized. `cx` is
/// the context decoded from the head, with the call's deadline.
///
/// Its error is the caller's answer, unchanged (`UNAUTHENTICATED`
/// included). It runs within the call's deadline, never once that deadline
/// has passed, and a panic is answered like a handler panic. A tenant the
/// scope names must pass
/// [`is_valid_tenant`](sekvent_context::headers::is_valid_tenant); any
/// other fails the call with `INTERNAL` and
/// [`SCOPE_TENANT_INVALID`](crate::reasons::SCOPE_TENANT_INVALID). When the
/// scope names a tenant, only end users of that tenant are admitted
/// ([`admit_end_user`]). REST routes run it through
/// [`App::resolve_scope`](crate::App::resolve_scope).
pub trait ScopeResolver: Send + Sync + 'static {
    /// The scope of `request`. `cx` is the context decoded from its head.
    fn resolve<'a>(
        &'a self,
        request: &'a Parts,
        cx: &'a CallContext,
    ) -> Pin<Box<dyn Future<Output = Result<RequestScope, AppError>> + Send + 'a>>;
}

impl<F> ScopeResolver for F
where
    F: Fn(&Parts, &CallContext) -> Result<RequestScope, AppError> + Send + Sync + 'static,
{
    fn resolve<'a>(
        &'a self,
        request: &'a Parts,
        cx: &'a CallContext,
    ) -> Pin<Box<dyn Future<Output = Result<RequestScope, AppError>> + Send + 'a>> {
        Box::pin(std::future::ready(self(request, cx)))
    }
}

/// Make `user` the end user of `cx`, as the component service does once
/// its authenticator accepted `user`.
///
/// When `cx`'s [scope](CallContext::scope) names a tenant, `user` must
/// name the same tenant: a user of another tenant, and a user without a
/// tenant, is refused with `UNAUTHENTICATED` and
/// [`END_USER_REJECTED_MESSAGE`], no reason and no metadata (the same
/// answer as a bad credential). When the scope names no tenant, or `cx`
/// has no scope, any user is admitted. The result is
/// `cx.with_end_user(user)`: its tenant is the scope's when it names one
/// (and then the user's too), the user's otherwise.
pub fn admit_end_user(cx: CallContext, user: EndUser) -> Result<CallContext, AppError> {
    if !admits(cx.scope(), &user) {
        return Err(refused());
    }
    Ok(cx.with_end_user(user))
}

/// Whether `scope` admits `user`: a scope that names a tenant admits only
/// users of that tenant.
pub(crate) fn admits(scope: Option<&RequestScope>, user: &EndUser) -> bool {
    match scope.and_then(RequestScope::tenant) {
        Some(scoped) => user.tenant() == Some(scoped),
        None => true,
    }
}

/// The answer to an end user the scope does not admit.
pub(crate) fn refused() -> AppError {
    tracing::debug!("an end user who does not belong to the request's tenant was refused");
    AppError::unauthenticated(END_USER_REJECTED_MESSAGE)
}

/// `scope`, unless it names a tenant that fails
/// [`is_valid_tenant`](sekvent_context::headers::is_valid_tenant):
/// `INTERNAL` with [`SCOPE_TENANT_INVALID`](reasons::SCOPE_TENANT_INVALID).
/// Only the tenant's length is logged.
pub(crate) fn checked(scope: RequestScope) -> Result<RequestScope, AppError> {
    match scope.tenant() {
        Some(tenant) if !sekvent_context::headers::is_valid_tenant(tenant) => {
            tracing::warn!(
                tenant_len = tenant.len(),
                "the scope resolver returned a tenant that is not a valid tenant"
            );
            Err(AppError::new(
                ErrorCode::Internal,
                "the scope resolver returned an invalid tenant",
            )
            .with_reason(reasons::SCOPE_TENANT_INVALID))
        }
        _ => Ok(scope),
    }
}

/// Run `resolver` within `bound`, `DEADLINE_EXCEEDED` past it.
pub(crate) async fn resolve_within(
    resolver: &dyn ScopeResolver,
    request: &Parts,
    cx: &CallContext,
    bound: Option<Instant>,
    expired: impl FnOnce() -> AppError,
) -> Result<RequestScope, AppError> {
    let attempt = resolver.resolve(request, cx);
    match bound {
        Some(bound) => tokio::time::timeout_at(bound, attempt)
            .await
            .map_err(|_elapsed| expired())?,
        None => attempt.await,
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    fn parts(host: &str) -> Parts {
        http::Request::builder()
            .uri("/shop.v1.Orders/Place")
            .header(http::header::HOST, host)
            .body(())
            .unwrap()
            .into_parts()
            .0
    }

    fn by_host(request: &Parts, _cx: &CallContext) -> Result<RequestScope, AppError> {
        match request
            .headers
            .get(http::header::HOST)
            .and_then(|value| value.to_str().ok())
        {
            Some("a.example.test") => Ok(RequestScope::new().with_tenant("tenant-a")),
            _ => Err(AppError::not_found("unknown site").with_reason("SITE_UNKNOWN")),
        }
    }

    #[tokio::test]
    async fn a_closure_is_a_resolver_and_its_error_passes() {
        let cx = CallContext::new();
        let scope = by_host
            .resolve(&parts("a.example.test"), &cx)
            .await
            .unwrap();
        assert_eq!(scope.tenant(), Some("tenant-a"));
        let error = by_host
            .resolve(&parts("b.example.test"), &cx)
            .await
            .unwrap_err();
        assert_eq!(error.code(), ErrorCode::NotFound);
        assert_eq!(error.message(), "unknown site");
        assert_eq!(error.reason(), Some("SITE_UNKNOWN"));
    }

    struct Hangs;

    impl ScopeResolver for Hangs {
        fn resolve<'a>(
            &'a self,
            _request: &'a Parts,
            _cx: &'a CallContext,
        ) -> Pin<Box<dyn Future<Output = Result<RequestScope, AppError>> + Send + 'a>> {
            Box::pin(std::future::pending())
        }
    }

    #[tokio::test(start_paused = true)]
    async fn resolution_is_bounded() {
        let cx = CallContext::new();
        let bound = Instant::now() + Duration::from_millis(50);
        let error = resolve_within(&Hangs, &parts("a"), &cx, Some(bound), || {
            AppError::deadline_exceeded("late")
        })
        .await
        .unwrap_err();
        assert_eq!(error.message(), "late");
        let scope = resolve_within(
            &by_host,
            &parts("a.example.test"),
            &cx,
            None,
            || unreachable!(),
        )
        .await
        .unwrap();
        assert_eq!(scope.tenant(), Some("tenant-a"));
    }

    fn scoped(tenant: Option<&str>) -> CallContext {
        let scope = match tenant {
            Some(tenant) => RequestScope::new().with_tenant(tenant),
            None => RequestScope::new(),
        };
        CallContext::new().with_scope(scope)
    }

    #[test]
    fn admission_follows_the_tenant_rule() {
        // No scope tenant: the user's tenant, or none.
        let cx = admit_end_user(scoped(None), EndUser::new("u").with_tenant("t-u")).unwrap();
        assert_eq!(cx.tenant(), Some("t-u"));
        let cx = admit_end_user(scoped(None), EndUser::new("u")).unwrap();
        assert_eq!(cx.tenant(), None);
        // Without any scope, as before.
        let cx = admit_end_user(CallContext::new(), EndUser::new("u").with_tenant("t-u")).unwrap();
        assert_eq!(cx.tenant(), Some("t-u"));
        assert_eq!(cx.end_user().map(EndUser::subject), Some("u"));

        // A scope tenant and a user of the same one.
        let cx = admit_end_user(scoped(Some("t-s")), EndUser::new("u").with_tenant("t-s")).unwrap();
        assert_eq!(cx.tenant(), Some("t-s"));
        assert_eq!(cx.subject(), Some("u"));

        // Another tenant, or none at all: the constant answer.
        for user in [EndUser::new("u").with_tenant("t-u"), EndUser::new("u")] {
            let error = admit_end_user(scoped(Some("t-s")), user).unwrap_err();
            assert_eq!(error.code(), ErrorCode::Unauthenticated);
            assert_eq!(error.message(), END_USER_REJECTED_MESSAGE);
            assert_eq!(error.reason(), None);
            assert!(error.metadata().is_empty());
        }
    }

    #[test]
    fn a_scope_with_an_invalid_tenant_is_internal() {
        let long = "t".repeat(256);
        for tenant in ["", "t 1", "tenänt", long.as_str()] {
            let error = checked(RequestScope::new().with_tenant(tenant)).unwrap_err();
            assert_eq!(error.code(), ErrorCode::Internal, "{tenant:?}");
            assert_eq!(error.reason(), Some(reasons::SCOPE_TENANT_INVALID));
            assert_eq!(
                error.message(),
                "the scope resolver returned an invalid tenant"
            );
            assert!(error.metadata().is_empty());
        }
        let scope = checked(RequestScope::new().with_tenant("t-1")).unwrap();
        assert_eq!(scope.tenant(), Some("t-1"));
        let scope = checked(RequestScope::new()).unwrap();
        assert_eq!(scope.tenant(), None);
    }
}
