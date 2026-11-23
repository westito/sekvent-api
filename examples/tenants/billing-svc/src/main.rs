//! The example billing service as a process.
//!
//! Reads its `BILLING_*` keys (see the example's README), migrates and
//! checks the control database, then serves the public App on
//! `BILLING_PUBLIC_ADDR` and the admin App on `BILLING_ADMIN_ADDR` until
//! `SIGINT` or `SIGTERM`.

#![forbid(unsafe_code)]

use billing_svc::{Billing, BillingConfig};
use sekvent::prelude::*;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let config: BillingConfig = sekvent::config::from_env()?;
    let billing = Billing::build(&config).await?;
    let servers = billing.bind(config.public_addr, config.admin_addr).await?;
    billing
        .runtime(servers, Runtime::builder())
        .build()?
        .run()
        .await?;
    Ok(())
}
