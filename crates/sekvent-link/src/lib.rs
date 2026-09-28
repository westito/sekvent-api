//! Service-to-service authentication with static bearer tokens.
//!
//! Each link between two services has one token. The calling side attaches
//! it as `Authorization: Bearer <token>` ([`BearerInjector`]); the receiving
//! side looks it up in a [`TokenMap`] and learns which service is calling
//! and whether that service may act on behalf of end users
//! ([`ServiceIdentity::trusted`]).
//!
//! Tokens are *canonical*: at least [`MIN_TOKEN_LEN`] characters from
//! `[A-Za-z0-9_-]`, nothing else. Nothing in this crate trims, and no error
//! message, log line or `Debug` output contains a token — errors name the
//! link. [`random_token`] generates a fresh one.
//!
//! Configuration comes from the environment through [`LinkConfig`]:
//!
//! | Key | Meaning |
//! |---|---|
//! | `SEKVENT_LINK_INBOUND_<NAME>` | token that link `<name>` presents to us |
//! | `SEKVENT_LINK_OUTBOUND_<NAME>` | token we present when calling `<name>` |
//! | `SEKVENT_LINK_TRUSTED` | comma-separated inbound links that may assert end-user identity |
//!
//! A process that must not let one token serve two purposes (say, accepted
//! from one link and presented to another) also calls
//! [`LinkConfig::check_distinct_tokens`].
//!
//! # Features
//!
//! - `axum`: `require_service`, an inbound middleware for
//!   `axum::middleware::from_fn_with_state`.
//! - `tonic`: `ServiceTokenInterceptor` (inbound) and a
//!   `tonic::service::Interceptor` implementation on [`BearerInjector`]
//!   (outbound).
//!
//! [`ServiceIdentity::trusted`]: sekvent_context::ServiceIdentity::trusted

#![forbid(unsafe_code)]

mod config;
mod error;
mod inbound;
mod outbound;
mod token;

pub use config::{
    INBOUND_PREFIX, LinkConfig, OUTBOUND_PREFIX, TRUSTED_KEY, inbound_key, outbound_key,
};
pub use error::LinkError;
#[cfg(feature = "tonic")]
pub use inbound::ServiceTokenInterceptor;
#[cfg(feature = "axum")]
pub use inbound::require_service;
pub use inbound::{Authenticator, REJECTED_MESSAGE, authenticate_headers, authenticator};
pub use outbound::{BearerInjector, InjectBearer};
pub use sekvent_context::ServiceIdentity;
pub use token::{
    InboundLink, MIN_TOKEN_LEN, TokenMap, random_token, validate_token, validate_unique,
};
