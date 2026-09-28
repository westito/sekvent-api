//! The caller side of the `grpc` binding: one client per remote component.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use bytes::Bytes;
use http::HeaderMap;
use http::uri::PathAndQuery;
use sekvent_context::{CallContext, headers};
use sekvent_error::{AppError, ErrorCode};
use sekvent_link::BearerInjector;
use sekvent_resilience::{BreakerState, CircuitBreaker, Policy, PolicyError, PolicySpec, Timeout};
use tonic::metadata::MetadataMap;

use super::LazyChannel;
use super::codec::BytesCodec;
use crate::policy;
use crate::server::{MethodPolicy, tag};
use crate::{ComponentDescriptor, MethodDescriptor, reasons};

/// Calls one remote component: a lazily connected channel (shared with the
/// other components on the same endpoint), a gRPC path and a resilience
/// policy per method, and the link's bearer token.
pub(crate) struct RemoteClient {
    descriptor: &'static ComponentDescriptor,
    channel: Arc<LazyChannel>,
    bearer: Option<BearerInjector>,
    methods: Box<[RemoteMethod]>,
}

struct RemoteMethod {
    path: PathAndQuery,
    policy: Policy,
}

impl std::fmt::Debug for RemoteClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RemoteClient")
            .field("component", &self.descriptor.name())
            .field("authenticated", &self.bearer.is_some())
            .finish_non_exhaustive()
    }
}

impl RemoteClient {
    /// A client for `descriptor`; `policies` holds one resolved policy per
    /// method, `component` the component-level spec (breaker and budget).
    pub(crate) fn new(
        descriptor: &'static ComponentDescriptor,
        channel: Arc<LazyChannel>,
        bearer: Option<BearerInjector>,
        policies: &[MethodPolicy],
        component: &PolicySpec,
    ) -> Result<Self, AppError> {
        let name = descriptor.name();
        let service = super::service_name(descriptor);
        let breaker = breaker(name, component).map_err(|error| policy_failure(name, &error))?;
        let budget =
            Arc::new(policy::budget(component).map_err(|error| policy_failure(name, &error))?);
        let mut methods = Vec::with_capacity(descriptor.methods().len());
        for (method, resolved) in descriptor.methods().iter().zip(policies) {
            let path =
                PathAndQuery::try_from(format!("/{service}/{}", method.rpc())).map_err(|_| {
                    AppError::new(
                        ErrorCode::Internal,
                        format!(
                            "component {name} method {} has no valid gRPC path",
                            method.name()
                        ),
                    )
                })?;
            let mut policy = Policy::new(format!("{name}.{}", method.name()))
                .with_timeout(Timeout::deadline_only());
            if let Some(retry) = policy::method_retry(&resolved.spec)
                .map_err(|error| policy_failure(name, &error))?
            {
                policy = policy.with_retry(retry.with_budget(Arc::clone(&budget)));
            }
            if let Some(breaker) = &breaker {
                policy = policy.with_breaker(Arc::clone(breaker));
            }
            methods.push(RemoteMethod { path, policy });
        }
        Ok(Self {
            descriptor,
            channel,
            bearer,
            methods: methods.into_boxed_slice(),
        })
    }

    /// Run method `method` under its policy: breaker, budgeted retries of
    /// idempotent methods, the context deadline. Errors the pipeline makes
    /// itself are tagged; the server's errors are returned untouched.
    pub(crate) async fn call(
        &self,
        method: usize,
        cx: CallContext,
        body: Bytes,
    ) -> Result<Bytes, AppError> {
        let Some(remote) = self.methods.get(method) else {
            return Err(tag(
                self.descriptor,
                AppError::unimplemented(format!(
                    "component {} has no method with index {method}",
                    self.descriptor.name()
                )),
                method,
            ));
        };
        let idempotent = self
            .descriptor
            .methods()
            .get(method)
            .is_some_and(MethodDescriptor::is_idempotent);
        let attempted = AtomicBool::new(false);
        let outcome = remote
            .policy
            .call(&cx, idempotent, || {
                attempted.store(true, Ordering::Relaxed);
                self.attempt(method, &remote.path, &cx, body.clone())
            })
            .await;
        match outcome {
            // Rejected before any attempt: the breaker, or a dead context.
            Err(error) if !attempted.load(Ordering::Relaxed) => {
                Err(tag(self.descriptor, error, method))
            }
            outcome => outcome,
        }
    }

    /// One attempt: fresh headers (context, then the bearer), a ready
    /// channel, one unary call.
    async fn attempt(
        &self,
        method: usize,
        path: &PathAndQuery,
        cx: &CallContext,
        body: Bytes,
    ) -> Result<Bytes, AppError> {
        let mut headers = HeaderMap::new();
        headers::inject(cx, &mut headers);
        if let Some(bearer) = &self.bearer {
            bearer.apply(&mut headers);
        }
        let mut grpc = tonic::client::Grpc::new(self.channel.get());
        grpc.ready()
            .await
            .map_err(|error| self.unreachable(method, error))?;
        let mut request = tonic::Request::new(body);
        *request.metadata_mut() = MetadataMap::from_headers(headers);
        match grpc.unary(request, path.clone(), BytesCodec).await {
            Ok(response) => Ok(response.into_inner()),
            Err(status) => Err(self.classify(method, status)),
        }
    }

    /// A status with an underlying error came from this side's transport;
    /// anything else is the server's answer (or inferred from its HTTP
    /// status) and is decoded untouched. The transport enforces the
    /// `grpc-timeout` header the context injects, so its expiry is the
    /// call's deadline passing, not an unreachable peer.
    fn classify(&self, method: usize, status: tonic::Status) -> AppError {
        if std::error::Error::source(&status).is_none() {
            return sekvent_error::grpc::from_status(&status);
        }
        if timed_out(&status) {
            let error = AppError::deadline_exceeded("the call did not complete in time")
                .with_source(status);
            return tag(self.descriptor, error, method);
        }
        self.unreachable(method, status)
    }

    fn unreachable(
        &self,
        method: usize,
        source: impl std::error::Error + Send + Sync + 'static,
    ) -> AppError {
        let error = AppError::unavailable(format!(
            "component {} is unreachable",
            self.descriptor.name()
        ))
        .with_reason(reasons::UNREACHABLE)
        .with_source(source);
        tag(self.descriptor, error, method)
    }
}

/// Whether tonic's `grpc-timeout` enforcement produced `status`.
fn timed_out(status: &tonic::Status) -> bool {
    let mut source = std::error::Error::source(status);
    while let Some(error) = source {
        if error.is::<tonic::TimeoutExpired>() {
            return true;
        }
        source = error.source();
    }
    false
}

/// The component's breaker, logging when it opens and closes.
fn breaker(
    component: &'static str,
    spec: &PolicySpec,
) -> Result<Option<Arc<CircuitBreaker>>, PolicyError> {
    Ok(spec
        .build_breaker(format!("component:{component}"))?
        .map(|breaker| {
            Arc::new(
                breaker.on_state_change(move |transition| match transition.to {
                    BreakerState::Open => {
                        tracing::warn!(component, "component circuit breaker opened");
                    }
                    BreakerState::Closed => {
                        tracing::info!(component, "component circuit breaker closed");
                    }
                    BreakerState::HalfOpen => {}
                }),
            )
        }))
}

fn policy_failure(component: &str, error: &PolicyError) -> AppError {
    AppError::new(
        ErrorCode::Internal,
        format!(
            "component {component} has an invalid resilience policy: {}: {}",
            error.parameter(),
            error.reason()
        ),
    )
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    const METHODS: &[MethodDescriptor] = &[
        MethodDescriptor::call("reserve", "Reserve").with_idempotent(),
        MethodDescriptor::call("release", "Release"),
    ];
    const INVENTORY: &ComponentDescriptor =
        &ComponentDescriptor::new("inventory", "Inventory", METHODS).with_package("shop.v1");

    fn channel() -> Arc<LazyChannel> {
        // Port 1 on loopback: nothing listens there.
        Arc::new(LazyChannel::new(
            super::super::endpoint::endpoint("http://127.0.0.1:1").unwrap(),
        ))
    }

    fn client(component: &PolicySpec) -> Result<RemoteClient, AppError> {
        let mut spec = PolicySpec::default();
        spec.retry_max_attempts = Some(1);
        let policies = [
            MethodPolicy {
                timeout: None,
                spec: spec.clone(),
            },
            MethodPolicy {
                timeout: None,
                spec,
            },
        ];
        RemoteClient::new(INVENTORY, channel(), None, &policies, component)
    }

    #[test]
    fn paths_follow_the_service_name() {
        let client = client(&PolicySpec::default()).unwrap();
        assert_eq!(
            client.methods[0].path.as_str(),
            "/shop.v1.Inventory/Reserve"
        );
        assert_eq!(
            client.methods[1].path.as_str(),
            "/shop.v1.Inventory/Release"
        );
        assert_eq!(client.methods[0].policy.name(), "inventory.reserve");
        assert!(client.methods[0].policy.breaker().is_none());
        let debug = format!("{client:?}");
        assert!(debug.contains("inventory"), "{debug}");
    }

    #[test]
    fn invalid_component_policies_are_internal_errors() {
        let mut spec = PolicySpec::default();
        spec.retry_budget_ratio = Some(0.0);
        spec.retry_budget_min_per_sec = Some(0);
        let error = client(&spec).unwrap_err();
        assert_eq!(error.code(), ErrorCode::Internal);
        assert!(error.message().contains("retry_budget"), "{error}");

        let mut spec = PolicySpec::default();
        spec.breaker_enabled = Some(true);
        spec.breaker_failure_rate = Some(0.0);
        assert_eq!(client(&spec).unwrap_err().code(), ErrorCode::Internal);
    }

    #[tokio::test]
    async fn an_unknown_method_index_is_unimplemented() {
        let client = client(&PolicySpec::default()).unwrap();
        let error = client
            .call(9, CallContext::new(), Bytes::new())
            .await
            .unwrap_err();
        assert_eq!(error.code(), ErrorCode::Unimplemented);
        assert_eq!(error.metadata()["component"], "inventory");
    }

    #[tokio::test]
    async fn a_dead_context_is_rejected_before_any_attempt_and_tagged() {
        let client = client(&PolicySpec::default()).unwrap();
        let cx = CallContext::new();
        cx.cancel_token().cancel();
        let error = client.call(1, cx, Bytes::new()).await.unwrap_err();
        assert_eq!(error.code(), ErrorCode::Cancelled);
        assert_eq!(error.metadata()["component"], "inventory");
        assert_eq!(error.metadata()["method"], "release");
    }

    #[tokio::test]
    async fn nothing_listening_is_unreachable() {
        let client = client(&PolicySpec::default()).unwrap();
        let cx =
            CallContext::new().with_deadline(std::time::Instant::now() + Duration::from_secs(10));
        let error = client.call(1, cx, Bytes::new()).await.unwrap_err();
        assert_eq!(error.code(), ErrorCode::Unavailable);
        assert_eq!(error.reason(), Some(reasons::UNREACHABLE));
        assert_eq!(error.message(), "component inventory is unreachable");
        assert_eq!(error.metadata()["component"], "inventory");
        assert_eq!(error.metadata()["method"], "release");
    }

    #[test]
    fn server_statuses_are_decoded_untouched() {
        let client = client(&PolicySpec::default()).unwrap();
        let status =
            sekvent_error::grpc::to_status(&AppError::not_found("gone").with_reason("GONE"));
        let error = client.classify(0, status);
        assert_eq!(error.code(), ErrorCode::NotFound);
        assert_eq!(error.reason(), Some("GONE"));
        assert!(!error.metadata().contains_key("component"));
    }

    #[test]
    fn the_transport_timeout_is_the_deadline_passing() {
        let client = client(&PolicySpec::default()).unwrap();
        let mut status = tonic::Status::cancelled("Timeout expired");
        status.set_source(Arc::new(tonic::TimeoutExpired(())));
        let error = client.classify(0, status);
        assert_eq!(error.code(), ErrorCode::DeadlineExceeded);
        assert_eq!(error.metadata()["component"], "inventory");
        assert_eq!(error.metadata()["method"], "reserve");

        let mut status = tonic::Status::unavailable("connection reset");
        status.set_source(Arc::new(std::io::Error::other("reset")));
        let error = client.classify(0, status);
        assert_eq!(error.code(), ErrorCode::Unavailable);
        assert_eq!(error.reason(), Some(reasons::UNREACHABLE));
    }
}
