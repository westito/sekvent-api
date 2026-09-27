//! `x-request-id` handling for HTTP services.
//!
//! [`RequestIdLayer`] makes sure every request carries a usable id: a
//! well-formed incoming `x-request-id` is kept, anything else (missing,
//! empty, too long, non-visible characters) is replaced by a fresh UUID v7.
//! The id is stored in the request extensions as [`RequestId`], echoed on the
//! response unless the handler set its own, and recorded as `request_id` on
//! a `request` span that wraps the inner service.
//!
//! Stacks built from `tower-http` directly can use [`MakeRequestUuidV7`]
//! with `SetRequestIdLayer`, which keeps any incoming value unvalidated.

use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll, ready};

use http::{HeaderName, HeaderValue, Request, Response};
use pin_project_lite::pin_project;
use tower::{Layer, Service};
use tower_http::request_id::MakeRequestId;
pub use tower_http::request_id::RequestId;
use tracing::Instrument;
use tracing::instrument::Instrumented;
use uuid::Uuid;

/// The request-id header.
pub const REQUEST_ID_HEADER: HeaderName = HeaderName::from_static("x-request-id");

/// Longest incoming id that is accepted as is.
pub const MAX_REQUEST_ID_LEN: usize = 128;

/// Whether `value` is acceptable as a request id: 1 to
/// [`MAX_REQUEST_ID_LEN`] visible ASCII characters (no spaces or controls).
pub fn is_valid_request_id(value: &[u8]) -> bool {
    !value.is_empty()
        && value.len() <= MAX_REQUEST_ID_LEN
        && value.iter().all(|byte| (0x21..=0x7e).contains(byte))
}

/// A fresh, time-ordered request id (UUID v7, hyphenated).
pub fn new_request_id() -> HeaderValue {
    let mut buffer = Uuid::encode_buffer();
    let text = Uuid::now_v7().hyphenated().encode_lower(&mut buffer);
    HeaderValue::from_str(text).expect("a hyphenated UUID is visible ASCII")
}

/// The id [`RequestIdLayer`] stored on `request`, if any.
pub fn request_id<B>(request: &Request<B>) -> Option<&str> {
    request
        .extensions()
        .get::<RequestId>()
        .and_then(|id| id.header_value().to_str().ok())
}

/// A `tower-http` [`MakeRequestId`] producing UUID v7 ids.
#[derive(Debug, Clone, Copy, Default)]
pub struct MakeRequestUuidV7;

impl MakeRequestId for MakeRequestUuidV7 {
    fn make_request_id<B>(&mut self, _request: &Request<B>) -> Option<RequestId> {
        Some(RequestId::new(new_request_id()))
    }
}

/// Layer applying [`RequestIdService`].
#[derive(Debug, Clone, Copy, Default)]
pub struct RequestIdLayer {
    _private: (),
}

impl RequestIdLayer {
    /// The layer.
    pub fn new() -> Self {
        Self::default()
    }
}

impl<S> Layer<S> for RequestIdLayer {
    type Service = RequestIdService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        RequestIdService { inner }
    }
}

/// Middleware that ensures, propagates and traces `x-request-id`; see the
/// [module docs](self).
#[derive(Debug, Clone)]
pub struct RequestIdService<S> {
    inner: S,
}

impl<S> RequestIdService<S> {
    /// Wrap `inner`.
    pub fn new(inner: S) -> Self {
        Self { inner }
    }
}

impl<S, ReqBody, ResBody> Service<Request<ReqBody>> for RequestIdService<S>
where
    S: Service<Request<ReqBody>, Response = Response<ResBody>>,
{
    type Response = Response<ResBody>;
    type Error = S::Error;
    type Future = ResponseFuture<S::Future>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, mut request: Request<ReqBody>) -> Self::Future {
        let id = ensure_request_id(&mut request);
        let span = tracing::info_span!("request", request_id = id.to_str().unwrap_or_default());
        request.extensions_mut().insert(RequestId::new(id.clone()));
        let inner = {
            let _entered = span.enter();
            self.inner.call(request)
        };
        ResponseFuture {
            inner: inner.instrument(span),
            id: Some(id),
        }
    }
}

/// Keep a valid incoming id (collapsing repeated headers to one); replace
/// anything else with a fresh id.
fn ensure_request_id<B>(request: &mut Request<B>) -> HeaderValue {
    let id = request
        .headers()
        .get(&REQUEST_ID_HEADER)
        .filter(|value| is_valid_request_id(value.as_bytes()))
        .cloned()
        .unwrap_or_else(new_request_id);
    request.headers_mut().insert(REQUEST_ID_HEADER, id.clone());
    id
}

pin_project! {
    /// Response future of [`RequestIdService`].
    pub struct ResponseFuture<F> {
        #[pin]
        inner: Instrumented<F>,
        id: Option<HeaderValue>,
    }
}

impl<F, B, E> Future for ResponseFuture<F>
where
    F: Future<Output = Result<Response<B>, E>>,
{
    type Output = Result<Response<B>, E>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.project();
        let mut response = ready!(this.inner.poll(cx))?;
        if let Some(id) = this.id.take() {
            response
                .headers_mut()
                .entry(REQUEST_ID_HEADER)
                .or_insert(id);
        }
        Poll::Ready(Ok(response))
    }
}

#[cfg(test)]
mod tests {
    use std::convert::Infallible;

    use tower::ServiceExt;
    use tracing_subscriber::Registry;
    use tracing_subscriber::layer::SubscriberExt;

    use super::*;
    use crate::LogBuffer;

    async fn echo(request: Request<()>) -> Result<Response<String>, Infallible> {
        tracing::info!("handled");
        let seen = request_id(&request).unwrap_or("none").to_owned();
        Ok(Response::new(seen))
    }

    async fn call(request: Request<()>) -> Response<String> {
        RequestIdLayer::new()
            .layer(tower::service_fn(echo))
            .oneshot(request)
            .await
            .expect("infallible")
    }

    fn with_header(value: &[u8]) -> Request<()> {
        let mut request = Request::new(());
        request.headers_mut().insert(
            REQUEST_ID_HEADER,
            HeaderValue::from_bytes(value).expect("valid header bytes"),
        );
        request
    }

    fn header(response: &Response<String>) -> &str {
        response
            .headers()
            .get(&REQUEST_ID_HEADER)
            .and_then(|value| value.to_str().ok())
            .expect("the response carries an id")
    }

    fn is_uuid_v7(text: &str) -> bool {
        Uuid::parse_str(text).is_ok_and(|id| id.get_version_num() == 7)
    }

    #[tokio::test]
    async fn keeps_a_valid_incoming_id() {
        let response = call(with_header(b"abc-123")).await;
        assert_eq!(header(&response), "abc-123");
        assert_eq!(response.body(), "abc-123");
    }

    #[tokio::test]
    async fn generates_an_id_when_missing() {
        let response = call(Request::new(())).await;
        let id = header(&response).to_owned();
        assert!(is_uuid_v7(&id), "{id}");
        assert_eq!(response.body(), &id);
    }

    #[tokio::test]
    async fn replaces_invalid_incoming_ids() {
        let too_long = vec![b'a'; MAX_REQUEST_ID_LEN + 1];
        let invalid: [&[u8]; 5] = [
            b"has space",
            b"tab\there",
            b"caf\xc3\xa9",
            too_long.as_slice(),
            b"",
        ];
        for bad in invalid {
            let response = call(with_header(bad)).await;
            let id = header(&response).to_owned();
            assert!(is_uuid_v7(&id), "{id}");
            assert_eq!(response.body(), &id);
        }
        let longest = vec![b'a'; MAX_REQUEST_ID_LEN];
        let response = call(with_header(&longest)).await;
        assert_eq!(header(&response).len(), MAX_REQUEST_ID_LEN);
    }

    #[tokio::test]
    async fn collapses_repeated_headers() {
        let mut request = with_header(b"first");
        request
            .headers_mut()
            .append(REQUEST_ID_HEADER, HeaderValue::from_static("second"));
        let service = tower::service_fn(|request: Request<()>| async move {
            let count = request.headers().get_all(&REQUEST_ID_HEADER).iter().count();
            Ok::<_, Infallible>(Response::new(count.to_string()))
        });
        let response = RequestIdLayer::new()
            .layer(service)
            .oneshot(request)
            .await
            .expect("infallible");
        assert_eq!(response.body(), "1");
        assert_eq!(header(&response), "first");
    }

    #[tokio::test]
    async fn keeps_an_id_the_handler_set() {
        let service = tower::service_fn(|_request: Request<()>| async {
            let mut response = Response::new(String::new());
            response
                .headers_mut()
                .insert(REQUEST_ID_HEADER, HeaderValue::from_static("from-handler"));
            Ok::<_, Infallible>(response)
        });
        let response = RequestIdService::new(service)
            .oneshot(with_header(b"from-client"))
            .await
            .expect("infallible");
        assert_eq!(header(&response), "from-handler");
    }

    #[tokio::test]
    async fn records_the_id_on_a_span() {
        let buffer = LogBuffer::new(4);
        let subscriber = Registry::default().with(buffer.layer());
        let _default = tracing::subscriber::set_default(subscriber);
        call(with_header(b"trace-me")).await;
        let records = buffer.snapshot();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].message, "handled");
        assert_eq!(records[0].fields["request_id"], "trace-me");
    }

    #[test]
    fn validation_rules() {
        assert!(is_valid_request_id(b"a"));
        assert!(is_valid_request_id(b"!~"));
        assert!(!is_valid_request_id(b""));
        assert!(!is_valid_request_id(b" "));
        assert!(!is_valid_request_id(b"\x7f"));
        assert!(!is_valid_request_id(&[b'x'; MAX_REQUEST_ID_LEN + 1]));
    }

    #[test]
    fn tower_http_maker_and_missing_extension() {
        let request = Request::new(());
        let id = MakeRequestUuidV7
            .make_request_id(&request)
            .expect("always produces an id");
        assert!(is_uuid_v7(id.header_value().to_str().expect("ascii")));
        assert_eq!(request_id(&request), None);
        assert_ne!(new_request_id(), new_request_id());
    }
}
