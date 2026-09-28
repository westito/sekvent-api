//! The inventory served over gRPC on loopback and called through the `grpc`
//! binding or a plain tonic client: results, typed errors, authentication,
//! the forwarded context, hop limits, malformed requests and health, and
//! the header-only decisions made before a request body is read.

mod support;

use std::net::SocketAddr;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use bytes::Bytes;
use http::uri::PathAndQuery;
use http_body::{Body as _, Frame};
use sekvent_component::{AppError, Binding, CallContext, ErrorCode, reasons};
use sekvent_config::{ConfigError, MapSource};
use sekvent_error::grpc::from_status;
use sekvent_runtime::ServiceStatus;
use support::fakes::{Behaviour, FakeInventory, Probe, build_with};
use support::grpc::{
    OTHER_TOKEN, SERVICE, Service, TOKEN, caller, caller_app, caller_keys, guarded, serve,
    service_keys,
};
use support::inventory::{
    InventoryError, InventoryHandle, ReleaseRequest, ReserveReply, ReserveRequest,
};
use tokio::sync::mpsc;
use tonic_types::StatusExt as _;

const RESERVE: &str = "/shop.inventory.v1.Inventory/Reserve";
const TRACEPARENT: &str = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";

fn reserve(quantity: u32) -> ReserveRequest {
    ReserveRequest {
        order_id: "o1".into(),
        sku: "sku-1".into(),
        quantity,
    }
}

fn with(mut keys: Vec<(String, String)>, extra: &[(&str, &str)]) -> Vec<(String, String)> {
    keys.extend(
        extra
            .iter()
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned())),
    );
    keys
}

async fn inventory_service(
    keys: &[(String, String)],
    inventory: FakeInventory,
) -> (Service, Probe) {
    let probe = inventory.probe.clone();
    let service = serve(keys, move |builder| {
        InventoryHandle::install(builder, move |_| Ok(inventory))
    })
    .await;
    (service, probe)
}

fn other(error: InventoryError) -> AppError {
    match error {
        InventoryError::Other(error) => error,
        unexpected => panic!("expected Other, got {unexpected:?}"),
    }
}

/// A message whose field 1 is a varint, which no inventory request accepts
/// as its string field 1.
#[derive(Clone, PartialEq, prost::Message)]
struct Garbage {
    #[prost(uint64, tag = "1")]
    value: u64,
}

/// One unary call through a plain tonic client with prost messages.
async fn raw_call<Req, Rep>(
    addr: SocketAddr,
    path: &'static str,
    request: Req,
    bearer: Option<&str>,
    timeout: Option<Duration>,
) -> Result<Rep, tonic::Status>
where
    Req: prost::Message + Send + Sync + 'static,
    Rep: prost::Message + Default + Send + Sync + 'static,
{
    let channel = tonic::transport::Endpoint::from_shared(format!("http://{addr}"))
        .unwrap()
        .connect()
        .await
        .unwrap();
    let mut grpc = tonic::client::Grpc::new(channel);
    grpc.ready().await.unwrap();
    let mut request = tonic::Request::new(request);
    if let Some(token) = bearer {
        request
            .metadata_mut()
            .insert("authorization", format!("Bearer {token}").parse().unwrap());
    }
    if let Some(timeout) = timeout {
        request.set_timeout(timeout);
    }
    grpc.unary(
        request,
        PathAndQuery::from_static(path),
        tonic_prost::ProstCodec::<Req, Rep>::default(),
    )
    .await
    .map(tonic::Response::into_inner)
}

#[tokio::test]
async fn calls_and_typed_errors_cross_the_wire() {
    guarded(async {
        let (service, _) = inventory_service(&service_keys(), FakeInventory::stock(5)).await;
        assert_eq!(service.app.grpc_services(), [SERVICE]);
        assert_eq!(service.app.binding("inventory"), Some(Binding::Local));
        let (app, inventory) = caller(&service.endpoint(), &[]).await;
        assert_eq!(inventory.binding(), Binding::Grpc);
        assert!(app.grpc_services().is_empty());

        let cx = CallContext::new();
        let reply = inventory.reserve(&cx, reserve(2)).await.unwrap();
        assert_eq!(
            reply,
            ReserveReply {
                reservation_id: "res-o1".into(),
                remaining: 3,
            }
        );
        let error = inventory.reserve(&cx, reserve(9)).await.unwrap_err();
        assert!(
            matches!(&error, InventoryError::OutOfStock { sku, available: 3 } if sku == "sku-1"),
            "{error:?}"
        );
        let released = inventory
            .release(
                &cx,
                ReleaseRequest {
                    reservation_id: "res-o1".into(),
                },
            )
            .await
            .unwrap();
        assert!(released.released);
        let error = inventory
            .release(
                &cx,
                ReleaseRequest {
                    reservation_id: "x-1".into(),
                },
            )
            .await
            .unwrap_err();
        match error {
            InventoryError::ReservationNotFound {
                reservation_id,
                hint,
            } => {
                assert_eq!(reservation_id, "x-1");
                assert_eq!(hint.as_deref(), Some("ids start with res-"));
            }
            unexpected => panic!("expected ReservationNotFound, got {unexpected:?}"),
        }
        app.stop(Duration::ZERO).await.unwrap();
        service.stop().await;
    })
    .await;
}

#[tokio::test]
async fn an_unknown_reason_is_other_with_the_server_metadata_untouched() {
    guarded(async {
        let inventory = FakeInventory::new(Behaviour::Fail(|| {
            InventoryError::Other(
                AppError::not_found("gone")
                    .with_reason("MYSTERY")
                    .with_metadata("shelf", "7"),
            )
        }));
        let (service, _) = inventory_service(&service_keys(), inventory).await;
        let (app, inventory) = caller(&service.endpoint(), &[]).await;
        let error = other(
            inventory
                .reserve(&CallContext::new(), reserve(1))
                .await
                .unwrap_err(),
        );
        assert_eq!(error.code(), ErrorCode::NotFound);
        assert_eq!(error.message(), "gone");
        assert_eq!(error.reason(), Some("MYSTERY"));
        assert_eq!(error.metadata()["shelf"], "7");
        assert!(!error.metadata().contains_key("component"));
        app.stop(Duration::ZERO).await.unwrap();
        service.stop().await;
    })
    .await;
}

#[tokio::test]
async fn tokens_are_checked_fail_closed() {
    guarded(async {
        let (service, probe) = inventory_service(&service_keys(), FakeInventory::stock(5)).await;
        let endpoint = service.endpoint();

        for extra in [
            vec![("SEKVENT_LINK_OUTBOUND_INVENTORY", OTHER_TOKEN)],
            vec![
                ("SEKVENT_COMPONENT_INVENTORY_AUTH", "none"),
                ("SEKVENT_LINK_OUTBOUND_INVENTORY", ""),
            ],
        ] {
            let (app, inventory) = caller(&endpoint, &extra).await;
            let error = other(
                inventory
                    .reserve(&CallContext::new(), reserve(1))
                    .await
                    .unwrap_err(),
            );
            assert_eq!(error.code(), ErrorCode::Unauthenticated, "{extra:?}");
            assert_eq!(error.message(), sekvent_link::REJECTED_MESSAGE);
            assert_eq!(error.reason(), None);
            assert!(error.metadata().is_empty());
            app.stop(Duration::ZERO).await.unwrap();
        }
        assert!(probe.calls().is_empty());

        let error = caller_app(&caller_keys(
            &endpoint,
            &[("SEKVENT_LINK_OUTBOUND_INVENTORY", "")],
        ))
        .unwrap_err();
        assert!(
            matches!(&error, sekvent_component::BuildError::Config(ConfigError::Missing { key })
                if key == "SEKVENT_LINK_OUTBOUND_INVENTORY"),
            "{error}"
        );
        service.stop().await;
    })
    .await;
}

#[tokio::test]
async fn a_component_may_be_served_without_authentication() {
    guarded(async {
        let keys = vec![
            (
                "SEKVENT_COMPONENT_INVENTORY_SERVE".to_owned(),
                "grpc".to_owned(),
            ),
            (
                "SEKVENT_COMPONENT_INVENTORY_SERVE_AUTH".to_owned(),
                "none".to_owned(),
            ),
        ];
        let (service, probe) = inventory_service(&keys, FakeInventory::stock(5)).await;
        let (app, inventory) = caller(
            &service.endpoint(),
            &[
                ("SEKVENT_COMPONENT_INVENTORY_AUTH", "none"),
                ("SEKVENT_LINK_OUTBOUND_INVENTORY", ""),
            ],
        )
        .await;
        inventory
            .reserve(&CallContext::new().with_tenant("t-1"), reserve(1))
            .await
            .unwrap();
        let seen = probe.calls();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].caller, None);
        assert_eq!(seen[0].tenant, None);
        app.stop(Duration::ZERO).await.unwrap();
        service.stop().await;
    })
    .await;
}

#[tokio::test]
async fn the_context_is_forwarded_and_identity_needs_a_trusted_link() {
    guarded(async {
        for trusted in [true, false] {
            let keys = if trusted {
                with(service_keys(), &[("SEKVENT_LINK_TRUSTED", "shop")])
            } else {
                service_keys()
            };
            let (service, probe) = inventory_service(&keys, FakeInventory::stock(5)).await;
            let (app, inventory) = caller(&service.endpoint(), &[]).await;
            let cx = CallContext::new()
                .with_request_id("req-42")
                .with_subject("u-1")
                .with_tenant("t-1")
                .with_idempotency_key("idem-1")
                .with_traceparent(TRACEPARENT);
            inventory.reserve(&cx, reserve(1)).await.unwrap();
            let seen = probe.calls();
            assert_eq!(seen.len(), 1);
            let seen = &seen[0];
            assert_eq!(seen.request_id, "req-42");
            assert_eq!(seen.idempotency_key.as_deref(), Some("idem-1"));
            assert_eq!(seen.traceparent.as_deref(), Some(TRACEPARENT));
            assert_eq!(seen.hops, 1);
            assert_eq!(seen.caller, Some(("shop".to_owned(), trusted)));
            if trusted {
                assert_eq!(seen.subject.as_deref(), Some("u-1"));
                assert_eq!(seen.tenant.as_deref(), Some("t-1"));
            } else {
                assert_eq!(seen.subject, None);
                assert_eq!(seen.tenant, None);
            }
            app.stop(Duration::ZERO).await.unwrap();
            service.stop().await;
        }
    })
    .await;
}

#[tokio::test]
async fn deadlines_cross_the_hop_and_the_server_applies_its_own_timeout() {
    guarded(async {
        let (service, probe) = inventory_service(&service_keys(), FakeInventory::stock(5)).await;
        let (app, inventory) = caller(&service.endpoint(), &[]).await;

        // Without a caller deadline, the method's 2 s timeout applies. This
        // call also connects the lazy channel, so the next one has its whole
        // budget for the hop.
        inventory
            .reserve(&CallContext::new(), reserve(1))
            .await
            .unwrap();
        let deadline = probe.calls()[0].deadline.unwrap();
        assert!(deadline <= Instant::now() + Duration::from_secs(2));

        // The caller's 1 s deadline shortens the server's 2 s timeout.
        let start = Instant::now();
        let cx = CallContext::new().with_deadline(start + Duration::from_secs(1));
        inventory.reserve(&cx, reserve(1)).await.unwrap();
        let deadline = probe.calls()[1].deadline.unwrap();
        assert!(deadline > start);
        assert!(deadline < start + Duration::from_secs(2), "{deadline:?}");

        // A plain client sending no grpc-timeout still gets the server's.
        let reply: ReserveReply = raw_call(service.addr, RESERVE, reserve(1), Some(TOKEN), None)
            .await
            .unwrap();
        assert_eq!(reply.remaining, 2);
        let seen = &probe.calls()[2];
        assert!(seen.deadline.unwrap() <= Instant::now() + Duration::from_secs(2));
        assert_eq!(seen.hops, 0);
        app.stop(Duration::ZERO).await.unwrap();
        service.stop().await;
    })
    .await;
}

#[tokio::test]
async fn the_server_enforces_its_own_hop_limit() {
    guarded(async {
        let keys = with(service_keys(), &[("SEKVENT_COMPONENT_MAX_HOPS", "1")]);
        let (service, probe) = inventory_service(&keys, FakeInventory::stock(5)).await;
        // The caller allows 16 hops, so only the server can refuse hop 2.
        let (app, inventory) =
            caller(&service.endpoint(), &[("SEKVENT_COMPONENT_MAX_HOPS", "16")]).await;
        let error = other(
            inventory
                .reserve(&CallContext::new().with_hops(1), reserve(1))
                .await
                .unwrap_err(),
        );
        assert_eq!(error.code(), ErrorCode::FailedPrecondition);
        assert_eq!(error.reason(), Some(reasons::CALL_DEPTH_EXCEEDED));
        assert_eq!(error.metadata()["component"], "inventory");
        assert_eq!(error.metadata()["method"], "reserve");

        // The same refusal for a plain client that sends the hop count.
        let response = raw_http(
            service.addr,
            RESERVE,
            &[("authorization", BEARER), ("x-sekvent-hops", "2")],
            Frames::message(&reserve(1)),
        )
        .await;
        let (status, _) = outcome(response).await;
        assert_eq!(status, tonic::Code::FailedPrecondition);
        assert!(probe.calls().is_empty());

        inventory
            .reserve(&CallContext::new(), reserve(1))
            .await
            .unwrap();
        assert_eq!(probe.calls().len(), 1);
        app.stop(Duration::ZERO).await.unwrap();
        service.stop().await;
    })
    .await;
}

#[tokio::test]
async fn the_caller_enforces_its_own_hop_limit() {
    guarded(async {
        // The server allows the default 16 hops and would serve hop 2.
        let (service, probe) = inventory_service(&service_keys(), FakeInventory::stock(5)).await;
        let (app, inventory) =
            caller(&service.endpoint(), &[("SEKVENT_COMPONENT_MAX_HOPS", "1")]).await;
        let error = other(
            inventory
                .reserve(&CallContext::new().with_hops(1), reserve(1))
                .await
                .unwrap_err(),
        );
        assert_eq!(error.code(), ErrorCode::FailedPrecondition);
        assert_eq!(error.reason(), Some(reasons::CALL_DEPTH_EXCEEDED));
        assert!(probe.calls().is_empty());
        app.stop(Duration::ZERO).await.unwrap();
        service.stop().await;
    })
    .await;
}

#[tokio::test]
async fn a_plain_tonic_client_interoperates() {
    guarded(async {
        let (service, probe) = inventory_service(&service_keys(), FakeInventory::stock(5)).await;
        let addr = service.addr;

        let reply: ReserveReply = raw_call(
            addr,
            RESERVE,
            reserve(1),
            Some(TOKEN),
            Some(Duration::from_secs(5)),
        )
        .await
        .unwrap();
        assert_eq!(reply.remaining, 4);
        assert!(probe.calls()[0].deadline.is_some());

        let status = raw_call::<_, ReserveReply>(addr, RESERVE, reserve(99), Some(TOKEN), None)
            .await
            .unwrap_err();
        assert_eq!(status.code(), tonic::Code::FailedPrecondition);
        let info = status.get_details_error_info().unwrap();
        assert_eq!(info.reason, "OUT_OF_STOCK");
        assert_eq!(info.domain, "shop.inventory.v1");
        assert_eq!(info.metadata["sku"], "sku-1");
        assert_eq!(info.metadata["available"], "4");

        let status = raw_call::<_, ReserveReply>(addr, RESERVE, reserve(1), None, None)
            .await
            .unwrap_err();
        assert_eq!(status.code(), tonic::Code::Unauthenticated);

        // Authentication comes before the method lookup.
        let status = raw_call::<_, ReserveReply>(
            addr,
            "/shop.inventory.v1.Inventory/Stock",
            reserve(1),
            None,
            None,
        )
        .await
        .unwrap_err();
        assert_eq!(status.code(), tonic::Code::Unauthenticated);
        let error = from_status(
            &raw_call::<_, ReserveReply>(
                addr,
                "/shop.inventory.v1.Inventory/Stock",
                reserve(1),
                Some(TOKEN),
                None,
            )
            .await
            .unwrap_err(),
        );
        assert_eq!(error.code(), ErrorCode::Unimplemented);
        assert_eq!(error.reason(), Some(reasons::UNKNOWN_METHOD));
        assert_eq!(error.metadata()["component"], "inventory");

        let status = raw_call::<_, ReserveReply>(
            addr,
            "/shop.inventory.v1.Warehouse/Reserve",
            reserve(1),
            Some(TOKEN),
            None,
        )
        .await
        .unwrap_err();
        assert_eq!(status.code(), tonic::Code::Unimplemented);

        let error = from_status(
            &raw_call::<_, ReserveReply>(addr, RESERVE, Garbage { value: 7 }, Some(TOKEN), None)
                .await
                .unwrap_err(),
        );
        assert_eq!(error.code(), ErrorCode::InvalidArgument);
        assert_eq!(error.reason(), Some(reasons::MALFORMED_REQUEST));
        assert_eq!(probe.calls().len(), 2);
        service.stop().await;
    })
    .await;
}

#[tokio::test]
async fn dropping_the_caller_resets_the_stream() {
    guarded(async {
        let (entered_tx, mut entered) = mpsc::unbounded_channel();
        let (dropped_tx, mut dropped) = mpsc::unbounded_channel();
        let keys = with(
            service_keys(),
            &[("SEKVENT_COMPONENT_INVENTORY_RESERVE_TIMEOUT", "1h")],
        );
        let (service, _) = inventory_service(
            &keys,
            FakeInventory::new(Behaviour::Pending {
                entered: entered_tx,
                dropped: dropped_tx,
            }),
        )
        .await;
        let (app, inventory) = caller(
            &service.endpoint(),
            &[("SEKVENT_COMPONENT_INVENTORY_RESERVE_TIMEOUT", "1h")],
        )
        .await;
        let call =
            tokio::spawn(async move { inventory.reserve(&CallContext::new(), reserve(1)).await });
        entered.recv().await.unwrap();
        call.abort();
        assert!(call.await.unwrap_err().is_cancelled());
        // Only the reset can end the pending server-side call.
        dropped.recv().await.unwrap();
        app.stop(Duration::ZERO).await.unwrap();
        service.stop().await;
    })
    .await;
}

#[tokio::test]
async fn a_remote_only_install_calls_the_service() {
    guarded(async {
        let (service, probe) = inventory_service(&service_keys(), FakeInventory::stock(5)).await;
        let config: MapSource = caller_keys(&service.endpoint(), &[]).into_iter().collect();
        let app = build_with(&config, support::inventory::install_remote).unwrap();
        assert_eq!(app.binding("inventory"), Some(Binding::Grpc));
        app.start().await.unwrap();
        let inventory = app.handle::<InventoryHandle>().unwrap();
        let reply = inventory
            .reserve(&CallContext::new(), reserve(2))
            .await
            .unwrap();
        assert_eq!(reply.remaining, 3);
        assert_eq!(probe.calls().len(), 1);
        app.stop(Duration::ZERO).await.unwrap();
        service.stop().await;
    })
    .await;
}

#[tokio::test]
async fn health_follows_the_app() {
    guarded(async {
        let (service, _) = inventory_service(&service_keys(), FakeInventory::stock(5)).await;
        let health = service.runtime.health().clone();
        assert_eq!(health.status(SERVICE), Some(ServiceStatus::Serving));

        let channel = tonic::transport::Endpoint::from_shared(service.endpoint())
            .unwrap()
            .connect()
            .await
            .unwrap();
        let mut client = tonic_health::pb::health_client::HealthClient::new(channel);
        let reply = client
            .check(tonic_health::pb::HealthCheckRequest {
                service: SERVICE.to_owned(),
            })
            .await
            .unwrap()
            .into_inner();
        assert_eq!(
            reply.status(),
            tonic_health::pb::health_check_response::ServingStatus::Serving
        );

        let app = service.app.clone();
        service.stop().await;
        assert_eq!(health.status(SERVICE), Some(ServiceStatus::NotServing));
        assert_eq!(
            app.state("inventory"),
            Some(sekvent_component::ComponentState::Stopped)
        );
    })
    .await;
}

const BEARER: &str = "Bearer shop-link-token-0123456789abcdefghijklmn";

/// A request body: gRPC-framed data, then optional trailers or a stall that
/// never ends.
struct Frames {
    data: Option<Bytes>,
    trailers: Option<http::HeaderMap>,
    stall: bool,
}

impl Frames {
    /// One complete gRPC message.
    fn message(message: &impl prost::Message) -> Self {
        let encoded = message.encode_to_vec();
        let mut data = vec![0];
        data.extend_from_slice(&u32::try_from(encoded.len()).unwrap().to_be_bytes());
        data.extend_from_slice(&encoded);
        Self {
            data: Some(Bytes::from(data)),
            trailers: None,
            stall: false,
        }
    }

    /// The message, then these request trailers.
    fn with_trailers(mut self, trailers: &[(&'static str, &str)]) -> Self {
        let mut map = http::HeaderMap::new();
        for (name, value) in trailers {
            map.insert(*name, value.parse().unwrap());
        }
        self.trailers = Some(map);
        self
    }

    /// The prefix of a 1 MiB message and some of its bytes, then nothing,
    /// ever: a server that waits for the body never answers.
    fn stalled() -> Self {
        let mut data = vec![0];
        data.extend_from_slice(&(1_u32 << 20).to_be_bytes());
        data.extend_from_slice(&[0; 1024]);
        Self {
            data: Some(Bytes::from(data)),
            trailers: None,
            stall: true,
        }
    }
}

impl http_body::Body for Frames {
    type Data = Bytes;
    type Error = std::convert::Infallible;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Self::Error>>> {
        if let Some(data) = self.data.take() {
            return Poll::Ready(Some(Ok(Frame::data(data))));
        }
        if self.stall {
            return Poll::Pending;
        }
        Poll::Ready(
            self.trailers
                .take()
                .map(|trailers| Ok(Frame::trailers(trailers))),
        )
    }
}

/// One raw HTTP/2 gRPC request with exactly these headers and body, over a
/// plain HTTP/2 client: a tonic channel would enforce `grpc-timeout` itself
/// and race the server's answer.
async fn raw_http(
    addr: SocketAddr,
    path: &str,
    headers: &[(&'static str, &str)],
    body: Frames,
) -> http::Response<tonic::body::Body> {
    let client = hyper_util::client::legacy::Client::builder(hyper_util::rt::TokioExecutor::new())
        .http2_only(true)
        .build_http::<tonic::body::Body>();
    let mut request = http::Request::builder()
        .method(http::Method::POST)
        .uri(format!("http://{addr}{path}"))
        .header("content-type", "application/grpc")
        .header("te", "trailers");
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    let request = request.body(tonic::body::Body::new(body)).unwrap();
    client
        .request(request)
        .await
        .unwrap()
        .map(tonic::body::Body::new)
}

/// The gRPC status of a response (from its headers when trailers-only, else
/// from its trailers) and the number of data bytes it carried.
async fn outcome(response: http::Response<tonic::body::Body>) -> (tonic::Code, usize) {
    let status = |map: &http::HeaderMap| {
        map.get("grpc-status")
            .map(|value| tonic::Code::from_bytes(value.as_bytes()))
    };
    if let Some(code) = status(response.headers()) {
        return (code, 0);
    }
    let mut body = response.into_body();
    let mut bytes = 0;
    let mut code = None;
    while let Some(frame) = std::future::poll_fn(|cx| Pin::new(&mut body).poll_frame(cx)).await {
        let frame = frame.unwrap();
        if let Some(data) = frame.data_ref() {
            bytes += data.len();
        } else if let Some(trailers) = frame.trailers_ref() {
            code = status(trailers);
        }
    }
    (code.expect("a grpc-status"), bytes)
}

#[tokio::test]
async fn request_trailers_never_reach_the_context() {
    guarded(async {
        let keys = with(
            service_keys(),
            &[
                ("SEKVENT_LINK_TRUSTED", "shop"),
                ("SEKVENT_COMPONENT_MAX_HOPS", "1"),
            ],
        );
        let (service, probe) = inventory_service(&keys, FakeInventory::stock(5)).await;
        let spoofed = [
            ("x-sekvent-subject", "u-spoofed"),
            ("x-sekvent-tenant", "t-spoofed"),
            ("x-sekvent-hops", "9"),
            ("grpc-timeout", "1n"),
            ("x-request-id", "req-spoofed"),
        ];

        // Identity, hops and timeout sent only as trailers are ignored: the
        // call is served, as hop 0, without the spoofed identity.
        let response = raw_http(
            service.addr,
            RESERVE,
            &[("authorization", BEARER), ("x-request-id", "req-1")],
            Frames::message(&reserve(1)).with_trailers(&spoofed),
        )
        .await;
        let (status, bytes) = outcome(response).await;
        assert_eq!(status, tonic::Code::Ok);
        assert!(bytes > 0);
        let seen = probe.calls();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].request_id, "req-1");
        assert_eq!(seen[0].subject, None);
        assert_eq!(seen[0].tenant, None);
        assert_eq!(seen[0].hops, 0);
        assert!(seen[0].deadline.unwrap() > Instant::now());

        // A token sent only as a trailer authenticates nothing.
        let response = raw_http(
            service.addr,
            RESERVE,
            &[],
            Frames::message(&reserve(1)).with_trailers(&[("authorization", BEARER)]),
        )
        .await;
        assert_eq!(outcome(response).await.0, tonic::Code::Unauthenticated);
        assert_eq!(probe.calls().len(), 1);
        service.stop().await;
    })
    .await;
}

#[tokio::test]
async fn requests_are_refused_before_their_body_is_read() {
    guarded(async {
        let (service, probe) = inventory_service(&service_keys(), FakeInventory::stock(5)).await;
        // The body never ends, so only an answer from the headers arrives.
        for (headers, path, code) in [
            (vec![], RESERVE, tonic::Code::Unauthenticated),
            (
                vec![(
                    "authorization",
                    "Bearer other-link-token-0123456789abcdefghijklm",
                )],
                RESERVE,
                tonic::Code::Unauthenticated,
            ),
            (
                vec![("authorization", BEARER)],
                "/shop.inventory.v1.Inventory/Stock",
                tonic::Code::Unimplemented,
            ),
            (
                vec![("authorization", BEARER), ("x-sekvent-hops", "99")],
                RESERVE,
                tonic::Code::FailedPrecondition,
            ),
        ] {
            let response = raw_http(service.addr, path, &headers, Frames::stalled()).await;
            assert_eq!(outcome(response).await.0, code, "{path} {headers:?}");
        }

        // An authenticated call whose body stalls ends at its deadline.
        let start = Instant::now();
        let response = raw_http(
            service.addr,
            RESERVE,
            &[("authorization", BEARER), ("grpc-timeout", "100m")],
            Frames::stalled(),
        )
        .await;
        let (status, _) = outcome(response).await;
        assert_eq!(status, tonic::Code::DeadlineExceeded);
        assert!(start.elapsed() >= Duration::from_millis(100));
        assert!(probe.calls().is_empty());
        service.stop().await;
    })
    .await;
}

#[tokio::test]
async fn only_the_exact_method_path_is_served() {
    guarded(async {
        let (service, probe) = inventory_service(&service_keys(), FakeInventory::stock(5)).await;
        for path in [
            "/shop.inventory.v1.Inventory/a/Reserve",
            "/shop.inventory.v1.Inventory/Reserve/",
            "/shop.inventory.v1.Inventory/x/y/Reserve",
        ] {
            let error = from_status(
                &raw_call::<_, ReserveReply>(service.addr, path, reserve(1), Some(TOKEN), None)
                    .await
                    .unwrap_err(),
            );
            assert_eq!(error.code(), ErrorCode::Unimplemented, "{path}");
            assert_eq!(error.reason(), Some(reasons::UNKNOWN_METHOD), "{path}");
        }
        assert!(probe.calls().is_empty());
        service.stop().await;
    })
    .await;
}

#[tokio::test]
async fn a_panicking_method_is_internal_and_not_retried() {
    guarded(async {
        let (service, probe) =
            inventory_service(&service_keys(), FakeInventory::new(Behaviour::Panic)).await;
        let (app, inventory) = caller(&service.endpoint(), &[]).await;
        let error = other(
            inventory
                .reserve(&CallContext::new(), reserve(1))
                .await
                .unwrap_err(),
        );
        assert_eq!(error.code(), ErrorCode::Internal);
        assert_eq!(error.reason(), Some(reasons::HANDLER_PANICKED));
        assert_eq!(
            error.message(),
            "component inventory method reserve panicked"
        );
        assert_eq!(error.metadata()["component"], "inventory");
        assert_eq!(error.metadata()["method"], "reserve");
        assert_eq!(probe.calls().len(), 1);

        // The server keeps serving.
        let released = inventory
            .release(
                &CallContext::new(),
                ReleaseRequest {
                    reservation_id: "res-o1".into(),
                },
            )
            .await
            .unwrap();
        assert!(released.released);
        app.stop(Duration::ZERO).await.unwrap();
        service.stop().await;
    })
    .await;
}

#[tokio::test]
async fn a_downstream_failure_is_internal_to_the_caller() {
    guarded(async {
        let inventory = FakeInventory::new(Behaviour::Fail(|| {
            InventoryError::Other(
                AppError::unavailable("pricing is down")
                    .with_reason(reasons::CIRCUIT_OPEN)
                    .with_metadata("downstream", "pricing"),
            )
        }));
        let (service, probe) = inventory_service(&service_keys(), inventory).await;
        let (app, inventory) = caller(&service.endpoint(), &[]).await;
        let error = other(
            inventory
                .reserve(&CallContext::new(), reserve(1))
                .await
                .unwrap_err(),
        );
        assert_eq!(error.code(), ErrorCode::Internal);
        assert_eq!(error.reason(), Some(reasons::DOWNSTREAM_FAILURE));
        assert_eq!(error.metadata()["downstream"], "pricing");
        assert_eq!(error.metadata()["downstream_code"], "UNAVAILABLE");
        assert_eq!(error.metadata()["downstream_reason"], reasons::CIRCUIT_OPEN);
        assert_eq!(error.metadata()["component"], "inventory");
        assert_eq!(error.metadata()["method"], "reserve");
        // Not retried: the inventory saw one call.
        assert_eq!(probe.calls().len(), 1);
        app.stop(Duration::ZERO).await.unwrap();
        service.stop().await;
    })
    .await;
}
