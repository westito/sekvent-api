//! The handle of the lifecycle: what every tenant operation needs, the requests and results of
//! the operations, and the checks and control-row helpers they share.
//!
//! The operations themselves are `impl` blocks of [`Provisioner`] in the files of the lifecycle
//! module: `provision.rs`, `adopt.rs`, `state.rs` (suspend, resume, set hosts), `purge.rs` and
//! `usage.rs` (usage, describe).

use std::fmt;
use std::sync::Arc;
use std::time::SystemTime;

use sekvent_config::Secret;
use sekvent_context::{Clock, SystemClock};
use sekvent_db::to_app_error;
use sekvent_error::{AppError, FieldViolation};
use sqlx::migrate::Migrator;
use sqlx::{Postgres, Transaction};

use super::hooks::{TenantHooks, UsageCounters};
use super::provision::PROVISION_LOCK;
use crate::control::{MAIN_CLUSTER, rows};
use crate::host::{HostError, canonical_host};
use crate::invalidation::{Sender, Sink};
use crate::label::{LabelError, LabelRules};
use crate::tenants::TenantCache;
use crate::{
    Connector, DatabaseName, Maintenance, ObjectNames, Registry, TenantId, TenantRole, TenantRow,
    Tenants,
};

/// The longest display name, in bytes.
pub const DISPLAY_NAME_MAX: usize = 255;

/// A control transaction of a lifecycle call.
pub(crate) type ControlTx = Transaction<'static, Postgres>;

/// The lifecycle of this product's tenants: provision, adopt, suspend, resume, set hosts, purge,
/// usage and describe. Clones share everything. `Debug` prints the type name only.
///
/// Every operation but `usage`, `describe`, `tenant` and `list` takes the registry's lifecycle
/// admission ([`Registry::admit`]) before its first control connection and holds it to its end,
/// then locks the tenant's control row and decides from the locked row. A hook runs on the warm
/// pool of this process's tenant cache for an `active` tenant it holds, and otherwise on a pool
/// opened for the operation that holds one slot of that cache's capacity (see
/// [`ProvisionerBuilder::tenants`]). A committed change is
/// announced (this process's lookups forgotten and its scope retired, the sink told, the change
/// sent to the other processes, the host snapshot reloaded) in a task of its own that the call
/// awaits, so a caller that goes away still announces.
pub struct Provisioner<H> {
    pub(crate) inner: Arc<Inner<H>>,
}

/// What a [`Provisioner`] holds.
pub(crate) struct Inner<H> {
    pub(crate) registry: Registry,
    pub(crate) maintenance: Maintenance,
    pub(crate) connector: Connector,
    pub(crate) names: ObjectNames,
    pub(crate) migrator: &'static Migrator,
    pub(crate) hooks: H,
    /// The key id new tenants' role passwords are derived under, at generation 1.
    pub(crate) role_key_id: String,
    pub(crate) connection_limit: u32,
    /// The most tenants that are not `purged`.
    pub(crate) tenant_cap: u64,
    /// The key of the advisory lock that orders the cap check and the insert of a new row.
    pub(crate) provision_lock: i64,
    pub(crate) labels: LabelRules,
    pub(crate) sender: Sender,
    pub(crate) sink: Arc<dyn Sink>,
    pub(crate) cache: Option<Arc<dyn TenantCache>>,
    pub(crate) clock: Arc<dyn Clock>,
    /// The control database's name: never adopted.
    pub(crate) control_database: String,
}

impl<H> Clone for Provisioner<H> {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

// `Provisioner(..)`
impl<H> fmt::Debug for Provisioner<H> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Provisioner(..)")
    }
}

impl<H: TenantHooks> Provisioner<H> {
    /// A builder. `registry` is the control database's, `maintenance` the cluster's maintenance
    /// connection (its object names are the tenants'), `connector` the one the tenant cache uses,
    /// `migrator` the tenant schema's (never built with `ignore_missing`), `sender` the
    /// invalidation channel's sender, `hooks` the product's.
    pub fn builder(
        registry: Registry,
        maintenance: Maintenance,
        connector: Connector,
        migrator: &'static Migrator,
        sender: Sender,
        hooks: H,
    ) -> ProvisionerBuilder<H> {
        ProvisionerBuilder {
            registry,
            maintenance,
            connector,
            migrator,
            sender,
            hooks,
            role_key_id: None,
            connection_limit: None,
            tenant_cap: None,
            provision_lock: PROVISION_LOCK,
            labels: LabelRules::new(),
            sink: Arc::new(()),
            cache: None,
            clock: Arc::new(SystemClock),
        }
    }

    /// The control registry.
    pub fn registry(&self) -> &Registry {
        &self.inner.registry
    }

    /// The product's hooks.
    pub fn hooks(&self) -> &H {
        &self.inner.hooks
    }

    /// The tenants' object names (the maintenance connection's).
    pub fn names(&self) -> &ObjectNames {
        &self.inner.names
    }

    /// The tenant's row as committed now, in any state. `TENANT_NOT_FOUND` without one. Takes no
    /// lifecycle admission.
    pub async fn tenant(&self, tenant: TenantId) -> Result<TenantRow, AppError> {
        self.inner
            .registry
            .tenant_now(tenant)
            .await?
            .ok_or_else(crate::error::tenant_not_found)
    }

    /// At most `limit` tenants in any state, ordered by id, after `after` (from the first when
    /// `None`). Takes no lifecycle admission; one statement.
    pub async fn list(
        &self,
        after: Option<TenantId>,
        limit: u32,
    ) -> Result<Vec<TenantRow>, AppError> {
        let mut conn = self
            .inner
            .registry
            .pool()
            .acquire()
            .await
            .map_err(to_app_error)?;
        rows::page(&mut conn, after, limit).await
    }

    /// The provisioner's time.
    pub(crate) fn now(&self) -> SystemTime {
        self.inner.clock.now()
    }

    /// A control transaction.
    pub(crate) async fn begin(&self) -> Result<ControlTx, AppError> {
        self.inner
            .registry
            .pool()
            .begin()
            .await
            .map_err(to_app_error)
    }

    /// A control transaction holding the tenant's row lock, and the row read under it.
    /// `TENANT_NOT_FOUND` (the transaction rolled back) without a row.
    pub(crate) async fn begin_locked(
        &self,
        tenant: TenantId,
    ) -> Result<(ControlTx, TenantRow), AppError> {
        let mut tx = self.begin().await?;
        if let Some(row) = rows::lock(&mut tx, tenant).await? {
            Ok((tx, row))
        } else {
            rollback(tx).await?;
            Err(crate::error::tenant_not_found())
        }
    }

    /// The tenant's role and database as the row stores them, checked: the cluster is
    /// `MAIN_CLUSTER`, the stored role is exactly `ObjectNames::role(id)` and the stored database
    /// name passes `DatabaseName::parse`. `INTERNAL` (naming the column, never a value)
    /// otherwise, before any statement: the row was changed by hand.
    pub(crate) fn stored_names(
        &self,
        row: &TenantRow,
    ) -> Result<(TenantRole, DatabaseName), AppError> {
        if row.db_cluster != MAIN_CLUSTER {
            return Err(AppError::internal(
                "tenants.db_cluster names another cluster",
            ));
        }
        let role = self.inner.names.role(row.info.id);
        if row.db_role != role.as_str() {
            return Err(AppError::internal(
                "tenants.db_role is not the tenant's derived role",
            ));
        }
        let database = DatabaseName::parse(&row.db_name)
            .map_err(|_| AppError::internal("tenants.db_name is not a usable database name"))?;
        Ok((role, database))
    }
}

/// Commits `tx`.
pub(crate) async fn commit(tx: ControlTx) -> Result<(), AppError> {
    tx.commit().await.map_err(to_app_error)
}

/// Rolls back `tx`, which wrote nothing that must stay.
pub(crate) async fn rollback(tx: ControlTx) -> Result<(), AppError> {
    tx.rollback().await.map_err(to_app_error)
}

/// The settings of a [`Provisioner`]; `build` refuses a missing required one.
pub struct ProvisionerBuilder<H> {
    registry: Registry,
    maintenance: Maintenance,
    connector: Connector,
    migrator: &'static Migrator,
    sender: Sender,
    hooks: H,
    role_key_id: Option<String>,
    connection_limit: Option<u32>,
    tenant_cap: Option<u64>,
    provision_lock: i64,
    labels: LabelRules,
    sink: Arc<dyn Sink>,
    cache: Option<Arc<dyn TenantCache>>,
    clock: Arc<dyn Clock>,
}

// `ProvisionerBuilder(..)`
impl<H> fmt::Debug for ProvisionerBuilder<H> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ProvisionerBuilder(..)")
    }
}

impl<H: TenantHooks> ProvisionerBuilder<H> {
    /// Required: the key id new tenants' role passwords are derived under (generation 1). It must
    /// be a key of the connector's secrets.
    #[must_use]
    pub fn role_key_id(mut self, key_id: &str) -> Self {
        self.role_key_id = Some(key_id.to_owned());
        self
    }

    /// Required: every tenant role's connection limit, 1 to 2147483647, counted by the server
    /// over every process that serves the tenant.
    #[must_use]
    pub fn connection_limit(mut self, limit: u32) -> Self {
        self.connection_limit = Some(limit);
        self
    }

    /// Required: the most tenants that are not `purged`; at least 1. Provisioning or adopting a
    /// new id at the cap is `TENANT_LIMIT_REACHED`.
    #[must_use]
    pub fn tenant_cap(mut self, cap: u64) -> Self {
        self.tenant_cap = Some(cap);
        self
    }

    /// The key of the control database's transaction-level advisory lock under which a new
    /// tenant row is counted against the cap and inserted, by provisioning and adoption (default
    /// [`PROVISION_LOCK`]). Every process of a deployment must use the same key: processes on
    /// different keys do not exclude each other, and the cap can be passed. A deployment that
    /// already takes a key of its own for this sets it here.
    #[must_use]
    pub fn provision_lock(mut self, key: i64) -> Self {
        self.provision_lock = key;
        self
    }

    /// The product's label rules (default `LabelRules::new()`): a request's subdomain must pass
    /// them.
    #[must_use]
    pub fn labels(mut self, rules: LabelRules) -> Self {
        self.labels = rules;
        self
    }

    /// The tenant cache of this process, whose lookups and scope every committed change drops
    /// and whose host snapshot it reloads. Without one, only the registry's lookups are dropped.
    ///
    /// The lifecycle also opens tenants through it: a hook on an `active` tenant the cache holds
    /// warm runs on that entry's pool (held, so it is not evicted meanwhile), and a pool the
    /// lifecycle opens itself holds one slot of the cache's capacity for its life, so a full
    /// cache answers `TENANT_BUSY` instead of opening connections past the cache's budget.
    #[must_use]
    pub fn tenants<S: Send + Sync + 'static>(mut self, tenants: Tenants<S>) -> Self {
        self.cache = Some(Arc::new(tenants));
        self
    }

    /// The sink told of every committed change on this process (`Sink::change`), the one the
    /// invalidation listener also has (share it through an `Arc`). Default: none.
    #[must_use]
    pub fn sink(mut self, sink: impl Sink) -> Self {
        self.sink = Arc::new(sink);
        self
    }

    /// The clock of the timestamps the control rows get. Default: the system clock.
    #[must_use]
    pub fn clock(mut self, clock: Arc<dyn Clock>) -> Self {
        self.clock = clock;
        self
    }

    /// The provisioner. `INVALID_ARGUMENT` naming the setting for a required one that is missing
    /// or out of range, and for a role key id the connector's secrets do not hold. No I/O.
    pub fn build(self) -> Result<Provisioner<H>, AppError> {
        let role_key_id = self
            .role_key_id
            .ok_or_else(|| AppError::invalid_argument("the provisioner needs role_key_id"))?;
        if !self.connector.secrets().contains(&role_key_id) {
            return Err(AppError::invalid_argument(
                "role_key_id is not a key of the connector's secrets",
            ));
        }
        let connection_limit = self
            .connection_limit
            .filter(|limit| (1..=2_147_483_647).contains(limit))
            .ok_or_else(|| {
                AppError::invalid_argument(
                    "the provisioner needs connection_limit, from 1 to 2147483647",
                )
            })?;
        let tenant_cap = self.tenant_cap.filter(|cap| *cap >= 1).ok_or_else(|| {
            AppError::invalid_argument("the provisioner needs tenant_cap, at least 1")
        })?;
        let options = self.registry.pool().connect_options();
        let control_database = options
            .get_database()
            .unwrap_or_else(|| options.get_username())
            .to_owned();
        let names = self.maintenance.names().clone();
        Ok(Provisioner {
            inner: Arc::new(Inner {
                registry: self.registry,
                maintenance: self.maintenance,
                connector: self.connector,
                names,
                migrator: self.migrator,
                hooks: self.hooks,
                role_key_id,
                connection_limit,
                tenant_cap,
                provision_lock: self.provision_lock,
                labels: self.labels,
                sender: self.sender,
                sink: self.sink,
                cache: self.cache,
                clock: self.clock,
                control_database,
            }),
        })
    }
}

/// A tenant to provision. `Debug` leaves the seed out.
#[derive(Clone)]
pub struct ProvisionRequest<S> {
    /// The tenant's id, chosen by the caller; a repeated call names the same id.
    pub tenant: TenantId,
    /// The tenant's label; it must pass the provisioner's label rules.
    pub subdomain: String,
    /// The name people see: 1 to 255 bytes, not blank, no control character.
    pub display_name: String,
    /// Every host the tenant answers on, the primary included; canonicalised, sorted and
    /// de-duplicated before they are stored.
    pub hosts: Vec<String>,
    /// One of `hosts`.
    pub primary_host: String,
    /// The product's seed payload. It is not stored: a repeated call must send the same.
    pub seed: S,
}

impl<S> fmt::Debug for ProvisionRequest<S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProvisionRequest")
            .field("tenant", &self.tenant)
            .field("subdomain", &self.subdomain)
            .field("display_name", &self.display_name)
            .field("hosts", &self.hosts)
            .field("primary_host", &self.primary_host)
            .finish_non_exhaustive()
    }
}

/// What a provisioning call left. `Debug` tells whether there is a handover, never its value.
pub struct Provisioned {
    /// The tenant's row as committed.
    pub row: TenantRow,
    /// The seed's handover secret: only from the call that ran the seed; a call that resumes
    /// after the seed committed, or finds the tenant active, returns `None`.
    pub handover: Option<Secret>,
}

impl fmt::Debug for Provisioned {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Provisioned")
            .field("row", &self.row)
            .field("handover", &self.handover.is_some())
            .finish()
    }
}

/// An existing database to adopt as a tenant's.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AdoptRequest {
    /// The tenant's id, chosen by the caller.
    pub tenant: TenantId,
    /// The tenant's label; it must pass the provisioner's label rules.
    pub subdomain: String,
    /// The name people see, as in [`ProvisionRequest`].
    pub display_name: String,
    /// The database: `is_adoptable_name`, not a tenant object name, neither the control nor the
    /// maintenance database.
    pub database: String,
    /// Every host the tenant answers on, the primary included.
    pub hosts: Vec<String>,
    /// One of `hosts`.
    pub primary_host: String,
}

/// A tenant's new label and hosts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostsChange {
    /// The tenant's label; it must pass the provisioner's label rules.
    pub subdomain: String,
    /// Every host the tenant answers on, the primary included; replaces the set whole.
    pub hosts: Vec<String>,
    /// One of `hosts`.
    pub primary_host: String,
}

/// A tenant's usage.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct Usage {
    /// The size of the tenant's database in bytes.
    pub database_bytes: u64,
    /// The product's counters.
    pub counters: UsageCounters,
}

impl Usage {
    /// A usage; for a product's own tests.
    pub fn new(database_bytes: u64, counters: UsageCounters) -> Self {
        Self {
            database_bytes,
            counters,
        }
    }
}

/// A request's label and hosts, checked and canonical.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ValidHosts {
    /// The canonical label.
    pub(crate) subdomain: String,
    /// Canonical, byte-sorted, without duplicates, the primary among them.
    pub(crate) hosts: Vec<String>,
    /// The canonical primary host.
    pub(crate) primary_host: String,
}

/// A request's label, display name and hosts, checked.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ValidTenant {
    pub(crate) hosts: ValidHosts,
    /// As given (it is checked, not changed).
    pub(crate) display_name: String,
}

/// The field violations of one request, collected so a caller hears of every problem at once.
#[derive(Default)]
pub(crate) struct Violations(Vec<FieldViolation>);

impl Violations {
    /// Records a violation of `field`.
    pub(crate) fn push(&mut self, field: impl Into<String>, description: impl Into<String>) {
        self.0.push(FieldViolation::new(field, description));
    }

    /// `Ok` without a violation; `INVALID_ARGUMENT` ("the request is invalid") with every one.
    pub(crate) fn finish(self) -> Result<(), AppError> {
        if self.0.is_empty() {
            return Ok(());
        }
        Err(self.0.into_iter().fold(
            AppError::invalid_argument("the request is invalid"),
            AppError::with_violation,
        ))
    }
}

/// What a host refusal says, without the host.
fn host_problem(error: HostError) -> &'static str {
    match error {
        HostError::Empty => "is empty",
        HostError::TooLong => "is too long",
        HostError::NotAscii => "is not ASCII",
        HostError::Malformed => "is not a host",
    }
}

/// What a label refusal says, without the label.
fn label_problem(error: LabelError) -> &'static str {
    match error {
        LabelError::Length => "has a length the rules refuse",
        LabelError::Characters => "may hold letters, digits and hyphens only",
        LabelError::Hyphen => "may not start or end with a hyphen",
        LabelError::Punycode => "may not hold `--` at positions 3 and 4",
        LabelError::Reserved => "is reserved",
    }
}

/// The label and hosts of a request into `violations`: the label by `labels`; every host and the
/// primary canonical; the set not empty, sorted and de-duplicated, the primary among it. A host
/// is named by its position from 0, never by its text. `None` when anything was recorded.
pub(crate) fn collect_hosts(
    violations: &mut Violations,
    labels: &LabelRules,
    subdomain: &str,
    hosts: &[String],
    primary_host: &str,
) -> Option<ValidHosts> {
    let label = match labels.canonical(subdomain) {
        Ok(label) => Some(label),
        Err(error) => {
            violations.push("subdomain", label_problem(error));
            None
        }
    };
    if hosts.is_empty() {
        violations.push("hosts", "at least one host is required");
    }
    let mut canonical = Vec::with_capacity(hosts.len());
    let mut hosts_ok = !hosts.is_empty();
    for (index, host) in hosts.iter().enumerate() {
        match canonical_host(host) {
            Ok(host) => canonical.push(host),
            Err(error) => {
                violations.push(format!("hosts[{index}]"), host_problem(error));
                hosts_ok = false;
            }
        }
    }
    canonical.sort_unstable();
    canonical.dedup();
    let primary = match canonical_host(primary_host) {
        Ok(primary) => Some(primary),
        Err(error) => {
            violations.push("primary_host", host_problem(error));
            None
        }
    };
    let primary = primary.filter(|primary| {
        if !hosts_ok || canonical.binary_search(primary).is_ok() {
            true
        } else {
            violations.push("primary_host", "not one of the hosts");
            false
        }
    });
    match (label, hosts_ok, primary) {
        (Some(subdomain), true, Some(primary_host)) => Some(ValidHosts {
            subdomain,
            hosts: canonical,
            primary_host,
        }),
        _ => None,
    }
}

/// A display name's problem: not 1 to [`DISPLAY_NAME_MAX`] bytes, blank, or holding a control
/// character.
pub(crate) fn display_name_problem(display_name: &str) -> Option<&'static str> {
    if display_name.trim().is_empty() {
        Some("is blank")
    } else if display_name.len() > DISPLAY_NAME_MAX {
        Some("is longer than 255 bytes")
    } else if display_name.chars().any(char::is_control) {
        Some("holds a control character")
    } else {
        None
    }
}

/// [`collect_hosts`] and the display name into `violations`; `None` when anything was recorded.
pub(crate) fn collect_tenant(
    violations: &mut Violations,
    labels: &LabelRules,
    subdomain: &str,
    display_name: &str,
    hosts: &[String],
    primary_host: &str,
) -> Option<ValidTenant> {
    let hosts = collect_hosts(violations, labels, subdomain, hosts, primary_host);
    if let Some(problem) = display_name_problem(display_name) {
        violations.push("display_name", problem);
        return None;
    }
    Some(ValidTenant {
        hosts: hosts?,
        display_name: display_name.to_owned(),
    })
}

/// A host change's label and hosts, checked. `INVALID_ARGUMENT` with every violation.
pub(crate) fn validate_hosts(
    labels: &LabelRules,
    change: &HostsChange,
) -> Result<ValidHosts, AppError> {
    let mut violations = Violations::default();
    let valid = collect_hosts(
        &mut violations,
        labels,
        &change.subdomain,
        &change.hosts,
        &change.primary_host,
    );
    violations.finish()?;
    valid.ok_or_else(|| AppError::internal("valid hosts without a value"))
}

#[cfg(test)]
mod tests {
    use sekvent_error::ErrorCode;

    use super::*;

    fn strings(texts: &[&str]) -> Vec<String> {
        texts.iter().map(|text| (*text).to_owned()).collect()
    }

    fn fields(error: &AppError) -> Vec<String> {
        error
            .field_violations()
            .iter()
            .map(|violation| violation.field.clone())
            .collect()
    }

    fn change(subdomain: &str, hosts: &[&str], primary: &str) -> HostsChange {
        HostsChange {
            subdomain: subdomain.to_owned(),
            hosts: strings(hosts),
            primary_host: primary.to_owned(),
        }
    }

    #[test]
    fn hosts_are_canonicalised_sorted_and_de_duplicated() {
        let valid = validate_hosts(
            &LabelRules::new(),
            &change(
                "Orders",
                &[
                    "www.orders.example.com",
                    "ORDERS.example.com.",
                    "orders.example.com",
                ],
                "Orders.Example.Com",
            ),
        )
        .expect("the change is valid");
        assert_eq!(
            valid,
            ValidHosts {
                subdomain: "orders".to_owned(),
                hosts: strings(&["orders.example.com", "www.orders.example.com"]),
                primary_host: "orders.example.com".to_owned(),
            }
        );
    }

    #[test]
    fn every_problem_is_reported_by_field_and_position() {
        let error = validate_hosts(
            &LabelRules::new(),
            &change("-orders", &["orders.example.com", "not a host"], ""),
        )
        .expect_err("the change is invalid");
        assert_eq!(error.code(), ErrorCode::InvalidArgument);
        assert_eq!(error.reason(), None);
        assert_eq!(fields(&error), ["subdomain", "hosts[1]", "primary_host"]);
        for violation in error.field_violations() {
            assert!(
                !violation.description.contains("host "),
                "{}",
                violation.description
            );
        }
    }

    #[test]
    fn no_host_and_a_foreign_primary_are_refused() {
        let empty = validate_hosts(
            &LabelRules::new(),
            &change("orders", &[], "orders.example.com"),
        )
        .expect_err("no host");
        assert_eq!(fields(&empty), ["hosts"]);
        let foreign = validate_hosts(
            &LabelRules::new(),
            &change("orders", &["orders.example.com"], "billing.example.com"),
        )
        .expect_err("a foreign primary");
        assert_eq!(fields(&foreign), ["primary_host"]);
    }

    #[test]
    fn a_reserved_label_is_refused() {
        let rules = LabelRules::new().with_reserved(|label| label == "www");
        let error = validate_hosts(
            &rules,
            &change("WWW", &["www.example.com"], "www.example.com"),
        )
        .expect_err("reserved");
        assert_eq!(fields(&error), ["subdomain"]);
        assert_eq!(error.field_violations()[0].description, "is reserved");
    }

    #[test]
    fn display_names_are_checked_with_the_hosts() {
        assert_eq!(display_name_problem("Orders Ltd"), None);
        assert_eq!(display_name_problem(" \t"), Some("is blank"));
        assert_eq!(display_name_problem(""), Some("is blank"));
        assert_eq!(
            display_name_problem(&"x".repeat(256)),
            Some("is longer than 255 bytes")
        );
        assert_eq!(display_name_problem(&"x".repeat(255)), None);
        assert_eq!(
            display_name_problem("Orders\nLtd"),
            Some("holds a control character")
        );
        let mut violations = Violations::default();
        let valid = collect_tenant(
            &mut violations,
            &LabelRules::new(),
            "orders",
            "",
            &[],
            "orders.example.com",
        );
        assert_eq!(valid, None);
        let error = violations.finish().expect_err("two problems");
        assert_eq!(fields(&error), ["hosts", "display_name"]);
    }

    #[test]
    fn a_valid_tenant_keeps_its_display_name() {
        let mut violations = Violations::default();
        let valid = collect_tenant(
            &mut violations,
            &LabelRules::new(),
            "orders",
            " Orders Ltd ",
            &strings(&["orders.example.com"]),
            "orders.example.com",
        )
        .expect("valid");
        assert!(violations.finish().is_ok());
        assert_eq!(valid.display_name, " Orders Ltd ");
        assert_eq!(valid.hosts.subdomain, "orders");
    }

    #[test]
    fn requests_and_results_print_no_secret() {
        let request = ProvisionRequest {
            tenant: TenantId::LOCAL,
            subdomain: "orders".to_owned(),
            display_name: "Orders".to_owned(),
            hosts: strings(&["orders.example.com"]),
            primary_host: "orders.example.com".to_owned(),
            seed: "owner@example.com",
        };
        let printed = format!("{request:?}");
        assert!(printed.contains("orders.example.com"), "{printed}");
        assert!(!printed.contains("owner@"), "{printed}");
        assert_eq!(Usage::new(7, UsageCounters::default()).database_bytes, 7);
    }
}
