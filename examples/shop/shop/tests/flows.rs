//! Business flows of the shop, each run under the `monolith-local` and
//! `monolith-serialized` profiles: the same calls must give the same
//! results whichever local binding carries them.

mod support;

use inventory_api::{InventoryHandle, StockRequest};
use notifications_api::{Notification, NotificationsHandle, SentRequest};
use orders_api::{GetOrderRequest, OrderStatus, OrdersError, OrdersHandle, PlaceOrderRequest};
use rstest::rstest;
use sekvent::prelude::*;
use support::{BLOCKED_CUSTOMER, CUSTOMER, Profile, SKU, STOCK, started_shop};

fn place(customer_id: &str, sku: &str, quantity: u32) -> PlaceOrderRequest {
    PlaceOrderRequest {
        customer_id: customer_id.to_owned(),
        sku: sku.to_owned(),
        quantity,
    }
}

async fn available(app: &App) -> u32 {
    let inventory = app.handle::<InventoryHandle>().unwrap();
    let request = StockRequest {
        sku: SKU.to_owned(),
    };
    inventory
        .stock(&CallContext::new(), request)
        .await
        .expect("the test SKU is stocked")
        .available
}

async fn sent_to(app: &App, customer_id: &str) -> Vec<Notification> {
    let notifications = app.handle::<NotificationsHandle>().unwrap();
    let request = SentRequest {
        customer_id: customer_id.to_owned(),
    };
    notifications
        .sent(&CallContext::new(), request)
        .await
        .expect("sent never fails")
        .notifications
}

#[rstest]
#[case::monolith_local(Profile::MonolithLocal)]
#[case::monolith_serialized(Profile::MonolithSerialized)]
#[tokio::test]
async fn an_order_reserves_stock_notifies_and_is_readable(#[case] profile: Profile) {
    let app = started_shop(profile).await;
    let orders = app.handle::<OrdersHandle>().unwrap();
    let cx = CallContext::new();

    let placed = orders
        .place_order(&cx, place(CUSTOMER, SKU, 3))
        .await
        .unwrap();
    assert_eq!(placed.order_id, "ord-1");
    assert_eq!(placed.reservation_id, "res-1");
    assert_eq!(available(&app).await, STOCK - 3);

    let sent = sent_to(&app, CUSTOMER).await;
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].notification_id, "ntf-1");
    assert_eq!(sent[0].order_id, "ord-1");
    assert_eq!(sent[0].template, orders::ORDER_PLACED_TEMPLATE);

    let request = GetOrderRequest {
        order_id: placed.order_id,
    };
    let order = orders.get_order(&cx, request).await.unwrap();
    assert_eq!(order.customer_id, CUSTOMER);
    assert_eq!(order.sku, SKU);
    assert_eq!(order.quantity, 3);
    assert_eq!(order.status(), OrderStatus::Placed);
}

#[rstest]
#[case::monolith_local(Profile::MonolithLocal)]
#[case::monolith_serialized(Profile::MonolithSerialized)]
#[tokio::test]
async fn ordering_more_than_the_stock_is_out_of_stock(#[case] profile: Profile) {
    let app = started_shop(profile).await;
    let orders = app.handle::<OrdersHandle>().unwrap();

    let error = orders
        .place_order(&CallContext::new(), place(CUSTOMER, SKU, STOCK + 1))
        .await
        .unwrap_err();
    assert!(
        matches!(&error, OrdersError::OutOfStock { sku, available } if sku == SKU && *available == STOCK),
        "{error:?}"
    );
    assert_eq!(available(&app).await, STOCK);
    assert!(sent_to(&app, CUSTOMER).await.is_empty());
}

#[rstest]
#[case::monolith_local(Profile::MonolithLocal)]
#[case::monolith_serialized(Profile::MonolithSerialized)]
#[tokio::test]
async fn an_unknown_sku_is_reported(#[case] profile: Profile) {
    let app = started_shop(profile).await;
    let orders = app.handle::<OrdersHandle>().unwrap();

    let error = orders
        .place_order(&CallContext::new(), place(CUSTOMER, "sku-none", 1))
        .await
        .unwrap_err();
    assert!(
        matches!(&error, OrdersError::UnknownSku { sku } if sku == "sku-none"),
        "{error:?}"
    );
}

#[rstest]
#[case::monolith_local(Profile::MonolithLocal)]
#[case::monolith_serialized(Profile::MonolithSerialized)]
#[tokio::test]
async fn a_zero_quantity_is_invalid(#[case] profile: Profile) {
    let app = started_shop(profile).await;
    let orders = app.handle::<OrdersHandle>().unwrap();

    let error = orders
        .place_order(&CallContext::new(), place(CUSTOMER, SKU, 0))
        .await
        .unwrap_err();
    assert!(
        matches!(error, OrdersError::InvalidQuantity { quantity: 0 }),
        "{error:?}"
    );
    assert_eq!(available(&app).await, STOCK);
}

#[rstest]
#[case::monolith_local(Profile::MonolithLocal)]
#[case::monolith_serialized(Profile::MonolithSerialized)]
#[tokio::test]
async fn a_blocked_customer_is_refused_and_the_reservation_released(#[case] profile: Profile) {
    let app = started_shop(profile).await;
    let orders = app.handle::<OrdersHandle>().unwrap();

    let error = orders
        .place_order(&CallContext::new(), place(BLOCKED_CUSTOMER, SKU, 4))
        .await
        .unwrap_err();
    assert!(
        matches!(&error, OrdersError::CustomerBlocked { customer_id } if customer_id == BLOCKED_CUSTOMER),
        "{error:?}"
    );
    assert_eq!(available(&app).await, STOCK);
    assert!(sent_to(&app, BLOCKED_CUSTOMER).await.is_empty());
}

#[rstest]
#[case::monolith_local(Profile::MonolithLocal)]
#[case::monolith_serialized(Profile::MonolithSerialized)]
#[tokio::test]
async fn an_unknown_order_is_not_found(#[case] profile: Profile) {
    let app = started_shop(profile).await;
    let orders = app.handle::<OrdersHandle>().unwrap();

    let request = GetOrderRequest {
        order_id: "ord-404".to_owned(),
    };
    let error = orders
        .get_order(&CallContext::new(), request)
        .await
        .unwrap_err();
    assert!(
        matches!(&error, OrdersError::OrderNotFound { order_id } if order_id == "ord-404"),
        "{error:?}"
    );
}

#[rstest]
#[case::monolith_local(Profile::MonolithLocal)]
#[case::monolith_serialized(Profile::MonolithSerialized)]
#[tokio::test]
async fn the_callers_tenant_reaches_the_notification(#[case] profile: Profile) {
    let app = started_shop(profile).await;
    let orders = app.handle::<OrdersHandle>().unwrap();

    let cx = CallContext::new().with_tenant("tenant-a");
    orders
        .place_order(&cx, place(CUSTOMER, SKU, 1))
        .await
        .unwrap();

    let sent = sent_to(&app, CUSTOMER).await;
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].tenant, "tenant-a");
}
