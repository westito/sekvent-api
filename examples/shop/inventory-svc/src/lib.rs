//! The example shop's inventory as its own service.
//!
//! The same [`InventoryService`](inventory::InventoryService) the monolith
//! runs, installed in an App of its own and exposed over gRPC. Nothing in the
//! component changes: the environment exposes it and authenticates its
//! callers.
//!
//! | Key | Value |
//! |---|---|
//! | `SEKVENT_COMPONENT_INVENTORY_SERVE` | `grpc`: serve inventory through [`App::grpc_routes`] |
//! | `SEKVENT_LINK_INBOUND_<CALLER>` | the token a calling service presents, e.g. `SEKVENT_LINK_INBOUND_SHOP` |
//! | `SEKVENT_LINK_TRUSTED` | callers whose end-user subject and tenant are kept, e.g. `shop` |
//!
//! ```no_run
//! # async fn run() -> Result<(), Box<dyn std::error::Error>> {
//! use sekvent::prelude::*;
//!
//! let source = sekvent::config::MapSource::new()
//!     .with("SEKVENT_COMPONENT_INVENTORY_SERVE", "grpc")
//!     .with("SEKVENT_LINK_INBOUND_SHOP", "0123456789abcdefghijklmnopqrstuvwxyzABCD");
//! let mut builder = App::builder(&source);
//! inventory_svc::install(&mut builder, vec![("sku-apple".to_owned(), 10)])?;
//! let app = builder.build()?;
//! let server = inventory_svc::bind(&app, "127.0.0.1:50051".parse()?).await?;
//! inventory_svc::runtime(&app, server, Runtime::builder())
//!     .build()?
//!     .run()
//!     .await?;
//! # Ok(())
//! # }
//! ```

#![forbid(unsafe_code)]

use std::net::SocketAddr;

use inventory::InventoryService;
use inventory_api::InventoryHandle;
use sekvent::component::{AppBuilder, BuildError};
use sekvent::prelude::*;

/// Name of the ingress unit that serves the gRPC listener.
pub const GRPC_UNIT: &str = "grpc";

/// Key of the address the binary listens on.
pub const ADDR_KEY: &str = "INVENTORY_SVC_ADDR";

/// Address the binary listens on when [`ADDR_KEY`] is unset.
pub const DEFAULT_ADDR: &str = "127.0.0.1:50051";

/// A small catalogue, for running the binary.
pub fn demo_stock() -> Vec<(String, u32)> {
    vec![("sku-apple".to_owned(), 10), ("sku-pear".to_owned(), 3)]
}

/// Install the inventory component with `stock`.
pub fn install(app: &mut AppBuilder<'_>, stock: Vec<(String, u32)>) -> Result<(), BuildError> {
    InventoryHandle::install(app, move |_deps| Ok(InventoryService::new(stock)))
}

/// A server on `addr` serving the App's exposed components and the health
/// endpoints (`grpc.health.v1` and HTTP).
///
/// This takes the App's gRPC routes, which [`App::start`] requires of every
/// exposed component. Bind to port 0 for an ephemeral port and read it back
/// with [`Server::local_addr`].
pub async fn bind(app: &App, addr: SocketAddr) -> Result<Server, AppError> {
    Server::builder()
        .grpc_routes(app.grpc_routes())
        .bind(addr)
        .await
}

/// The App's `components` unit plus `server` as the ingress unit
/// [`GRPC_UNIT`].
///
/// On shutdown the listener drains first, letting in-flight calls finish,
/// then the components stop; every exposed service reports `NotServing` in
/// the runtime's health registry from the moment the App stops serving.
pub fn runtime(app: &App, server: Server, runtime: RuntimeBuilder) -> RuntimeBuilder {
    app.register(runtime).unit(
        GRPC_UNIT,
        Stage::Ingress,
        UnitPolicy::default(),
        server.into_unit(),
    )
}
