//! The example shop's inventory as its own process.
//!
//! Serves inventory over gRPC on `INVENTORY_SVC_ADDR` (default
//! `127.0.0.1:50051`) until `SIGINT` or `SIGTERM`. The environment must
//! expose the component and name its callers, for example
//! `SEKVENT_COMPONENT_INVENTORY_SERVE=grpc` and
//! `SEKVENT_LINK_INBOUND_SHOP=<token>`; see the example's README.

#![forbid(unsafe_code)]

use std::net::SocketAddr;

use sekvent::config::EnvSource;
use sekvent::prelude::*;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let source = EnvSource;
    let default: SocketAddr = inventory_svc::DEFAULT_ADDR.parse()?;
    let addr = sekvent::config::opt_parse(&source, inventory_svc::ADDR_KEY, default)?;

    let mut builder = App::builder(&source);
    inventory_svc::install(&mut builder, inventory_svc::demo_stock())?;
    let app = builder.build()?;
    let server = inventory_svc::bind(&app, addr).await?;
    inventory_svc::runtime(&app, server, Runtime::builder())
        .build()?
        .run()
        .await?;
    Ok(())
}
