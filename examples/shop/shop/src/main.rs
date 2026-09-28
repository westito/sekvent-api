//! The example shop as one process.
//!
//! Bindings come from the environment (`SEKVENT_COMPONENT_BINDING=local-serialized`
//! moves every component behind a serialization boundary); the components run
//! as one unit of the sekvent runtime until `SIGINT` or `SIGTERM`.

#![forbid(unsafe_code)]

use sekvent::config::EnvSource;
use sekvent::prelude::*;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let source = EnvSource;
    let mut builder = App::builder(&source);
    shop::install(&mut builder, shop::ShopOptions::demo())?;
    let app = builder.build()?;
    app.register(Runtime::builder()).build()?.run().await?;
    Ok(())
}
