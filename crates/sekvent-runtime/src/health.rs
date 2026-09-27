use std::collections::BTreeMap;
use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use tokio::sync::watch;
use tonic_health::ServingStatus;
use tonic_health::pb::health_server::HealthServer;
use tonic_health::server::{HealthReporter, HealthService};

use crate::probe::ProbeStatus;

/// Whether one named service is taking traffic.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ServiceStatus {
    /// Taking traffic.
    Serving,
    /// Not taking traffic.
    NotServing,
}

impl ServiceStatus {
    fn to_grpc(self) -> ServingStatus {
        match self {
            Self::Serving => ServingStatus::Serving,
            Self::NotServing => ServingStatus::NotServing,
        }
    }

    fn from_ready(ready: bool) -> Self {
        if ready {
            Self::Serving
        } else {
            Self::NotServing
        }
    }
}

/// A point-in-time view of liveness and readiness.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
#[allow(clippy::struct_excessive_bools)] // independent probe answers, not a state machine.
pub struct Readiness {
    /// Whether the service should receive traffic.
    pub ready: bool,
    /// Whether the process is healthy enough to keep running.
    pub live: bool,
    /// Whether every stage has started.
    pub started: bool,
    /// Whether shutdown has begun.
    pub draining: bool,
    /// Build version, if one was set.
    pub version: Option<String>,
    /// Every registered dependency probe, by name.
    pub probes: Vec<ProbeState>,
}

/// The last known state of one dependency probe.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ProbeState {
    /// Probe name.
    pub name: String,
    /// Whether readiness depends on it.
    pub required: bool,
    /// Last result; `None` until the first probe completes.
    pub status: Option<ProbeStatus>,
}

/// The process-wide health state: per-service serving status, liveness,
/// readiness and dependency probes.
///
/// Cheap to clone; every clone shares the same state. The `grpc.health.v1`
/// service returned by [`grpc_service`](Self::grpc_service) is kept in sync:
/// the empty service name reports overall readiness, and every name set with
/// [`set_status`](Self::set_status) reports its own status.
///
/// Readiness is true when every stage has started, shutdown has not begun,
/// the process is not marked fatal, and every *required* probe last reported
/// [`ProbeStatus::Up`].
#[derive(Clone)]
pub struct HealthRegistry {
    inner: Arc<Inner>,
}

struct Inner {
    state: Mutex<State>,
    reporter: HealthReporter,
    /// Serializes pushes to the gRPC reporter so a stale snapshot never
    /// overwrites a newer one.
    publishing: tokio::sync::Mutex<()>,
    ready: watch::Sender<bool>,
}

#[derive(Default)]
struct State {
    services: BTreeMap<String, ServiceStatus>,
    probes: BTreeMap<String, ProbeEntry>,
    started: bool,
    draining: bool,
    fatal: Option<&'static str>,
    version: Option<String>,
}

struct ProbeEntry {
    required: bool,
    status: Option<ProbeStatus>,
}

impl State {
    fn is_ready(&self) -> bool {
        self.started
            && !self.draining
            && self.fatal.is_none()
            && self
                .probes
                .values()
                .all(|probe| !probe.required || probe.status.is_some_and(ProbeStatus::is_up))
    }
}

impl Default for HealthRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for HealthRegistry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HealthRegistry")
            .field("readiness", &self.readiness())
            .finish_non_exhaustive()
    }
}

impl HealthRegistry {
    /// A registry that is live but not ready until the runtime has started.
    pub fn new() -> Self {
        let (ready, _) = watch::channel(false);
        Self {
            inner: Arc::new(Inner {
                state: Mutex::new(State::default()),
                reporter: HealthReporter::new(),
                publishing: tokio::sync::Mutex::new(()),
                ready,
            }),
        }
    }

    /// Record the build version shown by full health output.
    pub fn set_version(&self, version: impl Into<String>) {
        self.state().version = Some(version.into());
    }

    /// Set the status of one named service (for gRPC, the fully qualified
    /// service name). The empty name is the overall status, which the
    /// registry derives from readiness; setting it here is ignored.
    pub async fn set_status(&self, service: &str, status: ServiceStatus) {
        if service.is_empty() {
            tracing::warn!("the overall health status is derived from readiness and cannot be set");
            return;
        }
        self.update(|state| {
            state.services.insert(service.to_owned(), status);
        });
        self.publish().await;
    }

    /// The status of one named service, if it was ever set.
    pub fn status(&self, service: &str) -> Option<ServiceStatus> {
        self.state().services.get(service).copied()
    }

    /// Begin draining: readiness turns false and every service reports
    /// [`ServiceStatus::NotServing`]. The runtime calls this first thing on
    /// shutdown.
    pub async fn set_all_not_serving(&self) {
        self.update(|state| {
            state.draining = true;
            for status in state.services.values_mut() {
                *status = ServiceStatus::NotServing;
            }
        });
        self.publish().await;
    }

    /// Mark the process as unrecoverable: liveness fails, so an orchestrator
    /// restarts it. `reason` is static and caller-safe; it is logged, never
    /// served.
    pub async fn mark_fatal(&self, reason: &'static str) {
        tracing::error!(reason, "process marked fatal");
        self.update(|state| state.fatal = Some(reason));
        self.publish().await;
    }

    /// Whether the process should keep running.
    pub fn is_live(&self) -> bool {
        self.state().fatal.is_none()
    }

    /// Whether the service should receive traffic.
    pub fn is_ready(&self) -> bool {
        self.state().is_ready()
    }

    /// A snapshot of everything health output reports.
    pub fn readiness(&self) -> Readiness {
        let state = self.state();
        Readiness {
            ready: state.is_ready(),
            live: state.fatal.is_none(),
            started: state.started,
            draining: state.draining,
            version: state.version.clone(),
            probes: state
                .probes
                .iter()
                .map(|(name, probe)| ProbeState {
                    name: name.clone(),
                    required: probe.required,
                    status: probe.status,
                })
                .collect(),
        }
    }

    /// Follow readiness changes.
    pub fn watch_ready(&self) -> watch::Receiver<bool> {
        self.inner.ready.subscribe()
    }

    /// The `grpc.health.v1.Health` service backed by this registry.
    pub fn grpc_service(&self) -> HealthServer<HealthService> {
        HealthServer::new(HealthService::from_health_reporter(
            self.inner.reporter.clone(),
        ))
    }

    pub(crate) async fn mark_started(&self) {
        self.update(|state| state.started = true);
        self.publish().await;
    }

    pub(crate) fn register_probe(&self, name: &str, required: bool) {
        self.update(|state| {
            state.probes.insert(
                name.to_owned(),
                ProbeEntry {
                    required,
                    status: None,
                },
            );
        });
    }

    /// Record a probe result; `true` when it changed.
    pub(crate) fn record_probe(&self, name: &str, status: ProbeStatus) -> bool {
        self.update(|state| match state.probes.get_mut(name) {
            Some(probe) if probe.status != Some(status) => {
                probe.status = Some(status);
                true
            }
            _ => false,
        })
    }

    /// Push the current state to the gRPC health service.
    pub(crate) async fn publish(&self) {
        let _serialized = self.inner.publishing.lock().await;
        let (overall, services) = {
            let state = self.state();
            (
                ServiceStatus::from_ready(state.is_ready()),
                state.services.clone(),
            )
        };
        let reporter = &self.inner.reporter;
        reporter.set_service_status("", overall.to_grpc()).await;
        for (name, status) in services {
            reporter.set_service_status(name, status.to_grpc()).await;
        }
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.inner
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    fn update<R>(&self, change: impl FnOnce(&mut State) -> R) -> R {
        let mut state = self.state();
        let result = change(&mut state);
        let ready = state.is_ready();
        drop(state);
        self.inner.ready.send_if_modified(|current| {
            let changed = *current != ready;
            *current = ready;
            changed
        });
        result
    }
}

#[cfg(test)]
mod tests {
    use tonic::Request;
    use tonic_health::pb::HealthCheckRequest;
    use tonic_health::pb::health_check_response::ServingStatus as Wire;
    use tonic_health::pb::health_server::Health;

    use super::*;
    use crate::probe::ProbeFailure;

    async fn grpc_status(registry: &HealthRegistry, service: &str) -> Option<Wire> {
        let service_impl = HealthService::from_health_reporter(registry.inner.reporter.clone());
        let request = Request::new(HealthCheckRequest {
            service: service.to_owned(),
        });
        service_impl
            .check(request)
            .await
            .ok()
            .map(|response| response.into_inner().status())
    }

    #[tokio::test]
    async fn readiness_follows_start_and_drain() {
        let registry = HealthRegistry::new();
        let mut ready = registry.watch_ready();
        assert!(registry.is_live());
        assert!(!registry.is_ready());

        registry.mark_started().await;
        assert!(registry.is_ready());
        assert!(ready.has_changed().unwrap());
        assert!(*ready.borrow_and_update());
        assert_eq!(grpc_status(&registry, "").await, Some(Wire::Serving));

        registry.set_all_not_serving().await;
        let snapshot = registry.readiness();
        assert!(!snapshot.ready && snapshot.draining && snapshot.started && snapshot.live);
        assert!(!*ready.borrow_and_update());
        assert_eq!(grpc_status(&registry, "").await, Some(Wire::NotServing));
    }

    #[tokio::test]
    async fn required_probes_gate_readiness_and_optional_ones_do_not() {
        let registry = HealthRegistry::new();
        registry.register_probe("db", true);
        registry.register_probe("cache", false);
        registry.mark_started().await;
        assert!(
            !registry.is_ready(),
            "an unprobed required dependency is not ready"
        );

        assert!(registry.record_probe("db", ProbeStatus::Up));
        assert!(!registry.record_probe("db", ProbeStatus::Up), "unchanged");
        assert!(registry.is_ready(), "optional probe still unknown");

        let down = ProbeStatus::Down(ProbeFailure::Unreachable("no route"));
        assert!(registry.record_probe("cache", down));
        assert!(registry.is_ready());

        let rejected = ProbeStatus::Down(ProbeFailure::Rejected("bad credentials"));
        assert!(registry.record_probe("db", rejected));
        assert!(!registry.is_ready());

        assert!(!registry.record_probe("unknown", ProbeStatus::Up));
        let probes = registry.readiness().probes;
        assert_eq!(probes.len(), 2);
        assert_eq!(probes[0].name, "cache");
        assert_eq!(probes[1].status, Some(rejected));
    }

    #[tokio::test]
    async fn named_services_are_synced_to_grpc() {
        let registry = HealthRegistry::new();
        registry
            .set_status("orders.v1.Orders", ServiceStatus::Serving)
            .await;
        assert_eq!(
            registry.status("orders.v1.Orders"),
            Some(ServiceStatus::Serving)
        );
        assert_eq!(
            grpc_status(&registry, "orders.v1.Orders").await,
            Some(Wire::Serving)
        );

        registry.set_status("", ServiceStatus::Serving).await;
        assert!(!registry.is_ready(), "the overall status cannot be forced");
        assert_eq!(registry.status(""), None);

        registry.set_all_not_serving().await;
        assert_eq!(
            registry.status("orders.v1.Orders"),
            Some(ServiceStatus::NotServing)
        );
        assert_eq!(
            grpc_status(&registry, "orders.v1.Orders").await,
            Some(Wire::NotServing)
        );
        assert_eq!(grpc_status(&registry, "unknown").await, None);
    }

    #[tokio::test]
    async fn a_fatal_process_is_neither_live_nor_ready() {
        let registry = HealthRegistry::default();
        registry.set_version("1.2.3");
        registry.mark_started().await;
        registry.mark_fatal("state corrupted").await;
        let snapshot = registry.readiness();
        assert!(!snapshot.live && !snapshot.ready);
        assert_eq!(snapshot.version.as_deref(), Some("1.2.3"));
        assert!(format!("{registry:?}").contains("HealthRegistry"));
    }
}
