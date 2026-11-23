//! The operator side of the admin surface: a token minter and a client that calls a product's
//! admin RPC by its path over gRPC-Web, with a typed outcome. No database driver.

mod client;
mod frame;
mod minter;

pub use client::{AdminClient, CallOutcome};
pub use minter::{Minted, Minter, TOKEN_LIFETIME};
