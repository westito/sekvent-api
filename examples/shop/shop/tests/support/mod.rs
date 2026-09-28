//! Profiles, topologies and fake inventories shared by the shop's tests.
//!
//! A test names a [`Profile`] and gets a started [`Shop`]. Under the
//! monolith profiles every component runs in the test's App; under
//! [`Profile::SplitGrpc`] inventory runs in a second App, served over gRPC
//! on an ephemeral loopback port by `inventory-svc`, and the test's App binds
//! it `grpc`. Both Apps live in the test process, so fakes installed as
//! inventory still report to the test through channels.

#![allow(
    dead_code,
    reason = "every test binary includes this module and uses a different part of it"
)]

pub(crate) mod fakes;

use std::net::SocketAddr;
use std::time::Duration;

use inventory_api::{Inventory, InventoryHandle};
use sekvent::component::{App, AppBuilder, Binding, BuildError};
use sekvent::config::{ConfigError, MapSource};
use sekvent::runtime::{Runtime, RuntimeHandle};
use shop::ShopOptions;

/// The key that selects the binding of every component.
pub(crate) const BINDING_KEY: &str = "SEKVENT_COMPONENT_BINDING";
/// Timeout override of `inventory.reserve`.
pub(crate) const RESERVE_TIMEOUT_KEY: &str = "SEKVENT_COMPONENT_INVENTORY_RESERVE_TIMEOUT";
/// Bulkhead override of `inventory.reserve`.
pub(crate) const RESERVE_BULKHEAD_KEY: &str =
    "SEKVENT_COMPONENT_INVENTORY_RESERVE_BULKHEAD_MAX_CONCURRENT";
/// The deepest chain of component calls a process accepts.
pub(crate) const MAX_HOPS_KEY: &str = "SEKVENT_COMPONENT_MAX_HOPS";
/// Whether the caller presents its link token to inventory.
pub(crate) const INVENTORY_AUTH_KEY: &str = "SEKVENT_COMPONENT_INVENTORY_AUTH";
/// The token the shop presents to inventory under [`Profile::SplitGrpc`].
pub(crate) const OUTBOUND_KEY: &str = "SEKVENT_LINK_OUTBOUND_INVENTORY";

/// The link token of the split topology: canonical, 40 characters.
pub(crate) const TOKEN: &str = "0123456789abcdefghijklmnopqrstuvwxyzABCD";
/// A second well-formed token, which the service does not expect.
pub(crate) const OTHER_TOKEN: &str = "ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789abcd";
/// The link name the service knows the shop by.
pub(crate) const SHOP_LINK: &str = "shop";
/// Full gRPC service name of inventory.
pub(crate) const INVENTORY_SERVICE: &str = "shop.inventory.v1.Inventory";

/// Every component the shop installs, in install order.
pub(crate) const COMPONENTS: [&str; 3] = ["inventory", "notifications", "orders"];

/// The SKU the tests order.
pub(crate) const SKU: &str = "sku-apple";
/// Units of [`SKU`] in stock at the start of every test.
pub(crate) const STOCK: u32 = 10;
/// A customer who may be notified.
pub(crate) const CUSTOMER: &str = "cust-1";
/// A customer who opted out of notifications.
pub(crate) const BLOCKED_CUSTOMER: &str = "cust-blocked";

/// Longest a real-clock test may run before it counts as hung.
pub(crate) const HANG_GUARD: Duration = Duration::from_secs(30);

/// How the components of one test are bound.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Profile {
    /// Every component in-process with direct calls.
    MonolithLocal,
    /// Every component in-process behind a serialization boundary.
    MonolithSerialized,
    /// Inventory in its own App behind gRPC on loopback; the rest local.
    SplitGrpc,
}

impl Profile {
    /// The binding `component` gets in the test's App under this profile.
    pub(crate) fn binding_of(self, component: &str) -> Binding {
        match self {
            Self::MonolithSerialized => Binding::LocalSerialized,
            Self::SplitGrpc if component == "inventory" => Binding::Grpc,
            Self::MonolithLocal | Self::SplitGrpc => Binding::Local,
        }
    }

    /// Whether the profile's timed tests run on tokio's paused clock. A
    /// socket needs the real clock: auto-advance would fire timers while
    /// loopback I/O is still pending.
    pub(crate) fn paused(self) -> bool {
        !matches!(self, Self::SplitGrpc)
    }
}

/// The link between the shop and the inventory service under
/// [`Profile::SplitGrpc`]. The default is the working configuration; the
/// remote fault tests change one part of it.
#[derive(Debug, Clone, Copy)]
pub(crate) struct LinkSetup {
    /// The token the service accepts from the shop.
    pub(crate) inbound: &'static str,
    /// The token the shop presents; `None` leaves [`OUTBOUND_KEY`] unset.
    pub(crate) outbound: Option<&'static str>,
    /// Whether the service trusts the shop with subject and tenant.
    pub(crate) trusted: bool,
}

impl Default for LinkSetup {
    fn default() -> Self {
        Self {
            inbound: TOKEN,
            outbound: Some(TOKEN),
            trusted: true,
        }
    }
}

/// The inventory service of a split topology: its App, bound `local`, run
/// by a runtime with the App's `components` unit and the gRPC listener.
pub(crate) struct InventoryService {
    /// The service's App.
    pub(crate) app: App,
    /// The service's runtime.
    pub(crate) runtime: RuntimeHandle,
    /// Where the service listens.
    pub(crate) addr: SocketAddr,
}

/// A topology: the caller-side App and, under [`Profile::SplitGrpc`], the
/// inventory service.
pub(crate) struct Shop {
    /// The App the test calls through: orders and notifications run here,
    /// and inventory too unless the profile splits it out.
    pub(crate) app: App,
    service: Option<InventoryService>,
}

impl Shop {
    /// The App inventory runs in.
    pub(crate) fn inventory_app(&self) -> &App {
        self.service
            .as_ref()
            .map_or(&self.app, |service| &service.app)
    }

    /// The inventory service, under [`Profile::SplitGrpc`].
    pub(crate) fn service(&self) -> Option<&InventoryService> {
        self.service.as_ref()
    }

    /// Take the inventory service out, to shut its runtime down and wait
    /// for it.
    pub(crate) fn take_service(&mut self) -> Option<InventoryService> {
        self.service.take()
    }
}

/// The stock and blocklist every test starts from.
pub(crate) fn options() -> ShopOptions {
    ShopOptions {
        stock: vec![(SKU.to_owned(), STOCK)],
        blocked_customers: vec![BLOCKED_CUSTOMER.to_owned()],
    }
}

fn source_of(extra: &[(&str, &str)]) -> MapSource {
    extra.iter().copied().collect()
}

/// The inventory service's configuration: `extra`, then inventory exposed
/// over gRPC and the shop's inbound link.
pub(crate) fn service_source(extra: &[(&str, &str)], link: LinkSetup) -> MapSource {
    let mut source = source_of(extra);
    source.set("SEKVENT_COMPONENT_INVENTORY_SERVE", "grpc");
    source.set("SEKVENT_LINK_INBOUND_SHOP", link.inbound);
    if link.trusted {
        source.set("SEKVENT_LINK_TRUSTED", SHOP_LINK);
    }
    source
}

/// The caller's configuration: `extra`, then the profile's keys. Under
/// [`Profile::SplitGrpc`] those are the `grpc` binding of inventory at
/// `service`, the outbound token, and 1 ms retry backoff without jitter so
/// retries are quick and predictable.
pub(crate) fn caller_source(
    profile: Profile,
    extra: &[(&str, &str)],
    link: LinkSetup,
    service: Option<SocketAddr>,
) -> MapSource {
    let mut source = source_of(extra);
    match profile {
        Profile::MonolithLocal => {}
        Profile::MonolithSerialized => source.set(BINDING_KEY, "local-serialized"),
        Profile::SplitGrpc => {
            let addr = service.expect("the split profile needs the service's address");
            source.set("SEKVENT_COMPONENT_INVENTORY_BINDING", "grpc");
            source.set(
                "SEKVENT_COMPONENT_INVENTORY_ENDPOINT",
                format!("http://{addr}"),
            );
            if let Some(token) = link.outbound {
                source.set(OUTBOUND_KEY, token);
            }
            source.set("SEKVENT_COMPONENT_INVENTORY_RETRY_INITIAL_BACKOFF", "1ms");
            source.set("SEKVENT_COMPONENT_INVENTORY_RETRY_MAX_BACKOFF", "1ms");
            source.set("SEKVENT_COMPONENT_INVENTORY_RETRY_JITTER", "none");
        }
    }
    source
}

/// Build an App from `source` with `inventory` installed, bind it to an
/// ephemeral loopback port with `inventory-svc` and start its runtime.
pub(crate) async fn start_service(
    source: &MapSource,
    inventory: impl FnOnce(&mut AppBuilder<'_>) -> Result<(), BuildError>,
) -> InventoryService {
    let mut builder = App::builder(source);
    inventory(&mut builder).expect("the service's inventory installs");
    let app = builder.build().expect("the service's App builds");
    assert_eq!(app.binding("inventory"), Some(Binding::Local));

    let loopback: SocketAddr = "127.0.0.1:0".parse().expect("a socket address");
    let server = inventory_svc::bind(&app, loopback)
        .await
        .expect("the service binds a loopback port");
    let addr = server.local_addr();
    let runtime = Runtime::builder()
        .without_signals()
        .shutdown_delay(Duration::ZERO);
    let runtime = inventory_svc::runtime(&app, server, runtime)
        .build()
        .expect("the service's runtime builds")
        .start()
        .await
        .expect("the service starts");
    InventoryService { app, runtime, addr }
}

/// Build a topology without starting the caller's App.
///
/// Under [`Profile::SplitGrpc`] the service is started first, with
/// `inventory`; otherwise `inventory` is installed in the caller's App.
/// `extra` keys go to every process; notifications and orders are installed
/// on the caller side. Asserts every component's binding against the
/// profile.
pub(crate) async fn try_build(
    profile: Profile,
    extra: &[(&str, &str)],
    link: LinkSetup,
    inventory: impl FnOnce(&mut AppBuilder<'_>) -> Result<(), BuildError>,
) -> Result<Shop, BuildError> {
    let mut local_inventory = Some(inventory);
    let service = if profile == Profile::SplitGrpc {
        let install = local_inventory.take().expect("inventory is installed once");
        Some(start_service(&service_source(extra, link), install).await)
    } else {
        None
    };

    let source = caller_source(
        profile,
        extra,
        link,
        service.as_ref().map(|service| service.addr),
    );
    let mut builder = App::builder(&source);
    match local_inventory {
        Some(install) => install(&mut builder)?,
        // Bound `grpc`, the factory never runs; the stock is the service's.
        None => shop::install_inventory(&mut builder, Vec::new())?,
    }
    shop::install_notifications(&mut builder, options().blocked_customers)?;
    shop::install_orders(&mut builder)?;
    let app = builder.build()?;
    for component in COMPONENTS {
        assert_eq!(
            app.binding(component),
            Some(profile.binding_of(component)),
            "binding of {component} under {profile:?}"
        );
    }
    Ok(Shop { app, service })
}

/// [`try_build`] with the default link, expecting success.
pub(crate) async fn build(
    profile: Profile,
    extra: &[(&str, &str)],
    inventory: impl FnOnce(&mut AppBuilder<'_>) -> Result<(), BuildError>,
) -> Shop {
    try_build(profile, extra, LinkSetup::default(), inventory)
        .await
        .expect("the caller's App builds")
}

/// A started topology with the default link; see [`try_build`].
pub(crate) async fn start(
    profile: Profile,
    extra: &[(&str, &str)],
    inventory: impl FnOnce(&mut AppBuilder<'_>) -> Result<(), BuildError>,
) -> Shop {
    start_with(profile, extra, LinkSetup::default(), inventory).await
}

/// A started topology with `link`; see [`try_build`].
pub(crate) async fn start_with(
    profile: Profile,
    extra: &[(&str, &str)],
    link: LinkSetup,
    inventory: impl FnOnce(&mut AppBuilder<'_>) -> Result<(), BuildError>,
) -> Shop {
    let shop = try_build(profile, extra, link, inventory)
        .await
        .expect("the caller's App builds");
    shop.app.start().await.expect("the caller's App starts");
    shop
}

/// Install `inventory` in place of the real inventory.
pub(crate) fn fake<T: Inventory>(
    inventory: T,
) -> impl FnOnce(&mut AppBuilder<'_>) -> Result<(), BuildError> {
    move |app: &mut AppBuilder<'_>| InventoryHandle::install(app, move |_deps| Ok(inventory))
}

/// The real inventory with the stock of [`options`].
pub(crate) fn real_inventory(app: &mut AppBuilder<'_>) -> Result<(), BuildError> {
    shop::install_inventory(app, options().stock)
}

/// The whole shop with [`options`], started.
pub(crate) async fn started_shop(profile: Profile) -> Shop {
    start(profile, &[], real_inventory).await
}

/// The shop with `inventory` in place of the real inventory, started with
/// `extra` keys.
pub(crate) async fn shop_with_inventory<T: Inventory>(
    profile: Profile,
    extra: &[(&str, &str)],
    inventory: T,
) -> Shop {
    start(profile, extra, fake(inventory)).await
}

/// Assert `at_least <= elapsed`, and `elapsed < below` on the paused clock.
/// On the real clock loopback timing is not exact, so only the lower bound
/// holds; the structural assertions carry the test.
pub(crate) fn assert_elapsed(
    profile: Profile,
    elapsed: Duration,
    at_least: Duration,
    below: Duration,
) {
    assert!(
        elapsed >= at_least,
        "{elapsed:?} under {profile:?}: expected at least {at_least:?}"
    );
    if profile.paused() {
        assert!(
            elapsed < below,
            "{elapsed:?} under {profile:?}: expected less than {below:?}"
        );
    }
}

/// Assert that no time passed on the paused clock; nothing on the real one.
pub(crate) fn assert_no_wait(profile: Profile, elapsed: Duration) {
    if profile.paused() {
        assert_eq!(elapsed, Duration::ZERO, "time passed under {profile:?}");
    }
}

/// Run a real-clock test body, failing it after [`HANG_GUARD`].
pub(crate) async fn guarded<F: Future>(test: F) -> F::Output {
    tokio::time::timeout(HANG_GUARD, test)
        .await
        .expect("the test finished within the hang guard")
}

/// The keys of every `Missing` configuration error in `error`.
pub(crate) fn missing_keys(error: &BuildError) -> Vec<String> {
    fn config(error: &ConfigError) -> Vec<String> {
        match error {
            ConfigError::Missing { key, .. } => vec![key.clone()],
            ConfigError::Multiple(errors) => errors.iter().flat_map(config).collect(),
            _ => Vec::new(),
        }
    }
    match error {
        BuildError::Config(error) => config(error),
        BuildError::Multiple(errors) => errors.iter().flat_map(missing_keys).collect(),
        _ => Vec::new(),
    }
}
