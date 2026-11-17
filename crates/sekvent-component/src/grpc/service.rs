//! The serving side of gRPC: one generic service per exposed component,
//! routed by its service name, answering through the component's gate and
//! byte-level dispatcher.
//!
//! Everything that decides whether a call runs (authentication, routing,
//! the context, the hop limit, the deadline) is read from the request's
//! HTTP headers; request trailers never reach the call context.
//!
//! The order is fixed. The checks that need the head alone come first and
//! cost nothing shared: the link token, the route, the context, the
//! address-keyed client limit, the hop limit and the App's end-user
//! precheck. A call no link token authenticated then takes a slot of the
//! App's one admission, shared by every component it serves, and keeps it
//! until the call reaches the gate. Next the request is read whole,
//! bounded by the largest message, the App's read timeout and the call's
//! own deadline, so nothing after it waits on the client. Then the request
//! scope, the end user, the end-user-keyed client limit and the call
//! authorizer, then the gate and the handler with the buffered request.
//!
//! Callers authenticate as a link (an inbound token), as an end user (the
//! App's end-user authenticator, which sees the request head), or either,
//! by the component's `SERVE_AUTH`. A link token is checked first; an
//! anonymous method skips the end-user precheck and authenticator but
//! never the link check, and an optional-end-user method skips them only
//! for a request without an `Authorization` header.
//!
//! A method's per-client limit (`CLIENT_*`) runs once the route and the
//! context are known: keyed by the client address, before anything that
//! costs, so a flood is refused cheaply; keyed by the end user, right after
//! the authenticator. A refused call never reaches the handler.
//!
//! The App's scope resolver runs for every call no link token authenticated,
//! once its request has been read and before the end-user authenticator:
//! its scope goes into the head's extensions (where the authenticator finds
//! it) and into the context. When the scope names a tenant, a user who
//! does not name the same one is refused like a bad credential. The call's
//! cancellation guard exists from the request read on, and an expired call
//! never reaches the resolver.
//!
//! The App's call authorizer runs last, with the context the handler will
//! see; its error is the answer, unchanged, and its panic is answered like
//! a handler's.
//!
//! A streaming method's `TIMEOUT` does not narrow the context: it bounds
//! the open (the request, the end user, the handler's open) together with
//! the caller's deadline. The response headers go out once the stream is
//! open, each item is one gRPC message (at most 4 MiB), and the trailers
//! carry the end. The response carries a [`MessageCount`] for the access
//! log.

use std::collections::HashMap;
use std::convert::Infallible;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;

use bytes::{Bytes, BytesMut};
use futures::StreamExt as _;
use sekvent_context::{CallContext, ClientAddr, RequestScope, ServiceIdentity, headers};
use sekvent_error::{AppError, ErrorCode};
use sekvent_link::TokenMap;
use sekvent_resilience::{ClientKey, RateKey};
use sekvent_telemetry::access_log::MessageCount;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio::time::Instant;
use tonic::body::Body;
use tonic::server::Grpc;
use tonic::service::Routes;
use tracing::Instrument as _;

use super::codec::BytesCodec;
use crate::__private::{BoxFuture, Dispatch};
use crate::end_user::{BearerShape, EndUserAuthenticator, EndUserPrecheck, rejection};
use crate::link::{DOWNSTREAM, depth_exceeded};
use crate::scope::{ScopeResolver, admit_end_user};
use crate::server::{Server, earliest, tag};
use crate::stream::CallStream;
use crate::wire::{CatchPanic, CatchPanicStream, Panicked};
use crate::{Binding, CallTarget, ComponentDescriptor, ServeAdmission, reasons};

/// The largest item a served stream sends (4 MiB); a larger one ends the
/// stream with `OUT_OF_RANGE`. Requests are bounded by the App's
/// [`ServeAdmission`], whose largest message is the same size.
pub(crate) const MAX_MESSAGE_SIZE: usize = ServeAdmission::MAX_MESSAGE_BYTES;

/// The bytes a gRPC frame adds to its message: a flag and a length.
const FRAME_HEADER: usize = 5;

/// The admission every component of one App serves under: the slots of
/// the calls no link token authenticated between their admission and the
/// gate, and the bounds on reading a request.
#[derive(Debug)]
pub(crate) struct Admission {
    slots: Arc<Semaphore>,
    clients: Mutex<HashMap<RateKey, usize>>,
    max_pending_per_client: usize,
    read_timeout: Duration,
    max_message_bytes: usize,
}

impl Admission {
    /// Slots and bounds as `bounds` says; the builder validated them.
    pub(crate) fn new(bounds: ServeAdmission) -> Self {
        Self {
            slots: Arc::new(Semaphore::new(bounds.max_pending())),
            clients: Mutex::new(HashMap::new()),
            max_pending_per_client: bounds.max_pending_per_client(),
            read_timeout: bounds.read_timeout(),
            max_message_bytes: bounds.max_message_bytes(),
        }
    }

    /// The most bytes a request body may carry: one frame of the largest
    /// message.
    fn max_body_bytes(&self) -> usize {
        self.max_message_bytes.saturating_add(FRAME_HEADER)
    }
}

#[derive(Debug)]
struct AdmissionPermit {
    admission: Arc<Admission>,
    key: RateKey,
    _shared: OwnedSemaphorePermit,
}

impl Drop for AdmissionPermit {
    fn drop(&mut self) {
        let mut clients = self
            .admission
            .clients
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(count) = clients.get_mut(&self.key) {
            *count -= 1;
            if *count == 0 {
                clients.remove(&self.key);
            }
        }
    }
}

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
    /// The App's end-user precheck, [`BearerShape`] unless it registered
    /// one; asked only where the authenticator will run.
    precheck: Arc<dyn EndUserPrecheck>,
    /// The App's scope resolver, whatever `SERVE_AUTH` says.
    scope: Option<Arc<dyn ScopeResolver>>,
    /// The App's admission, shared by every component it serves.
    admission: Arc<Admission>,
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
            .field("admission", &self.admission)
            .field("max_hops", &self.max_hops)
            .finish_non_exhaustive()
    }
}

impl Served {
    #[cfg(test)]
    pub(crate) fn new(
        server: Arc<Server>,
        dispatch: Arc<dyn Dispatch>,
        inbound: Option<Arc<TokenMap>>,
        max_hops: u32,
    ) -> Self {
        Self::with_shared_admission(
            server,
            dispatch,
            inbound,
            max_hops,
            Arc::new(Admission::new(ServeAdmission::new())),
        )
    }

    pub(crate) fn with_shared_admission(
        server: Arc<Server>,
        dispatch: Arc<dyn Dispatch>,
        inbound: Option<Arc<TokenMap>>,
        max_hops: u32,
        admission: Arc<Admission>,
    ) -> Self {
        let prefix = format!("/{}/", super::service_name(server.descriptor()));
        Self {
            server,
            dispatch,
            inbound,
            end_user: None,
            precheck: Arc::new(BearerShape),
            scope: None,
            admission,
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

    /// Check end-user credentials with `precheck` before admission;
    /// [`BearerShape`] without one.
    pub(crate) fn with_precheck(mut self, precheck: Option<Arc<dyn EndUserPrecheck>>) -> Self {
        self.precheck = precheck.unwrap_or_else(|| Arc::new(BearerShape));
        self
    }

    /// Resolve the request scope of calls no link authenticated with
    /// `resolver`.
    pub(crate) fn with_scope_resolver(mut self, resolver: Option<Arc<dyn ScopeResolver>>) -> Self {
        self.scope = resolver;
        self
    }

    /// Admit calls through `admission`, which the App's other served
    /// components share.
    #[cfg(test)]
    pub(crate) fn with_admission(mut self, admission: Arc<Admission>) -> Self {
        self.admission = admission;
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
            Err(Panicked) => Err(self.hook_panicked(method, "scope resolver")),
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
            Err(Panicked) => Err(self.hook_panicked(method, "end-user authenticator")),
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

    /// The App's end-user precheck on the request head, its panic caught;
    /// its error is answered like the authenticator's.
    fn precheck(&self, method: usize, head: &http::request::Parts) -> Result<(), AppError> {
        let descriptor = self.descriptor();
        let checked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let target = CallTarget::new(descriptor, method).ok_or_else(|| {
                AppError::new(ErrorCode::Internal, "the routed method does not exist")
            })?;
            self.precheck.precheck(head, target)
        }));
        match checked {
            Ok(Ok(())) => Ok(()),
            Ok(Err(error)) => Err(rejection(error)),
            Err(_payload) => Err(self.hook_panicked(method, "end-user precheck")),
        }
    }

    /// A slot of the App's admission, for a call no link token
    /// authenticated; `RESOURCE_EXHAUSTED` at once when none is free.
    fn admit(&self, method: usize, cx: &CallContext) -> Result<AdmissionPermit, AppError> {
        let refused = || {
            tag(
                self.descriptor(),
                AppError::resource_exhausted(
                    "too many calls are waiting to be admitted; try again later",
                )
                .with_reason(reasons::ADMISSION_FULL),
                method,
            )
        };
        let shared = Arc::clone(&self.admission.slots)
            .try_acquire_owned()
            .map_err(|_| refused())?;
        let key = ClientKey::ClientAddr.key_of(cx);
        let mut clients = self
            .admission
            .clients
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let count = clients.entry(key.clone()).or_default();
        if *count >= self.admission.max_pending_per_client {
            return Err(refused());
        }
        *count += 1;
        Ok(AdmissionPermit {
            admission: Arc::clone(&self.admission),
            key,
            _shared: shared,
        })
    }

    /// Read the request body whole, within the App's read timeout and the
    /// call's own bound (its deadline, and a stream's `open_deadline`),
    /// refusing it as soon as it is larger than one frame of the largest
    /// message. Never starts once the call's bound has passed. A failure of
    /// the body itself is answered as tonic answers it.
    async fn read_request<B>(
        &self,
        method: usize,
        cx: &CallContext,
        open_deadline: Option<Instant>,
        body: B,
    ) -> Result<Bytes, tonic::Status>
    where
        B: http_body::Body<Data = Bytes>,
        B::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
    {
        let descriptor = self.descriptor();
        let call = open_bound(cx, open_deadline);
        let now = Instant::now();
        if call.is_some_and(|bound| bound <= now) {
            return Err(deadline_exceeded(descriptor, method).into());
        }
        let read = now
            .checked_add(self.admission.read_timeout)
            .ok_or_else(|| self.read_timed_out(method))?;
        // On a tie the call's own deadline is the answer.
        let read_first = call.is_none_or(|call| read < call);
        let reading = read_body(body, self.admission.max_body_bytes());
        let outcome = match earliest(call, Some(read)) {
            Some(bound) => tokio::time::timeout_at(bound, reading)
                .await
                .map_err(|_elapsed| {
                    if read_first {
                        self.read_timed_out(method)
                    } else {
                        deadline_exceeded(descriptor, method).into()
                    }
                })?,
            None => reading.await,
        };
        outcome.map_err(|unread| match unread {
            Unread::TooLarge => tag(
                descriptor,
                AppError::new(
                    ErrorCode::OutOfRange,
                    format!(
                        "the request message is larger than {} bytes",
                        self.admission.max_message_bytes
                    ),
                )
                .with_reason(reasons::REQUEST_TOO_LARGE)
                .with_metadata("max_bytes", self.admission.max_message_bytes.to_string()),
                method,
            )
            .into(),
            Unread::Failed(status) => status,
        })
    }

    /// `DEADLINE_EXCEEDED` / [`reasons::REQUEST_READ_TIMEOUT`].
    fn read_timed_out(&self, method: usize) -> tonic::Status {
        let millis = self.admission.read_timeout.as_millis();
        tag(
            self.descriptor(),
            AppError::deadline_exceeded(format!(
                "the request did not arrive within the read timeout of {millis} ms"
            ))
            .with_reason(reasons::REQUEST_READ_TIMEOUT)
            .with_metadata("timeout_ms", millis.to_string()),
            method,
        )
        .into()
    }

    /// The hop limit, checked from the head before end-user authentication.
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
        .unwrap_or_else(|_payload| Err(self.hook_panicked(method, "call authorizer")))
    }

    fn hook_panicked(&self, method: usize, hook: &str) -> AppError {
        tag(
            self.descriptor(),
            AppError::new(
                ErrorCode::Internal,
                format!(
                    "the {hook} of component {} panicked",
                    self.descriptor().name()
                ),
            )
            .with_reason(reasons::HANDLER_PANICKED),
            method,
        )
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

/// Why a request body was not read whole.
enum Unread {
    /// It carries more than the limit.
    TooLarge,
    /// The body itself failed, answered as tonic answers it.
    Failed(tonic::Status),
}

/// Read decoded transport bytes, bounded independently of wire size hints.
/// Tonic remains responsible for flags, compression and unary framing.
async fn read_body<B>(body: B, limit: usize) -> Result<Bytes, Unread>
where
    B: http_body::Body<Data = Bytes>,
    B::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
{
    let mut body = std::pin::pin!(body);
    let mut bytes = BytesMut::with_capacity(FRAME_HEADER);
    let mut declared = false;
    while let Some(frame) = std::future::poll_fn(|cx| body.as_mut().poll_frame(cx)).await {
        let frame =
            frame.map_err(|error| Unread::Failed(tonic::Status::from_error(error.into())))?;
        let Ok(mut data) = frame.into_data() else {
            continue;
        };
        if bytes.len().saturating_add(data.len()) > limit {
            return Err(Unread::TooLarge);
        }
        if !declared {
            let take = (FRAME_HEADER - bytes.len()).min(data.len());
            bytes.extend_from_slice(&data.split_to(take));
            if bytes.len() == FRAME_HEADER {
                let length = u32::from_be_bytes([bytes[1], bytes[2], bytes[3], bytes[4]]) as usize;
                if length > limit - FRAME_HEADER {
                    return Err(Unread::TooLarge);
                }
                bytes.reserve(length);
                declared = true;
            }
        }
        bytes.extend_from_slice(&data);
    }
    Ok(bytes.freeze())
}

/// A request body read whole, handed to tonic as one frame.
struct Buffered(Option<Bytes>);

impl http_body::Body for Buffered {
    type Data = Bytes;
    type Error = Infallible;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
    ) -> Poll<Option<Result<http_body::Frame<Bytes>, Infallible>>> {
        Poll::Ready(self.0.take().map(|data| Ok(http_body::Frame::data(data))))
    }
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
                if let Err(error) = served
                    .check_client(method, &cx, false)
                    .and_then(|()| served.check_hops(method, &cx))
                {
                    return Ok(error_response(error));
                }
                let (mut head, body) = request.into_parts();
                if end_user && let Err(error) = served.precheck(method, &head) {
                    return Ok(error_response(error));
                }
                // Held until the call reaches the gate: nobody vouches for
                // the caller yet, and what follows costs.
                let admitted = if cx.caller().is_none() {
                    match served.admit(method, &cx) {
                        Ok(slot) => Some(slot),
                        Err(error) => return Ok(error_response(error)),
                    }
                } else {
                    None
                };
                // Cancels the call's token when the client resets the stream
                // and this future is dropped, or when the deadline passes,
                // from the request read on: work the scope resolver spawned
                // with the token ends with the call.
                let guard = cx.cancel_token().clone().drop_guard();
                let message = match served.read_request(method, &cx, open_deadline, body).await {
                    Ok(message) => message,
                    Err(status) => return Ok(status.into_http()),
                };
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
                    .and_then(|()| served.check_authorized(method, &cx))
                {
                    return Ok(error_response(error));
                }
                drop(admitted);
                let request = http::Request::from_parts(head, Buffered(Some(message)));
                if descriptor.methods()[method].is_stream() {
                    return Ok(
                        serve_stream(served, method, cx, open_deadline, request, guard).await,
                    );
                }
                let remaining = sekvent_resilience::remaining(&cx);
                let mut grpc = Grpc::new(BytesCodec)
                    .max_decoding_message_size(served.admission.max_message_bytes);
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
        .max_decoding_message_size(served.admission.max_message_bytes)
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
        assert!(debug.contains("max_message_bytes: 4194304"), "{debug}");
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

    /// `prepare`, the precheck and the end user when `prepare` asks for
    /// one, and the scope when no link authenticated the call: the serving
    /// order up to the end user, without the admission and the request.
    async fn admit(
        served: &Served,
        path: &str,
        wire: http::HeaderMap,
    ) -> Result<Prepared, AppError> {
        let mut prepared = served.prepare(path, &wire, None)?;
        let mut head = head_at(path, wire);
        if prepared.end_user {
            served.precheck(prepared.method, &head)?;
        }
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

    /// The `grpc-status` of a call `served` refuses: a request to `path`
    /// with `wire` headers and `body`.
    async fn refused<B>(served: &Arc<Served>, path: &str, wire: http::HeaderMap, body: B) -> String
    where
        B: http_body::Body<Data = Bytes> + Send + 'static,
        B::Error: Into<Box<dyn std::error::Error + Send + Sync>> + Send,
    {
        let mut request = http::Request::new(body);
        *request.uri_mut() = path.parse().unwrap();
        *request.headers_mut() = wire;
        let call = tower::Service::call(&mut ComponentService(Arc::clone(served)), request);
        let response = tokio::time::timeout(Duration::from_secs(30), call)
            .await
            .expect("answered")
            .unwrap();
        response.headers()["grpc-status"]
            .to_str()
            .unwrap()
            .to_owned()
    }

    /// The `grpc-status` `served` answers a request to `path` with `wire`
    /// headers and an empty message, when it refuses the call.
    async fn status_of(served: &Arc<Served>, path: &str, wire: http::HeaderMap) -> String {
        refused(served, path, wire, framed(b"")).await
    }

    /// The `grpc-status` `served` answers a request to `path` with `wire`
    /// headers and a body that never arrives: answered at all only when
    /// the call is refused before its request is read.
    async fn status_unread(served: &Arc<Served>, path: &str, wire: http::HeaderMap) -> String {
        refused(served, path, wire, NeverArrives).await
    }

    #[tokio::test]
    async fn the_call_authorizer_runs_after_authentication_and_before_the_handler() {
        let (authorizer, log) = recording();
        let served = Arc::new(guarded(true, 1, authorizer));

        // An end user refused by role: the handler never runs.
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

        // A rejected end user, an unknown path and the hop limit never ask;
        // the last two are answered from the head, without the request.
        assert_eq!(status_of(&served, RESERVE, bearer("forged")).await, "16");
        assert_eq!(
            status_unread(&served, "/shop.v1.Inventory/Stock", bearer("user-7")).await,
            "12"
        );
        let mut deep = bearer(TOKEN);
        deep.insert(headers::HOPS, "2".parse().unwrap());
        assert_eq!(status_unread(&served, RESERVE, deep).await, "9");
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
        precheck: Option<Arc<dyn EndUserPrecheck>>,
        /// Shared with other served components; the defaults without one.
        admission: Option<Arc<Admission>>,
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
            let served = Served::new(
                Arc::new(gate),
                Arc::new(Probed(Arc::clone(probe))),
                inbound,
                16,
            )
            .with_end_user(sessions)
            .with_precheck(self.precheck)
            .with_scope_resolver(Some(resolver));
            Arc::new(match self.admission {
                Some(admission) => served.with_admission(admission),
                None => served,
            })
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
            status_unread(&served, "/shop.v1.Inventory/Stock", stock).await,
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

        // The handler never runs.
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
        assert_eq!(status_unread(&inventory, RELEASE, wire).await, "4");
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

    /// One gRPC frame of `message`.
    fn frame(message: &[u8]) -> Bytes {
        let mut frame = vec![0_u8];
        frame.extend_from_slice(&u32::try_from(message.len()).unwrap().to_be_bytes());
        frame.extend_from_slice(message);
        Bytes::from(frame)
    }

    /// A request body: its chunks one by one, then a trailers frame when
    /// `trailers`, then its end, or nothing ever when `stall`; `size` is
    /// its size hint.
    #[derive(Default)]
    struct Chunks {
        queue: std::collections::VecDeque<Bytes>,
        trailers: bool,
        stall: bool,
        size: Option<u64>,
    }

    impl Chunks {
        fn of(chunks: impl IntoIterator<Item = Bytes>) -> Self {
            Self {
                queue: chunks.into_iter().collect(),
                ..Self::default()
            }
        }

        fn stalled(mut self) -> Self {
            self.stall = true;
            self
        }
    }

    impl http_body::Body for Chunks {
        type Data = Bytes;
        type Error = Infallible;

        fn poll_frame(
            mut self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
        ) -> Poll<Option<Result<http_body::Frame<Bytes>, Infallible>>> {
            if let Some(chunk) = self.queue.pop_front() {
                return Poll::Ready(Some(Ok(http_body::Frame::data(chunk))));
            }
            if std::mem::take(&mut self.trailers) {
                let trailers = http::HeaderMap::new();
                return Poll::Ready(Some(Ok(http_body::Frame::trailers(trailers))));
            }
            if self.stall {
                Poll::Pending
            } else {
                Poll::Ready(None)
            }
        }

        fn size_hint(&self) -> http_body::SizeHint {
            self.size.map_or_else(
                http_body::SizeHint::default,
                http_body::SizeHint::with_exact,
            )
        }
    }

    /// A request body that fails as a broken gRPC-Web body does.
    struct Broken;

    impl http_body::Body for Broken {
        type Data = Bytes;
        type Error = tonic::Status;

        fn poll_frame(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
        ) -> Poll<Option<Result<http_body::Frame<Bytes>, tonic::Status>>> {
            Poll::Ready(Some(Err(tonic::Status::internal("garbled frame"))))
        }
    }

    /// The error of a trailers-only answer.
    fn refusal(response: &http::Response<Body>) -> AppError {
        AppError::from(
            tonic::Status::from_header_map(response.headers()).expect("a trailers-only answer"),
        )
    }

    /// `Authorization` headers with these values, in order.
    fn authorization(values: &[&str]) -> http::HeaderMap {
        let mut wire = http::HeaderMap::new();
        for value in values {
            wire.append(http::header::AUTHORIZATION, value.parse().unwrap());
        }
        wire
    }

    /// An admission of `slots` slots, the other bounds as given.
    fn slots(slots: usize, bounds: ServeAdmission) -> Arc<Admission> {
        Arc::new(Admission::new(bounds.with_max_pending(slots)))
    }

    #[tokio::test]
    async fn a_call_without_a_well_formed_bearer_never_reaches_the_resolver() {
        let (hosts, probe) = fixtures();
        let admission = slots(1, ServeAdmission::new());
        let served = Setup {
            bearer: true,
            admission: Some(Arc::clone(&admission)),
            ..Setup::default()
        }
        .serve(INVENTORY, &hosts, &probe);

        // A required end user: no header, another scheme, no token, two
        // headers. Answered from the head, the request never read.
        let malformed = [
            authorization(&["Basic dXNlcg=="]),
            authorization(&["Bearer "]),
            authorization(&["Bearer user-1", "Bearer user-2"]),
        ];
        for wire in std::iter::once(http::HeaderMap::new()).chain(malformed.clone()) {
            assert_eq!(
                status_unread(&served, RESERVE, hosted("a.test", wire.clone())).await,
                "16",
                "{wire:?}"
            );
        }
        // An optional end user with a malformed header: refused the same way.
        for wire in malformed.clone() {
            assert_eq!(
                status_unread(&served, TRACK, hosted("a.test", wire.clone())).await,
                "16",
                "{wire:?}"
            );
        }
        let response = respond(
            &served,
            request(
                RESERVE,
                hosted("a.test", http::HeaderMap::new()),
                NeverArrives,
            ),
        )
        .await;
        let error = refusal(&response);
        assert_eq!(error.message(), END_USER_REJECTED_MESSAGE);
        assert_eq!(error.reason(), None);
        assert_eq!(hosts.seen(), 0, "the resolver was never asked");
        assert_eq!(admission.slots.available_permits(), 1, "no slot was taken");

        // A well-formed token the authenticator does not know is resolved
        // first, then refused.
        assert_eq!(
            status_of(&served, RESERVE, hosted("a.test", bearer("forged"))).await,
            "16"
        );
        assert_eq!(hosts.seen(), 1);

        // Anonymous and optional methods without a header go on to the
        // resolver and the handler; an anonymous one ignores a bad header.
        for (path, wire) in [
            (RELEASE, http::HeaderMap::new()),
            (TRACK, http::HeaderMap::new()),
            (RELEASE, malformed[0].clone()),
        ] {
            let response =
                respond(&served, request(path, hosted("a.test", wire), framed(b""))).await;
            assert_eq!(early_status(&response), None, "{path}");
        }
        assert_eq!(hosts.seen(), 4);
        assert_eq!(probe.saw().len(), 3);
        assert_eq!(admission.slots.available_permits(), 1);
    }

    #[tokio::test]
    async fn a_registered_precheck_decides_and_sees_the_target() {
        let (hosts, probe) = fixtures();
        let asked = Arc::new(std::sync::Mutex::new(Vec::new()));
        let log = Arc::clone(&asked);
        let precheck =
            move |request: &http::request::Parts, target: CallTarget| -> Result<(), AppError> {
                log.lock().unwrap().push(target.method().name());
                match request
                    .headers
                    .get("x-api-key")
                    .and_then(|value| value.to_str().ok())
                {
                    Some("key-1") => Ok(()),
                    Some("down") => Err(AppError::unavailable("keys are down")),
                    Some("boom") => panic!("secret key"),
                    _ => Err(crate::EndUserRejection::new("API_KEY_REQUIRED").into()),
                }
            };
        let served = Setup {
            bearer: true,
            precheck: Some(Arc::new(precheck)),
            ..Setup::default()
        }
        .serve(INVENTORY, &hosts, &probe);
        let keyed = |key: &str| {
            let mut wire = hosted("a.test", bearer("user-1"));
            wire.insert("x-api-key", key.parse().unwrap());
            wire
        };

        // It replaces the bearer shape: what it accepts reaches the
        // authenticator, which still decides.
        let response = respond(&served, request(RESERVE, keyed("key-1"), framed(b""))).await;
        assert_eq!(early_status(&response), None);

        let response = respond(
            &served,
            request(RESERVE, hosted("a.test", bearer("user-1")), NeverArrives),
        )
        .await;
        let error = refusal(&response);
        assert_eq!(error.code(), ErrorCode::Unauthenticated);
        assert_eq!(error.message(), END_USER_REJECTED_MESSAGE);
        assert_eq!(error.reason(), Some("API_KEY_REQUIRED"));

        let error = refusal(&respond(&served, request(RESERVE, keyed("down"), NeverArrives)).await);
        assert_eq!(error.code(), ErrorCode::Unavailable);
        assert_eq!(error.message(), "keys are down");

        let error = refusal(&respond(&served, request(RESERVE, keyed("boom"), NeverArrives)).await);
        assert_eq!(error.code(), ErrorCode::Internal);
        assert_eq!(error.reason(), Some(reasons::HANDLER_PANICKED));
        assert_eq!(
            error.message(),
            "the end-user precheck of component inventory panicked"
        );
        assert_eq!(error.metadata()["method"], "reserve");

        // Never asked for an anonymous method.
        let response = respond(
            &served,
            request(
                RELEASE,
                hosted("a.test", http::HeaderMap::new()),
                framed(b""),
            ),
        )
        .await;
        assert_eq!(early_status(&response), None);
        assert_eq!(asked.lock().unwrap().clone(), ["reserve"; 4]);
        assert_eq!(hosts.seen(), 2, "only the calls it let through");

        // A method the component does not have is refused, never waved on.
        let error = served
            .precheck(9, &head_at(RESERVE, keyed("key-1")))
            .unwrap_err();
        assert_eq!(error.code(), ErrorCode::Internal);
    }

    #[tokio::test(start_paused = true)]
    async fn an_incomplete_request_never_reaches_the_resolver_and_ends_at_the_read_timeout() {
        let (hosts, probe) = fixtures();
        let admission = slots(
            1,
            ServeAdmission::new().with_read_timeout(Duration::from_millis(50)),
        );
        let bounded = |timeout| Setup {
            timeout,
            admission: Some(Arc::clone(&admission)),
            ..Setup::default()
        };
        let inventory = bounded(None).serve(INVENTORY, &hosts, &probe);
        let watcher = bounded(None).serve(WATCHER, &hosts, &probe);
        for (served, path, method) in [(&inventory, RELEASE, "release"), (&watcher, WATCH, "watch")]
        {
            let started = Instant::now();
            // The start of a frame, then nothing.
            let body = Chunks::of([frame(b"partial").slice(..6)]).stalled();
            let wire = hosted("a.test", http::HeaderMap::new());
            let error = refusal(&respond(served, request(path, wire, body)).await);
            assert_eq!(error.code(), ErrorCode::DeadlineExceeded, "{path}");
            assert_eq!(error.reason(), Some(reasons::REQUEST_READ_TIMEOUT));
            assert_eq!(
                error.message(),
                "the request did not arrive within the read timeout of 50 ms"
            );
            assert_eq!(error.metadata()["timeout_ms"], "50");
            assert_eq!(error.metadata()["method"], method);
            assert!(started.elapsed() >= Duration::from_millis(50));
            assert_eq!(admission.slots.available_permits(), 1, "{path}");
        }
        assert_eq!(hosts.seen(), 0, "the resolver was never asked");
        assert_eq!(probe.saw(), [] as [Saw; 0]);

        // The call's own deadline, when it comes first, is the answer.
        let deadline = bounded(Some(Duration::from_millis(20))).serve(INVENTORY, &hosts, &probe);
        let started = Instant::now();
        let wire = hosted("a.test", http::HeaderMap::new());
        let error = refusal(&respond(&deadline, request(RELEASE, wire, NeverArrives)).await);
        assert_eq!(error.code(), ErrorCode::DeadlineExceeded);
        assert_eq!(error.reason(), None);
        assert_eq!(error.message(), "the call deadline was exceeded");
        assert!(started.elapsed() >= Duration::from_millis(20));
        assert!(started.elapsed() < Duration::from_millis(50));
        assert_eq!(hosts.seen(), 0);
    }

    #[tokio::test]
    async fn the_admission_refuses_one_call_too_many_across_services() {
        let (hosts, probe) = fixtures();
        let admission = slots(1, ServeAdmission::new());
        let inventory = Setup {
            link: true,
            bearer: true,
            admission: Some(Arc::clone(&admission)),
            ..Setup::default()
        }
        .serve(INVENTORY, &hosts, &probe);
        let watcher = Setup {
            bearer: true,
            admission: Some(Arc::clone(&admission)),
            ..Setup::default()
        }
        .serve(WATCHER, &hosts, &probe);

        // A call whose resolution never ends holds the only slot.
        let mut held = tower::Service::call(
            &mut ComponentService(Arc::clone(&inventory)),
            request(
                RELEASE,
                hosted("hang.test", http::HeaderMap::new()),
                framed(b""),
            ),
        );
        assert!(futures::poll!(&mut held).is_pending());
        assert_eq!(hosts.seen(), 1);
        assert_eq!(admission.slots.available_permits(), 0);

        // The next one, to another service of the same App, is refused
        // before its request is read or its scope resolved.
        let wire = hosted("a.test", bearer("user-1"));
        let error = refusal(&respond(&watcher, request(WATCH, wire, NeverArrives)).await);
        assert_eq!(error.code(), ErrorCode::ResourceExhausted);
        assert_eq!(error.reason(), Some(reasons::ADMISSION_FULL));
        assert_eq!(error.metadata()["component"], "watcher");
        assert_eq!(error.metadata()["method"], "watch");
        assert_eq!(hosts.seen(), 1);

        // A link caller takes no slot.
        let wire = hosted("a.test", bearer(TOKEN));
        let response = respond(&inventory, request(RESERVE, wire, framed(b""))).await;
        assert_eq!(early_status(&response), None, "served");

        // Once the held call goes away, its slot is free; an open stream
        // holds none.
        drop(held);
        assert_eq!(admission.slots.available_permits(), 1);
        let wire = hosted("a.test", bearer("user-1"));
        let response = respond(&watcher, request(WATCH, wire, framed(b""))).await;
        assert_eq!(early_status(&response), None, "the stream opened");
        assert_eq!(admission.slots.available_permits(), 1);
        drop(response);
        assert_eq!(
            probe.saw().len(),
            2,
            "the link call and the stream reached their handlers"
        );
    }

    #[tokio::test]
    async fn a_slot_is_given_back_once_the_call_reaches_the_gate() {
        let (hosts, probe) = fixtures();
        let admission = slots(1, ServeAdmission::new());
        let served = Setup {
            admission: Some(Arc::clone(&admission)),
            ..Setup::default()
        }
        .serve(INVENTORY, &hosts, &probe);
        let parked = {
            let served = Arc::clone(&served);
            let wire = hosted("a.test", http::HeaderMap::new());
            tokio::spawn(
                async move { respond(&served, request(RELEASE, wire, framed(b"park"))).await },
            )
        };
        tokio::time::timeout(Duration::from_secs(30), probe.entered.notified())
            .await
            .expect("the handler runs");
        assert_eq!(
            admission.slots.available_permits(),
            1,
            "a handler holds no slot"
        );
        let wire = hosted("a.test", http::HeaderMap::new());
        let response = respond(&served, request(RELEASE, wire, framed(b""))).await;
        assert_eq!(early_status(&response), None, "admitted meanwhile");
        probe.finish.notify_one();
        assert_eq!(early_status(&parked.await.unwrap()), None);
    }

    #[tokio::test]
    async fn a_client_gone_during_the_request_read_frees_its_slot() {
        let (hosts, probe) = fixtures();
        let admission = slots(1, ServeAdmission::new());
        let served = Setup {
            admission: Some(Arc::clone(&admission)),
            ..Setup::default()
        }
        .serve(INVENTORY, &hosts, &probe);
        let mut call = tower::Service::call(
            &mut ComponentService(Arc::clone(&served)),
            request(
                RELEASE,
                hosted("a.test", http::HeaderMap::new()),
                Chunks::of([frame(b"partial").slice(..6)]).stalled(),
            ),
        );
        assert!(futures::poll!(&mut call).is_pending(), "reading");
        assert_eq!(admission.slots.available_permits(), 0);
        // As hyper drops it when the client goes away.
        drop(call);
        assert_eq!(admission.slots.available_permits(), 1);
        assert_eq!(hosts.seen(), 0);
    }

    #[tokio::test]
    async fn a_request_larger_than_the_limit_is_refused_before_resolution() {
        let (hosts, probe) = fixtures();
        let admission = slots(4, ServeAdmission::new().with_max_message_bytes(8));
        let setup = || Setup {
            admission: Some(Arc::clone(&admission)),
            ..Setup::default()
        };
        let inventory = setup().serve(INVENTORY, &hosts, &probe);
        let watcher = setup().serve(WATCHER, &hosts, &probe);
        let wire = || hosted("a.test", http::HeaderMap::new());
        let too_large = |response: &http::Response<Body>, method: &str| {
            let error = refusal(response);
            assert_eq!(error.code(), ErrorCode::OutOfRange, "{method}");
            assert_eq!(error.reason(), Some(reasons::REQUEST_TOO_LARGE));
            assert_eq!(
                error.message(),
                "the request message is larger than 8 bytes"
            );
            assert_eq!(error.metadata()["max_bytes"], "8");
            assert_eq!(error.metadata()["method"], method);
        };

        let announced = Chunks::of([Bytes::from_static(&[0, 0, 0, 4, 0])]).stalled();
        too_large(
            &respond(&inventory, request(RELEASE, wire(), announced)).await,
            "release",
        );
        // Not announced: cut off once it passes the limit, the rest unread.
        let trickled = Chunks::of([frame(b"12345678"), Bytes::from_static(b"9")]).stalled();
        too_large(
            &respond(&inventory, request(RELEASE, wire(), trickled)).await,
            "release",
        );
        // A stream's request too.
        too_large(
            &respond(&watcher, request(WATCH, wire(), framed(b"123456789"))).await,
            "watch",
        );
        assert_eq!(hosts.seen(), 0, "the resolver was never asked");

        // A message of the limit, in two chunks and with trailers, is served.
        let whole = frame(b"12345678");
        let mut split = Chunks::of([whole.slice(..3), whole.slice(3..)]);
        split.trailers = true;
        let response = respond(&inventory, request(RELEASE, wire(), split)).await;
        assert_eq!(early_status(&response), None);
        assert_eq!(hosts.seen(), 1);
        assert_eq!(probe.saw().len(), 1);
        assert_eq!(admission.slots.available_permits(), 4);
    }

    #[tokio::test]
    async fn a_failing_request_body_is_answered_as_tonic_answers_it() {
        let (hosts, probe) = fixtures();
        let served = Setup::default().serve(INVENTORY, &hosts, &probe);
        let wire = hosted("a.test", http::HeaderMap::new());
        let response = respond(&served, request(RELEASE, wire, Broken)).await;
        assert_eq!(early_status(&response), Some("13"));
        assert_eq!(refusal(&response).message(), "garbled frame");
        assert_eq!(hosts.seen(), 0);
        assert_eq!(probe.saw(), [] as [Saw; 0]);
    }
    #[tokio::test(start_paused = true)]
    async fn a_call_deadline_wins_a_tie_but_not_a_later_deadline() {
        for (call_millis, expected) in [(50, None), (51, Some(reasons::REQUEST_READ_TIMEOUT))] {
            let served = served().with_admission(slots(
                1,
                ServeAdmission::new().with_read_timeout(Duration::from_millis(50)),
            ));
            let now = Instant::now();
            let cx = CallContext::new()
                .with_deadline((now + Duration::from_millis(call_millis)).into_std());
            let error = AppError::from(
                served
                    .read_request(0, &cx, None, NeverArrives)
                    .await
                    .unwrap_err(),
            );
            assert_eq!(error.code(), ErrorCode::DeadlineExceeded);
            assert_eq!(error.reason(), expected);
            assert!(now.elapsed() >= Duration::from_millis(50));
        }
    }

    #[tokio::test(start_paused = true)]
    async fn an_overflowing_read_deadline_fails_closed() {
        let served = served().with_admission(slots(
            1,
            ServeAdmission::new().with_read_timeout(Duration::MAX),
        ));
        let error = AppError::from(
            served
                .read_request(0, &CallContext::new(), None, NeverArrives)
                .await
                .unwrap_err(),
        );
        assert_eq!(error.reason(), Some(reasons::REQUEST_READ_TIMEOUT));
    }

    #[tokio::test]
    async fn per_client_slots_isolate_addresses_and_release_on_cancellation() {
        let admission = slots(3, ServeAdmission::new().with_max_pending_per_client(1));
        let served = Arc::new(served().with_admission(Arc::clone(&admission)));
        let from = |address: &str| {
            let mut request = request(RELEASE, http::HeaderMap::new(), NeverArrives);
            request
                .extensions_mut()
                .insert(ClientAddr(address.parse().unwrap()));
            request
        };
        for (first, same, other) in [
            ("192.0.2.1", "::ffff:192.0.2.1", "192.0.2.2"),
            ("2001:db8::1", "2001:db8::2", "2001:db8:0:1::1"),
        ] {
            let mut held =
                tower::Service::call(&mut ComponentService(Arc::clone(&served)), from(first));
            assert!(futures::poll!(&mut held).is_pending());
            let error = refusal(&respond(&served, from(same)).await);
            assert_eq!(error.code(), ErrorCode::ResourceExhausted);
            assert_eq!(error.reason(), Some(reasons::ADMISSION_FULL));
            let mut other =
                tower::Service::call(&mut ComponentService(Arc::clone(&served)), from(other));
            assert!(futures::poll!(&mut other).is_pending());
            assert_eq!(admission.slots.available_permits(), 1);
            assert_eq!(admission.clients.lock().unwrap().len(), 2);
            drop(held);
            drop(other);
            assert_eq!(admission.slots.available_permits(), 3);
            assert_eq!(admission.clients.lock().unwrap().len(), 0);
        }
        let permit = served.admit(0, &CallContext::new()).unwrap();
        assert_eq!(
            served.admit(0, &CallContext::new()).unwrap_err().reason(),
            Some(reasons::ADMISSION_FULL)
        );
        drop(permit);
        assert_eq!(admission.clients.lock().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn the_frame_header_refuses_oversized_truncated_and_compressed_messages() {
        let served = Arc::new(
            served().with_admission(slots(1, ServeAdmission::new().with_max_message_bytes(8))),
        );
        for flag in [0, 1] {
            for stall in [false, true] {
                let mut body = Chunks::of([
                    Bytes::copy_from_slice(&[flag, 0]),
                    Bytes::from_static(&[0, 0]),
                    Bytes::from_static(&[9]),
                ]);
                body.stall = stall;
                let error = refusal(
                    &respond(&served, request(RELEASE, http::HeaderMap::new(), body)).await,
                );
                assert_eq!(error.code(), ErrorCode::OutOfRange);
                assert_eq!(error.reason(), Some(reasons::REQUEST_TOO_LARGE));
                assert_eq!(error.metadata()["max_bytes"], "8");
            }
        }
    }

    #[tokio::test]
    async fn wire_size_hints_do_not_override_the_decoded_byte_limit() {
        let mut body = Chunks::of(
            frame(b"12345678")
                .iter()
                .map(|byte| Bytes::copy_from_slice(&[*byte])),
        );
        body.size = Some(1024);
        let bytes = read_body(body, 13)
            .await
            .unwrap_or_else(|_| panic!("accepted"));
        assert_eq!(bytes, frame(b"12345678"));
    }

    #[tokio::test]
    async fn tonic_still_decides_unary_framing_edges() {
        use http_body_util::BodyExt as _;
        let cases = [
            Bytes::new(),
            Bytes::from_static(&[0, 0]),
            Bytes::from_static(&[0, 0, 0, 0, 2, 1]),
            Bytes::from_static(&[1, 0, 0, 0, 1, 0]),
            Bytes::from_static(&[2, 0, 0, 0, 0]),
            Bytes::from_static(&[0, 0, 0, 0, 0, 99]),
            Bytes::from_static(&[0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
        ];
        let served = Arc::new(served());
        for bytes in cases {
            let direct = Grpc::new(BytesCodec)
                .max_decoding_message_size(MAX_MESSAGE_SIZE)
                .unary(
                    Unary {
                        served: Arc::clone(&served),
                        method: 1,
                        cx: CallContext::new(),
                    },
                    request(
                        RELEASE,
                        http::HeaderMap::new(),
                        Buffered(Some(bytes.clone())),
                    ),
                )
                .await;
            let buffered = respond(
                &served,
                request(RELEASE, http::HeaderMap::new(), Buffered(Some(bytes))),
            )
            .await;
            assert_eq!(buffered.headers(), direct.headers());
            let direct = direct.into_body().collect().await.unwrap();
            let buffered = buffered.into_body().collect().await.unwrap();
            assert_eq!(buffered.trailers(), direct.trailers());
            assert_eq!(buffered.to_bytes(), direct.to_bytes());
        }
    }
}
