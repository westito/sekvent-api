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
//! check.

use std::convert::Infallible;
use std::sync::Arc;
use std::task::{Context, Poll};

use bytes::Bytes;
use sekvent_context::{CallContext, ServiceIdentity, headers};
use sekvent_error::{AppError, ErrorCode};
use sekvent_link::TokenMap;
use tonic::body::Body;
use tonic::server::Grpc;
use tonic::service::Routes;
use tracing::Instrument as _;

use super::codec::BytesCodec;
use crate::__private::{BoxFuture, Dispatch};
use crate::end_user::{EndUserAuthenticator, rejection};
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
    /// The accepted inbound tokens; `None` unless `SERVE_AUTH` includes
    /// `link`.
    inbound: Option<Arc<TokenMap>>,
    /// The end-user authenticator; `None` unless `SERVE_AUTH` includes
    /// `bearer`.
    end_user: Option<Arc<dyn EndUserAuthenticator>>,
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

    /// Everything decided from the request headers alone, before the body
    /// is read and before an end user is authenticated: the link, the
    /// route, the context and its deadline (the method's timeout narrows
    /// it), and whether the end-user authenticator must run.
    fn prepare(&self, path: &str, wire_headers: &http::HeaderMap) -> Result<Prepared, AppError> {
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
        let end_user = caller.is_none()
            && self.end_user.is_some()
            && !descriptor
                .methods()
                .get(method)
                .is_some_and(crate::MethodDescriptor::is_anonymous);
        let mut cx = headers::from_headers(wire_headers, caller);
        if let Some(deadline) = self
            .server
            .timeout(method)
            .and_then(|timeout| tokio::time::Instant::now().checked_add(timeout))
        {
            cx = cx.with_deadline(deadline.into_std());
        }
        Ok(Prepared {
            method,
            cx,
            end_user,
        })
    }

    /// Run the end-user authenticator on the request head within the
    /// call's deadline, and make the end user the context's caller.
    async fn authenticate(
        &self,
        method: usize,
        cx: CallContext,
        head: &http::request::Parts,
    ) -> Result<CallContext, AppError> {
        let Some(authenticator) = &self.end_user else {
            return Ok(cx);
        };
        // Deferred into the caught poll, so a sync closure that panics is
        // caught too.
        let attempt = CatchPanic::new(async move { authenticator.authenticate(head).await });
        let outcome = match sekvent_resilience::remaining(&cx) {
            Some(remaining) => tokio::time::timeout(remaining, attempt)
                .await
                .map_err(|_elapsed| deadline_exceeded(self.descriptor(), method))?,
            None => attempt.await,
        };
        match outcome {
            Ok(Ok(user)) => Ok(cx.with_end_user(user)),
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
        let prepared = served.prepare(path, request.headers());
        Box::pin(
            async move {
                let Prepared {
                    method,
                    mut cx,
                    end_user,
                } = match prepared {
                    Ok(prepared) => prepared,
                    Err(error) => return Ok(error_response(error)),
                };
                let (head, body) = request.into_parts();
                if end_user {
                    cx = match served.authenticate(method, cx, &head).await {
                        Ok(cx) => cx,
                        Err(error) => return Ok(error_response(error)),
                    };
                }
                if let Err(error) = served.check_hops(method, &cx) {
                    return Ok(error_response(error));
                }
                let request = http::Request::from_parts(head, body);
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
        // Anonymous methods still need the link under link-only serving.
        let error = served
            .prepare("/shop.v1.Inventory/Release", &http::HeaderMap::new())
            .unwrap_err();
        assert_eq!(error.message(), sekvent_link::REJECTED_MESSAGE);
        let prepared = served.prepare(RESERVE, &bearer(TOKEN)).unwrap();
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
        let prepared = served.prepare(RESERVE, &wire).unwrap();
        let error = served
            .check_hops(prepared.method, &prepared.cx)
            .unwrap_err();
        assert_eq!(error.reason(), Some(reasons::CALL_DEPTH_EXCEEDED));
        assert_eq!(error.metadata()["method"], "reserve");

        let before = std::time::Instant::now();
        let prepared = served.prepare(RESERVE, &bearer(TOKEN)).unwrap();
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

    /// Accepts `Bearer user-<n>`, fails `UNAVAILABLE` for `Bearer down`,
    /// panics for `Bearer panic`, never answers for `Bearer hang`, and
    /// rejects anything else with a detailed message.
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
            Box::pin(async move {
                match token.as_deref() {
                    Some("down") => Err(AppError::unavailable("sessions are down")),
                    Some("panic") => panic!("secret token"),
                    Some("hang") => std::future::pending().await,
                    Some(user) if user.starts_with("user-") => {
                        Ok(EndUser::new(user).with_tenant("t-1").with_roles(["buyer"]))
                    }
                    _ => Err(AppError::unauthenticated("token signature mismatch")),
                }
            })
        }
    }

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

    /// `prepare` and, when it asks for one, the end user.
    async fn admit(
        served: &Served,
        path: &str,
        wire: http::HeaderMap,
    ) -> Result<Prepared, AppError> {
        let mut prepared = served.prepare(path, &wire)?;
        if prepared.end_user {
            prepared.cx = served
                .authenticate(prepared.method, prepared.cx, &head(wire))
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
    }

    #[tokio::test(start_paused = true)]
    async fn the_authenticator_runs_within_the_deadline() {
        let served = end_user_served(false, Some(Duration::from_millis(50)));
        let error = admit(&served, RESERVE, bearer("hang")).await.unwrap_err();
        assert_eq!(error.code(), ErrorCode::DeadlineExceeded);
        assert_eq!(error.metadata()["component"], "inventory");

        let unbounded = end_user_served(false, None);
        let prepared = unbounded.prepare(RESERVE, &bearer("user-1")).unwrap();
        let cx = served
            .authenticate(prepared.method, prepared.cx, &head(bearer("user-1")))
            .await
            .unwrap();
        assert!(cx.end_user().is_some());
    }

    #[tokio::test]
    async fn without_an_authenticator_the_context_is_unchanged() {
        let served = served();
        let cx = served
            .authenticate(0, CallContext::new(), &head(bearer("user-1")))
            .await
            .unwrap();
        assert_eq!(cx.end_user(), None);
    }

    #[test]
    fn errors_answer_without_a_body() {
        let response = error_response(AppError::unauthenticated("no"));
        assert_eq!(response.headers()["grpc-status"], "16");
    }
}
