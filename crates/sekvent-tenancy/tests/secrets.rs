//! Role credentials on a real server: the verifier signs the derived password in, the connector yields a tenant session.
#![cfg(feature = "lifecycle")]

mod common;

use std::sync::{Arc, Mutex};

use sekvent_config::Secret;
use sekvent_db::KeyedPools;
use sekvent_tenancy::cluster::roles;
use sekvent_tenancy::{
    Connector, DatabaseName, RoleKey, RoleSecrets, ScramVerifier, TenantId, TenantRole,
};
use sqlx::{Connection, PgConnection};
use tracing::field::{Field, Visit};
use tracing::{Event, Metadata, Subscriber, span};

const CONTEXT: &str = "example-app.role.v1";
const CONNECTION_LIMIT: u32 = 5;
const INVALID_PASSWORD: &str = "28P01";

fn role_secrets() -> RoleSecrets {
    RoleSecrets::parse(&Secret::new("k1:correct horse battery staple 936"), CONTEXT)
        .expect("the test master secret parses")
}

fn key(generation: u32) -> RoleKey {
    RoleKey {
        key_id: "k1".to_owned(),
        generation,
    }
}

/// A tenant role created through `roles::create` with the verifier of its
/// generation-1 password, and a database it owns.
async fn tenant_role(
    cluster: &common::Cluster,
    secrets: &RoleSecrets,
    tenant: TenantId,
) -> (TenantRole, DatabaseName) {
    let role = cluster.names().role(tenant);
    let password = secrets.derive(tenant, &key(1)).expect("a derived password");
    let verifier = tokio::task::spawn_blocking(move || ScramVerifier::new(&password))
        .await
        .expect("the verifier task finished")
        .expect("a verifier");
    let mut conn = cluster
        .maintenance()
        .connect()
        .await
        .expect("the maintenance connection opened");
    roles::create(&mut conn, &role, &verifier, CONNECTION_LIMIT)
        .await
        .expect("the role was created");
    conn.close()
        .await
        .expect("the maintenance connection closed");
    let database = cluster.create_database("tenant", role.as_str()).await;
    let database = DatabaseName::parse(&database).expect("the fixture name is a database name");
    (role, database)
}

async fn identity(conn: &mut PgConnection) -> (String, String, String) {
    sqlx::query_as("SELECT current_user::text, session_user::text, current_database()::text")
        .fetch_one(conn)
        .await
        .expect("the session identity is readable")
}

#[tokio::test]
#[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
async fn a_role_created_with_the_verifier_signs_in_with_the_derived_password() {
    let Some(cluster) = common::Cluster::start().await else {
        return;
    };
    common::guard(async {
        let secrets = role_secrets();
        let tenant = common::new_tenant();
        let (role, database) = tenant_role(&cluster, &secrets, tenant).await;
        let password = secrets.derive(tenant, &key(1)).expect("a derived password");
        let mut conn = cluster
            .connect_as(role.as_str(), &password, database.as_str())
            .await
            .expect("the tenant role signed in");
        let (current, session, current_database) = identity(&mut conn).await;
        assert_eq!(current, role.as_str());
        assert_eq!(session, role.as_str());
        assert_eq!(current_database, database.as_str());
        conn.close().await.expect("the tenant session closed");
    })
    .await;
}

#[tokio::test]
#[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
async fn the_password_of_another_generation_is_rejected() {
    let Some(cluster) = common::Cluster::start().await else {
        return;
    };
    common::guard(async {
        let secrets = role_secrets();
        let tenant = common::new_tenant();
        let (role, database) = tenant_role(&cluster, &secrets, tenant).await;
        let other = secrets.derive(tenant, &key(2)).expect("a derived password");
        let Err(error) = cluster
            .connect_as(role.as_str(), &other, database.as_str())
            .await
        else {
            panic!("the password of generation 2 signed in");
        };
        let code = error
            .as_database_error()
            .and_then(sqlx::error::DatabaseError::code)
            .map(std::borrow::Cow::into_owned);
        assert_eq!(code.as_deref(), Some(INVALID_PASSWORD));
    })
    .await;
}

#[tokio::test]
#[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
async fn the_connector_signs_in_as_the_tenant_whatever_the_base_carries() {
    let Some(cluster) = common::Cluster::start().await else {
        return;
    };
    common::guard(async {
        let secrets = role_secrets();
        let tenant = common::new_tenant();
        let (role, database) = tenant_role(&cluster, &secrets, tenant).await;
        // The base carries the bootstrap superuser, its password and `postgres`.
        let connector = Connector::new(cluster.admin_options("postgres"), secrets);
        let options = connector
            .options(tenant, &role, &database, &key(1))
            .expect("options for the tenant");
        let mut conn = PgConnection::connect_with(&options)
            .await
            .expect("the tenant signed in through the connector");
        let (current, session, current_database) = identity(&mut conn).await;
        assert_eq!(current, role.as_str());
        assert_eq!(session, role.as_str());
        assert_eq!(current_database, database.as_str());
        conn.close().await.expect("the tenant session closed");

        let candidates = connector
            .candidates(tenant, &role, &database, &[key(1), key(2)])
            .expect("two candidates");
        assert_eq!(format!("{candidates:?}"), "ConnectCandidates { count: 2 }");
    })
    .await;
}

/// Collects the `source` field of every event on the thread it is the
/// default subscriber of: the keyed cache logs a failed creation's source
/// there and hands its callers a copy without it.
#[derive(Clone, Default)]
struct Sources(Arc<Mutex<Vec<String>>>);

impl Sources {
    fn taken(&self) -> Vec<String> {
        std::mem::take(&mut *self.0.lock().expect("the capture lock"))
    }
}

/// Reads the `source` field of one event.
struct SourceField<'a>(&'a mut Option<String>);

impl Visit for SourceField<'_> {
    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "source" {
            *self.0 = Some(value.to_owned());
        }
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if field.name() == "source" {
            *self.0 = Some(format!("{value:?}"));
        }
    }
}

impl Subscriber for Sources {
    fn enabled(&self, _: &Metadata<'_>) -> bool {
        true
    }

    fn new_span(&self, _: &span::Attributes<'_>) -> span::Id {
        span::Id::from_u64(1)
    }

    fn record(&self, _: &span::Id, _: &span::Record<'_>) {}

    fn record_follows_from(&self, _: &span::Id, _: &span::Id) {}

    fn event(&self, event: &Event<'_>) {
        let mut source = None;
        event.record(&mut SourceField(&mut source));
        if let Some(source) = source {
            self.0.lock().expect("the capture lock").push(source);
        }
    }

    fn enter(&self, _: &span::Id) {}

    fn exit(&self, _: &span::Id) {}
}

/// The logged source of a keyed pool's failed connect with the candidates
/// of the keys of `generations`, in that order: it names the failure and
/// how many candidates were tried.
async fn connect_failure(
    connector: &Connector,
    tenant: TenantId,
    role: &TenantRole,
    database: &DatabaseName,
    generations: Vec<u32>,
) -> Vec<String> {
    let connector = connector.clone();
    let role = role.clone();
    let database = database.clone();
    let pools = KeyedPools::builder(move |generations: Vec<u32>| {
        let keys: Vec<RoleKey> = generations.into_iter().map(key).collect();
        let candidates = connector.candidates(tenant, &role, &database, &keys);
        async move { candidates }
    })
    .build()
    .expect("the keyed pools were built");
    let sources = Sources::default();
    let outcome = {
        // The test's runtime is single-threaded, so the creation task logs
        // on this thread too.
        let _capture = tracing::subscriber::set_default(sources.clone());
        pools.get(&generations).await
    };
    pools.close().await;
    assert!(
        outcome.is_err(),
        "a database that accepts no connection was opened"
    );
    sources.taken()
}

#[tokio::test]
#[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
async fn the_candidates_are_tried_first_key_first() {
    let Some(cluster) = common::Cluster::start().await else {
        return;
    };
    common::guard(async {
        let secrets = role_secrets();
        let tenant = common::new_tenant();
        let (role, database) = tenant_role(&cluster, &secrets, tenant).await;
        // The server checks the password before it refuses the closed
        // database: generation 1 signs in and is then refused (55000), which
        // ends the attempts; generation 2's password is rejected (28P01),
        // which moves on to the next candidate.
        cluster
            .admin_execute(
                "postgres",
                &format!(
                    "ALTER DATABASE {} WITH ALLOW_CONNECTIONS false",
                    common::quote(database.as_str())
                ),
            )
            .await;
        let connector = Connector::new(cluster.admin_options("postgres"), secrets);
        assert_eq!(
            connect_failure(&connector, tenant, &role, &database, vec![1, 2]).await,
            ["connect failed: other (sqlstate 55000), 1 of 2 candidates tried"]
        );
        assert_eq!(
            connect_failure(&connector, tenant, &role, &database, vec![2, 1]).await,
            ["connect failed: other (sqlstate 55000), 2 of 2 candidates tried"]
        );
    })
    .await;
}
