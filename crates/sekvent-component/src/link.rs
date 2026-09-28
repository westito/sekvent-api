//! The caller side: what a generated handle wraps, and the pipelines that
//! carry one call to the serving side under each binding.

use std::fmt;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use http::HeaderMap;
use sekvent_context::{CallContext, ServiceIdentity, headers};
use sekvent_error::{AppError, ErrorCode};
use tokio::task::JoinError;
use tokio::time::Instant;
use tracing::Instrument as _;

use crate::__private::{Dispatch, LocalMessage, WireMessage};
#[cfg(feature = "grpc")]
use crate::grpc::client::RemoteClient;
use crate::server::Server;
use crate::wire::{self, AbortOnDrop};
use crate::{Binding, ComponentError, LOCAL_CALLER, reasons};

/// Metadata key naming the component whose transient failure a call made
/// from inside a handler returned. The gRPC serving side turns an error
/// carrying it into `INTERNAL` / [`reasons::DOWNSTREAM_FAILURE`], so the
/// callers above neither retry the healthy component in between nor count
/// the failure against it.
pub(crate) const DOWNSTREAM: &str = "downstream";

/// The caller's view of one installed component: its binding, the gate
/// its calls pass, the hop limit and, under `grpc`, the remote client.
#[derive(Debug)]
pub(crate) struct Link {
    binding: Binding,
    server: Arc<Server>,
    max_hops: u32,
    #[cfg(feature = "grpc")]
    remote: Option<Arc<RemoteClient>>,
}

impl Link {
    pub(crate) fn new(binding: Binding, server: Arc<Server>, max_hops: u32) -> Self {
        Self {
            binding,
            server,
            max_hops,
            #[cfg(feature = "grpc")]
            remote: None,
        }
    }

    /// Send the calls through `client`.
    #[cfg(feature = "grpc")]
    #[must_use]
    pub(crate) fn with_remote(mut self, client: Arc<RemoteClient>) -> Self {
        self.remote = Some(client);
        self
    }

    pub(crate) fn binding(&self) -> Binding {
        self.binding
    }
}

/// Where the calls of an endpoint go.
pub(crate) enum Route<D: ?Sized> {
    /// Straight into the implementation.
    Local(Arc<D>),
    /// Through the byte-level dispatcher, on a task of its own.
    Serialized(Arc<dyn Dispatch>),
    /// Over gRPC to another process.
    #[cfg(feature = "grpc")]
    Remote(Arc<RemoteClient>),
}

impl<D: ?Sized> Clone for Route<D> {
    fn clone(&self) -> Self {
        match self {
            Self::Local(imp) => Self::Local(Arc::clone(imp)),
            Self::Serialized(dispatch) => Self::Serialized(Arc::clone(dispatch)),
            #[cfg(feature = "grpc")]
            Self::Remote(client) => Self::Remote(Arc::clone(client)),
        }
    }
}

/// What a generated component handle wraps. Built only by the App builder.
///
/// `D` is the component's object-safe trait (`dyn __XDyn`).
pub struct Endpoint<D: ?Sized> {
    link: Arc<Link>,
    route: Route<D>,
}

impl<D: ?Sized> Endpoint<D> {
    pub(crate) fn new(link: Arc<Link>, route: Route<D>) -> Self {
        Self { link, route }
    }

    /// An endpoint calling the link's remote client; `None` without one.
    pub(crate) fn remote(link: Arc<Link>) -> Option<Self> {
        #[cfg(feature = "grpc")]
        if let Some(client) = link.remote.clone() {
            return Some(Self::new(link, Route::Remote(client)));
        }
        drop(link);
        None
    }
}

impl<D: ?Sized> Clone for Endpoint<D> {
    fn clone(&self) -> Self {
        Self {
            link: Arc::clone(&self.link),
            route: self.route.clone(),
        }
    }
}

impl<D: ?Sized> fmt::Debug for Endpoint<D> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Endpoint")
            .field("component", &self.link.server.descriptor().name())
            .field("binding", &self.link.binding)
            .finish_non_exhaustive()
    }
}

impl<D: ?Sized + Send + Sync + 'static> Endpoint<D> {
    /// The binding this endpoint's calls use.
    pub fn binding(&self) -> Binding {
        self.link.binding
    }

    /// Call method `method` of a standard component: directly under
    /// `local` (through `local(imp, callee_cx, req)`), encoded and on a task
    /// of its own under `local-serialized`, over gRPC under `grpc`.
    #[allow(clippy::manual_async_fn)]
    pub fn call<'a, Req, Rep, E, L, Fut>(
        &'a self,
        method: usize,
        cx: &'a CallContext,
        req: Req,
        local: L,
    ) -> impl Future<Output = Result<Rep, E>> + Send + 'a
    where
        Req: WireMessage,
        Rep: WireMessage,
        E: ComponentError,
        L: FnOnce(Arc<D>, CallContext, Req) -> Fut + Send + 'a,
        Fut: Future<Output = Result<Rep, E>> + Send + 'a,
    {
        async move {
            let link = &*self.link;
            let server = &link.server;
            let outcome = match &self.route {
                Route::Local(imp) => {
                    let imp = Arc::clone(imp);
                    invoke(link, method, cx, MethodTimeout::Here, move |callee| {
                        run_local(server, method, callee, imp, req, local)
                    })
                    .await
                }
                Route::Serialized(dispatch) => {
                    let dispatch = Arc::clone(dispatch);
                    invoke(link, method, cx, MethodTimeout::Here, move |callee| {
                        run_serialized::<Req, Rep>(server, dispatch, method, callee, req)
                    })
                    .await
                }
                #[cfg(feature = "grpc")]
                Route::Remote(client) => {
                    let client = Arc::clone(client);
                    invoke(link, method, cx, MethodTimeout::Transport, move |callee| {
                        run_remote::<Req, Rep>(server, client, method, callee, req)
                    })
                    .await
                }
            };
            outcome.map_err(E::from_app_error)
        }
    }

    /// Call method `method` of a `local_only` component: always the local
    /// pipeline. Another route cannot exist after a successful build; it
    /// would yield `INTERNAL`, never a panic.
    #[allow(clippy::manual_async_fn)]
    pub fn call_local<'a, Req, Rep, E, L, Fut>(
        &'a self,
        method: usize,
        cx: &'a CallContext,
        req: Req,
        local: L,
    ) -> impl Future<Output = Result<Rep, E>> + Send + 'a
    where
        Req: LocalMessage,
        Rep: LocalMessage,
        E: ComponentError,
        L: FnOnce(Arc<D>, CallContext, Req) -> Fut + Send + 'a,
        Fut: Future<Output = Result<Rep, E>> + Send + 'a,
    {
        async move {
            let link = &*self.link;
            let server = &link.server;
            let outcome = match &self.route {
                Route::Local(imp) => {
                    let imp = Arc::clone(imp);
                    invoke(link, method, cx, MethodTimeout::Here, move |callee| {
                        run_local(server, method, callee, imp, req, local)
                    })
                    .await
                }
                Route::Serialized(_) => Err(not_serializable(server, method)),
                #[cfg(feature = "grpc")]
                Route::Remote(_) => Err(not_serializable(server, method)),
            };
            outcome.map_err(E::from_app_error)
        }
    }
}

/// Who enforces the method's `TIMEOUT` for one call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MethodTimeout {
    /// [`invoke`] narrows the callee's deadline and reports its expiry.
    Here,
    /// The transport does (the remote client, which must tell its circuit
    /// breaker a slow callee from a caller that gave up); [`invoke`] only
    /// enforces the caller's deadline.
    #[cfg_attr(not(feature = "grpc"), allow(dead_code))]
    Transport,
}

/// The caller side shared by every binding: enforce the hop limit, derive
/// the callee's context, and run `transport` within the call's deadline and
/// until the caller cancels, in one `debug` span.
///
/// The method's `TIMEOUT` bounds the whole call. Its expiry while the
/// caller still has time is `DEADLINE_EXCEEDED` / [`reasons::METHOD_TIMEOUT`];
/// the caller's own deadline passing is a plain `DEADLINE_EXCEEDED`. A call
/// whose caller already cancelled or ran out of time fails before
/// `transport` is polled, so nothing is encoded or sent.
async fn invoke<Rep, T, Fut>(
    link: &Link,
    method: usize,
    cx: &CallContext,
    enforce: MethodTimeout,
    transport: T,
) -> Result<Rep, AppError>
where
    T: FnOnce(CallContext) -> Fut,
    Fut: Future<Output = Result<Rep, AppError>>,
{
    let server = &*link.server;
    let descriptor = server.descriptor();
    let span = tracing::debug_span!(
        "component_call",
        component = descriptor.name(),
        method = descriptor
            .methods()
            .get(method)
            .map_or("?", |method| method.name()),
        binding = link.binding.as_str(),
    );
    async move {
        let timeout = match enforce {
            MethodTimeout::Here => server.timeout(method),
            MethodTimeout::Transport => None,
        };
        let bounds = Bounds::new(cx, timeout);
        let expired = || server.tag(bounds.expired(cx, timeout), method);
        let cancelled = || server.tag(AppError::cancelled("the call was cancelled"), method);
        if cx.cancel_token().is_cancelled() {
            return Err(cancelled());
        }
        if bounds.passed() {
            return Err(expired());
        }
        let hops = cx.hops().saturating_add(1);
        if hops > link.max_hops {
            return Err(server.tag(depth_exceeded(link.max_hops), method));
        }
        let callee = callee_context(cx, bounds.method).with_hops(hops);
        let outcome = tokio::select! {
            biased;
            () = cx.cancelled() => Err(cancelled()),
            result = transport(callee) => result.map_err(|error| {
                if bounds.method_fired(cx) && is_plain_deadline(&error) {
                    expired()
                } else {
                    error
                }
            }),
            () = bounds.sleep() => Err(expired()),
        };
        outcome.map_err(|error| mark_downstream(cx, descriptor.name(), error))
    }
    .instrument(span)
    .await
}

/// The two deadlines of one call, on tokio's clock: the caller's and, when
/// [`invoke`] enforces it, the method's own.
#[derive(Debug, Clone, Copy)]
struct Bounds {
    caller: Option<Instant>,
    method: Option<Instant>,
}

impl Bounds {
    fn new(cx: &CallContext, timeout: Option<Duration>) -> Self {
        Self {
            caller: cx.deadline().map(Instant::from_std),
            method: timeout.and_then(|timeout| Instant::now().checked_add(timeout)),
        }
    }

    /// The earlier of the two deadlines.
    fn effective(self) -> Option<Instant> {
        match (self.caller, self.method) {
            (Some(caller), Some(method)) => Some(caller.min(method)),
            (caller, method) => caller.or(method),
        }
    }

    fn passed(self) -> bool {
        self.effective()
            .is_some_and(|deadline| deadline <= Instant::now())
    }

    async fn sleep(self) {
        match self.effective() {
            Some(deadline) => tokio::time::sleep_until(deadline).await,
            None => std::future::pending().await,
        }
    }

    /// Whether the method's deadline — not the caller's — has passed, with
    /// the caller still having time left.
    fn method_fired(self, cx: &CallContext) -> bool {
        let now = Instant::now();
        self.method.is_some_and(|method| {
            method <= now && self.caller.is_none_or(|caller| method < caller) && !caller_expired(cx)
        })
    }

    /// The error for the call's deadline passing.
    fn expired(self, cx: &CallContext, timeout: Option<Duration>) -> AppError {
        match timeout {
            Some(timeout) if self.method_fired(cx) => method_timeout(timeout),
            _ => AppError::deadline_exceeded("the call deadline was exceeded"),
        }
    }
}

/// Whether `cx`'s own deadline has passed (on tokio's clock).
pub(crate) fn caller_expired(cx: &CallContext) -> bool {
    sekvent_resilience::remaining(cx) == Some(Duration::ZERO)
}

/// `DEADLINE_EXCEEDED` / [`reasons::METHOD_TIMEOUT`]: the method's own
/// `timeout` expired while the caller still had time.
pub(crate) fn method_timeout(timeout: Duration) -> AppError {
    AppError::deadline_exceeded(format!(
        "the call did not complete within the method timeout of {} ms",
        timeout.as_millis()
    ))
    .with_reason(reasons::METHOD_TIMEOUT)
    .with_metadata("timeout_ms", timeout.as_millis().to_string())
}

/// A `DEADLINE_EXCEEDED` that does not say whose deadline passed: the
/// serving gate's or a transport's view of the same expiry.
fn is_plain_deadline(error: &AppError) -> bool {
    error.code() == ErrorCode::DeadlineExceeded && error.reason().is_none()
}

/// Whether `error` is a transient failure of the callee itself, as opposed
/// to the caller's own deadline or cancellation, or a definitive answer.
fn is_downstream_failure(error: &AppError) -> bool {
    match error.code() {
        ErrorCode::Unavailable | ErrorCode::ResourceExhausted => true,
        ErrorCode::DeadlineExceeded => error.reason() == Some(reasons::METHOD_TIMEOUT),
        _ => false,
    }
}

/// Name `component` as the [`DOWNSTREAM`] failure when a call made from
/// inside a handler (`hops > 0`) failed transiently. A marker set further
/// down the chain is kept, so it names where the failure started; a call
/// made at the edge (`hops == 0`) is left alone, its caller is the one to
/// retry.
fn mark_downstream(cx: &CallContext, component: &str, error: AppError) -> AppError {
    if cx.hops() == 0 || error.metadata().contains_key(DOWNSTREAM) || !is_downstream_failure(&error)
    {
        return error;
    }
    error.with_metadata(DOWNSTREAM, component)
}

/// `INTERNAL` for a `local_only` component reached through a route that
/// serializes; a successful build never creates one.
fn not_serializable(server: &Server, method: usize) -> AppError {
    server.tag(
        AppError::new(
            ErrorCode::Internal,
            format!(
                "component {} takes plain Rust values and cannot be called \
                 across a serialization boundary",
                server.descriptor().name()
            ),
        ),
        method,
    )
}

/// `FAILED_PRECONDITION` / `CALL_DEPTH_EXCEEDED` for a call deeper than
/// `max_hops`.
pub(crate) fn depth_exceeded(max_hops: u32) -> AppError {
    AppError::failed_precondition(format!(
        "the call is more than {max_hops} component calls deep"
    ))
    .with_reason(reasons::CALL_DEPTH_EXCEEDED)
}

/// The context sent to the callee: the caller's identity and trace, a child
/// cancellation token, the in-process caller identity, and the deadline
/// narrowed to `method_deadline` (on tokio's clock). An idempotency key the
/// caller received is not carried over (see [`CallContext::child`]).
fn callee_context(cx: &CallContext, method_deadline: Option<Instant>) -> CallContext {
    let callee = cx
        .child()
        .with_caller(ServiceIdentity::trusted(LOCAL_CALLER));
    match method_deadline {
        Some(deadline) => callee.with_deadline(deadline.into_std()),
        None => callee,
    }
}

/// `local`: the serving side around the implementation, in the caller's
/// task. A typed error is turned into its `AppError` form, so both bindings
/// hand the caller the same value.
async fn run_local<D, Req, Rep, E, L, Fut>(
    server: &Server,
    method: usize,
    callee: CallContext,
    imp: Arc<D>,
    req: Req,
    local: L,
) -> Result<Rep, AppError>
where
    D: ?Sized,
    E: ComponentError,
    L: FnOnce(Arc<D>, CallContext, Req) -> Fut,
    Fut: Future<Output = Result<Rep, E>>,
{
    match server
        .run(method, callee.into_inbound(), move |cx| local(imp, cx, req))
        .await
    {
        Ok(Ok(reply)) => Ok(reply),
        Ok(Err(error)) => Err(error.into_app_error()),
        Err(error) => Err(error),
    }
}

/// `local-serialized`: encode the request and the context, serve the call on
/// a task of its own, decode the outcome. Dropping the returned future aborts
/// the task.
async fn run_serialized<Req, Rep>(
    server: &Arc<Server>,
    dispatch: Arc<dyn Dispatch>,
    method: usize,
    callee: CallContext,
    req: Req,
) -> Result<Rep, AppError>
where
    Req: WireMessage,
    Rep: WireMessage,
{
    let body = Bytes::from(req.encode_to_vec());
    let mut wire_headers = HeaderMap::new();
    headers::inject(&callee, &mut wire_headers);
    // The header codec re-anchors the deadline on the std clock when the
    // task reads it, which can land it a little after the caller's.
    let deadline = callee.deadline();
    // Headers carry no cancellation: the served context shares the caller's
    // child token, so work the handler hands off (spawned tasks, detached
    // calls it chose to bind to the call) sees the caller cancel.
    let cancel = callee.cancel_token().clone();
    let serving = Arc::clone(server);
    let task = tokio::spawn(
        async move {
            let mut cx =
                headers::from_headers(&wire_headers, Some(ServiceIdentity::trusted(LOCAL_CALLER)))
                    .with_cancel(cancel);
            if let Some(deadline) = deadline {
                cx = cx.with_deadline(deadline);
            }
            match serving
                .run(method, cx, move |cx| dispatch.dispatch(method, cx, body))
                .await
            {
                Ok(Ok(reply)) => Ok(reply),
                Ok(Err(error)) | Err(error) => Err(wire::encode_error(&error)),
            }
        }
        .instrument(tracing::Span::current()),
    );
    match AbortOnDrop(task).await {
        Ok(Ok(reply)) => Rep::decode(reply).map_err(|_| server.tag(malformed_reply(), method)),
        Ok(Err(error)) => {
            Err(wire::decode_error(error).unwrap_or_else(|| server.tag(malformed_reply(), method)))
        }
        Err(failure) => Err(server.tag(task_failed(server, method, &failure), method)),
    }
}

/// `grpc`: encode the request, pass the component's gate (admission,
/// shedding, deadline), call the remote client — which enforces the
/// method's timeout — and decode the reply. Dropping the returned future
/// resets the HTTP/2 stream.
#[cfg(feature = "grpc")]
async fn run_remote<Req, Rep>(
    server: &Server,
    client: Arc<RemoteClient>,
    method: usize,
    callee: CallContext,
    req: Req,
) -> Result<Rep, AppError>
where
    Req: WireMessage,
    Rep: WireMessage,
{
    let body = Bytes::from(req.encode_to_vec());
    // The tonic call and its retry loop are tens of kilobytes; boxed, they
    // do not inflate every handle future, local calls included.
    match server
        .run(method, callee, move |cx| async move {
            Box::pin(client.call(method, cx, body)).await
        })
        .await
    {
        Ok(Ok(reply)) => Rep::decode(reply).map_err(|_| server.tag(malformed_reply(), method)),
        Ok(Err(error)) | Err(error) => Err(error),
    }
}

fn malformed_reply() -> AppError {
    AppError::new(
        ErrorCode::Internal,
        "the component reply could not be decoded",
    )
    .with_reason(reasons::MALFORMED_REPLY)
}

/// The error for a serving task that did not return: a panic (whose payload
/// is never shown) or, during runtime shutdown, a cancellation.
fn task_failed(server: &Server, method: usize, failure: &JoinError) -> AppError {
    if !failure.is_panic() {
        return AppError::cancelled("the call was cancelled");
    }
    let descriptor = server.descriptor();
    let method = descriptor
        .methods()
        .get(method)
        .map_or("?", |method| method.name());
    AppError::new(
        ErrorCode::Internal,
        format!("component {} method {method} panicked", descriptor.name()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn the_callee_context_is_narrowed_and_trusted() {
        let deadline = (Instant::now() + Duration::from_secs(5)).into_std();
        let cx = CallContext::new()
            .with_request_id("req-1")
            .with_deadline(deadline);

        let narrowed = callee_context(&cx, Some(Instant::now() + Duration::from_secs(1)));
        assert_eq!(narrowed.request_id(), "req-1");
        assert_eq!(
            sekvent_resilience::remaining(&narrowed),
            Some(Duration::from_secs(1))
        );
        assert_eq!(
            narrowed.caller(),
            Some(&ServiceIdentity::trusted(LOCAL_CALLER))
        );

        let kept = callee_context(&cx, Some(Instant::now() + Duration::from_secs(9)));
        assert_eq!(kept.deadline(), Some(deadline));

        let unbounded = callee_context(&CallContext::new(), None);
        assert_eq!(unbounded.deadline(), None);

        cx.cancel_token().cancel();
        assert!(narrowed.cancel_token().is_cancelled());
    }

    #[test]
    fn the_callee_context_forwards_only_a_key_set_for_the_call() {
        let own = CallContext::new().with_idempotency_key("order-7");
        let callee = callee_context(&own, None);
        assert_eq!(callee.outbound_idempotency_key(), Some("order-7"));
        assert_eq!(
            callee.into_inbound().outbound_idempotency_key(),
            None,
            "the callee reads the key but does not pass it on"
        );

        let received = CallContext::new()
            .with_idempotency_key("upstream")
            .into_inbound();
        assert_eq!(callee_context(&received, None).idempotency_key(), None);
    }

    #[tokio::test(start_paused = true)]
    async fn bounds_tell_the_method_timeout_from_the_callers_deadline() {
        let far =
            CallContext::new().with_deadline((Instant::now() + Duration::from_secs(10)).into_std());
        let bounds = Bounds::new(&far, Some(Duration::from_secs(1)));
        assert!(!bounds.passed());
        assert!(!bounds.method_fired(&far));
        assert_eq!(bounds.effective(), bounds.method);
        let started = Instant::now();
        bounds.sleep().await;
        assert_eq!(started.elapsed(), Duration::from_secs(1));
        assert!(bounds.passed());
        assert!(bounds.method_fired(&far));
        let error = bounds.expired(&far, Some(Duration::from_secs(1)));
        assert_eq!(error.code(), ErrorCode::DeadlineExceeded);
        assert_eq!(error.reason(), Some(reasons::METHOD_TIMEOUT));
        assert_eq!(error.metadata()["timeout_ms"], "1000");

        let near =
            CallContext::new().with_deadline((Instant::now() + Duration::from_secs(1)).into_std());
        let bounds = Bounds::new(&near, Some(Duration::from_secs(5)));
        assert_eq!(bounds.effective(), bounds.caller);
        bounds.sleep().await;
        assert!(caller_expired(&near));
        assert!(!bounds.method_fired(&near));
        let error = bounds.expired(&near, Some(Duration::from_secs(5)));
        assert_eq!(error.code(), ErrorCode::DeadlineExceeded);
        assert_eq!(error.reason(), None, "the caller's own deadline passed");

        let unbounded = Bounds::new(&CallContext::new(), None);
        assert_eq!(unbounded.effective(), None);
        assert!(!unbounded.passed());
        assert!(!unbounded.method_fired(&CallContext::new()));
        let never = tokio::time::timeout(Duration::from_secs(60), unbounded.sleep()).await;
        assert!(never.is_err(), "no deadline never expires");
        assert_eq!(unbounded.expired(&CallContext::new(), None).reason(), None);
        let huge = Bounds::new(&CallContext::new(), Some(Duration::MAX));
        assert_eq!(huge.method, None);
    }

    #[test]
    fn only_plain_deadlines_are_reinterpreted() {
        assert!(is_plain_deadline(&AppError::deadline_exceeded("late")));
        assert!(!is_plain_deadline(&method_timeout(Duration::from_secs(1))));
        assert!(!is_plain_deadline(&AppError::unavailable("down")));
    }

    #[test]
    fn transient_failures_of_a_nested_call_name_the_callee() {
        let nested = CallContext::new().with_hops(1);
        for error in [
            AppError::unavailable("down").with_reason(reasons::UNREACHABLE),
            AppError::unavailable("open").with_reason(reasons::CIRCUIT_OPEN),
            AppError::resource_exhausted("full"),
            method_timeout(Duration::from_millis(250)),
        ] {
            let marked = mark_downstream(&nested, "inventory", error);
            assert_eq!(marked.metadata()[DOWNSTREAM], "inventory", "{marked}");
        }
        for error in [
            AppError::deadline_exceeded("the call deadline was exceeded"),
            AppError::cancelled("the call was cancelled"),
            AppError::not_found("no such item"),
            AppError::new(ErrorCode::Internal, "bug"),
        ] {
            let kept = mark_downstream(&nested, "inventory", error);
            assert!(!kept.metadata().contains_key(DOWNSTREAM), "{kept}");
        }

        let deeper = AppError::unavailable("down").with_metadata(DOWNSTREAM, "stock");
        let kept = mark_downstream(&nested, "inventory", deeper);
        assert_eq!(kept.metadata()[DOWNSTREAM], "stock", "the origin is kept");

        let edge = mark_downstream(
            &CallContext::new(),
            "inventory",
            AppError::unavailable("down"),
        );
        assert!(
            !edge.metadata().contains_key(DOWNSTREAM),
            "a call from the edge is retried by its own caller"
        );
    }

    #[test]
    fn the_depth_error() {
        let error = depth_exceeded(3);
        assert_eq!(error.code(), ErrorCode::FailedPrecondition);
        assert_eq!(error.reason(), Some(reasons::CALL_DEPTH_EXCEEDED));
        assert!(error.message().contains('3'), "{error}");
    }

    #[test]
    fn malformed_reply_is_internal() {
        let error = malformed_reply();
        assert_eq!(error.code(), ErrorCode::Internal);
        assert_eq!(error.reason(), Some(reasons::MALFORMED_REPLY));
    }
}
