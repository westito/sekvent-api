//! Wiring of the example shop: three components in one App.
//!
//! Inventory, notifications and orders are installed in dependency order;
//! orders receives the other two as handles through constructor injection.
//! Nothing here chooses a binding: `SEKVENT_COMPONENT_BINDING` (for all
//! components) or `SEKVENT_COMPONENT_<NAME>_BINDING` (for one) decide at
//! build time whether calls stay in-process (`local`, the default), cross a
//! serialization boundary (`local-serialized`) or go to another process over
//! gRPC (`grpc`, with `SEKVENT_COMPONENT_<NAME>_ENDPOINT` and a link token).
//! A component bound `grpc` is installed exactly as before; its factory
//! simply does not run.
//!
//! ```no_run
//! # async fn run() -> Result<(), Box<dyn std::error::Error>> {
//! use sekvent::component::App;
//!
//! let source = sekvent::config::MapSource::new()
//!     .with("SEKVENT_COMPONENT_BINDING", "local-serialized");
//! let mut builder = App::builder(&source);
//! shop::install(&mut builder, shop::ShopOptions::demo())?;
//! let app = builder.build()?;
//! app.start().await?;
//! # Ok(())
//! # }
//! ```

#![forbid(unsafe_code)]

use std::net::SocketAddr;

use inventory::InventoryService;
use inventory_api::InventoryHandle;
use notifications::NotificationsService;
use notifications_api::NotificationsHandle;
use orders::OrdersService;
use orders_api::OrdersHandle;
use sekvent::component::{App, AppBuilder, BuildError};
use sekvent::error::AppError;
use sekvent::runtime::{RuntimeBuilder, Server, Stage, UnitPolicy};

/// Name of the ingress unit that serves the gRPC listener.
pub const GRPC_UNIT: &str = "grpc";

/// Key of the address the binary serves gRPC on.
pub const GRPC_ADDR_KEY: &str = "SHOP_GRPC_ADDR";

/// Address the binary serves gRPC on when [`GRPC_ADDR_KEY`] is unset.
pub const DEFAULT_GRPC_ADDR: &str = "127.0.0.1:50050";

/// What the shop starts with.
#[derive(Debug, Clone, Default)]
pub struct ShopOptions {
    /// Initial stock as `(sku, units)` pairs.
    pub stock: Vec<(String, u32)>,
    /// Customers who opted out of notifications; their orders are refused.
    pub blocked_customers: Vec<String>,
}

impl ShopOptions {
    /// A small catalogue and one blocked customer, for running the binary.
    pub fn demo() -> Self {
        Self {
            stock: vec![("sku-apple".to_owned(), 10), ("sku-pear".to_owned(), 3)],
            blocked_customers: vec!["cust-blocked".to_owned()],
        }
    }
}

/// Install the in-memory inventory holding `stock`.
pub fn install_inventory(
    app: &mut AppBuilder<'_>,
    stock: Vec<(String, u32)>,
) -> Result<(), BuildError> {
    InventoryHandle::install(app, move |_deps| Ok(InventoryService::new(stock)))
}

/// Install the in-memory notifications outbox, refusing the `blocked`
/// customers. Its `Lifecycle` hooks run when the App starts and stops.
pub fn install_notifications(
    app: &mut AppBuilder<'_>,
    blocked: Vec<String>,
) -> Result<(), BuildError> {
    NotificationsHandle::install_with_lifecycle(app, move |_deps| {
        Ok(NotificationsService::new(blocked))
    })
}

/// Install orders. Inventory and notifications must be installed first:
/// the factory asks for their handles, and a component can only depend on
/// components installed before it.
pub fn install_orders(app: &mut AppBuilder<'_>) -> Result<(), BuildError> {
    OrdersHandle::install(app, |deps| {
        Ok(OrdersService::new(
            deps.handle::<InventoryHandle>()?,
            deps.handle::<NotificationsHandle>()?,
        ))
    })
}

/// Install the whole shop: inventory, notifications, then orders.
pub fn install(app: &mut AppBuilder<'_>, options: ShopOptions) -> Result<(), BuildError> {
    install_inventory(app, options.stock)?;
    install_notifications(app, options.blocked_customers)?;
    install_orders(app)
}

/// A server on `addr` serving every component the environment exposes
/// (`SEKVENT_COMPONENT_<NAME>_SERVE=grpc`) and the health endpoints.
///
/// With nothing exposed it serves health only; taking the routes is what
/// lets [`App::start`] accept an exposed component.
pub async fn bind(app: &App, addr: SocketAddr) -> Result<Server, AppError> {
    Server::builder()
        .grpc_routes(app.grpc_routes())
        .bind(addr)
        .await
}

/// The App's `components` unit plus `server` as the ingress unit
/// [`GRPC_UNIT`].
pub fn runtime(app: &App, server: Server, runtime: RuntimeBuilder) -> RuntimeBuilder {
    app.register(runtime).unit(
        GRPC_UNIT,
        Stage::Ingress,
        UnitPolicy::default(),
        server.into_unit(),
    )
}
