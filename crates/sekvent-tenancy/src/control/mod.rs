//! The control database: the reference schema, the tenant rows, the registry with its lookup cache
//! and the two admissions.

mod admission;
mod lookup;
mod registry;
pub mod rows;
mod schema;

pub use admission::LifecyclePermit;
pub use registry::Registry;
pub use rows::{MAIN_CLUSTER, NewTenant, ProvisionStep, TenantRow, VersionStored};
pub use schema::{REFERENCE_SCHEMA, verify_schema};
