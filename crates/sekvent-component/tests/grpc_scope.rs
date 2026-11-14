//! The request scope over the wire: gRPC-Web over HTTP/1.1 through the
//! runtime server under a path prefix with CORS. The App's scope resolver
//! maps the `host` header to a tenant for calls of every kind
//! (authenticated, anonymous, optional-end-user, streaming), never for link
//! callers, preflights or in-process calls; the tenant rule against end
//! users; and the scope's lifetime, which lasts until a unary handler
//! returned or a stream ended.

mod support;

use std::net::SocketAddr;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use bytes::{Buf, BufMut, Bytes, BytesMut};
use http::header::{AUTHORIZATION, CONTENT_TYPE, HOST, ORIGIN};
use http_body_util::{BodyExt as _, Full};
use hyper_util::rt::TokioIo;
use prost::Message;
use sekvent_component::{
    AppError, Binding, CallContext, CallStream, END_USER_REJECTED_MESSAGE, EndUser, RequestScope,
    ScopeResolver,
};
use sekvent_config::MapSource;
use sekvent_context::{ServiceIdentity, headers};
use sekvent_runtime::{Cors, Runtime, RuntimeHandle, Server, Stage, UnitPolicy};
use sites::{SiteReply, SiteRequest, Sites, SitesHandle, Whoami, WhoamiHandle};
use support::fakes::{LOCAL_BINDINGS, build_with};
use support::grpc::{TOKEN, guarded};
use tokio::sync::{Notify, watch};
use tonic_types::StatusExt as _;

const ORIGIN_URL: &str = "https://shop.example";
const LOOKUP: &str = "/api/shop.sites.v1.Sites/Lookup";
const PING: &str = "/api/shop.sites.v1.Sites/Ping";
const PEEK: &str = "/api/shop.sites.v1.Sites/Peek";
const WATCH: &str = "/api/shop.sites.v1.Sites/Watch";

/// Two components declared with `#[component]`: `sites`, with one method
/// of each kind, served; `whoami`, which `sites` calls in process.
#[allow(dead_code, missing_docs, unreachable_pub)]
mod sites {
    use sekvent_component::{AppError, CallContext, CallStream};

    /// The contracts sekvent-proto-build would generate for
    /// `shop.sites.v1.Sites` and `shop.sites.v1.Whoami`.
    pub mod proto {
        #[allow(non_upper_case_globals, clippy::type_complexity)]
        pub const __sekvent_service_Sites: (&str, &[(&str, &str, &str, bool, bool)]) = (
            "shop.sites.v1.Sites",
            &[
                (
                    "Lookup",
                    "shop.sites.v1.SiteRequest",
                    "shop.sites.v1.SiteReply",
                    false,
                    false,
                ),
                (
                    "Ping",
                    "shop.sites.v1.SiteRequest",
                    "shop.sites.v1.SiteReply",
                    false,
                    false,
                ),
                (
                    "Peek",
                    "shop.sites.v1.SiteRequest",
                    "shop.sites.v1.SiteReply",
                    false,
                    false,
                ),
                (
                    "Watch",
                    "shop.sites.v1.SiteRequest",
                    "shop.sites.v1.SiteReply",
                    false,
                    true,
                ),
            ],
        );
        #[allow(non_camel_case_types)]
        pub type __sekvent_rpc_Sites__Lookup = (super::SiteRequest, super::SiteReply);
        #[allow(non_camel_case_types)]
        pub type __sekvent_rpc_Sites__Ping = (super::SiteRequest, super::SiteReply);
        #[allow(non_camel_case_types)]
        pub type __sekvent_rpc_Sites__Peek = (super::SiteRequest, super::SiteReply);
        #[allow(non_camel_case_types)]
        pub type __sekvent_rpc_Sites__Watch = (super::SiteRequest, super::SiteReply);

        #[allow(non_upper_case_globals, clippy::type_complexity)]
        pub const __sekvent_service_Whoami: (&str, &[(&str, &str, &str, bool, bool)]) = (
            "shop.sites.v1.Whoami",
            &[(
                "Ask",
                "shop.sites.v1.SiteRequest",
                "shop.sites.v1.SiteReply",
                false,
                false,
            )],
        );
        #[allow(non_camel_case_types)]
        pub type __sekvent_rpc_Whoami__Ask = (super::SiteRequest, super::SiteReply);
    }

    /// `park` makes a lookup wait until the test lets it finish, `nested`
    /// makes it ask `whoami`; `endless` makes a watch endless.
    #[derive(Clone, PartialEq, prost::Message)]
    pub struct SiteRequest {
        #[prost(string, tag = "1")]
        pub code: String,
    }

    impl prost::Name for SiteRequest {
        const NAME: &'static str = "SiteRequest";
        const PACKAGE: &'static str = "shop.sites.v1";
    }

    /// What the handler saw.
    #[derive(Clone, PartialEq, prost::Message)]
    pub struct SiteReply {
        #[prost(string, tag = "1")]
        pub tenant: String,
        #[prost(string, tag = "2")]
        pub subject: String,
        #[prost(bool, tag = "3")]
        pub scoped: bool,
    }

    impl prost::Name for SiteReply {
        const NAME: &'static str = "SiteReply";
        const PACKAGE: &'static str = "shop.sites.v1";
    }

    /// The pages of a site.
    #[sekvent_component::component(
        name = "sites",
        package = "shop.sites.v1",
        proto = "crate::sites::proto"
    )]
    pub trait Sites: Send + Sync + 'static {
        /// A page for signed-in users.
        #[call]
        async fn lookup(&self, cx: &CallContext, req: SiteRequest) -> Result<SiteReply, AppError>;
        /// A page for everyone.
        #[call(anonymous)]
        async fn ping(&self, cx: &CallContext, req: SiteRequest) -> Result<SiteReply, AppError>;
        /// A page personal for signed-in users.
        #[call(optional_end_user)]
        async fn peek(&self, cx: &CallContext, req: SiteRequest) -> Result<SiteReply, AppError>;
        /// Updates of a page for signed-in users.
        #[call(stream)]
        async fn watch(
            &self,
            cx: &CallContext,
            req: SiteRequest,
        ) -> Result<CallStream<SiteReply, AppError>, AppError>;
    }

    /// What a callee further down sees.
    #[sekvent_component::component(
        name = "whoami",
        package = "shop.sites.v1",
        proto = "crate::sites::proto"
    )]
    pub trait Whoami: Send + Sync + 'static {
        /// The tenant and the scope the call arrived with.
        #[call]
        async fn ask(&self, cx: &CallContext, req: SiteRequest) -> Result<SiteReply, AppError>;
    }
}

/// The site a scope was resolved for.
#[derive(Debug, Clone, PartialEq)]
struct Site(&'static str);

/// What a handler saw of its context.
#[derive(Debug, Clone, PartialEq)]
struct Seen {
    tenant: Option<String>,
    site: Option<Site>,
    scoped: bool,
    caller: Option<ServiceIdentity>,
    end_user: Option<String>,
}

fn reply_of(cx: &CallContext) -> SiteReply {
    SiteReply {
        tenant: cx.tenant().unwrap_or_default().to_owned(),
        subject: cx.subject().unwrap_or_default().to_owned(),
        scoped: cx.scope().is_some(),
    }
}

/// The sites implementation: records every context; a `park` lookup waits
/// for `finish`, a `nested` one answers with what `whoami` saw; a watch
/// yields one reply, then a second once `finish` is signalled (`endless`:
/// replies without end), without keeping its context.
#[derive(Clone, Default)]
struct Pages {
    seen: Arc<Mutex<Vec<Seen>>>,
    entered: Arc<Notify>,
    finish: Arc<Notify>,
    whoami: Option<WhoamiHandle>,
}

impl Pages {
    fn record(&self, cx: &CallContext) -> SiteReply {
        self.seen
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(Seen {
                tenant: cx.tenant().map(str::to_owned),
                site: cx.scoped::<Site>().cloned(),
                scoped: cx.scope().is_some(),
                caller: cx.caller().cloned(),
                end_user: cx.end_user().map(|user| user.subject().to_owned()),
            });
        reply_of(cx)
    }

    fn seen(&self) -> Vec<Seen> {
        self.seen
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

impl Sites for Pages {
    async fn lookup(&self, cx: &CallContext, req: SiteRequest) -> Result<SiteReply, AppError> {
        let reply = self.record(cx);
        match req.code.as_str() {
            "park" => {
                self.entered.notify_one();
                self.finish.notified().await;
                Ok(reply)
            }
            "nested" => {
                let whoami = self.whoami.as_ref().expect("whoami is installed");
                whoami.ask(cx, req).await
            }
            _ => Ok(reply),
        }
    }

    async fn ping(&self, cx: &CallContext, _req: SiteRequest) -> Result<SiteReply, AppError> {
        Ok(self.record(cx))
    }

    async fn peek(&self, cx: &CallContext, _req: SiteRequest) -> Result<SiteReply, AppError> {
        Ok(self.record(cx))
    }

    async fn watch(
        &self,
        cx: &CallContext,
        req: SiteRequest,
    ) -> Result<CallStream<SiteReply, AppError>, AppError> {
        let first = self.record(cx);
        if req.code == "endless" {
            return Ok(CallStream::new(futures::stream::repeat_with(move || {
                Ok(first.clone())
            })));
        }
        let finish = Arc::clone(&self.finish);
        let rest = futures::stream::once(async move {
            finish.notified().await;
            Ok(SiteReply::default())
        });
        Ok(CallStream::new(futures::StreamExt::chain(
            futures::stream::once(std::future::ready(Ok(first))),
            rest,
        )))
    }
}

/// The `whoami` implementation.
struct Mirror;

impl Whoami for Mirror {
    async fn ask(&self, cx: &CallContext, _req: SiteRequest) -> Result<SiteReply, AppError> {
        Ok(reply_of(cx))
    }
}

/// Signals from `Drop` once the last clone of a [`Held`] is gone.
struct Tracker(watch::Sender<bool>);

impl Drop for Tracker {
    fn drop(&mut self) {
        self.0.send_replace(true);
    }
}

/// A scope value whose release is observable.
#[derive(Clone)]
struct Held(#[allow(dead_code)] Arc<Tracker>);

/// Resolves the scope from the `host` header: `a.shop.test` is tenant
/// `t-1`, `b.shop.test` tenant `t-2`, `held.shop.test` tenant `t-1` with a
/// [`Held`] value; anything else is `NOT_FOUND` / `SITE_UNKNOWN`.
#[derive(Clone, Default)]
struct Hosts {
    /// How often it ran.
    runs: Arc<Mutex<usize>>,
    /// Per `held.shop.test` scope, whether its value was released.
    held: Arc<Mutex<Vec<watch::Receiver<bool>>>>,
}

impl Hosts {
    fn runs(&self) -> usize {
        *self.runs.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn held(&self, index: usize) -> watch::Receiver<bool> {
        self.held.lock().unwrap_or_else(PoisonError::into_inner)[index].clone()
    }
}

impl ScopeResolver for Hosts {
    fn resolve<'a>(
        &'a self,
        request: &'a http::request::Parts,
        _cx: &'a CallContext,
    ) -> std::pin::Pin<Box<dyn Future<Output = Result<RequestScope, AppError>> + Send + 'a>> {
        *self.runs.lock().unwrap_or_else(PoisonError::into_inner) += 1;
        let host = request
            .headers
            .get(HOST)
            .and_then(|value| value.to_str().ok());
        let scope = match host {
            Some("a.shop.test") => Ok(RequestScope::new()
                .with_tenant("t-1")
                .with_extension(Site("a"))),
            Some("b.shop.test") => Ok(RequestScope::new()
                .with_tenant("t-2")
                .with_extension(Site("b"))),
            Some("held.shop.test") => {
                let (tx, rx) = watch::channel(false);
                self.held
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .push(rx);
                Ok(RequestScope::new()
                    .with_tenant("t-1")
                    .with_extension(Held(Arc::new(Tracker(tx)))))
            }
            _ => Err(AppError::not_found("no such site").with_reason("SITE_UNKNOWN")),
        };
        Box::pin(std::future::ready(scope))
    }
}

/// Accepts `Bearer user-<n>` (tenant `t-1`), `Bearer roamer` (no tenant)
/// and `Bearer probe`, a user of the tenant of the scope it found in the
/// request's extensions, whose subject names that tenant.
fn sessions(request: &http::request::Parts) -> Result<EndUser, AppError> {
    let token = request
        .headers
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "));
    match token {
        Some("roamer") => Ok(EndUser::new("roamer")),
        Some("probe") => {
            let scoped = request
                .extensions
                .get::<RequestScope>()
                .and_then(RequestScope::tenant);
            let user = EndUser::new(format!("saw-{}", scoped.unwrap_or("nothing")));
            Ok(match scoped {
                Some(tenant) => user.with_tenant(tenant),
                None => user,
            })
        }
        Some(token) if token.starts_with("user-") => Ok(EndUser::new(token).with_tenant("t-1")),
        _ => Err(AppError::unauthenticated("unknown session token")),
    }
}

/// A started runtime serving the sites under `/api` with CORS, to end users
/// and to the trusted link `shop`; `whoami` runs in process under the
/// binding given.
struct Shop {
    runtime: RuntimeHandle,
    addr: SocketAddr,
    pages: Pages,
    hosts: Hosts,
}

impl Shop {
    async fn stop(self) {
        self.runtime.shutdown();
        self.runtime.wait().await.unwrap();
    }
}

async fn serve() -> Shop {
    serve_with(Binding::Local).await
}

async fn serve_with(whoami: Binding) -> Shop {
    let keys: MapSource = [
        ("SEKVENT_COMPONENT_SITES_SERVE", "grpc"),
        ("SEKVENT_COMPONENT_SITES_SERVE_AUTH", "link,bearer"),
        ("SEKVENT_COMPONENT_WHOAMI_BINDING", whoami.as_str()),
        ("SEKVENT_LINK_INBOUND_SHOP", TOKEN),
        ("SEKVENT_LINK_TRUSTED", "shop"),
    ]
    .into_iter()
    .collect();
    let pages = Pages::default();
    let hosts = Hosts::default();
    let app = {
        let (pages, hosts) = (pages.clone(), hosts.clone());
        build_with(&keys, move |builder| {
            builder.end_user_authenticator(sessions)?;
            builder.scope_resolver(hosts)?;
            WhoamiHandle::install(builder, |_| Ok(Mirror))?;
            SitesHandle::install(builder, move |deps| {
                Ok(Pages {
                    whoami: Some(deps.handle::<WhoamiHandle>()?),
                    ..pages
                })
            })
        })
        .unwrap()
    };
    let server = Server::builder()
        .prefix("/api")
        .cors(Cors::origins([ORIGIN_URL]).unwrap())
        .grpc_routes(app.grpc_routes())
        .bind(SocketAddr::from(([127, 0, 0, 1], 0)))
        .await
        .unwrap();
    let addr = server.local_addr();
    let runtime = app
        .register(
            Runtime::builder()
                .without_signals()
                .shutdown_delay(Duration::ZERO),
        )
        .unit(
            "grpc",
            Stage::Ingress,
            UnitPolicy::Critical,
            server.into_unit(),
        )
        .build()
        .unwrap()
        .start()
        .await
        .unwrap();
    Shop {
        runtime,
        addr,
        pages,
        hosts,
    }
}

/// A gRPC-Web request to `path` on `host`, as a browser sends it, with
/// extra `wire` headers and the request code `code`.
fn web_request(
    path: &str,
    host: &str,
    wire: &[(&str, &str)],
    code: &str,
) -> http::Request<Full<Bytes>> {
    let encoded = SiteRequest { code: code.into() }.encode_to_vec();
    let mut frame = BytesMut::with_capacity(5 + encoded.len());
    frame.put_u8(0);
    frame.put_u32(u32::try_from(encoded.len()).unwrap());
    frame.put_slice(&encoded);
    let mut request = http::Request::post(path)
        .version(http::Version::HTTP_11)
        .header(HOST, host)
        .header(CONTENT_TYPE, "application/grpc-web+proto")
        .header("x-grpc-web", "1")
        .header(ORIGIN, ORIGIN_URL);
    for (name, value) in wire {
        request = request.header(*name, *value);
    }
    request.body(Full::new(frame.freeze())).unwrap()
}

/// One gRPC-Web exchange on a connection of its own, read frame by frame.
struct WebCall {
    headers: http::HeaderMap,
    body: hyper::body::Incoming,
    buffer: BytesMut,
    connection: tokio::task::JoinHandle<()>,
}

impl WebCall {
    async fn open(addr: SocketAddr, request: http::Request<Full<Bytes>>) -> Self {
        let tcp = tokio::net::TcpStream::connect(addr).await.unwrap();
        let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(tcp))
            .await
            .unwrap();
        let connection = tokio::spawn(async move {
            drop(connection.await);
        });
        let response = sender.send_request(request).await.unwrap();
        let (parts, body) = response.into_parts();
        Self {
            headers: parts.headers,
            body,
            buffer: BytesMut::new(),
            connection,
        }
    }

    /// Read from the body until the buffer holds a whole frame; `false` at
    /// the end of the body.
    async fn fill(&mut self) -> bool {
        while whole_frame(&self.buffer).is_none() {
            match self.body.frame().await {
                None => return false,
                Some(frame) => {
                    if let Ok(data) = frame.unwrap().into_data() {
                        self.buffer.extend_from_slice(&data);
                    }
                }
            }
        }
        true
    }

    /// The next message; `None` once the trailers (left in the buffer) or
    /// the end of the body come next.
    async fn message(&mut self) -> Option<SiteReply> {
        if !self.fill().await || self.buffer[0] & 0x80 != 0 {
            return None;
        }
        let length = whole_frame(&self.buffer).unwrap();
        let mut frame = self.buffer.split_to(length);
        frame.advance(5);
        Some(SiteReply::decode(frame.freeze()).unwrap())
    }

    /// Every remaining message, and the status: from the headers when
    /// trailers-only, else from the trailer frame.
    async fn finish(mut self) -> (Vec<SiteReply>, tonic::Status) {
        let mut messages = Vec::new();
        while let Some(message) = self.message().await {
            messages.push(message);
        }
        let mut trailers = http::HeaderMap::new();
        if let Some(length) = whole_frame(&self.buffer) {
            let mut frame = self.buffer.split_to(length);
            frame.advance(5);
            for line in std::str::from_utf8(&frame).unwrap().split("\r\n") {
                if let Some((name, value)) = line.split_once(':') {
                    trailers.insert(
                        http::HeaderName::from_bytes(name.trim().to_ascii_lowercase().as_bytes())
                            .unwrap(),
                        value.trim().parse().unwrap(),
                    );
                }
            }
        }
        let status = tonic::Status::from_header_map(&self.headers)
            .or_else(|| tonic::Status::from_header_map(&trailers))
            .expect("a grpc-status");
        (messages, status)
    }

    /// Close the connection, as a browser does when the page goes away.
    fn disconnect(self) {
        self.connection.abort();
    }
}

/// The length of the whole gRPC-Web frame at the start of `buffer`, if it
/// holds one.
fn whole_frame(buffer: &[u8]) -> Option<usize> {
    let length: [u8; 4] = buffer.get(1..5)?.try_into().unwrap();
    let length = 5 + usize::try_from(u32::from_be_bytes(length)).unwrap();
    (buffer.len() >= length).then_some(length)
}

/// One whole gRPC-Web call: its replies and its status.
async fn call(
    shop: &Shop,
    path: &str,
    host: &str,
    wire: &[(&str, &str)],
) -> (Vec<SiteReply>, tonic::Status) {
    WebCall::open(shop.addr, web_request(path, host, wire, ""))
        .await
        .finish()
        .await
}

/// The one reply of a successful unary call.
async fn reply(shop: &Shop, path: &str, host: &str, wire: &[(&str, &str)]) -> SiteReply {
    let (mut messages, status) = call(shop, path, host, wire).await;
    assert_eq!(status.code(), tonic::Code::Ok, "{status:?}");
    assert_eq!(messages.len(), 1);
    messages.remove(0)
}

fn scoped(tenant: &str, site: &'static str, end_user: Option<&str>) -> Seen {
    Seen {
        tenant: Some(tenant.into()),
        site: Some(Site(site)),
        scoped: true,
        caller: None,
        end_user: end_user.map(str::to_owned),
    }
}

/// Wait until the value behind `released` is gone.
async fn released(mut released: watch::Receiver<bool>) {
    drop(released.wait_for(|released| *released).await);
}

const USER: [(&str, &str); 1] = [("authorization", "Bearer user-1")];

#[tokio::test]
async fn every_call_no_link_authenticated_is_scoped() {
    guarded(async {
        let shop = serve().await;

        let lookup = reply(&shop, LOOKUP, "a.shop.test", &USER).await;
        assert_eq!(lookup.tenant, "t-1");
        assert_eq!(lookup.subject, "user-1");
        assert!(lookup.scoped);
        reply(&shop, PING, "a.shop.test", &[]).await;
        reply(&shop, PEEK, "b.shop.test", &[]).await;
        reply(&shop, PEEK, "a.shop.test", &USER).await;
        // Let the watch's second reply come at once.
        shop.pages.finish.notify_one();
        let (messages, status) = call(&shop, WATCH, "a.shop.test", &USER).await;
        assert_eq!(status.code(), tonic::Code::Ok, "{status:?}");
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].tenant, "t-1");

        assert_eq!(
            shop.pages.seen(),
            [
                scoped("t-1", "a", Some("user-1")),
                scoped("t-1", "a", None),
                scoped("t-2", "b", None),
                scoped("t-1", "a", Some("user-1")),
                scoped("t-1", "a", Some("user-1")),
            ]
        );
        assert_eq!(shop.hosts.runs(), 5);
        shop.stop().await;
    })
    .await;
}

#[tokio::test]
async fn an_unknown_host_gets_the_resolvers_answer() {
    guarded(async {
        let shop = serve().await;
        for path in [PING, LOOKUP] {
            let (messages, status) = call(&shop, path, "c.shop.test", &[]).await;
            assert_eq!(status.code(), tonic::Code::NotFound, "{path}");
            assert_eq!(status.message(), "no such site");
            let info = status.get_details_error_info().expect("error details");
            assert_eq!(info.reason, "SITE_UNKNOWN");
            assert!(info.metadata.is_empty(), "{:?}", info.metadata);
            assert_eq!(messages, [] as [SiteReply; 0]);
        }
        assert_eq!(shop.pages.seen(), [] as [Seen; 0]);
        shop.stop().await;
    })
    .await;
}

#[tokio::test]
async fn a_preflight_never_reaches_the_resolver() {
    guarded(async {
        let shop = serve().await;
        let preflight = http::Request::builder()
            .method(http::Method::OPTIONS)
            .uri(LOOKUP)
            .header(HOST, "a.shop.test")
            .header(ORIGIN, ORIGIN_URL)
            .header("access-control-request-method", "POST")
            .header(
                "access-control-request-headers",
                "authorization,content-type,x-grpc-web",
            )
            .body(Full::new(Bytes::new()))
            .unwrap();
        let answered = WebCall::open(shop.addr, preflight).await;
        assert_eq!(answered.headers["access-control-allow-origin"], ORIGIN_URL);
        answered.disconnect();
        assert_eq!(shop.hosts.runs(), 0);
        shop.stop().await;
    })
    .await;
}

#[tokio::test]
async fn link_callers_are_never_scoped() {
    guarded(async {
        let shop = serve().await;
        let link = format!("Bearer {TOKEN}");
        let wire = [("authorization", link.as_str()), (headers::TENANT, "t-9")];
        let linked = reply(&shop, LOOKUP, "a.shop.test", &wire).await;
        assert_eq!(linked.tenant, "t-9", "the trusted link's");
        assert!(!linked.scoped);
        assert_eq!(
            shop.pages.seen().pop().unwrap().caller,
            Some(ServiceIdentity::trusted("shop"))
        );
        assert_eq!(shop.hosts.runs(), 0);
        shop.stop().await;
    })
    .await;
}

#[tokio::test]
async fn an_in_process_callee_gets_the_tenant_and_no_scope() {
    guarded(async {
        for binding in LOCAL_BINDINGS {
            let shop = serve_with(binding).await;
            let request = web_request(LOOKUP, "a.shop.test", &USER, "nested");
            let (messages, status) = WebCall::open(shop.addr, request).await.finish().await;
            assert_eq!(status.code(), tonic::Code::Ok, "{binding}: {status:?}");
            assert_eq!(messages[0].tenant, "t-1", "{binding}");
            assert_eq!(messages[0].subject, "user-1", "{binding}");
            assert!(!messages[0].scoped, "{binding}");
            assert_eq!(shop.pages.seen(), [scoped("t-1", "a", Some("user-1"))]);
            assert_eq!(shop.hosts.runs(), 1, "the callee never resolves");
            shop.stop().await;
        }
    })
    .await;
}

#[tokio::test]
async fn the_tenant_comes_from_the_scope_never_from_a_header() {
    guarded(async {
        let shop = serve().await;
        let forged = [(headers::TENANT, "t-forged")];
        let anonymous = reply(&shop, PING, "a.shop.test", &forged).await;
        assert_eq!(anonymous.tenant, "t-1");
        let signed_in = [
            ("authorization", "Bearer user-1"),
            (headers::TENANT, "t-forged"),
        ];
        let user = reply(&shop, LOOKUP, "a.shop.test", &signed_in).await;
        assert_eq!(user.tenant, "t-1");
        assert_eq!(user.subject, "user-1");
        shop.stop().await;
    })
    .await;
}

#[tokio::test]
async fn an_end_user_of_another_tenant_or_of_none_gets_the_constant_answer() {
    guarded(async {
        let shop = serve().await;
        let roamer = [("authorization", "Bearer roamer")];
        for (host, wire) in [("b.shop.test", &USER), ("a.shop.test", &roamer)] {
            for path in [LOOKUP, PEEK, WATCH] {
                let (messages, status) = call(&shop, path, host, wire).await;
                assert_eq!(status.code(), tonic::Code::Unauthenticated, "{path}");
                assert_eq!(status.message(), END_USER_REJECTED_MESSAGE);
                assert!(status.get_details_error_info().is_none(), "{path}");
                assert_eq!(messages, [] as [SiteReply; 0]);
            }
        }
        assert_eq!(shop.pages.seen(), [] as [Seen; 0]);
        shop.stop().await;
    })
    .await;
}

#[tokio::test]
async fn the_authenticator_sees_the_scope_in_the_request_extensions() {
    guarded(async {
        let shop = serve().await;
        let probe = [("authorization", "Bearer probe")];
        let seen = reply(&shop, LOOKUP, "b.shop.test", &probe).await;
        assert_eq!(seen.subject, "saw-t-2");
        shop.stop().await;
    })
    .await;
}

#[tokio::test]
async fn a_unary_call_holds_the_scope_until_its_handler_returned() {
    guarded(async {
        let shop = serve().await;
        let request = web_request(LOOKUP, "held.shop.test", &USER, "park");
        let addr = shop.addr;
        let pending =
            tokio::spawn(async move { WebCall::open(addr, request).await.finish().await });
        shop.pages.entered.notified().await;
        let held = shop.hosts.held(0);
        assert!(!*held.borrow(), "held while the handler is parked");

        shop.pages.finish.notify_one();
        let (messages, status) = pending.await.unwrap();
        assert_eq!(status.code(), tonic::Code::Ok, "{status:?}");
        assert_eq!(messages.len(), 1);
        released(held).await;
        shop.stop().await;
    })
    .await;
}

#[tokio::test]
async fn a_stream_holds_the_scope_until_the_client_read_the_last_item() {
    guarded(async {
        let shop = serve().await;
        let request = web_request(WATCH, "held.shop.test", &USER, "");
        let mut stream = WebCall::open(shop.addr, request).await;
        let first = stream.message().await.expect("the first reply");
        assert_eq!(first.tenant, "t-1");
        let held = shop.hosts.held(0);
        assert!(!*held.borrow(), "held while the stream is open");

        shop.pages.finish.notify_one();
        let (messages, status) = stream.finish().await;
        assert_eq!(status.code(), tonic::Code::Ok, "{status:?}");
        assert_eq!(messages.len(), 1, "the second reply");
        released(held).await;
        shop.stop().await;
    })
    .await;
}

#[tokio::test]
async fn a_stream_releases_the_scope_when_the_client_goes_away() {
    guarded(async {
        let shop = serve().await;
        let request = web_request(WATCH, "held.shop.test", &USER, "endless");
        let mut stream = WebCall::open(shop.addr, request).await;
        assert!(stream.message().await.is_some());
        let held = shop.hosts.held(0);
        assert!(!*held.borrow(), "held while the stream is open");

        stream.disconnect();
        released(held).await;
        shop.stop().await;
    })
    .await;
}
