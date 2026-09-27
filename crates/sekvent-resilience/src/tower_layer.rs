use std::task::{Context, Poll};

use futures::future::BoxFuture;
use sekvent_context::CallContext;
use sekvent_error::AppError;
use tower::{Layer, Service, ServiceExt};

use crate::Policy;

/// What [`PolicyService`] needs to know about a request.
pub trait PolicyRequest: Sized + Send + 'static {
    /// The call context (deadline, cancellation) the request runs under.
    fn context(&self) -> CallContext;

    /// Whether the request may be sent more than once.
    fn idempotent(&self) -> bool;

    /// A copy for another attempt; `None` if the request cannot be
    /// replayed, which disables retries for it.
    fn try_clone(&self) -> Option<Self>;
}

/// `http::Request` with a cloneable body.
///
/// The context is read from the request extensions (a fresh one if absent).
/// `GET`, `HEAD`, `PUT`, `DELETE`, `OPTIONS` and `TRACE` are idempotent, as is
/// any request carrying an `idempotency-key` header.
impl<B> PolicyRequest for http::Request<B>
where
    B: Clone + Send + 'static,
{
    fn context(&self) -> CallContext {
        self.extensions()
            .get::<CallContext>()
            .cloned()
            .unwrap_or_default()
    }

    fn idempotent(&self) -> bool {
        use http::Method;
        matches!(
            *self.method(),
            Method::GET
                | Method::HEAD
                | Method::PUT
                | Method::DELETE
                | Method::OPTIONS
                | Method::TRACE
        ) || self.headers().contains_key("idempotency-key")
    }

    fn try_clone(&self) -> Option<Self> {
        let mut copy = http::Request::new(self.body().clone());
        *copy.method_mut() = self.method().clone();
        *copy.uri_mut() = self.uri().clone();
        *copy.version_mut() = self.version();
        *copy.headers_mut() = self.headers().clone();
        *copy.extensions_mut() = self.extensions().clone();
        Some(copy)
    }
}

/// A tower [`Layer`] applying a [`Policy`] to every request.
#[derive(Debug, Clone)]
pub struct PolicyLayer {
    policy: Policy,
}

impl PolicyLayer {
    /// Wrap services with `policy`.
    pub fn new(policy: Policy) -> Self {
        Self { policy }
    }
}

impl<S> Layer<S> for PolicyLayer {
    type Service = PolicyService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        PolicyService {
            inner,
            policy: self.policy.clone(),
        }
    }
}

/// The service produced by [`PolicyLayer`].
///
/// Each attempt drives its own clone of the inner service to readiness, so
/// the inner service must be `Clone`. Inner errors convert into [`AppError`].
#[derive(Debug, Clone)]
pub struct PolicyService<S> {
    inner: S,
    policy: Policy,
}

impl<S> PolicyService<S> {
    /// Wrap `inner` with `policy`.
    pub fn new(inner: S, policy: Policy) -> Self {
        Self { inner, policy }
    }

    /// The wrapped service.
    pub fn get_ref(&self) -> &S {
        &self.inner
    }
}

impl<S, R> Service<R> for PolicyService<S>
where
    R: PolicyRequest,
    S: Service<R> + Clone + Send + 'static,
    S::Response: Send + 'static,
    S::Error: Into<AppError>,
    S::Future: Send + 'static,
{
    type Response = S::Response;
    type Error = AppError;
    type Future = BoxFuture<'static, Result<S::Response, AppError>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), AppError>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, request: R) -> Self::Future {
        let policy = self.policy.clone();
        let inner = self.inner.clone();
        Box::pin(async move {
            let ctx = request.context();
            let template = request.try_clone();
            let idempotent = request.idempotent() && template.is_some();
            let mut first = Some(request);
            policy
                .call(&ctx, idempotent, move || {
                    let next = first
                        .take()
                        .or_else(|| template.as_ref().and_then(PolicyRequest::try_clone));
                    let service = inner.clone();
                    async move {
                        let request =
                            next.ok_or_else(|| AppError::internal("request cannot be replayed"))?;
                        service.oneshot(request).await.map_err(Into::into)
                    }
                })
                .await
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::time::Duration;

    use sekvent_error::ErrorCode;
    use tower::service_fn;
    use tower::util::BoxCloneService;

    use super::*;
    use crate::{Backoff, RetryPolicy};

    fn policy() -> Policy {
        Policy::new("test").with_retry(RetryPolicy::new(
            3,
            Backoff::constant(Duration::from_millis(10)),
        ))
    }

    type TestService = BoxCloneService<http::Request<String>, String, AppError>;

    fn flaky(failures: u32) -> (Arc<AtomicU32>, TestService) {
        let calls = Arc::new(AtomicU32::new(0));
        let counter = Arc::clone(&calls);
        let service = service_fn(move |request: http::Request<String>| {
            let n = counter.fetch_add(1, Ordering::SeqCst) + 1;
            async move {
                if n <= failures {
                    Err(AppError::unavailable("down"))
                } else {
                    Ok(format!("{} {}", request.method(), request.body()))
                }
            }
        });
        (calls, BoxCloneService::new(service))
    }

    #[tokio::test(start_paused = true)]
    async fn retries_idempotent_http_requests() {
        let (calls, service) = flaky(2);
        let service = PolicyLayer::new(policy()).layer(service);
        let request = http::Request::get("/orders")
            .body("payload".to_owned())
            .unwrap();
        let response = service.oneshot(request).await.unwrap();
        assert_eq!(response, "GET payload");
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }

    #[tokio::test(start_paused = true)]
    async fn does_not_retry_posts_without_an_idempotency_key() {
        let (calls, service) = flaky(2);
        let service = PolicyService::new(service, policy());
        assert!(service.get_ref().clone().ready().await.is_ok());
        let request = http::Request::post("/orders").body(String::new()).unwrap();
        let error = service.clone().oneshot(request).await.unwrap_err();
        assert_eq!(error.code(), ErrorCode::Unavailable);
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        let keyed = http::Request::post("/orders")
            .header("idempotency-key", "abc")
            .body("x".to_owned())
            .unwrap();
        assert_eq!(service.oneshot(keyed).await.unwrap(), "POST x");
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }

    #[tokio::test(start_paused = true)]
    async fn context_comes_from_extensions() {
        let (calls, service) = flaky(0);
        let service = PolicyLayer::new(policy()).layer(service);
        let mut request = http::Request::get("/orders").body(String::new()).unwrap();
        let ctx = CallContext::new()
            .with_deadline(tokio::time::Instant::now().into_std() + Duration::from_secs(1));
        request.extensions_mut().insert(ctx.clone());
        assert_eq!(request.context().deadline(), ctx.deadline());
        tokio::time::advance(Duration::from_secs(2)).await;
        let error = service.oneshot(request).await.unwrap_err();
        assert_eq!(error.code(), ErrorCode::DeadlineExceeded);
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[derive(Debug)]
    struct OneShot;

    impl PolicyRequest for OneShot {
        fn context(&self) -> CallContext {
            CallContext::new()
        }
        fn idempotent(&self) -> bool {
            true
        }
        fn try_clone(&self) -> Option<Self> {
            None
        }
    }

    #[tokio::test(start_paused = true)]
    async fn unreplayable_requests_run_once() {
        let calls = Arc::new(AtomicU32::new(0));
        let counter = Arc::clone(&calls);
        let service = service_fn(move |_: OneShot| {
            counter.fetch_add(1, Ordering::SeqCst);
            async { Err::<(), _>(AppError::unavailable("down")) }
        });
        let error = PolicyLayer::new(policy())
            .layer(service)
            .oneshot(OneShot)
            .await
            .unwrap_err();
        assert_eq!(error.code(), ErrorCode::Unavailable);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn http_requests_clone_completely() {
        let mut request = http::Request::put("/orders/1")
            .header("x-test", "1")
            .body("body".to_owned())
            .unwrap();
        request.extensions_mut().insert(7_u32);
        let copy = request.try_clone().unwrap();
        assert_eq!(copy.method(), http::Method::PUT);
        assert_eq!(copy.uri(), "/orders/1");
        assert_eq!(copy.headers()["x-test"], "1");
        assert_eq!(copy.body(), "body");
        assert_eq!(copy.extensions().get::<u32>(), Some(&7));
        assert!(copy.idempotent());
        assert!(request.context().deadline().is_none());
    }
}
