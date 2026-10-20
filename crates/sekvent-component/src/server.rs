//! The serving side of an installed component: the gate that admits calls
//! only while the component is serving and counts those in flight, the
//! per-method policies, and the serving wrapper of its streams.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};
use std::task::{Context, Poll};
use std::time::Duration;

use futures::Stream;
use sekvent_context::CallContext;
use sekvent_error::AppError;
use sekvent_resilience::{Bulkhead, BulkheadPermit, PolicySpec, Timeout};
use tokio::sync::Notify;
use tokio::time::{Instant, Sleep};
use tokio_util::sync::{CancellationToken, DropGuard, WaitForCancellationFutureOwned};

use crate::stream::CallStream;
use crate::{ComponentDescriptor, ComponentState, reasons};

/// The resolved policy of one method.
#[derive(Debug, Clone, PartialEq, Default)]
pub(crate) struct MethodPolicy {
    /// Time limit of one call (of the open, for a stream); `None` leaves
    /// only the caller's deadline.
    pub(crate) timeout: Option<Duration>,
    /// Every resolved field (4.3): the bulkhead for the serving side, the
    /// retries for a remote caller.
    pub(crate) spec: PolicySpec,
    /// The configured stream lifetime; `Some` exactly for streaming
    /// methods.
    pub(crate) max_lifetime: Option<Duration>,
}

/// The gate of one installed component and, when it runs in this process,
/// its serving-side bulkheads.
#[derive(Debug)]
pub(crate) struct Server {
    descriptor: &'static ComponentDescriptor,
    gate: Gate,
    methods: Box<[MethodRuntime]>,
    /// The caller side's gate of a component served by another process:
    /// no bulkheads, no stream lifetimes, no stream events.
    remote: bool,
}

#[derive(Debug)]
struct MethodRuntime {
    timeout: Option<Duration>,
    bulkhead: Option<Bulkhead>,
    max_lifetime: Option<Duration>,
}

#[derive(Debug)]
struct Gate {
    state: AtomicU8,
    in_flight: AtomicUsize,
    idle: Notify,
    /// Fired once the component leaves `Serving`, or by
    /// [`Server::close_streams`]: ends every open stream and refuses new
    /// ones. Never reset.
    closing: CancellationToken,
}

impl Gate {
    fn leave(&self) {
        if self.in_flight.fetch_sub(1, Ordering::SeqCst) == 1 {
            self.idle.notify_waiters();
        }
    }
}

/// Keeps one admitted call counted until dropped.
struct Admission<'a>(&'a Gate);

impl Drop for Admission<'_> {
    fn drop(&mut self) {
        self.0.leave();
    }
}

/// Keeps one admitted stream counted until dropped.
struct StreamAdmission(Arc<Server>);

impl Drop for StreamAdmission {
    fn drop(&mut self) {
        self.0.gate.leave();
    }
}

const fn state_code(state: ComponentState) -> u8 {
    match state {
        ComponentState::NotStarted => 0,
        ComponentState::Serving => 1,
        ComponentState::Draining => 2,
        ComponentState::Stopped => 3,
    }
}

const fn state_of(code: u8) -> ComponentState {
    match code {
        0 => ComponentState::NotStarted,
        1 => ComponentState::Serving,
        2 => ComponentState::Draining,
        _ => ComponentState::Stopped,
    }
}

impl Server {
    /// A server in [`ComponentState::NotStarted`]; `policies` holds one
    /// entry per method of `descriptor`, in order. A `remote` component's
    /// gate has no bulkheads: they belong to the process that serves it.
    pub(crate) fn new(
        descriptor: &'static ComponentDescriptor,
        policies: &[MethodPolicy],
        remote: bool,
    ) -> Self {
        let methods = policies
            .iter()
            .map(|policy| MethodRuntime {
                timeout: policy.timeout,
                // Validated while resolving the configuration.
                bulkhead: if remote {
                    None
                } else {
                    policy.spec.build_bulkhead().ok().flatten()
                },
                max_lifetime: if remote { None } else { policy.max_lifetime },
            })
            .collect();
        Self {
            descriptor,
            gate: Gate {
                state: AtomicU8::new(state_code(ComponentState::NotStarted)),
                in_flight: AtomicUsize::new(0),
                idle: Notify::new(),
                closing: CancellationToken::new(),
            },
            methods,
            remote,
        }
    }

    pub(crate) fn descriptor(&self) -> &'static ComponentDescriptor {
        self.descriptor
    }

    /// The resolved timeout of method `method`; `None` for an unknown index.
    pub(crate) fn timeout(&self, method: usize) -> Option<Duration> {
        self.methods.get(method).and_then(|runtime| runtime.timeout)
    }

    pub(crate) fn state(&self) -> ComponentState {
        state_of(self.gate.state.load(Ordering::SeqCst))
    }

    /// Move to `state`; any state but `Serving` also closes the streams.
    pub(crate) fn set_state(&self, state: ComponentState) {
        self.gate.state.store(state_code(state), Ordering::SeqCst);
        if state != ComponentState::Serving {
            self.gate.closing.cancel();
        }
    }

    /// End every open stream with `UNAVAILABLE` / `COMPONENT_DRAINING` and
    /// refuse new ones; unary calls are not affected. Irreversible.
    pub(crate) fn close_streams(&self) {
        self.gate.closing.cancel();
    }

    /// Whether the streams are closed.
    pub(crate) fn streams_closed(&self) -> bool {
        self.gate.closing.is_cancelled()
    }

    /// The signal that closes the streams.
    pub(crate) fn closing(&self) -> &CancellationToken {
        &self.gate.closing
    }

    /// Start serving a component that has not started. `false` when its
    /// state was changed meanwhile (a stop that raced the start).
    pub(crate) fn open(&self) -> bool {
        self.gate
            .state
            .compare_exchange(
                state_code(ComponentState::NotStarted),
                state_code(ComponentState::Serving),
                Ordering::SeqCst,
                Ordering::SeqCst,
            )
            .is_ok()
    }

    pub(crate) fn in_flight(&self) -> usize {
        self.gate.in_flight.load(Ordering::SeqCst)
    }

    /// Wait until no call is in flight, or until `deadline` passes (`None`:
    /// wait as long as it takes). Returns whether the component is idle.
    pub(crate) async fn wait_idle(&self, deadline: Option<tokio::time::Instant>) -> bool {
        loop {
            let notified = self.gate.idle.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.in_flight() == 0 {
                return true;
            }
            let expiry = async {
                match deadline {
                    Some(deadline) => tokio::time::sleep_until(deadline).await,
                    None => std::future::pending().await,
                }
            };
            tokio::select! {
                () = notified => {}
                () = expiry => return self.in_flight() == 0,
            }
        }
    }

    /// Add `component` and `method` metadata to an error this crate made.
    pub(crate) fn tag(&self, error: AppError, method: usize) -> AppError {
        tag(self.descriptor, error, method)
    }

    /// Run one call through admission, dead-call shedding, the method's
    /// bulkhead and the context deadline.
    ///
    /// The outer `Err` is an error of the serving side, tagged with the
    /// component and method; the call's own outcome is `Ok` and untouched.
    pub(crate) async fn run<T, F, Fut>(
        &self,
        method: usize,
        cx: CallContext,
        call: F,
    ) -> Result<T, AppError>
    where
        F: FnOnce(CallContext) -> Fut,
        Fut: Future<Output = T>,
    {
        let _admitted = self.admit().map_err(|error| self.tag(error, method))?;
        shed(&cx).map_err(|error| self.tag(error, method))?;
        let _permit = match self.methods.get(method).and_then(|m| m.bulkhead.as_ref()) {
            Some(bulkhead) => Some(
                bulkhead
                    .acquire(&cx)
                    .await
                    .map_err(|error| self.tag(error, method))?,
            ),
            None => None,
        };
        let limit = Timeout::deadline_only();
        let inner = cx.clone();
        limit
            .call(&cx, async move { Ok(call(inner).await) })
            .await
            .map_err(|error| self.tag(error, method))
    }

    /// Count a call in, then check the state: incrementing first means a
    /// drain that starts concurrently either sees this call or rejects it.
    fn admit(&self) -> Result<Admission<'_>, AppError> {
        self.gate.in_flight.fetch_add(1, Ordering::SeqCst);
        let admission = Admission(&self.gate);
        self.check_state(self.state())?;
        Ok(admission)
    }

    /// [`admit`](Self::admit) for a stream, which is also refused once the
    /// streams are closed.
    fn admit_stream(self: &Arc<Self>) -> Result<StreamAdmission, AppError> {
        self.gate.in_flight.fetch_add(1, Ordering::SeqCst);
        let admission = StreamAdmission(Arc::clone(self));
        self.check_state(self.state())?;
        if self.streams_closed() {
            return Err(self.closed());
        }
        Ok(admission)
    }

    /// `Ok` while serving, the error for `state` otherwise.
    fn check_state(&self, state: ComponentState) -> Result<(), AppError> {
        let name = self.descriptor.name();
        match state {
            ComponentState::Serving => Ok(()),
            ComponentState::NotStarted => Err(AppError::unavailable(format!(
                "component {name} is not started"
            ))
            .with_reason(reasons::NOT_STARTED)),
            ComponentState::Draining => Err(draining(name)),
            ComponentState::Stopped => Err(stopped(name)),
        }
    }

    /// What closed streams end with: `COMPONENT_STOPPED` once stopped,
    /// `COMPONENT_DRAINING` otherwise (draining, or closed while serving).
    fn closed(&self) -> AppError {
        let name = self.descriptor.name();
        match self.state() {
            ComponentState::Stopped => stopped(name),
            _ => draining(name),
        }
    }

    /// Open one stream of method `method`: admission (held for the whole
    /// stream), dead-call shedding, the method's bulkhead (its permit held
    /// for the whole stream) and `open`, the handler's open, all within the
    /// open bound: `open_deadline` (the method's `TIMEOUT`, when this side
    /// enforces it), the caller's deadline and the stream's lifetime.
    ///
    /// `open` receives the stream's context: the caller's deadline narrowed
    /// to the drawn lifetime, and a token of its own that is cancelled when
    /// the stream ends for any reason. The returned stream is the serving
    /// wrapper: it ends the producer when the streams close, the caller's
    /// deadline or the lifetime passes, and logs one `stream ended` event.
    ///
    /// Errors of the serving side are tagged; the open's own error and the
    /// producer's errors are untouched.
    pub(crate) async fn open_stream<T, F, Fut>(
        self: &Arc<Self>,
        method: usize,
        cx: CallContext,
        open_deadline: Option<Instant>,
        binding: &'static str,
        open: F,
    ) -> Result<CallStream<T, AppError>, AppError>
    where
        T: Send + 'static,
        F: FnOnce(CallContext) -> Fut,
        Fut: Future<Output = Result<CallStream<T, AppError>, AppError>>,
    {
        let admission = self
            .admit_stream()
            .map_err(|error| self.tag(error, method))?;
        shed(&cx).map_err(|error| self.tag(error, method))?;
        let admitted = Instant::now();
        let runtime = self.methods.get(method);
        let max_lifetime = runtime.and_then(|runtime| runtime.max_lifetime);
        let expiry = max_lifetime
            .map(|max| draw_lifetime(max, rand::random::<f64>()))
            .and_then(|lifetime| admitted.checked_add(lifetime));
        let caller = cx.deadline().map(Instant::from_std);
        let token = cx.cancel_token().child_token();
        let mut stream_cx = cx.with_cancel(token.clone());
        if let Some(expiry) = expiry {
            stream_cx = stream_cx.with_deadline(expiry.into_std());
        }
        // Cancels the stream's token if the open fails or is dropped.
        let guard = token.drop_guard();
        let bound = earliest(earliest(open_deadline, caller), expiry);
        let closing = self.gate.closing.clone();
        let opening = async {
            let permit = match runtime.and_then(|runtime| runtime.bulkhead.as_ref()) {
                Some(bulkhead) => Some(
                    bulkhead
                        .acquire(&stream_cx)
                        .await
                        .map_err(|error| self.tag(error, method))?,
                ),
                None => None,
            };
            Ok::<_, AppError>((permit, open(stream_cx.clone()).await))
        };
        let opened = tokio::select! {
            biased;
            () = closing.cancelled() => Err(self.tag(self.closed(), method)),
            opened = opening => opened,
            () = sleep_until(bound) => Err(self.tag(
                if expiry.is_some_and(|expiry| Some(expiry) == bound
                    && caller.is_none_or(|caller| expiry < caller)
                    && open_deadline.is_none_or(|open| expiry < open))
                {
                    expired(max_lifetime)
                } else {
                    AppError::deadline_exceeded("the call deadline was exceeded")
                },
                method,
            )),
        };
        let (permit, producer) = opened?;
        let producer = producer?;
        Ok(CallStream::new(ServingStream {
            live: Some(Live {
                producer,
                closing: Box::pin(self.gate.closing.clone().cancelled_owned()),
                timer: earliest(caller, expiry).map(|at| Box::pin(tokio::time::sleep_until(at))),
                caller,
                expiry,
                _permit: permit,
                _token: guard,
                _admission: admission,
            }),
            record: Record {
                server: Arc::clone(self),
                method,
                binding,
                span: tracing::Span::current(),
                started: admitted,
                items: 0,
                max_lifetime,
            },
        }))
    }
}

/// `UNAVAILABLE` / `COMPONENT_DRAINING` for component `name`.
fn draining(name: &str) -> AppError {
    AppError::unavailable(format!("component {name} is shutting down"))
        .with_reason(reasons::DRAINING)
}

/// `UNAVAILABLE` / `COMPONENT_STOPPED` for component `name`.
fn stopped(name: &str) -> AppError {
    AppError::unavailable(format!("component {name} is stopped")).with_reason(reasons::STOPPED)
}

/// `UNAVAILABLE` / [`reasons::STREAM_EXPIRED`]; `max_lifetime` is the
/// configured value, not the drawn one.
fn expired(max_lifetime: Option<Duration>) -> AppError {
    let millis = max_lifetime.unwrap_or_default().as_millis();
    AppError::unavailable(format!(
        "the stream reached its maximum lifetime of {millis} ms; open it again"
    ))
    .with_reason(reasons::STREAM_EXPIRED)
    .with_metadata("max_lifetime_ms", millis.to_string())
}

/// The earlier of two optional instants.
fn earliest(left: Option<Instant>, right: Option<Instant>) -> Option<Instant> {
    match (left, right) {
        (Some(left), Some(right)) => Some(left.min(right)),
        (left, right) => left.or(right),
    }
}

async fn sleep_until(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}

/// One stream's lifetime: uniformly between 90 % and 100 % of `max` for a
/// `unit` between 0 and 1, never above `max`.
pub(crate) fn draw_lifetime(max: Duration, unit: f64) -> Duration {
    let fraction = 0.9 + 0.1 * unit.clamp(0.0, 1.0);
    let drawn = Duration::try_from_secs_f64(max.as_secs_f64() * fraction).unwrap_or(max);
    drawn.clamp(max / 10 * 9, max)
}

/// How a stream ended, as its `stream ended` event says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Outcome {
    /// The producer ended.
    Ended,
    /// The producer yielded an error.
    Failed,
    /// The consumer dropped the stream.
    Dropped,
    /// The caller's deadline passed.
    Deadline,
    /// The lifetime passed.
    Expired,
    /// The component closed its streams.
    Closed,
    /// The producer panicked.
    Panicked,
}

impl Outcome {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Ended => "ended",
            Self::Failed => "failed",
            Self::Dropped => "dropped",
            Self::Deadline => "deadline",
            Self::Expired => "expired",
            Self::Closed => "closed",
            Self::Panicked => "panicked",
        }
    }
}

/// What a live stream holds; dropped as a whole when it ends, producer
/// first.
struct Live<T> {
    producer: CallStream<T, AppError>,
    closing: Pin<Box<WaitForCancellationFutureOwned>>,
    /// Fires at the earlier of the caller's deadline and the expiry.
    timer: Option<Pin<Box<Sleep>>>,
    caller: Option<Instant>,
    expiry: Option<Instant>,
    _permit: Option<BulkheadPermit>,
    /// Cancels the stream's token.
    _token: DropGuard,
    _admission: StreamAdmission,
}

/// What the `stream ended` event reports.
struct Record {
    server: Arc<Server>,
    method: usize,
    binding: &'static str,
    span: tracing::Span,
    started: Instant,
    items: u64,
    max_lifetime: Option<Duration>,
}

impl Record {
    fn report(&self, outcome: Outcome, error: Option<&AppError>) {
        if self.server.remote {
            return;
        }
        let descriptor = self.server.descriptor;
        let method = descriptor
            .methods()
            .get(self.method)
            .map_or("?", |method| method.name());
        let duration_ms = u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX);
        self.span.in_scope(|| {
            tracing::debug!(
                component = descriptor.name(),
                method,
                binding = self.binding,
                items = self.items,
                duration_ms,
                outcome = outcome.as_str(),
                code = error.map(|error| error.code().as_str()),
                reason = error.and_then(AppError::reason),
                "stream ended"
            );
        });
    }
}

/// The serving wrapper of one stream (see [`Server::open_stream`]).
struct ServingStream<T> {
    /// `None` once the stream has ended.
    live: Option<Live<T>>,
    record: Record,
}

impl<T> ServingStream<T> {
    /// Release everything and report; `error` is the terminal item.
    fn end(
        &mut self,
        outcome: Outcome,
        error: Option<AppError>,
    ) -> Poll<Option<Result<T, AppError>>> {
        self.live = None;
        self.record.report(outcome, error.as_ref());
        Poll::Ready(error.map(Err))
    }
}

impl<T> Stream for ServingStream<T> {
    type Item = Result<T, AppError>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        let span = this.record.span.clone();
        let _entered = span.enter();
        let Some(live) = this.live.as_mut() else {
            return Poll::Ready(None);
        };
        // Terminal conditions first: they win over a ready item.
        if live.closing.as_mut().poll(cx).is_ready() {
            let error = this.record.server.closed();
            let error = this.record.server.tag(error, this.record.method);
            return this.end(Outcome::Closed, Some(error));
        }
        if let Some(timer) = live.timer.as_mut()
            && timer.as_mut().poll(cx).is_ready()
        {
            let now = Instant::now();
            let caller = live.caller.filter(|caller| *caller <= now);
            let expiry = live.expiry.filter(|expiry| *expiry <= now);
            let lifetime_first = match (caller, expiry) {
                (Some(caller), Some(expiry)) => expiry < caller,
                (None, Some(_)) => true,
                _ => false,
            };
            let (outcome, error) = if lifetime_first {
                (Outcome::Expired, expired(this.record.max_lifetime))
            } else {
                (
                    Outcome::Deadline,
                    AppError::deadline_exceeded("the call deadline was exceeded"),
                )
            };
            let error = this.record.server.tag(error, this.record.method);
            return this.end(outcome, Some(error));
        }
        let polled = Pin::new(&mut live.producer).poll_next(cx);
        match polled {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Some(Ok(item))) => {
                this.record.items += 1;
                Poll::Ready(Some(Ok(item)))
            }
            Poll::Ready(Some(Err(error))) => {
                let outcome = if error.reason() == Some(reasons::HANDLER_PANICKED) {
                    Outcome::Panicked
                } else {
                    Outcome::Failed
                };
                this.end(outcome, Some(error))
            }
            Poll::Ready(None) => this.end(Outcome::Ended, None),
        }
    }
}

impl<T> Drop for ServingStream<T> {
    fn drop(&mut self) {
        if self.live.is_some() {
            // Only a drop during the unwind itself sees the panic: a stream
            // polled in a tokio task is dropped after tokio caught the panic
            // and reports `dropped`.
            let outcome = if std::thread::panicking() {
                Outcome::Panicked
            } else {
                Outcome::Dropped
            };
            let _entered = self.record.span.enter();
            self.live = None;
            self.record.report(outcome, None);
        }
    }
}

/// Add `component` and `method` metadata (names only) to `error`.
pub(crate) fn tag(
    descriptor: &'static ComponentDescriptor,
    error: AppError,
    method: usize,
) -> AppError {
    let error = error.with_metadata("component", descriptor.name());
    match descriptor.methods().get(method) {
        Some(method) => error.with_metadata("method", method.name()),
        None => error,
    }
}

/// Reject a call nobody waits for: a cancelled context or a passed deadline
/// (on tokio's clock).
pub(crate) fn shed(cx: &CallContext) -> Result<(), AppError> {
    if cx.cancel_token().is_cancelled() {
        return Err(AppError::cancelled("the call was cancelled"));
    }
    if sekvent_resilience::remaining(cx) == Some(Duration::ZERO) {
        return Err(AppError::deadline_exceeded(
            "the call deadline was exceeded",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use futures::StreamExt as _;
    use sekvent_error::ErrorCode;

    use super::*;
    use crate::MethodDescriptor;

    const METHODS: &[MethodDescriptor] = &[
        MethodDescriptor::call("reserve", "Reserve"),
        MethodDescriptor::call("release", "Release"),
    ];
    const INVENTORY: &ComponentDescriptor =
        &ComponentDescriptor::new("inventory", "Inventory", METHODS);

    fn policy(timeout: Option<Duration>, bulkhead: Option<u32>) -> MethodPolicy {
        let mut spec = PolicySpec::default();
        spec.timeout = timeout;
        spec.bulkhead_max_concurrent = bulkhead;
        MethodPolicy {
            timeout,
            spec,
            max_lifetime: None,
        }
    }

    fn server(bulkhead: Option<u32>) -> Server {
        Server::new(
            INVENTORY,
            &[
                policy(Some(Duration::from_secs(1)), bulkhead),
                MethodPolicy::default(),
            ],
            false,
        )
    }

    #[test]
    fn states_round_trip_through_their_codes() {
        for state in [
            ComponentState::NotStarted,
            ComponentState::Serving,
            ComponentState::Draining,
            ComponentState::Stopped,
        ] {
            assert_eq!(state_of(state_code(state)), state);
        }
        assert_eq!(state_of(200), ComponentState::Stopped);
    }

    #[test]
    fn opening_only_moves_a_component_that_has_not_started() {
        let server = server(None);
        assert_eq!(server.state(), ComponentState::NotStarted);
        assert!(server.open());
        assert_eq!(server.state(), ComponentState::Serving);
        assert!(!server.open());
        server.set_state(ComponentState::Draining);
        assert!(!server.open());
        assert_eq!(server.state(), ComponentState::Draining);
    }

    #[test]
    fn policies_by_index() {
        let server = server(Some(2));
        assert_eq!(server.timeout(0), Some(Duration::from_secs(1)));
        assert_eq!(
            server.methods[0]
                .bulkhead
                .as_ref()
                .map(Bulkhead::max_concurrent),
            Some(2)
        );
        assert_eq!(server.timeout(1), None);
        assert!(server.methods[1].bulkhead.is_none());
        assert_eq!(server.timeout(9), None);
        assert_eq!(server.descriptor().name(), "inventory");
    }

    #[test]
    fn a_remote_gate_has_no_bulkheads() {
        let remote = Server::new(
            INVENTORY,
            &[policy(None, Some(2)), MethodPolicy::default()],
            true,
        );
        assert!(
            remote
                .methods
                .iter()
                .all(|method| method.bulkhead.is_none())
        );
    }

    #[test]
    fn bulkheads_keep_their_queue_settings() {
        let mut queued = policy(None, Some(1));
        queued.spec.bulkhead_max_queue = Some(1);
        queued.spec.bulkhead_queue_timeout = Some(Duration::from_secs(1));
        let server = Server::new(INVENTORY, &[queued, MethodPolicy::default()], false);
        assert_eq!(
            server.methods[0].bulkhead.as_ref().map(Bulkhead::max_queue),
            Some(1)
        );
    }

    #[tokio::test]
    async fn admission_follows_the_state() {
        let server = server(None);
        for (state, reason) in [
            (ComponentState::NotStarted, reasons::NOT_STARTED),
            (ComponentState::Draining, reasons::DRAINING),
            (ComponentState::Stopped, reasons::STOPPED),
        ] {
            server.set_state(state);
            let error = server
                .run(1, CallContext::new(), |_| async { 1 })
                .await
                .unwrap_err();
            assert_eq!(error.code(), ErrorCode::Unavailable);
            assert_eq!(error.reason(), Some(reason));
            assert_eq!(error.metadata()["component"], "inventory");
            assert_eq!(error.metadata()["method"], "release");
            assert_eq!(server.in_flight(), 0);
        }
        server.set_state(ComponentState::Serving);
        assert_eq!(server.state(), ComponentState::Serving);
        assert_eq!(
            server
                .run(1, CallContext::new(), |_| async { 1 })
                .await
                .unwrap(),
            1
        );
    }

    #[test]
    fn tagging_an_unknown_method_names_only_the_component() {
        let error = tag(INVENTORY, AppError::unavailable("x"), 7);
        assert_eq!(error.metadata()["component"], "inventory");
        assert!(!error.metadata().contains_key("method"));
    }

    #[tokio::test(start_paused = true)]
    async fn shedding_follows_the_tokio_clock() {
        assert!(shed(&CallContext::new()).is_ok());
        let deadline = (tokio::time::Instant::now() + Duration::from_secs(1)).into_std();
        let cx = CallContext::new().with_deadline(deadline);
        assert!(shed(&cx).is_ok());
        tokio::time::advance(Duration::from_secs(1)).await;
        assert_eq!(shed(&cx).unwrap_err().code(), ErrorCode::DeadlineExceeded);
        let cancelled = CallContext::new();
        cancelled.cancel_token().cancel();
        assert_eq!(shed(&cancelled).unwrap_err().code(), ErrorCode::Cancelled);
    }

    const STREAMS: &[MethodDescriptor] = &[
        MethodDescriptor::call("ticks", "Ticks").with_stream(),
        MethodDescriptor::call("latest", "Latest"),
    ];
    const TICKER: &ComponentDescriptor = &ComponentDescriptor::new("ticker", "Ticker", STREAMS);

    fn ticker(bulkhead: Option<u32>, max_lifetime: Option<Duration>) -> Arc<Server> {
        let mut ticks = policy(Some(Duration::from_secs(1)), bulkhead);
        ticks.max_lifetime = max_lifetime;
        let server = Arc::new(Server::new(
            TICKER,
            &[ticks, MethodPolicy::default()],
            false,
        ));
        assert!(server.open());
        server
    }

    /// Open a stream of `items` (then pending forever when `endless`).
    async fn open(
        server: &Arc<Server>,
        cx: CallContext,
        items: Vec<Result<u32, AppError>>,
        endless: bool,
    ) -> Result<CallStream<u32, AppError>, AppError> {
        server
            .open_stream(0, cx, None, "local", move |_cx| async move {
                let items = futures::stream::iter(items);
                Ok(if endless {
                    CallStream::new(items.chain(futures::stream::pending()))
                } else {
                    CallStream::new(items)
                })
            })
            .await
    }

    #[test]
    fn the_lifetime_draw_stays_within_ninety_to_a_hundred_percent() {
        let max = Duration::from_secs(600);
        assert_eq!(draw_lifetime(max, 0.0), Duration::from_secs(540));
        assert_eq!(draw_lifetime(max, 0.5), Duration::from_secs(570));
        assert_eq!(draw_lifetime(max, 1.0), max);
        assert_eq!(draw_lifetime(max, -3.0), Duration::from_secs(540));
        assert_eq!(draw_lifetime(max, 7.0), max);
        assert_eq!(draw_lifetime(Duration::MAX, 1.0), Duration::MAX);
        let tiny = Duration::from_nanos(1);
        assert!(draw_lifetime(tiny, 0.0) <= tiny);
        for unit in [0.1, 0.33, 0.9, 0.999] {
            let drawn = draw_lifetime(max, unit);
            assert!(
                drawn >= Duration::from_secs(540) && drawn <= max,
                "{drawn:?}"
            );
        }
    }

    #[test]
    fn closing_is_fired_by_leaving_serving_and_by_close_streams() {
        for state in [ComponentState::Draining, ComponentState::Stopped] {
            let server = ticker(None, None);
            server.set_state(ComponentState::Serving);
            assert!(!server.streams_closed());
            server.set_state(state);
            assert!(server.streams_closed(), "{state:?}");
        }
        let server = ticker(None, None);
        server.close_streams();
        assert!(server.streams_closed());
        assert_eq!(server.state(), ComponentState::Serving, "unary calls go on");
    }

    #[tokio::test]
    async fn opens_after_closing_are_refused_with_the_reason_of_the_state() {
        let server = ticker(None, None);
        server.close_streams();
        let error = open(&server, CallContext::new(), vec![], false)
            .await
            .unwrap_err();
        assert_eq!(error.code(), ErrorCode::Unavailable);
        assert_eq!(error.reason(), Some(reasons::DRAINING));
        assert_eq!(error.message(), "component ticker is shutting down");
        assert_eq!(error.metadata()["method"], "ticks");
        assert_eq!(server.in_flight(), 0);
        // Unary calls still run.
        assert_eq!(
            server
                .run(1, CallContext::new(), |_| async { 3 })
                .await
                .unwrap(),
            3
        );

        for (state, reason) in [
            (ComponentState::NotStarted, reasons::NOT_STARTED),
            (ComponentState::Draining, reasons::DRAINING),
            (ComponentState::Stopped, reasons::STOPPED),
        ] {
            server.set_state(state);
            let error = open(&server, CallContext::new(), vec![], false)
                .await
                .unwrap_err();
            assert_eq!(error.reason(), Some(reason), "{state:?}");
            assert_eq!(server.in_flight(), 0);
        }

        let dead = CallContext::new();
        dead.cancel_token().cancel();
        let live = ticker(None, None);
        let error = open(&live, dead, vec![], false).await.unwrap_err();
        assert_eq!(error.code(), ErrorCode::Cancelled);
        assert_eq!(live.in_flight(), 0);
    }

    #[tokio::test]
    async fn admission_and_permit_are_held_until_the_end_and_released_at_it() {
        let server = ticker(Some(1), None);
        let mut stream = open(&server, CallContext::new(), vec![Ok(1), Ok(2)], false)
            .await
            .unwrap();
        assert_eq!(server.in_flight(), 1);
        let full = open(&server, CallContext::new(), vec![], false)
            .await
            .unwrap_err();
        assert_eq!(full.code(), ErrorCode::ResourceExhausted);
        assert_eq!(full.metadata()["method"], "ticks");
        assert_eq!(server.in_flight(), 1, "a refused open leaves at once");

        assert_eq!(stream.next().await.unwrap().unwrap(), 1);
        assert_eq!(stream.next().await.unwrap().unwrap(), 2);
        assert_eq!(server.in_flight(), 1);
        assert!(stream.next().await.is_none());
        assert_eq!(server.in_flight(), 0, "released at the end, not at drop");
        let again = open(&server, CallContext::new(), vec![], false)
            .await
            .unwrap();
        drop(again);
        assert_eq!(server.in_flight(), 0);
        drop(stream);
    }

    #[tokio::test]
    async fn an_error_ends_the_stream_and_releases_it() {
        let server = ticker(None, None);
        let mut stream = open(
            &server,
            CallContext::new(),
            vec![Ok(1), Err(AppError::not_found("gone")), Ok(2)],
            false,
        )
        .await
        .unwrap();
        assert_eq!(stream.next().await.unwrap().unwrap(), 1);
        let error = stream.next().await.unwrap().unwrap_err();
        assert_eq!(error.code(), ErrorCode::NotFound);
        assert!(
            error.metadata().is_empty(),
            "the producer's error is untouched"
        );
        assert_eq!(server.in_flight(), 0);
        assert!(stream.next().await.is_none());

        let failed = server
            .open_stream::<u32, _, _>(0, CallContext::new(), None, "local", |_| async {
                Err(AppError::invalid_argument("no such series"))
            })
            .await
            .unwrap_err();
        assert!(
            failed.metadata().is_empty(),
            "the open's error is untouched"
        );
        assert_eq!(server.in_flight(), 0);
    }

    #[tokio::test]
    async fn closing_ends_an_open_stream_before_a_ready_item() {
        let server = ticker(None, None);
        let mut stream = open(&server, CallContext::new(), vec![Ok(1), Ok(2)], true)
            .await
            .unwrap();
        assert_eq!(stream.next().await.unwrap().unwrap(), 1);
        server.close_streams();
        let error = stream.next().await.unwrap().unwrap_err();
        assert_eq!(error.reason(), Some(reasons::DRAINING));
        assert_eq!(error.metadata()["component"], "ticker");
        assert_eq!(server.in_flight(), 0);
        assert!(stream.next().await.is_none());

        let server = ticker(None, None);
        let mut stream = open(&server, CallContext::new(), vec![], true)
            .await
            .unwrap();
        server.set_state(ComponentState::Stopped);
        let error = stream.next().await.unwrap().unwrap_err();
        assert_eq!(error.reason(), Some(reasons::STOPPED));
    }

    #[tokio::test]
    async fn the_stream_context_has_its_own_token_cancelled_at_the_end() {
        let server = ticker(None, Some(Duration::from_secs(100)));
        let (seen_tx, seen_rx) = tokio::sync::oneshot::channel();
        let caller = CallContext::new();
        let stream = server
            .open_stream(0, caller.clone(), None, "local", move |cx| async move {
                seen_tx.send(cx).unwrap();
                Ok(CallStream::<u32, AppError>::new(futures::stream::pending()))
            })
            .await
            .unwrap();
        let seen = seen_rx.await.unwrap();
        assert!(seen.deadline().is_some(), "the lifetime is the deadline");
        assert!(!seen.cancel_token().is_cancelled());
        drop(stream);
        assert!(seen.cancel_token().is_cancelled());
        assert!(
            !caller.cancel_token().is_cancelled(),
            "only the stream's token"
        );
        assert_eq!(server.in_flight(), 0);

        // The caller cancelling reaches the stream's token too.
        let (seen_tx, seen_rx) = tokio::sync::oneshot::channel();
        let caller = CallContext::new();
        let _stream = server
            .open_stream(0, caller.clone(), None, "local", move |cx| async move {
                seen_tx.send(cx).unwrap();
                Ok(CallStream::<u32, AppError>::new(futures::stream::pending()))
            })
            .await
            .unwrap();
        caller.cancel_token().cancel();
        assert!(seen_rx.await.unwrap().cancel_token().is_cancelled());
    }

    #[tokio::test(start_paused = true)]
    async fn deadlines_and_lifetimes_end_a_stream() {
        let server = ticker(None, Some(Duration::from_secs(10)));
        let started = tokio::time::Instant::now();
        let mut stream = open(&server, CallContext::new(), vec![], true)
            .await
            .unwrap();
        let error = stream.next().await.unwrap().unwrap_err();
        let elapsed = started.elapsed();
        assert!(
            elapsed >= Duration::from_secs(9) && elapsed <= Duration::from_secs(10),
            "{elapsed:?}"
        );
        assert_eq!(error.code(), ErrorCode::Unavailable);
        assert_eq!(error.reason(), Some(reasons::STREAM_EXPIRED));
        assert_eq!(error.metadata()["max_lifetime_ms"], "10000");
        assert_eq!(error.metadata()["method"], "ticks");
        assert_eq!(server.in_flight(), 0);

        let deadline = (tokio::time::Instant::now() + Duration::from_secs(2)).into_std();
        let started = tokio::time::Instant::now();
        let mut stream = open(
            &server,
            CallContext::new().with_deadline(deadline),
            vec![],
            true,
        )
        .await
        .unwrap();
        let error = stream.next().await.unwrap().unwrap_err();
        assert_eq!(started.elapsed(), Duration::from_secs(2));
        assert_eq!(error.code(), ErrorCode::DeadlineExceeded);
        assert_eq!(error.reason(), None);

        // Both passed by the next poll: the one that passed first wins.
        let deadline = (tokio::time::Instant::now() + Duration::from_secs(20)).into_std();
        let mut stream = open(
            &server,
            CallContext::new().with_deadline(deadline),
            vec![],
            true,
        )
        .await
        .unwrap();
        tokio::time::advance(Duration::from_secs(30)).await;
        let error = stream.next().await.unwrap().unwrap_err();
        assert_eq!(error.reason(), Some(reasons::STREAM_EXPIRED));

        let unbounded = ticker(None, None);
        let deadline = (tokio::time::Instant::now() + Duration::from_secs(1)).into_std();
        let mut stream = open(
            &unbounded,
            CallContext::new().with_deadline(deadline),
            vec![],
            true,
        )
        .await
        .unwrap();
        tokio::time::advance(Duration::from_secs(5)).await;
        let error = stream.next().await.unwrap().unwrap_err();
        assert_eq!(error.code(), ErrorCode::DeadlineExceeded);
    }

    #[tokio::test(start_paused = true)]
    async fn the_open_is_bounded_by_its_deadline_and_the_lifetime() {
        let server = ticker(None, Some(Duration::from_secs(10)));
        let hang = |_cx: CallContext| async {
            std::future::pending::<Result<CallStream<u32, AppError>, AppError>>().await
        };
        let open_deadline = tokio::time::Instant::now() + Duration::from_secs(1);
        let error = server
            .open_stream(0, CallContext::new(), Some(open_deadline), "grpc", hang)
            .await
            .unwrap_err();
        assert_eq!(error.code(), ErrorCode::DeadlineExceeded);
        assert_eq!(error.reason(), None);
        assert_eq!(server.in_flight(), 0);

        let error = server
            .open_stream(0, CallContext::new(), None, "grpc", hang)
            .await
            .unwrap_err();
        assert_eq!(error.reason(), Some(reasons::STREAM_EXPIRED));

        server.close_streams();
        let closing = Arc::clone(&server);
        let opening = tokio::spawn(async move {
            closing
                .open_stream(0, CallContext::new(), None, "grpc", hang)
                .await
                .unwrap_err()
        });
        assert_eq!(opening.await.unwrap().reason(), Some(reasons::DRAINING));
    }

    #[tokio::test]
    async fn closing_during_the_open_refuses_it() {
        let server = ticker(None, None);
        let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
        let opening = {
            let server = Arc::clone(&server);
            tokio::spawn(async move {
                server
                    .open_stream(
                        0,
                        CallContext::new(),
                        None,
                        "local",
                        move |_cx| async move {
                            entered_tx.send(()).unwrap();
                            std::future::pending::<Result<CallStream<u32, AppError>, AppError>>()
                                .await
                        },
                    )
                    .await
                    .unwrap_err()
            })
        };
        entered_rx.await.unwrap();
        server.set_state(ComponentState::Draining);
        let error = opening.await.unwrap();
        assert_eq!(error.reason(), Some(reasons::DRAINING));
        assert_eq!(server.in_flight(), 0);
    }

    #[tokio::test]
    async fn a_remote_gate_reports_nothing_and_ends_streams_at_closing() {
        let remote = Arc::new(Server::new(
            TICKER,
            &[
                MethodPolicy {
                    max_lifetime: Some(Duration::from_secs(1)),
                    ..policy(None, Some(1))
                },
                MethodPolicy::default(),
            ],
            true,
        ));
        assert!(remote.open());
        assert!(remote.methods[0].max_lifetime.is_none());
        let mut stream = open(&remote, CallContext::new(), vec![Ok(5)], true)
            .await
            .unwrap();
        assert_eq!(stream.next().await.unwrap().unwrap(), 5);
        remote.set_state(ComponentState::Draining);
        assert_eq!(
            stream.next().await.unwrap().unwrap_err().reason(),
            Some(reasons::DRAINING)
        );
    }

    #[test]
    fn outcomes_have_names() {
        let names: Vec<&str> = [
            Outcome::Ended,
            Outcome::Failed,
            Outcome::Dropped,
            Outcome::Deadline,
            Outcome::Expired,
            Outcome::Closed,
            Outcome::Panicked,
        ]
        .into_iter()
        .map(Outcome::as_str)
        .collect();
        assert_eq!(
            names,
            [
                "ended", "failed", "dropped", "deadline", "expired", "closed", "panicked"
            ]
        );
        assert_eq!(expired(None).metadata()["max_lifetime_ms"], "0");
        assert_eq!(earliest(None, None), None);
    }

    #[tokio::test]
    async fn a_panicking_producer_reports_on_unwind() {
        let server = ticker(None, None);
        let stream = server
            .open_stream(0, CallContext::new(), None, "local", |_cx| async {
                Ok(CallStream::<u32, AppError>::new(futures::stream::poll_fn(
                    |_| -> Poll<Option<Result<u32, AppError>>> { panic!("producer") },
                )))
            })
            .await
            .unwrap();
        let mut stream = stream;
        let caught = tokio::spawn(async move { stream.next().await }).await;
        assert!(caught.unwrap_err().is_panic());
        assert_eq!(server.in_flight(), 0);

        let marked = server
            .open_stream(0, CallContext::new(), None, "grpc", |_cx| async {
                Ok(CallStream::<u32, AppError>::iter([Err(
                    AppError::internal(std::io::Error::other("x"))
                        .with_reason(reasons::HANDLER_PANICKED),
                )]))
            })
            .await
            .unwrap();
        let items: Vec<_> = futures::StreamExt::collect(marked).await;
        assert_eq!(items.len(), 1);
    }

    #[tokio::test]
    async fn a_stream_dropped_while_its_producer_unwinds_is_released() {
        let server = ticker(None, None);
        let stream = server
            .open_stream(0, CallContext::new(), None, "local", |_cx| async {
                Ok(CallStream::<u32, AppError>::new(futures::stream::poll_fn(
                    |_| -> Poll<Option<Result<u32, AppError>>> { panic!("producer") },
                )))
            })
            .await
            .unwrap();
        assert_eq!(server.in_flight(), 1);
        // Polled in this thread, so the stream is dropped during the unwind
        // (a tokio task would be dropped only after its panic was caught).
        let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
            let mut stream = stream;
            let mut cx = Context::from_waker(std::task::Waker::noop());
            let _ = Pin::new(&mut stream).poll_next(&mut cx);
        }));
        assert!(caught.is_err());
        assert_eq!(server.in_flight(), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn waiting_for_idle() {
        let server = std::sync::Arc::new(server(None));
        server.set_state(ComponentState::Serving);
        assert!(server.wait_idle(None).await);

        let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();
        let call = {
            let server = std::sync::Arc::clone(&server);
            tokio::spawn(async move {
                server
                    .run(1, CallContext::new(), |_| async move {
                        entered_tx.send(()).unwrap();
                        release_rx.await.unwrap();
                    })
                    .await
            })
        };
        entered_rx.await.unwrap();
        assert_eq!(server.in_flight(), 1);

        let expiry = tokio::time::Instant::now() + Duration::from_secs(5);
        assert!(!server.wait_idle(Some(expiry)).await);
        assert!(tokio::time::Instant::now() >= expiry);

        let waiter = {
            let server = std::sync::Arc::clone(&server);
            tokio::spawn(async move { server.wait_idle(None).await })
        };
        tokio::task::yield_now().await;
        release_tx.send(()).unwrap();
        call.await.unwrap().unwrap();
        assert!(waiter.await.unwrap());
        assert_eq!(server.in_flight(), 0);
    }
}
