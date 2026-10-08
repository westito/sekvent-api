//! End-user authentication for components served with
//! `SEKVENT_COMPONENT_<C>_SERVE_AUTH=bearer` or `link,bearer`.

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
/// `UNAUTHENTICATED` with [`END_USER_REJECTED_MESSAGE`] and nothing else;
/// any other error (a session store that is down) reaches it unchanged.
/// The authenticator runs within the call's deadline.
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

/// What a caller sees of an authenticator's error: a rejection becomes the
/// one constant answer, anything else passes unchanged.
#[cfg_attr(not(feature = "grpc"), allow(dead_code))]
pub(crate) fn rejection(error: AppError) -> AppError {
    if error.code() == ErrorCode::Unauthenticated {
        AppError::unauthenticated(END_USER_REJECTED_MESSAGE)
    } else {
        error
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
}
