//! The product's own `TenantAdmin` component: every body runs inside the
//! admin guard and calls the provisioner.

use billing_api::{
    GetTenantReply, ProvisionTenantReply, ProvisionTenantRequest, Tenant, TenantAdmin,
    TenantRequest, ops,
};
use sekvent::prelude::*;
use sekvent::tenancy::admin::{AdminAudit, AdminClaims, guard};
use sekvent::tenancy::lifecycle::{ProvisionRequest, Provisioner};
use sekvent::tenancy::{TenantId, TenantRow, TenantState};

use crate::hooks::{BillingHooks, BillingSeed};

/// The longest owner address, in bytes.
const EMAIL_MAX: usize = 254;

/// The `TenantAdmin` component: the provisioner of this product's tenants
/// and the audit that makes every admin token usable once.
///
/// Each method parses the tenant id, then runs its body through
/// [`guard::run`]: the token must have been minted for this RPC and this
/// tenant, and its id is recorded before the body runs. Which subjects may
/// call is the verifier's subject check (the service's wiring).
pub struct TenantAdminService {
    provisioner: Provisioner<BillingHooks>,
    audit: AdminAudit,
}

impl std::fmt::Debug for TenantAdminService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("TenantAdminService(..)")
    }
}

impl TenantAdminService {
    /// The component over `provisioner`, recording tokens in `audit`.
    pub fn new(provisioner: Provisioner<BillingHooks>, audit: AdminAudit) -> Self {
        Self { provisioner, audit }
    }
}

/// `INVALID_ARGUMENT` naming `field`.
fn invalid(field: &str, problem: &str) -> AppError {
    AppError::invalid_argument("the request is invalid").with_field_violation(field, problem)
}

/// The tenant a request names.
fn tenant_id(text: &str) -> Result<TenantId, AppError> {
    TenantId::parse(text).ok_or_else(|| invalid("tenant_id", "is not a tenant id"))
}

/// The seed of a provisioning request, checked: an address with an `@` and
/// no control character, a currency of three capital letters.
fn seed_of(req: &ProvisionTenantRequest) -> Result<BillingSeed, AppError> {
    let email = req.owner_email.trim();
    let email_ok = email.len() <= EMAIL_MAX
        && email.contains('@')
        && !email.chars().any(|c| c.is_control() || c.is_whitespace());
    let currency_ok =
        req.currency.len() == 3 && req.currency.bytes().all(|b| b.is_ascii_uppercase());
    match (email_ok, currency_ok) {
        (true, true) => Ok(BillingSeed {
            owner_email: email.to_owned(),
            currency: req.currency.clone(),
        }),
        (false, true) => Err(invalid("owner_email", "is not an address")),
        (true, false) => Err(invalid("currency", "is not three capital letters")),
        (false, false) => Err(invalid("owner_email", "is not an address")
            .with_field_violation("currency", "is not three capital letters")),
    }
}

/// The wire form of a control row.
fn tenant_of(row: &TenantRow) -> Tenant {
    let info = &row.info;
    Tenant {
        tenant_id: info.id.to_string(),
        subdomain: info.subdomain.clone(),
        display_name: info.display_name.clone(),
        state: info.state.as_str().to_owned(),
        primary_host: info.primary_host.clone(),
        hosts: info.hosts.clone(),
    }
}

impl TenantAdmin for TenantAdminService {
    async fn provision_tenant(
        &self,
        cx: &CallContext,
        req: ProvisionTenantRequest,
    ) -> Result<ProvisionTenantReply, AppError> {
        let tenant = tenant_id(&req.tenant_id)?;
        guard::run(
            &self.audit,
            cx,
            ops::PROVISION_TENANT,
            Some(tenant),
            |_claims: AdminClaims| async move {
                let seed = seed_of(&req)?;
                let request = ProvisionRequest {
                    tenant,
                    subdomain: req.subdomain,
                    display_name: req.display_name,
                    hosts: req.hosts,
                    primary_host: req.primary_host,
                    seed,
                };
                let provisioned = Box::pin(self.provisioner.provision(request)).await?;
                Ok(ProvisionTenantReply {
                    tenant: Some(tenant_of(&provisioned.row)),
                    owner_token_withheld: provisioned.handover.is_none(),
                    owner_token: provisioned
                        .handover
                        .map(|token| token.expose().to_owned())
                        .unwrap_or_default(),
                })
            },
        )
        .await
    }

    async fn suspend_tenant(
        &self,
        cx: &CallContext,
        req: TenantRequest,
    ) -> Result<Tenant, AppError> {
        let tenant = tenant_id(&req.tenant_id)?;
        guard::run(
            &self.audit,
            cx,
            ops::SUSPEND_TENANT,
            Some(tenant),
            |_claims: AdminClaims| async move {
                Ok(tenant_of(
                    &Box::pin(self.provisioner.suspend(tenant)).await?,
                ))
            },
        )
        .await
    }

    async fn resume_tenant(
        &self,
        cx: &CallContext,
        req: TenantRequest,
    ) -> Result<Tenant, AppError> {
        let tenant = tenant_id(&req.tenant_id)?;
        guard::run(
            &self.audit,
            cx,
            ops::RESUME_TENANT,
            Some(tenant),
            |_claims: AdminClaims| async move {
                Ok(tenant_of(&Box::pin(self.provisioner.resume(tenant)).await?))
            },
        )
        .await
    }

    async fn purge_tenant(&self, cx: &CallContext, req: TenantRequest) -> Result<Tenant, AppError> {
        let tenant = tenant_id(&req.tenant_id)?;
        guard::run(
            &self.audit,
            cx,
            ops::PURGE_TENANT,
            Some(tenant),
            |_claims: AdminClaims| async move {
                Ok(tenant_of(&Box::pin(self.provisioner.purge(tenant)).await?))
            },
        )
        .await
    }

    async fn get_tenant(
        &self,
        cx: &CallContext,
        req: TenantRequest,
    ) -> Result<GetTenantReply, AppError> {
        let tenant = tenant_id(&req.tenant_id)?;
        guard::run(
            &self.audit,
            cx,
            ops::GET_TENANT,
            Some(tenant),
            |_claims: AdminClaims| async move {
                let row = self.provisioner.tenant(tenant).await?;
                let mut reply = GetTenantReply {
                    tenant: Some(tenant_of(&row)),
                    ..GetTenantReply::default()
                };
                if matches!(row.info.state, TenantState::Active | TenantState::Suspended) {
                    let usage = Box::pin(self.provisioner.usage(tenant)).await?;
                    reply.measured = true;
                    reply.database_bytes = usage.database_bytes;
                    reply.invoices = usage
                        .counters
                        .counters
                        .get("invoices")
                        .copied()
                        .unwrap_or_default();
                }
                Ok(reply)
            },
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(owner_email: &str, currency: &str) -> ProvisionTenantRequest {
        ProvisionTenantRequest {
            owner_email: owner_email.to_owned(),
            currency: currency.to_owned(),
            ..ProvisionTenantRequest::default()
        }
    }

    fn fields(error: &AppError) -> Vec<&str> {
        error
            .field_violations()
            .iter()
            .map(|violation| violation.field.as_str())
            .collect()
    }

    #[test]
    fn seeds_and_tenant_ids_are_checked() {
        let seed = seed_of(&request(" owner@example.com ", "EUR")).expect("valid");
        assert_eq!(seed.owner_email, "owner@example.com");
        assert_eq!(seed.currency, "EUR");

        let error = seed_of(&request("owner", "EUR")).expect_err("no @");
        assert_eq!(fields(&error), ["owner_email"]);
        let error = seed_of(&request("owner@example.com", "eur")).expect_err("lower case");
        assert_eq!(fields(&error), ["currency"]);
        let error = seed_of(&request("", "EURO")).expect_err("both");
        assert_eq!(fields(&error), ["owner_email", "currency"]);

        let error = tenant_id("not-a-tenant").expect_err("malformed");
        assert_eq!(error.code(), ErrorCode::InvalidArgument);
        assert_eq!(fields(&error), ["tenant_id"]);
    }

    #[tokio::test]
    async fn a_call_without_admin_claims_is_refused_before_the_body() {
        // A pool that never connects: the guard refuses before it records.
        let control = sqlx::postgres::PgPoolOptions::new().connect_lazy_with(
            sqlx::postgres::PgConnectOptions::new()
                .host("127.0.0.1")
                .port(1),
        );
        let audit = AdminAudit::new(control, std::sync::Arc::new(sekvent::context::SystemClock));
        let tenant = TenantId::from_uuid(uuid::Uuid::from_u128(7));
        let error = guard::run(
            &audit,
            &CallContext::new(),
            ops::SUSPEND_TENANT,
            Some(tenant),
            |_claims: AdminClaims| async { Ok::<_, AppError>(()) },
        )
        .await
        .expect_err("no claims");
        assert_eq!(error.code(), ErrorCode::Unauthenticated);
        assert_eq!(
            error.reason(),
            Some(sekvent::tenancy::reasons::ADMIN_TOKEN_INVALID)
        );
    }
}
