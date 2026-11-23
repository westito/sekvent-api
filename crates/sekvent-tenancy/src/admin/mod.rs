//! The admin surface a product serves to an operator service: the token verifier, the end-user
//! authenticator of the admin App, the audit record that refuses a replayed token, and the guard
//! a product wraps each of its own admin RPC bodies in. No service contract ships from sekvent:
//! the product declares its admin service in its own proto.

mod audit;
mod authenticator;
mod claims;
pub mod guard;
mod verifier;

pub use audit::AdminAudit;
pub use authenticator::{AdminAuthenticator, authenticator};
pub use claims::{AdminClaims, MAX_LIFETIME, NoExtra};
pub use verifier::{Verifier, parse_keys};
