use std::convert::Infallible;
use std::fmt;
use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, PoisonError};
use std::task::{Context, Poll};
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::{DefaultBodyLimit, FromRequestParts, MatchedPath};
use axum::middleware::{Next, from_fn};
use axum::response::{IntoResponse, Response};
use axum::routing::Route;
use futures::future::BoxFuture;
use http::header::CONTENT_TYPE;
use http::request::Parts;
use http::{HeaderMap, Request, StatusCode};
use hyper_util::rt::{TokioExecutor, TokioIo, TokioTimer};
use hyper_util::server::conn::auto;
use hyper_util::server::graceful::{GracefulShutdown, Watcher};
use hyper_util::service::TowerToHyperService;
use sekvent_context::{CallContext, ServiceIdentity, headers};
use sekvent_error::AppError;
use sekvent_telemetry::access_log::{AccessLogLayer, RouteTemplate};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use tonic::server::NamedService;
use tonic::service::Routes;
use tower::ServiceExt;
use tower::util::MapRequestLayer;

use crate::{Cors, HealthRegistry, HealthVisibility, UnitContext};

/// Establishes who is calling from the request head, typically by checking a
/// service token. `None` means an anonymous (untrusted) caller.
pub type Authenticator = Arc<dyn Fn(&Parts) -> Option<ServiceIdentity> + Send + Sync>;

/// An application layer, applied to the routes inside the call context.
type ServerLayer = Arc<dyn Fn(Router) -> Router + Send + Sync>;

/// The health endpoints, which always live at the root.
const HEALTH_PATHS: [&str; 3] = ["/livez", "/readyz", "/healthz"];

/// Path of the gRPC health service, without the trailing slash.
const GRPC_HEALTH_SERVICE: &str = "/grpc.health.v1.Health";

/// Route of the gRPC health service's methods.
const GRPC_HEALTH_ROUTE: &str = "/grpc.health.v1.Health/{*method}";

/// Default largest REST body, axum's own default.
const DEFAULT_REST_BODY_LIMIT: usize = 2 * 1024 * 1024;

/// Pause after the first accept failure that is not about one connection.
const ACCEPT_BACKOFF_FIRST: Duration = Duration::from_millis(100);
/// Longest pause between accept attempts while they keep failing.
const ACCEPT_BACKOFF_MAX: Duration = Duration::from_secs(1);

/// Axum extractor for the [`CallContext`] the server attached to the request.
///
/// In a tonic handler read it with
/// `request.extensions().get::<CallContext>()` instead.
#[derive(Debug, Clone)]
pub struct Ctx(
    /// The request's call context.
    pub CallContext,
);

impl<S: Send + Sync> FromRequestParts<S> for Ctx {
    type Rejection = AppError;

    fn from_request_parts(
        parts: &mut Parts,
        _state: &S,
    ) -> impl Future<Output = Result<Self, Self::Rejection>> + Send {
        std::future::ready(
            parts
                .extensions
                .get::<CallContext>()
                .cloned()
                .map(Ctx)
                .ok_or_else(|| AppError::internal("the request has no call context")),
        )
    }
}

/// Configures a [`Server`].
#[allow(
    clippy::struct_excessive_bools,
    reason = "independent feature switches"
)]
pub struct ServerBuilder {
    prefix: Option<String>,
    grpc: Option<Routes>,
    rest: Router,
    grpc_web: bool,
    grpc_at_root: bool,
    health_routes: bool,
    grpc_health: bool,
    visibility: HealthVisibility,
    authenticator: Option<Authenticator>,
    header_read_timeout: Option<Duration>,
    max_connections: Option<usize>,
    cors: Option<Cors>,
    rest_body_limit: usize,
    layers: Vec<ServerLayer>,
    access_log: bool,
    problems: Vec<String>,
}

impl fmt::Debug for ServerBuilder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ServerBuilder")
            .field("prefix", &self.prefix)
            .field("grpc_web", &self.grpc_web)
            .field("grpc_at_root", &self.grpc_at_root)
            .field("health_routes", &self.health_routes)
            .field("grpc_health", &self.grpc_health)
            .field("visibility", &self.visibility)
            .field("header_read_timeout", &self.header_read_timeout)
            .field("max_connections", &self.max_connections)
            .field("cors", &self.cors)
            .field("rest_body_limit", &self.rest_body_limit)
            .field("layers", &self.layers.len())
            .field("access_log", &self.access_log)
            .finish_non_exhaustive()
    }
}

impl ServerBuilder {
    /// Serve gRPC-Web and REST under `prefix` (e.g. `/api`) instead of at the
    /// root. The prefix is stripped before routing, so tonic still sees
    /// `/pkg.Service/Method`. It must start with `/`, must not end with one
    /// and may only contain unreserved URL characters.
    #[must_use]
    pub fn prefix(mut self, prefix: impl Into<String>) -> Self {
        self.prefix = Some(prefix.into());
        self
    }

    /// Serve these gRPC routes. Can be given once; add further services with
    /// [`add_service`](Self::add_service).
    #[must_use]
    pub fn grpc_routes(mut self, routes: Routes) -> Self {
        if self.grpc.is_some() {
            self.problems
                .push("gRPC routes were given more than once; use add_service to add more".into());
        } else {
            self.grpc = Some(routes);
        }
        self
    }

    /// Serve one more gRPC service.
    ///
    /// Message-size limits stay with each generated service; tonic's
    /// default is 4 MiB for decoding. Raise them on the service itself, e.g.
    /// `OrdersServer::new(svc).max_decoding_message_size(16 << 20)`.
    #[must_use]
    pub fn add_service<S>(mut self, service: S) -> Self
    where
        S: tower::Service<Request<tonic::body::Body>, Error = Infallible>
            + NamedService
            + Clone
            + Send
            + Sync
            + 'static,
        S::Response: IntoResponse,
        S::Future: Send + 'static,
    {
        self.grpc = Some(self.grpc.take().unwrap_or_default().add_service(service));
        self
    }

    /// Merge REST routes (plain axum). Panics on overlapping routes, like
    /// [`Router::merge`].
    ///
    /// Extractors refuse bodies above [`rest_body_limit`](Self::rest_body_limit)
    /// with `413`. An upload route raises the limit for itself alone with
    /// `.layer(DefaultBodyLimit::max(16 * 1024 * 1024))` on its method
    /// router. A handler reading the raw `Body` must bound it itself, e.g.
    /// with `http_body_util::Limited`.
    #[must_use]
    pub fn rest(mut self, router: Router) -> Self {
        self.rest = self.rest.merge(router);
        self
    }

    /// Whether to translate gRPC-Web requests (default on; has no effect
    /// without the `grpc-web` feature).
    #[must_use]
    pub fn grpc_web(mut self, enabled: bool) -> Self {
        self.grpc_web = enabled;
        self
    }

    /// With a prefix set, whether native gRPC is also served at the root
    /// (default on), because native clients rarely support a path prefix.
    #[must_use]
    pub fn grpc_at_root(mut self, enabled: bool) -> Self {
        self.grpc_at_root = enabled;
        self
    }

    /// Whether to serve `/livez`, `/readyz` and `/healthz` at the root
    /// (default on).
    #[must_use]
    pub fn health_routes(mut self, enabled: bool) -> Self {
        self.health_routes = enabled;
        self
    }

    /// How much the HTTP health endpoints reveal (default
    /// [`HealthVisibility::Minimal`]).
    #[must_use]
    pub fn health_visibility(mut self, visibility: HealthVisibility) -> Self {
        self.visibility = visibility;
        self
    }

    /// Whether to serve `grpc.health.v1.Health` from the runtime's registry
    /// (default on): at the root when native gRPC is served there, and
    /// under the prefix. Like the HTTP health endpoints it bypasses the
    /// authenticator and the application layers. Turn it off when adding
    /// your own health service.
    #[must_use]
    pub fn grpc_health(mut self, enabled: bool) -> Self {
        self.grpc_health = enabled;
        self
    }

    /// Identify callers; the result feeds [`headers::from_headers`], so only
    /// a trusted caller may assert `subject` and `tenant`.
    #[must_use]
    pub fn authenticator(
        mut self,
        authenticator: impl Fn(&Parts) -> Option<ServiceIdentity> + Send + Sync + 'static,
    ) -> Self {
        self.authenticator = Some(Arc::new(authenticator));
        self
    }

    /// Longest a new connection may stay silent, and an HTTP/1 client may
    /// take to send a request's headers, before the connection is closed
    /// (default 30 s). `None` waits forever, which lets idle clients hold
    /// connections open.
    #[must_use]
    pub fn header_read_timeout(mut self, timeout: Option<Duration>) -> Self {
        self.header_read_timeout = timeout;
        self
    }

    /// Most connections served at once (default 10 000). Beyond it, new
    /// connections wait in the operating system's accept queue until one
    /// closes. `None` removes the limit.
    #[must_use]
    pub fn max_connections(mut self, limit: Option<usize>) -> Self {
        self.max_connections = limit;
        self
    }

    /// Answer browsers' cross-origin requests (REST and gRPC-Web). Off by
    /// default. Preflights are answered before authentication and handlers,
    /// other `OPTIONS` requests reach the application, and these rules
    /// replace any `access-control-*` headers set by layers or handlers;
    /// [`bind`](Self::bind) runs [`Cors::validate`].
    #[must_use]
    pub fn cors(mut self, cors: Cors) -> Self {
        self.cors = Some(cors);
        self
    }

    /// Largest body REST extractors accept unless a route sets its own
    /// `axum::extract::DefaultBodyLimit` (default 2 MiB, axum's own
    /// default). Must be positive. gRPC limits are per service (see
    /// [`add_service`](Self::add_service)).
    #[must_use]
    pub fn rest_body_limit(mut self, bytes: usize) -> Self {
        self.rest_body_limit = bytes;
        self
    }

    /// Wrap every REST, gRPC and gRPC-Web request (inside the call context,
    /// outside routing; never the HTTP or gRPC health endpoints). Same
    /// bounds as [`Router::layer`]; the layer must be `Clone`. A later call
    /// wraps the earlier ones.
    ///
    /// Component calls served on this listener pass through the layer too,
    /// so an end-user authentication layer must let link-authenticated
    /// component paths through, or be applied per service instead.
    #[must_use]
    pub fn layer<L>(mut self, layer: L) -> Self
    where
        L: tower::Layer<Route> + Clone + Send + Sync + 'static,
        L::Service: tower::Service<Request<Body>> + Clone + Send + Sync + 'static,
        <L::Service as tower::Service<Request<Body>>>::Response: IntoResponse + 'static,
        <L::Service as tower::Service<Request<Body>>>::Error: Into<Infallible> + 'static,
        <L::Service as tower::Service<Request<Body>>>::Future: Send + 'static,
    {
        self.layers
            .push(Arc::new(move |router: Router| router.layer(layer.clone())));
        self
    }

    /// Whether to log one event per request (target `sekvent::access`,
    /// default on). Request ids and the `request` span stay on either way.
    #[must_use]
    pub fn access_log(mut self, enabled: bool) -> Self {
        self.access_log = enabled;
        self
    }

    /// Validate and bind `addr` (port 0 picks a free port).
    pub async fn bind(self, addr: SocketAddr) -> Result<Server, AppError> {
        self.validate()?;
        let listener = TcpListener::bind(addr)
            .await
            .map_err(|error| bind_error(addr, error))?;
        self.from_listener(listener)
    }

    /// Validate and serve on an already bound listener.
    pub fn from_listener(self, listener: TcpListener) -> Result<Server, AppError> {
        self.validate()?;
        let local_addr = listener.local_addr().map_err(|error| {
            AppError::unavailable("the listener has no local address").with_source(error)
        })?;
        Ok(Server {
            inner: Arc::new(ServerInner {
                listener: Mutex::new(Some(listener)),
                local_addr,
                config: ServerConfig {
                    prefix: self.prefix,
                    grpc: self.grpc,
                    rest: self.rest,
                    grpc_web: self.grpc_web,
                    grpc_at_root: self.grpc_at_root,
                    health_routes: self.health_routes,
                    grpc_health: self.grpc_health,
                    visibility: self.visibility,
                    authenticator: self.authenticator,
                    header_read_timeout: self.header_read_timeout,
                    max_connections: self.max_connections,
                    cors: self.cors,
                    rest_body_limit: self.rest_body_limit,
                    layers: self.layers,
                    access_log: self.access_log,
                },
            }),
        })
    }

    fn validate(&self) -> Result<(), AppError> {
        if let Some(problem) = self.problems.first() {
            return Err(AppError::invalid_argument(problem.clone()));
        }
        if self
            .header_read_timeout
            .is_some_and(|timeout| timeout.is_zero())
        {
            return Err(AppError::invalid_argument(
                "the header read timeout must be positive",
            ));
        }
        if self
            .max_connections
            .is_some_and(|limit| limit == 0 || limit > Semaphore::MAX_PERMITS)
        {
            return Err(AppError::invalid_argument(format!(
                "the connection limit must be between 1 and {}",
                Semaphore::MAX_PERMITS
            )));
        }
        if let Some(prefix) = &self.prefix {
            if !is_valid_prefix(prefix) {
                return Err(AppError::invalid_argument(format!(
                    "the path prefix {prefix:?} must look like /api: a leading slash, \
                     no trailing slash, unreserved characters only"
                )));
            }
            if self.health_routes && HEALTH_PATHS.contains(&prefix.as_str()) {
                return Err(AppError::invalid_argument(format!(
                    "the path prefix {prefix} collides with a health route"
                )));
            }
            if self.grpc_health
                && (prefix == GRPC_HEALTH_SERVICE
                    || prefix.starts_with(&format!("{GRPC_HEALTH_SERVICE}/")))
            {
                return Err(AppError::invalid_argument(format!(
                    "the path prefix {prefix} collides with the gRPC health service"
                )));
            }
        }
        if self.rest_body_limit == 0 {
            return Err(AppError::invalid_argument(
                "the REST body limit must be positive",
            ));
        }
        if let Some(cors) = &self.cors {
            cors.validate()?;
        }
        Ok(())
    }
}

fn is_valid_prefix(prefix: &str) -> bool {
    prefix.len() > 1
        && prefix.starts_with('/')
        && prefix[1..].split('/').all(|segment| {
            !segment.is_empty()
                && segment
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~'))
        })
}

fn bind_error(addr: SocketAddr, error: io::Error) -> AppError {
    AppError::unavailable(format!("could not listen on {addr}")).with_source(error)
}

#[allow(
    clippy::struct_excessive_bools,
    reason = "independent feature switches"
)]
struct ServerConfig {
    prefix: Option<String>,
    grpc: Option<Routes>,
    rest: Router,
    grpc_web: bool,
    grpc_at_root: bool,
    health_routes: bool,
    grpc_health: bool,
    visibility: HealthVisibility,
    authenticator: Option<Authenticator>,
    header_read_timeout: Option<Duration>,
    max_connections: Option<usize>,
    cors: Option<Cors>,
    rest_body_limit: usize,
    layers: Vec<ServerLayer>,
    access_log: bool,
}

impl ServerConfig {
    /// The REST routes with the body limit and the route recorder.
    fn rest_router(&self) -> Router {
        let rest = if self.rest.has_routes() {
            self.rest.clone().route_layer(from_fn(record_route))
        } else {
            self.rest.clone()
        };
        rest.layer(DefaultBodyLimit::max(self.rest_body_limit))
    }

    /// Whether native gRPC is served at the root.
    fn serves_grpc_at_root(&self) -> bool {
        self.prefix.is_none() || self.grpc_at_root
    }

    /// Request ids and the access log, with the health traffic (every path
    /// a health endpoint is mounted at) at `debug`.
    fn access_log_layer(&self) -> AccessLogLayer {
        let mut quiet: Vec<String> = Vec::new();
        if self.health_routes {
            quiet.extend(HEALTH_PATHS.map(str::to_owned));
        }
        if self.grpc_health {
            if self.serves_grpc_at_root() {
                quiet.push(format!("{GRPC_HEALTH_SERVICE}/"));
            }
            if let Some(prefix) = &self.prefix {
                quiet.push(format!("{prefix}{GRPC_HEALTH_SERVICE}/"));
            }
        }
        AccessLogLayer::new().quiet(quiet).events(self.access_log)
    }

    /// `router` behind the gRPC-Web translation, when enabled.
    fn with_grpc_web(&self, router: Router) -> Router {
        #[cfg(feature = "grpc-web")]
        let router = if self.grpc_web {
            router.layer(tonic_web::GrpcWebLayer::new())
        } else {
            router
        };
        #[cfg(not(feature = "grpc-web"))]
        let _ = self.grpc_web;
        router
    }

    /// The gRPC health service at the root (when native gRPC is served
    /// there) and under the prefix, gRPC by content type only.
    fn grpc_health_routes(&self, health: &HealthRegistry) -> Router {
        if !self.grpc_health {
            return Router::new();
        }
        let service = Dispatch {
            grpc: self.with_grpc_web(Routes::new(health.grpc_service()).into_axum_router()),
            rest: None,
        };
        let routes = Router::new().route_service(GRPC_HEALTH_ROUTE, service);
        match &self.prefix {
            Some(prefix) if self.serves_grpc_at_root() => routes.clone().nest(prefix, routes),
            Some(prefix) => Router::new().nest(prefix, routes),
            None => routes,
        }
    }
}

/// Copy the matched route template (prefix included when nested) into the
/// response, where the access log reads it.
async fn record_route(request: Request<Body>, next: Next) -> Response {
    let template = request
        .extensions()
        .get::<MatchedPath>()
        .map(|path| RouteTemplate(path.as_str().to_owned()));
    let mut response = next.run(request).await;
    if let Some(template) = template
        && response.extensions().get::<RouteTemplate>().is_none()
    {
        response.extensions_mut().insert(template);
    }
    response
}

struct ServerInner {
    /// Taken by a running serve loop and dropped when it stops accepting, so
    /// the port is released as soon as draining begins.
    listener: Mutex<Option<TcpListener>>,
    local_addr: SocketAddr,
    config: ServerConfig,
}

/// One listener serving native gRPC, gRPC-Web and REST, plus the health
/// endpoints.
///
/// Every request carries an `x-request-id` (echoed on the response) and is
/// logged once at completion (target `sekvent::access`, health traffic at
/// `debug`). Every request except the HTTP and gRPC health endpoints gets a
/// [`CallContext`] built from its headers (see [`Ctx`]) and passes the
/// application layers; see [`router`](Self::router) for the full stack.
/// Run it as an ingress unit with [`into_unit`](Self::into_unit).
#[derive(Clone)]
pub struct Server {
    inner: Arc<ServerInner>,
}

impl fmt::Debug for Server {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Server")
            .field("local_addr", &self.inner.local_addr)
            .field("prefix", &self.inner.config.prefix)
            .finish_non_exhaustive()
    }
}

impl Server {
    /// Start configuring a server.
    pub fn builder() -> ServerBuilder {
        ServerBuilder {
            prefix: None,
            grpc: None,
            rest: Router::new(),
            grpc_web: true,
            grpc_at_root: true,
            health_routes: true,
            grpc_health: true,
            visibility: HealthVisibility::default(),
            authenticator: None,
            header_read_timeout: Some(Duration::from_secs(30)),
            max_connections: Some(10_000),
            cors: None,
            rest_body_limit: DEFAULT_REST_BODY_LIMIT,
            layers: Vec::new(),
            access_log: true,
            problems: Vec::new(),
        }
    }

    /// The bound address (the real port when bound to port 0).
    pub fn local_addr(&self) -> SocketAddr {
        self.inner.local_addr
    }

    /// The complete request router, for serving elsewhere or testing
    /// without a socket. Outermost first:
    ///
    /// 1. request id (a valid `x-request-id` is kept, anything else
    ///    replaced), the `request` span and the access log;
    /// 2. CORS, when configured;
    /// 3. the health endpoints (`/livez`, `/readyz`, `/healthz` and
    ///    `grpc.health.v1.Health`, also under the prefix), or else
    /// 4. the call context (its request id is the one above), the
    ///    application layers, then the prefix strip and the split between
    ///    gRPC (by content type) and the REST routes with their body limit.
    pub fn router(&self, health: &HealthRegistry) -> Router {
        let config = &self.inner.config;
        let grpc = self.grpc_router();
        let rest = config.rest_router();
        let mut services = match &config.prefix {
            Some(prefix) => {
                let nested = Router::new().nest_service(
                    prefix,
                    Dispatch {
                        grpc: grpc.clone(),
                        rest: Some(rest),
                    },
                );
                if config.grpc_at_root {
                    nested.fallback_service(Dispatch { grpc, rest: None })
                } else {
                    nested
                }
            }
            None => Router::new().fallback_service(Dispatch {
                grpc,
                rest: Some(rest),
            }),
        };
        for layer in &config.layers {
            services = layer(services);
        }
        let authenticator = config.authenticator.clone();
        services = services.layer(MapRequestLayer::new(move |request: Request<Body>| {
            attach_context(request, authenticator.as_ref())
        }));
        let health_routes = if config.health_routes {
            health
                .http_routes(config.visibility)
                .route_layer(from_fn(record_route))
        } else {
            Router::new()
        };
        let mut app = health_routes
            .merge(config.grpc_health_routes(health))
            .fallback_service(services);
        if let Some(cors) = &config.cors {
            app = cors.apply(app);
        }
        app.layer(config.access_log_layer())
    }

    /// The unit that serves this listener: ready once listening, stops
    /// accepting when its stage drains, then lets in-flight requests finish.
    /// A restarted unit binds the same address again.
    ///
    /// Accept failures that concern one connection are skipped; running out
    /// of file descriptors or memory pauses accepting (100 ms, doubling up
    /// to 1 s) and is logged once per burst. Only a listener that can no
    /// longer accept at all fails the unit: it returns the error at once and
    /// its open connections drain in the background for at most the stage
    /// grace period.
    pub fn into_unit(
        self,
    ) -> impl FnMut(UnitContext) -> BoxFuture<'static, Result<(), AppError>> + Send + 'static {
        move |ctx: UnitContext| -> BoxFuture<'static, Result<(), AppError>> {
            Box::pin(self.clone().serve(ctx))
        }
    }

    fn grpc_router(&self) -> Router {
        let config = &self.inner.config;
        config.with_grpc_web(config.grpc.clone().unwrap_or_default().into_axum_router())
    }

    async fn serve(self, ctx: UnitContext) -> Result<(), AppError> {
        let listener = self.take_listener().await?;
        self.serve_on(listener, ctx).await
    }

    async fn serve_on<L: Incoming>(
        self,
        mut listener: L,
        ctx: UnitContext,
    ) -> Result<(), AppError> {
        let config = &self.inner.config;
        let app = self.router(ctx.health());
        ctx.health().publish().await;
        let shutdown = ctx.shutdown();
        // Releases connections still waiting for their first byte.
        let silent = shutdown.child_token();
        let mut builder = auto::Builder::new(TokioExecutor::new());
        builder
            .http1()
            .timer(TokioTimer::new())
            .header_read_timeout(config.header_read_timeout);
        let builder = Arc::new(builder);
        let limit = config
            .max_connections
            .map(|limit| Arc::new(Semaphore::new(limit)));
        let graceful = GracefulShutdown::new();
        let mut connections = JoinSet::new();
        let mut accept_errors = AcceptErrors::default();
        let addr = self.inner.local_addr;
        tracing::info!(%addr, unit = ctx.name(), "listening");
        ctx.ready();

        let outcome = loop {
            while connections.try_join_next().is_some() {}
            let permit = match &limit {
                None => None,
                Some(limit) => tokio::select! {
                    biased;
                    () = shutdown.cancelled() => break Ok(()),
                    permit = Arc::clone(limit).acquire_owned() => permit.ok(),
                },
            };
            let accepted = tokio::select! {
                biased;
                () = shutdown.cancelled() => break Ok(()),
                accepted = listener.next_connection() => accepted,
            };
            let error = match accepted {
                Ok(stream) => {
                    accept_errors.recovered(addr);
                    connections.spawn(serve_connection(
                        stream,
                        Accepted {
                            builder: Arc::clone(&builder),
                            service: TowerToHyperService::new(app.clone()),
                            watcher: graceful.watcher(),
                            first_byte: config.header_read_timeout,
                            silent: silent.clone(),
                            permit,
                        },
                    ));
                    continue;
                }
                Err(error) => error,
            };
            match AcceptFailure::of(&error) {
                AcceptFailure::Connection => {
                    tracing::debug!(%error, "accept failed for one connection");
                }
                AcceptFailure::Listener => {
                    break Err(
                        AppError::unavailable(format!("the listener on {addr} failed"))
                            .with_source(error),
                    );
                }
                AcceptFailure::Resources => {
                    let pause = accept_errors.failed(addr, &error);
                    tokio::select! {
                        biased;
                        () = shutdown.cancelled() => break Ok(()),
                        () = tokio::time::sleep(pause) => {}
                    }
                }
            }
        };

        drop(listener);
        if outcome.is_ok() {
            tracing::info!(%addr, "stopped accepting; draining connections");
            graceful.shutdown().await;
            while connections.join_next().await.is_some() {}
            return outcome;
        }
        tracing::warn!(%addr, "the listener failed; draining its connections in the background");
        silent.cancel();
        drain_in_background(graceful, connections, ctx.stage_grace(), addr);
        outcome
    }

    async fn take_listener(&self) -> Result<TcpListener, AppError> {
        let existing = self
            .inner
            .listener
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if let Some(listener) = existing {
            return Ok(listener);
        }
        let addr = self.inner.local_addr;
        TcpListener::bind(addr)
            .await
            .map_err(|error| bind_error(addr, error))
    }
}

/// Let a failed listener's connections finish in the background, cutting
/// whatever is left after `grace`: the failure is reported at once, and an
/// open gRPC health watch, for one, never ends on its own.
fn drain_in_background(
    graceful: GracefulShutdown,
    mut connections: JoinSet<()>,
    grace: Duration,
    addr: SocketAddr,
) {
    tokio::spawn(async move {
        let drained = tokio::time::timeout(grace, async {
            graceful.shutdown().await;
            while connections.join_next().await.is_some() {}
        })
        .await;
        if drained.is_err() {
            tracing::warn!(%addr, "connections outlived the grace period and were closed");
        }
    });
}

/// Where the serve loop takes its connections from; a seam so tests can
/// inject accept failures.
trait Incoming: Send + 'static {
    fn next_connection(&mut self) -> impl Future<Output = io::Result<TcpStream>> + Send;
}

impl Incoming for TcpListener {
    #[allow(clippy::manual_async_fn)]
    fn next_connection(&mut self) -> impl Future<Output = io::Result<TcpStream>> + Send {
        async move { self.accept().await.map(|(stream, _peer)| stream) }
    }
}

/// What a failed `accept` means for the listener.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AcceptFailure {
    /// One connection went away before it was accepted; carry on.
    Connection,
    /// The socket cannot accept at all (not listening, wrong type).
    Listener,
    /// The process is out of something (file descriptors, memory, buffers);
    /// pause, then try again.
    Resources,
}

impl AcceptFailure {
    fn of(error: &io::Error) -> Self {
        match error.kind() {
            io::ErrorKind::ConnectionAborted
            | io::ErrorKind::ConnectionReset
            | io::ErrorKind::ConnectionRefused
            | io::ErrorKind::Interrupted
            | io::ErrorKind::WouldBlock => Self::Connection,
            io::ErrorKind::InvalidInput | io::ErrorKind::Unsupported => Self::Listener,
            _ => Self::Resources,
        }
    }
}

/// Backoff and log throttling for a burst of resource-related accept
/// failures.
#[derive(Debug, Default)]
struct AcceptErrors {
    burst: u32,
}

impl AcceptErrors {
    /// Record a failure and return how long to pause before accepting again.
    fn failed(&mut self, addr: SocketAddr, error: &io::Error) -> Duration {
        if self.burst == 0 {
            tracing::warn!(%addr, %error, "accepting connections failed; backing off");
        } else {
            let failures = self.burst.saturating_add(1);
            tracing::debug!(%addr, %error, failures, "accepting connections still fails");
        }
        let pause = ACCEPT_BACKOFF_FIRST
            .saturating_mul(1 << self.burst.min(4))
            .min(ACCEPT_BACKOFF_MAX);
        self.burst = self.burst.saturating_add(1);
        pause
    }

    /// A connection was accepted: end the burst.
    fn recovered(&mut self, addr: SocketAddr) {
        if self.burst > 0 {
            tracing::info!(%addr, failures = self.burst, "accepting connections again");
            self.burst = 0;
        }
    }
}

/// Everything one accepted connection needs to be served.
struct Accepted {
    builder: Arc<auto::Builder<TokioExecutor>>,
    service: TowerToHyperService<Router>,
    watcher: Watcher,
    first_byte: Option<Duration>,
    silent: CancellationToken,
    permit: Option<OwnedSemaphorePermit>,
}

async fn serve_connection(stream: TcpStream, accepted: Accepted) {
    let Accepted {
        builder,
        service,
        watcher,
        first_byte,
        silent,
        // Held until the connection is done, so it counts against the limit.
        permit: _permit,
    } = accepted;
    if let Some(timeout) = first_byte {
        // The protocol sniffing below has no timeout of its own.
        let mut first = [0_u8; 1];
        let spoke = tokio::select! {
            biased;
            () = silent.cancelled() => false,
            peeked = tokio::time::timeout(timeout, stream.peek(&mut first)) => {
                matches!(peeked, Ok(Ok(read)) if read > 0)
            }
        };
        if !spoke {
            tracing::debug!("connection closed before it sent anything");
            return;
        }
    }
    let connection = builder
        .serve_connection_with_upgrades(TokioIo::new(stream), service)
        .into_owned();
    if let Err(error) = watcher.watch(connection).await {
        tracing::debug!(%error, "connection closed with an error");
    }
}

fn attach_context(request: Request<Body>, authenticator: Option<&Authenticator>) -> Request<Body> {
    let (mut parts, body) = request.into_parts();
    let caller = authenticator.and_then(|authenticate| authenticate(&parts));
    let ctx = headers::from_headers(&parts.headers, caller);
    parts.extensions.insert(ctx);
    Request::from_parts(parts, body)
}

fn is_grpc(headers: &HeaderMap) -> bool {
    headers
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.starts_with("application/grpc"))
}

/// Sends gRPC (by content type) to the tonic routes and everything else to
/// the REST routes, or to a 404 when there are none.
#[derive(Clone)]
struct Dispatch {
    grpc: Router,
    rest: Option<Router>,
}

impl tower::Service<Request<Body>> for Dispatch {
    type Response = Response;
    type Error = Infallible;
    type Future = BoxFuture<'static, Result<Response, Infallible>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, request: Request<Body>) -> Self::Future {
        let target = if is_grpc(request.headers()) {
            Some(self.grpc.clone())
        } else {
            self.rest.clone()
        };
        Box::pin(async move {
            match target {
                Some(router) => router.oneshot(request).await,
                None => Ok(StatusCode::NOT_FOUND.into_response()),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicBool, Ordering};

    use axum::routing::get;
    use http_body_util::BodyExt;
    use sekvent_error::ErrorCode;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::sync::{oneshot, watch};
    use tokio::time::Instant;

    use super::*;
    use crate::Stage;

    const GRACE: Duration = Duration::from_secs(5);

    fn localhost() -> SocketAddr {
        SocketAddr::from(([127, 0, 0, 1], 0))
    }

    async fn send(router: &Router, request: Request<Body>) -> (StatusCode, HeaderMap, String) {
        let response = router.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let mut headers = response.headers().clone();
        let collected = response.into_body().collect().await.unwrap();
        if let Some(trailers) = collected.trailers() {
            headers.extend(trailers.clone());
        }
        let body = collected.to_bytes();
        (status, headers, String::from_utf8_lossy(&body).into_owned())
    }

    fn get_request(path: &str) -> Request<Body> {
        Request::get(path).body(Body::empty()).unwrap()
    }

    fn grpc_check() -> Request<Body> {
        Request::post("/grpc.health.v1.Health/Check")
            .version(http::Version::HTTP_2)
            .header(CONTENT_TYPE, "application/grpc")
            .body(Body::from(vec![0_u8, 0, 0, 0, 0]))
            .unwrap()
    }

    fn echo() -> Router {
        Router::new().route(
            "/echo",
            get(|Ctx(ctx): Ctx| async move {
                format!(
                    "{}|{}|{}",
                    ctx.request_id(),
                    ctx.caller().map_or("-", |caller| caller.name.as_str()),
                    ctx.subject().unwrap_or("-")
                )
            }),
        )
    }

    #[tokio::test]
    async fn without_a_prefix_rest_and_grpc_share_the_root() {
        let server = Server::builder()
            .rest(echo())
            .authenticator(|parts: &Parts| {
                parts
                    .headers
                    .contains_key("x-link")
                    .then(|| ServiceIdentity::trusted("billing"))
            })
            .bind(localhost())
            .await
            .unwrap();
        let health = HealthRegistry::new();
        let router = server.router(&health);

        let request = Request::get("/echo")
            .header("x-request-id", "r-1")
            .header("x-link", "1")
            .header("x-sekvent-subject", "user-1")
            .body(Body::empty())
            .unwrap();
        let (status, _, body) = send(&router, request).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, "r-1|billing|user-1");

        let anonymous = Request::get("/echo")
            .header("x-sekvent-subject", "user-1")
            .body(Body::empty())
            .unwrap();
        let (_, _, body) = send(&router, anonymous).await;
        assert!(body.ends_with("|-|-"), "{body}");

        let (_, headers, _) = send(&router, grpc_check()).await;
        assert_eq!(
            headers.get("grpc-status").map(|v| v.to_str().unwrap()),
            Some("0")
        );

        let (status, _, _) = send(&router, get_request("/readyz")).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "not started yet");
        let (status, _, _) = send(&router, get_request("/missing")).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn optional_parts_can_be_switched_off() {
        let server = Server::builder()
            .prefix("/api/v1")
            .grpc_at_root(false)
            .grpc_health(false)
            .grpc_web(false)
            .health_routes(false)
            .health_visibility(HealthVisibility::Full)
            .rest(echo())
            .bind(localhost())
            .await
            .unwrap();
        let router = server.router(&HealthRegistry::new());

        let (status, _, _) = send(&router, get_request("/readyz")).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let (status, _, _) = send(&router, get_request("/api/v1/echo")).await;
        assert_eq!(status, StatusCode::OK);
        let (status, _, _) = send(&router, grpc_check()).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "no native gRPC at the root");

        let mut prefixed = grpc_check();
        *prefixed.uri_mut() = "/api/v1/grpc.health.v1.Health/Check".parse().unwrap();
        let (_, headers, _) = send(&router, prefixed).await;
        assert_eq!(
            headers.get("grpc-status").map(|v| v.to_str().unwrap()),
            Some("12"),
            "the health service is off, so the method is unimplemented"
        );
    }

    #[tokio::test]
    async fn services_can_be_added_one_by_one() {
        let health = HealthRegistry::new();
        let server = Server::builder()
            .grpc_health(false)
            .add_service(health.grpc_service())
            .bind(localhost())
            .await
            .unwrap();
        let (_, headers, _) = send(&server.router(&health), grpc_check()).await;
        assert_eq!(
            headers.get("grpc-status").map(|v| v.to_str().unwrap()),
            Some("0")
        );
        assert!(format!("{server:?}").contains("local_addr"));
    }

    #[tokio::test]
    async fn invalid_configurations_are_rejected() {
        for prefix in [
            "api",
            "/",
            "/api/",
            "/a//b",
            "/a b",
            "/{x}",
            "/*rest",
            "/livez",
            "/grpc.health.v1.Health",
            "/grpc.health.v1.Health/v2",
        ] {
            let result = Server::builder().prefix(prefix).bind(localhost()).await;
            assert!(result.is_err(), "{prefix:?} must be rejected");
        }
        assert!(
            Server::builder()
                .health_routes(false)
                .prefix("/livez")
                .bind(localhost())
                .await
                .is_ok()
        );
        assert!(
            Server::builder()
                .grpc_health(false)
                .prefix("/grpc.health.v1.Health")
                .bind(localhost())
                .await
                .is_ok()
        );
        assert!(
            Server::builder()
                .prefix("/grpc.health.v1.HealthX")
                .bind(localhost())
                .await
                .is_ok()
        );

        let twice = Server::builder()
            .grpc_routes(Routes::default())
            .grpc_routes(Routes::default())
            .bind(localhost())
            .await;
        assert!(twice.is_err());

        let taken = TcpListener::bind(localhost()).await.unwrap();
        let addr = taken.local_addr().unwrap();
        assert!(
            Server::builder().bind(addr).await.is_err(),
            "address in use"
        );
        let reused = Server::builder().from_listener(taken).unwrap();
        assert_eq!(reused.local_addr(), addr);
        assert!(format!("{:?}", Server::builder()).contains("max_connections: Some(10000)"));

        for limits in [
            Server::builder().header_read_timeout(Some(Duration::ZERO)),
            Server::builder().max_connections(Some(0)),
            Server::builder().max_connections(Some(usize::MAX)),
        ] {
            let error = limits.bind(localhost()).await.unwrap_err();
            assert_eq!(error.code(), ErrorCode::InvalidArgument);
        }
    }

    #[tokio::test]
    async fn the_extractor_fails_closed_without_a_context() {
        let router = echo();
        let (status, _, _) = send(&router, get_request("/echo")).await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[test]
    fn accept_failures_are_told_apart() {
        let of = |kind: io::ErrorKind| AcceptFailure::of(&io::Error::from(kind));
        assert_eq!(
            of(io::ErrorKind::ConnectionAborted),
            AcceptFailure::Connection
        );
        assert_eq!(of(io::ErrorKind::Interrupted), AcceptFailure::Connection);
        assert_eq!(of(io::ErrorKind::InvalidInput), AcceptFailure::Listener);
        assert_eq!(of(io::ErrorKind::OutOfMemory), AcceptFailure::Resources);
        assert_eq!(
            AcceptFailure::of(&io::Error::other("too many open files")),
            AcceptFailure::Resources
        );
    }

    #[test]
    fn accept_backoff_doubles_to_a_cap_and_resets() {
        let addr = localhost();
        let error = io::Error::other("too many open files");
        let mut errors = AcceptErrors::default();
        let pauses: Vec<u128> = (0..6)
            .map(|_| errors.failed(addr, &error).as_millis())
            .collect();
        assert_eq!(pauses, [100, 200, 400, 800, 1000, 1000]);
        errors.recovered(addr);
        assert_eq!(errors.burst, 0);
        errors.recovered(addr);
        assert_eq!(errors.failed(addr, &error), ACCEPT_BACKOFF_FIRST);
    }

    fn context(token: &CancellationToken) -> (UnitContext, watch::Receiver<bool>) {
        let (ready_tx, ready) = watch::channel(false);
        let ctx = UnitContext::new(
            Arc::from("api"),
            Stage::Ingress,
            0,
            token.clone(),
            Arc::new(ready_tx),
            HealthRegistry::new(),
            crate::unit::StopWindow {
                grace: GRACE,
                by: Arc::default(),
            },
        );
        (ctx, ready)
    }

    /// One step of a scripted listener.
    enum Step {
        /// Accept a real connection.
        Accept,
        /// Fail this accept with the error.
        Fail(io::Error),
        /// Hold the next accept until the sender fires.
        Wait(oneshot::Receiver<()>),
        /// Fire the sender, then go on with the next step.
        Tell(oneshot::Sender<()>),
    }

    /// A real listener that plays a script of accept outcomes first.
    struct Scripted {
        listener: TcpListener,
        steps: VecDeque<Step>,
    }

    impl Incoming for Scripted {
        #[allow(clippy::manual_async_fn)]
        fn next_connection(&mut self) -> impl Future<Output = io::Result<TcpStream>> + Send {
            async move {
                loop {
                    match self.steps.pop_front() {
                        Some(Step::Fail(error)) => return Err(error),
                        Some(Step::Wait(go)) => {
                            let _ = go.await;
                        }
                        Some(Step::Tell(told)) => {
                            let _ = told.send(());
                        }
                        Some(Step::Accept) | None => return self.listener.next_connection().await,
                    }
                }
            }
        }
    }

    async fn scripted(server: &Server, steps: impl IntoIterator<Item = Step>) -> Scripted {
        Scripted {
            listener: server.take_listener().await.unwrap(),
            steps: steps.into_iter().collect(),
        }
    }

    /// One `connection: close` request over a fresh connection; the raw
    /// response.
    async fn exchange(addr: SocketAddr, path: &str) -> String {
        let mut stream = TcpStream::connect(addr).await.unwrap();
        let request = format!("GET {path} HTTP/1.1\r\nhost: test\r\nconnection: close\r\n\r\n");
        stream.write_all(request.as_bytes()).await.unwrap();
        let mut response = Vec::new();
        stream.read_to_end(&mut response).await.unwrap();
        String::from_utf8_lossy(&response).into_owned()
    }

    fn out_of_files() -> Step {
        Step::Fail(io::Error::other("too many open files"))
    }

    #[tokio::test(start_paused = true)]
    async fn resource_failures_pause_accepting_without_ending_it() {
        let server = Server::builder().bind(localhost()).await.unwrap();
        let addr = server.local_addr();
        let reset = Step::Fail(io::ErrorKind::ConnectionReset.into());
        let listener = scripted(
            &server,
            [out_of_files(), out_of_files(), out_of_files(), reset],
        )
        .await;
        let token = CancellationToken::new();
        let (ctx, mut ready) = context(&token);
        let started = Instant::now();
        let running = tokio::spawn(server.serve_on(listener, ctx));
        ready.wait_for(|ready| *ready).await.unwrap();

        let response = exchange(addr, "/livez").await;
        assert!(response.starts_with("HTTP/1.1 200"), "{response}");
        assert!(Instant::now() - started >= Duration::from_millis(700));

        token.cancel();
        running.await.unwrap().unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn shutdown_cuts_an_accept_pause_short() {
        let server = Server::builder().bind(localhost()).await.unwrap();
        let (told_tx, told_rx) = oneshot::channel::<()>();
        let listener = scripted(&server, [Step::Tell(told_tx), out_of_files()]).await;
        let token = CancellationToken::new();
        let (ctx, _ready) = context(&token);
        let running = tokio::spawn(server.serve_on(listener, ctx));
        // The unit reaches its pause without yielding after telling.
        told_rx.await.unwrap();
        let paused_at = Instant::now();
        token.cancel();
        running.await.unwrap().unwrap();
        assert_eq!(Instant::now(), paused_at);
    }

    #[tokio::test(start_paused = true)]
    async fn a_failed_listener_fails_the_unit_at_once_and_bounds_the_drain() {
        let (entered_tx, entered_rx) = oneshot::channel::<()>();
        let (guard_tx, guard_rx) = oneshot::channel::<()>();
        let slot = Arc::new(Mutex::new(Some((entered_tx, guard_tx))));
        let hold = Router::new().route(
            "/hold",
            get(move || {
                let taken = slot.lock().unwrap().take();
                async move {
                    if let Some((entered, guard)) = taken {
                        let _guard = guard;
                        let _ = entered.send(());
                        std::future::pending::<()>().await;
                    }
                    "released"
                }
            }),
        );
        let server = Server::builder()
            .rest(hold)
            .bind(localhost())
            .await
            .unwrap();
        let addr = server.local_addr();
        let (go_tx, go_rx) = oneshot::channel::<()>();
        let broken = Step::Fail(io::ErrorKind::InvalidInput.into());
        let listener = scripted(&server, [Step::Accept, Step::Wait(go_rx), broken]).await;
        let token = CancellationToken::new();
        let (ctx, mut ready) = context(&token);
        let running = tokio::spawn(server.serve_on(listener, ctx));
        ready.wait_for(|ready| *ready).await.unwrap();

        let mut client = TcpStream::connect(addr).await.unwrap();
        client
            .write_all(b"GET /hold HTTP/1.1\r\nhost: test\r\n\r\n")
            .await
            .unwrap();
        entered_rx.await.unwrap();

        let failed = Instant::now();
        go_tx.send(()).unwrap();
        let error = running.await.unwrap().unwrap_err();
        assert_eq!(error.code(), ErrorCode::Unavailable);
        assert!(error.message().contains("listener"), "{}", error.message());
        assert_eq!(Instant::now(), failed, "reported before any draining");
        TcpListener::bind(addr)
            .await
            .expect("the port was released at once");

        assert!(guard_rx.await.is_err(), "the held request was cut");
        assert_eq!(Instant::now() - failed, GRACE);
        let mut rest = Vec::new();
        let _ = client.read_to_end(&mut rest).await;
        assert!(!token.is_cancelled());
    }

    #[tokio::test(start_paused = true)]
    async fn silent_and_slow_clients_are_cut_off() {
        let timeout = Duration::from_secs(3);
        let server = Server::builder()
            .header_read_timeout(Some(timeout))
            .bind(localhost())
            .await
            .unwrap();
        let addr = server.local_addr();
        let token = CancellationToken::new();
        let (ctx, mut ready) = context(&token);
        let mut unit = server.into_unit();
        let running = tokio::spawn(unit(ctx));
        ready.wait_for(|ready| *ready).await.unwrap();

        for opening in [&b""[..], &b"GET /livez HTTP/1.1\r\n"[..]] {
            let started = Instant::now();
            let mut client = TcpStream::connect(addr).await.unwrap();
            client.write_all(opening).await.unwrap();
            let mut rest = Vec::new();
            let _ = client.read_to_end(&mut rest).await;
            assert!(Instant::now() - started >= timeout);
        }
        token.cancel();
        running.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn the_connection_limit_queues_new_connections() {
        let (entered_tx, entered_rx) = oneshot::channel::<()>();
        let (release_tx, release_rx) = oneshot::channel::<()>();
        let slot = Arc::new(Mutex::new(Some((entered_tx, release_rx))));
        let first_done = Arc::new(AtomicBool::new(false));
        let done = Arc::clone(&first_done);
        let routes = Router::new()
            .route(
                "/hold",
                get(move || {
                    let taken = slot.lock().unwrap().take();
                    let done = Arc::clone(&done);
                    async move {
                        if let Some((entered, release)) = taken {
                            let _ = entered.send(());
                            let _ = release.await;
                        }
                        done.store(true, Ordering::SeqCst);
                        "held"
                    }
                }),
            )
            .route(
                "/first-done",
                get(move || {
                    let seen = first_done.load(Ordering::SeqCst);
                    async move { seen.to_string() }
                }),
            );
        let server = Server::builder()
            .rest(routes)
            .max_connections(Some(1))
            .bind(localhost())
            .await
            .unwrap();
        let addr = server.local_addr();
        let token = CancellationToken::new();
        let (ctx, mut ready) = context(&token);
        let mut unit = server.into_unit();
        let running = tokio::spawn(unit(ctx));
        ready.wait_for(|ready| *ready).await.unwrap();

        let first = tokio::spawn(exchange(addr, "/hold"));
        entered_rx.await.unwrap();
        let mut second = TcpStream::connect(addr).await.unwrap();
        second
            .write_all(b"GET /first-done HTTP/1.1\r\nhost: test\r\nconnection: close\r\n\r\n")
            .await
            .unwrap();
        release_tx.send(()).unwrap();
        assert!(first.await.unwrap().ends_with("held"));
        let mut response = Vec::new();
        second.read_to_end(&mut response).await.unwrap();
        let response = String::from_utf8_lossy(&response);
        assert!(
            response.ends_with("true"),
            "served only once the first connection closed: {response}"
        );

        token.cancel();
        running.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn limits_can_be_lifted() {
        let server = Server::builder()
            .header_read_timeout(None)
            .max_connections(None)
            .bind(localhost())
            .await
            .unwrap();
        let addr = server.local_addr();
        let token = CancellationToken::new();
        let (ctx, mut ready) = context(&token);
        let mut unit = server.into_unit();
        let running = tokio::spawn(unit(ctx));
        ready.wait_for(|ready| *ready).await.unwrap();
        let response = exchange(addr, "/livez").await;
        assert!(response.starts_with("HTTP/1.1 200"), "{response}");
        token.cancel();
        running.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn a_restarted_unit_binds_the_same_address_again() {
        let server = Server::builder().bind(localhost()).await.unwrap();
        let addr = server.local_addr();
        let mut unit = server.into_unit();

        for _ in 0..2 {
            let token = CancellationToken::new();
            let (ctx, mut ready) = context(&token);
            let running = tokio::spawn(unit(ctx));
            ready.wait_for(|ready| *ready).await.unwrap();
            let probe = tokio::net::TcpStream::connect(addr).await;
            assert!(probe.is_ok(), "listening on {addr}");
            drop(probe);
            token.cancel();
            running.await.unwrap().unwrap();
        }
        TcpListener::bind(addr)
            .await
            .expect("released after the last run");
    }
}
