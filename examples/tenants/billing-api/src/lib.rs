//! Contracts of the example billing product.
//!
//! Two services, each a component checked against its proto:
//!
//! - [`Invoices`] (`billing.invoices.v1`): the public API, served to the end
//!   users of every tenant. Which tenant a call is for comes from the
//!   request, through the App's scope resolver; no message names it.
//! - [`TenantAdmin`] (`billing.admin.v1`): the product's own admin API,
//!   served on a second listener to an operator service that calls it with
//!   short-lived admin tokens. sekvent ships no admin service: the product
//!   declares the RPCs it wants and wraps each body in the admin guard.
//!
//! Callers depend on this crate only; the implementations live in the
//! `billing` crate.

#![forbid(unsafe_code)]

/// Messages of the `billing.invoices.v1` and `billing.admin.v1` packages and
/// the contracts of their services, which `#[component(proto = …)]` checks
/// the traits against.
pub mod proto {
    include!(concat!(env!("OUT_DIR"), "/sekvent_protos.rs"));
}

pub use proto::billing::admin::v1::{
    GetTenantReply, ProvisionTenantReply, ProvisionTenantRequest, Tenant, TenantRequest,
};
pub use proto::billing::invoices::v1::{
    CreateInvoiceRequest, Invoice, ListInvoicesReply, ListInvoicesRequest,
};
use sekvent::prelude::*;

/// The admin service's name, `<package>.<Service>`: what the admin
/// authenticator accepts calls for.
pub const ADMIN_SERVICE: &str = "billing.admin.v1.TenantAdmin";

/// The operations of [`TenantAdmin`] as admin tokens name them: the RPC path
/// without its leading slash.
pub mod ops {
    /// `ProvisionTenant`.
    pub const PROVISION_TENANT: &str = "billing.admin.v1.TenantAdmin/ProvisionTenant";
    /// `SuspendTenant`.
    pub const SUSPEND_TENANT: &str = "billing.admin.v1.TenantAdmin/SuspendTenant";
    /// `ResumeTenant`.
    pub const RESUME_TENANT: &str = "billing.admin.v1.TenantAdmin/ResumeTenant";
    /// `PurgeTenant`.
    pub const PURGE_TENANT: &str = "billing.admin.v1.TenantAdmin/PurgeTenant";
    /// `GetTenant`.
    pub const GET_TENANT: &str = "billing.admin.v1.TenantAdmin/GetTenant";
}

/// What can go wrong in the invoices component.
///
/// Each variant travels as an `AppError` with its reason, the domain
/// `billing.invoices.v1` and its fields as metadata. A refusal of the tenant
/// itself (unknown, suspended, busy) is answered by the scope resolver
/// before the component runs, in the serving domain.
#[derive(Debug, ComponentError)]
#[component_error(domain = "billing.invoices.v1")]
pub enum InvoicesError {
    /// A field of the request is out of range.
    #[reason("INVALID_INVOICE", code = InvalidArgument, message = "{field} {problem}")]
    InvalidInvoice {
        /// The field.
        field: String,
        /// What is wrong with it.
        problem: String,
    },
    /// The tenant has no default invoice series (it was adopted, not seeded).
    #[reason("NO_INVOICE_SERIES", code = FailedPrecondition)]
    NoInvoiceSeries,
    /// Any other error, including framework errors such as a missed deadline.
    #[other]
    Other(AppError),
}

/// The invoices of one tenant: the tenant of the call.
#[sekvent::component(
    name = "invoices",
    package = "billing.invoices.v1",
    proto = "crate::proto::billing::invoices::v1"
)]
pub trait Invoices: Send + Sync + 'static {
    /// Issue an invoice under the tenant's default series.
    #[call(timeout = "5s")]
    async fn create_invoice(
        &self,
        cx: &CallContext,
        req: CreateInvoiceRequest,
    ) -> Result<Invoice, InvoicesError>;

    /// The tenant's invoices, oldest first.
    #[call(idempotent, timeout = "5s")]
    async fn list_invoices(
        &self,
        cx: &CallContext,
        req: ListInvoicesRequest,
    ) -> Result<ListInvoicesReply, InvoicesError>;
}

/// The lifecycle of the billing product's tenants, for an operator service.
///
/// Every method runs under an admin token minted for that one RPC and the
/// tenant it addresses; the implementation checks both and records the
/// token before it acts, so a token is usable once.
#[sekvent::component(
    name = "tenant_admin",
    package = "billing.admin.v1",
    proto = "crate::proto::billing::admin::v1"
)]
pub trait TenantAdmin: Send + Sync + 'static {
    /// Create a tenant and make it active; resumes an interrupted
    /// provisioning when repeated with the same request.
    #[call(timeout = "30s")]
    async fn provision_tenant(
        &self,
        cx: &CallContext,
        req: ProvisionTenantRequest,
    ) -> Result<ProvisionTenantReply, AppError>;

    /// Stop serving an active tenant.
    #[call(timeout = "10s")]
    async fn suspend_tenant(
        &self,
        cx: &CallContext,
        req: TenantRequest,
    ) -> Result<Tenant, AppError>;

    /// Serve a suspended tenant again.
    #[call(timeout = "10s")]
    async fn resume_tenant(&self, cx: &CallContext, req: TenantRequest)
    -> Result<Tenant, AppError>;

    /// Drop a suspended tenant's database and role.
    #[call(timeout = "30s")]
    async fn purge_tenant(&self, cx: &CallContext, req: TenantRequest) -> Result<Tenant, AppError>;

    /// A tenant in any state, with its usage when its database is there.
    #[call(idempotent, timeout = "10s")]
    async fn get_tenant(
        &self,
        cx: &CallContext,
        req: TenantRequest,
    ) -> Result<GetTenantReply, AppError>;
}
