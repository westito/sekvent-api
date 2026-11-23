//! The tenants this process serves: one fixed scope, or a cache of scopes.

mod keyed;
mod lease;
mod sweep;
#[cfg(feature = "runtime")]
mod unit;
mod walk;

#[cfg(feature = "lifecycle")]
pub(crate) use keyed::TenantCache;
pub use keyed::{ScopeFactory, ScopeFuture, Tenants, TenantsBuilder, TenantsStats};
pub use lease::TenantLease;
pub use walk::WalkReport;
