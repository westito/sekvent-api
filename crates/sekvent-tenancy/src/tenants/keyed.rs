//! The tenant cache: one fixed scope, or a cache of scopes keyed by tenant id.
//!
//! The keyed form is a `KeyedPools<TenantId, Built<S>>`. Its three steps run on the creation's own
//! task: the connector reads the row (through the lookup cache, no permit), refuses a row that is
//! not `active` or whose stored names may not be used, records the role key it connects with and
//! returns the candidates for the tenant's derived role; `init` runs the tenant migrator on the new
//! pool, so as the tenant's role; `build_value` reads the row as committed now, checks it again,
//! refuses it unless it names the database, the role and the role key the pool was opened with,
//! and calls the product's factory.
//!
//! Before any of this, a request's `get` checks the tenant's state with a request's lookup by id:
//! through the lookup cache, joining a read of the same id already running, under the lookup
//! admission.
//!
//! The keyed cache logs a failed creation once with the `Display` of the error's source chain and
//! hands the waiters the error without its source. So every error made here carries one private
//! `OpenFailed` as its source, whose text is fixed words and at most a SQLSTATE; nothing else of
//! a cause is attached, wrapped or formatted. A refusal of the product's factory that carries a
//! reason is marked while it crosses the cache (`mark`, `unmark`), so that its fields reach the
//! caller exactly as written and cannot be mistaken for the framework's.

use std::collections::HashMap;
use std::error::Error as StdError;
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use http::request::Parts;
use sekvent_db::reasons::{KEYED_CAPACITY, KEYED_FACTORY_PANICKED};
use sekvent_db::{ConnectCandidates, KeyedPools, KeyedPoolsBuilder, KeyedStats};
use sekvent_error::{AppError, ErrorCode};
use sqlx::PgPool;
use sqlx::migrate::Migrator;

use super::lease::{Built, TenantLease};
use crate::control::{MAIN_CLUSTER, TenantRow};
use crate::error::serving;
use crate::select::{Selection, is_bypass};
use crate::{
    Connector, DatabaseName, ErrorDomains, HostSnapshot, ObjectNames, Registry, RoleKey, Selector,
    SelectorRules, TenantId, TenantInfo, TenantRole, reasons,
};

/// The future of [`ScopeFactory::build`].
pub type ScopeFuture<'a, S> = Pin<Box<dyn Future<Output = Result<S, AppError>> + Send + 'a>>;

/// Builds a tenant's scope on its migrated pool. May take seconds; may fail.
pub trait ScopeFactory: Send + Sync + 'static {
    /// What a request to the tenant gets: the tenant's pool and whatever the product builds on it.
    type Scope: Send + Sync + 'static;

    /// Builds the scope of `info`'s tenant on `pool`, which is migrated and connects as the
    /// tenant's role.
    ///
    /// An error with a reason of the product's own reaches the caller with its code, message,
    /// reason, domain, metadata, retry hint and field violations, whatever its reason text is; an
    /// error without a reason becomes `TENANT_UNAVAILABLE`. A failure is never cached. The error's
    /// source is dropped and not logged by the cache: a factory that wants its cause in the log
    /// writes its own event, and puts no connection detail in it.
    ///
    /// This is the seam for a tenant that one process at a time may own: the factory takes the
    /// product's lease there and refuses with a reason of its own when another process holds it.
    fn build<'a>(&'a self, info: &'a TenantInfo, pool: PgPool) -> ScopeFuture<'a, Self::Scope>;
}

/// The tenants this process serves: one fixed scope, or a cache of scopes. Clones share
/// everything.
pub struct Tenants<S> {
    form: Arc<Form<S>>,
}

/// The two forms behind one type.
enum Form<S> {
    /// One scope for `TenantId::LOCAL`.
    Fixed {
        built: Arc<Built<S>>,
        bypass: &'static [&'static str],
    },
    /// A cache of scopes.
    Keyed(Box<Cache<S>>),
}

/// The keyed form.
struct Cache<S> {
    pools: KeyedPools<TenantId, Built<S>>,
    /// `KeyedPools::stats`, taken where `S: Send + Sync` holds, so `Debug` needs no bound on `S`.
    stats_of: fn(&KeyedPools<TenantId, Built<S>>) -> KeyedStats,
    registry: Registry,
    rules: SelectorRules,
    snapshot: HostSnapshot,
    migrator: &'static Migrator,
    domains: ErrorDomains,
    /// The role key each tenant's latest creation connects with.
    opened: OpenedKeys,
}

/// The fixed form's stats.
const FIXED_STATS: TenantsStats = TenantsStats {
    capacity: 1,
    warm: 1,
    in_use: 0,
    creating: 0,
    retiring: 0,
    creations: 0,
    create_failures: 0,
    evictions: 0,
    capacity_refusals: 0,
};

impl<S> Form<S> {
    fn stats(&self) -> TenantsStats {
        match self {
            Self::Fixed { .. } => FIXED_STATS,
            Self::Keyed(cache) => {
                let stats = (cache.stats_of)(&cache.pools);
                TenantsStats {
                    capacity: stats.capacity,
                    warm: stats.warm,
                    in_use: stats.in_use,
                    creating: stats.creating,
                    retiring: stats.retiring,
                    creations: stats.creations,
                    create_failures: stats.create_failures,
                    evictions: stats.evictions,
                    capacity_refusals: stats.capacity_refusals,
                }
            }
        }
    }
}

impl<S> Clone for Tenants<S> {
    fn clone(&self) -> Self {
        Self {
            form: Arc::clone(&self.form),
        }
    }
}

// `Tenants { stats: TenantsStats { .. }, .. }`
impl<S> fmt::Debug for Tenants<S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Tenants")
            .field("stats", &self.form.stats())
            .finish_non_exhaustive()
    }
}

impl<S: Send + Sync + 'static> Tenants<S> {
    /// One scope for `TenantId::LOCAL`; no registry, no snapshot, no cache, no bypass prefix:
    /// `fixed_with_bypass(scope, &[])`.
    pub fn fixed(scope: S) -> Self {
        Self::fixed_with_bypass(scope, &[])
    }

    /// `fixed`, with RPC path prefixes that belong to no tenant: a request under one of them gets
    /// an empty scope, as with `SelectorRules::bypass` on the keyed form (the same comparison: a
    /// plain string prefix of the request path as received, never percent-decoded; `"/"`
    /// bypasses every path, so end a prefix with `/`; an empty prefix matches nothing).
    pub fn fixed_with_bypass(scope: S, bypass: &'static [&'static str]) -> Self {
        let built = Arc::new(Built {
            info: TenantInfo::local(),
            scope,
        });
        Self {
            form: Arc::new(Form::Fixed { built, bypass }),
        }
    }

    /// The keyed form. `migrator` is the tenant schema's.
    pub fn builder<F>(
        registry: Registry,
        connector: Connector,
        names: ObjectNames,
        rules: SelectorRules,
        migrator: &'static Migrator,
        factory: F,
    ) -> TenantsBuilder<S>
    where
        F: ScopeFactory<Scope = S>,
    {
        let factory = Arc::new(factory);
        let opened = OpenedKeys::default();
        let connect = {
            let registry = registry.clone();
            let names = names.clone();
            let opened = opened.clone();
            move |id: TenantId| {
                let registry = registry.clone();
                let names = names.clone();
                let connector = connector.clone();
                let opened = opened.clone();
                async move { candidates(&registry, &connector, &names, &opened, id).await }
            }
        };
        let build = {
            let registry = registry.clone();
            let opened = opened.clone();
            move |id: TenantId, pool: PgPool| {
                let registry = registry.clone();
                let names = names.clone();
                let opened = opened.clone();
                let factory = Arc::clone(&factory);
                async move { build_scope(&registry, &names, &opened, &*factory, id, pool).await }
            }
        };
        let pools = <KeyedPools<TenantId>>::builder(connect)
            .init(move |_id: TenantId, pool: PgPool| async move {
                migrator
                    .run(&pool)
                    .await
                    .map_err(|error| open_failed(Stage::Migrate, sqlstate_in(&error)))
            })
            .build_value(build);
        TenantsBuilder {
            pools,
            registry,
            rules,
            migrator,
            domains: ErrorDomains::new(),
            opened,
        }
    }

    /// The tenant `request` names, after the lookup, the state check and the header and origin
    /// checks; `None` for a bypass path. Never opens a tenant. Fixed: `None` for a bypass path,
    /// otherwise `Some(TenantId::LOCAL)`; nothing else of the request is read.
    pub async fn select(&self, request: &Parts) -> Result<Option<TenantId>, AppError> {
        match &*self.form {
            Form::Fixed { bypass, .. } => {
                Ok((!is_bypass(bypass, request.uri.path())).then_some(TenantId::LOCAL))
            }
            Form::Keyed(cache) => cache
                .select(request)
                .await
                .map_err(|error| serving::in_domain(error, cache.domains)),
        }
    }

    /// `select`, then `get`; `None` for a bypass path in both forms.
    pub async fn resolve(&self, request: &Parts) -> Result<Option<TenantLease<S>>, AppError> {
        match self.select(request).await? {
            Some(id) => self.get(id).await.map(Some),
            None => Ok(None),
        }
    }

    /// The tenant's scope, opened when the cache does not hold it. Checks the row's state first,
    /// with a request's lookup by id (`Registry::lookup`): through the lookup cache, joining a
    /// read of the same id that is already running, otherwise under a lookup permit taken
    /// without waiting. So many concurrent `get`s of one id whose lookup is not cached cause one
    /// control read.
    ///
    /// | The open ends with | The caller gets |
    /// |---|---|
    /// | a refusal of the factory with a reason | that error, exactly as the factory wrote it |
    /// | the row not `active` (either read) | the state's serving reason |
    /// | a full cache, or no free lookup permit | `TENANT_BUSY`, retried after a second |
    /// | a panic in the creation, the factory's included | `INTERNAL` |
    /// | anything else | `TENANT_UNAVAILABLE` |
    ///
    /// "Anything else" includes a row that changed while the tenant was being opened, so that it
    /// no longer names the database, the role or the role key the new pool connected with:
    /// nothing is cached, and the next `get` opens afresh from the row as it is then.
    pub async fn get(&self, id: TenantId) -> Result<TenantLease<S>, AppError> {
        self.open(id).await.map_err(Refused::into_error)
    }

    /// `get`, telling the cache's and the lookup's own busy answer from every other failure, a
    /// refusal of the product's factory included whatever its reason: the walks and the sweep
    /// count the one and record the other.
    pub(super) async fn open(&self, id: TenantId) -> Result<TenantLease<S>, Refused> {
        match &*self.form {
            Form::Fixed { built, .. } => {
                if id == TenantId::LOCAL {
                    Ok(TenantLease::fixed(Arc::clone(built)))
                } else {
                    Err(Refused::Failed(serving::in_domain(
                        serving::not_here(),
                        ErrorDomains::new(),
                    )))
                }
            }
            Form::Keyed(cache) => cache.open(id).await,
        }
    }

    /// The scope if it is warm. Never opens, never reads the row, does not count as a use. It
    /// can return the scope of a tenant whose row changed a moment ago.
    pub fn peek(&self, id: TenantId) -> Option<TenantLease<S>> {
        match &*self.form {
            Form::Fixed { built, .. } => {
                (id == TenantId::LOCAL).then(|| TenantLease::fixed(Arc::clone(built)))
            }
            Form::Keyed(cache) => cache.pools.peek(&id).map(TenantLease::keyed),
        }
    }

    /// Forgets the tenant's lookups and retires its warm entry or its creation; whether the cache
    /// held one. Fixed: `false`. A creation it retires runs on and its result is discarded.
    pub async fn invalidate(&self, id: TenantId) -> bool {
        match &*self.form {
            Form::Fixed { .. } => false,
            Form::Keyed(cache) => {
                cache.registry.forget(id);
                cache.opened.forget(id);
                cache.pools.invalidate(&id).await
            }
        }
    }

    /// Evicts every scope nothing holds that was last used at least `idle` ago; how many.
    /// Fixed: 0.
    ///
    /// With [`stats`](Self::stats) this is the seam for a product that sheds scopes by a
    /// measure of its own (memory, say) rather than by their number.
    pub async fn evict_idle_older_than(&self, idle: Duration) -> usize {
        match &*self.form {
            Form::Fixed { .. } => 0,
            Form::Keyed(cache) => cache.pools.evict_idle_older_than(idle).await,
        }
    }

    /// `hosts::refresh_hosts` on this cache's snapshot and registry. Fixed: `Ok(vec![])`, no I/O.
    pub async fn refresh_hosts(&self) -> Result<Vec<TenantInfo>, AppError> {
        match &*self.form {
            Form::Fixed { .. } => Ok(Vec::new()),
            Form::Keyed(cache) => {
                crate::hosts::refresh_hosts(&cache.snapshot, &cache.registry).await
            }
        }
    }

    /// Gauges and counters. Fixed: `warm` 1, `capacity` 1, the rest 0.
    pub fn stats(&self) -> TenantsStats {
        self.form.stats()
    }

    /// The host snapshot; `None` for the fixed form.
    pub fn hosts(&self) -> Option<&HostSnapshot> {
        match &*self.form {
            Form::Fixed { .. } => None,
            Form::Keyed(cache) => Some(&cache.snapshot),
        }
    }

    /// The registry; `None` for the fixed form.
    pub fn registry(&self) -> Option<&Registry> {
        match &*self.form {
            Form::Fixed { .. } => None,
            Form::Keyed(cache) => Some(&cache.registry),
        }
    }

    /// The error domains this cache answers with.
    pub fn domains(&self) -> ErrorDomains {
        match &*self.form {
            Form::Fixed { .. } => ErrorDomains::new(),
            Form::Keyed(cache) => cache.domains,
        }
    }

    /// Closes the cache as `KeyedPools::close` does. Fixed: nothing.
    pub async fn close(&self) {
        if let Form::Keyed(cache) = &*self.form {
            cache.pools.close().await;
            cache.opened.clear();
        }
    }

    /// The active tenants in id order; the fixed form's one tenant. Only the list read fails.
    pub(super) async fn active_ids(&self) -> Result<Vec<TenantId>, AppError> {
        match &*self.form {
            Form::Fixed { .. } => Ok(vec![TenantId::LOCAL]),
            Form::Keyed(cache) => {
                let active = cache
                    .registry
                    .active()
                    .await
                    .map_err(|error| serving::in_domain(error, cache.domains))?;
                let mut ids: Vec<TenantId> = active.into_iter().map(|info| info.id).collect();
                ids.sort_unstable();
                Ok(ids)
            }
        }
    }

    /// The highest version among the tenant migrator's up migrations; `None` for the fixed form
    /// and for a migrator without one.
    pub(super) fn latest_version(&self) -> Option<i64> {
        match &*self.form {
            Form::Fixed { .. } => None,
            Form::Keyed(cache) => cache
                .migrator
                .iter()
                .filter(|migration| !migration.migration_type.is_down_migration())
                .map(|migration| migration.version)
                .max(),
        }
    }

    /// The keyed form's cache.
    #[cfg(feature = "runtime")]
    pub(super) fn cache(&self) -> Option<&KeyedPools<TenantId, Built<S>>> {
        match &*self.form {
            Form::Fixed { .. } => None,
            Form::Keyed(cache) => Some(&cache.pools),
        }
    }
}

impl<S> Cache<S> {
    /// The keyed `select`, before the domain is added.
    async fn select(&self, request: &Parts) -> Result<Option<TenantId>, AppError> {
        let selected = match self.rules.select(request)? {
            Selection::Bypass => return Ok(None),
            Selection::Tenant(selected) => selected,
        };
        let row = served(self.registry.lookup(&selected.selector).await)?;
        selected.admit(&row.info)?;
        Ok(Some(row.info.id))
    }
}

impl<S: Send + Sync + 'static> Cache<S> {
    /// The keyed `open`: the state check through a request's lookup by id, then the cache.
    ///
    /// The registry's lookup never carries an error of the product's factory, so a
    /// `TENANT_BUSY` it answers is the lookup admission's own (no free permit, or a caller that
    /// joined twice without an answer).
    async fn open(&self, id: TenantId) -> Result<TenantLease<S>, Refused> {
        let read = self.registry.lookup(&Selector::Id(id)).await;
        let busy = matches!(&read, Err(error) if error.reason() == Some(reasons::TENANT_BUSY));
        if let Err(error) = served(read) {
            let error = serving::in_domain(error, self.domains);
            return Err(if busy {
                Refused::Busy(error)
            } else {
                Refused::Failed(error)
            });
        }
        match self.pools.get(&id).await {
            Ok(reference) => Ok(TenantLease::keyed(reference)),
            Err(error) => Err(classify(error, self.domains)),
        }
    }
}

/// Why an open failed, and whose answer it is.
#[derive(Debug)]
pub(super) enum Refused {
    /// The cache's or the lookup's own `TENANT_BUSY`: the cache was full, or no lookup permit
    /// was free. Never a refusal of the product's factory.
    Busy(AppError),
    /// Every other failure, a refusal of the product's factory included whatever its reason.
    Failed(AppError),
}

impl Refused {
    /// The caller's error.
    pub(super) fn into_error(self) -> AppError {
        match self {
            Self::Busy(error) | Self::Failed(error) => error,
        }
    }
}

/// A serving read of a row: no row is `TENANT_NOT_HERE`, a row that is not `active` its state's
/// refusal, a failed read with a serving reason that error, any other failed read
/// `TENANT_UNAVAILABLE` with the read's error as its source. No domain.
fn served(read: Result<Option<TenantRow>, AppError>) -> Result<TenantRow, AppError> {
    let row = match read {
        Ok(Some(row)) => row,
        Ok(None) => return Err(serving::not_here()),
        Err(error) if serving::is_serving(&error) => return Err(error),
        Err(error) => return Err(serving::unavailable().with_source(error)),
    };
    match serving::for_state(row.info.state) {
        Some(refusal) => Err(refusal),
        None => Ok(row),
    }
}

/// The caller's answer for an error of `KeyedPools::get`. A marked error is the product's,
/// returned exactly and never busy; an unmarked one is the framework's or this crate's own, and
/// only the cache's `KEYED_CAPACITY` is busy.
fn classify(error: AppError, domains: ErrorDomains) -> Refused {
    let error = match unmark(error) {
        Ok(product) => return Refused::Failed(product),
        Err(error) => error,
    };
    if serving::is_serving(&error) {
        return Refused::Failed(serving::in_domain(error, domains));
    }
    match error.reason() {
        Some(KEYED_CAPACITY) => Refused::Busy(serving::in_domain(serving::busy(), domains)),
        Some(KEYED_FACTORY_PANICKED) => {
            Refused::Failed(AppError::new(ErrorCode::Internal, "internal error"))
        }
        _ => Refused::Failed(serving::in_domain(serving::unavailable(), domains)),
    }
}

/// The metadata key that marks an error as the scope factory's own while it crosses the keyed
/// cache.
const FACTORY_MARK: &str = "sekvent.tenancy.scope_factory";

/// `error` without its source, marked: metadata [`FACTORY_MARK`] is `-`, or `+` followed by the
/// value the product's error already had under that key. Its source is
/// `OpenFailed { stage: Build }`.
fn mark(error: &AppError) -> AppError {
    let mut wire = error.to_wire();
    let value = match wire.metadata.remove(FACTORY_MARK) {
        Some(own) => format!("+{own}"),
        None => "-".to_owned(),
    };
    wire.metadata.insert(FACTORY_MARK.to_owned(), value);
    AppError::from_wire(wire).with_source(OpenFailed::at(Stage::Build))
}

/// `Ok(the product's error exactly as the factory returned it, minus its source)` when `error`
/// is marked (the key is removed, or restored to the product's own value); `Err(error)` when it
/// is not.
fn unmark(error: AppError) -> Result<AppError, AppError> {
    if !error.metadata().contains_key(FACTORY_MARK) {
        return Err(error);
    }
    let mut wire = error.to_wire();
    if let Some(own) = wire
        .metadata
        .remove(FACTORY_MARK)
        .and_then(|value| value.strip_prefix('+').map(str::to_owned))
    {
        wire.metadata.insert(FACTORY_MARK.to_owned(), own);
    }
    Ok(AppError::from_wire(wire))
}

/// Where an open failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Stage {
    /// Reading the control row.
    ControlRead,
    /// The stored row's cluster, role or database name.
    Row,
    /// Deriving the connect candidates.
    Candidates,
    /// Migrating the tenant's database.
    Migrate,
    /// The product's build.
    Build,
}

impl Stage {
    const fn as_str(self) -> &'static str {
        match self {
            Self::ControlRead => "control_read",
            Self::Row => "row",
            Self::Candidates => "candidates",
            Self::Migrate => "migrate",
            Self::Build => "build",
        }
    }
}

/// Why an open failed, for the keyed cache's log event. Its text is fixed words and, at most, a
/// five-character SQLSTATE: never a name, a host, an option, a credential or a driver's text.
#[derive(Debug)]
struct OpenFailed {
    stage: Stage,
    sqlstate: Option<String>,
}

impl OpenFailed {
    const fn at(stage: Stage) -> Self {
        Self {
            stage,
            sqlstate: None,
        }
    }
}

impl fmt::Display for OpenFailed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "opening the tenant failed: {}", self.stage.as_str())?;
        if let Some(code) = &self.sqlstate {
            write!(f, " (sqlstate {code})")?;
        }
        Ok(())
    }
}

impl StdError for OpenFailed {}

/// `TENANT_UNAVAILABLE` with nothing but `OpenFailed` as its source.
fn open_failed(stage: Stage, sqlstate: Option<String>) -> AppError {
    serving::unavailable().with_source(OpenFailed { stage, sqlstate })
}

/// The first printable SQLSTATE of a `sqlx::Error` in `error`'s source chain, `error` itself
/// included. Formats nothing.
fn sqlstate_in(error: &(dyn StdError + 'static)) -> Option<String> {
    let mut next = Some(error);
    while let Some(link) = next {
        let code = link
            .downcast_ref::<sqlx::Error>()
            .and_then(sekvent_db::sqlstate)
            .filter(|code| is_sqlstate(code));
        if let Some(code) = code {
            return Some(code.into_owned());
        }
        next = link.source();
    }
    None
}

/// Five ASCII letters or digits.
fn is_sqlstate(code: &str) -> bool {
    code.len() == 5 && code.bytes().all(|byte| byte.is_ascii_alphanumeric())
}

/// The names a tenant is opened with, or `None` when the stored row may not be used: `db_cluster`
/// is not `MAIN_CLUSTER`, `db_role` is not exactly `names.role(row.info.id).as_str()`, or
/// `db_name` does not pass `DatabaseName::parse` (an adopted database name is allowed; the role
/// is always the derived one).
fn stored_names(names: &ObjectNames, row: &TenantRow) -> Option<(TenantRole, DatabaseName)> {
    if row.db_cluster != MAIN_CLUSTER {
        return None;
    }
    let role = names.role(row.info.id);
    if row.db_role != role.as_str() {
        return None;
    }
    let database = DatabaseName::parse(&row.db_name).ok()?;
    Some((role, database))
}

/// A row a creation may open: read, `active`, with usable stored names. Every refusal is this
/// crate's own error: the state's refusal or `TENANT_UNAVAILABLE` with a stage.
fn openable(
    read: Result<Option<TenantRow>, AppError>,
    names: &ObjectNames,
) -> Result<(TenantRow, TenantRole, DatabaseName), AppError> {
    let row = match read {
        Ok(Some(row)) => row,
        Ok(None) => return Err(serving::not_here()),
        Err(error) => return Err(open_failed(Stage::ControlRead, sqlstate_in(&error))),
    };
    if let Some(refusal) = serving::for_state(row.info.state) {
        return Err(refusal);
    }
    let Some((role, database)) = stored_names(names, &row) else {
        return Err(open_failed(Stage::Row, None));
    };
    Ok((row, role, database))
}

/// The role key the latest creation of each tenant connects with: recorded by the connector
/// step, read by the `build_value` step. Clones share the map.
///
/// The keyed cache runs at most one creation per tenant that can still be published; a creation
/// `invalidate` retired runs on beside the next one and its result is discarded. So the record
/// of a tenant must be its current creation's. Each tenant has an epoch that `invalidate` raises
/// (dropping the record); a connector step takes the epoch before it reads the row and records
/// only while the epoch is unchanged, so a retired creation whose connector step ends after the
/// next creation's never overwrites that creation's record. A retired creation that reaches
/// `build_value` finds no record, or the next creation's, and what it builds is discarded either
/// way. The map holds one small entry per tenant opened or invalidated since the cache was
/// built, and none after `close`.
#[derive(Clone, Default)]
struct OpenedKeys {
    records: Arc<Mutex<HashMap<TenantId, Opening>>>,
}

/// A tenant's epoch and the key its current creation connects with.
#[derive(Default)]
struct Opening {
    epoch: u64,
    key: Option<RoleKey>,
}

impl OpenedKeys {
    /// The locked map; a poisoned lock is still used, no section leaves it half-written.
    fn records(&self) -> MutexGuard<'_, HashMap<TenantId, Opening>> {
        self.records.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The tenant's epoch, taken by a connector step before it reads the row.
    fn epoch(&self, id: TenantId) -> u64 {
        self.records().get(&id).map_or(0, |opening| opening.epoch)
    }

    /// Records the key the tenant's current creation connects with, unless `invalidate` forgot
    /// the tenant since `epoch` was taken.
    fn record(&self, id: TenantId, epoch: u64, key: RoleKey) {
        let mut records = self.records();
        let opening = records.entry(id).or_default();
        if opening.epoch == epoch {
            opening.key = Some(key);
        }
    }

    /// The key recorded for the tenant.
    fn get(&self, id: TenantId) -> Option<RoleKey> {
        self.records()
            .get(&id)
            .and_then(|opening| opening.key.clone())
    }

    /// Drops the tenant's record and raises its epoch.
    fn forget(&self, id: TenantId) {
        let mut records = self.records();
        let opening = records.entry(id).or_default();
        opening.epoch += 1;
        opening.key = None;
    }

    /// Drops every entry.
    fn clear(&self) {
        self.records().clear();
    }
}

/// What a creation's pool was opened with: the database and the role from the pool's own
/// connect options, the role key recorded by the creation's connector step.
struct Opened {
    database: String,
    role: String,
    role_key: RoleKey,
}

impl Opened {
    /// What `pool` was opened with; `None` without a database in its options or without a
    /// recorded key.
    fn of(pool: &PgPool, role_key: Option<RoleKey>) -> Option<Self> {
        let options = pool.connect_options();
        Some(Self {
            database: options.get_database()?.to_owned(),
            role: options.get_username().to_owned(),
            role_key: role_key?,
        })
    }

    /// Whether `row` names exactly the database, the role and the role key (id and generation)
    /// the pool was opened with.
    fn matches(&self, row: &TenantRow) -> bool {
        row.db_name == self.database
            && row.db_role == self.role
            && row.info.role_key == self.role_key
    }
}

/// The connector step: the row, then the candidates of the derived role; the role key they
/// derive from is recorded for the `build_value` step.
async fn candidates(
    registry: &Registry,
    connector: &Connector,
    names: &ObjectNames,
    opened: &OpenedKeys,
    id: TenantId,
) -> Result<ConnectCandidates, AppError> {
    let epoch = opened.epoch(id);
    let (row, role, database) = openable(registry.tenant(id).await, names)?;
    let key = row.info.role_key;
    let candidates = connector
        .candidates(id, &role, &database, std::slice::from_ref(&key))
        .map_err(|_| open_failed(Stage::Candidates, None))?;
    opened.record(id, epoch, key);
    Ok(candidates)
}

/// The `build_value` step: the row as committed now, checked again and compared with what the
/// pool was opened with, then the product's build. A row that left `active` is its state's
/// refusal; a row that names another database, role or role key than the pool's is
/// `TENANT_UNAVAILABLE` with the stage `row`, and the factory is not called.
async fn build_scope<F: ScopeFactory>(
    registry: &Registry,
    names: &ObjectNames,
    opened: &OpenedKeys,
    factory: &F,
    id: TenantId,
    pool: PgPool,
) -> Result<Built<F::Scope>, AppError> {
    let (row, _, _) = openable(registry.tenant_now(id).await, names)?;
    let consistent = Opened::of(&pool, opened.get(id)).is_some_and(|with| with.matches(&row));
    if !consistent {
        return Err(open_failed(Stage::Row, None));
    }
    let info = row.info;
    let built = factory.build(&info, pool).await;
    match built {
        Ok(scope) => Ok(Built { info, scope }),
        Err(error) if error.reason().is_some() => Err(mark(&error)),
        Err(_) => Err(open_failed(Stage::Build, None)),
    }
}

/// Builds the keyed form of `Tenants`. Defaults are `KeyedPools`' (capacity 64, 5 connections,
/// 5 s acquire, 30 s create, idle eviction after 10 min, reaping every 30 s, connection idle
/// 10 min, connection lifetime 30 min) and `ErrorDomains::new()`.
pub struct TenantsBuilder<S> {
    pools: KeyedPoolsBuilder<TenantId, Built<S>>,
    registry: Registry,
    rules: SelectorRules,
    migrator: &'static Migrator,
    domains: ErrorDomains,
    opened: OpenedKeys,
}

// type name only
impl<S> fmt::Debug for TenantsBuilder<S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("TenantsBuilder(..)")
    }
}

impl<S: Send + Sync + 'static> TenantsBuilder<S> {
    /// The domains the cache answers with.
    #[must_use]
    pub fn domains(self, domains: ErrorDomains) -> Self {
        Self { domains, ..self }
    }

    /// The most tenants held at once, warm and creating together.
    #[must_use]
    pub fn capacity(self, tenants: usize) -> Self {
        Self {
            pools: self.pools.capacity(tenants),
            ..self
        }
    }

    /// The connections of each tenant's pool.
    #[must_use]
    pub fn max_connections(self, max: u32) -> Self {
        Self {
            pools: self.pools.max_connections(max),
            ..self
        }
    }

    /// How long a tenant pool's acquire waits.
    #[must_use]
    pub fn acquire_timeout(self, timeout: Duration) -> Self {
        Self {
            pools: self.pools.acquire_timeout(timeout),
            ..self
        }
    }

    /// The one deadline of a creation: connect, migration and build. A product with a slow
    /// build raises it.
    #[must_use]
    pub fn create_timeout(self, timeout: Duration) -> Self {
        Self {
            pools: self.pools.create_timeout(timeout),
            ..self
        }
    }

    /// Evict a scope nothing holds once it was idle this long; `None` never.
    #[must_use]
    pub fn evict_idle_after(self, idle: Option<Duration>) -> Self {
        Self {
            pools: self.pools.evict_idle_after(idle),
            ..self
        }
    }

    /// How often the runtime unit reaps idle scopes.
    #[must_use]
    pub fn reap_interval(self, interval: Duration) -> Self {
        Self {
            pools: self.pools.reap_interval(interval),
            ..self
        }
    }

    /// How long a tenant pool keeps an idle connection; `None` for ever.
    #[must_use]
    pub fn connection_idle_timeout(self, idle: Option<Duration>) -> Self {
        Self {
            pools: self.pools.connection_idle_timeout(idle),
            ..self
        }
    }

    /// How long a tenant pool keeps a connection at most; `None` for ever.
    #[must_use]
    pub fn connection_max_lifetime(self, lifetime: Option<Duration>) -> Self {
        Self {
            pools: self.pools.connection_max_lifetime(lifetime),
            ..self
        }
    }

    /// `INVALID_ARGUMENT`, naming the setting, for the values `KeyedPoolsBuilder::build` refuses.
    ///
    /// A scope is dropped before its pool is closed, so a scope that owns tasks ends them in its
    /// `Drop`. A callback on eviction would be a disposal hook of `KeyedPoolsBuilder`, passed
    /// through here.
    pub fn build(self) -> Result<Tenants<S>, AppError> {
        let pools = self.pools.build()?;
        let cache = Cache {
            pools,
            stats_of: KeyedPools::<TenantId, Built<S>>::stats,
            registry: self.registry,
            rules: self.rules,
            snapshot: HostSnapshot::new(),
            migrator: self.migrator,
            domains: self.domains,
            opened: self.opened,
        };
        Ok(Tenants {
            form: Arc::new(Form::Keyed(Box::new(cache))),
        })
    }
}

/// A snapshot of the tenant cache. The counters only grow; the rest are gauges.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct TenantsStats {
    /// The most tenants (warm and being opened) the cache holds.
    pub capacity: usize,
    /// Tenants with a scope.
    pub warm: usize,
    /// Warm tenants with at least one lease, or a waiter about to receive one.
    pub in_use: usize,
    /// Tenants being opened.
    pub creating: usize,
    /// Scopes out of the cache that a lease still holds or that are being closed. Not bounded by
    /// `capacity`.
    pub retiring: usize,
    /// Opens started.
    pub creations: u64,
    /// Opens that ended without a scope.
    pub create_failures: u64,
    /// Tenants the cache removed on its own, for capacity or idleness.
    pub evictions: u64,
    /// `get`s refused because the cache was full.
    pub capacity_refusals: u64,
}

#[cfg(test)]
mod tests {
    use std::borrow::Cow;
    use std::time::SystemTime;

    use sekvent_config::Secret;
    use sekvent_context::ManualClock;
    use sekvent_db::reasons::{
        DB_CREDENTIALS_REJECTED, DB_PERMISSION_DENIED, KEYED_CLOSED, KEYED_CREATE_TIMEOUT,
    };
    use sekvent_error::FieldViolation;
    use sqlx::error::{DatabaseError, ErrorKind};
    use sqlx::postgres::{PgConnectOptions, PgPoolOptions};

    use super::*;
    use crate::control::ProvisionStep;
    use crate::{RoleSecrets, TenantState};

    static EMPTY: Migrator = Migrator::DEFAULT;

    /// A scope type that derives nothing.
    struct Marker(u32);

    /// A factory the unit tests never reach.
    struct Unreached;

    impl ScopeFactory for Unreached {
        type Scope = Marker;

        fn build<'a>(&'a self, _info: &'a TenantInfo, _pool: PgPool) -> ScopeFuture<'a, Marker> {
            Box::pin(std::future::ready(Ok(Marker(0))))
        }
    }

    fn request(path: &str) -> Parts {
        http::Request::builder()
            .uri(path)
            .body(())
            .expect("a request head")
            .into_parts()
            .0
    }

    fn other_tenant() -> TenantId {
        TenantId::parse("0b9c6f1e-2d4a-4c8b-9e3f-5a6b7c8d9e0f").expect("a tenant id")
    }

    fn names() -> ObjectNames {
        ObjectNames::new("app_t_").expect("a valid prefix")
    }

    /// The source chain below `error`, joined with `": "`.
    fn source_chain(error: &AppError) -> String {
        let mut parts = Vec::new();
        let mut next = StdError::source(error);
        while let Some(link) = next {
            parts.push(link.to_string());
            next = link.source();
        }
        parts.join(": ")
    }

    /// A registry over a pool of three that never opened a connection.
    fn lazy_registry() -> (Registry, PgPool) {
        let options = PgConnectOptions::new_without_pgpass()
            .host("127.0.0.1")
            .port(1)
            .username("orders")
            .database("orders");
        let pool = PgPoolOptions::new()
            .max_connections(3)
            .connect_lazy_with(options);
        let clock = Arc::new(ManualClock::new(SystemTime::UNIX_EPOCH));
        let registry = Registry::new(pool.clone(), clock).expect("a pool of three is enough");
        (registry, pool)
    }

    fn keyed_builder(registry: Registry) -> TenantsBuilder<Marker> {
        let secrets = RoleSecrets::parse(
            &Secret::new("k1:an example master secret for the unit tests"),
            "orders.role.v1",
        )
        .expect("the test secrets parse");
        let connector = Connector::new(
            PgConnectOptions::new_without_pgpass()
                .host("127.0.0.1")
                .port(1),
            secrets,
        );
        Tenants::builder(
            registry,
            connector,
            names(),
            SelectorRules::new(),
            &EMPTY,
            Unreached,
        )
    }

    #[tokio::test]
    async fn the_fixed_form_serves_the_local_tenant_alone() {
        let tenants = Tenants::fixed(Marker(5));
        let unary = request("/billing.v1.Billing/Charge");

        assert_eq!(
            tenants.select(&unary).await.ok(),
            Some(Some(TenantId::LOCAL))
        );
        let resolved = tenants.resolve(&unary).await.expect("resolved");
        assert_eq!(resolved.map(|lease| lease.0), Some(5));
        let lease = tenants.get(TenantId::LOCAL).await.expect("the local scope");
        assert_eq!(lease.0, 5);
        assert_eq!(lease.info(), &TenantInfo::local());

        let refused = tenants.get(other_tenant()).await.expect_err("not here");
        assert_eq!(refused.code(), ErrorCode::NotFound);
        assert_eq!(refused.reason(), Some(reasons::TENANT_NOT_HERE));
        assert_eq!(refused.domain(), Some(ErrorDomains::DEFAULT_SERVING));

        assert!(
            tenants
                .peek(TenantId::LOCAL)
                .is_some_and(|lease| lease.0 == 5)
        );
        assert!(tenants.peek(other_tenant()).is_none());
        assert!(!tenants.invalidate(TenantId::LOCAL).await);
        assert_eq!(tenants.evict_idle_older_than(Duration::ZERO).await, 0);
        assert_eq!(tenants.refresh_hosts().await.ok(), Some(Vec::new()));
        assert!(tenants.hosts().is_none());
        assert!(tenants.registry().is_none());
        assert_eq!(tenants.domains(), ErrorDomains::new());
        assert_eq!(tenants.latest_version(), None);
        assert_eq!(tenants.active_ids().await.ok(), Some(vec![TenantId::LOCAL]));

        tenants.close().await;
        let after = tenants
            .clone()
            .get(TenantId::LOCAL)
            .await
            .expect("still served");
        assert!(
            std::ptr::eq(&raw const *after, &raw const *lease),
            "clones share the one scope"
        );
    }

    #[tokio::test]
    async fn the_fixed_form_honours_its_bypass_prefixes_only() {
        let tenants = Tenants::fixed_with_bypass(Marker(1), &["/billing.health.v1.Health/"]);
        let health = request("/billing.health.v1.Health/Check");
        let unary = request("/billing.v1.Billing/Charge");
        assert_eq!(tenants.select(&health).await.ok(), Some(None));
        assert!(tenants.resolve(&health).await.expect("resolved").is_none());
        assert_eq!(
            tenants.select(&unary).await.ok(),
            Some(Some(TenantId::LOCAL))
        );
        assert!(tenants.resolve(&unary).await.expect("resolved").is_some());

        let plain = Tenants::fixed(Marker(2));
        assert_eq!(
            plain.select(&health).await.ok(),
            Some(Some(TenantId::LOCAL))
        );
        assert!(plain.resolve(&health).await.expect("resolved").is_some());

        let empty = Tenants::fixed_with_bypass(Marker(3), &[""]);
        assert_eq!(empty.select(&unary).await.ok(), Some(Some(TenantId::LOCAL)));
        assert_eq!(
            empty.select(&health).await.ok(),
            Some(Some(TenantId::LOCAL))
        );
    }

    #[test]
    fn the_fixed_form_reports_one_warm_tenant() {
        let tenants = Tenants::fixed(Marker(0));
        let stats = tenants.stats();
        assert_eq!(stats, FIXED_STATS);
        assert_eq!((stats.capacity, stats.warm, stats.in_use), (1, 1, 0));
        assert_eq!((stats.creating, stats.retiring), (0, 0));
        assert_eq!((stats.creations, stats.create_failures), (0, 0));
        assert_eq!((stats.evictions, stats.capacity_refusals), (0, 0));
        assert_eq!(
            format!("{tenants:?}"),
            "Tenants { stats: TenantsStats { capacity: 1, warm: 1, in_use: 0, creating: 0, \
             retiring: 0, creations: 0, create_failures: 0, evictions: 0, \
             capacity_refusals: 0 }, .. }"
        );
    }

    fn product_error() -> AppError {
        AppError::failed_precondition("served by another process")
            .with_reason("OWNED_ELSEWHERE")
            .with_domain("billing.v1")
            .with_metadata("owner", "process-7")
            .with_metadata("region", "north")
            .with_retry_after(Duration::from_secs(3))
            .with_violation(FieldViolation::new("tenant", "held elsewhere").with_reason("HELD"))
            .with_source(std::io::Error::other(
                "postgres://app_t_owner:hunter2@db.example.com/orders",
            ))
    }

    /// What the waiters of a creation receive: the error through its wire form.
    fn crossed(error: &AppError) -> AppError {
        AppError::from_wire(error.to_wire())
    }

    /// The error of a failure that is not the cache's own busy answer.
    #[track_caller]
    fn failed(refused: Refused) -> AppError {
        match refused {
            Refused::Failed(error) => error,
            Refused::Busy(error) => panic!("counted as busy: {:?}", error.reason()),
        }
    }

    #[test]
    fn a_marked_error_comes_back_as_the_product_wrote_it() {
        let marked = mark(&product_error());
        assert_eq!(source_chain(&marked), "opening the tenant failed: build");
        assert_eq!(
            marked.metadata().get(FACTORY_MARK).map(String::as_str),
            Some("-")
        );

        let back = unmark(crossed(&marked)).expect("the error is marked");
        assert_eq!(back.to_wire(), product_error().to_wire());
        assert_eq!(back.code(), ErrorCode::FailedPrecondition);
        assert_eq!(back.message(), "served by another process");
        assert_eq!(back.reason(), Some("OWNED_ELSEWHERE"));
        assert_eq!(back.domain(), Some("billing.v1"));
        assert_eq!(back.metadata().len(), 2);
        assert!(!back.metadata().contains_key(FACTORY_MARK));
        assert_eq!(back.retry_after(), Some(Duration::from_secs(3)));
        assert_eq!(back.field_violations().len(), 1);
        assert!(StdError::source(&back).is_none());
    }

    #[test]
    fn a_product_value_under_the_mark_key_is_restored() {
        let own = || product_error().with_metadata(FACTORY_MARK, "theirs");
        let marked = mark(&own());
        assert_eq!(
            marked.metadata().get(FACTORY_MARK).map(String::as_str),
            Some("+theirs")
        );
        let back = unmark(crossed(&marked)).expect("the error is marked");
        assert_eq!(back.to_wire(), own().to_wire());
    }

    #[test]
    fn an_unmarked_error_is_returned_unchanged() {
        let plain = serving::busy().with_metadata("hint", "later");
        let wire = plain.to_wire();
        let returned = unmark(plain).expect_err("not marked");
        assert_eq!(returned.to_wire(), wire);
    }

    #[test]
    fn a_marked_refusal_keeps_a_reason_that_equals_a_framework_reason() {
        let domains = ErrorDomains::new().with_serving("orders.tenants.v1");
        for reason in [
            KEYED_CAPACITY,
            KEYED_FACTORY_PANICKED,
            DB_CREDENTIALS_REJECTED,
            reasons::TENANT_BUSY,
        ] {
            let product = || AppError::unavailable("the product refused").with_reason(reason);
            let answer = failed(classify(crossed(&mark(&product())), domains));
            assert_eq!(answer.to_wire(), product().to_wire(), "{reason}");
            assert_eq!(answer.domain(), None, "{reason}");
            assert!(StdError::source(&answer).is_none(), "{reason}");
        }
    }

    #[test]
    fn unmarked_errors_are_classified_row_by_row() {
        let domains = ErrorDomains::new().with_serving("orders.tenants.v1");

        let suspended = failed(classify(crossed(&serving::suspended()), domains));
        assert_eq!(suspended.reason(), Some(reasons::TENANT_SUSPENDED));
        assert_eq!(suspended.domain(), Some("orders.tenants.v1"));
        let own = failed(classify(
            crossed(&open_failed(Stage::Migrate, None)),
            domains,
        ));
        assert_eq!(own.reason(), Some(reasons::TENANT_UNAVAILABLE));
        assert_eq!(own.domain(), Some("orders.tenants.v1"));

        let full = AppError::resource_exhausted("the cache is full").with_reason(KEYED_CAPACITY);
        let Refused::Busy(busy) = classify(full, domains) else {
            panic!("the cache's own capacity refusal is busy");
        };
        assert_eq!(busy.code(), ErrorCode::Unavailable);
        assert_eq!(busy.reason(), Some(reasons::TENANT_BUSY));
        assert_eq!(busy.domain(), Some("orders.tenants.v1"));
        assert_eq!(busy.retry_after(), Some(Duration::from_secs(1)));

        let panicked = AppError::new(ErrorCode::Internal, "a factory panicked")
            .with_reason(KEYED_FACTORY_PANICKED);
        let internal = failed(classify(panicked, domains));
        assert_eq!(internal.code(), ErrorCode::Internal);
        assert_eq!(internal.message(), "internal error");
        assert_eq!(internal.reason(), None);
        assert_eq!(internal.domain(), None);

        for reason in [
            None,
            Some(KEYED_CREATE_TIMEOUT),
            Some(KEYED_CLOSED),
            Some(DB_CREDENTIALS_REJECTED),
            Some(DB_PERMISSION_DENIED),
            Some("SOMETHING_ELSE"),
        ] {
            let error = AppError::new(ErrorCode::Internal, "database error");
            let error = match reason {
                Some(reason) => error.with_reason(reason),
                None => error,
            };
            let answer = failed(classify(error, domains));
            assert_eq!(answer.code(), ErrorCode::Unavailable, "{reason:?}");
            assert_eq!(
                answer.reason(),
                Some(reasons::TENANT_UNAVAILABLE),
                "{reason:?}"
            );
            assert_eq!(answer.domain(), Some("orders.tenants.v1"), "{reason:?}");
            assert_eq!(
                answer.message(),
                "this tenant is unavailable, try again later"
            );
            assert!(answer.metadata().is_empty());
        }
    }

    #[test]
    fn open_failed_prints_its_stage_and_sqlstate_only() {
        for (stage, text) in [
            (Stage::ControlRead, "control_read"),
            (Stage::Row, "row"),
            (Stage::Candidates, "candidates"),
            (Stage::Migrate, "migrate"),
            (Stage::Build, "build"),
        ] {
            assert_eq!(
                OpenFailed::at(stage).to_string(),
                format!("opening the tenant failed: {text}")
            );
            let with_code = OpenFailed {
                stage,
                sqlstate: Some("42P07".to_owned()),
            };
            assert_eq!(
                with_code.to_string(),
                format!("opening the tenant failed: {text} (sqlstate 42P07)")
            );
            assert!(with_code.source().is_none());
        }
        let error = open_failed(Stage::Row, None);
        assert_eq!(error.reason(), Some(reasons::TENANT_UNAVAILABLE));
        assert_eq!(source_chain(&error), "opening the tenant failed: row");
    }

    #[derive(Debug)]
    struct FakeDbError(&'static str);

    impl fmt::Display for FakeDbError {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("driver text naming user app_t_owner")
        }
    }

    impl StdError for FakeDbError {}

    impl DatabaseError for FakeDbError {
        fn message(&self) -> &'static str {
            "driver text naming user app_t_owner"
        }
        fn code(&self) -> Option<Cow<'_, str>> {
            Some(Cow::Borrowed(self.0))
        }
        fn as_error(&self) -> &(dyn StdError + Send + Sync + 'static) {
            self
        }
        fn as_error_mut(&mut self) -> &mut (dyn StdError + Send + Sync + 'static) {
            self
        }
        fn into_error(self: Box<Self>) -> Box<dyn StdError + Send + Sync + 'static> {
            self
        }
        fn kind(&self) -> ErrorKind {
            ErrorKind::Other
        }
    }

    fn database_error(code: &'static str) -> AppError {
        sekvent_db::to_app_error(sqlx::Error::Database(Box::new(FakeDbError(code))))
    }

    #[test]
    fn the_sqlstate_comes_from_a_sqlx_error_in_the_chain_only() {
        assert_eq!(sqlstate_in(&AppError::unavailable("no source")), None);
        let io = AppError::unavailable("io").with_source(std::io::Error::other("42P07"));
        assert_eq!(sqlstate_in(&io), None);
        assert_eq!(sqlstate_in(&sqlx::Error::PoolTimedOut), None);
        assert_eq!(
            sqlstate_in(&database_error("42P07")).as_deref(),
            Some("42P07")
        );
        let wrapped = serving::unavailable().with_source(database_error("40001"));
        assert_eq!(sqlstate_in(&wrapped).as_deref(), Some("40001"));
        assert_eq!(sqlstate_in(&database_error("42P0'")), None);
        assert_eq!(sqlstate_in(&database_error("42P071")), None);
    }

    fn stored_row(names: &ObjectNames, id: TenantId) -> TenantRow {
        TenantRow {
            info: TenantInfo {
                id,
                subdomain: "alpha".to_owned(),
                display_name: "Alpha".to_owned(),
                state: TenantState::Active,
                primary_host: "alpha.example.com".to_owned(),
                hosts: vec!["alpha.example.com".to_owned()],
                role_key: RoleKey {
                    key_id: "k1".to_owned(),
                    generation: 1,
                },
            },
            provision_step: ProvisionStep::Active,
            db_cluster: MAIN_CLUSTER.to_owned(),
            db_name: names.database(id).as_str().to_owned(),
            db_role: names.role(id).as_str().to_owned(),
            schema_version: None,
            created_at: SystemTime::UNIX_EPOCH,
            updated_at: SystemTime::UNIX_EPOCH,
        }
    }

    #[test]
    fn the_stored_row_must_name_the_derived_role_on_the_main_cluster() {
        let names = names();
        let id = TenantId::parse("4f1c2b3a-5d6e-4f70-8a9b-0c1d2e3f4a5b").expect("a tenant id");
        let row = stored_row(&names, id);

        let (role, database) = stored_names(&names, &row).expect("the derived names pass");
        assert_eq!(role.as_str(), names.role(id).as_str());
        assert_eq!(database.as_str(), names.database(id).as_str());

        let adopted = TenantRow {
            db_name: "orders_legacy".to_owned(),
            ..row.clone()
        };
        let (role, database) = stored_names(&names, &adopted).expect("an adopted name passes");
        assert_eq!(role.as_str(), names.role(id).as_str());
        assert_eq!(database.as_str(), "orders_legacy");

        for refused in [
            TenantRow {
                db_role: names.role(other_tenant()).as_str().to_owned(),
                ..row.clone()
            },
            TenantRow {
                db_role: "app_t_orders".to_owned(),
                ..row.clone()
            },
            TenantRow {
                db_name: "Orders-Database".to_owned(),
                ..row.clone()
            },
            TenantRow {
                db_name: "postgres".to_owned(),
                ..row.clone()
            },
            TenantRow {
                db_cluster: "replica".to_owned(),
                ..row.clone()
            },
        ] {
            assert!(stored_names(&names, &refused).is_none());
        }
    }

    #[test]
    fn a_creation_refuses_a_row_it_may_not_open() {
        let names = names();
        let id = TenantId::parse("4f1c2b3a-5d6e-4f70-8a9b-0c1d2e3f4a5b").expect("a tenant id");
        let row = stored_row(&names, id);

        assert!(openable(Ok(Some(row.clone())), &names).is_ok());
        let missing = openable(Ok(None), &names).expect_err("no row");
        assert_eq!(missing.reason(), Some(reasons::TENANT_NOT_HERE));
        assert!(StdError::source(&missing).is_none());

        let mut suspended = row.clone();
        suspended.info.state = TenantState::Suspended;
        let refusal = openable(Ok(Some(suspended)), &names).expect_err("suspended");
        assert_eq!(refusal.reason(), Some(reasons::TENANT_SUSPENDED));
        assert!(StdError::source(&refusal).is_none());

        let failed = openable(Err(database_error("57P01")), &names).expect_err("a failed read");
        assert_eq!(failed.reason(), Some(reasons::TENANT_UNAVAILABLE));
        assert_eq!(
            source_chain(&failed),
            "opening the tenant failed: control_read (sqlstate 57P01)"
        );

        let foreign = TenantRow {
            db_cluster: "replica".to_owned(),
            ..row
        };
        let refused = openable(Ok(Some(foreign)), &names).expect_err("another cluster");
        assert_eq!(source_chain(&refused), "opening the tenant failed: row");
    }

    /// A pool that never connected, opened with `role` on `database`.
    fn opened_pool(role: &str, database: &str) -> PgPool {
        PgPoolOptions::new().max_connections(1).connect_lazy_with(
            PgConnectOptions::new_without_pgpass()
                .host("127.0.0.1")
                .port(1)
                .username(role)
                .database(database),
        )
    }

    #[tokio::test]
    async fn a_row_must_name_what_the_pool_was_opened_with() {
        let names = names();
        let id = TenantId::parse("4f1c2b3a-5d6e-4f70-8a9b-0c1d2e3f4a5b").expect("a tenant id");
        let row = stored_row(&names, id);
        let pool = opened_pool(&row.db_role, &row.db_name);
        let key = row.info.role_key.clone();

        let opened = Opened::of(&pool, Some(key.clone())).expect("a database and a key");
        assert!(opened.matches(&row));

        let mut changed_key = row.clone();
        changed_key.info.role_key.key_id = "k9".to_owned();
        let mut changed_generation = row.clone();
        changed_generation.info.role_key.generation = 2;
        let changed_database = TenantRow {
            db_name: "orders_moved".to_owned(),
            ..row.clone()
        };
        let changed_role = TenantRow {
            db_role: names.role(other_tenant()).as_str().to_owned(),
            ..row.clone()
        };
        for changed in [
            changed_key,
            changed_generation,
            changed_database,
            changed_role,
        ] {
            assert!(!opened.matches(&changed));
        }

        assert!(Opened::of(&pool, None).is_none(), "no recorded key");
        let elsewhere = opened_pool(&row.db_role, "orders_moved");
        let moved = Opened::of(&elsewhere, Some(key)).expect("a database and a key");
        assert!(!moved.matches(&row));
    }

    #[test]
    fn the_recorded_key_is_the_latest_and_forget_and_clear_drop_it() {
        let opened = OpenedKeys::default();
        let shared = opened.clone();
        let first = RoleKey {
            key_id: "k1".to_owned(),
            generation: 1,
        };
        let second = RoleKey {
            key_id: "k1".to_owned(),
            generation: 2,
        };
        assert_eq!(opened.get(other_tenant()), None);
        let epoch = opened.epoch(other_tenant());
        opened.record(other_tenant(), epoch, first);
        shared.record(other_tenant(), epoch, second.clone());
        assert_eq!(opened.get(other_tenant()), Some(second.clone()));
        opened.forget(other_tenant());
        assert_eq!(shared.get(other_tenant()), None);
        let local = shared.epoch(TenantId::LOCAL);
        shared.record(TenantId::LOCAL, local, second);
        opened.clear();
        assert_eq!(shared.get(TenantId::LOCAL), None);
    }

    #[test]
    fn a_retired_creation_never_overwrites_the_next_creations_record() {
        let opened = OpenedKeys::default();
        let old = RoleKey {
            key_id: "k1".to_owned(),
            generation: 1,
        };
        let new = RoleKey {
            key_id: "k2".to_owned(),
            generation: 1,
        };
        let retired = opened.epoch(other_tenant());
        opened.forget(other_tenant());
        let current = opened.epoch(other_tenant());
        assert_ne!(retired, current);
        opened.record(other_tenant(), current, new.clone());
        opened.record(other_tenant(), retired, old.clone());
        assert_eq!(opened.get(other_tenant()), Some(new));

        opened.forget(other_tenant());
        opened.record(other_tenant(), current, old);
        assert_eq!(
            opened.get(other_tenant()),
            None,
            "a forgotten epoch records nothing"
        );
        assert_eq!(
            opened.get(TenantId::LOCAL),
            None,
            "other tenants are untouched"
        );
    }

    #[test]
    fn a_serving_read_maps_each_outcome() {
        let names = names();
        let id = TenantId::parse("4f1c2b3a-5d6e-4f70-8a9b-0c1d2e3f4a5b").expect("a tenant id");
        let row = stored_row(&names, id);
        assert_eq!(served(Ok(Some(row.clone()))).ok(), Some(row.clone()));
        let missing = served(Ok(None)).expect_err("no row");
        assert_eq!(missing.reason(), Some(reasons::TENANT_NOT_HERE));
        let busy = served(Err(serving::busy())).expect_err("busy");
        assert_eq!(busy.reason(), Some(reasons::TENANT_BUSY));
        let failed = served(Err(database_error("57P01"))).expect_err("a failed read");
        assert_eq!(failed.reason(), Some(reasons::TENANT_UNAVAILABLE));
        let mut purging = row;
        purging.info.state = TenantState::Purging;
        let gone = served(Ok(Some(purging))).expect_err("purging");
        assert_eq!(gone.reason(), Some(reasons::TENANT_NOT_HERE));
    }

    #[tokio::test]
    async fn the_builder_refuses_what_the_keyed_cache_refuses() {
        type Setting = fn(TenantsBuilder<Marker>) -> TenantsBuilder<Marker>;
        let refused: [(Setting, &str); 7] = [
            (|builder| builder.capacity(0), "capacity"),
            (|builder| builder.max_connections(0), "max_connections"),
            (
                |builder| builder.acquire_timeout(Duration::ZERO),
                "acquire_timeout",
            ),
            (
                |builder| builder.create_timeout(Duration::ZERO),
                "create_timeout",
            ),
            (
                |builder| builder.reap_interval(Duration::ZERO),
                "reap_interval",
            ),
            (
                |builder| builder.evict_idle_after(Some(Duration::ZERO)),
                "evict_idle_after",
            ),
            (
                |builder| builder.connection_idle_timeout(Some(Duration::ZERO)),
                "connection_idle_timeout",
            ),
        ];
        for (setting, name) in refused {
            let (registry, _pool) = lazy_registry();
            let error = setting(keyed_builder(registry))
                .build()
                .expect_err("a refused setting");
            assert_eq!(error.code(), ErrorCode::InvalidArgument, "{name}");
            assert!(error.message().contains(name), "{}", error.message());
        }
        let (registry, _pool) = lazy_registry();
        let error = keyed_builder(registry)
            .connection_max_lifetime(Some(Duration::ZERO))
            .build()
            .expect_err("a refused lifetime");
        assert!(error.message().contains("connection_max_lifetime"));
    }

    #[tokio::test]
    async fn the_keyed_form_is_built_with_the_settings_given() {
        let (registry, _pool) = lazy_registry();
        let builder = keyed_builder(registry)
            .domains(ErrorDomains::new().with_serving("orders.tenants.v1"))
            .capacity(3)
            .max_connections(2)
            .acquire_timeout(Duration::from_secs(2))
            .create_timeout(Duration::from_secs(9))
            .evict_idle_after(None)
            .reap_interval(Duration::from_secs(5))
            .connection_idle_timeout(None)
            .connection_max_lifetime(None);
        assert_eq!(format!("{builder:?}"), "TenantsBuilder(..)");
        let tenants = builder.build().expect("valid settings");

        assert_eq!(tenants.stats().capacity, 3);
        assert_eq!(tenants.stats().warm, 0);
        assert_eq!(tenants.domains().serving, "orders.tenants.v1");
        assert!(tenants.hosts().is_some_and(HostSnapshot::is_empty));
        assert!(tenants.registry().is_some());
        assert_eq!(tenants.latest_version(), None);
        assert!(tenants.peek(other_tenant()).is_none());
        assert!(!tenants.invalidate(other_tenant()).await);
        assert_eq!(tenants.evict_idle_older_than(Duration::ZERO).await, 0);
        let debug = format!("{tenants:?}");
        assert!(
            debug.starts_with("Tenants { stats: TenantsStats { capacity: 3, warm: 0,"),
            "{debug}"
        );
        assert!(!debug.contains("127.0.0.1"), "{debug}");

        let defaults = keyed_builder(lazy_registry().0)
            .build()
            .expect("the defaults");
        assert_eq!(defaults.stats().capacity, 64);
        assert_eq!(defaults.domains(), ErrorDomains::new());
    }

    #[tokio::test]
    async fn a_keyed_get_without_the_control_database_is_unavailable() {
        let (registry, pool) = lazy_registry();
        let tenants = keyed_builder(registry).build().expect("the defaults");
        pool.close().await;

        let error = tenants
            .get(other_tenant())
            .await
            .expect_err("no control read");
        assert_eq!(error.code(), ErrorCode::Unavailable);
        assert_eq!(error.reason(), Some(reasons::TENANT_UNAVAILABLE));
        assert_eq!(error.domain(), Some(ErrorDomains::DEFAULT_SERVING));
        assert_eq!(tenants.stats().creations, 0);

        tenants.close().await;
        assert!(tenants.peek(other_tenant()).is_none());
    }
}
