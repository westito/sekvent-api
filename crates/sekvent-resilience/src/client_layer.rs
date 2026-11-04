//! [`ClientLimitLayer`]: a [`ClientLimiter`] in front of axum routes.

use std::convert::Infallible;
use std::task::{Context, Poll};

use axum::response::{IntoResponse, Response};
use futures::future::{Either, Ready, ready};
use http::Request;
use sekvent_context::CallContext;

use crate::ClientLimiter;

/// Limits the requests of each client with a [`ClientLimiter`], keyed from
/// the [`CallContext`] in the request extensions (the sekvent server puts
/// one there, with the resolved client address). A request without a
/// context counts against the shared unknown bucket.
///
/// A refused request is answered `429` with `Retry-After` and the error
/// body (reason `CLIENT_RATE_LIMITED`) without reaching the inner service.
/// Share the buckets of a served component method by passing the limiter
/// `App::client_limiter` returns, so a REST facade over the method drains
/// the same buckets as its gRPC callers.
#[derive(Debug, Clone)]
pub struct ClientLimitLayer {
    limiter: ClientLimiter,
}

impl ClientLimitLayer {
    /// A layer drawing from `limiter`.
    pub fn new(limiter: ClientLimiter) -> Self {
        Self { limiter }
    }
}

impl<S> tower::Layer<S> for ClientLimitLayer {
    type Service = ClientLimitService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        ClientLimitService {
            inner,
            limiter: self.limiter.clone(),
        }
    }
}

/// The service [`ClientLimitLayer`] wraps around its inner one.
#[derive(Debug, Clone)]
pub struct ClientLimitService<S> {
    inner: S,
    limiter: ClientLimiter,
}

impl<S, B> tower::Service<Request<B>> for ClientLimitService<S>
where
    S: tower::Service<Request<B>, Response = Response, Error = Infallible>,
{
    type Response = Response;
    type Error = Infallible;
    type Future = Either<Ready<Result<Response, Infallible>>, S::Future>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Infallible>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, request: Request<B>) -> Self::Future {
        let checked = match request.extensions().get::<CallContext>() {
            Some(cx) => self.limiter.check(cx),
            None => self.limiter.check(&CallContext::new()),
        };
        match checked {
            Ok(()) => Either::Right(self.inner.call(request)),
            Err(error) => Either::Left(ready(Ok(error.into_response()))),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::net::{IpAddr, Ipv4Addr};
    use std::time::Duration;

    use axum::Router;
    use axum::body::Body;
    use axum::routing::get;
    use http::StatusCode;
    use http::header::RETRY_AFTER;
    use tower::ServiceExt as _;

    use super::*;
    use crate::{ClientKey, KeyedLimit};

    fn request(addr: Option<u8>) -> Request<Body> {
        let mut request = Request::get("/quote").body(Body::empty()).unwrap();
        if let Some(last) = addr {
            let cx =
                CallContext::new().with_client_addr(IpAddr::V4(Ipv4Addr::new(192, 0, 2, last)));
            request.extensions_mut().insert(cx);
        }
        request
    }

    #[tokio::test(start_paused = true)]
    async fn a_flooding_client_is_refused_before_the_handler() {
        let limiter = ClientLimiter::new(
            KeyedLimit::new(1, Duration::from_mins(1)),
            ClientKey::ClientAddr,
        )
        .unwrap();
        let router = Router::new()
            .route("/quote", get(|| async { "ok" }))
            .layer(ClientLimitLayer::new(limiter.clone()));

        let first = router.clone().oneshot(request(Some(1))).await.unwrap();
        assert_eq!(first.status(), StatusCode::OK);
        let second = router.clone().oneshot(request(Some(1))).await.unwrap();
        assert_eq!(second.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(second.headers()[RETRY_AFTER], "60");
        let other = router.clone().oneshot(request(Some(2))).await.unwrap();
        assert_eq!(other.status(), StatusCode::OK);

        let anonymous = router.clone().oneshot(request(None)).await.unwrap();
        assert_eq!(anonymous.status(), StatusCode::OK);
        let again = router.clone().oneshot(request(None)).await.unwrap();
        assert_eq!(
            again.status(),
            StatusCode::TOO_MANY_REQUESTS,
            "requests without a context share one bucket"
        );
        assert_eq!(limiter.limiter().len(), 3);
    }
}
