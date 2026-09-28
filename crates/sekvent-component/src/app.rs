//! The App: installing components, building them fail-closed, and running
//! their lifecycle.

use std::any::{Any, TypeId, type_name};
use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use sekvent_config::ConfigSource;
use sekvent_error::AppError;
use tokio::sync::watch;

use crate::__private::Dispatch;
use crate::config::{self, Entry, Resolved, Settings};
use crate::lifecycle::LifecycleDyn;
use crate::link::Link;
use crate::server::Server;
use crate::{Binding, BuildError, ComponentDescriptor, ComponentHandle, reasons};

/// Where a component is in its lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ComponentState {
    /// Built, not started: calls are rejected.
    NotStarted,
    /// Accepting calls.
    Serving,
    /// Stopping: new calls are rejected while in-flight calls finish.
    Draining,
    /// Stopped: calls are rejected.
    Stopped,
}

/// What a factory run produced, with the handle type erased.
pub(crate) struct Built {
    pub(crate) handle: Box<dyn Any + Send + Sync>,
    pub(crate) lifecycle: Option<Arc<dyn LifecycleDyn>>,
    /// The byte-level dispatcher, which serving over gRPC needs.
    pub(crate) dispatch: Option<Arc<dyn Dispatch>>,
}

/// A type-erased factory: builds the implementation and the handle for the
/// given link.
pub(crate) type Factory =
    Box<dyn FnOnce(&mut Deps<'_>, Arc<Link>) -> Result<Built, AppError> + Send>;

/// Box a factory closure, fixing its signature.
pub(crate) fn erase<F>(factory: F) -> Factory
where
    F: FnOnce(&mut Deps<'_>, Arc<Link>) -> Result<Built, AppError> + Send + 'static,
{
    Box::new(factory)
}

/// Makes the handle of a component bound to a remote transport; `None` when
/// the link has no remote client.
pub(crate) type RemoteFactory = Box<dyn FnOnce(Arc<Link>) -> Option<Handle> + Send>;

struct Install {
    descriptor: &'static ComponentDescriptor,
    handle_type: TypeId,
    /// `None` for a `remote_only` component.
    factory: Option<Factory>,
    remote: RemoteFactory,
}

struct Resource {
    type_name: &'static str,
    value: Box<dyn Any + Send + Sync>,
}

pub(crate) type Handle = Box<dyn Any + Send + Sync>;

/// Collects components and resources, then builds an [`App`].
///
/// Create it with [`App::builder`], install components with their generated
/// handles (`InventoryHandle::install(&mut builder, factory)`), then call
/// [`build`](Self::build).
pub struct AppBuilder<'a> {
    source: &'a dyn ConfigSource,
    installs: Vec<Install>,
    resources: HashMap<TypeId, Resource>,
}

impl AppBuilder<'_> {
    /// Make `value` available to factories through [`Deps::resource`]. A
    /// second value of the same type is an error.
    pub fn provide<T: Clone + Send + Sync + 'static>(
        &mut self,
        value: T,
    ) -> Result<(), BuildError> {
        let type_name = type_name::<T>();
        if self.resources.contains_key(&TypeId::of::<T>()) {
            return Err(BuildError::DuplicateResource { type_name });
        }
        self.resources.insert(
            TypeId::of::<T>(),
            Resource {
                type_name,
                value: Box::new(value),
            },
        );
        Ok(())
    }

    /// Record a component; duplicates (by handle type or by name) are
    /// rejected at once, everything else is checked in [`build`](Self::build).
    pub(crate) fn install<H: ComponentHandle>(
        &mut self,
        factory: Option<Factory>,
        remote: RemoteFactory,
    ) -> Result<(), BuildError> {
        let descriptor = H::DESCRIPTOR;
        let handle_type = TypeId::of::<H>();
        if self.installs.iter().any(|install| {
            install.handle_type == handle_type || install.descriptor.name() == descriptor.name()
        }) {
            return Err(BuildError::DuplicateInstall {
                component: descriptor.name().to_owned(),
            });
        }
        self.installs.push(Install {
            descriptor,
            handle_type,
            factory,
            remote,
        });
        Ok(())
    }

    /// Validate the configuration, resolve every binding and policy, and run
    /// the factories of the components that run in this process, in install
    /// order.
    ///
    /// Every configuration problem is reported at once, and no factory runs
    /// unless the configuration is clean. The first factory error stops the
    /// build.
    pub fn build(self) -> Result<App, BuildError> {
        let entries: Vec<Entry> = self
            .installs
            .iter()
            .map(|install| Entry {
                descriptor: install.descriptor,
                remote: install.factory.is_none(),
            })
            .collect();
        let settings = config::resolve(self.source, &entries)?;
        let installed: Vec<(TypeId, &'static ComponentDescriptor)> = self
            .installs
            .iter()
            .map(|install| (install.handle_type, install.descriptor))
            .collect();
        let mut remotes = Remotes::new(&settings);

        let mut handles: Vec<Option<Handle>> = Vec::with_capacity(self.installs.len());
        let mut parts = Vec::with_capacity(self.installs.len());
        let mut exposed = Vec::new();
        for (position, (install, resolved)) in self
            .installs
            .into_iter()
            .zip(&settings.components)
            .enumerate()
        {
            let descriptor = install.descriptor;
            let name = descriptor.name();
            let remote = resolved.binding == Binding::Grpc;
            let server = Arc::new(Server::new(descriptor, &resolved.policies, remote));
            let link = Link::new(resolved.binding, Arc::clone(&server), settings.max_hops);
            let built = if remote {
                let link = remotes.attach(link, descriptor, resolved)?;
                let handle = (install.remote)(Arc::new(link)).ok_or_else(|| {
                    BuildError::BindingUnavailable {
                        component: name.to_owned(),
                        binding: resolved.binding,
                        key: resolved.key.clone(),
                    }
                })?;
                Built {
                    handle,
                    lifecycle: None,
                    dispatch: None,
                }
            } else {
                // A component without a factory is remote_only and resolves
                // to a remote binding only.
                let Some(factory) = install.factory else {
                    handles.push(None);
                    continue;
                };
                let mut deps = Deps {
                    component: name,
                    binding: resolved.binding,
                    position,
                    installed: &installed,
                    built: &handles,
                    resources: &self.resources,
                    source: self.source,
                };
                factory(&mut deps, Arc::new(link)).map_err(|source| BuildError::Factory {
                    component: name.to_owned(),
                    source,
                })?
            };
            log_built(descriptor, resolved);
            if let Some(serve) = &resolved.serve {
                exposed.push(expose(
                    &settings,
                    descriptor,
                    serve,
                    &server,
                    built.dispatch.clone(),
                )?);
            }
            handles.push(Some(built.handle));
            parts.push((descriptor, resolved.binding, server, built.lifecycle));
        }

        let components = parts
            .into_iter()
            .zip(handles.into_iter().flatten())
            .map(
                |((descriptor, binding, server, lifecycle), handle)| Component {
                    descriptor,
                    binding,
                    handle,
                    server,
                    lifecycle,
                },
            )
            .collect();
        Ok(App {
            inner: Arc::new(Inner {
                components,
                exposed,
                mounted: AtomicBool::new(false),
                phase: watch::Sender::new(Phase::Built),
            }),
        })
    }
}

/// Log the binding and the method policies a component was built with.
fn log_built(descriptor: &ComponentDescriptor, resolved: &Resolved) {
    let name = descriptor.name();
    tracing::debug!(
        component = name,
        binding = %resolved.binding,
        key = %resolved.key,
        "component built"
    );
    for (method, policy) in descriptor.methods().iter().zip(&resolved.policies) {
        tracing::debug!(
            component = name,
            method = method.name(),
            timeout = ?policy.timeout,
            bulkhead = ?policy.spec.bulkhead_max_concurrent,
            retry_max_attempts = ?policy.spec.retry_max_attempts,
            "method policy"
        );
    }
}

/// Builds the remote clients of `grpc`-bound components, sharing one
/// channel per endpoint.
struct Remotes<'s> {
    #[cfg_attr(not(feature = "grpc"), allow(dead_code))]
    settings: &'s Settings,
    #[cfg(feature = "grpc")]
    channels: crate::grpc::ChannelCache,
}

impl<'s> Remotes<'s> {
    fn new(settings: &'s Settings) -> Self {
        Self {
            settings,
            #[cfg(feature = "grpc")]
            channels: crate::grpc::ChannelCache::default(),
        }
    }

    /// Give `link` the remote client its component's settings describe.
    #[cfg(feature = "grpc")]
    fn attach(
        &mut self,
        link: Link,
        descriptor: &'static ComponentDescriptor,
        resolved: &Resolved,
    ) -> Result<Link, BuildError> {
        use crate::grpc::client::RemoteClient;

        let name = descriptor.name();
        let failed = |source: AppError| BuildError::Factory {
            component: name.to_owned(),
            source,
        };
        let Some(remote) = &resolved.remote else {
            return Ok(link);
        };
        let channel = self.channels.channel(&remote.endpoint).map_err(|error| {
            failed(
                AppError::unavailable(format!("component {name} has an unusable endpoint"))
                    .with_source(error),
            )
        })?;
        let bearer = if remote.auth {
            self.settings
                .links
                .as_ref()
                .and_then(|links| links.outbound(&remote.link))
                .cloned()
        } else {
            None
        };
        let client = RemoteClient::new(
            descriptor,
            channel,
            bearer,
            &resolved.policies,
            &resolved.component,
        )
        .map_err(failed)?;
        Ok(link.with_remote(Arc::new(client)))
    }

    /// Without the `grpc` feature no component resolves to a remote
    /// binding.
    #[cfg(not(feature = "grpc"))]
    #[allow(clippy::unused_self, clippy::unnecessary_wraps)]
    fn attach(
        &mut self,
        link: Link,
        _descriptor: &'static ComponentDescriptor,
        _resolved: &Resolved,
    ) -> Result<Link, BuildError> {
        Ok(link)
    }
}

/// One component exposed over gRPC.
struct Exposed {
    component: &'static str,
    /// Its `SERVE` key.
    key: String,
    /// Its full gRPC service name.
    service: String,
    #[cfg(feature = "grpc")]
    served: Arc<crate::grpc::service::Served>,
}

/// Prepare serving `descriptor` over gRPC through `server` and `dispatch`.
#[cfg(feature = "grpc")]
fn expose(
    settings: &Settings,
    descriptor: &'static ComponentDescriptor,
    serve: &config::Serve,
    server: &Arc<Server>,
    dispatch: Option<Arc<dyn Dispatch>>,
) -> Result<Exposed, BuildError> {
    let Some(dispatch) = dispatch else {
        return Err(BuildError::NotServable {
            component: descriptor.name().to_owned(),
            key: serve.key.clone(),
            reason: "its implementation has no byte-level dispatcher",
        });
    };
    let inbound = if serve.auth {
        settings
            .links
            .as_ref()
            .map(|links| Arc::clone(links.inbound()))
    } else {
        None
    };
    Ok(Exposed {
        component: descriptor.name(),
        key: serve.key.clone(),
        service: crate::grpc::service_name(descriptor),
        served: Arc::new(crate::grpc::service::Served::new(
            Arc::clone(server),
            dispatch,
            inbound,
            settings.max_hops,
        )),
    })
}

/// Without the `grpc` feature nothing resolves to be served.
#[cfg(not(feature = "grpc"))]
fn expose(
    _settings: &Settings,
    descriptor: &'static ComponentDescriptor,
    serve: &config::Serve,
    _server: &Arc<Server>,
    _dispatch: Option<Arc<dyn Dispatch>>,
) -> Result<Exposed, BuildError> {
    Ok(Exposed {
        component: descriptor.name(),
        key: serve.key.clone(),
        service: descriptor
            .full_service_name()
            .unwrap_or_else(|| descriptor.service().to_owned()),
    })
}

impl fmt::Debug for AppBuilder<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let components: Vec<&str> = self
            .installs
            .iter()
            .map(|install| install.descriptor.name())
            .collect();
        let mut resources: Vec<&str> = self
            .resources
            .values()
            .map(|resource| resource.type_name)
            .collect();
        resources.sort_unstable();
        f.debug_struct("AppBuilder")
            .field("components", &components)
            .field("resources", &resources)
            .finish_non_exhaustive()
    }
}

/// A factory's view of the build in progress.
pub struct Deps<'b> {
    component: &'static str,
    binding: Binding,
    position: usize,
    installed: &'b [(TypeId, &'static ComponentDescriptor)],
    built: &'b [Option<Handle>],
    resources: &'b HashMap<TypeId, Resource>,
    source: &'b dyn ConfigSource,
}

impl Deps<'_> {
    /// The component being built.
    pub fn component(&self) -> &'static str {
        self.component
    }

    /// Its resolved binding.
    pub fn binding(&self) -> Binding {
        self.binding
    }

    /// The handle of a component installed before this one.
    ///
    /// `FAILED_PRECONDITION`, naming both components, when that component is
    /// not installed, is installed at or after this one, or failed to build.
    pub fn handle<H: ComponentHandle>(&self) -> Result<H, AppError> {
        let me = self.component;
        let wanted = H::DESCRIPTOR.name();
        let fail = |message: String| Err(AppError::failed_precondition(message));
        let Some(index) = self
            .installed
            .iter()
            .position(|(handle_type, _)| *handle_type == TypeId::of::<H>())
        else {
            return fail(format!(
                "component {me} depends on {wanted}, which is not installed"
            ));
        };
        if index == self.position {
            return fail(format!("component {me} depends on itself"));
        }
        if index > self.position {
            return fail(format!(
                "component {me} depends on {wanted}; install {wanted} before {me}"
            ));
        }
        match self
            .built
            .get(index)
            .and_then(Option::as_ref)
            .and_then(|handle| handle.downcast_ref::<H>())
        {
            Some(handle) => Ok(handle.clone()),
            None => fail(format!(
                "component {me} depends on {wanted}, which failed to build"
            )),
        }
    }

    /// A value given to [`AppBuilder::provide`]; `FAILED_PRECONDITION`
    /// naming the type when there is none.
    pub fn resource<T: Clone + Send + Sync + 'static>(&self) -> Result<T, AppError> {
        self.resources
            .get(&TypeId::of::<T>())
            .and_then(|resource| resource.value.downcast_ref::<T>())
            .cloned()
            .ok_or_else(|| {
                AppError::failed_precondition(format!(
                    "component {} needs a resource of type {}, which is not provided",
                    self.component,
                    type_name::<T>()
                ))
            })
    }

    /// The configuration source the App is built from.
    pub fn config(&self) -> &dyn ConfigSource {
        self.source
    }
}

impl fmt::Debug for Deps<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Deps")
            .field("component", &self.component)
            .field("binding", &self.binding)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Built,
    Starting,
    Running,
    Stopping,
    Stopped,
}

struct Component {
    descriptor: &'static ComponentDescriptor,
    binding: Binding,
    handle: Handle,
    server: Arc<Server>,
    lifecycle: Option<Arc<dyn LifecycleDyn>>,
}

struct Inner {
    components: Vec<Component>,
    /// The components exposed over gRPC, in install order.
    exposed: Vec<Exposed>,
    /// Whether [`App::grpc_routes`] was called.
    mounted: AtomicBool,
    phase: watch::Sender<Phase>,
}

/// A built set of components. Cheap to clone; clones share everything.
#[derive(Clone)]
pub struct App {
    inner: Arc<Inner>,
}

impl App {
    /// Start collecting components, configured from `source`.
    pub fn builder(source: &dyn ConfigSource) -> AppBuilder<'_> {
        AppBuilder {
            source,
            installs: Vec::new(),
            resources: HashMap::new(),
        }
    }

    /// A handle for code outside the components (ingress, tests).
    /// `FAILED_PRECONDITION` when the component is not installed.
    pub fn handle<H: ComponentHandle>(&self) -> Result<H, AppError> {
        self.inner
            .components
            .iter()
            .find_map(|component| component.handle.downcast_ref::<H>())
            .cloned()
            .ok_or_else(|| {
                AppError::failed_precondition(format!(
                    "component {} is not installed",
                    H::DESCRIPTOR.name()
                ))
            })
    }

    /// The binding of the component called `component`.
    pub fn binding(&self, component: &str) -> Option<Binding> {
        self.find(component).map(|component| component.binding)
    }

    /// The state of the component called `component`.
    pub fn state(&self, component: &str) -> Option<ComponentState> {
        self.find(component)
            .map(|component| component.server.state())
    }

    /// Every component name, in install order.
    pub fn components(&self) -> Vec<&'static str> {
        self.inner
            .components
            .iter()
            .map(|component| component.descriptor.name())
            .collect()
    }

    /// Full gRPC service names of the components exposed over gRPC
    /// (`SEKVENT_COMPONENT_<C>_SERVE=grpc`), in install order. Empty without
    /// the `grpc` feature.
    pub fn grpc_services(&self) -> Vec<String> {
        self.inner
            .exposed
            .iter()
            .map(|exposed| exposed.service.clone())
            .collect()
    }

    /// tonic routes serving every exposed component, for
    /// `sekvent_runtime::ServerBuilder::grpc_routes` (or tonic's own
    /// server). Empty when nothing is exposed. Calling it marks the routes
    /// mounted.
    #[cfg(feature = "grpc")]
    pub fn grpc_routes(&self) -> tonic::service::Routes {
        self.inner.mounted.store(true, Ordering::SeqCst);
        crate::grpc::service::routes(self.inner.exposed.iter().map(|exposed| &exposed.served))
    }

    /// Run every component's `on_start` hook in install order, opening each
    /// component for calls once its hook succeeded.
    ///
    /// On the first failure the components already started are stopped in
    /// reverse order, the App is stopped, and the error is returned with
    /// `component` metadata. Starting an App twice, or after
    /// [`stop`](Self::stop), is `FAILED_PRECONDITION`; so is starting one
    /// whose exposed components' gRPC routes were never taken (reason
    /// `GRPC_NOT_MOUNTED`), before any component starts.
    pub async fn start(&self) -> Result<(), AppError> {
        self.check_mounted()?;
        match self.transition(|phase| (phase == Phase::Built).then_some(Phase::Starting)) {
            Phase::Built => {}
            Phase::Stopping | Phase::Stopped => {
                return Err(AppError::failed_precondition(
                    "the app was stopped and cannot start again",
                ));
            }
            Phase::Starting | Phase::Running => {
                return Err(AppError::failed_precondition("the app was already started"));
            }
        }
        let components = &self.inner.components;
        for (index, component) in components.iter().enumerate() {
            let name = component.descriptor.name();
            if let Some(lifecycle) = &component.lifecycle
                && let Err(error) = lifecycle.start().await
            {
                self.roll_back(index).await;
                return Err(error.with_metadata("component", name));
            }
            if !component.server.open() {
                if let Some(lifecycle) = &component.lifecycle
                    && let Err(error) = lifecycle.stop().await
                {
                    tracing::warn!(component = name, error = %error, "stop hook failed");
                }
                return Err(stopped_while_starting().with_metadata("component", name));
            }
        }
        let previous =
            self.transition(|phase| (phase == Phase::Starting).then_some(Phase::Running));
        if previous == Phase::Starting {
            Ok(())
        } else {
            Err(stopped_while_starting())
        }
    }

    /// Stop every component in reverse install order: reject new calls, wait
    /// for in-flight calls until `grace` has passed (in total, not per
    /// component), then run its `on_stop` hook.
    ///
    /// Every component is stopped even when a hook fails; the first hook
    /// error is returned with `component` metadata. Idempotent: a second call
    /// waits for the first to finish and returns `Ok`. On an App that never
    /// started, only marks the components stopped.
    pub async fn stop(&self, grace: Duration) -> Result<(), AppError> {
        let previous = self.transition(|phase| match phase {
            Phase::Built => Some(Phase::Stopped),
            Phase::Starting | Phase::Running => Some(Phase::Stopping),
            Phase::Stopping | Phase::Stopped => None,
        });
        match previous {
            Phase::Built => {
                for component in &self.inner.components {
                    component.server.set_state(ComponentState::Stopped);
                }
                return Ok(());
            }
            Phase::Stopping | Phase::Stopped => {
                let mut phases = self.inner.phase.subscribe();
                // The sender lives as long as `self`, so the wait cannot fail.
                drop(phases.wait_for(|phase| *phase == Phase::Stopped).await);
                return Ok(());
            }
            Phase::Starting | Phase::Running => {}
        }
        let deadline = tokio::time::Instant::now().checked_add(grace);
        let mut first_error = None;
        for component in self.inner.components.iter().rev() {
            if let Err(error) = stop_component(component, deadline).await
                && first_error.is_none()
            {
                first_error = Some(error.with_metadata("component", component.descriptor.name()));
            }
        }
        self.inner.phase.send_replace(Phase::Stopped);
        first_error.map_or(Ok(()), Err)
    }

    /// After component `failed` did not start: stop the ones before it at
    /// once, mark the rest stopped, and stop the App.
    async fn roll_back(&self, failed: usize) {
        let now = Some(tokio::time::Instant::now());
        let components = &self.inner.components;
        for component in components[..failed].iter().rev() {
            if let Err(error) = stop_component(component, now).await {
                tracing::warn!(
                    component = component.descriptor.name(),
                    error = %error,
                    "stop hook failed while rolling back a failed start"
                );
            }
        }
        for component in &components[failed..] {
            component.server.set_state(ComponentState::Stopped);
        }
        self.inner.phase.send_replace(Phase::Stopped);
    }

    /// Apply `next` to the phase atomically; returns the phase before.
    fn transition(&self, next: impl FnOnce(Phase) -> Option<Phase>) -> Phase {
        let mut previous = Phase::Built;
        self.inner.phase.send_if_modified(|phase| {
            previous = *phase;
            match next(*phase) {
                Some(new) => {
                    *phase = new;
                    true
                }
                None => false,
            }
        });
        previous
    }

    /// `GRPC_NOT_MOUNTED` for the first exposed component when
    /// [`grpc_routes`](Self::grpc_routes) was never called.
    fn check_mounted(&self) -> Result<(), AppError> {
        if self.inner.mounted.load(Ordering::SeqCst) {
            return Ok(());
        }
        match self.inner.exposed.first() {
            None => Ok(()),
            Some(exposed) => Err(AppError::failed_precondition(format!(
                "component {} is exposed over gRPC ({}) but App::grpc_routes was never mounted",
                exposed.component, exposed.key
            ))
            .with_reason(reasons::GRPC_NOT_MOUNTED)
            .with_metadata("component", exposed.component)),
        }
    }

    fn find(&self, name: &str) -> Option<&Component> {
        self.inner
            .components
            .iter()
            .find(|component| component.descriptor.name() == name)
    }
}

fn stopped_while_starting() -> AppError {
    AppError::failed_precondition("the app was stopped while it was starting")
}

/// Drain one component until `deadline` (`None`: no limit), run its stop
/// hook if it had started, and mark it stopped.
async fn stop_component(
    component: &Component,
    deadline: Option<tokio::time::Instant>,
) -> Result<(), AppError> {
    let server = &component.server;
    let started = match server.state() {
        ComponentState::Stopped => return Ok(()),
        ComponentState::NotStarted => false,
        ComponentState::Serving | ComponentState::Draining => true,
    };
    server.set_state(ComponentState::Draining);
    if !server.wait_idle(deadline).await {
        tracing::warn!(
            component = component.descriptor.name(),
            in_flight = server.in_flight(),
            "grace period passed with calls still in flight"
        );
    }
    let outcome = match &component.lifecycle {
        Some(lifecycle) if started => lifecycle.stop().await,
        _ => Ok(()),
    };
    server.set_state(ComponentState::Stopped);
    outcome
}

impl fmt::Debug for App {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let components: Vec<(&str, Binding, ComponentState)> = self
            .inner
            .components
            .iter()
            .map(|component| {
                (
                    component.descriptor.name(),
                    component.binding,
                    component.server.state(),
                )
            })
            .collect();
        f.debug_struct("App")
            .field("components", &components)
            .finish()
    }
}
