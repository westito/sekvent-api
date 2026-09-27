//! Password hashing, JWT sessions and login helpers.
//!
//! The core ([`password`], [`jwt`], [`login`], [`roles`]) is pure: it spawns
//! nothing, reads no environment and never asks the system for the time.
//! Every time-dependent check takes `now_unix_secs` as a parameter, so the
//! same code runs on servers, in tests with a frozen clock and on
//! `wasm32-unknown-unknown`, where `SystemTime::now()` panics.
//!
//! # WebAssembly
//!
//! Randomness (password salts) comes from [`getrandom`]. On
//! `wasm32-unknown-unknown` it has no default backend: a consumer building
//! for the browser enables it in its own manifest,
//!
//! ```toml
//! getrandom = { version = "0.4", features = ["wasm_js"] }
//! ```
//!
//! and, because `jsonwebtoken` still pulls `getrandom` 0.2 on wasm32, also
//! `getrandom_02 = { package = "getrandom", version = "0.2", features = ["js"] }`.
//!
//! # Features
//!
//! - `axum`: the `axum::Bearer` extractor and `axum::RequireRole` guard.
//! - `tonic`: the `tonic::BearerInterceptor` and role helpers.
//!
//! Both adapters verify through a `BearerAuth`, which reads the time from an
//! injected `sekvent_context::Clock`; they are not wasm-compatible.

#![forbid(unsafe_code)]

pub mod jwt;
pub mod login;
pub mod password;
pub mod roles;

#[cfg(any(feature = "axum", feature = "tonic"))]
mod adapter;
#[cfg(feature = "axum")]
pub mod axum;
#[cfg(feature = "tonic")]
pub mod tonic;

#[cfg(any(feature = "axum", feature = "tonic"))]
pub use adapter::BearerAuth;
pub use jwt::{Claims, DEVELOPMENT_SECRET, JwtKeys, NoClaims, TokenRejected, Validation};
pub use login::{Authenticated, LoginOutcome, LoginRejected, authenticate};
pub use password::{PasswordHasher, PasswordParams, Verification};
pub use roles::{HasRoles, require_any_role};
