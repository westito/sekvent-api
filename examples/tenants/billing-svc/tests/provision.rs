//! One tenant end to end, on a real Postgres server: an operator provisions
//! it through the admin App with the manager kit, its end users issue and
//! list invoices on the public App, and once suspended it is refused.
//!
//! The fixture follows sekvent-tenancy's own: one server per test binary,
//! a maintenance role that is not a superuser (`LOGIN CREATEDB
//! CREATEROLE`), a separate role that owns the control database, `CONNECT`
//! and `TEMPORARY` revoked from `PUBLIC` on both, and every name derived
//! from the test's unique control database. The service migrates the
//! control database itself.

use std::future::Future;
use std::net::SocketAddr;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use billing_api::{
    CreateInvoiceRequest, GetTenantReply, Invoice, ListInvoicesReply, ListInvoicesRequest,
    ProvisionTenantReply, ProvisionTenantRequest, Tenant, TenantRequest, ops,
};
use billing_svc::{Billing, BillingConfig, TENANT_HEADER};
use sekvent::auth::EdDsaSigningKey;
use sekvent::config::{MapSource, Secret};
use sekvent::context::SystemClock;
use sekvent::prelude::*;
use sekvent::tenancy::TenantId;
use sekvent::tenancy::admin::NoExtra;
use sekvent::tenancy::manager::{AdminClient, Minter};
use sekvent_testing::{PostgresHarness, docker_tests_enabled};
use sqlx::postgres::PgConnectOptions;
use sqlx::{AssertSqlSafe, Connection, PgConnection};
use uuid::Uuid;

const ISSUER: &str = "operator-console";
const AUDIENCE: &str = "billing-test";
const SUBJECT: &str = "operator:test";
const API_HOST: &str = "api.example.com";
const DEADLINE: Duration = Duration::from_secs(20);
/// gRPC `OK`, `INVALID_ARGUMENT` and `FAILED_PRECONDITION`.
const OK: i32 = 0;
const INVALID_ARGUMENT: i32 = 3;
const FAILED_PRECONDITION: i32 = 9;

/// Runs `scenario` under a 30 s guard against hangs.
async fn guard<F: Future>(scenario: F) -> F::Output {
    tokio::time::timeout(Duration::from_secs(30), scenario)
        .await
        .expect("the scenario finished within its 30 s guard")
}

/// A fresh random password: 32 hex digits, safe in a SQL literal.
fn new_password() -> Secret {
    Secret::new(Uuid::new_v4().simple().to_string())
}

/// One test's cluster identity on the shared server.
struct Cluster {
    harness: &'static PostgresHarness,
    /// 12 hex digits of the control database's unique name.
    stem: String,
    control_database: String,
    control_owner: String,
    control_password: Secret,
    maintenance_role: String,
    maintenance_password: Secret,
    maintenance_database: String,
}

impl Cluster {
    /// The fixture; `None` when Docker tests are off.
    async fn start() -> Option<Self> {
        if !docker_tests_enabled() {
            return None;
        }
        let harness = PostgresHarness::shared().await;
        let control = harness
            .create_database()
            .await
            .expect("the harness created the control database");
        let stem = control
            .name
            .get(2..14)
            .expect("a harness database name has 32 hex digits")
            .to_owned();
        let this = Self {
            harness,
            control_database: control.name,
            control_owner: format!("fx_{stem}_control"),
            control_password: new_password(),
            maintenance_role: format!("fx_{stem}_maintenance"),
            maintenance_password: new_password(),
            maintenance_database: format!("fx_{stem}_maintenance"),
            stem,
        };
        this.set_up().await;
        Some(this)
    }

    async fn set_up(&self) {
        let maintenance = &self.maintenance_role;
        let maintenance_db = &self.maintenance_database;
        let owner = &self.control_owner;
        let control = &self.control_database;
        let options =
            PgConnectOptions::from_str(&self.harness.admin_url()).expect("the harness URL parses");
        let mut admin = PgConnection::connect_with(&options)
            .await
            .expect("the bootstrap admin connected");
        for statement in [
            format!(
                "CREATE ROLE \"{maintenance}\" LOGIN CREATEDB CREATEROLE PASSWORD '{}'",
                self.maintenance_password.expose()
            ),
            format!(
                "CREATE ROLE \"{owner}\" LOGIN PASSWORD '{}'",
                self.control_password.expose()
            ),
            format!("CREATE DATABASE \"{maintenance_db}\""),
            format!("REVOKE CONNECT, TEMPORARY ON DATABASE \"{maintenance_db}\" FROM PUBLIC"),
            format!("GRANT CONNECT ON DATABASE \"{maintenance_db}\" TO \"{maintenance}\""),
            format!("ALTER DATABASE \"{control}\" OWNER TO \"{owner}\""),
            format!("REVOKE CONNECT, TEMPORARY ON DATABASE \"{control}\" FROM PUBLIC"),
        ] {
            sqlx::raw_sql(AssertSqlSafe(statement))
                .execute(&mut admin)
                .await
                .expect("the fixture statement succeeded");
        }
        admin.close().await.expect("the admin session closed");
    }

    /// `postgres://role:password@host:port/database` on the shared server.
    fn url(&self, role: &str, password: &Secret, database: &str) -> String {
        let host = self.harness.host();
        let host = if host.contains(':') {
            format!("[{host}]")
        } else {
            host.to_owned()
        };
        format!(
            "postgres://{role}:{}@{host}:{}/{database}",
            password.expose(),
            self.harness.port()
        )
    }

    /// The service's settings: both listeners on ephemeral loopback ports,
    /// names and the channel derived from the stem, `operator_keys` the
    /// operator's public keys.
    fn config(&self, operator_keys: &str) -> BillingConfig {
        let source = MapSource::new()
            .with("BILLING_PUBLIC_ADDR", "127.0.0.1:0")
            .with("BILLING_ADMIN_ADDR", "127.0.0.1:0")
            .with(
                "BILLING_CONTROL_URL",
                self.url(
                    &self.control_owner,
                    &self.control_password,
                    &self.control_database,
                ),
            )
            .with("BILLING_CONTROL_MAX_CONNECTIONS", "6")
            .with(
                "BILLING_MAINTENANCE_URL",
                self.url(
                    &self.maintenance_role,
                    &self.maintenance_password,
                    &self.maintenance_database,
                ),
            )
            .with(
                "BILLING_ROLE_SECRETS",
                format!("k1:{}", self.stem.repeat(4)),
            )
            .with("BILLING_ROLE_KEY_ID", "k1")
            .with("BILLING_OBJECT_PREFIX", format!("app_{}_", self.stem))
            .with("BILLING_CHANNEL", format!("fx_{}_changes", self.stem))
            .with("BILLING_API_HOST", API_HOST)
            .with("BILLING_TENANT_CONNECTION_LIMIT", "10")
            .with("BILLING_ADMIN_ISSUER", ISSUER)
            .with("BILLING_ADMIN_AUDIENCE", AUDIENCE)
            .with("BILLING_ADMIN_KEYS", operator_keys);
        sekvent::config::load(&source).expect("the test settings are valid")
    }
}

/// The operator service's minter.
fn minter() -> Minter {
    let key = EdDsaSigningKey::from_seed("ops-1", &[7; 32]).expect("a signing key");
    Minter::new(ISSUER, key, Arc::new(SystemClock))
}

/// One unary gRPC-Web call of `Invoices/<rpc>` on the shared API host,
/// naming `tenant` in the tenant header when given: the reply, or the
/// non-OK gRPC status.
async fn invoices<Req, Rep>(
    client: &reqwest::Client,
    addr: SocketAddr,
    rpc: &str,
    tenant: Option<TenantId>,
    request: &Req,
) -> Result<Rep, i32>
where
    Req: prost::Message,
    Rep: prost::Message + Default,
{
    let payload = request.encode_to_vec();
    let mut body = vec![0_u8];
    body.extend_from_slice(
        &u32::try_from(payload.len())
            .expect("a small message")
            .to_be_bytes(),
    );
    body.extend_from_slice(&payload);
    let mut call = client
        .post(format!("http://{addr}/billing.invoices.v1.Invoices/{rpc}"))
        .header("host", API_HOST)
        .header("content-type", "application/grpc-web+proto")
        .header("x-grpc-web", "1")
        .body(body);
    if let Some(tenant) = tenant {
        call = call.header(TENANT_HEADER, tenant.to_string());
    }
    let response = call.send().await.expect("the call was answered");
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let mut status: Option<i32> = response
        .headers()
        .get("grpc-status")
        .map(|value| value.to_str().unwrap().parse().unwrap());
    let body = response.bytes().await.expect("the body was read").to_vec();
    let mut reply = None;
    let mut rest = body.as_slice();
    while rest.len() >= 5 {
        let flag = rest[0];
        let length = usize::try_from(u32::from_be_bytes([rest[1], rest[2], rest[3], rest[4]]))
            .expect("a frame length");
        let (frame, after) = rest[5..].split_at(length);
        rest = after;
        if flag & 0x80 == 0 {
            reply = Some(Rep::decode(frame).expect("the reply decodes"));
            continue;
        }
        for line in std::str::from_utf8(frame).expect("trailers").split("\r\n") {
            if let Some(value) = line.strip_prefix("grpc-status:") {
                status = Some(value.trim().parse().expect("a status code"));
            }
        }
    }
    match status.expect("the answer carries a gRPC status") {
        OK => Ok(reply.expect("an OK answer carries a reply")),
        code => Err(code),
    }
}

fn create(customer: &str, amount_cents: i64) -> CreateInvoiceRequest {
    CreateInvoiceRequest {
        customer: customer.to_owned(),
        amount_cents,
    }
}

#[tokio::test]
#[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --include-ignored"]
#[allow(clippy::too_many_lines)]
async fn an_operator_provisions_a_tenant_that_then_serves_invoices() {
    let Some(cluster) = Cluster::start().await else {
        return;
    };
    guard(async {
        let minter = minter();
        let config = cluster.config(&minter.public_key_entry());
        let billing = Billing::build(&config).await.expect("the service built");
        let servers = billing
            .bind(config.public_addr, config.admin_addr)
            .await
            .expect("the listeners are bound");
        let public = servers.public.local_addr();
        let admin = servers.admin.local_addr();
        let runtime = billing
            .runtime(
                servers,
                Runtime::builder()
                    .without_signals()
                    .shutdown_delay(Duration::ZERO),
            )
            .build()
            .expect("the runtime is valid")
            .start()
            .await
            .expect("the runtime started");

        // The operator provisions a tenant through the admin App.
        let operator = AdminClient::new(&format!("http://{admin}"), AUDIENCE, minter)
            .expect("the admin client");
        let tenant = TenantId::from_uuid(Uuid::new_v4());
        let request = ProvisionTenantRequest {
            tenant_id: tenant.to_string(),
            subdomain: "acme".to_owned(),
            display_name: "Acme Ltd".to_owned(),
            hosts: vec!["acme.example.com".to_owned()],
            primary_host: "acme.example.com".to_owned(),
            owner_email: "owner@example.com".to_owned(),
            currency: "EUR".to_owned(),
        };
        let provisioned: ProvisionTenantReply = operator
            .call(
                ops::PROVISION_TENANT,
                Some(tenant),
                SUBJECT,
                &NoExtra {},
                DEADLINE,
                &request,
            )
            .await
            .into_result()
            .expect("the tenant was provisioned");
        let row = provisioned.tenant.expect("the reply names the tenant");
        assert_eq!(row.tenant_id, tenant.to_string());
        assert_eq!(row.state, "active");
        assert_eq!(row.hosts, ["acme.example.com"]);
        assert!(!provisioned.owner_token_withheld);
        assert_eq!(provisioned.owner_token.len(), 64);

        // Provisioning again with a fresh token converges and withholds the token.
        let again: ProvisionTenantReply = operator
            .call(
                ops::PROVISION_TENANT,
                Some(tenant),
                SUBJECT,
                &NoExtra {},
                DEADLINE,
                &request,
            )
            .await
            .into_result()
            .expect("the repeat converged");
        assert!(again.owner_token_withheld);
        assert_eq!(again.owner_token, "");

        // Its end users issue and list invoices on the public App.
        let client = sekvent::client::reqwest_builder()
            .http1_only()
            .build()
            .expect("an HTTP client");
        let first: Invoice = invoices(
            &client,
            public,
            "CreateInvoice",
            Some(tenant),
            &create("Globex", 12_500),
        )
        .await
        .expect("the first invoice");
        assert_eq!(first.number, "INV-000001");
        assert_eq!(first.currency, "EUR");
        let second: Invoice = invoices(
            &client,
            public,
            "CreateInvoice",
            Some(tenant),
            &create("Initech", 900),
        )
        .await
        .expect("the second invoice");
        assert_eq!(second.number, "INV-000002");
        let listed: ListInvoicesReply = invoices(
            &client,
            public,
            "ListInvoices",
            Some(tenant),
            &ListInvoicesRequest { page_size: 0 },
        )
        .await
        .expect("the invoices");
        assert_eq!(listed.invoices, [first, second]);

        // The shared host without a tenant names none.
        let unnamed = invoices::<_, ListInvoicesReply>(
            &client,
            public,
            "ListInvoices",
            None,
            &ListInvoicesRequest { page_size: 0 },
        )
        .await
        .expect_err("no tenant named");
        assert_eq!(unnamed, INVALID_ARGUMENT);

        // The operator reads the usage, then suspends the tenant: it is refused at once.
        let read = TenantRequest {
            tenant_id: tenant.to_string(),
        };
        let usage: GetTenantReply = operator
            .call(
                ops::GET_TENANT,
                Some(tenant),
                SUBJECT,
                &NoExtra {},
                DEADLINE,
                &read,
            )
            .await
            .into_result()
            .expect("the tenant was read");
        assert!(usage.measured);
        assert_eq!(usage.invoices, 2);
        assert!(usage.database_bytes > 0);
        let suspended: Tenant = operator
            .call(
                ops::SUSPEND_TENANT,
                Some(tenant),
                SUBJECT,
                &NoExtra {},
                DEADLINE,
                &read,
            )
            .await
            .into_result()
            .expect("the tenant was suspended");
        assert_eq!(suspended.state, "suspended");
        let refused = invoices::<_, Invoice>(
            &client,
            public,
            "CreateInvoice",
            Some(tenant),
            &create("Globex", 1),
        )
        .await
        .expect_err("a suspended tenant is not served");
        assert_eq!(refused, FAILED_PRECONDITION);

        runtime.shutdown();
        runtime.wait().await.expect("the runtime stopped");
    })
    .await;
}
