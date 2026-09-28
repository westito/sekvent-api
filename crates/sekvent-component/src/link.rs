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
use sekvent_resilience::Timeout;
use tokio::task::JoinError;
use tracing::Instrument as _;

use crate::__private::{Dispatch, LocalMessage, WireMessage};
use crate::server::{Server, shed};
use crate::wire::{self, AbortOnDrop};
use crate::{Binding, ComponentError, LOCAL_CALLER, reasons};

/// The caller's view of one installed component: its binding and the
/// serving side the calls reach.
#[derive(Debug)]
pub(crate) struct Link {
    binding: Binding,
    server: Arc<Server>,
}

impl Link {
    pub(crate) fn new(binding: Binding, server: Arc<Server>) -> Self {
        Self { binding, server }
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
}

impl<D: ?Sized> Clone for Route<D> {
    fn clone(&self) -> Self {
        match self {
            Self::Local(imp) => Self::Local(Arc::clone(imp)),
            Self::Serialized(dispatch) => Self::Serialized(Arc::clone(dispatch)),
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
    /// of its own under `local-serialized`.
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
            let server = &self.link.server;
            let outcome = match &self.route {
                Route::Local(imp) => {
                    let imp = Arc::clone(imp);
                    invoke(server, method, cx, move |callee| {
                        run_local(server, method, callee, imp, req, local)
                    })
                    .await
                }
                Route::Serialized(dispatch) => {
                    let dispatch = Arc::clone(dispatch);
                    invoke(server, method, cx, move |callee| {
                        run_serialized::<Req, Rep>(server, dispatch, method, callee, req)
                    })
                    .await
                }
            };
            outcome.map_err(E::from_app_error)
        }
    }

    /// Call method `method` of a `local_only` component: always the local
    /// pipeline. A serialized route cannot exist after a successful build;
    /// it would yield `INTERNAL`, never a panic.
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
            let server = &self.link.server;
            let outcome = match &self.route {
                Route::Local(imp) => {
                    let imp = Arc::clone(imp);
                    invoke(server, method, cx, move |callee| {
                        run_local(server, method, callee, imp, req, local)
                    })
                    .await
                }
                Route::Serialized(_) => Err(server.tag(
                    AppError::new(
                        ErrorCode::Internal,
                        format!(
                            "component {} takes plain Rust values and cannot be called \
                             across a serialization boundary",
                            server.descriptor().name()
                        ),
                    ),
                    method,
                )),
            };
            outcome.map_err(E::from_app_error)
        }
    }
}

/// The caller side shared by both bindings: shed a dead call, derive the
/// callee's context, and run `transport` within the callee's deadline and
/// until the caller cancels.
async fn invoke<Rep, T, Fut>(
    server: &Server,
    method: usize,
    cx: &CallContext,
    transport: T,
) -> Result<Rep, AppError>
where
    T: FnOnce(CallContext) -> Fut,
    Fut: Future<Output = Result<Rep, AppError>>,
{
    shed(cx).map_err(|error| server.tag(error, method))?;
    let callee = callee_context(cx, server.policy(method).timeout);
    let sent = callee.clone();
    let limit = Timeout::deadline_only();
    let run = limit.call(&callee, async move { Ok(transport(sent).await) });
    tokio::select! {
        biased;
        () = cx.cancelled() => Err(server.tag(AppError::cancelled("the call was cancelled"), method)),
        outcome = run => match outcome {
            Ok(result) => result,
            Err(expired) => Err(server.tag(expired, method)),
        },
    }
}

/// The context the callee sees: the caller's identity and trace, a child
/// cancellation token, the in-process caller identity, and the deadline
/// narrowed by the method's timeout (measured on tokio's clock).
fn callee_context(cx: &CallContext, timeout: Option<Duration>) -> CallContext {
    let callee = cx
        .child()
        .with_caller(ServiceIdentity::trusted(LOCAL_CALLER));
    match timeout.and_then(|timeout| tokio::time::Instant::now().checked_add(timeout)) {
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
        .run(method, callee, move |cx| local(imp, cx, req))
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
    let serving = Arc::clone(server);
    let task = tokio::spawn(
        async move {
            let mut cx =
                headers::from_headers(&wire_headers, Some(ServiceIdentity::trusted(LOCAL_CALLER)));
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
        let deadline = (tokio::time::Instant::now() + Duration::from_secs(5)).into_std();
        let cx = CallContext::new()
            .with_request_id("req-1")
            .with_deadline(deadline);

        let narrowed = callee_context(&cx, Some(Duration::from_secs(1)));
        assert_eq!(narrowed.request_id(), "req-1");
        assert_eq!(
            sekvent_resilience::remaining(&narrowed),
            Some(Duration::from_secs(1))
        );
        assert_eq!(
            narrowed.caller(),
            Some(&ServiceIdentity::trusted(LOCAL_CALLER))
        );

        let kept = callee_context(&cx, Some(Duration::from_secs(9)));
        assert_eq!(kept.deadline(), Some(deadline));

        let unbounded = callee_context(&CallContext::new(), None);
        assert_eq!(unbounded.deadline(), None);
        let huge = callee_context(&CallContext::new(), Some(Duration::MAX));
        assert_eq!(huge.deadline(), None);

        cx.cancel_token().cancel();
        assert!(narrowed.cancel_token().is_cancelled());
    }

    #[test]
    fn malformed_reply_is_internal() {
        let error = malformed_reply();
        assert_eq!(error.code(), ErrorCode::Internal);
        assert_eq!(error.reason(), Some(reasons::MALFORMED_REPLY));
    }
}
