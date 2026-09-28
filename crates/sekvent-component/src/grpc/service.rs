//! The serving side of gRPC: one generic service per exposed component,
//! routed by its service name, answering through the component's gate and
//! byte-level dispatcher.
//!
//! Everything that decides whether a call runs (authentication, routing,
//! the context, the hop limit, the deadline) is read from the request's
//! HTTP headers before tonic reads the body; request trailers never reach
//! the call context.

use std::convert::Infallible;
use std::sync::Arc;
use std::task::{Context, Poll};

use bytes::Bytes;
use sekvent_context::{CallContext, headers};
use sekvent_error::{AppError, ErrorCode};
use sekvent_link::TokenMap;
use tonic::body::Body;
use tonic::server::Grpc;
use tonic::service::Routes;
use tracing::Instrument as _;

use super::codec::BytesCodec;
use crate::__private::{BoxFuture, Dispatch};
use crate::link::{DOWNSTREAM, depth_exceeded};
use crate::server::{Server, tag};
use crate::wire::{CatchPanic, Panicked};
use crate::{ComponentDescriptor, reasons};

/// The largest request message a served component decodes (4 MiB); a
/// larger one is rejected by tonic with `OUT_OF_RANGE` before it is read.
pub(crate) const MAX_MESSAGE_SIZE: usize = 4 * 1024 * 1024;

/// The original code of a converted downstream failure.
pub(crate) const DOWNSTREAM_CODE: &str = "downstream_code";
/// The original reason of a converted downstream failure, if it had one.
pub(crate) const DOWNSTREAM_REASON: &str = "downstream_reason";

/// One exposed component.
pub(crate) struct Served {
    server: Arc<Server>,
    dispatch: Arc<dyn Dispatch>,
    /// The accepted inbound tokens; `None` with `SERVE_AUTH=none`.
    inbound: Option<Arc<TokenMap>>,
    max_hops: u32,
    /// `/<service>/`, the path prefix of every method.
    prefix: String,
}

impl std::fmt::Debug for Served {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Served")
            .field("component", &self.descriptor().name())
            .field("authenticated", &self.inbound.is_some())
            .field("max_hops", &self.max_hops)
            .finish_non_exhaustive()
    }
}

impl Served {
    pub(crate) fn new(
        server: Arc<Server>,
        dispatch: Arc<dyn Dispatch>,
        inbound: Option<Arc<TokenMap>>,
        max_hops: u32,
    ) -> Self {
        let prefix = format!("/{}/", super::service_name(server.descriptor()));
        Self {
            server,
            dispatch,
            inbound,
            max_hops,
            prefix,
        }
    }

    pub(crate) fn descriptor(&self) -> &'static ComponentDescriptor {
        self.server.descriptor()
    }

    /// The method whose path is exactly `/<service>/<Rpc>`.
    fn method_of(&self, path: &str) -> Option<usize> {
        let rpc = path.strip_prefix(self.prefix.as_str())?;
        if rpc.is_empty() || rpc.contains('/') {
            return None;
        }
        self.descriptor()
            .methods()
            .iter()
            .position(|method| method.rpc() == rpc)
    }

    /// Everything decided from the request headers alone, before the body
    /// is read: authenticate, route, build the context, check the hop
    /// limit and narrow the deadline by the method's timeout.
    fn prepare(
        &self,
        path: &str,
        wire_headers: &http::HeaderMap,
    ) -> Result<(usize, CallContext), AppError> {
        let descriptor = self.descriptor();
        let caller = match &self.inbound {
            Some(inbound) => Some(
                sekvent_link::authenticate_headers(inbound, wire_headers)
                    .ok_or_else(|| AppError::unauthenticated(sekvent_link::REJECTED_MESSAGE))?,
            ),
            None => None,
        };
        let Some(method) = self.method_of(path) else {
            return Err(AppError::unimplemented(format!(
                "component {} has no such method",
                descriptor.name()
            ))
            .with_reason(reasons::UNKNOWN_METHOD)
            .with_metadata("component", descriptor.name()));
        };
        let mut cx = headers::from_headers(wire_headers, caller);
        if cx.hops() > self.max_hops {
            return Err(tag(descriptor, depth_exceeded(self.max_hops), method));
        }
        if let Some(deadline) = self
            .server
            .timeout(method)
            .and_then(|timeout| tokio::time::Instant::now().checked_add(timeout))
        {
            cx = cx.with_deadline(deadline.into_std());
        }
        Ok((method, cx))
    }

    /// Run one prepared call through the gate and answer.
    async fn handle(&self, method: usize, cx: CallContext, body: Bytes) -> Result<Bytes, AppError> {
        let descriptor = self.descriptor();
        let dispatch = Arc::clone(&self.dispatch);
        let outcome = self
            .server
            .run(method, cx, move |cx| {
                // The async block defers `dispatch` itself into the caught
                // poll, so a panic before its future exists is caught too.
                CatchPanic::new(async move { dispatch.dispatch(method, cx, body).await })
            })
            .await?;
        match outcome {
            Ok(Ok(reply)) => Ok(reply),
            Ok(Err(error)) => Err(from_handler(descriptor, method, error)),
            Err(Panicked) => Err(panicked(descriptor, method)),
        }
    }
}

/// The error a served method's own failure is answered with: a transient
/// failure of a component further down (marked with `downstream`) becomes
/// `INTERNAL`/`DOWNSTREAM_FAILURE`, so the caller neither retries it nor
/// counts it against this component; anything else passes unchanged.
pub(crate) fn from_handler(
    descriptor: &'static ComponentDescriptor,
    method: usize,
    error: AppError,
) -> AppError {
    let Some(callee) = error.metadata().get(DOWNSTREAM) else {
        return error;
    };
    if !error.is_transient() {
        return error;
    }
    let mut converted = AppError::new(ErrorCode::Internal, error.message())
        .with_reason(reasons::DOWNSTREAM_FAILURE)
        .with_metadata(DOWNSTREAM, callee.as_str())
        .with_metadata(DOWNSTREAM_CODE, error.code().as_str());
    if let Some(reason) = error.reason() {
        converted = converted.with_metadata(DOWNSTREAM_REASON, reason);
    }
    tag(descriptor, converted, method)
}

/// The error for a method that panicked; the payload is never shown.
fn panicked(descriptor: &'static ComponentDescriptor, method: usize) -> AppError {
    let name = descriptor
        .methods()
        .get(method)
        .map_or("?", |method| method.name());
    let error = AppError::new(
        ErrorCode::Internal,
        format!("component {} method {name} panicked", descriptor.name()),
    )
    .with_reason(reasons::HANDLER_PANICKED);
    tag(descriptor, error, method)
}

/// The gRPC response for `error`, answered without reading the body.
fn error_response(error: &AppError) -> http::Response<Body> {
    sekvent_error::grpc::to_status(error).into_http()
}

/// The tower service mounted at `/<service>/{*rest}`.
#[derive(Debug, Clone)]
pub(crate) struct ComponentService(Arc<Served>);

impl<B> tower::Service<http::Request<B>> for ComponentService
where
    B: http_body::Body<Data = Bytes> + Send + 'static,
    B::Error: Into<Box<dyn std::error::Error + Send + Sync>> + Send,
{
    type Response = http::Response<Body>;
    type Error = Infallible;
    type Future = BoxFuture<'static, Result<Self::Response, Infallible>>;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Infallible>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, request: http::Request<B>) -> Self::Future {
        let served = Arc::clone(&self.0);
        let descriptor = served.descriptor();
        let path = request.uri().path();
        let span = tracing::debug_span!(
            "component_serve",
            component = descriptor.name(),
            method = served
                .method_of(path)
                .and_then(|method| descriptor.methods().get(method))
                .map_or("?", |method| method.name()),
        );
        let prepared = served.prepare(path, request.headers());
        Box::pin(
            async move {
                let (method, cx) = match prepared {
                    Ok(prepared) => prepared,
                    Err(error) => return Ok(error_response(&error)),
                };
                // Cancels the call's token when the client resets the stream
                // and this future is dropped, or when the deadline passes.
                let guard = cx.cancel_token().clone().drop_guard();
                let remaining = sekvent_resilience::remaining(&cx);
                let mut grpc = Grpc::new(BytesCodec).max_decoding_message_size(MAX_MESSAGE_SIZE);
                let call = grpc.unary(Unary { served, method, cx }, request);
                let response = match remaining {
                    Some(remaining) => match tokio::time::timeout(remaining, call).await {
                        Ok(response) => response,
                        Err(_elapsed) => {
                            let error =
                                AppError::deadline_exceeded("the call deadline was exceeded");
                            return Ok(error_response(&tag(descriptor, error, method)));
                        }
                    },
                    None => call.await,
                };
                drop(guard.disarm());
                Ok(response)
            }
            .instrument(span),
        )
    }
}

/// One prepared call, as tonic's unary server sees it; tonic's metadata
/// (headers merged with trailers) is ignored.
struct Unary {
    served: Arc<Served>,
    method: usize,
    cx: CallContext,
}

impl tower::Service<tonic::Request<Bytes>> for Unary {
    type Response = tonic::Response<Bytes>;
    type Error = tonic::Status;
    type Future = BoxFuture<'static, Result<tonic::Response<Bytes>, tonic::Status>>;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), tonic::Status>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, request: tonic::Request<Bytes>) -> Self::Future {
        let served = Arc::clone(&self.served);
        let method = self.method;
        let cx = self.cx.clone();
        let body = request.into_inner();
        Box::pin(async move {
            served
                .handle(method, cx, body)
                .await
                .map(tonic::Response::new)
                .map_err(|error| sekvent_error::grpc::to_status(&error))
        })
    }
}

/// tonic routes serving every component in `served`; unknown paths get
/// tonic's `UNIMPLEMENTED`.
pub(crate) fn routes<'a>(served: impl IntoIterator<Item = &'a Arc<Served>>) -> Routes {
    let mut router = Routes::default().into_axum_router();
    for served in served {
        let path = format!("/{}/{{*rest}}", super::service_name(served.descriptor()));
        router = router.route_service(&path, ComponentService(Arc::clone(served)));
    }
    Routes::from(router)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use sekvent_config::Secret;
    use sekvent_link::InboundLink;

    use super::*;
    use crate::MethodDescriptor;
    use crate::server::MethodPolicy;

    const METHODS: &[MethodDescriptor] = &[
        MethodDescriptor::call("reserve", "Reserve"),
        MethodDescriptor::call("release", "Release"),
    ];
    const INVENTORY: &ComponentDescriptor =
        &ComponentDescriptor::new("inventory", "Inventory", METHODS).with_package("shop.v1");
    const RESERVE: &str = "/shop.v1.Inventory/Reserve";
    const TOKEN: &str = "shop-link-token-0123456789abcdefghijklmn";

    /// Echoes the body; `panic` panics in the future, `panic-now` before
    /// returning it, `fail` fails with a downstream-marked `UNAVAILABLE`.
    struct Echo;

    impl Dispatch for Echo {
        fn dispatch(
            &self,
            _method: usize,
            _cx: CallContext,
            body: Bytes,
        ) -> BoxFuture<'static, Result<Bytes, AppError>> {
            assert!(&body[..] != b"panic-now", "sync panic");
            Box::pin(async move {
                match &body[..] {
                    b"panic" => panic!("secret payload"),
                    b"fail" => Err(AppError::unavailable("pricing is down")
                        .with_reason(reasons::CIRCUIT_OPEN)
                        .with_metadata(DOWNSTREAM, "pricing")),
                    _ => Ok(body),
                }
            })
        }
    }

    fn server(timeout: Option<Duration>) -> Arc<Server> {
        let policy = MethodPolicy {
            timeout,
            ..MethodPolicy::default()
        };
        let server = Arc::new(Server::new(INVENTORY, &[policy.clone(), policy], false));
        assert!(server.open());
        server
    }

    fn served() -> Served {
        Served::new(server(None), Arc::new(Echo), None, 16)
    }

    fn authenticated(timeout: Option<Duration>) -> Served {
        let map = TokenMap::new([InboundLink::untrusted("shop", Secret::new(TOKEN))]).unwrap();
        Served::new(server(timeout), Arc::new(Echo), Some(Arc::new(map)), 1)
    }

    fn bearer(token: &str) -> http::HeaderMap {
        let mut map = http::HeaderMap::new();
        map.insert(
            http::header::AUTHORIZATION,
            format!("Bearer {token}").parse().unwrap(),
        );
        map
    }

    #[test]
    fn methods_are_found_by_their_exact_path() {
        let served = served();
        assert_eq!(served.method_of(RESERVE), Some(0));
        assert_eq!(served.method_of("/shop.v1.Inventory/Release"), Some(1));
        assert_eq!(served.method_of("/shop.v1.Inventory/Stock"), None);
        assert_eq!(served.method_of("/shop.v1.Inventory/a/b/Reserve"), None);
        assert_eq!(served.method_of("/shop.v1.Inventory/Reserve/"), None);
        assert_eq!(served.method_of("/shop.v1.Inventory/"), None);
        assert_eq!(served.method_of("/other.Inventory/Reserve"), None);
        assert_eq!(served.method_of("no-slash"), None);
        let debug = format!("{served:?}");
        assert!(debug.contains("inventory"), "{debug}");
    }

    #[test]
    fn authentication_comes_before_the_method_lookup() {
        let served = authenticated(None);
        let error = served
            .prepare("/shop.v1.Inventory/Stock", &http::HeaderMap::new())
            .unwrap_err();
        assert_eq!(error.code(), ErrorCode::Unauthenticated);
        let error = served
            .prepare("/shop.v1.Inventory/Stock", &bearer(TOKEN))
            .unwrap_err();
        assert_eq!(error.code(), ErrorCode::Unimplemented);
        assert_eq!(error.reason(), Some(reasons::UNKNOWN_METHOD));
        let error = served.prepare(RESERVE, &bearer("wrong")).unwrap_err();
        assert_eq!(error.code(), ErrorCode::Unauthenticated);
        let (method, cx) = served.prepare(RESERVE, &bearer(TOKEN)).unwrap();
        assert_eq!(method, 0);
        assert_eq!(cx.caller().map(|caller| caller.name.as_str()), Some("shop"));
    }

    #[tokio::test]
    async fn the_hop_limit_and_the_method_timeout_apply() {
        let served = authenticated(Some(Duration::from_secs(2)));
        let mut wire = bearer(TOKEN);
        wire.insert(headers::HOPS, "2".parse().unwrap());
        let error = served.prepare(RESERVE, &wire).unwrap_err();
        assert_eq!(error.reason(), Some(reasons::CALL_DEPTH_EXCEEDED));
        assert_eq!(error.metadata()["method"], "reserve");

        let before = std::time::Instant::now();
        let (_, cx) = served.prepare(RESERVE, &bearer(TOKEN)).unwrap();
        let deadline = cx.deadline().unwrap();
        assert!(deadline > before);
        assert!(deadline <= std::time::Instant::now() + Duration::from_secs(2));
    }

    #[tokio::test]
    async fn a_panic_is_internal_without_its_payload() {
        let served = served();
        for body in ["panic", "panic-now"] {
            let error = served
                .handle(0, CallContext::new(), Bytes::from(body))
                .await
                .unwrap_err();
            assert_eq!(error.code(), ErrorCode::Internal);
            assert_eq!(error.reason(), Some(reasons::HANDLER_PANICKED));
            assert_eq!(
                error.message(),
                "component inventory method reserve panicked"
            );
            assert_eq!(error.metadata()["component"], "inventory");
            assert_eq!(error.metadata()["method"], "reserve");
        }
        // The gate is still usable afterwards.
        let reply = served
            .handle(1, CallContext::new(), Bytes::from_static(b"ok"))
            .await
            .unwrap();
        assert_eq!(&reply[..], b"ok");
        assert_eq!(served.server.in_flight(), 0);
        assert_eq!(
            panicked(INVENTORY, 9).message(),
            "component inventory method ? panicked"
        );
    }

    #[tokio::test]
    async fn a_downstream_failure_is_internal_here() {
        let error = served()
            .handle(1, CallContext::new(), Bytes::from_static(b"fail"))
            .await
            .unwrap_err();
        assert_eq!(error.code(), ErrorCode::Internal);
        assert!(!error.is_transient());
        assert_eq!(error.reason(), Some(reasons::DOWNSTREAM_FAILURE));
        assert_eq!(error.message(), "pricing is down");
        assert_eq!(error.metadata()[DOWNSTREAM], "pricing");
        assert_eq!(error.metadata()[DOWNSTREAM_CODE], "UNAVAILABLE");
        assert_eq!(error.metadata()[DOWNSTREAM_REASON], reasons::CIRCUIT_OPEN);
        assert_eq!(error.metadata()["component"], "inventory");
        assert_eq!(error.metadata()["method"], "release");
    }

    #[test]
    fn only_marked_transient_errors_are_converted() {
        let unmarked = AppError::unavailable("down");
        let kept = from_handler(INVENTORY, 0, unmarked);
        assert_eq!(kept.code(), ErrorCode::Unavailable);
        assert!(kept.metadata().is_empty());

        let settled = AppError::not_found("gone").with_metadata(DOWNSTREAM, "pricing");
        let kept = from_handler(INVENTORY, 0, settled);
        assert_eq!(kept.code(), ErrorCode::NotFound);
        assert!(!kept.metadata().contains_key("component"));

        let bare = AppError::deadline_exceeded("slow").with_metadata(DOWNSTREAM, "pricing");
        let converted = from_handler(INVENTORY, 0, bare);
        assert_eq!(converted.code(), ErrorCode::Internal);
        assert_eq!(converted.metadata()[DOWNSTREAM_CODE], "DEADLINE_EXCEEDED");
        assert!(!converted.metadata().contains_key(DOWNSTREAM_REASON));
    }

    #[test]
    fn errors_answer_without_a_body() {
        let response = error_response(&AppError::unauthenticated("no"));
        assert_eq!(response.headers()["grpc-status"], "16");
    }
}
