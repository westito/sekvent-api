//! The example shop as one process.
//!
//! Bindings come from the environment: `SEKVENT_COMPONENT_BINDING=local-serialized`
//! moves every component behind a serialization boundary, and
//! `SEKVENT_COMPONENT_INVENTORY_BINDING=grpc` with an endpoint and a link
//! token calls an `inventory-svc` process instead (see the example's README).
//! gRPC and the health endpoints are served on `SHOP_GRPC_ADDR` (default
//! `127.0.0.1:50050`), including any component the environment exposes. The
//! components and the listener run as units of the sekvent runtime until
//! `SIGINT` or `SIGTERM`.

#![forbid(unsafe_code)]

use std::net::SocketAddr;

use sekvent::config::EnvSource;
use sekvent::prelude::*;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let source = EnvSource;
    let default: SocketAddr = shop::DEFAULT_GRPC_ADDR.parse()?;
    let addr = sekvent::config::opt_parse(&source, shop::GRPC_ADDR_KEY, default)?;

    let mut builder = App::builder(&source);
    shop::install(&mut builder, shop::ShopOptions::demo())?;
    let app = builder.build()?;
    let server = shop::bind(&app, addr).await?;
    shop::runtime(&app, server, Runtime::builder())
        .build()?
        .run()
        .await?;
    Ok(())
}
