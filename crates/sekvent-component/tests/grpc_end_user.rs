//! Components served to end users: gRPC-Web over HTTP/1.1 through the
//! runtime server under a path prefix with CORS, bearer tokens checked by
//! the App's end-user authenticator, anonymous and optional-end-user
//! methods, and link callers next to end users.

mod support;

use std::net::SocketAddr;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use bytes::{Buf, BufMut, Bytes, BytesMut};
use http::header::{AUTHORIZATION, CONTENT_TYPE, ORIGIN};
use http_body_util::{BodyExt, Full};
use prost::Message;
use sekvent_component::{
    AppError, BuildError, CallContext, END_USER_REJECTED_MESSAGE, EndUser, ErrorCode,
};
use sekvent_config::MapSource;
use sekvent_context::ServiceIdentity;
use sekvent_runtime::{Cors, Runtime, RuntimeHandle, Server, Stage, UnitPolicy};
use support::fakes::build_with;
use support::grpc::{TOKEN, caller, guarded};
use support::inventory::{
    Inventory, InventoryError, InventoryHandle, ReleaseReply, ReleaseRequest, ReserveReply,
    ReserveRequest,
};
use tracking::{TrackReply, TrackRequest, Tracker, TrackerHandle};

const ORIGIN_URL: &str = "https://shop.example";
const RESERVE: &str = "/api/shop.inventory.v1.Inventory/Reserve";
const RELEASE: &str = "/api/shop.inventory.v1.Inventory/Release";
const TRACK: &str = "/api/shop.tracking.v1.Tracker/Track";

/// A one-method component declared with `#[component]`, for the method kind
/// the hand-expanded inventory does not carry: `#[call(optional_end_user)]`.
#[allow(dead_code, missing_docs, unreachable_pub)]
mod tracking {
    use sekvent_component::{AppError, CallContext};

    /// The contract sekvent-proto-build would generate for
    /// `shop.tracking.v1.Tracker`.
    pub mod proto {
        #[allow(non_upper_case_globals)]
        pub const __sekvent_service_Tracker: (&str, &[(&str, &str, &str, bool)]) = (
            "shop.tracking.v1.Tracker",
            &[(
                "Track",
                "shop.tracking.v1.TrackRequest",
                "shop.tracking.v1.TrackReply",
                false,
            )],
        );
        #[allow(non_camel_case_types)]
        pub type __sekvent_rpc_Tracker__Track = (super::TrackRequest, super::TrackReply);
    }

    /// Track an order by its public code.
    #[derive(Clone, PartialEq, prost::Message)]
    pub struct TrackRequest {
        #[prost(string, tag = "1")]
        pub code: String,
    }

    impl prost::Name for TrackRequest {
        const NAME: &'static str = "TrackRequest";
        const PACKAGE: &'static str = "shop.tracking.v1";
    }

    /// Whether the viewer was signed in.
    #[derive(Clone, PartialEq, prost::Message)]
    pub struct TrackReply {
        #[prost(bool, tag = "1")]
        pub signed_in: bool,
    }

    impl prost::Name for TrackReply {
        const NAME: &'static str = "TrackReply";
        const PACKAGE: &'static str = "shop.tracking.v1";
    }

    /// Order tracking, open to everyone and personal for signed-in buyers.
    #[sekvent_component::component(
        name = "tracker",
        package = "shop.tracking.v1",
        proto = "crate::tracking::proto"
    )]
    pub trait Tracker: Send + Sync + 'static {
        /// The order's state, as the viewer may see it.
        #[call(optional_end_user)]
        async fn track(&self, cx: &CallContext, req: TrackRequest) -> Result<TrackReply, AppError>;
    }
}

/// What the implementation saw of one call.
#[derive(Debug, Clone, PartialEq)]
struct Seen {
    end_user: Option<EndUser>,
    caller: Option<ServiceIdentity>,
    subject: Option<String>,
    tenant: Option<String>,
}

/// An inventory that records who called it.
#[derive(Clone, Default)]
struct Storefront(Arc<Mutex<Vec<Seen>>>);

impl Storefront {
    fn record(&self, cx: &CallContext) {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(Seen {
                end_user: cx.end_user().cloned(),
                caller: cx.caller().cloned(),
                subject: cx.subject().map(str::to_owned),
                tenant: cx.tenant().map(str::to_owned),
            });
    }

    fn seen(&self) -> Vec<Seen> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

impl Inventory for Storefront {
    async fn reserve(
        &self,
        cx: &CallContext,
        req: ReserveRequest,
    ) -> Result<ReserveReply, InventoryError> {
        self.record(cx);
        Ok(ReserveReply {
            reservation_id: format!("res-{}", req.order_id),
            remaining: 1,
        })
    }

    async fn release(
        &self,
        cx: &CallContext,
        _req: ReleaseRequest,
    ) -> Result<ReleaseReply, InventoryError> {
        self.record(cx);
        Ok(ReleaseReply { released: true })
    }
}

impl Tracker for Storefront {
    async fn track(&self, cx: &CallContext, _req: TrackRequest) -> Result<TrackReply, AppError> {
        self.record(cx);
        Ok(TrackReply {
            signed_in: cx.end_user().is_some(),
        })
    }
}

/// Accepts `Bearer user-<n>` session tokens.
fn sessions(request: &http::request::Parts) -> Result<EndUser, AppError> {
    request
        .headers
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .filter(|token| token.starts_with("user-"))
        .map(|token| EndUser::new(token).with_tenant("t-1").with_roles(["buyer"]))
        .ok_or_else(|| AppError::unauthenticated("unknown session token"))
}

/// A started runtime serving the storefront (inventory and tracker) under
/// `/api` with CORS.
struct Shop {
    runtime: RuntimeHandle,
    addr: SocketAddr,
}

impl Shop {
    fn endpoint(&self) -> String {
        format!("http://{}", self.addr)
    }

    async fn stop(self) {
        self.runtime.shutdown();
        self.runtime.wait().await.unwrap();
    }
}

async fn serve(serve_auth: &str, storefront: Storefront) -> Shop {
    let keys: MapSource = [
        ("SEKVENT_COMPONENT_INVENTORY_SERVE", "grpc"),
        ("SEKVENT_COMPONENT_INVENTORY_SERVE_AUTH", serve_auth),
        ("SEKVENT_COMPONENT_TRACKER_SERVE", "grpc"),
        ("SEKVENT_COMPONENT_TRACKER_SERVE_AUTH", serve_auth),
        ("SEKVENT_LINK_INBOUND_SHOP", TOKEN),
        ("SEKVENT_LINK_TRUSTED", "shop"),
    ]
    .into_iter()
    .collect();
    let app = build_with(&keys, move |builder| {
        builder.end_user_authenticator(sessions)?;
        let tracker = storefront.clone();
        TrackerHandle::install(builder, move |_| Ok(tracker))?;
        InventoryHandle::install(builder, move |_| Ok(storefront))
    })
    .unwrap();
    let server = Server::builder()
        .prefix("/api")
        .cors(Cors::origins([ORIGIN_URL]).unwrap())
        // A server-level identity never decides component calls.
        .authenticator(|_| Some(ServiceIdentity::untrusted("edge")))
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
    Shop { runtime, addr }
}

/// One gRPC-Web answer: the HTTP head, the reply message (if any) and the
/// gRPC status, from the headers when trailers-only, else from the trailer
/// frame in the body.
struct WebReply {
    status: http::StatusCode,
    headers: http::HeaderMap,
    message: Option<Bytes>,
    grpc: tonic::Status,
}

/// Send `request` (its URI a path) to `addr` over HTTP/1.1; the status,
/// the headers and the whole body.
async fn send(
    addr: SocketAddr,
    request: http::Request<Full<Bytes>>,
) -> (http::StatusCode, http::HeaderMap, Bytes) {
    let client = hyper_util::client::legacy::Client::builder(hyper_util::rt::TokioExecutor::new())
        .build_http::<Full<Bytes>>();
    let (mut parts, body) = request.into_parts();
    parts.uri = format!("http://{addr}{}", parts.uri).parse().unwrap();
    let response = client
        .request(http::Request::from_parts(parts, body))
        .await
        .unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    (status, headers, body)
}

/// A unary gRPC-Web call over HTTP/1.1, as a browser makes it.
async fn grpc_web(
    addr: SocketAddr,
    path: &str,
    authorization: Option<&str>,
    message: &impl Message,
) -> WebReply {
    let encoded = message.encode_to_vec();
    let mut frame = BytesMut::with_capacity(5 + encoded.len());
    frame.put_u8(0);
    frame.put_u32(u32::try_from(encoded.len()).unwrap());
    frame.put_slice(&encoded);
    let mut request = http::Request::post(path)
        .version(http::Version::HTTP_11)
        .header(CONTENT_TYPE, "application/grpc-web+proto")
        .header("x-grpc-web", "1")
        .header(ORIGIN, ORIGIN_URL);
    if let Some(value) = authorization {
        request = request.header(AUTHORIZATION, value);
    }
    let (status, headers, mut body) =
        send(addr, request.body(Full::new(frame.freeze())).unwrap()).await;
    let mut message = None;
    let mut trailers = http::HeaderMap::new();
    while body.has_remaining() {
        let flag = body.get_u8();
        let length = usize::try_from(body.get_u32()).unwrap();
        let payload = body.split_to(length);
        if flag & 0x80 == 0 {
            message = Some(payload);
            continue;
        }
        for line in std::str::from_utf8(&payload).unwrap().split("\r\n") {
            if let Some((name, value)) = line.split_once(':') {
                trailers.insert(
                    http::HeaderName::from_bytes(name.trim().to_ascii_lowercase().as_bytes())
                        .unwrap(),
                    value.trim().parse().unwrap(),
                );
            }
        }
    }
    let grpc = tonic::Status::from_header_map(&headers)
        .or_else(|| tonic::Status::from_header_map(&trailers))
        .expect("a grpc-status");
    WebReply {
        status,
        headers,
        message,
        grpc,
    }
}

fn reserve() -> ReserveRequest {
    ReserveRequest {
        order_id: "o1".into(),
        sku: "sku-1".into(),
        quantity: 1,
    }
}

#[tokio::test]
async fn end_users_reach_components_over_grpc_web() {
    guarded(async {
        let storefront = Storefront::default();
        let shop = serve("link,bearer", storefront.clone()).await;

        // A browser's preflight is answered before authentication.
        let preflight = http::Request::builder()
            .method(http::Method::OPTIONS)
            .uri(RESERVE)
            .header(ORIGIN, ORIGIN_URL)
            .header("access-control-request-method", "POST")
            .header(
                "access-control-request-headers",
                "authorization,content-type,x-grpc-web",
            )
            .body(Full::new(Bytes::new()))
            .unwrap();
        let (status, headers, _) = send(shop.addr, preflight).await;
        assert!(status.is_success(), "{status}");
        assert_eq!(headers["access-control-allow-origin"], ORIGIN_URL);
        let allowed = headers["access-control-allow-headers"]
            .to_str()
            .unwrap()
            .to_owned();
        assert!(allowed.contains("authorization"), "{allowed}");
        assert!(allowed.contains("x-grpc-web"), "{allowed}");
        assert!(storefront.seen().is_empty());

        // A signed-in end user.
        let reply = grpc_web(shop.addr, RESERVE, Some("Bearer user-7"), &reserve()).await;
        assert_eq!(reply.status, http::StatusCode::OK);
        assert_eq!(reply.grpc.code(), tonic::Code::Ok, "{:?}", reply.grpc);
        assert_eq!(reply.headers["access-control-allow-origin"], ORIGIN_URL);
        let decoded = ReserveReply::decode(reply.message.unwrap()).unwrap();
        assert_eq!(decoded.reservation_id, "res-o1");
        assert_eq!(
            storefront.seen(),
            [Seen {
                end_user: Some(
                    EndUser::new("user-7")
                        .with_tenant("t-1")
                        .with_roles(["buyer"])
                ),
                caller: None,
                subject: Some("user-7".into()),
                tenant: Some("t-1".into()),
            }]
        );

        // No token and a wrong one get the same answer; the handler never runs.
        let missing = grpc_web(shop.addr, RESERVE, None, &reserve()).await;
        let wrong = grpc_web(shop.addr, RESERVE, Some("Bearer forged"), &reserve()).await;
        for reply in [&missing, &wrong] {
            assert_eq!(reply.grpc.code(), tonic::Code::Unauthenticated);
            assert_eq!(reply.grpc.message(), END_USER_REJECTED_MESSAGE);
            assert!(reply.message.is_none());
        }
        assert_eq!(storefront.seen().len(), 1);

        // An anonymous method needs no token and sees no identity.
        let reply = grpc_web(
            shop.addr,
            RELEASE,
            None,
            &ReleaseRequest {
                reservation_id: "res-o1".into(),
            },
        )
        .await;
        assert_eq!(reply.grpc.code(), tonic::Code::Ok, "{:?}", reply.grpc);
        assert!(
            ReleaseReply::decode(reply.message.unwrap())
                .unwrap()
                .released
        );
        let anonymous = storefront.seen().pop().unwrap();
        assert_eq!(anonymous.end_user, None);
        assert_eq!(anonymous.caller, None);

        // A peer service on native gRPC at the root, next to the end users.
        let tenant = CallContext::new().with_tenant("t-9");
        let (app, handle) = caller(&shop.endpoint(), &[]).await;
        handle.reserve(&tenant, reserve()).await.unwrap();
        let peer = storefront.seen().pop().unwrap();
        assert_eq!(peer.caller, Some(ServiceIdentity::trusted("shop")));
        assert_eq!(peer.end_user, None);
        assert_eq!(peer.tenant.as_deref(), Some("t-9"));
        app.stop(Duration::ZERO).await.unwrap();

        shop.stop().await;
    })
    .await;
}

/// One `Track` call over gRPC-Web.
async fn track(shop: &Shop, authorization: Option<&str>) -> WebReply {
    let request = TrackRequest { code: "o1".into() };
    grpc_web(shop.addr, TRACK, authorization, &request).await
}

/// Whether a successful `Track` reply saw a signed-in viewer.
fn signed_in(reply: WebReply) -> bool {
    assert_eq!(reply.grpc.code(), tonic::Code::Ok, "{:?}", reply.grpc);
    TrackReply::decode(reply.message.unwrap())
        .unwrap()
        .signed_in
}

fn buyer(subject: &str) -> Seen {
    Seen {
        end_user: Some(
            EndUser::new(subject)
                .with_tenant("t-1")
                .with_roles(["buyer"]),
        ),
        caller: None,
        subject: Some(subject.into()),
        tenant: Some("t-1".into()),
    }
}

const NOBODY: Seen = Seen {
    end_user: None,
    caller: None,
    subject: None,
    tenant: None,
};

#[tokio::test]
async fn optional_end_users_are_authenticated_only_when_they_send_a_credential() {
    guarded(async {
        let storefront = Storefront::default();
        let shop = serve("bearer", storefront.clone()).await;

        // No credential: served, without an end user.
        assert!(!signed_in(track(&shop, None).await));
        assert_eq!(storefront.seen().pop(), Some(NOBODY));

        // A valid credential: the end user, as for any other method.
        assert!(signed_in(track(&shop, Some("Bearer user-5")).await));
        assert_eq!(storefront.seen().pop(), Some(buyer("user-5")));

        // A rejected credential is refused, never served as nobody; the
        // handler does not run.
        for authorization in ["Bearer forged", "Basic dXNlcg==", ""] {
            let reply = track(&shop, Some(authorization)).await;
            assert_eq!(reply.grpc.code(), tonic::Code::Unauthenticated);
            assert_eq!(reply.grpc.message(), END_USER_REJECTED_MESSAGE);
            assert!(reply.message.is_none());
        }
        assert_eq!(storefront.seen().len(), 2);

        shop.stop().await;
    })
    .await;
}

#[tokio::test]
async fn optional_end_users_next_to_links() {
    guarded(async {
        let link_token = format!("Bearer {TOKEN}");

        // link,bearer: a link, an end user, or nobody.
        let storefront = Storefront::default();
        let shop = serve("link,bearer", storefront.clone()).await;
        assert!(!signed_in(track(&shop, Some(link_token.as_str())).await));
        let link = storefront.seen().pop().unwrap();
        assert_eq!(link.caller, Some(ServiceIdentity::trusted("shop")));
        assert_eq!(link.end_user, None);
        assert!(signed_in(track(&shop, Some("Bearer user-6")).await));
        assert_eq!(storefront.seen().pop(), Some(buyer("user-6")));
        assert!(!signed_in(track(&shop, None).await));
        assert_eq!(storefront.seen().pop(), Some(NOBODY));
        let reply = track(&shop, Some("Bearer forged")).await;
        assert_eq!(reply.grpc.message(), END_USER_REJECTED_MESSAGE);
        shop.stop().await;

        // link: the attribute never waives the link.
        let storefront = Storefront::default();
        let shop = serve("link", storefront.clone()).await;
        for authorization in [None, Some("Bearer user-6")] {
            let reply = track(&shop, authorization).await;
            assert_eq!(reply.grpc.code(), tonic::Code::Unauthenticated);
            assert_ne!(reply.grpc.message(), END_USER_REJECTED_MESSAGE);
        }
        assert!(storefront.seen().is_empty());
        assert!(!signed_in(track(&shop, Some(link_token.as_str())).await));
        assert_eq!(
            storefront.seen().pop().unwrap().caller,
            Some(ServiceIdentity::trusted("shop"))
        );
        shop.stop().await;

        // none: anyone, and the authenticator never runs.
        let storefront = Storefront::default();
        let shop = serve("none", storefront.clone()).await;
        for authorization in [None, Some("Bearer user-6"), Some("Bearer forged")] {
            assert!(!signed_in(track(&shop, authorization).await));
            assert_eq!(storefront.seen().pop(), Some(NOBODY));
        }
        shop.stop().await;
    })
    .await;
}

#[tokio::test]
async fn bearer_serving_does_not_accept_link_tokens() {
    guarded(async {
        let storefront = Storefront::default();
        let shop = serve("bearer", storefront.clone()).await;

        // The link token is not a session the authenticator knows.
        let (app, handle) = caller(&shop.endpoint(), &[]).await;
        let error = match handle.reserve(&CallContext::new(), reserve()).await {
            Err(InventoryError::Other(error)) => error,
            other => panic!("expected an authentication error, got {other:?}"),
        };
        assert_eq!(error.code(), ErrorCode::Unauthenticated);
        assert_eq!(error.message(), END_USER_REJECTED_MESSAGE);
        app.stop(Duration::ZERO).await.unwrap();
        assert!(storefront.seen().is_empty());

        let reply = grpc_web(shop.addr, RESERVE, Some("Bearer user-3"), &reserve()).await;
        assert_eq!(reply.grpc.code(), tonic::Code::Ok, "{:?}", reply.grpc);
        assert_eq!(
            storefront.seen()[0].end_user.as_ref().map(EndUser::subject),
            Some("user-3")
        );

        shop.stop().await;
    })
    .await;
}

#[test]
fn end_user_serving_fails_closed_without_an_authenticator() {
    let keys: MapSource = [
        ("SEKVENT_COMPONENT_INVENTORY_SERVE", "grpc"),
        ("SEKVENT_COMPONENT_INVENTORY_SERVE_AUTH", "bearer"),
    ]
    .into_iter()
    .collect();
    let error = build_with(&keys, |builder| {
        InventoryHandle::install(builder, |_| Ok(Storefront::default()))
    })
    .unwrap_err();
    assert!(
        matches!(&error, BuildError::EndUserAuthenticatorMissing { component, key }
            if component == "inventory" && key == "SEKVENT_COMPONENT_INVENTORY_SERVE_AUTH"),
        "{error}"
    );

    let error = build_with(&MapSource::new(), |builder| {
        builder.end_user_authenticator(sessions)?;
        builder.end_user_authenticator(sessions)
    })
    .unwrap_err();
    assert!(
        matches!(error, BuildError::DuplicateEndUserAuthenticator),
        "{error}"
    );

    // Registered but unused: not an error, and invisible to local calls.
    let app = build_with(&MapSource::new(), |builder| {
        builder.end_user_authenticator(sessions)?;
        InventoryHandle::install(builder, |_| Ok(Storefront::default()))
    })
    .unwrap();
    assert!(app.grpc_services().is_empty());
}
