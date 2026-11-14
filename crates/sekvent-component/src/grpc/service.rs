//! The serving side of gRPC: one generic service per exposed component,
//! routed by its service name, answering through the component's gate and
//! byte-level dispatcher.
//!
//! Everything that decides whether a call runs (authentication, routing,
//! the context, the hop limit, the deadline) is read from the request's
//! HTTP headers before tonic reads the body; request trailers never reach
//! the call context.
//!
//! Callers authenticate as a link (an inbound token), as an end user (the
//! App's end-user authenticator, which sees the request head only), or
//! either, by the component's `SERVE_AUTH`. A link token is checked first;
//! an anonymous method skips the end-user authenticator but never the link
//! check, and an optional-end-user method skips it only for a request
//! without an `Authorization` header.
//!
//! A method's per-client limit (`CLIENT_*`) runs once the route and the
//! context are known: keyed by the client address, before the end-user
//! authenticator, so a flood is refused cheaply; keyed by the end user,
//! right after it. A refused call never reaches the body or the handler.
//!
//! The App's scope resolver runs for every call no link token authenticated,
//! after the address-keyed limit and before the end-user authenticator: its
//! scope goes into the head's extensions (where the authenticator finds it)
//! and into the context. When the scope names a tenant, a user who does not
//! name the same one is refused like a bad credential. The call's
//! cancellation guard exists from the resolver on, and an expired call
//! never reaches it.
//!
//! The App's call authorizer runs last among these head-only checks, after
//! the hop limit, with the context the handler will see; its error is the
//! answer, unchanged, and its panic is answered like a handler's.
//!
//! A streaming method's `TIMEOUT` does not narrow the context: it bounds
//! the open (the end user, the body, the handler's open) together with the
//! caller's deadline. The response headers go out once the stream is open,
//! each item is one gRPC message (at most 4 MiB), and the trailers carry
//! the end. The response carries a [`MessageCount`] for the access log.

use std::convert::Infallible;
use std::sync::Arc;
use std::task::{Context, Poll};

use bytes::Bytes;
use futures::StreamExt as _;
use sekvent_context::{CallContext, ClientAddr, RequestScope, ServiceIdentity, headers};
use sekvent_error::{AppError, ErrorCode};
use sekvent_link::TokenMap;
use sekvent_telemetry::access_log::MessageCount;
use tokio::time::Instant;
use tonic::body::Body;
use tonic::server::Grpc;
use tonic::service::Routes;
use tracing::Instrument as _;

use super::codec::BytesCodec;
use crate::__private::{BoxFuture, Dispatch};
use crate::end_user::{EndUserAuthenticator, rejection};
use crate::link::{DOWNSTREAM, depth_exceeded};
use crate::scope::{ScopeResolver, admit_end_user};
use crate::server::{Server, tag};
use crate::stream::CallStream;
use crate::wire::{CatchPanic, CatchPanicStream, Panicked};
use crate::{Binding, ComponentDescriptor, reasons};

/// The largest request message a served component decodes (4 MiB); a
/// larger one is rejected by tonic with `OUT_OF_RANGE` before it is read.
/// Stream items above it end the stream with `OUT_OF_RANGE`.
pub(crate) const MAX_MESSAGE_SIZE: usize = 4 * 1024 * 1024;

/// The original code of a converted downstream failure.
pub(crate) const DOWNSTREAM_CODE: &str = "downstream_code";
/// The original reason of a converted downstream failure, if it had one.
pub(crate) const DOWNSTREAM_REASON: &str = "downstream_reason";

/// One exposed component.
pub(crate) struct Served {
    server: Arc<Server>,
    dispatch: Arc<dyn Dispatch>,
    /// The accepted inbound tokens; `None` unless `SERVE_AUTH` includes
    /// `link`.
    inbound: Option<Arc<TokenMap>>,
    /// The end-user authenticator; `None` unless `SERVE_AUTH` includes
    /// `bearer`.
    end_user: Option<Arc<dyn EndUserAuthenticator>>,
    /// The App's scope resolver, whatever `SERVE_AUTH` says.
    scope: Option<Arc<dyn ScopeResolver>>,
    max_hops: u32,
    /// `/<service>/`, the path prefix of every method.
    prefix: String,
}

impl std::fmt::Debug for Served {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Served")
            .field("component", &self.descriptor().name())
            .field("authenticated", &self.inbound.is_some())
            .field("end_user", &self.end_user.is_some())
            .field("scope_resolver", &self.scope.is_some())
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
            end_user: None,
            scope: None,
            max_hops,
            prefix,
        }
    }

    /// Authenticate end users with `authenticator` (`SERVE_AUTH` includes
    /// `bearer`).
    pub(crate) fn with_end_user(
        mut self,
        authenticator: Option<Arc<dyn EndUserAuthenticator>>,
    ) -> Self {
        self.end_user = authenticator;
        self
    }

    /// Resolve the request scope of calls no link authenticated with
    /// `resolver`.
    pub(crate) fn with_scope_resolver(mut self, resolver: Option<Arc<dyn ScopeResolver>>) -> Self {
        self.scope = resolver;
        self
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

    /// The link a request's token names, or `None`; under link-only
    /// serving a request without one is refused.
    fn link(&self, wire_headers: &http::HeaderMap) -> Result<Option<ServiceIdentity>, AppError> {
        let Some(inbound) = &self.inbound else {
            return Ok(None);
        };
        match sekvent_link::authenticate_headers(inbound, wire_headers) {
            Some(caller) => Ok(Some(caller)),
            None if self.end_user.is_some() => Ok(None),
            None => Err(AppError::unauthenticated(sekvent_link::REJECTED_MESSAGE)),
        }
    }

    /// Everything decided from the request head alone, before the body is
    /// read and before an end user is authenticated: the link, the route,
    /// the context (with the transport's client address unless a trusted
    /// link named its visitor) and its deadline (the method's timeout
    /// narrows it, or bounds the open of a stream), and whether the
    /// end-user authenticator must run.
    fn prepare(
        &self,
        path: &str,
        wire_headers: &http::HeaderMap,
        client_addr: Option<ClientAddr>,
    ) -> Result<Prepared, AppError> {
        let descriptor = self.descriptor();
        let caller = self.link(wire_headers)?;
        let Some(method) = self.method_of(path) else {
            return Err(AppError::unimplemented(format!(
                "component {} has no such method",
                descriptor.name()
            ))
            .with_reason(reasons::UNKNOWN_METHOD)
            .with_metadata("component", descriptor.name()));
        };
        let declared = &descriptor.methods()[method];
        let end_user = caller.is_none()
            && self.end_user.is_some()
            && !declared.is_anonymous()
            && (!declared.is_optional_end_user()
                || wire_headers.contains_key(http::header::AUTHORIZATION));
        let mut cx = headers::from_headers(wire_headers, caller);
        if let (None, Some(ClientAddr(addr))) = (cx.client_addr(), client_addr) {
            cx = cx.with_client_addr(addr);
        }
        let timeout = self
            .server
            .timeout(method)
            .and_then(|timeout| Instant::now().checked_add(timeout));
        let mut open_deadline = None;
        match timeout {
            Some(deadline) if declared.is_stream() => open_deadline = Some(deadline),
            Some(deadline) => cx = cx.with_deadline(deadline.into_std()),
            None => {}
        }
        Ok(Prepared {
            method,
            cx,
            end_user,
            open_deadline,
        })
    }

    /// Run the scope resolver on the request head within the call's
    /// deadline (and a stream's `open_deadline`), never once it has passed,
    /// check the scope's tenant, put the scope into the head's extensions
    /// and attach it to the context. Without a resolver the context is
    /// returned unchanged; the resolver's error is returned unchanged.
    async fn resolve_scope(
        &self,
        method: usize,
        cx: CallContext,
        head: &mut http::request::Parts,
        open_deadline: Option<Instant>,
    ) -> Result<CallContext, AppError> {
        let Some(resolver) = &self.scope else {
            return Ok(cx);
        };
        let bound = open_bound(&cx, open_deadline);
        if bound.is_some_and(|bound| bound <= Instant::now()) {
            return Err(deadline_exceeded(self.descriptor(), method));
        }
        let request: &http::request::Parts = head;
        let context = &cx;
        // Deferred into the caught poll, so a sync closure that panics is
        // caught too.
        let attempt = CatchPanic::new(async move { resolver.resolve(request, context).await });
        let outcome = match bound {
            Some(bound) => tokio::time::timeout_at(bound, attempt)
                .await
                .map_err(|_elapsed| deadline_exceeded(self.descriptor(), method))?,
            None => attempt.await,
        };
        match outcome {
            Ok(Ok(scope)) => {
                let scope = crate::scope::checked(scope)
                    .map_err(|error| tag(self.descriptor(), error, method))?;
                head.extensions.insert::<RequestScope>(scope.clone());
                Ok(cx.with_scope(scope))
            }
            Ok(Err(error)) => Err(error),
            Err(Panicked) => Err(tag(
                self.descriptor(),
                AppError::new(
                    ErrorCode::Internal,
                    format!(
                        "the scope resolver of component {} panicked",
                        self.descriptor().name()
                    ),
                )
                .with_reason(reasons::HANDLER_PANICKED),
                method,
            )),
        }
    }

    /// Run the end-user authenticator on the request head within the
    /// call's deadline (and a stream's `open_deadline`), and make the end
    /// user the context's caller, refusing one who does not name the
    /// scope's tenant.
    async fn authenticate(
        &self,
        method: usize,
        cx: CallContext,
        head: &http::request::Parts,
        open_deadline: Option<Instant>,
    ) -> Result<CallContext, AppError> {
        let Some(authenticator) = &self.end_user else {
            return Ok(cx);
        };
        // Deferred into the caught poll, so a sync closure that panics is
        // caught too.
        let attempt = CatchPanic::new(async move { authenticator.authenticate(head).await });
        let outcome = match open_bound(&cx, open_deadline) {
            Some(bound) => tokio::time::timeout_at(bound, attempt)
                .await
                .map_err(|_elapsed| deadline_exceeded(self.descriptor(), method))?,
            None => attempt.await,
        };
        match outcome {
            Ok(Ok(user)) => admit_end_user(cx, user),
            Ok(Err(error)) => Err(rejection(error)),
            Err(Panicked) => Err(tag(
                self.descriptor(),
                AppError::new(
                    ErrorCode::Internal,
                    format!(
                        "the end-user authenticator of component {} panicked",
                        self.descriptor().name()
                    ),
                )
                .with_reason(reasons::HANDLER_PANICKED),
                method,
            )),
        }
    }

    /// Take a permit from the method's per-client limiter, if it has one
    /// whose key is (`after_end_user`) or is not known only after
    /// end-user authentication.
    fn check_client(
        &self,
        method: usize,
        cx: &CallContext,
        after_end_user: bool,
    ) -> Result<(), AppError> {
        match self.server.client_limiter(method) {
            Some(limiter) if limiter.key().needs_end_user() == after_end_user => limiter
                .check(cx)
                .map_err(|error| tag(self.descriptor(), error, method)),
            _ => Ok(()),
        }
    }

    /// The hop limit, checked once the caller is known.
    fn check_hops(&self, method: usize, cx: &CallContext) -> Result<(), AppError> {
        if cx.hops() > self.max_hops {
            return Err(tag(
                self.descriptor(),
                depth_exceeded(self.max_hops),
                method,
            ));
        }
        Ok(())
    }

    /// The App's call authorizer, with its panic caught; its error is
    /// returned unchanged.
    fn check_authorized(&self, method: usize, cx: &CallContext) -> Result<(), AppError> {
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.server.authorize(method, cx)
        }))
        .unwrap_or_else(|_payload| {
            Err(tag(
                self.descriptor(),
                AppError::new(
                    ErrorCode::Internal,
                    format!(
                        "the call authorizer of component {} panicked",
                        self.descriptor().name()
                    ),
                )
                .with_reason(reasons::HANDLER_PANICKED),
                method,
            ))
        })
    }

    /// Open one prepared stream through the gate: the open within the open
    /// bound, a panic of the open or of the producer caught, an open error
    /// passing [`from_handler`]. Item errors cross untouched.
    async fn open(
        &self,
        method: usize,
        cx: CallContext,
        open_deadline: Option<Instant>,
        body: Bytes,
    ) -> Result<CallStream<Bytes, AppError>, AppError> {
        let descriptor = self.descriptor();
        let dispatch = Arc::clone(&self.dispatch);
        self.server
            .open_stream(
                method,
                cx,
                open_deadline,
                Binding::Grpc.as_str(),
                move |cx| async move {
                    // As for unary calls, a panic before the future exists
                    // is caught too.
                    let opened =
                        CatchPanic::new(
                            async move { dispatch.dispatch_stream(method, cx, body).await },
                        )
                        .await;
                    match opened {
                        Ok(Ok(stream)) => Ok(CallStream::new(CatchPanicStream::new(stream).map(
                            move |item| match item {
                                Ok(item) => item,
                                Err(Panicked) => Err(panicked(descriptor, method)),
                            },
                        ))),
                        Ok(Err(error)) => Err(from_handler(descriptor, method, error)),
                        Err(Panicked) => Err(panicked(descriptor, method)),
                    }
                },
            )
            .await
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

/// What [`Served::prepare`] decided from the headers.
#[derive(Debug)]
struct Prepared {
    method: usize,
    cx: CallContext,
    /// Whether the end-user authenticator must run.
    end_user: bool,
    /// For a streaming method, the end of its `TIMEOUT`, which bounds the
    /// open only.
    open_deadline: Option<Instant>,
}

/// The earlier of the context's deadline and `open_deadline`, on tokio's
/// clock.
fn open_bound(cx: &CallContext, open_deadline: Option<Instant>) -> Option<Instant> {
    let deadline = cx.deadline().map(Instant::from_std);
    match (deadline, open_deadline) {
        (Some(deadline), Some(open)) => Some(deadline.min(open)),
        (deadline, open) => deadline.or(open),
    }
}

/// The call's deadline passed while it was being served.
fn deadline_exceeded(descriptor: &'static ComponentDescriptor, method: usize) -> AppError {
    tag(
        descriptor,
        AppError::deadline_exceeded("the call deadline was exceeded"),
        method,
    )
}

/// The gRPC response for `error`, answered without reading the body; a
/// server-side failure is logged once on the way.
fn error_response(error: AppError) -> http::Response<Body> {
    tonic::Status::from(error).into_http()
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
        let prepared = served.prepare(
            path,
            request.headers(),
            request.extensions().get::<ClientAddr>().copied(),
        );
        Box::pin(
            async move {
                let Prepared {
                    method,
                    mut cx,
                    end_user,
                    open_deadline,
                } = match prepared {
                    Ok(prepared) => prepared,
                    Err(error) => return Ok(error_response(error)),
                };
                if let Err(error) = served.check_client(method, &cx, false) {
                    return Ok(error_response(error));
                }
                // Cancels the call's token when the client resets the stream
                // and this future is dropped, or when the deadline passes,
                // from the scope resolver on: work it spawned with the token
                // ends with the call.
                let guard = cx.cancel_token().clone().drop_guard();
                let (mut head, body) = request.into_parts();
                if cx.caller().is_none() {
                    cx = match served
                        .resolve_scope(method, cx, &mut head, open_deadline)
                        .await
                    {
                        Ok(cx) => cx,
                        Err(error) => return Ok(error_response(error)),
                    };
                }
                if end_user {
                    cx = match served.authenticate(method, cx, &head, open_deadline).await {
                        Ok(cx) => cx,
                        Err(error) => return Ok(error_response(error)),
                    };
                }
                if let Err(error) = served
                    .check_client(method, &cx, true)
                    .and_then(|()| served.check_hops(method, &cx))
                    .and_then(|()| served.check_authorized(method, &cx))
                {
                    return Ok(error_response(error));
                }
                let request = http::Request::from_parts(head, body);
                if descriptor.methods()[method].is_stream() {
                    return Ok(
                        serve_stream(served, method, cx, open_deadline, request, guard).await,
                    );
                }
                let remaining = sekvent_resilience::remaining(&cx);
                let mut grpc = Grpc::new(BytesCodec).max_decoding_message_size(MAX_MESSAGE_SIZE);
                let call = grpc.unary(Unary { served, method, cx }, request);
                let response = match remaining {
                    Some(remaining) => match tokio::time::timeout(remaining, call).await {
                        Ok(response) => response,
                        Err(_elapsed) => {
                            return Ok(error_response(deadline_exceeded(descriptor, method)));
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

/// Serve one prepared stream: open it within the open bound (a trailers-only
/// `DEADLINE_EXCEEDED` when it passes), then hand its items to tonic as
/// they come, counted in the response's [`MessageCount`]. `guard`, the
/// call's drop guard, cancels the call's token if the response future is
/// dropped during the open or the open bound passes; after the open, the
/// stream's own guard does when hyper drops the body.
async fn serve_stream<B>(
    served: Arc<Served>,
    method: usize,
    cx: CallContext,
    open_deadline: Option<Instant>,
    request: http::Request<B>,
    guard: tokio_util::sync::DropGuard,
) -> http::Response<Body>
where
    B: http_body::Body<Data = Bytes> + Send + 'static,
    B::Error: Into<Box<dyn std::error::Error + Send + Sync>> + Send,
{
    let descriptor = served.descriptor();
    let bound = open_bound(&cx, open_deadline);
    let count = MessageCount::new();
    let mut grpc = Grpc::new(BytesCodec)
        .max_decoding_message_size(MAX_MESSAGE_SIZE)
        .max_encoding_message_size(MAX_MESSAGE_SIZE);
    let call = grpc.server_streaming(
        StreamOpen {
            served,
            method,
            cx,
            open_deadline,
            count: count.clone(),
        },
        request,
    );
    let mut response = match bound {
        Some(bound) => match tokio::time::timeout_at(bound, call).await {
            Ok(response) => response,
            Err(_elapsed) => return error_response(deadline_exceeded(descriptor, method)),
        },
        None => call.await,
    };
    drop(guard.disarm());
    // A trailers-only answer is a failed open: no stream, nothing to count.
    if !response.headers().contains_key("grpc-status") {
        response.extensions_mut().insert(count);
    }
    response
}

/// One prepared stream, as tonic's server-streaming server sees it.
struct StreamOpen {
    served: Arc<Served>,
    method: usize,
    cx: CallContext,
    open_deadline: Option<Instant>,
    count: MessageCount,
}

impl tower::Service<tonic::Request<Bytes>> for StreamOpen {
    type Response = tonic::Response<CallStream<Bytes, tonic::Status>>;
    type Error = tonic::Status;
    type Future = BoxFuture<'static, Result<Self::Response, tonic::Status>>;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), tonic::Status>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, request: tonic::Request<Bytes>) -> Self::Future {
        let served = Arc::clone(&self.served);
        let method = self.method;
        let cx = self.cx.clone();
        let open_deadline = self.open_deadline;
        let count = self.count.clone();
        let body = request.into_inner();
        Box::pin(async move {
            let stream = served
                .open(method, cx, open_deadline, body)
                .await
                .map_err(tonic::Status::from)?;
            Ok(tonic::Response::new(CallStream::new(stream.map(
                move |item| {
                    if item.is_ok() {
                        count.increment();
                    }
                    item.map_err(tonic::Status::from)
                },
            ))))
        })
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
                .map_err(tonic::Status::from)
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
    use sekvent_context::EndUser;
    use sekvent_link::InboundLink;

    use super::*;
    use crate::server::MethodPolicy;
    use crate::{END_USER_REJECTED_MESSAGE, MethodDescriptor};

    const METHODS: &[MethodDescriptor] = &[
        MethodDescriptor::call("reserve", "Reserve"),
        MethodDescriptor::call("release", "Release").with_anonymous(),
        MethodDescriptor::call("track", "Track").with_optional_end_user(),
    ];
    const INVENTORY: &ComponentDescriptor =
        &ComponentDescriptor::new("inventory", "Inventory", METHODS).with_package("shop.v1");
    const RESERVE: &str = "/shop.v1.Inventory/Reserve";
    const TRACK: &str = "/shop.v1.Inventory/Track";
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
        let server = Arc::new(Server::new(
            INVENTORY,
            &[policy.clone(), policy.clone(), policy],
            false,
        ));
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
            .prepare("/shop.v1.Inventory/Stock", &http::HeaderMap::new(), None)
            .unwrap_err();
        assert_eq!(error.code(), ErrorCode::Unauthenticated);
        let error = served
            .prepare("/shop.v1.Inventory/Stock", &bearer(TOKEN), None)
            .unwrap_err();
        assert_eq!(error.code(), ErrorCode::Unimplemented);
        assert_eq!(error.reason(), Some(reasons::UNKNOWN_METHOD));
        let error = served.prepare(RESERVE, &bearer("wrong"), None).unwrap_err();
        assert_eq!(error.code(), ErrorCode::Unauthenticated);
        // Anonymous methods still need the link under link-only serving.
        let error = served
            .prepare("/shop.v1.Inventory/Release", &http::HeaderMap::new(), None)
            .unwrap_err();
        assert_eq!(error.message(), sekvent_link::REJECTED_MESSAGE);
        // So do optional-end-user methods, with or without a credential.
        for wire in [http::HeaderMap::new(), bearer("user-1")] {
            let error = served.prepare(TRACK, &wire, None).unwrap_err();
            assert_eq!(error.message(), sekvent_link::REJECTED_MESSAGE);
        }
        let prepared = served.prepare(TRACK, &bearer(TOKEN), None).unwrap();
        assert!(!prepared.end_user);
        let prepared = served.prepare(RESERVE, &bearer(TOKEN), None).unwrap();
        assert_eq!(prepared.method, 0);
        assert!(!prepared.end_user);
        assert_eq!(
            prepared.cx.caller().map(|caller| caller.name.as_str()),
            Some("shop")
        );
    }

    #[tokio::test]
    async fn the_hop_limit_and_the_method_timeout_apply() {
        let served = authenticated(Some(Duration::from_secs(2)));
        let mut wire = bearer(TOKEN);
        wire.insert(headers::HOPS, "2".parse().unwrap());
        let prepared = served.prepare(RESERVE, &wire, None).unwrap();
        let error = served
            .check_hops(prepared.method, &prepared.cx)
            .unwrap_err();
        assert_eq!(error.reason(), Some(reasons::CALL_DEPTH_EXCEEDED));
        assert_eq!(error.metadata()["method"], "reserve");

        let before = std::time::Instant::now();
        let prepared = served.prepare(RESERVE, &bearer(TOKEN), None).unwrap();
        served.check_hops(prepared.method, &prepared.cx).unwrap();
        let cx = prepared.cx;
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

    /// Accepts `Bearer user-<n>` (tenant `t-1`), `Bearer anyone` (no
    /// tenant) and `Bearer probe` (subject `saw-<the scope's tenant>` and
    /// that tenant, as found in the head's extensions), fails `UNAVAILABLE` for
    /// `Bearer down`, rejects `Bearer ended` with [`SESSION_ENDED`], panics
    /// for `Bearer panic`, never answers for `Bearer hang`, and rejects
    /// anything else with a detailed message.
    struct Sessions;

    impl EndUserAuthenticator for Sessions {
        fn authenticate<'a>(
            &'a self,
            request: &'a http::request::Parts,
        ) -> BoxFuture<'a, Result<EndUser, AppError>> {
            let token = request
                .headers
                .get(http::header::AUTHORIZATION)
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.strip_prefix("Bearer "))
                .map(str::to_owned);
            assert!(token.as_deref() != Some("panic-now"), "sync panic");
            let scoped = request
                .extensions
                .get::<RequestScope>()
                .map(|scope| scope.tenant().map(str::to_owned));
            Box::pin(async move {
                match token.as_deref() {
                    Some("down") => Err(AppError::unavailable("sessions are down")),
                    Some("ended") => Err(AppError::from(SESSION_ENDED)
                        .with_metadata("session", "7f3a")
                        .with_reason("SESSION_7F3A")),
                    Some("anyone") => Ok(EndUser::new("anyone")),
                    Some("probe") => Ok(match scoped {
                        None => EndUser::new("saw-nothing"),
                        Some(None) => EndUser::new("saw-none"),
                        Some(Some(tenant)) => {
                            EndUser::new(format!("saw-{tenant}")).with_tenant(tenant)
                        }
                    }),
                    Some("panic") => panic!("secret token"),
                    Some("hang") => std::future::pending().await,
                    Some(user) if user.starts_with("user-") => Ok(EndUser::new(user)
                        .with_tenant("t-1")
                        .with_roles(["buyer"])
                        .with_extension(SessionScope("orders:read"))),
                    _ => Err(AppError::unauthenticated("token signature mismatch")),
                }
            })
        }
    }

    /// What `Sessions` learned about a session besides its user.
    #[derive(Debug, Clone, PartialEq)]
    struct SessionScope(&'static str);

    fn end_user_served(link: bool, timeout: Option<Duration>) -> Served {
        let inbound = link.then(|| {
            Arc::new(TokenMap::new([InboundLink::trusted("shop", Secret::new(TOKEN))]).unwrap())
        });
        let sessions: Arc<dyn EndUserAuthenticator> = Arc::new(Sessions);
        Served::new(server(timeout), Arc::new(Echo), inbound, 16).with_end_user(Some(sessions))
    }

    fn head(wire: http::HeaderMap) -> http::request::Parts {
        let mut request = http::Request::new(());
        *request.headers_mut() = wire;
        request.into_parts().0
    }

    /// The head of a request to `path`.
    fn head_at(path: &str, wire: http::HeaderMap) -> http::request::Parts {
        let mut request = http::Request::new(());
        *request.uri_mut() = path.parse().unwrap();
        *request.headers_mut() = wire;
        request.into_parts().0
    }

    /// `prepare`, the scope when no link authenticated the call and, when
    /// `prepare` asks for one, the end user: the serving order up to the
    /// end user.
    async fn admit(
        served: &Served,
        path: &str,
        wire: http::HeaderMap,
    ) -> Result<Prepared, AppError> {
        let mut prepared = served.prepare(path, &wire, None)?;
        let mut head = head_at(path, wire);
        if prepared.cx.caller().is_none() {
            prepared.cx = served
                .resolve_scope(
                    prepared.method,
                    prepared.cx,
                    &mut head,
                    prepared.open_deadline,
                )
                .await?;
        }
        if prepared.end_user {
            prepared.cx = served
                .authenticate(prepared.method, prepared.cx, &head, prepared.open_deadline)
                .await?;
        }
        Ok(prepared)
    }

    #[tokio::test]
    async fn end_users_are_authenticated_from_the_head() {
        let served = end_user_served(false, None);
        let mut wire = bearer("user-7");
        wire.insert(headers::SUBJECT, "someone-else".parse().unwrap());
        let prepared = admit(&served, RESERVE, wire).await.unwrap();
        let user = prepared.cx.end_user().unwrap();
        assert_eq!(user.subject(), "user-7");
        assert!(user.has_role("buyer"));
        assert_eq!(
            user.extension::<SessionScope>(),
            Some(&SessionScope("orders:read")),
            "the authenticator's extensions reach the handler"
        );
        assert_eq!(prepared.cx.subject(), Some("user-7"), "never the header's");
        assert_eq!(prepared.cx.tenant(), Some("t-1"));
        assert_eq!(prepared.cx.caller(), None);

        // Missing and wrong tokens get one answer.
        let missing = admit(&served, RESERVE, http::HeaderMap::new())
            .await
            .unwrap_err();
        let wrong = admit(&served, RESERVE, bearer("forged")).await.unwrap_err();
        for error in [&missing, &wrong] {
            assert_eq!(error.code(), ErrorCode::Unauthenticated);
            assert_eq!(error.message(), END_USER_REJECTED_MESSAGE);
            assert_eq!(error.reason(), None);
            assert!(error.metadata().is_empty());
        }

        // Other failures pass; panics are internal without their payload.
        let error = admit(&served, RESERVE, bearer("down")).await.unwrap_err();
        assert_eq!(error.code(), ErrorCode::Unavailable);
        for token in ["panic", "panic-now"] {
            let error = admit(&served, RESERVE, bearer(token)).await.unwrap_err();
            assert_eq!(error.code(), ErrorCode::Internal);
            assert_eq!(error.reason(), Some(reasons::HANDLER_PANICKED));
            assert!(!error.message().contains("secret"), "{}", error.message());
            assert_eq!(error.metadata()["method"], "reserve");
        }

        // Routing comes before the authenticator; anonymous methods skip it.
        let error = admit(&served, "/shop.v1.Inventory/Stock", http::HeaderMap::new())
            .await
            .unwrap_err();
        assert_eq!(error.reason(), Some(reasons::UNKNOWN_METHOD));
        let prepared = admit(&served, "/shop.v1.Inventory/Release", bearer("forged"))
            .await
            .unwrap();
        assert!(!prepared.end_user);
        assert_eq!(prepared.cx.end_user(), None);
        assert_eq!(prepared.cx.caller(), None);
        assert!(format!("{served:?}").contains("end_user: true"));
    }

    #[tokio::test]
    async fn optional_end_users_are_authenticated_only_when_they_send_a_credential() {
        let served = end_user_served(false, None);
        // No header: served without identity, the authenticator not run.
        let mut wire = http::HeaderMap::new();
        wire.insert(headers::SUBJECT, "someone-else".parse().unwrap());
        let prepared = admit(&served, TRACK, wire).await.unwrap();
        assert!(!prepared.end_user);
        assert_eq!(prepared.cx.end_user(), None);
        assert_eq!(prepared.cx.subject(), None, "never the header's");
        assert_eq!(prepared.cx.caller(), None);

        // A header: authenticated exactly as for any other method.
        let prepared = admit(&served, TRACK, bearer("user-4")).await.unwrap();
        assert!(prepared.end_user);
        assert_eq!(prepared.cx.end_user().map(EndUser::subject), Some("user-4"));
        assert_eq!(prepared.cx.subject(), Some("user-4"));
        assert_eq!(prepared.cx.tenant(), Some("t-1"));

        // A rejected credential is refused, never downgraded to no end user,
        // and an empty header still counts as one.
        let mut empty = http::HeaderMap::new();
        empty.insert(http::header::AUTHORIZATION, "".parse().unwrap());
        for wire in [bearer("forged"), empty] {
            let error = admit(&served, TRACK, wire).await.unwrap_err();
            assert_eq!(error.code(), ErrorCode::Unauthenticated);
            assert_eq!(error.message(), END_USER_REJECTED_MESSAGE);
        }
        // Other failures pass through unchanged.
        let error = admit(&served, TRACK, bearer("down")).await.unwrap_err();
        assert_eq!(error.code(), ErrorCode::Unavailable);
    }

    #[tokio::test(start_paused = true)]
    async fn an_optional_end_user_is_authenticated_within_the_deadline() {
        let served = end_user_served(false, Some(Duration::from_millis(50)));
        let error = admit(&served, TRACK, bearer("hang")).await.unwrap_err();
        assert_eq!(error.code(), ErrorCode::DeadlineExceeded);
        assert_eq!(error.metadata()["method"], "track");
    }

    #[tokio::test]
    async fn links_come_first_when_both_are_served() {
        let served = end_user_served(true, None);
        let mut wire = bearer(TOKEN);
        wire.insert(headers::SUBJECT, "user-3".parse().unwrap());
        let prepared = admit(&served, RESERVE, wire).await.unwrap();
        assert!(!prepared.end_user);
        assert_eq!(
            prepared.cx.caller(),
            Some(&ServiceIdentity::trusted("shop"))
        );
        assert_eq!(
            prepared.cx.subject(),
            Some("user-3"),
            "asserted by a trusted link"
        );
        assert_eq!(prepared.cx.end_user(), None);

        let prepared = admit(&served, RESERVE, bearer("user-9")).await.unwrap();
        assert_eq!(prepared.cx.end_user().map(EndUser::subject), Some("user-9"));

        let error = admit(&served, RESERVE, bearer("forged")).await.unwrap_err();
        assert_eq!(error.message(), END_USER_REJECTED_MESSAGE);

        // An anonymous method keeps a link caller's identity.
        let prepared = admit(&served, "/shop.v1.Inventory/Release", bearer(TOKEN))
            .await
            .unwrap();
        assert_eq!(
            prepared.cx.caller(),
            Some(&ServiceIdentity::trusted("shop"))
        );

        // An optional-end-user method: the link, an end user, or nobody.
        let prepared = admit(&served, TRACK, bearer(TOKEN)).await.unwrap();
        assert!(!prepared.end_user);
        assert_eq!(
            prepared.cx.caller(),
            Some(&ServiceIdentity::trusted("shop"))
        );
        let prepared = admit(&served, TRACK, bearer("user-2")).await.unwrap();
        assert_eq!(prepared.cx.end_user().map(EndUser::subject), Some("user-2"));
        let prepared = admit(&served, TRACK, http::HeaderMap::new()).await.unwrap();
        assert_eq!(prepared.cx.end_user(), None);
        assert_eq!(prepared.cx.caller(), None);
        let error = admit(&served, TRACK, bearer("forged")).await.unwrap_err();
        assert_eq!(error.message(), END_USER_REJECTED_MESSAGE);
    }

    #[tokio::test(start_paused = true)]
    async fn the_authenticator_runs_within_the_deadline() {
        let served = end_user_served(false, Some(Duration::from_millis(50)));
        let error = admit(&served, RESERVE, bearer("hang")).await.unwrap_err();
        assert_eq!(error.code(), ErrorCode::DeadlineExceeded);
        assert_eq!(error.metadata()["component"], "inventory");

        let unbounded = end_user_served(false, None);
        let prepared = unbounded.prepare(RESERVE, &bearer("user-1"), None).unwrap();
        let cx = served
            .authenticate(prepared.method, prepared.cx, &head(bearer("user-1")), None)
            .await
            .unwrap();
        assert!(cx.end_user().is_some());
    }

    #[tokio::test]
    async fn without_an_authenticator_the_context_is_unchanged() {
        let served = served();
        let cx = served
            .authenticate(0, CallContext::new(), &head(bearer("user-1")), None)
            .await
            .unwrap();
        assert_eq!(cx.end_user(), None);
    }

    /// What the authorizer saw of one call.
    #[derive(Debug, Clone, PartialEq)]
    struct Asked {
        method: &'static str,
        caller: Option<String>,
        end_user: Option<String>,
        buyer: bool,
        subject: Option<String>,
    }

    type Log = Arc<std::sync::Mutex<Vec<Asked>>>;

    /// Records every call; refuses `reserve` for end users with
    /// `PERMISSION_DENIED`, panics for the subject `panic`, allows the rest.
    fn recording() -> (Arc<dyn crate::CallAuthorizer>, Log) {
        let log = Log::default();
        let seen = Arc::clone(&log);
        let authorizer = move |target: crate::CallTarget, cx: &CallContext| {
            assert!(cx.subject() != Some("user-panic"), "secret policy");
            seen.lock().unwrap().push(Asked {
                method: target.method().name(),
                caller: cx.caller().map(|caller| caller.name.clone()),
                end_user: cx.end_user().map(|user| user.subject().to_owned()),
                buyer: cx.end_user().is_some_and(|user| user.has_role("buyer")),
                subject: cx.subject().map(str::to_owned),
            });
            if target.method().name() == "reserve" && cx.end_user().is_some() {
                return Err(AppError::permission_denied("buyers may not reserve")
                    .with_reason("ROLE_MISSING")
                    .with_metadata("role", "admin"));
            }
            Ok(())
        };
        (Arc::new(authorizer), log)
    }

    fn guarded(link: bool, max_hops: u32, authorizer: Arc<dyn crate::CallAuthorizer>) -> Served {
        let inbound = link.then(|| {
            Arc::new(TokenMap::new([InboundLink::trusted("shop", Secret::new(TOKEN))]).unwrap())
        });
        let gate = Server::new(
            INVENTORY,
            &[
                MethodPolicy::default(),
                MethodPolicy::default(),
                MethodPolicy::default(),
            ],
            false,
        )
        .with_authorizer(Some(authorizer));
        assert!(gate.open());
        let sessions: Arc<dyn EndUserAuthenticator> = Arc::new(Sessions);
        Served::new(Arc::new(gate), Arc::new(Echo), inbound, max_hops).with_end_user(Some(sessions))
    }

    /// A request body that never yields a frame.
    struct NeverArrives;

    impl http_body::Body for NeverArrives {
        type Data = Bytes;
        type Error = Infallible;

        fn poll_frame(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut Context<'_>,
        ) -> Poll<Option<Result<http_body::Frame<Bytes>, Infallible>>> {
            Poll::Pending
        }
    }

    /// The `grpc-status` `served` answers a request to `path` with `wire`
    /// headers and a body that never arrives.
    async fn status_of(served: &Arc<Served>, path: &str, wire: http::HeaderMap) -> String {
        let mut request = http::Request::new(NeverArrives);
        *request.uri_mut() = path.parse().unwrap();
        *request.headers_mut() = wire;
        let call = tower::Service::call(&mut ComponentService(Arc::clone(served)), request);
        let response = tokio::time::timeout(Duration::from_secs(30), call)
            .await
            .expect("answered without the body")
            .unwrap();
        response.headers()["grpc-status"]
            .to_str()
            .unwrap()
            .to_owned()
    }

    #[tokio::test]
    async fn the_call_authorizer_runs_after_authentication_and_before_the_body() {
        let (authorizer, log) = recording();
        let served = Arc::new(guarded(true, 1, authorizer));

        // An end user refused by role: answered before the body arrives.
        assert_eq!(status_of(&served, RESERVE, bearer("user-7")).await, "7");
        assert_eq!(
            log.lock().unwrap().pop(),
            Some(Asked {
                method: "reserve",
                caller: None,
                end_user: Some("user-7".into()),
                buyer: true,
                subject: Some("user-7".into()),
            })
        );

        // A rejected end user, an unknown path and the hop limit never ask.
        assert_eq!(status_of(&served, RESERVE, bearer("forged")).await, "16");
        assert_eq!(
            status_of(&served, "/shop.v1.Inventory/Stock", bearer("user-7")).await,
            "12"
        );
        let mut deep = bearer(TOKEN);
        deep.insert(headers::HOPS, "2".parse().unwrap());
        assert_eq!(status_of(&served, RESERVE, deep).await, "9");
        assert!(log.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn the_call_authorizer_sees_anonymous_optional_and_link_callers() {
        let (authorizer, log) = recording();
        let served = guarded(true, 16, authorizer);
        let decide = |path: &str, wire: http::HeaderMap| {
            let served = &served;
            let path = path.to_owned();
            async move {
                let prepared = admit(served, &path, wire).await?;
                served.check_authorized(prepared.method, &prepared.cx)
            }
        };

        decide("/shop.v1.Inventory/Release", http::HeaderMap::new())
            .await
            .unwrap();
        decide(TRACK, http::HeaderMap::new()).await.unwrap();
        let mut link = bearer(TOKEN);
        link.insert(headers::SUBJECT, "user-3".parse().unwrap());
        decide(RESERVE, link).await.unwrap();
        let denied = decide(RESERVE, bearer("user-1")).await.unwrap_err();
        assert_eq!(denied.code(), ErrorCode::PermissionDenied);
        assert_eq!(denied.reason(), Some("ROLE_MISSING"));
        assert_eq!(denied.metadata().len(), 1, "never tagged: {denied:?}");
        assert_eq!(denied.metadata()["role"], "admin");

        let asked = log.lock().unwrap().clone();
        let who: Vec<_> = asked
            .iter()
            .map(|asked| {
                (
                    asked.method,
                    asked.caller.as_deref(),
                    asked.end_user.as_deref(),
                )
            })
            .collect();
        assert_eq!(
            who,
            [
                ("release", None, None),
                ("track", None, None),
                ("reserve", Some("shop"), None),
                ("reserve", None, Some("user-1")),
            ]
        );
        assert_eq!(asked[2].subject.as_deref(), Some("user-3"));
    }

    #[tokio::test]
    async fn a_panicking_call_authorizer_is_internal_without_its_payload() {
        let (authorizer, _log) = recording();
        let served = Arc::new(guarded(false, 16, authorizer));
        assert_eq!(
            status_of(&served, RESERVE, bearer("user-panic")).await,
            "13"
        );
        let prepared = admit(&served, RESERVE, bearer("user-panic")).await.unwrap();
        let error = served
            .check_authorized(prepared.method, &prepared.cx)
            .unwrap_err();
        assert_eq!(error.reason(), Some(reasons::HANDLER_PANICKED));
        assert!(!error.message().contains("secret"), "{}", error.message());
        assert_eq!(error.metadata()["method"], "reserve");
        assert_eq!(served.server.in_flight(), 0);
    }

    const SESSION_ENDED: crate::EndUserRejection =
        crate::EndUserRejection::new("SESSION_ENDED").with_domain("accounts");

    const RELEASE: &str = "/shop.v1.Inventory/Release";
    const WATCH_METHODS: &[MethodDescriptor] =
        &[MethodDescriptor::call("watch", "Watch").with_stream()];
    const WATCHER: &ComponentDescriptor =
        &ComponentDescriptor::new("watcher", "Watcher", WATCH_METHODS).with_package("shop.v1");
    const WATCH: &str = "/shop.v1.Watcher/Watch";

    /// The site a scope was resolved for.
    #[derive(Debug, Clone, PartialEq)]
    struct Site(&'static str);

    /// Signals from `Drop` once the last clone of a [`Held`] is gone.
    struct Tracker(tokio::sync::watch::Sender<bool>);

    impl Drop for Tracker {
        fn drop(&mut self) {
            self.0.send_replace(true);
        }
    }

    /// A scope value whose release is observable.
    #[derive(Clone)]
    struct Held(#[allow(dead_code)] Arc<Tracker>);

    /// Resolves the scope from the `host` header: `a.test` is tenant `t-1`,
    /// `b.test` tenant `t-2`, `none.test` a scope without a tenant,
    /// `route.test` tenant `t-1` for `Reserve` only, `held.test` tenant
    /// `t-1` with a [`Held`], `locked.test` an `UNAUTHENTICATED` of its own,
    /// `bad.test` the tenant `t 1` (not a valid tenant); `hang.test` never
    /// answers, `panic.test` and `panic-now.test` panic, anything else is
    /// `NOT_FOUND`.
    #[derive(Default)]
    struct Hosts {
        /// The path of every request it was asked about.
        seen: std::sync::Mutex<Vec<String>>,
        /// The cancellation token of every call it was asked about, as a
        /// resolver that spawns work with it keeps it.
        tokens: std::sync::Mutex<Vec<tokio_util::sync::CancellationToken>>,
        /// Per `held.test` scope, whether its value was released.
        held: std::sync::Mutex<Vec<tokio::sync::watch::Receiver<bool>>>,
    }

    impl Hosts {
        fn seen(&self) -> usize {
            self.seen.lock().unwrap().len()
        }

        fn token(&self, index: usize) -> tokio_util::sync::CancellationToken {
            self.tokens.lock().unwrap()[index].clone()
        }

        fn released(&self, index: usize) -> bool {
            *self.held.lock().unwrap()[index].borrow()
        }

        fn scope_of(&self, path: &str, host: Option<&str>) -> Result<RequestScope, AppError> {
            match host {
                Some("a.test") => Ok(RequestScope::new()
                    .with_tenant("t-1")
                    .with_extension(Site("a"))),
                Some("b.test") => Ok(RequestScope::new()
                    .with_tenant("t-2")
                    .with_extension(Site("b"))),
                Some("none.test") => Ok(RequestScope::new().with_extension(Site("none"))),
                Some("route.test") if path == RESERVE => Ok(RequestScope::new().with_tenant("t-1")),
                Some("route.test") => Ok(RequestScope::new()),
                Some("held.test") => {
                    let (tx, rx) = tokio::sync::watch::channel(false);
                    self.held.lock().unwrap().push(rx);
                    Ok(RequestScope::new()
                        .with_tenant("t-1")
                        .with_extension(Held(Arc::new(Tracker(tx)))))
                }
                Some("locked.test") => {
                    Err(AppError::unauthenticated("this site is locked").with_reason("SITE_LOCKED"))
                }
                Some("bad.test") => Ok(RequestScope::new().with_tenant("t 1")),
                _ => Err(AppError::not_found("unknown site")
                    .with_reason("SITE_UNKNOWN")
                    .with_metadata("site", "unknown")),
            }
        }
    }

    impl ScopeResolver for Hosts {
        fn resolve<'a>(
            &'a self,
            request: &'a http::request::Parts,
            cx: &'a CallContext,
        ) -> BoxFuture<'a, Result<RequestScope, AppError>> {
            let path = request.uri.path();
            self.seen.lock().unwrap().push(path.to_owned());
            self.tokens.lock().unwrap().push(cx.cancel_token().clone());
            let host = request
                .headers
                .get(http::header::HOST)
                .and_then(|value| value.to_str().ok());
            assert!(host != Some("panic-now.test"), "sync panic");
            let scope = self.scope_of(path, host);
            let host = host.map(str::to_owned);
            Box::pin(async move {
                match host.as_deref() {
                    Some("hang.test") => std::future::pending().await,
                    Some("panic.test") => panic!("secret site"),
                    _ => scope,
                }
            })
        }
    }

    /// What a handler or the authorizer saw of its context.
    #[derive(Debug, Clone, PartialEq)]
    struct Saw {
        tenant: Option<String>,
        site: Option<Site>,
        scoped: bool,
    }

    impl Saw {
        fn of(cx: &CallContext) -> Self {
            Self {
                tenant: cx.tenant().map(str::to_owned),
                site: cx.scoped::<Site>().cloned(),
                scoped: cx.scope().is_some(),
            }
        }

        fn new(tenant: Option<&str>, site: Option<&'static str>, scoped: bool) -> Self {
            Self {
                tenant: tenant.map(str::to_owned),
                site: site.map(Site),
                scoped,
            }
        }
    }

    /// Records what each handler saw. A unary body `park` waits for
    /// `finish` after signalling `entered`; a stream yields `one`, then
    /// `two` once `finish` is signalled. Neither keeps its context.
    #[derive(Default)]
    struct Probe {
        saw: std::sync::Mutex<Vec<Saw>>,
        entered: tokio::sync::Notify,
        finish: tokio::sync::Notify,
    }

    impl Probe {
        fn saw(&self) -> Vec<Saw> {
            self.saw.lock().unwrap().clone()
        }
    }

    struct Probed(Arc<Probe>);

    impl Dispatch for Probed {
        fn dispatch(
            &self,
            _method: usize,
            cx: CallContext,
            body: Bytes,
        ) -> BoxFuture<'static, Result<Bytes, AppError>> {
            self.0.saw.lock().unwrap().push(Saw::of(&cx));
            drop(cx);
            let probe = Arc::clone(&self.0);
            Box::pin(async move {
                if &body[..] == b"park" {
                    probe.entered.notify_one();
                    probe.finish.notified().await;
                }
                Ok(body)
            })
        }

        fn dispatch_stream(
            &self,
            _method: usize,
            cx: CallContext,
            _body: Bytes,
        ) -> BoxFuture<'static, Result<CallStream<Bytes, AppError>, AppError>> {
            self.0.saw.lock().unwrap().push(Saw::of(&cx));
            drop(cx);
            let probe = Arc::clone(&self.0);
            let first = futures::stream::once(async { Ok(Bytes::from_static(b"one")) });
            let second = futures::stream::once(async move {
                probe.finish.notified().await;
                Ok(Bytes::from_static(b"two"))
            });
            Box::pin(async move { Ok(CallStream::new(futures::StreamExt::chain(first, second))) })
        }
    }

    /// How a scoped component is served.
    #[derive(Default)]
    struct Setup {
        link: bool,
        bearer: bool,
        timeout: Option<Duration>,
        client: Option<sekvent_resilience::ClientLimiter>,
        authorizer: Option<Arc<dyn crate::CallAuthorizer>>,
    }

    impl Setup {
        fn bearer() -> Self {
            Self {
                bearer: true,
                ..Self::default()
            }
        }

        fn serve(
            self,
            descriptor: &'static ComponentDescriptor,
            hosts: &Arc<Hosts>,
            probe: &Arc<Probe>,
        ) -> Arc<Served> {
            let policy = MethodPolicy {
                timeout: self.timeout,
                client: self.client,
                ..MethodPolicy::default()
            };
            let policies = vec![policy; descriptor.methods().len()];
            let gate = Server::new(descriptor, &policies, false).with_authorizer(self.authorizer);
            assert!(gate.open());
            let inbound = self.link.then(|| {
                Arc::new(TokenMap::new([InboundLink::trusted("shop", Secret::new(TOKEN))]).unwrap())
            });
            let sessions: Option<Arc<dyn EndUserAuthenticator>> = if self.bearer {
                Some(Arc::new(Sessions))
            } else {
                None
            };
            let hosts: Arc<Hosts> = Arc::clone(hosts);
            let resolver: Arc<dyn ScopeResolver> = hosts;
            Arc::new(
                Served::new(
                    Arc::new(gate),
                    Arc::new(Probed(Arc::clone(probe))),
                    inbound,
                    16,
                )
                .with_end_user(sessions)
                .with_scope_resolver(Some(resolver)),
            )
        }
    }

    fn fixtures() -> (Arc<Hosts>, Arc<Probe>) {
        (Arc::new(Hosts::default()), Arc::new(Probe::default()))
    }

    /// `wire` with the `host` header set.
    fn hosted(host: &str, mut wire: http::HeaderMap) -> http::HeaderMap {
        wire.insert(http::header::HOST, host.parse().unwrap());
        wire
    }

    /// One gRPC message frame.
    fn framed(message: &'static [u8]) -> http_body_util::Full<Bytes> {
        let mut frame = vec![0_u8];
        frame.extend_from_slice(&u32::try_from(message.len()).unwrap().to_be_bytes());
        frame.extend_from_slice(message);
        http_body_util::Full::new(Bytes::from(frame))
    }

    /// A request to `path` with `wire` headers and `body`.
    fn request<B>(path: &str, wire: http::HeaderMap, body: B) -> http::Request<B> {
        let mut request = http::Request::new(body);
        *request.uri_mut() = path.parse().unwrap();
        *request.headers_mut() = wire;
        request
    }

    /// `served`'s response to `request`, within 30 s.
    async fn respond<B>(served: &Arc<Served>, request: http::Request<B>) -> http::Response<Body>
    where
        B: http_body::Body<Data = Bytes> + Send + 'static,
        B::Error: Into<Box<dyn std::error::Error + Send + Sync>> + Send,
    {
        let call = tower::Service::call(&mut ComponentService(Arc::clone(served)), request);
        tokio::time::timeout(Duration::from_secs(30), call)
            .await
            .expect("answered")
            .unwrap()
    }

    /// The next frame of a response body, within 30 s.
    async fn next_frame(
        body: &mut Body,
    ) -> Option<Result<http_body::Frame<Bytes>, <Body as http_body::Body>::Error>> {
        tokio::time::timeout(
            Duration::from_secs(30),
            http_body_util::BodyExt::frame(body),
        )
        .await
        .expect("a frame")
    }

    /// The `grpc-status` of a trailers-only answer; `None` when the call
    /// went on to the handler.
    fn early_status(response: &http::Response<Body>) -> Option<&str> {
        response
            .headers()
            .get("grpc-status")
            .and_then(|value| value.to_str().ok())
    }

    #[tokio::test]
    async fn the_resolver_runs_for_every_caller_no_link_authenticated() {
        let (hosts, probe) = fixtures();
        let served = Setup::bearer().serve(INVENTORY, &hosts, &probe);

        let prepared = admit(&served, RESERVE, hosted("a.test", bearer("user-1")))
            .await
            .unwrap();
        assert_eq!(
            Saw::of(&prepared.cx),
            Saw::new(Some("t-1"), Some("a"), true)
        );
        assert_eq!(prepared.cx.end_user().map(EndUser::subject), Some("user-1"));

        let prepared = admit(&served, RELEASE, hosted("a.test", http::HeaderMap::new()))
            .await
            .unwrap();
        assert_eq!(
            Saw::of(&prepared.cx),
            Saw::new(Some("t-1"), Some("a"), true)
        );
        assert_eq!(prepared.cx.end_user(), None, "anonymous");

        let prepared = admit(&served, TRACK, hosted("a.test", http::HeaderMap::new()))
            .await
            .unwrap();
        assert_eq!(
            Saw::of(&prepared.cx),
            Saw::new(Some("t-1"), Some("a"), true)
        );
        assert_eq!(prepared.cx.end_user(), None, "optional, no header");
        let prepared = admit(&served, TRACK, hosted("a.test", bearer("user-2")))
            .await
            .unwrap();
        assert_eq!(
            Saw::of(&prepared.cx),
            Saw::new(Some("t-1"), Some("a"), true)
        );
        assert_eq!(prepared.cx.end_user().map(EndUser::subject), Some("user-2"));
        assert_eq!(hosts.seen(), 4);
        assert_eq!(hosts.seen.lock().unwrap()[0], RESERVE, "the routed path");

        // Under `SERVE_AUTH=none` too.
        let (hosts, probe) = fixtures();
        let open = Setup::default().serve(INVENTORY, &hosts, &probe);
        let prepared = admit(&open, RESERVE, hosted("b.test", http::HeaderMap::new()))
            .await
            .unwrap();
        assert_eq!(
            Saw::of(&prepared.cx),
            Saw::new(Some("t-2"), Some("b"), true)
        );
        assert!(format!("{open:?}").contains("scope_resolver: true"));
    }

    #[tokio::test]
    async fn the_resolver_never_runs_for_a_link_caller_or_an_unknown_path() {
        for with_bearer in [false, true] {
            let (hosts, probe) = fixtures();
            let served = Setup {
                link: true,
                bearer: with_bearer,
                ..Setup::default()
            }
            .serve(INVENTORY, &hosts, &probe);
            let wire = hosted("a.test", bearer(TOKEN));
            let response = respond(&served, request(RESERVE, wire, framed(b""))).await;
            assert_eq!(early_status(&response), None, "served");
            assert_eq!(probe.saw(), [Saw::new(None, None, false)]);
            assert_eq!(hosts.seen(), 0, "link, bearer: {with_bearer}");
        }

        let (hosts, probe) = fixtures();
        let served = Setup::bearer().serve(INVENTORY, &hosts, &probe);
        let stock = hosted("a.test", bearer("user-1"));
        assert_eq!(
            status_of(&served, "/shop.v1.Inventory/Stock", stock).await,
            "12"
        );
        assert_eq!(hosts.seen(), 0);
    }

    #[tokio::test]
    async fn the_resolver_runs_after_the_address_limit() {
        let (hosts, probe) = fixtures();
        let limiter = sekvent_resilience::ClientLimiter::new(
            sekvent_resilience::KeyedLimit::new(1, Duration::from_mins(1)),
            sekvent_resilience::ClientKey::ClientAddr,
        )
        .unwrap();
        let served = Setup {
            client: Some(limiter),
            ..Setup::default()
        }
        .serve(INVENTORY, &hosts, &probe);
        let from = |addr: [u8; 4]| {
            let mut request = request(
                RELEASE,
                hosted("a.test", http::HeaderMap::new()),
                framed(b""),
            );
            request
                .extensions_mut()
                .insert(ClientAddr(std::net::IpAddr::from(addr)));
            request
        };
        let first = respond(&served, from([192, 0, 2, 1])).await;
        assert_eq!(early_status(&first), None);
        let refused = respond(&served, from([192, 0, 2, 1])).await;
        assert_eq!(early_status(&refused), Some("8"));
        assert_eq!(hosts.seen(), 1, "a refused flood costs no resolution");
    }

    #[tokio::test]
    async fn the_authenticator_finds_the_scope_in_the_head() {
        let (hosts, probe) = fixtures();
        let served = Setup::bearer().serve(INVENTORY, &hosts, &probe);
        for (host, subject) in [("a.test", "saw-t-1"), ("none.test", "saw-none")] {
            let prepared = admit(&served, RESERVE, hosted(host, bearer("probe")))
                .await
                .unwrap();
            assert_eq!(prepared.cx.end_user().map(EndUser::subject), Some(subject));
        }
    }

    #[tokio::test]
    async fn the_handler_and_the_authorizer_see_the_scope() {
        let (hosts, probe) = fixtures();
        let asked = Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen = Arc::clone(&asked);
        let authorizer =
            move |_target: crate::CallTarget, cx: &CallContext| -> Result<(), AppError> {
                seen.lock().unwrap().push(Saw::of(cx));
                Ok(())
            };
        let served = Setup {
            bearer: true,
            authorizer: Some(Arc::new(authorizer)),
            ..Setup::default()
        }
        .serve(INVENTORY, &hosts, &probe);
        for (path, wire) in [
            (RESERVE, bearer("user-1")),
            (RELEASE, http::HeaderMap::new()),
        ] {
            let response =
                respond(&served, request(path, hosted("a.test", wire), framed(b""))).await;
            assert_eq!(early_status(&response), None);
        }
        let expected = Saw::new(Some("t-1"), Some("a"), true);
        assert_eq!(probe.saw(), [expected.clone(), expected.clone()]);
        assert_eq!(asked.lock().unwrap().clone(), [expected.clone(), expected]);
    }

    #[tokio::test]
    async fn a_client_tenant_header_never_replaces_the_scope() {
        let (hosts, probe) = fixtures();
        let served = Setup::bearer().serve(INVENTORY, &hosts, &probe);
        let forged = |mut wire: http::HeaderMap| {
            wire.insert(headers::TENANT, "t-forged".parse().unwrap());
            wire
        };
        for (path, wire) in [
            (RELEASE, http::HeaderMap::new()),
            (RESERVE, bearer("user-1")),
        ] {
            let prepared = admit(&served, path, hosted("a.test", forged(wire)))
                .await
                .unwrap();
            assert_eq!(prepared.cx.tenant(), Some("t-1"), "{path}");
        }
        // Without a tenant in the scope the header is dropped all the same.
        let prepared = admit(
            &served,
            RELEASE,
            hosted("none.test", forged(http::HeaderMap::new())),
        )
        .await
        .unwrap();
        assert_eq!(prepared.cx.tenant(), None);
        assert!(prepared.cx.scope().is_some());
    }

    #[tokio::test]
    async fn a_resolver_error_is_the_answer_unchanged() {
        let (hosts, probe) = fixtures();
        let served = Setup::bearer().serve(INVENTORY, &hosts, &probe);
        let error = admit(&served, RELEASE, hosted("c.test", http::HeaderMap::new()))
            .await
            .unwrap_err();
        assert_eq!(error.code(), ErrorCode::NotFound);
        assert_eq!(error.message(), "unknown site");
        assert_eq!(error.reason(), Some("SITE_UNKNOWN"));
        assert_eq!(error.metadata().len(), 1, "never tagged: {error:?}");
        assert_eq!(error.metadata()["site"], "unknown");

        // Its own UNAUTHENTICATED is not flattened.
        let error = admit(&served, RESERVE, hosted("locked.test", bearer("user-1")))
            .await
            .unwrap_err();
        assert_eq!(error.code(), ErrorCode::Unauthenticated);
        assert_eq!(error.message(), "this site is locked");
        assert_eq!(error.reason(), Some("SITE_LOCKED"));

        // Answered before the body, and the handler never runs.
        let wire = hosted("c.test", http::HeaderMap::new());
        assert_eq!(status_of(&served, RELEASE, wire).await, "5");
        assert_eq!(probe.saw(), [] as [Saw; 0]);
    }

    #[tokio::test(start_paused = true)]
    async fn the_resolver_runs_within_the_deadline_or_the_open_bound() {
        let (hosts, probe) = fixtures();
        let served = Setup {
            timeout: Some(Duration::from_millis(50)),
            ..Setup::default()
        }
        .serve(INVENTORY, &hosts, &probe);
        let error = admit(
            &served,
            RESERVE,
            hosted("hang.test", http::HeaderMap::new()),
        )
        .await
        .unwrap_err();
        assert_eq!(error.code(), ErrorCode::DeadlineExceeded);
        assert_eq!(error.message(), "the call deadline was exceeded");
        assert_eq!(error.metadata()["method"], "reserve");

        let watcher = Setup {
            timeout: Some(Duration::from_millis(50)),
            ..Setup::default()
        }
        .serve(WATCHER, &hosts, &probe);
        let error = admit(&watcher, WATCH, hosted("hang.test", http::HeaderMap::new()))
            .await
            .unwrap_err();
        assert_eq!(error.code(), ErrorCode::DeadlineExceeded);
        assert_eq!(error.metadata()["method"], "watch");
    }

    #[tokio::test]
    async fn a_panicking_resolver_is_internal_without_its_payload() {
        let (hosts, probe) = fixtures();
        let served = Setup::default().serve(INVENTORY, &hosts, &probe);
        for host in ["panic.test", "panic-now.test"] {
            let error = admit(&served, RESERVE, hosted(host, http::HeaderMap::new()))
                .await
                .unwrap_err();
            assert_eq!(error.code(), ErrorCode::Internal);
            assert_eq!(error.reason(), Some(reasons::HANDLER_PANICKED));
            assert_eq!(
                error.message(),
                "the scope resolver of component inventory panicked"
            );
            assert_eq!(error.metadata()["method"], "reserve");
        }
        assert_eq!(served.server.in_flight(), 0);
    }

    #[tokio::test]
    async fn an_end_user_must_belong_to_the_scopes_tenant() {
        let (hosts, probe) = fixtures();
        let served = Setup::bearer().serve(INVENTORY, &hosts, &probe);
        for (host, token, tenant) in [
            ("none.test", "user-1", Some("t-1")),
            ("none.test", "anyone", None),
            ("a.test", "user-1", Some("t-1")),
        ] {
            let prepared = admit(&served, RESERVE, hosted(host, bearer(token)))
                .await
                .unwrap();
            assert_eq!(prepared.cx.tenant(), tenant, "{host} {token}");
            assert_eq!(
                prepared.cx.end_user().and_then(EndUser::tenant),
                if token == "user-1" { Some("t-1") } else { None }
            );
        }

        // A user of `t-1` on a site of `t-2`, or a user without a tenant on
        // a site of `t-1`: the answer to a bad token, even from an
        // authenticator that shows reasons for other refusals.
        for (host, token) in [("b.test", "user-1"), ("a.test", "anyone")] {
            for path in [RESERVE, TRACK] {
                let error = admit(&served, path, hosted(host, bearer(token)))
                    .await
                    .unwrap_err();
                assert_eq!(error.code(), ErrorCode::Unauthenticated, "{host} {token}");
                assert_eq!(error.message(), END_USER_REJECTED_MESSAGE);
                assert_eq!(error.reason(), None);
                assert_eq!(error.domain(), None);
                assert!(error.metadata().is_empty());
            }
            let wire = hosted(host, bearer(token));
            assert_eq!(status_of(&served, RESERVE, wire).await, "16");
        }
        assert_eq!(probe.saw(), [] as [Saw; 0]);
    }

    #[tokio::test]
    async fn a_resolver_may_scope_by_route() {
        let (hosts, probe) = fixtures();
        let served = Setup::bearer().serve(INVENTORY, &hosts, &probe);
        let prepared = admit(&served, RESERVE, hosted("route.test", bearer("user-1")))
            .await
            .unwrap();
        assert_eq!(prepared.cx.tenant(), Some("t-1"));
        // The scope that route names a tenant, so a user without one is refused.
        let error = admit(&served, RESERVE, hosted("route.test", bearer("anyone")))
            .await
            .unwrap_err();
        assert_eq!(error.code(), ErrorCode::Unauthenticated);
        let prepared = admit(
            &served,
            RELEASE,
            hosted("route.test", http::HeaderMap::new()),
        )
        .await
        .unwrap();
        assert_eq!(Saw::of(&prepared.cx), Saw::new(None, None, true));
    }

    #[tokio::test]
    async fn a_unary_call_keeps_the_scope_until_the_handler_returned() {
        let (hosts, probe) = fixtures();
        let served = Setup::default().serve(INVENTORY, &hosts, &probe);
        let call = {
            let served = Arc::clone(&served);
            let request = request(
                RELEASE,
                hosted("held.test", http::HeaderMap::new()),
                framed(b"park"),
            );
            tokio::spawn(async move { respond(&served, request).await })
        };
        tokio::time::timeout(Duration::from_secs(30), probe.entered.notified())
            .await
            .expect("the handler runs");
        assert!(!hosts.released(0), "held while the handler is parked");
        probe.finish.notify_one();
        let response = call.await.unwrap();
        assert_eq!(early_status(&response), None);
        drop(response);
        assert!(hosts.released(0), "released once the call returned");
        assert!(
            !hosts.token(0).is_cancelled(),
            "a served call is not cancelled"
        );
    }

    #[tokio::test]
    async fn a_stream_keeps_the_scope_until_the_response_ended() {
        let (hosts, probe) = fixtures();
        let served = Setup::default().serve(WATCHER, &hosts, &probe);
        let response = respond(
            &served,
            request(
                WATCH,
                hosted("held.test", http::HeaderMap::new()),
                framed(b""),
            ),
        )
        .await;
        assert_eq!(early_status(&response), None, "the stream opened");
        assert_eq!(probe.saw(), [Saw::new(Some("t-1"), None, true)]);
        assert!(!hosts.released(0), "kept after the open returned");

        let mut body = response.into_body();
        let first = next_frame(&mut body).await.unwrap().unwrap();
        assert!(first.is_data());
        assert!(!hosts.released(0), "kept while the stream runs");
        assert!(
            !hosts.token(0).is_cancelled(),
            "the open handed the stream over without cancelling it"
        );

        probe.finish.notify_one();
        while let Some(frame) = next_frame(&mut body).await {
            frame.unwrap();
        }
        assert!(hosts.released(0), "released once the stream ended");
    }

    #[tokio::test]
    async fn an_end_user_rejection_shows_its_reason_and_domain() {
        let (hosts, probe) = fixtures();
        let served = Setup::bearer().serve(INVENTORY, &hosts, &probe);
        for path in [RESERVE, TRACK] {
            let error = admit(&served, path, hosted("a.test", bearer("ended")))
                .await
                .unwrap_err();
            assert_eq!(error.code(), ErrorCode::Unauthenticated);
            assert_eq!(error.message(), END_USER_REJECTED_MESSAGE);
            assert_eq!(error.reason(), Some("SESSION_ENDED"));
            assert_eq!(error.domain(), Some("accounts"));
            assert!(error.metadata().is_empty(), "{error:?}");
            let wire = hosted("a.test", bearer("ended"));
            assert_eq!(status_of(&served, path, wire).await, "16");
        }
        assert_eq!(probe.saw(), [] as [Saw; 0], "the handler never ran");
    }

    #[tokio::test]
    async fn an_expired_call_never_reaches_the_resolver() {
        let (hosts, probe) = fixtures();
        let inventory = Setup::bearer().serve(INVENTORY, &hosts, &probe);
        let watcher = Setup::bearer().serve(WATCHER, &hosts, &probe);
        for (served, path, method) in [
            (&inventory, RESERVE, "reserve"),
            (&inventory, RELEASE, "release"),
            (&watcher, WATCH, "watch"),
        ] {
            for host in ["a.test", "c.test"] {
                let mut wire = hosted(host, bearer("user-1"));
                wire.insert(headers::GRPC_TIMEOUT, "0n".parse().unwrap());
                let error = admit(served, path, wire).await.unwrap_err();
                assert_eq!(error.code(), ErrorCode::DeadlineExceeded, "{path} {host}");
                assert_eq!(error.message(), "the call deadline was exceeded");
                assert_eq!(error.metadata()["method"], method);
            }
        }
        let mut wire = hosted("c.test", http::HeaderMap::new());
        wire.insert(headers::GRPC_TIMEOUT, "0n".parse().unwrap());
        assert_eq!(status_of(&inventory, RELEASE, wire).await, "4");
        assert_eq!(hosts.seen(), 0, "never asked");
        assert_eq!(probe.saw(), [] as [Saw; 0]);
    }

    #[tokio::test]
    async fn a_scope_with_an_invalid_tenant_is_internal() {
        let (hosts, probe) = fixtures();
        let served = Setup::bearer().serve(INVENTORY, &hosts, &probe);
        for (path, wire, method) in [
            (RELEASE, http::HeaderMap::new(), "release"),
            (RESERVE, bearer("user-1"), "reserve"),
        ] {
            let error = admit(&served, path, hosted("bad.test", wire))
                .await
                .unwrap_err();
            assert_eq!(error.code(), ErrorCode::Internal, "{path}");
            assert_eq!(error.reason(), Some(reasons::SCOPE_TENANT_INVALID));
            assert_eq!(
                error.message(),
                "the scope resolver returned an invalid tenant"
            );
            assert_eq!(error.metadata()["method"], method);
        }
        let wire = hosted("bad.test", http::HeaderMap::new());
        assert_eq!(status_of(&served, RELEASE, wire).await, "13");
        assert_eq!(probe.saw(), [] as [Saw; 0]);
    }

    #[tokio::test]
    async fn a_call_dropped_during_resolution_cancels_its_token() {
        let (hosts, probe) = fixtures();
        let inventory = Setup::default().serve(INVENTORY, &hosts, &probe);
        let watcher = Setup::default().serve(WATCHER, &hosts, &probe);
        for (index, (served, path)) in [(&inventory, RELEASE), (&watcher, WATCH)]
            .into_iter()
            .enumerate()
        {
            let request = request(
                path,
                hosted("hang.test", http::HeaderMap::new()),
                framed(b""),
            );
            let mut call = tower::Service::call(&mut ComponentService(Arc::clone(served)), request);
            assert!(futures::poll!(&mut call).is_pending(), "{path}");
            let token = hosts.token(index);
            assert!(!token.is_cancelled(), "{path}: resolving");
            // As hyper drops it when the client goes away.
            drop(call);
            assert!(token.is_cancelled(), "{path}");
        }
        assert_eq!(probe.saw(), [] as [Saw; 0]);
    }

    #[tokio::test(start_paused = true)]
    async fn a_resolution_past_the_deadline_cancels_its_token() {
        let (hosts, probe) = fixtures();
        let bounded = || Setup {
            timeout: Some(Duration::from_millis(50)),
            ..Setup::default()
        };
        let inventory = bounded().serve(INVENTORY, &hosts, &probe);
        let watcher = bounded().serve(WATCHER, &hosts, &probe);
        for (index, (served, path)) in [(&inventory, RELEASE), (&watcher, WATCH)]
            .into_iter()
            .enumerate()
        {
            let wire = hosted("hang.test", http::HeaderMap::new());
            let response = respond(served, request(path, wire, framed(b""))).await;
            assert_eq!(early_status(&response), Some("4"), "{path}");
            assert!(hosts.token(index).is_cancelled(), "{path}");
        }
        assert_eq!(probe.saw(), [] as [Saw; 0]);
    }

    #[test]
    fn errors_answer_without_a_body() {
        let response = error_response(AppError::unauthenticated("no"));
        assert_eq!(response.headers()["grpc-status"], "16");
    }
}
