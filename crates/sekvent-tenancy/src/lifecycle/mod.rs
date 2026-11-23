//! Tenant operations on top of the cluster operations: provision, adopt, suspend, resume, set
//! hosts, purge, usage and describe, through one [`Provisioner`] and the product's
//! [`TenantHooks`].
//!
//! Every hook but [`TenantHooks::check_adoptable`] runs on a pool on the tenant's migrated
//! database, connected as the tenant's role: the warm entry's pool of this process's tenant cache
//! for an `active` tenant the cache holds, otherwise a pool opened for the operation outside the
//! cache that holds one slot of the cache's capacity. Opening a tenant that is not `active` needs
//! a crate-private capability that only the provisioning, state, adoption and usage operations
//! construct; a purge never opens the tenant.

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
pub use provision::PROVISION_LOCK;
pub use provisioner::{
    AdoptRequest, DISPLAY_NAME_MAX, HostsChange, ProvisionRequest, Provisioned, Provisioner,
    ProvisionerBuilder, Usage,
};
