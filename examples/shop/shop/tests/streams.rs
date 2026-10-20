//! Inventory's server-streaming `WatchStock` under the `monolith-local`,
//! `monolith-serialized` and `split-grpc` profiles: the same items, the same
//! open errors and the same ends whichever binding carries the stream.
//!
//! Each test installs a real [`InventoryService`] and keeps a clone of it,
//! which shares its stock, to observe the open subscriptions. Timed
//! scenarios run the monolith profiles on tokio's paused clock and the
//! `split_grpc` twin on the real clock inside a hang guard, asserting lower
//! time bounds only.

mod support;

use std::time::Duration;

use inventory::InventoryService;
use inventory_api::{InventoryError, InventoryHandle, StockReply, StockRequest};
use orders_api::{OrdersHandle, PlaceOrderRequest};
use rstest::rstest;
use sekvent::component::reasons;
use sekvent::prelude::*;
use sekvent::runtime::UnitExit;
use sekvent_testing::await_until;
use support::{CUSTOMER, Profile, SKU, STOCK, Shop, assert_no_wait, fake, guarded, options, start};
use tokio::time::Instant;

/// Lifetime override of `inventory.watch_stock`.
const LIFETIME_KEY: &str = "SEKVENT_COMPONENT_INVENTORY_WATCH_STOCK_STREAM_MAX_LIFETIME";

/// What `watch_stock` answers with.
type Levels = CallStream<StockReply, InventoryError>;

/// A started shop whose inventory is a real one the test keeps a clone of.
async fn shop_watching(profile: Profile, extra: &[(&str, &str)]) -> (Shop, InventoryService) {
    let inventory = InventoryService::new(options().stock);
    let shop = start(profile, extra, fake(inventory.clone())).await;
    (shop, inventory)
}

fn inventory(shop: &Shop) -> InventoryHandle {
    shop.app
        .handle::<InventoryHandle>()
        .expect("inventory is installed")
}

fn watch(sku: &str) -> StockRequest {
    StockRequest {
        sku: sku.to_owned(),
    }
}

/// Open a stream of the test SKU's levels.
async fn open(shop: &Shop) -> Levels {
    inventory(shop)
        .watch_stock(&CallContext::new(), watch(SKU))
        .await
        .expect("the stream opens")
}

/// The next level of `levels`, which must be an item.
async fn next_level(levels: &mut Levels) -> u32 {
    let level = levels
        .next()
        .await
        .expect("the stream is still open")
        .expect("the item is a level");
    assert_eq!(level.sku, SKU);
    level.available
}

/// The error that ends `levels`; the stream yields nothing after it.
async fn end_of(levels: &mut Levels) -> AppError {
    let error = match levels.next().await {
        Some(Err(InventoryError::Other(error))) => error,
        other => panic!("expected the stream to end with InventoryError::Other, got {other:?}"),
    };
    assert!(
        levels.next().await.is_none(),
        "the stream ended at its error"
    );
    error
}

#[rstest]
#[case::monolith_local(Profile::MonolithLocal)]
#[case::monolith_serialized(Profile::MonolithSerialized)]
#[case::split_grpc(Profile::SplitGrpc)]
#[tokio::test]
async fn the_first_level_is_the_stock_and_an_order_sends_the_next(#[case] profile: Profile) {
    guarded(async {
        let (shop, _inventory) = shop_watching(profile, &[]).await;
        let mut levels = open(&shop).await;
        assert_eq!(next_level(&mut levels).await, STOCK);

        let orders = shop.app.handle::<OrdersHandle>().unwrap();
        let request = PlaceOrderRequest {
            customer_id: CUSTOMER.to_owned(),
            sku: SKU.to_owned(),
            quantity: 3,
        };
        orders
            .place_order(&CallContext::new(), request)
            .await
            .expect("the order is placed");
        assert_eq!(next_level(&mut levels).await, STOCK - 3);
    })
    .await;
}

#[rstest]
#[case::monolith_local(Profile::MonolithLocal)]
#[case::monolith_serialized(Profile::MonolithSerialized)]
#[case::split_grpc(Profile::SplitGrpc)]
#[tokio::test]
async fn an_unknown_sku_fails_the_open(#[case] profile: Profile) {
    guarded(async {
        let (shop, inventory_service) = shop_watching(profile, &[]).await;

        let opened = inventory(&shop)
            .watch_stock(&CallContext::new(), watch("sku-none"))
            .await;
        let Err(error) = opened else {
            panic!("a stream of an unknown SKU opened");
        };
        assert!(
            matches!(&error, InventoryError::UnknownSku { sku } if sku == "sku-none"),
            "{error:?}"
        );
        assert_eq!(inventory_service.watchers(SKU), 0);
    })
    .await;
}

/// Under `local` the subscription goes with the dropped stream; under
/// `local-serialized` the serving task is aborted, and under `split-grpc`
/// the service drops the producer once the HTTP/2 stream is reset, so those
/// are awaited.
#[rstest]
#[case::monolith_local(Profile::MonolithLocal)]
#[case::monolith_serialized(Profile::MonolithSerialized)]
#[case::split_grpc(Profile::SplitGrpc)]
#[tokio::test]
async fn dropping_the_stream_releases_the_subscription(#[case] profile: Profile) {
    guarded(async {
        let (shop, inventory_service) = shop_watching(profile, &[]).await;
        let mut levels = open(&shop).await;
        assert_eq!(next_level(&mut levels).await, STOCK);
        assert_eq!(inventory_service.watchers(SKU), 1);

        drop(levels);
        if profile == Profile::MonolithLocal {
            assert_eq!(inventory_service.watchers(SKU), 0);
        }
        await_until!(inventory_service.watchers(SKU) == 0);
    })
    .await;
}

/// The App that hosts inventory stops with a stream open: the stream ends
/// with `COMPONENT_DRAINING` at once, and the stop does not wait out its
/// grace.
async fn stopping_ends_a_stream(profile: Profile) {
    let (shop, inventory_service) = shop_watching(profile, &[]).await;
    let mut levels = open(&shop).await;
    assert_eq!(next_level(&mut levels).await, STOCK);

    let started = Instant::now();
    let (stopped, error) =
        tokio::join!(shop.app.stop(Duration::from_secs(30)), end_of(&mut levels));
    stopped.expect("the App stops");
    assert_no_wait(profile, started.elapsed());
    assert_eq!(error.code(), ErrorCode::Unavailable, "{error:?}");
    assert_eq!(error.reason(), Some(reasons::DRAINING), "{error:?}");
    await_until!(inventory_service.watchers(SKU) == 0);
}

#[rstest]
#[case::monolith_local(Profile::MonolithLocal)]
#[case::monolith_serialized(Profile::MonolithSerialized)]
#[tokio::test(start_paused = true)]
async fn stopping_the_app_ends_an_open_stream(#[case] profile: Profile) {
    stopping_ends_a_stream(profile).await;
}

/// The inventory service shuts down with a stream open: `App::register`
/// closes the stream when shutdown begins, so the listener drains without
/// waiting on it and the service's runtime stops cleanly.
#[tokio::test]
async fn shutting_the_service_down_ends_an_open_stream_split_grpc() {
    guarded(async {
        let (mut shop, inventory_service) = shop_watching(Profile::SplitGrpc, &[]).await;
        let mut levels = open(&shop).await;
        assert_eq!(next_level(&mut levels).await, STOCK);
        let service = shop
            .take_service()
            .expect("the split profile has a service");

        service.runtime.shutdown();
        let error = end_of(&mut levels).await;
        assert_eq!(error.code(), ErrorCode::Unavailable, "{error:?}");
        assert_eq!(error.reason(), Some(reasons::DRAINING), "{error:?}");

        let report = service
            .runtime
            .wait()
            .await
            .expect("the service stops cleanly");
        let ingress = report
            .unit(inventory_svc::GRPC_UNIT)
            .expect("the service runs its listener as a unit");
        assert_eq!(
            ingress.exit,
            UnitExit::Completed,
            "the listener drained without being aborted"
        );
        assert_eq!(inventory_service.watchers(SKU), 0);
    })
    .await;
}

/// A stream ends at its lifetime, with `STREAM_EXPIRED` between 90 % and
/// 100 % of the configured value, which its metadata carries.
/// Under `split-grpc` the key reaches both processes; the service enforces
/// it and the caller accepts and ignores it.
async fn the_lifetime_ends_a_stream(profile: Profile) {
    let (lifetime, configured) = if profile.paused() {
        ("10s", Duration::from_secs(10))
    } else {
        ("300ms", Duration::from_millis(300))
    };
    let (shop, _inventory) = shop_watching(profile, &[(LIFETIME_KEY, lifetime)]).await;

    let started = Instant::now();
    let mut levels = open(&shop).await;
    assert_eq!(next_level(&mut levels).await, STOCK);
    let error = end_of(&mut levels).await;
    let elapsed = started.elapsed();

    assert_eq!(error.code(), ErrorCode::Unavailable, "{error:?}");
    assert_eq!(error.reason(), Some(reasons::STREAM_EXPIRED), "{error:?}");
    let configured_ms = configured.as_millis().to_string();
    assert_eq!(
        error.metadata().get("max_lifetime_ms").map(String::as_str),
        Some(configured_ms.as_str()),
        "{error:?}"
    );
    assert!(
        elapsed >= configured * 9 / 10,
        "{elapsed:?} under {profile:?}: expected at least 90 % of {configured:?}"
    );
    if profile.paused() {
        assert!(
            elapsed <= configured,
            "{elapsed:?} under {profile:?}: expected at most {configured:?}"
        );
    }
}

#[rstest]
#[case::monolith_local(Profile::MonolithLocal)]
#[case::monolith_serialized(Profile::MonolithSerialized)]
#[tokio::test(start_paused = true)]
async fn the_lifetime_ends_an_open_stream(#[case] profile: Profile) {
    the_lifetime_ends_a_stream(profile).await;
}

#[tokio::test]
async fn the_lifetime_ends_an_open_stream_split_grpc() {
    guarded(the_lifetime_ends_a_stream(Profile::SplitGrpc)).await;
}
