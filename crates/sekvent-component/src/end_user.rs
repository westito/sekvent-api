//! End-user authentication for components served with
//! `SEKVENT_COMPONENT_<C>_SERVE_AUTH=bearer` or `link,bearer`.

use std::fmt;
use std::future::Future;
use std::pin::Pin;

use http::request::Parts;
use sekvent_context::EndUser;
use sekvent_error::{AppError, ErrorCode};

/// The message of every end-user authentication failure: one answer for a
/// missing, malformed, expired or unknown credential.
pub const END_USER_REJECTED_MESSAGE: &str = "invalid or expired credentials";

/// Authenticates the end users of components served with
/// `SEKVENT_COMPONENT_<C>_SERVE_AUTH=bearer` or `link,bearer`, from the
/// request head alone (the body is not read yet).
///
/// Register one per App with
/// [`AppBuilder::end_user_authenticator`](crate::AppBuilder::end_user_authenticator).
/// A sync closure `Fn(&http::request::Parts) -> Result<EndUser, AppError>`
/// is an authenticator; implement the trait for verification that needs
/// I/O (a session store, a key server).
///
/// An error with code `UNAUTHENTICATED` reaches the caller as
/// `UNAUTHENTICATED` with [`END_USER_REJECTED_MESSAGE`] and nothing else,
/// unless it was converted from an [`EndUserRejection`], whose reason and
/// domain the caller then sees as well; any other error (a session store
/// that is down) reaches it unchanged. The authenticator runs within the
/// call's deadline.
///
/// The App's [`ScopeResolver`](crate::ScopeResolver), when it has one, has
/// run before it: the resolved scope is in `request.extensions`
/// (`request.extensions.get::<RequestScope>()`). When that scope names a
/// tenant, a user the authenticator accepts who does not name the same
/// tenant (another one, or none) is refused like a bad credential
/// ([`admit_end_user`](crate::admit_end_user)).
///
/// It is not run for a link caller, for a method marked
/// `#[call(anonymous)]`, or for a method marked `#[call(optional_end_user)]`
/// when the request carries no `Authorization` header; such a call has no
/// end user. When an optional-end-user request does carry the header, the
/// authenticator runs and its errors are answered exactly as above.
pub trait EndUserAuthenticator: Send + Sync + 'static {
    /// The end user `request` was made by.
    fn authenticate<'a>(
        &'a self,
        request: &'a Parts,
    ) -> Pin<Box<dyn Future<Output = Result<EndUser, AppError>> + Send + 'a>>;
}

impl<F> EndUserAuthenticator for F
where
    F: Fn(&Parts) -> Result<EndUser, AppError> + Send + Sync + 'static,
{
    fn authenticate<'a>(
        &'a self,
        request: &'a Parts,
    ) -> Pin<Box<dyn Future<Output = Result<EndUser, AppError>> + Send + 'a>> {
        Box::pin(std::future::ready(self(request)))
    }
}

/// A refusal of an end user whose reason the caller is told.
///
/// An [`EndUserAuthenticator`] returns it converted into an [`AppError`]
/// (`Err(REJECTION.into())`); the caller then gets `UNAUTHENTICATED` with
/// [`END_USER_REJECTED_MESSAGE`], this reason and, when set, this domain,
/// and nothing else: no metadata, no retry hint, no field violations, and
/// none of what was changed on the error after the conversion. Any other
/// `UNAUTHENTICATED` from the authenticator, and one whose source was
/// replaced with `with_source`, still gets the constant answer without a
/// reason.
///
/// The reason and the domain are `&'static str`, constants of the program,
/// so nothing read from a request or a store can reach the caller through
/// them.
///
/// **What a reason gives away.** Every reason tells anyone who can present
/// a credential which outcome it met. Use one for a state the holder of a
/// once-valid credential may know (the session ended, the password must be
/// changed), never to tell an unknown user from a wrong password, or a
/// malformed token from one signed by an unknown key.
///
/// ```
/// use sekvent_component::{AppError, EndUserRejection};
///
/// const SESSION_ENDED: EndUserRejection =
///     EndUserRejection::new("SESSION_ENDED").with_domain("accounts");
///
/// let error: AppError = SESSION_ENDED.into();
/// assert_eq!(error.reason(), Some("SESSION_ENDED"));
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EndUserRejection {
    reason: &'static str,
    domain: Option<&'static str>,
}

impl EndUserRejection {
    /// A rejection the caller sees as `UNAUTHENTICATED` with `reason`
    /// (`UPPER_SNAKE_CASE` by convention).
    pub const fn new(reason: &'static str) -> Self {
        Self {
            reason,
            domain: None,
        }
    }

    /// Name the domain the reason belongs to.
    #[must_use]
    pub const fn with_domain(self, domain: &'static str) -> Self {
        Self {
            reason: self.reason,
            domain: Some(domain),
        }
    }

    /// The reason the caller sees.
    pub const fn reason(&self) -> &'static str {
        self.reason
    }

    /// The domain of the reason, if any.
    pub const fn domain(&self) -> Option<&'static str> {
        self.domain
    }

    /// The answer a caller gets for this rejection.
    fn answer(self) -> AppError {
        let error = AppError::unauthenticated(END_USER_REJECTED_MESSAGE).with_reason(self.reason);
        match self.domain {
            Some(domain) => error.with_domain(domain),
            None => error,
        }
    }
}

impl fmt::Display for EndUserRejection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "end user rejected: {}", self.reason)
    }
}

impl std::error::Error for EndUserRejection {}

/// `UNAUTHENTICATED` with [`END_USER_REJECTED_MESSAGE`], the reason, the
/// domain when there is one, and the rejection itself as the source.
impl From<EndUserRejection> for AppError {
    fn from(rejection: EndUserRejection) -> Self {
        rejection.answer().with_source(rejection)
    }
}

/// What a caller sees of an authenticator's error: an `UNAUTHENTICATED`
/// whose direct source is an [`EndUserRejection`] becomes that rejection's
/// answer, any other `UNAUTHENTICATED` the one constant answer, anything
/// else passes unchanged.
#[cfg_attr(not(feature = "grpc"), allow(dead_code))]
pub(crate) fn rejection(error: AppError) -> AppError {
    if error.code() != ErrorCode::Unauthenticated {
        return error;
    }
    match std::error::Error::source(&error)
        .and_then(|source| source.downcast_ref::<EndUserRejection>())
    {
        Some(rejection) => rejection.answer(),
        None => AppError::unauthenticated(END_USER_REJECTED_MESSAGE),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parts(authorization: Option<&str>) -> Parts {
        let mut request = http::Request::builder().uri("/shop.v1.Orders/Place");
        if let Some(value) = authorization {
            request = request.header(http::header::AUTHORIZATION, value);
        }
        request.body(()).unwrap().into_parts().0
    }

    #[tokio::test]
    async fn a_closure_is_an_authenticator() {
        let authenticator = |request: &Parts| match request.headers.get(http::header::AUTHORIZATION)
        {
            Some(_) => Ok(EndUser::new("user-7")),
            None => Err(AppError::unauthenticated("no token")),
        };
        let user = authenticator
            .authenticate(&parts(Some("Bearer t")))
            .await
            .unwrap();
        assert_eq!(user.subject(), "user-7");
        let error = authenticator.authenticate(&parts(None)).await.unwrap_err();
        assert_eq!(error.message(), "no token");
    }

    #[test]
    fn rejections_are_constant_and_other_errors_pass() {
        let error = rejection(AppError::unauthenticated("token expired at 12:00").with_reason("X"));
        assert_eq!(error.code(), ErrorCode::Unauthenticated);
        assert_eq!(error.message(), END_USER_REJECTED_MESSAGE);
        assert_eq!(error.reason(), None);
        assert!(error.metadata().is_empty());

        let error = rejection(AppError::unavailable("sessions are down"));
        assert_eq!(error.code(), ErrorCode::Unavailable);
        assert_eq!(error.message(), "sessions are down");
    }

    const SESSION_ENDED: EndUserRejection =
        EndUserRejection::new("SESSION_ENDED").with_domain("accounts");
    const BARE: EndUserRejection = EndUserRejection::new("PASSWORD_EXPIRED");

    #[test]
    fn a_rejection_converts_into_an_unauthenticated_error() {
        let error = AppError::from(SESSION_ENDED);
        assert_eq!(error.code(), ErrorCode::Unauthenticated);
        assert_eq!(error.message(), END_USER_REJECTED_MESSAGE);
        assert_eq!(error.reason(), Some("SESSION_ENDED"));
        assert_eq!(error.domain(), Some("accounts"));
        assert!(error.metadata().is_empty());
        let source = std::error::Error::source(&error)
            .and_then(|source| source.downcast_ref::<EndUserRejection>());
        assert_eq!(source, Some(&SESSION_ENDED));

        let bare = AppError::from(BARE);
        assert_eq!(bare.reason(), Some("PASSWORD_EXPIRED"));
        assert_eq!(bare.domain(), None);
    }

    #[test]
    fn a_rejection_shows_its_reason_and_nothing_else() {
        let answer = rejection(SESSION_ENDED.into());
        assert_eq!(answer.code(), ErrorCode::Unauthenticated);
        assert_eq!(answer.message(), END_USER_REJECTED_MESSAGE);
        assert_eq!(answer.reason(), Some("SESSION_ENDED"));
        assert_eq!(answer.domain(), Some("accounts"));
        assert!(std::error::Error::source(&answer).is_none());

        // Whatever was changed after the conversion is not sent.
        let changed = AppError::from(BARE)
            .with_reason("SESSION_ID_7F3A")
            .with_domain("internal")
            .with_metadata("session", "7f3a")
            .with_retry_after(std::time::Duration::from_secs(5))
            .with_field_violation("token", "expired at 12:00");
        let answer = rejection(changed);
        assert_eq!(answer.message(), END_USER_REJECTED_MESSAGE);
        assert_eq!(answer.reason(), Some("PASSWORD_EXPIRED"));
        assert_eq!(answer.domain(), None);
        assert!(answer.metadata().is_empty());
        assert_eq!(answer.retry_after(), None);
        assert_eq!(answer.field_violations(), []);
    }

    /// Wraps a rejection one level further down.
    #[derive(Debug)]
    struct Wrapper(EndUserRejection);

    impl fmt::Display for Wrapper {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("wrapped")
        }
    }

    impl std::error::Error for Wrapper {
        fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
            Some(&self.0)
        }
    }

    #[test]
    fn a_replaced_or_buried_rejection_gets_the_constant_answer() {
        let replaced = AppError::from(SESSION_ENDED).with_source(std::io::Error::other("x"));
        let answer = rejection(replaced);
        assert_eq!(answer.reason(), None);
        assert_eq!(answer.domain(), None);

        let buried = AppError::unauthenticated("x").with_source(Wrapper(SESSION_ENDED));
        assert_eq!(rejection(buried).reason(), None);

        // Another code is never turned into a rejection.
        let other = AppError::unavailable("sessions are down").with_source(SESSION_ENDED);
        let answer = rejection(other);
        assert_eq!(answer.code(), ErrorCode::Unavailable);
        assert_eq!(answer.message(), "sessions are down");
        assert_eq!(answer.reason(), None);
    }

    #[test]
    fn display_debug_and_accessors() {
        assert_eq!(
            SESSION_ENDED.to_string(),
            "end user rejected: SESSION_ENDED"
        );
        assert!(format!("{SESSION_ENDED:?}").contains("accounts"));
        assert_eq!(SESSION_ENDED.reason(), "SESSION_ENDED");
        assert_eq!(SESSION_ENDED.domain(), Some("accounts"));
        assert_eq!(BARE.domain(), None);
        assert_ne!(SESSION_ENDED, BARE);
    }
}
