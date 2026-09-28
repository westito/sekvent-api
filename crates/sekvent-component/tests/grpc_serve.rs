//! The inventory served over gRPC on loopback and called through the `grpc`
//! binding or a plain tonic client: results, typed errors, authentication,
//! the forwarded context, hop limits, malformed requests and health.

mod support;

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use http::uri::PathAndQuery;
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

        // The caller's 300 ms deadline shortens the server's 2 s timeout.
        let start = Instant::now();
        let cx = CallContext::new().with_deadline(start + Duration::from_millis(300));
        inventory.reserve(&cx, reserve(1)).await.unwrap();
        let deadline = probe.calls()[0].deadline.unwrap();
        assert!(deadline > start);
        assert!(
            deadline < start + Duration::from_millis(1500),
            "{deadline:?}"
        );

        // Without a caller deadline, the method's 2 s timeout applies.
        inventory
            .reserve(&CallContext::new(), reserve(1))
            .await
            .unwrap();
        let deadline = probe.calls()[1].deadline.unwrap();
        assert!(deadline <= Instant::now() + Duration::from_secs(2));

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
async fn calls_deeper_than_the_hop_limit_are_rejected() {
    guarded(async {
        let keys = with(service_keys(), &[("SEKVENT_COMPONENT_MAX_HOPS", "1")]);
        let (service, probe) = inventory_service(&keys, FakeInventory::stock(5)).await;
        let endpoint = service.endpoint();
        for caller_limit in ["16", "1"] {
            let (app, inventory) =
                caller(&endpoint, &[("SEKVENT_COMPONENT_MAX_HOPS", caller_limit)]).await;
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
            app.stop(Duration::ZERO).await.unwrap();
        }
        assert!(probe.calls().is_empty());
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

        // The method lookup comes before authentication.
        let error = from_status(
            &raw_call::<_, ReserveReply>(
                addr,
                "/shop.inventory.v1.Inventory/Stock",
                reserve(1),
                None,
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
