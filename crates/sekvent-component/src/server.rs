//! The serving side of an installed component: the gate that admits calls
//! only while the component is serving and counts those in flight, and the
//! per-method policies.

use std::future::Future;
use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};
use std::time::Duration;

use sekvent_context::CallContext;
use sekvent_error::AppError;
use sekvent_resilience::{Bulkhead, Timeout};
use tokio::sync::Notify;

use crate::{ComponentDescriptor, ComponentState, reasons};

/// The resolved policy of one method.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct MethodPolicy {
    /// Time limit of one call; `None` leaves only the caller's deadline.
    pub(crate) timeout: Option<Duration>,
    /// Cap on concurrent calls; `None` for no cap.
    pub(crate) bulkhead: Option<u32>,
}

/// One installed, in-process component.
#[derive(Debug)]
pub(crate) struct Server {
    descriptor: &'static ComponentDescriptor,
    gate: Gate,
    methods: Box<[MethodRuntime]>,
}

#[derive(Debug)]
struct MethodRuntime {
    policy: MethodPolicy,
    bulkhead: Option<Bulkhead>,
}

#[derive(Debug)]
struct Gate {
    state: AtomicU8,
    in_flight: AtomicUsize,
    idle: Notify,
}

/// Keeps one admitted call counted until dropped.
struct Admission<'a>(&'a Gate);

impl Drop for Admission<'_> {
    fn drop(&mut self) {
        if self.0.in_flight.fetch_sub(1, Ordering::SeqCst) == 1 {
            self.0.idle.notify_waiters();
        }
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
    /// entry per method of `descriptor`, in order.
    pub(crate) fn new(descriptor: &'static ComponentDescriptor, policies: &[MethodPolicy]) -> Self {
        let methods = policies
            .iter()
            .map(|policy| MethodRuntime {
                policy: *policy,
                bulkhead: policy.bulkhead.and_then(|size| Bulkhead::new(size).ok()),
            })
            .collect();
        Self {
            descriptor,
            gate: Gate {
                state: AtomicU8::new(state_code(ComponentState::NotStarted)),
                in_flight: AtomicUsize::new(0),
                idle: Notify::new(),
            },
            methods,
        }
    }

    pub(crate) fn descriptor(&self) -> &'static ComponentDescriptor {
        self.descriptor
    }

    /// The resolved policy of method `method`; the default for an unknown
    /// index.
    pub(crate) fn policy(&self, method: usize) -> MethodPolicy {
        self.methods
            .get(method)
            .map(|runtime| runtime.policy)
            .unwrap_or_default()
    }

    pub(crate) fn state(&self) -> ComponentState {
        state_of(self.gate.state.load(Ordering::SeqCst))
    }

    pub(crate) fn set_state(&self, state: ComponentState) {
        self.gate.state.store(state_code(state), Ordering::SeqCst);
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
        let name = self.descriptor.name();
        match self.state() {
            ComponentState::Serving => Ok(admission),
            ComponentState::NotStarted => Err(AppError::unavailable(format!(
                "component {name} is not started"
            ))
            .with_reason(reasons::NOT_STARTED)),
            ComponentState::Draining => Err(AppError::unavailable(format!(
                "component {name} is shutting down"
            ))
            .with_reason(reasons::DRAINING)),
            ComponentState::Stopped => Err(AppError::unavailable(format!(
                "component {name} is stopped"
            ))
            .with_reason(reasons::STOPPED)),
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
    use sekvent_error::ErrorCode;

    use super::*;
    use crate::MethodDescriptor;

    const METHODS: &[MethodDescriptor] = &[
        MethodDescriptor::call("reserve", "Reserve"),
        MethodDescriptor::call("release", "Release"),
    ];
    const INVENTORY: &ComponentDescriptor =
        &ComponentDescriptor::new("inventory", "Inventory", METHODS);

    fn server(bulkhead: Option<u32>) -> Server {
        Server::new(
            INVENTORY,
            &[
                MethodPolicy {
                    timeout: Some(Duration::from_secs(1)),
                    bulkhead,
                },
                MethodPolicy::default(),
            ],
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
        assert_eq!(server.policy(0).timeout, Some(Duration::from_secs(1)));
        assert_eq!(server.policy(0).bulkhead, Some(2));
        assert_eq!(server.policy(1), MethodPolicy::default());
        assert_eq!(server.policy(9), MethodPolicy::default());
        assert_eq!(server.descriptor().name(), "inventory");
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
