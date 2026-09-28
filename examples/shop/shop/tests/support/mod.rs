//! Profiles, App helpers and fake inventories shared by the shop's tests.

#![allow(
    dead_code,
    reason = "every test binary includes this module and uses a different part of it"
)]

pub(crate) mod fakes;

use sekvent::component::{App, AppBuilder, Binding, BuildError};
use sekvent::config::MapSource;
use shop::ShopOptions;

/// The key that selects the binding of every component.
pub(crate) const BINDING_KEY: &str = "SEKVENT_COMPONENT_BINDING";
/// Timeout override of `inventory.reserve`.
pub(crate) const RESERVE_TIMEOUT_KEY: &str = "SEKVENT_COMPONENT_INVENTORY_RESERVE_TIMEOUT";
/// Bulkhead override of `inventory.reserve`.
pub(crate) const RESERVE_BULKHEAD_KEY: &str =
    "SEKVENT_COMPONENT_INVENTORY_RESERVE_BULKHEAD_MAX_CONCURRENT";

/// The SKU the tests order.
pub(crate) const SKU: &str = "sku-apple";
/// Units of [`SKU`] in stock at the start of every test.
pub(crate) const STOCK: u32 = 10;
/// A customer who may be notified.
pub(crate) const CUSTOMER: &str = "cust-1";
/// A customer who opted out of notifications.
pub(crate) const BLOCKED_CUSTOMER: &str = "cust-blocked";

/// How the components of one test are bound.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Profile {
    /// Every component in-process with direct calls.
    MonolithLocal,
    /// Every component in-process behind a serialization boundary.
    MonolithSerialized,
}

impl Profile {
    /// `monolith-local`: no keys (`local` is the default);
    /// `monolith-serialized`: `SEKVENT_COMPONENT_BINDING=local-serialized`.
    pub(crate) fn source(self) -> MapSource {
        match self {
            Self::MonolithLocal => MapSource::new(),
            Self::MonolithSerialized => MapSource::new().with(BINDING_KEY, "local-serialized"),
        }
    }

    /// The binding every component gets under this profile.
    pub(crate) fn binding(self) -> Binding {
        match self {
            Self::MonolithLocal => Binding::Local,
            Self::MonolithSerialized => Binding::LocalSerialized,
        }
    }
}

/// The stock and blocklist every test starts from.
pub(crate) fn options() -> ShopOptions {
    ShopOptions {
        stock: vec![(SKU.to_owned(), STOCK)],
        blocked_customers: vec![BLOCKED_CUSTOMER.to_owned()],
    }
}

/// Build with `install` and assert every component's binding equals
/// `profile.binding()`, without starting.
pub(crate) fn built(
    source: &MapSource,
    profile: Profile,
    install: impl FnOnce(&mut AppBuilder<'_>) -> Result<(), BuildError>,
) -> App {
    let mut builder = App::builder(source);
    install(&mut builder).expect("the components install");
    let app = builder.build().expect("the App builds");
    for component in app.components() {
        assert_eq!(
            app.binding(component),
            Some(profile.binding()),
            "binding of {component} under {profile:?}"
        );
    }
    app
}

/// Build with `install`, assert every component's binding equals
/// `profile.binding()`, start.
pub(crate) async fn started(
    source: &MapSource,
    profile: Profile,
    install: impl FnOnce(&mut AppBuilder<'_>) -> Result<(), BuildError>,
) -> App {
    let app = built(source, profile, install);
    app.start().await.expect("the App starts");
    app
}

/// The whole shop with [`options`], started.
pub(crate) async fn started_shop(profile: Profile) -> App {
    started(&profile.source(), profile, |app| {
        shop::install(app, options())
    })
    .await
}

/// The shop with `inventory` in place of the real inventory, started.
pub(crate) async fn shop_with_inventory<T>(
    source: &MapSource,
    profile: Profile,
    inventory: T,
) -> App
where
    T: inventory_api::Inventory,
{
    started(source, profile, move |app| {
        inventory_api::InventoryHandle::install(app, move |_deps| Ok(inventory))?;
        shop::install_notifications(app, options().blocked_customers)?;
        shop::install_orders(app)
    })
    .await
}
