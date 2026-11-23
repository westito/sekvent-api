//! Provisioning as resumable steps: the role, the database, the migration, the seed, `active`.
//!
//! The call is admitted first (`Registry::admit`, held to its end). The row is inserted under a
//! transaction-level advisory lock (the provisioner's `provision_lock`) that orders the cap check
//! and the insert. Then each step is a
//! control transaction of its own: lock the row, re-read it, decide (`plan`), do the step's work
//! (idempotent), record the step, commit. A repeated call resumes after the last recorded step;
//! two calls for one tenant run one after the other. Adoption runs the same machine
//! (`run_steps`) with its own database step and its own last hook.

use sekvent_config::Secret;
use sekvent_db::to_app_error;
use sekvent_error::AppError;

use super::announce::announce;
use super::hooks::TenantHooks;
use super::open::{LifecycleCapability, OpenMode, open};
use super::provisioner::{
    ControlTx, ProvisionRequest, Provisioned, Provisioner, Violations, collect_tenant, commit,
    rollback,
};
use crate::cluster::{databases, roles};
use crate::control::{NewTenant, ProvisionStep, VersionStored, rows};
use crate::error::{tenant_limit_reached, tenant_state};
use crate::invalidation::ChangeKind;
use crate::{RoleKey, ScramVerifier, TenantRow, TenantState};

/// The default key of the control database's transaction-level advisory lock under which a new
/// tenant row is counted against the cap and inserted (provisioning and adoption); see
/// [`ProvisionerBuilder::provision_lock`](super::ProvisionerBuilder::provision_lock).
pub const PROVISION_LOCK: i64 = 0x7365_6b76_5f74_6e74;

/// The most rounds of the step machine: one per step, with room to spare.
const MAX_ROUNDS: usize = 8;

/// What the step machine runs for the `database` and `seeded` steps.
pub(crate) enum Steps<'a, S> {
    /// Provisioning: `databases::create`, then `TenantHooks::seed` with this payload.
    Provision {
        /// The seed payload of the request.
        seed: &'a S,
    },
    /// Adoption: `adopt::hand_over`, then `TenantHooks::adopted`; no handover secret exists.
    Adopt,
}

impl<H: TenantHooks> Provisioner<H> {
    /// Provisions a tenant, or resumes a provisioning at its recorded step.
    ///
    /// The request is validated first (`INVALID_ARGUMENT` with a field violation per problem:
    /// the label by the label rules, the display name, the hosts canonical with the primary among
    /// them). Then the lifecycle admission; the row inserted when the id is new (a new id at the
    /// cap is `TENANT_LIMIT_REACHED`; a host, role or database name another tenant holds is
    /// `TENANT_STATE`); then the steps.
    ///
    /// | Step recorded | Work |
    /// |---|---|
    /// | `role` | the password derived under the configured key id at generation 1; the SCRAM verifier on the blocking pool; `roles::create` with the connection limit; the key stored on the row |
    /// | `database` | `databases::create` |
    /// | `migrated` | the tenant opened for the operation (which migrates it); the migrator's latest version stored |
    /// | `seeded` | `TenantHooks::seed` on that pool |
    /// | `active` | step and state together, then the change announced |
    ///
    /// A call for an existing id resumes when the request matches the row (label, display name,
    /// hosts, primary host, and the database a provisioning names), else `TENANT_STATE`; so is
    /// a tenant that is suspended, purging or purged. A tenant already `active` that matches is
    /// returned and announced again, never re-seeded. The handover secret is returned once, by
    /// the call that ran the seed. A refusal can follow committed work: steps are resumable, not
    /// one transaction.
    pub async fn provision(
        &self,
        request: ProvisionRequest<H::Seed>,
    ) -> Result<Provisioned, AppError> {
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
        violations.finish()?;
        let valid = valid.ok_or_else(|| AppError::internal("a valid request without a value"))?;
        let id = request.tenant;
        let new = NewTenant {
            id,
            subdomain: valid.hosts.subdomain,
            display_name: valid.display_name,
            hosts: valid.hosts.hosts,
            primary_host: valid.hosts.primary_host,
            database: inner.names.database(id),
            role: inner.names.role(id),
            role_key: RoleKey {
                key_id: inner.role_key_id.clone(),
                generation: 1,
            },
        };
        let _permit = inner.registry.admit().await?;
        ensure_row(self, &new).await?;
        let (row, handover) = run_steps(
            self,
            &new,
            Steps::Provision {
                seed: &request.seed,
            },
        )
        .await?;
        Ok(Provisioned { row, handover })
    }
}

/// The tenant's row, inserted when it has none: under [`PROVISION_LOCK`], the row looked up again
/// (an existing one is a resume and never counts against the cap), `rows::count_live` against
/// the cap (`TENANT_LIMIT_REACHED`), `rows::insert`, a commit of its own, the registry's lookups of
/// the tenant forgotten. Called after the lifecycle admission was taken.
pub(crate) async fn ensure_row<H: TenantHooks>(
    provisioner: &Provisioner<H>,
    new: &NewTenant,
) -> Result<(), AppError> {
    let mut tx = provisioner.begin().await?;
    match insert_locked(provisioner, &mut tx, new).await {
        Ok(true) => {
            commit(tx).await?;
            provisioner.inner.registry.forget(new.id);
            Ok(())
        }
        Ok(false) => rollback(tx).await,
        Err(error) => Err(abandon(tx, error).await),
    }
}

/// The body of [`ensure_row`] in its transaction: whether a row was inserted.
async fn insert_locked<H: TenantHooks>(
    provisioner: &Provisioner<H>,
    tx: &mut ControlTx,
    new: &NewTenant,
) -> Result<bool, AppError> {
    sqlx::query("SELECT pg_catalog.pg_advisory_xact_lock($1)")
        .bind(provisioner.inner.provision_lock)
        .execute(&mut **tx)
        .await
        .map_err(to_app_error)?;
    if rows::get(tx, new.id).await?.is_some() {
        return Ok(false);
    }
    if rows::count_live(tx).await? >= provisioner.inner.tenant_cap {
        return Err(tenant_limit_reached());
    }
    let (_, inserted) = rows::insert(tx, new, provisioner.now()).await?;
    Ok(inserted)
}

/// The step machine of a provisioning and an adoption, after the row exists: per round, the
/// row locked, the plan (`TENANT_STATE` for a state or a request it refuses), the step, the step
/// recorded and committed. The `role`, `migrated` and `active` steps are shared; `database` and
/// `seeded` follow `steps`. An `active` row that matches is returned and announced again. Returns
/// the row as committed and the seed's handover (always `None` for an adoption). `INTERNAL` when
/// the steps do not finish within a fixed number of rounds.
pub(crate) async fn run_steps<H: TenantHooks>(
    provisioner: &Provisioner<H>,
    new: &NewTenant,
    steps: Steps<'_, H::Seed>,
) -> Result<(TenantRow, Option<Secret>), AppError> {
    let mut handover = None;
    for _ in 0..MAX_ROUNDS {
        let (mut tx, row) = provisioner.begin_locked(new.id).await?;
        let step = match plan(&row, new) {
            Ok(Plan::Done) => {
                rollback(tx).await?;
                announce(provisioner, new.id, ChangeKind::STATE).await;
                return Ok((row, handover));
            }
            Ok(Plan::Run(step)) => step,
            Err(error) => return Err(abandon(tx, error).await),
        };
        if let Err(error) = provisioner.stored_names(&row) {
            return Err(abandon(tx, error).await);
        }
        match work(provisioner, &mut tx, &row, step, &steps).await {
            Ok(Some(secret)) => handover = Some(secret),
            Ok(None) => {}
            Err(error) => return Err(abandon(tx, error).await),
        }
        if step == ProvisionStep::Active {
            let done = match activate(provisioner, &mut tx, &row).await {
                Ok(done) => done,
                Err(error) => return Err(abandon(tx, error).await),
            };
            commit(tx).await?;
            announce(provisioner, new.id, ChangeKind::STATE).await;
            return Ok((done, handover));
        }
        let recorded = rows::set_step(&mut tx, new.id, step, provisioner.now()).await;
        match recorded {
            Ok(true) => {}
            Ok(false) => return Err(abandon(tx, row_gone()).await),
            Err(error) => return Err(abandon(tx, error).await),
        }
        commit(tx).await?;
        provisioner.inner.registry.forget(new.id);
    }
    Err(AppError::internal("the provisioning steps did not finish"))
}

/// What a round of the step machine does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Plan {
    /// The tenant is active and matches: nothing to do.
    Done,
    /// Run this step and record it.
    Run(ProvisionStep),
}

/// The round's decision from the locked row: `TENANT_STATE` for a suspended, purging or purged
/// tenant and for a request that does not match the row; `Done` for a matching active tenant;
/// otherwise the step after the recorded one.
fn plan(row: &TenantRow, new: &NewTenant) -> Result<Plan, AppError> {
    let next = match row.info.state {
        TenantState::Active => None,
        TenantState::Provisioning => Some(next_step(row.provision_step)),
        TenantState::Suspended | TenantState::Purging | TenantState::Purged => {
            return Err(tenant_state());
        }
    };
    if !matches_request(row, new) {
        return Err(tenant_state());
    }
    Ok(next.map_or(Plan::Done, Plan::Run))
}

/// The step after `recorded`; `active` after `seeded` and after `active`.
fn next_step(recorded: ProvisionStep) -> ProvisionStep {
    match recorded {
        ProvisionStep::Inserted => ProvisionStep::Role,
        ProvisionStep::Role => ProvisionStep::Database,
        ProvisionStep::Database => ProvisionStep::Migrated,
        ProvisionStep::Migrated => ProvisionStep::Seeded,
        ProvisionStep::Seeded | ProvisionStep::Active => ProvisionStep::Active,
    }
}

/// Whether the row is the one `new` asks for: the label, the display name, the primary host, the
/// host set (sorted, without duplicates) and the database name.
fn matches_request(row: &TenantRow, new: &NewTenant) -> bool {
    let mut hosts = row.info.hosts.clone();
    hosts.sort_unstable();
    hosts.dedup();
    let mut wanted = new.hosts.clone();
    wanted.sort_unstable();
    wanted.dedup();
    row.info.subdomain == new.subdomain
        && row.info.display_name == new.display_name
        && row.info.primary_host == new.primary_host
        && hosts == wanted
        && row.db_name == new.database.as_str()
}

/// The work of `step` (all but recording it); a seed's handover when it ran one.
async fn work<H: TenantHooks>(
    provisioner: &Provisioner<H>,
    tx: &mut ControlTx,
    row: &TenantRow,
    step: ProvisionStep,
    steps: &Steps<'_, H::Seed>,
) -> Result<Option<Secret>, AppError> {
    match step {
        ProvisionStep::Role => create_role(provisioner, tx, row).await.map(|()| None),
        ProvisionStep::Database => match steps {
            Steps::Provision { .. } => create_database(provisioner, row).await.map(|()| None),
            Steps::Adopt => super::adopt::hand_over(provisioner, row)
                .await
                .map(|()| None),
        },
        ProvisionStep::Migrated => migrate(provisioner, tx, row).await.map(|()| None),
        ProvisionStep::Seeded => seed(provisioner, row, steps).await,
        ProvisionStep::Inserted | ProvisionStep::Active => Ok(None),
    }
}

/// The `role` step: the derived password's verifier, the role created or converged, the
/// configured key stored on the row when it holds another.
async fn create_role<H: TenantHooks>(
    provisioner: &Provisioner<H>,
    tx: &mut ControlTx,
    row: &TenantRow,
) -> Result<(), AppError> {
    let inner = &provisioner.inner;
    let (role, _) = provisioner.stored_names(row)?;
    let key = RoleKey {
        key_id: inner.role_key_id.clone(),
        generation: 1,
    };
    let password = inner.connector.secrets().derive(row.info.id, &key)?;
    let verifier = tokio::task::spawn_blocking(move || ScramVerifier::new(&password))
        .await
        .map_err(|_| AppError::internal("the verifier's task did not finish"))??;
    let mut conn = inner.maintenance.connect().await?;
    let created = roles::create(&mut conn, &role, &verifier, inner.connection_limit).await;
    let closed = conn.close().await;
    created.and(closed)?;
    if row.info.role_key != key {
        let stored = rows::set_role_key(tx, row.info.id, &key, provisioner.now()).await?;
        if !stored {
            return Err(row_gone());
        }
    }
    Ok(())
}

/// The provisioning's `database` step: `databases::create` on the maintenance connection.
async fn create_database<H: TenantHooks>(
    provisioner: &Provisioner<H>,
    row: &TenantRow,
) -> Result<(), AppError> {
    let (role, database) = provisioner.stored_names(row)?;
    let mut conn = provisioner.inner.maintenance.connect().await?;
    let created = databases::create(&mut conn, &database, &role).await;
    let closed = conn.close().await;
    created.and(closed)
}

/// The `migrated` step: the tenant opened (which migrates it) and closed, the migrator's latest
/// version stored on the row this transaction holds.
async fn migrate<H: TenantHooks>(
    provisioner: &Provisioner<H>,
    tx: &mut ControlTx,
    row: &TenantRow,
) -> Result<(), AppError> {
    let pool = open(
        provisioner,
        &LifecycleCapability::new(),
        row,
        OpenMode::ReadWrite,
    )
    .await?;
    pool.close().await;
    let Some(version) = latest_version(provisioner.inner.migrator) else {
        return Ok(());
    };
    match rows::store_schema_version(tx, row.info.id, version).await? {
        VersionStored::Stored | VersionStored::Unchanged => Ok(()),
        VersionStored::Busy => Err(row_gone()),
    }
}

/// The highest version among `migrator`'s up migrations.
fn latest_version(migrator: &sqlx::migrate::Migrator) -> Option<i64> {
    migrator
        .iter()
        .filter(|migration| !migration.migration_type.is_down_migration())
        .map(|migration| migration.version)
        .max()
}

/// The `seeded` step: the product's seed (provisioning) or `adopted` (adoption) on a lifecycle
/// pool, closed whatever the hook answered. A hook's error is returned unchanged.
async fn seed<H: TenantHooks>(
    provisioner: &Provisioner<H>,
    row: &TenantRow,
    steps: &Steps<'_, H::Seed>,
) -> Result<Option<Secret>, AppError> {
    let pool = open(
        provisioner,
        &LifecycleCapability::new(),
        row,
        OpenMode::ReadWrite,
    )
    .await?;
    let hooks = &provisioner.inner.hooks;
    let outcome = match steps {
        Steps::Provision { seed } => hooks
            .seed(&row.info, pool.pool(), seed)
            .await
            .map(|outcome| outcome.handover),
        Steps::Adopt => hooks.adopted(&row.info, pool.pool()).await.map(|()| None),
    };
    pool.close().await;
    outcome
}

/// The `active` step: step and state set with one time, the row read back in the transaction.
async fn activate<H: TenantHooks>(
    provisioner: &Provisioner<H>,
    tx: &mut ControlTx,
    row: &TenantRow,
) -> Result<TenantRow, AppError> {
    let id = row.info.id;
    let now = provisioner.now();
    let stepped = rows::set_step(tx, id, ProvisionStep::Active, now).await?;
    let activated = rows::set_state(tx, id, TenantState::Active, now).await?;
    if !(stepped && activated) {
        return Err(row_gone());
    }
    rows::get(tx, id).await?.ok_or_else(row_gone)
}

/// `INTERNAL`: the locked row changed no row or is gone.
fn row_gone() -> AppError {
    AppError::internal("the tenant's locked row was not found")
}

/// Rolls `tx` back and returns `error`; a failed rollback is dropped in favour of `error` (the
/// server ends the transaction with the session either way).
async fn abandon(tx: ControlTx, error: AppError) -> AppError {
    let _ = rollback(tx).await;
    error
}

#[cfg(test)]
mod tests {
    use std::time::SystemTime;

    use sekvent_error::ErrorCode;
    use uuid::Uuid;

    use super::*;
    use crate::control::MAIN_CLUSTER;
    use crate::{ObjectNames, TenantId, TenantInfo, reasons};

    fn names() -> ObjectNames {
        ObjectNames::new("app_t_").expect("a valid prefix")
    }

    fn id() -> TenantId {
        TenantId::from_uuid(Uuid::from_u128(0x5a))
    }

    fn strings(texts: &[&str]) -> Vec<String> {
        texts.iter().map(|text| (*text).to_owned()).collect()
    }

    fn new() -> NewTenant {
        NewTenant {
            id: id(),
            subdomain: "orders".to_owned(),
            display_name: "Orders".to_owned(),
            hosts: strings(&["orders.example.com", "www.orders.example.com"]),
            primary_host: "orders.example.com".to_owned(),
            database: names().database(id()),
            role: names().role(id()),
            role_key: RoleKey {
                key_id: "k1".to_owned(),
                generation: 1,
            },
        }
    }

    fn row(state: TenantState, step: ProvisionStep) -> TenantRow {
        TenantRow {
            info: TenantInfo {
                id: id(),
                subdomain: "orders".to_owned(),
                display_name: "Orders".to_owned(),
                state,
                primary_host: "orders.example.com".to_owned(),
                hosts: strings(&["www.orders.example.com", "orders.example.com"]),
                role_key: RoleKey {
                    key_id: "k1".to_owned(),
                    generation: 1,
                },
            },
            provision_step: step,
            db_cluster: MAIN_CLUSTER.to_owned(),
            db_name: names().database(id()).as_str().to_owned(),
            db_role: names().role(id()).as_str().to_owned(),
            schema_version: None,
            created_at: SystemTime::UNIX_EPOCH,
            updated_at: SystemTime::UNIX_EPOCH,
        }
    }

    fn refused(result: Result<Plan, AppError>) -> bool {
        result.is_err_and(|error| {
            error.code() == ErrorCode::FailedPrecondition
                && error.reason() == Some(reasons::TENANT_STATE)
        })
    }

    #[test]
    fn a_provisioning_row_runs_the_step_after_the_recorded_one() {
        for (recorded, next) in [
            (ProvisionStep::Inserted, ProvisionStep::Role),
            (ProvisionStep::Role, ProvisionStep::Database),
            (ProvisionStep::Database, ProvisionStep::Migrated),
            (ProvisionStep::Migrated, ProvisionStep::Seeded),
            (ProvisionStep::Seeded, ProvisionStep::Active),
            (ProvisionStep::Active, ProvisionStep::Active),
        ] {
            assert_eq!(
                plan(&row(TenantState::Provisioning, recorded), &new()).ok(),
                Some(Plan::Run(next)),
                "after {recorded:?}"
            );
        }
    }

    #[test]
    fn a_matching_active_row_is_done() {
        assert_eq!(
            plan(&row(TenantState::Active, ProvisionStep::Active), &new()).ok(),
            Some(Plan::Done)
        );
    }

    #[test]
    fn suspended_purging_and_purged_rows_are_refused() {
        for state in [
            TenantState::Suspended,
            TenantState::Purging,
            TenantState::Purged,
        ] {
            assert!(
                refused(plan(&row(state, ProvisionStep::Active), &new())),
                "{state:?}"
            );
        }
    }

    #[test]
    fn a_request_that_does_not_match_the_row_is_refused() {
        let changes: [fn(&mut NewTenant); 6] = [
            |new| new.subdomain = "billing".to_owned(),
            |new| new.display_name = "Billing".to_owned(),
            |new| new.primary_host = "www.orders.example.com".to_owned(),
            |new| new.hosts.push("shop.example.com".to_owned()),
            |new| new.hosts.truncate(1),
            |new| {
                new.database = crate::DatabaseName::parse("legacy_orders").expect("a name");
            },
        ];
        for (index, change) in changes.into_iter().enumerate() {
            let mut other = new();
            change(&mut other);
            for state in [TenantState::Provisioning, TenantState::Active] {
                assert!(
                    refused(plan(&row(state, ProvisionStep::Role), &other)),
                    "change {index} in {state:?}"
                );
            }
        }
    }

    #[test]
    fn hosts_match_as_a_set() {
        let mut repeated = new();
        repeated
            .hosts
            .insert(0, "www.orders.example.com".to_owned());
        assert!(matches_request(
            &row(TenantState::Provisioning, ProvisionStep::Inserted),
            &repeated
        ));
    }

    #[test]
    fn the_latest_version_is_the_highest_up_migration() {
        static EMPTY: sqlx::migrate::Migrator = sqlx::migrate::Migrator::DEFAULT;
        assert_eq!(latest_version(&EMPTY), None);
        assert_eq!(row_gone().code(), ErrorCode::Internal);
        assert_eq!(row_gone().reason(), None);
    }
}
