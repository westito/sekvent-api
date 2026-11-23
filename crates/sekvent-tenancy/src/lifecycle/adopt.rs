//! Adoption: an existing database becomes a tenant's, checked before it changes hands.

use sekvent_error::AppError;

use super::hooks::{AdoptCandidate, TenantHooks};
use super::provision::{Steps, ensure_row, run_steps};
use super::provisioner::{AdoptRequest, Provisioner, Violations, collect_tenant};
use crate::cluster::handover::{self, Adoption};
use crate::cluster::maintenance::OPERATION_TIMEOUT;
use crate::control::NewTenant;
use crate::{DatabaseName, ObjectNames, RoleKey, TenantRole, TenantRow, is_adoptable_name};

/// What the check of a candidate answers when it outlives a maintenance operation's bound.
const CHECK_TOO_SLOW: &str = "the database to adopt did not answer in time";

impl<H: TenantHooks> Provisioner<H> {
    /// Adopts `request.database` as the tenant's, or resumes an adoption at its recorded step.
    ///
    /// 1. Validation (`INVALID_ARGUMENT` with field violations; the database must pass
    ///    `is_adoptable_name`, must not be a tenant object name of the provisioner's names, nor
    ///    the control or the maintenance database), then the lifecycle admission.
    /// 2. Without a row yet: `handover::prepare` (an existing role of the tenant's name that fails
    ///    the tenant-role check is refused first, before anything changes), then, while the
    ///    maintenance role still owns the database, `TenantHooks::check_adoptable` on
    ///    `Maintenance::pool_on` of it (read-only sessions), bounded like a maintenance operation.
    ///    A refusal leaves no control row and no role (at most `PUBLIC`'s `CONNECT` revoked).
    /// 3. The row inserted (`provision::ensure_row`) and the steps of a provisioning run with
    ///    the database step replaced by [`hand_over`] and the seed by `TenantHooks::adopted`. No
    ///    handover secret exists.
    pub async fn adopt(&self, request: AdoptRequest) -> Result<TenantRow, AppError> {
        let inner = &self.inner;
        let mut violations = Violations::default();
        let valid = collect_tenant(
            &mut violations,
            &inner.labels,
            inner.display_name_max,
            &request.subdomain,
            &request.display_name,
            &request.hosts,
            &request.primary_host,
        );
        if let Some(problem) = database_problem(
            &inner.names,
            &inner.control_database,
            inner.maintenance.database(),
            &request.database,
        ) {
            violations.push("database", problem);
        }
        violations.finish()?;
        let valid = valid.ok_or_else(|| AppError::internal("a valid tenant without a value"))?;
        let database = DatabaseName::parse(&request.database)?;
        let id = request.tenant;
        let role = inner.names.role(id);

        let _permit = inner.registry.admit().await?;
        if inner.registry.tenant_now(id).await?.is_none() {
            let candidate = AdoptCandidate::new(
                id,
                database.clone(),
                valid.hosts.subdomain.clone(),
                valid.display_name.clone(),
            );
            check_before_handover(self, &candidate, &role).await?;
        }
        let new = NewTenant {
            id,
            subdomain: valid.hosts.subdomain,
            display_name: valid.display_name,
            hosts: valid.hosts.hosts,
            primary_host: valid.hosts.primary_host,
            database,
            role,
            role_key: RoleKey {
                key_id: inner.role_key_id.clone(),
                generation: 1,
            },
        };
        ensure_row(self, &new).await?;
        let (row, _) = run_steps(self, &new, Steps::Adopt).await?;
        Ok(row)
    }
}

/// The adoption's `database` step: the checks of step 2 again, against the stored database
/// (`check_adoptable` only while the maintenance role still owns it), then `handover::adopt` to
/// the tenant's role, which exists (the `role` step ran).
pub(crate) async fn hand_over<H: TenantHooks>(
    provisioner: &Provisioner<H>,
    row: &TenantRow,
) -> Result<(), AppError> {
    let (role, database) = provisioner.stored_names(row)?;
    let candidate = AdoptCandidate::new(
        row.info.id,
        database.clone(),
        row.info.subdomain.clone(),
        row.info.display_name.clone(),
    );
    check_before_handover(provisioner, &candidate, &role).await?;
    handover::adopt(&provisioner.inner.maintenance, &database, &role).await
}

/// `handover::prepare` for the candidate and `role` (an existing `role` is checked first, so a
/// drifted one is refused before step 2's revoke and before the hook); when the maintenance role
/// still owns the database, the product's `check_adoptable` on a read-only maintenance pool of it,
/// bounded by a maintenance operation's deadline. The pool is closed whatever the check answered;
/// the hook's answer is returned unchanged.
async fn check_before_handover<H: TenantHooks>(
    provisioner: &Provisioner<H>,
    candidate: &AdoptCandidate,
    role: &TenantRole,
) -> Result<(), AppError> {
    let inner = &provisioner.inner;
    if handover::prepare(&inner.maintenance, &candidate.database, role).await? == Adoption::Restrict
    {
        // The tenant's role owns it already: the maintenance role may no longer connect.
        return Ok(());
    }
    let pool = inner.maintenance.pool_on(&candidate.database).await?;
    let checked = tokio::time::timeout(
        OPERATION_TIMEOUT,
        inner.hooks.check_adoptable(candidate, pool.pool()),
    )
    .await;
    pool.close().await;
    checked.unwrap_or_else(|_| Err(AppError::unavailable(CHECK_TOO_SLOW)))
}

/// Why `database` cannot be named by an adoption, in words that never repeat it: not a name
/// [`is_adoptable_name`] accepts, tenant-named (it has the prefix of `names`, which covers every
/// tenant object name), the control database or the maintenance database.
fn database_problem(
    names: &ObjectNames,
    control_database: &str,
    maintenance_database: &str,
    database: &str,
) -> Option<&'static str> {
    if !is_adoptable_name(database) {
        Some("is not a database name that can be adopted")
    } else if names.has_prefix(database) {
        Some("is named like a tenant's database")
    } else if database == control_database {
        Some("is the control database")
    } else if database == maintenance_database {
        Some("is the maintenance database")
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use uuid::Uuid;

    use super::database_problem;
    use crate::{ObjectNames, TenantId};

    const CONTROL: &str = "billing_control";
    const MAINTENANCE: &str = "billing_maintenance";

    fn names() -> ObjectNames {
        ObjectNames::new("app_t_").expect("the prefix is valid")
    }

    fn problem(database: &str) -> Option<&'static str> {
        database_problem(&names(), CONTROL, MAINTENANCE, database)
    }

    #[test]
    fn an_ordinary_database_may_be_adopted() {
        assert_eq!(problem("legacy_orders"), None);
        assert_eq!(problem("_inventory2"), None);
        assert_eq!(problem("app_orders"), None);
        assert_eq!(problem(&"a".repeat(63)), None);
    }

    #[test]
    fn a_name_outside_the_grammar_is_refused() {
        let refused = Some("is not a database name that can be adopted");
        let long = "a".repeat(64);
        for database in [
            "",
            "Orders",
            "orders-old",
            "9orders",
            "orders old",
            "postgres",
            "template0",
            "template1",
            long.as_str(),
        ] {
            assert_eq!(problem(database), refused, "{database:?}");
        }
    }

    #[test]
    fn a_tenant_named_database_is_refused() {
        let refused = Some("is named like a tenant's database");
        let object = names().database(TenantId::from_uuid(Uuid::from_u128(7)));
        assert_eq!(problem(object.as_str()), refused);
        assert_eq!(problem("app_t_orders"), refused);
        assert_eq!(problem("app_t_"), refused);
    }

    #[test]
    fn the_control_and_the_maintenance_database_are_refused() {
        assert_eq!(problem(CONTROL), Some("is the control database"));
        assert_eq!(problem(MAINTENANCE), Some("is the maintenance database"));
    }

    #[test]
    fn no_description_repeats_the_name() {
        for database in ["Legacy", "app_t_legacy", CONTROL, MAINTENANCE] {
            let description = problem(database).expect("refused");
            assert!(!description.contains(database), "{description}");
        }
    }
}
