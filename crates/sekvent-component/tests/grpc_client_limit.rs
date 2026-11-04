//! Per-client limits of served methods: refused before the end-user
//! authenticator when keyed by the client address, after it when keyed by
//! the end user, shared with a REST route through `ClientLimitLayer`, and
//! never applied to in-process calls; the client address a trusted link
//! forwards, and one an untrusted link cannot.

mod support;

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use axum::Router;
use axum::routing::get;
use bytes::{Buf, BufMut, Bytes, BytesMut};
use http::header::{AUTHORIZATION, CONTENT_TYPE, RETRY_AFTER};
use http_body_util::{BodyExt, Full};
use prost::Message;
use sekvent_component::{AppError, BuildError, CallContext, EndUser, ErrorCode};
use sekvent_config::MapSource;
use sekvent_resilience::{CLIENT_RATE_LIMITED, ClientKey, ClientLimitLayer};
use sekvent_runtime::{Runtime, RuntimeHandle, Server, Stage, UnitPolicy};
use support::fakes::build_with;
use support::grpc::{OTHER_TOKEN, TOKEN, caller, guarded};
use support::inventory::{
    Inventory, InventoryError, InventoryHandle, ReleaseReply, ReleaseRequest, ReserveReply,
    ReserveRequest,
};

const RESERVE: &str = "/shop.inventory.v1.Inventory/Reserve";
const LOOPBACK: IpAddr = IpAddr::V4(Ipv4Addr::LOCALHOST);
const VISITOR: IpAddr = IpAddr::V4(Ipv4Addr::new(198, 51, 100, 7));

/// An inventory that records the client address of every call.
#[derive(Clone, Default)]
struct Storefront(Arc<Mutex<Vec<Option<IpAddr>>>>);

impl Storefront {
    fn record(&self, cx: &CallContext) {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(cx.client_addr());
    }

    fn seen(&self) -> Vec<Option<IpAddr>> {
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

/// Accepts `Bearer user-<n>`, counting every run.
fn sessions(
    runs: Arc<AtomicUsize>,
) -> impl Fn(&http::request::Parts) -> Result<EndUser, AppError> + Send + Sync + 'static {
    move |request| {
        runs.fetch_add(1, Ordering::SeqCst);
        request
            .headers
            .get(AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("Bearer "))
            .filter(|token| token.starts_with("user-"))
            .map(EndUser::new)
            .ok_or_else(|| AppError::unauthenticated("unknown session token"))
    }
}

struct Shop {
    runtime: RuntimeHandle,
    addr: SocketAddr,
    storefront: Storefront,
    authenticated: Arc<AtomicUsize>,
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

/// Serve the inventory to end users and links (`shop` trusted, `web` not)
/// with `keys` on top, and `/quote` over REST behind the limiter of
/// `inventory.reserve` when it has one.
async fn serve(keys: &[(&str, &str)]) -> Shop {
    let mut all = vec![
        ("SEKVENT_COMPONENT_INVENTORY_SERVE", "grpc"),
        ("SEKVENT_COMPONENT_INVENTORY_SERVE_AUTH", "link,bearer"),
        ("SEKVENT_LINK_INBOUND_SHOP", TOKEN),
        ("SEKVENT_LINK_INBOUND_WEB", OTHER_TOKEN),
        ("SEKVENT_LINK_TRUSTED", "shop"),
    ];
    all.extend_from_slice(keys);
    let source: MapSource = all.into_iter().collect();
    let storefront = Storefront::default();
    let authenticated = Arc::new(AtomicUsize::new(0));
    let installed = storefront.clone();
    let runs = Arc::clone(&authenticated);
    let app = build_with(&source, move |builder| {
        builder.end_user_authenticator(sessions(runs))?;
        InventoryHandle::install(builder, move |_| Ok(installed))
    })
    .unwrap();
    let mut rest = Router::new().route("/quote", get(|| async { "quoted" }));
    if let Some(limiter) = app.client_limiter("inventory", "reserve") {
        rest = rest.layer(ClientLimitLayer::new(limiter));
    }
    let server = Server::builder()
        .rest(rest)
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
        storefront,
        authenticated,
    }
}

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

/// A unary gRPC-Web `Reserve` call, as a browser makes it; its status.
async fn reserve_over_web(addr: SocketAddr, authorization: Option<&str>) -> tonic::Status {
    let encoded = reserve().encode_to_vec();
    let mut frame = BytesMut::with_capacity(5 + encoded.len());
    frame.put_u8(0);
    frame.put_u32(u32::try_from(encoded.len()).unwrap());
    frame.put_slice(&encoded);
    let mut request = http::Request::post(RESERVE)
        .header(CONTENT_TYPE, "application/grpc-web+proto")
        .header("x-grpc-web", "1");
    if let Some(value) = authorization {
        request = request.header(AUTHORIZATION, value);
    }
    let (_, headers, mut body) = send(addr, request.body(Full::new(frame.freeze())).unwrap()).await;
    let mut trailers = http::HeaderMap::new();
    while body.has_remaining() {
        let flag = body.get_u8();
        let length = usize::try_from(body.get_u32()).unwrap();
        let payload = body.split_to(length);
        if flag & 0x80 == 0 {
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
    tonic::Status::from_header_map(&headers)
        .or_else(|| tonic::Status::from_header_map(&trailers))
        .expect("a grpc-status")
}

async fn quote(addr: SocketAddr) -> (http::StatusCode, http::HeaderMap) {
    let request = http::Request::get("/quote")
        .body(Full::new(Bytes::new()))
        .unwrap();
    let (status, headers, _) = send(addr, request).await;
    (status, headers)
}

fn reserve() -> ReserveRequest {
    ReserveRequest {
        order_id: "o1".into(),
        sku: "sku-1".into(),
        quantity: 1,
    }
}

fn release() -> ReleaseRequest {
    ReleaseRequest {
        reservation_id: "res-o1".into(),
    }
}

fn rate_limited(status: &tonic::Status) -> AppError {
    assert_eq!(status.code(), tonic::Code::ResourceExhausted, "{status:?}");
    let error = AppError::from(status.clone());
    assert_eq!(error.reason(), Some(CLIENT_RATE_LIMITED));
    error
}

#[tokio::test]
async fn an_address_flood_is_refused_before_the_authenticator_and_shares_rest_buckets() {
    guarded(async {
        let shop = serve(&[
            ("SEKVENT_COMPONENT_INVENTORY_RESERVE_POLICY", "orders"),
            ("SEKVENT_POLICY_ORDERS_CLIENT_RATE", "3/1h"),
        ])
        .await;

        // REST and gRPC-Web from one address drain one bucket.
        assert_eq!(quote(shop.addr).await.0, http::StatusCode::OK);
        let signed_in = reserve_over_web(shop.addr, Some("Bearer user-1")).await;
        assert_eq!(signed_in.code(), tonic::Code::Ok, "{signed_in:?}");
        let forged = reserve_over_web(shop.addr, Some("Bearer forged")).await;
        assert_eq!(forged.code(), tonic::Code::Unauthenticated);
        assert_eq!(shop.authenticated.load(Ordering::SeqCst), 2);

        // The flood: refused without running the authenticator or the handler.
        for authorization in [None, Some("Bearer user-1"), Some("Bearer forged")] {
            let error = rate_limited(&reserve_over_web(shop.addr, authorization).await);
            let retry_after = error.retry_after().expect("a retry_after");
            assert!(retry_after > Duration::from_mins(19), "{retry_after:?}");
            assert_eq!(
                error.metadata().get("method").map(String::as_str),
                Some("reserve")
            );
        }
        assert_eq!(shop.authenticated.load(Ordering::SeqCst), 2);
        assert_eq!(shop.storefront.seen(), [Some(LOOPBACK)]);
        let (status, headers) = quote(shop.addr).await;
        assert_eq!(status, http::StatusCode::TOO_MANY_REQUESTS);
        assert!(headers.contains_key(RETRY_AFTER));

        // A trusted link calls for its visitor, who has a bucket of its own.
        let (app, handle) = caller(&shop.endpoint(), &[]).await;
        let visitor = CallContext::new().with_client_addr(VISITOR);
        handle.reserve(&visitor, reserve()).await.unwrap();
        assert_eq!(shop.storefront.seen().pop().unwrap(), Some(VISITOR));
        // Without one, the link counts as its own address: refused, and
        // never retried.
        let refused = match handle.reserve(&CallContext::new(), reserve()).await {
            Err(InventoryError::Other(error)) => error,
            other => panic!("expected the client limit, got {other:?}"),
        };
        assert_eq!(refused.code(), ErrorCode::ResourceExhausted);
        assert_eq!(refused.reason(), Some(CLIENT_RATE_LIMITED));
        app.stop(Duration::ZERO).await.unwrap();

        // Unlimited methods are not limited.
        let (app, handle) = caller(&shop.endpoint(), &[]).await;
        for _ in 0..5 {
            handle
                .release(&CallContext::new(), release())
                .await
                .unwrap();
        }
        app.stop(Duration::ZERO).await.unwrap();

        shop.stop().await;
    })
    .await;
}

#[tokio::test]
async fn only_a_trusted_link_forwards_the_client_address() {
    guarded(async {
        let shop = serve(&[]).await;
        let visitor = CallContext::new().with_client_addr(VISITOR);

        let (app, handle) = caller(&shop.endpoint(), &[]).await;
        handle.release(&visitor, release()).await.unwrap();
        handle
            .release(&CallContext::new(), release())
            .await
            .unwrap();
        app.stop(Duration::ZERO).await.unwrap();

        let (app, handle) = caller(
            &shop.endpoint(),
            &[("SEKVENT_LINK_OUTBOUND_INVENTORY", OTHER_TOKEN)],
        )
        .await;
        handle.release(&visitor, release()).await.unwrap();
        app.stop(Duration::ZERO).await.unwrap();

        assert_eq!(
            shop.storefront.seen(),
            [Some(VISITOR), Some(LOOPBACK), Some(LOOPBACK)],
            "the untrusted link's claim is replaced by its own address"
        );
        assert_eq!(quote(shop.addr).await.0, http::StatusCode::OK, "no limit");
        shop.stop().await;
    })
    .await;
}

#[tokio::test]
async fn an_end_user_limit_runs_after_the_authenticator() {
    guarded(async {
        let shop = serve(&[
            ("SEKVENT_COMPONENT_INVENTORY_RESERVE_CLIENT_RATE", "1/1h"),
            ("SEKVENT_COMPONENT_INVENTORY_RESERVE_CLIENT_KEY", "end_user"),
        ])
        .await;

        let first = reserve_over_web(shop.addr, Some("Bearer user-1")).await;
        assert_eq!(first.code(), tonic::Code::Ok, "{first:?}");
        rate_limited(&reserve_over_web(shop.addr, Some("Bearer user-1")).await);
        assert_eq!(
            shop.authenticated.load(Ordering::SeqCst),
            2,
            "the key is known only after authentication"
        );
        let other = reserve_over_web(shop.addr, Some("Bearer user-2")).await;
        assert_eq!(other.code(), tonic::Code::Ok, "{other:?}");
        let rejected = reserve_over_web(shop.addr, Some("Bearer forged")).await;
        assert_eq!(rejected.code(), tonic::Code::Unauthenticated);

        // A trusted link's asserted subject is the key.
        let (app, handle) = caller(&shop.endpoint(), &[]).await;
        let buyer = CallContext::new().with_subject("user-3");
        handle.reserve(&buyer, reserve()).await.unwrap();
        assert!(handle.reserve(&buyer, reserve()).await.is_err());
        // Without one: the shared unknown bucket.
        handle
            .reserve(&CallContext::new(), reserve())
            .await
            .unwrap();
        assert!(
            handle
                .reserve(&CallContext::new(), reserve())
                .await
                .is_err()
        );
        app.stop(Duration::ZERO).await.unwrap();

        assert_eq!(shop.storefront.seen().len(), 4);
        shop.stop().await;
    })
    .await;
}

#[tokio::test]
async fn in_process_calls_are_never_limited() {
    let source: MapSource = [
        ("SEKVENT_COMPONENT_INVENTORY_RESERVE_CLIENT_RATE", "1/1h"),
        ("SEKVENT_COMPONENT_INVENTORY_RELEASE_POLICY", "public"),
        ("SEKVENT_POLICY_PUBLIC_CLIENT_RATE", "1/1h"),
        (
            "SEKVENT_POLICY_PUBLIC_CLIENT_KEY",
            "end_user_or_client_addr",
        ),
    ]
    .into_iter()
    .collect();
    for binding in ["local", "local-serialized"] {
        let mut keys = source.clone();
        keys.set("SEKVENT_COMPONENT_INVENTORY_BINDING", binding);
        let storefront = Storefront::default();
        let installed = storefront.clone();
        let app = build_with(&keys, move |builder| {
            InventoryHandle::install(builder, move |_| Ok(installed))
        })
        .unwrap();
        app.start().await.unwrap();
        let handle = app.handle::<InventoryHandle>().unwrap();
        let cx = CallContext::new().with_client_addr(VISITOR);
        for _ in 0..3 {
            handle.reserve(&cx, reserve()).await.unwrap();
            handle.release(&cx, release()).await.unwrap();
        }
        assert_eq!(storefront.seen(), [Some(VISITOR); 6], "{binding}");

        let limiter = app.client_limiter("inventory", "release").unwrap();
        assert_eq!(limiter.key(), ClientKey::EndUserOrClientAddr);
        assert!(limiter.limiter().is_empty(), "{binding}: untouched");
        assert!(app.client_limiter("inventory", "reserve").is_some());
        assert!(app.client_limiter("inventory", "missing").is_none());
        assert!(app.client_limiter("ledger", "reserve").is_none());
        app.stop(Duration::ZERO).await.unwrap();
    }
}

#[test]
fn client_limits_are_method_and_named_policy_keys_only() {
    let source: MapSource = [
        ("SEKVENT_COMPONENT_INVENTORY_CLIENT_RATE", "5/1m"),
        (
            "SEKVENT_COMPONENT_INVENTORY_RESERVE_RATE_LIMIT_PERMITS",
            "5",
        ),
        ("SEKVENT_COMPONENT_INVENTORY_RESERVE_CLIENT_RATE", "fast"),
    ]
    .into_iter()
    .collect();
    let error = build_with(&source, |builder| {
        InventoryHandle::install(builder, |_| Ok(Storefront::default()))
    })
    .unwrap_err();
    let BuildError::Multiple(errors) = &error else {
        panic!("{error}");
    };
    let text = error.to_string();
    assert_eq!(errors.len(), 2, "{text}");
    assert!(
        text.contains("SEKVENT_COMPONENT_INVENTORY_CLIENT_RATE"),
        "{text}"
    );
    assert!(
        text.contains("SEKVENT_COMPONENT_INVENTORY_RESERVE_RATE_LIMIT_PERMITS"),
        "{text}"
    );
    assert!(
        text.contains("SEKVENT_COMPONENT_INVENTORY_RESERVE_CLIENT_RATE"),
        "{text}"
    );
    assert!(!text.contains("fast"), "values are never shown: {text}");
}
