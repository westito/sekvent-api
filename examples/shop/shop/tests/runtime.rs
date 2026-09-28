//! The shop's App under the sekvent runtime, wired as the binary wires it
//! (`shop::bind` and `shop::runtime`), under the `monolith-local`,
//! `monolith-serialized` and `split-grpc` profiles.

mod support;

use std::net::SocketAddr;
use std::time::Duration;

use orders_api::{OrdersHandle, PlaceOrderRequest};
use rstest::rstest;
use sekvent::component::ComponentState;
use sekvent::prelude::*;
use sekvent::runtime::ServiceStatus;
use support::{CUSTOMER, INVENTORY_SERVICE, Profile, SKU, build, guarded, real_inventory};

#[rstest]
#[case::monolith_local(Profile::MonolithLocal)]
#[case::monolith_serialized(Profile::MonolithSerialized)]
#[case::split_grpc(Profile::SplitGrpc)]
#[tokio::test]
async fn the_runtime_starts_and_stops_every_component(#[case] profile: Profile) {
    guarded(async {
        let shop = build(profile, &[], real_inventory).await;
        let app = &shop.app;
        let loopback: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let server = shop::bind(app, loopback)
            .await
            .expect("the shop binds a loopback port");
        let runtime = Runtime::builder()
            .without_signals()
            .shutdown_delay(Duration::ZERO);
        let handle = shop::runtime(app, server, runtime)
            .build()
            .expect("the runtime builds")
            .start()
            .await
            .expect("the runtime starts");
        for component in app.components() {
            assert_eq!(
                app.state(component),
                Some(ComponentState::Serving),
                "{component}"
            );
        }
        if let Some(service) = shop.service() {
            assert_eq!(
                service.runtime.health().status(INVENTORY_SERVICE),
                Some(ServiceStatus::Serving)
            );
        }

        let orders = app.handle::<OrdersHandle>().unwrap();
        let request = PlaceOrderRequest {
            customer_id: CUSTOMER.to_owned(),
            sku: SKU.to_owned(),
            quantity: 1,
        };
        let placed = orders
            .place_order(&CallContext::new(), request)
            .await
            .expect("an order is placed while the runtime runs");
        assert_eq!(placed.order_id, "ord-1");

        handle.shutdown();
        handle.wait().await.expect("the runtime stops cleanly");
        for component in app.components() {
            assert_eq!(
                app.state(component),
                Some(ComponentState::Stopped),
                "{component}"
            );
        }
    })
    .await;
}
