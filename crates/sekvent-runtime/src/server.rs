use std::convert::Infallible;
use std::fmt;
use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, PoisonError};
use std::task::{Context, Poll};

use axum::Router;
use axum::body::Body;
use axum::extract::FromRequestParts;
use axum::response::{IntoResponse, Response};
use futures::future::BoxFuture;
use http::header::CONTENT_TYPE;
use http::request::Parts;
use http::{HeaderMap, Request, StatusCode};
use hyper_util::rt::{TokioExecutor, TokioIo};
use hyper_util::server::conn::auto;
use hyper_util::server::graceful::GracefulShutdown;
use hyper_util::service::TowerToHyperService;
use sekvent_context::{CallContext, ServiceIdentity, headers};
use sekvent_error::AppError;
use tokio::net::TcpListener;
use tokio::task::JoinSet;
use tonic::server::NamedService;
use tonic::service::Routes;
use tower::ServiceExt;
use tower::util::MapRequestLayer;

use crate::{HealthRegistry, HealthVisibility, UnitContext};

/// Establishes who is calling from the request head, typically by checking a
/// service token. `None` means an anonymous (untrusted) caller.
pub type Authenticator = Arc<dyn Fn(&Parts) -> Option<ServiceIdentity> + Send + Sync>;

/// The health endpoints, which always live at the root.
const HEALTH_PATHS: [&str; 3] = ["/livez", "/readyz", "/healthz"];

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
    /// (default on). Turn it off when adding your own health service.
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
                },
            }),
        })
    }

    fn validate(&self) -> Result<(), AppError> {
        if let Some(problem) = self.problems.first() {
            return Err(AppError::invalid_argument(problem.clone()));
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
/// Every request gets a [`CallContext`] built from its headers (see
/// [`Ctx`]). Run it as an ingress unit with [`into_unit`](Self::into_unit).
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
            problems: Vec::new(),
        }
    }

    /// The bound address (the real port when bound to port 0).
    pub fn local_addr(&self) -> SocketAddr {
        self.inner.local_addr
    }

    /// The complete request router, health endpoints and context layer
    /// included, for serving elsewhere or testing without a socket.
    pub fn router(&self, health: &HealthRegistry) -> Router {
        let config = &self.inner.config;
        let grpc = self.grpc_router(health);
        let mut app = if config.health_routes {
            health.http_routes(config.visibility)
        } else {
            Router::new()
        };
        app = match &config.prefix {
            Some(prefix) => {
                let nested = app.nest_service(
                    prefix,
                    Dispatch {
                        grpc: grpc.clone(),
                        rest: Some(config.rest.clone()),
                    },
                );
                if config.grpc_at_root {
                    nested.fallback_service(Dispatch { grpc, rest: None })
                } else {
                    nested
                }
            }
            None => app.fallback_service(Dispatch {
                grpc,
                rest: Some(config.rest.clone()),
            }),
        };
        let authenticator = config.authenticator.clone();
        app.layer(MapRequestLayer::new(move |request: Request<Body>| {
            attach_context(request, authenticator.as_ref())
        }))
    }

    /// The unit that serves this listener: ready once listening, stops
    /// accepting when its stage drains, then lets in-flight requests finish.
    /// A restarted unit binds the same address again.
    pub fn into_unit(
        self,
    ) -> impl FnMut(UnitContext) -> BoxFuture<'static, Result<(), AppError>> + Send + 'static {
        move |ctx: UnitContext| -> BoxFuture<'static, Result<(), AppError>> {
            Box::pin(self.clone().serve(ctx))
        }
    }

    fn grpc_router(&self, health: &HealthRegistry) -> Router {
        let config = &self.inner.config;
        let mut services = config.grpc.clone().unwrap_or_default();
        if config.grpc_health {
            services = services.add_service(health.grpc_service());
        }
        let router = services.into_axum_router();
        #[cfg(feature = "grpc-web")]
        let router = if config.grpc_web {
            router.layer(tonic_web::GrpcWebLayer::new())
        } else {
            router
        };
        #[cfg(not(feature = "grpc-web"))]
        let _ = config.grpc_web;
        router
    }

    async fn serve(self, ctx: UnitContext) -> Result<(), AppError> {
        let listener = self.take_listener().await?;
        let app = self.router(ctx.health());
        ctx.health().publish().await;
        let shutdown = ctx.shutdown();
        let builder = auto::Builder::new(TokioExecutor::new());
        let graceful = GracefulShutdown::new();
        let mut connections = JoinSet::new();
        let addr = self.inner.local_addr;
        tracing::info!(%addr, unit = ctx.name(), "listening");
        ctx.ready();

        let outcome = loop {
            while connections.try_join_next().is_some() {}
            tokio::select! {
                biased;
                () = shutdown.cancelled() => break Ok(()),
                accepted = listener.accept() => match accepted {
                    Ok((stream, _peer)) => {
                        let service = TowerToHyperService::new(app.clone());
                        let connection = builder
                            .serve_connection_with_upgrades(TokioIo::new(stream), service)
                            .into_owned();
                        let connection = graceful.watch(connection);
                        connections.spawn(async move {
                            if let Err(error) = connection.await {
                                tracing::debug!(%error, "connection closed with an error");
                            }
                        });
                    }
                    Err(error) if is_per_connection(&error) => {
                        tracing::debug!(%error, "accept failed for one connection");
                    }
                    Err(error) => {
                        break Err(AppError::unavailable(format!("the listener on {addr} failed"))
                            .with_source(error));
                    }
                },
            }
        };

        drop(listener);
        tracing::info!(%addr, "stopped accepting; draining connections");
        graceful.shutdown().await;
        while connections.join_next().await.is_some() {}
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

/// Accept errors that concern one connection, not the listener.
fn is_per_connection(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::ConnectionAborted
            | io::ErrorKind::ConnectionReset
            | io::ErrorKind::ConnectionRefused
            | io::ErrorKind::Interrupted
    )
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
    use axum::routing::get;
    use http_body_util::BodyExt;
    use tokio::sync::watch;
    use tokio_util::sync::CancellationToken;

    use super::*;
    use crate::Stage;

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
            "api", "/", "/api/", "/a//b", "/a b", "/{x}", "/*rest", "/livez",
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
        assert!(format!("{:?}", Server::builder()).contains("ServerBuilder"));
    }

    #[tokio::test]
    async fn the_extractor_fails_closed_without_a_context() {
        let router = echo();
        let (status, _, _) = send(&router, get_request("/echo")).await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[test]
    fn only_connection_level_accept_errors_are_tolerated() {
        assert!(is_per_connection(&io::Error::from(
            io::ErrorKind::ConnectionAborted
        )));
        assert!(!is_per_connection(&io::Error::other("fd limit")));
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
        );
        (ctx, ready)
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
