//! The shop's App as one unit of the sekvent runtime, under the
//! `monolith-local` and `monolith-serialized` profiles.

mod support;

use orders_api::{OrdersHandle, PlaceOrderRequest};
use rstest::rstest;
use sekvent::component::ComponentState;
use sekvent::prelude::*;
use support::{CUSTOMER, Profile, SKU, built, options};

#[rstest]
#[case::monolith_local(Profile::MonolithLocal)]
#[case::monolith_serialized(Profile::MonolithSerialized)]
#[tokio::test]
async fn the_runtime_starts_and_stops_every_component(#[case] profile: Profile) {
    let app = built(&profile.source(), profile, |app| {
        shop::install(app, options())
    });
    let runtime = app
        .register(Runtime::builder().without_signals())
        .build()
        .expect("the runtime builds");
    let handle = runtime.start().await.expect("the runtime starts");
    for component in app.components() {
        assert_eq!(
            app.state(component),
            Some(ComponentState::Serving),
            "{component}"
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
}
