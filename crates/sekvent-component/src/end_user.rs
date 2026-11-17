//! End-user authentication for components served with
//! `SEKVENT_COMPONENT_<C>_SERVE_AUTH=bearer` or `link,bearer`.

use std::fmt;
use std::future::Future;
use std::pin::Pin;

use http::request::Parts;
use sekvent_context::EndUser;
use sekvent_error::{AppError, ErrorCode};

use crate::CallTarget;

/// The message of every end-user authentication failure: one answer for a
/// missing, malformed, expired or unknown credential.
pub const END_USER_REJECTED_MESSAGE: &str = "invalid or expired credentials";

/// Authenticates the end users of components served with
/// `SEKVENT_COMPONENT_<C>_SERVE_AUTH=bearer` or `link,bearer`, from the
/// request head.
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
/// The App's [`EndUserPrecheck`] has accepted the request head before it,
/// the request has been read whole (the authenticator never sees the
/// body), and the App's [`ScopeResolver`](crate::ScopeResolver), when it
/// has one, has run: the resolved scope is in `request.extensions`
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

/// A cheap check of the end-user credential on the request head, made
/// before anything that costs: before the call is admitted, before its
/// request is read and before the App's scope resolver runs.
///
/// It runs for exactly the calls the [`EndUserAuthenticator`] will see
/// (`SERVE_AUTH` includes `bearer`, no link token matched, the method is not
/// `#[call(anonymous)]`, and an `#[call(optional_end_user)]` method only
/// when the request carries an `Authorization` header), on the head alone,
/// synchronously. Its job is to refuse what can never be a credential (no
/// header, a malformed one, another scheme) before such a call takes any
/// shared capacity; the authenticator still decides everything else.
///
/// Register one per App with
/// [`AppBuilder::end_user_precheck`](crate::AppBuilder::end_user_precheck);
/// without one the App uses [`BearerShape`]. A sync closure
/// `Fn(&http::request::Parts, CallTarget) -> Result<(), AppError>` is a
/// precheck. It must not block or do I/O: look at the headers, never at a
/// store.
///
/// Its error is answered like the authenticator's: `UNAUTHENTICATED`
/// becomes the constant answer ([`END_USER_REJECTED_MESSAGE`], or an
/// [`EndUserRejection`]'s reason and domain), any other code passes
/// unchanged, and a panic is `INTERNAL` /
/// [`HANDLER_PANICKED`](crate::reasons::HANDLER_PANICKED).
///
/// # Warning
///
/// Only `UNAUTHENTICATED` is replaced by the constant answer. An error with
/// any other code reaches the caller exactly as returned: its message,
/// reason and metadata included. Never put a header value, a token or
/// anything else read from the request into such an error (`format!` of a
/// header included); return a fixed message, or refuse with
/// [`EndUserRejection`]. A panic is caught and answered without its
/// payload, but it still goes through the process's panic hook first, which
/// by default prints the panic message: do not panic with request data in
/// the message either.
pub trait EndUserPrecheck: Send + Sync + 'static {
    /// `Ok` lets the call go on to its admission; `Err` is the answer.
    fn precheck(&self, request: &Parts, target: CallTarget) -> Result<(), AppError>;
}

impl<F> EndUserPrecheck for F
where
    F: Fn(&Parts, CallTarget) -> Result<(), AppError> + Send + Sync + 'static,
{
    fn precheck(&self, request: &Parts, target: CallTarget) -> Result<(), AppError> {
        self(request, target)
    }
}

/// The default [`EndUserPrecheck`]: the request carries exactly one
/// `Authorization` header, and it reads `Bearer <token>`, the scheme in any
/// case, one or more spaces, and a token68 (letters, digits,
/// `-._~+/`, then optional `=` padding) up to the end.
///
/// Anything else is refused with `UNAUTHENTICATED`, which the caller sees
/// as the constant [`END_USER_REJECTED_MESSAGE`], the same answer as for a
/// token the authenticator rejects. An App whose authenticator accepts
/// another kind of credential registers its own precheck.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BearerShape;

impl BearerShape {
    /// The token of the request's one well-formed `Authorization: Bearer`
    /// header; `None` when there is no such header, more than one, or one
    /// that is not visible ASCII, names another scheme or carries no
    /// token68.
    pub fn token(headers: &http::HeaderMap) -> Option<&str> {
        let mut values = headers.get_all(http::header::AUTHORIZATION).iter();
        let value = values.next()?;
        if values.next().is_some() {
            return None;
        }
        let (scheme, rest) = value.to_str().ok()?.split_once(' ')?;
        if !scheme.eq_ignore_ascii_case("bearer") {
            return None;
        }
        let token = rest.trim_start_matches(' ');
        is_token68(token).then_some(token)
    }
}

impl EndUserPrecheck for BearerShape {
    fn precheck(&self, request: &Parts, _target: CallTarget) -> Result<(), AppError> {
        match Self::token(&request.headers) {
            Some(_) => Ok(()),
            None => Err(AppError::unauthenticated(
                "the request carries no well-formed bearer credential",
            )),
        }
    }
}

/// `1*( ALPHA / DIGIT / "-" / "." / "_" / "~" / "+" / "/" ) *"="`.
fn is_token68(token: &str) -> bool {
    let body = token.trim_end_matches('=');
    !body.is_empty()
        && body.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~' | b'+' | b'/')
        })
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

    const METHODS: &[crate::MethodDescriptor] = &[crate::MethodDescriptor::call("place", "Place")];
    const ORDERS: &crate::ComponentDescriptor =
        &crate::ComponentDescriptor::new("orders", "Orders", METHODS);

    fn place() -> CallTarget {
        CallTarget::new(ORDERS, 0).unwrap()
    }

    #[test]
    fn the_bearer_shape_accepts_one_bearer_token68() {
        for (value, token) in [
            ("Bearer user-7", "user-7"),
            ("bearer abc.DEF_ghi~jk+l/m", "abc.DEF_ghi~jk+l/m"),
            ("BEARER   dG9rZW4=", "dG9rZW4="),
            ("Bearer eyJh.eyJz.c2ln==", "eyJh.eyJz.c2ln=="),
        ] {
            let request = parts(Some(value));
            assert_eq!(BearerShape::token(&request.headers), Some(token), "{value}");
            BearerShape.precheck(&request, place()).unwrap();
        }
    }

    #[test]
    fn the_bearer_shape_refuses_anything_else() {
        let mut twice = parts(Some("Bearer one"));
        twice
            .headers
            .append(http::header::AUTHORIZATION, "Bearer two".parse().unwrap());
        let mut opaque = parts(None);
        opaque.headers.insert(
            http::header::AUTHORIZATION,
            http::HeaderValue::from_bytes(b"Bearer t\xffoken").unwrap(),
        );
        let mut cases = vec![parts(None), twice, opaque];
        for value in [
            "",
            "Bearer",
            "Bearer ",
            "Bearer  ",
            "Basic dXNlcg==",
            "Bearertoken",
            "Bearer\ttoken",
            "Bearer to ken",
            "Bearer token ",
            "Bearer =abc",
            "Bearer ===",
            "Bearer to=ken",
            "Bearer tök",
            "Bearer to,ken",
        ] {
            cases.push(parts(Some(value)));
        }
        for request in cases {
            assert_eq!(BearerShape::token(&request.headers), None, "{request:?}");
            let error = BearerShape.precheck(&request, place()).unwrap_err();
            assert_eq!(error.code(), ErrorCode::Unauthenticated);
            assert_eq!(rejection(error).message(), END_USER_REJECTED_MESSAGE);
        }
        assert_eq!(format!("{BearerShape:?}"), "BearerShape");
    }

    #[test]
    fn a_closure_is_a_precheck() {
        let precheck = |request: &Parts, target: CallTarget| {
            if target.method().name() == "place" && request.headers.contains_key("x-api-key") {
                Ok(())
            } else {
                Err(AppError::from(EndUserRejection::new("API_KEY_REQUIRED")))
            }
        };
        let mut keyed = parts(None);
        keyed.headers.insert("x-api-key", "k-1".parse().unwrap());
        precheck.precheck(&keyed, place()).unwrap();
        let error = rejection(precheck.precheck(&parts(None), place()).unwrap_err());
        assert_eq!(error.message(), END_USER_REJECTED_MESSAGE);
        assert_eq!(error.reason(), Some("API_KEY_REQUIRED"));
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
