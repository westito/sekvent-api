//! Tenant operations on top of the cluster operations: provision, adopt, suspend, resume, set
//! hosts, purge, usage and describe, through one [`Provisioner`] and the product's
//! [`TenantHooks`].
//!
//! Every hook but [`TenantHooks::check_adoptable`] runs on a pool of its own on the tenant's
//! migrated database, opened as the tenant's role outside the tenant cache. Opening a tenant that
//! is not `active` needs a crate-private capability that only the provisioning, state, adoption
//! and usage operations construct; a purge never opens the tenant.

mod adopt;
mod announce;
mod hooks;
mod open;
mod provision;
mod provisioner;
mod purge;
mod state;
mod usage;

pub use hooks::{
    AdoptCandidate, Description, HookFuture, NOT_CHECKED, SeedOutcome, TenantHooks, UsageCounters,
};
pub use provisioner::{
    AdoptRequest, DISPLAY_NAME_MAX, HostsChange, ProvisionRequest, Provisioned, Provisioner,
    ProvisionerBuilder, Usage,
};
