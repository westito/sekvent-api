//! The example billing service: every tenant on a database and a role of its
//! own, served from one process.
//!
//! Two Apps run on one runtime, each behind a listener of its own:
//!
//! - **The public App** serves `billing.invoices.v1.Invoices` over gRPC and
//!   gRPC-Web. Its scope resolver is a [`TenantResolver`]: a request to the
//!   shared API host names its tenant in the [`TENANT_HEADER`]; a request to
//!   a tenant's own host (`acme.example.com`) names it by that host. The
//!   handler reads the tenant's pool from the lease in its scope.
//! - **The admin App** serves the product's own `billing.admin.v1.TenantAdmin`
//!   to an operator service. Its one end-user authenticator verifies admin
//!   tokens (`EdDSA`, 60 s, minted for one RPC and at most one tenant), and
//!   every RPC body runs in the admin guard and calls the
//!   [`Provisioner`].
//!
//! Beside them run the tenant cache's unit (idle eviction, closing on
//! shutdown) and the invalidation listener's (other processes' changes to a
//! tenant drop this process's scope of it), both in
//! [`Stage::Infrastructure`], so they are up before a listener accepts.
//!
//! The service reads its own keys, all under `BILLING_` ([`BillingConfig`]);
//! sekvent-tenancy reads none. The two Apps' `SEKVENT_COMPONENT_*` settings
//! are fixed in code (the public component is served without end-user
//! authentication, the admin component with bearer tokens), so the two Apps
//! never see each other's keys.
//!
//! ```no_run
//! # async fn run() -> Result<(), Box<dyn std::error::Error>> {
//! use sekvent::prelude::*;
//!
//! let config: billing_svc::BillingConfig = sekvent::config::from_env()?;
//! let billing = billing_svc::Billing::build(&config).await?;
//! let servers = billing.bind(config.public_addr, config.admin_addr).await?;
//! billing.runtime(servers, Runtime::builder()).build()?.run().await?;
//! # Ok(())
//! # }
//! ```

#![forbid(unsafe_code)]

use std::error::Error;
use std::net::SocketAddr;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use billing::{
    BillingFactory, BillingHooks, BillingScope, CONTROL_MIGRATOR, InvoicesService, TENANT_MIGRATOR,
    TenantAdminService,
};
use billing_api::{ADMIN_SERVICE, InvoicesHandle, TenantAdminHandle};
use http::HeaderName;
use sekvent::config::MapSource;
use sekvent::context::{Clock, SystemClock};
use sekvent::db::to_app_error;
use sekvent::prelude::*;
use sekvent::tenancy::admin::{self, AdminAudit, AdminAuthenticator, NoExtra, Verifier};
use sekvent::tenancy::invalidation::{Channel, Listener, Sender};
use sekvent::tenancy::lifecycle::Provisioner;
use sekvent::tenancy::{
    Connector, ErrorDomains, Maintenance, ObjectNames, Registry, RoleSecrets, SelectorRules,
    TenantResolver, Tenants, control,
};
use sqlx::ConnectOptions;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};

/// The header that names the tenant on the shared API host.
pub const TENANT_HEADER: &str = "x-billing-tenant";

/// The domains of the tenancy reasons this service answers with.
pub const DOMAINS: ErrorDomains = ErrorDomains::new()
    .with_serving("billing.serving.v1")
    .with_admin("billing.admin.v1");

/// The context string of the tenant role passwords: part of every
/// derivation, so another product's master secret derives other passwords.
pub const ROLE_SECRET_CONTEXT: &str = "billing";

/// The admin token subjects the admin App accepts: `operator:<name>`.
pub const SUBJECT_PREFIX: &str = "operator:";

/// The unit of the admin App (the public App's is `components`).
pub const ADMIN_COMPONENTS_UNIT: &str = "admin-components";
/// The unit of the tenant cache.
pub const TENANTS_UNIT: &str = "tenants";
/// The unit of the invalidation listener.
pub const LISTENER_UNIT: &str = "tenant-changes";
/// The unit of the public listener.
pub const PUBLIC_UNIT: &str = "public-grpc";
/// The unit of the admin listener.
pub const ADMIN_UNIT: &str = "admin-grpc";

/// The settings of the billing service, read from `BILLING_*` keys.
///
/// `Debug` prints no URL and no secret.
#[derive(Debug, EnvConfig)]
#[config(prefix = "BILLING_")]
pub struct BillingConfig {
    /// Where the public App listens.
    #[config(default = "127.0.0.1:8080")]
    pub public_addr: SocketAddr,
    /// Where the admin App listens; reachable by the operator service only.
    #[config(default = "127.0.0.1:8081")]
    pub admin_addr: SocketAddr,
    /// The control database, as the role that owns it (credentials
    /// included).
    pub control_url: Secret,
    /// The control pool's size, at least 3.
    #[config(default = "8")]
    pub control_max_connections: u32,
    /// The maintenance role (`LOGIN CREATEDB CREATEROLE`, no superuser) on
    /// its maintenance database. Its host, port and TLS settings are also
    /// where tenant databases are opened.
    pub maintenance_url: Secret,
    /// The master secrets the tenant role passwords are derived from,
    /// `key_id:secret[,key_id:secret…]`.
    pub role_secrets: Secret,
    /// The key id new tenants' passwords are derived under.
    pub role_key_id: String,
    /// The prefix of every tenant role and database name.
    #[config(default = "app_t_")]
    pub object_prefix: String,
    /// The invalidation channel on the control database.
    #[config(default = "billing_changes")]
    pub channel: String,
    /// The shared API host, where the tenant is named in [`TENANT_HEADER`].
    #[config(default = "api.example.com")]
    pub api_host: String,
    /// The most tenants this process holds open at once.
    #[config(default = "64")]
    pub tenant_capacity: usize,
    /// The most tenants that are not purged.
    #[config(default = "1000")]
    pub tenant_cap: u64,
    /// The connection limit of each tenant role, over every process.
    #[config(default = "20")]
    pub tenant_connection_limit: u32,
    /// The issuer of admin tokens (the operator service).
    pub admin_issuer: String,
    /// This instance's name in admin tokens.
    pub admin_audience: String,
    /// The operator service's public keys, `key_id:<base64url Ed25519 key>[,…]`.
    pub admin_keys: String,
}

/// The service, built and not yet serving: its two Apps, the tenant cache
/// and the invalidation listener.
pub struct Billing {
    public: App,
    admin: App,
    tenants: Tenants<BillingScope>,
    listener: Listener<BillingScope>,
}

/// The two listeners, bound.
pub struct Servers {
    /// The public listener.
    pub public: Server,
    /// The admin listener.
    pub admin: Server,
}

/// `url` as connect options without statement logging. `INVALID_ARGUMENT`
/// naming `key`, never the value.
fn connect_options(url: &Secret, key: &str) -> Result<PgConnectOptions, AppError> {
    PgConnectOptions::from_str(url.expose())
        .map(ConnectOptions::disable_statement_logging)
        .map_err(|_| AppError::invalid_argument(format!("{key} is not a Postgres URL")))
}

/// Whether an admin token's subject is an operator's.
fn is_operator(subject: &str) -> bool {
    subject
        .strip_prefix(SUBJECT_PREFIX)
        .is_some_and(|name| !name.is_empty())
}

/// The public App: the invoices component served to every tenant's end
/// users, its scope resolved from the request.
fn public_app(tenants: &Tenants<BillingScope>) -> Result<App, Box<dyn Error>> {
    let source = MapSource::new()
        .with("SEKVENT_COMPONENT_INVOICES_SERVE", "grpc")
        .with("SEKVENT_COMPONENT_INVOICES_SERVE_AUTH", "none");
    let mut builder = App::builder(&source);
    builder.scope_resolver(TenantResolver::new(tenants.clone(), &[]))?;
    InvoicesHandle::install(&mut builder, |_deps| Ok(InvoicesService))?;
    Ok(builder.build()?)
}

/// The admin App: the tenant admin component, for admin tokens only.
fn admin_app(
    authenticator: AdminAuthenticator,
    component: TenantAdminService,
) -> Result<App, Box<dyn Error>> {
    let source = MapSource::new()
        .with("SEKVENT_COMPONENT_TENANT_ADMIN_SERVE", "grpc")
        .with("SEKVENT_COMPONENT_TENANT_ADMIN_SERVE_AUTH", "bearer");
    let mut builder = App::builder(&source);
    builder.end_user_authenticator(authenticator)?;
    TenantAdminHandle::install(&mut builder, move |_deps| Ok(component))?;
    Ok(builder.build()?)
}

impl Billing {
    /// Connects to the control database, migrates and checks it, and builds
    /// the tenant cache, the provisioner, the admin surface and both Apps.
    /// Opens no tenant.
    pub async fn build(config: &BillingConfig) -> Result<Self, Box<dyn Error>> {
        let clock: Arc<dyn Clock> = Arc::new(SystemClock);
        let control = PgPoolOptions::new()
            .max_connections(config.control_max_connections)
            .acquire_timeout(Duration::from_secs(5))
            .connect_with(connect_options(&config.control_url, "BILLING_CONTROL_URL")?)
            .await
            .map_err(to_app_error)?;
        CONTROL_MIGRATOR
            .run(&control)
            .await
            .map_err(AppError::internal)?;
        control::verify_schema(&control).await?;
        let registry = Registry::new(control.clone(), Arc::clone(&clock))?;

        let names = ObjectNames::new(&config.object_prefix)?;
        let maintenance_options =
            connect_options(&config.maintenance_url, "BILLING_MAINTENANCE_URL")?;
        let secrets = RoleSecrets::parse(&config.role_secrets, ROLE_SECRET_CONTEXT)?;
        let connector = Connector::new(maintenance_options.clone(), secrets);
        let rules = SelectorRules::new()
            .id_header(
                HeaderName::from_static(TENANT_HEADER),
                [config.api_host.clone()],
            )?
            .tenant_hosts(true);
        let tenants = Tenants::builder(
            registry.clone(),
            connector.clone(),
            names.clone(),
            rules,
            &TENANT_MIGRATOR,
            BillingFactory,
        )
        .domains(DOMAINS)
        .capacity(config.tenant_capacity)
        .build()?;

        let channel = Channel::new(&config.channel)?;
        let listener = Listener::new(tenants.clone(), channel.clone());
        let provisioner = Provisioner::builder(
            registry,
            Maintenance::new(maintenance_options, names),
            connector,
            &TENANT_MIGRATOR,
            Sender::new(control.clone(), channel),
            BillingHooks,
        )
        .role_key_id(&config.role_key_id)
        .connection_limit(config.tenant_connection_limit)
        .tenant_cap(config.tenant_cap)
        .tenants(tenants.clone())
        .clock(Arc::clone(&clock))
        .build()?;

        let audit = AdminAudit::new(control, Arc::clone(&clock)).with_domains(DOMAINS);
        let keys = admin::parse_keys(&config.admin_keys)?;
        let verifier = Verifier::<NoExtra>::new(&config.admin_issuer, &config.admin_audience, keys)
            .with_subject_check(is_operator)
            .with_domains(DOMAINS);
        let authenticator = admin::authenticator(verifier, &[ADMIN_SERVICE], clock)?;

        Ok(Self {
            public: public_app(&tenants)?,
            admin: admin_app(authenticator, TenantAdminService::new(provisioner, audit))?,
            tenants,
            listener,
        })
    }

    /// The public listener on `public` and the admin listener on `admin`,
    /// each serving gRPC and gRPC-Web and the health endpoints. Bind to port
    /// 0 for an ephemeral port and read it back with [`Server::local_addr`].
    pub async fn bind(&self, public: SocketAddr, admin: SocketAddr) -> Result<Servers, AppError> {
        let public = Server::builder()
            .grpc_web(true)
            .grpc_routes(self.public.grpc_routes())
            .bind(public)
            .await?;
        let admin = Server::builder()
            .grpc_web(true)
            .grpc_routes(self.admin.grpc_routes())
            .bind(admin)
            .await?;
        Ok(Servers { public, admin })
    }

    /// Every unit of the service on `runtime`: the tenant cache and the
    /// listener in [`Stage::Infrastructure`], the public App's `components`
    /// and the admin App's [`ADMIN_COMPONENTS_UNIT`], and the two listeners
    /// in [`Stage::Ingress`]. On shutdown the listeners drain first, then the
    /// Apps, then the cache closes its pools.
    pub fn runtime(self, servers: Servers, runtime: RuntimeBuilder) -> RuntimeBuilder {
        let runtime = self.public.register(runtime);
        self.admin
            .register_as(ADMIN_COMPONENTS_UNIT, runtime)
            .unit(
                TENANTS_UNIT,
                Stage::Infrastructure,
                UnitPolicy::Critical,
                self.tenants.into_unit(),
            )
            .unit(
                LISTENER_UNIT,
                Stage::Infrastructure,
                UnitPolicy::Critical,
                self.listener.into_unit(),
            )
            .unit(
                PUBLIC_UNIT,
                Stage::Ingress,
                UnitPolicy::default(),
                servers.public.into_unit(),
            )
            .unit(
                ADMIN_UNIT,
                Stage::Ingress,
                UnitPolicy::default(),
                servers.admin.into_unit(),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn operators_are_named() {
        assert!(is_operator("operator:ada"));
        assert!(!is_operator("operator:"));
        assert!(!is_operator("ada"));
    }

    #[test]
    fn a_malformed_url_is_refused_by_its_key() {
        let error = connect_options(&Secret::new("not a url".to_owned()), "BILLING_CONTROL_URL")
            .expect_err("not a URL");
        assert_eq!(error.code(), ErrorCode::InvalidArgument);
        assert!(error.message().contains("BILLING_CONTROL_URL"));
        assert!(!error.message().contains("not a url"));
    }

    #[test]
    fn the_settings_print_no_secret() {
        let source = MapSource::new()
            .with(
                "BILLING_CONTROL_URL",
                "postgres://control:hunter2@db/control",
            )
            .with("BILLING_MAINTENANCE_URL", "postgres://ops:hunter3@db/ops")
            .with(
                "BILLING_ROLE_SECRETS",
                "k1:0123456789abcdef0123456789abcdef",
            )
            .with("BILLING_ROLE_KEY_ID", "k1")
            .with("BILLING_ADMIN_ISSUER", "operator-console")
            .with("BILLING_ADMIN_AUDIENCE", "billing-eu")
            .with("BILLING_ADMIN_KEYS", "ops-1:key");
        let config: BillingConfig = sekvent::config::load(&source).expect("valid settings");
        assert_eq!(config.object_prefix, "app_t_");
        let printed = format!("{config:?}");
        assert!(!printed.contains("hunter"), "{printed}");
        assert!(!printed.contains("0123456789abcdef"), "{printed}");
    }
}
