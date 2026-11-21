//! Privilege inspection on a real server: membership chains, effective CONNECT, databases without an ACL.
#![cfg(feature = "serving")]

mod common;

use std::collections::BTreeSet;
use std::error::Error as _;

use common::{Cluster, quote};
use sekvent_error::{AppError, ErrorCode};
use sekvent_tenancy::cluster::privileges::{
    self, DatabaseGrant, DatabasePrivilege, Grantee, MembershipEdge, RoleAttributes,
};
use sekvent_tenancy::{DatabaseName, TenantRole};
use sqlx::{Connection, PgConnection};

/// Runs `statement` as the bootstrap admin.
async fn admin_sql(cluster: &Cluster, statement: &str) {
    cluster.admin_execute("postgres", statement).await;
}

/// A session of the maintenance role on its database.
async fn maintenance_session(cluster: &Cluster) -> PgConnection {
    cluster
        .connect_as(
            cluster.maintenance_role(),
            cluster.maintenance_password(),
            cluster.maintenance_database(),
        )
        .await
        .expect("the maintenance role connected")
}

/// A tenant role made the way the crate makes one (`LOGIN NOINHERIT`, held
/// by the maintenance role without `INHERIT`), and its database name.
async fn tenant_role(cluster: &Cluster) -> (TenantRole, DatabaseName) {
    let tenant = common::new_tenant();
    let role = cluster.names().role(tenant);
    let database = cluster.names().database(tenant);
    cluster
        .create_role_named(role.as_str(), "LOGIN NOINHERIT")
        .await;
    admin_sql(
        cluster,
        &format!(
            "GRANT {} TO {} WITH INHERIT FALSE, SET TRUE",
            quote(role.as_str()),
            quote(cluster.maintenance_role())
        ),
    )
    .await;
    (role, database)
}

fn source_of(error: &AppError) -> String {
    error.source().map(ToString::to_string).unwrap_or_default()
}

/// The effective `CONNECT` of an existing role on an existing database.
async fn may(admin: &mut PgConnection, role: &str, database: &str) -> bool {
    privileges::role_may_connect(admin, role, database)
        .await
        .unwrap()
        .expect("role and database exist")
}

fn sorted(mut names: Vec<String>) -> Vec<String> {
    names.sort();
    names
}

/// The grants as `(grantee, privilege)` pairs, `PUBLIC` for the public.
fn grant_set(grants: &[DatabaseGrant]) -> BTreeSet<(String, DatabasePrivilege)> {
    grants
        .iter()
        .map(|grant| {
            let grantee = match &grant.grantee {
                Grantee::Public => "PUBLIC".to_owned(),
                Grantee::Role(role) => role.clone(),
            };
            (grantee, grant.privilege)
        })
        .collect()
}

#[tokio::test]
#[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
#[allow(clippy::too_many_lines)]
async fn membership_chains_are_followed_in_both_directions() {
    let Some(cluster) = Cluster::start().await else {
        return;
    };
    common::guard(async {
        let target = cluster.create_role("target", "").await;
        let up = [
            cluster.create_role("up", "").await,
            cluster.create_role("up", "").await,
            cluster.create_role("up", "").await,
        ];
        let down = [
            cluster.create_role("down", "").await,
            cluster.create_role("down", "").await,
            cluster.create_role("down", "").await,
        ];
        // target -> up[0] -> up[1] -> up[2]: three edges out of the target.
        admin_sql(
            &cluster,
            &format!(
                "GRANT {} TO {} WITH ADMIN TRUE, INHERIT FALSE, SET FALSE",
                quote(&up[0]),
                quote(&target)
            ),
        )
        .await;
        admin_sql(
            &cluster,
            &format!("GRANT {} TO {}", quote(&up[1]), quote(&up[0])),
        )
        .await;
        admin_sql(
            &cluster,
            &format!("GRANT {} TO {}", quote(&up[2]), quote(&up[1])),
        )
        .await;
        // down[2] -> down[1] -> down[0] -> target: three edges into it.
        admin_sql(
            &cluster,
            &format!("GRANT {} TO {}", quote(&target), quote(&down[0])),
        )
        .await;
        admin_sql(
            &cluster,
            &format!("GRANT {} TO {}", quote(&down[0]), quote(&down[1])),
        )
        .await;
        admin_sql(
            &cluster,
            &format!("GRANT {} TO {}", quote(&down[1]), quote(&down[2])),
        )
        .await;

        let mut admin = cluster.admin().await;
        let facts = privileges::role_facts(&mut admin, &target)
            .await
            .unwrap()
            .expect("the target exists");
        assert_eq!(facts.name, target);
        assert_eq!(facts.reached, sorted(up.to_vec()));
        assert_eq!(facts.reaching, sorted(down.to_vec()));
        // Only the direct edges, each with its options.
        let mut edges = facts.edges;
        edges.sort_by(|a, b| (&a.role, &a.member).cmp(&(&b.role, &b.member)));
        let mut expected = vec![
            MembershipEdge {
                role: up[0].clone(),
                member: target.clone(),
                admin_option: true,
                inherit_option: false,
                set_option: false,
            },
            MembershipEdge {
                role: target.clone(),
                member: down[0].clone(),
                admin_option: false,
                inherit_option: true,
                set_option: true,
            },
        ];
        expected.sort_by(|a, b| (&a.role, &a.member).cmp(&(&b.role, &b.member)));
        assert_eq!(edges, expected);

        // Two edges: up[0] reaches up[1] and up[2]; down[0] is reached by
        // down[1] and down[2].
        let middle_up = privileges::role_facts(&mut admin, &up[0])
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            middle_up.reached,
            sorted(vec![up[1].clone(), up[2].clone()])
        );
        let mut through = down.to_vec();
        through.push(target.clone());
        assert_eq!(middle_up.reaching, sorted(through));
        let middle_down = privileges::role_facts(&mut admin, &down[0])
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            middle_down.reaching,
            sorted(vec![down[1].clone(), down[2].clone()])
        );
        let mut beyond = up.to_vec();
        beyond.push(target.clone());
        assert_eq!(middle_down.reached, sorted(beyond));

        assert_eq!(
            privileges::role_facts(&mut admin, &cluster.unique_name("absent"))
                .await
                .unwrap(),
            None
        );
        admin.close().await.unwrap();
    })
    .await;
}

#[tokio::test]
#[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
async fn attributes_are_read_from_the_catalog() {
    let Some(cluster) = Cluster::start().await else {
        return;
    };
    common::guard(async {
        let role = cluster
            .create_role(
                "attributes",
                "LOGIN NOINHERIT CREATEDB REPLICATION BYPASSRLS",
            )
            .await;
        let mut admin = cluster.admin().await;
        let facts = privileges::role_facts(&mut admin, &role)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            facts.attributes,
            RoleAttributes {
                superuser: false,
                inherit: false,
                create_role: false,
                create_db: true,
                login: true,
                replication: true,
                bypass_rls: true,
            }
        );
        assert_eq!(
            facts.attributes.unexpected_for_tenant(),
            ["CREATEDB", "REPLICATION", "BYPASSRLS"]
        );
        admin.close().await.unwrap();
    })
    .await;
}

#[tokio::test]
#[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
async fn a_tenant_role_passes_until_it_drifts() {
    let Some(cluster) = Cluster::start().await else {
        return;
    };
    common::guard(async {
        let mut maintenance = maintenance_session(&cluster).await;

        let (role, _) = tenant_role(&cluster).await;
        privileges::check_tenant_role(&mut maintenance, &role)
            .await
            .expect("a role made like the crate makes one passes");

        let reader = cluster.create_role("reader", "").await;
        admin_sql(
            &cluster,
            &format!("GRANT {} TO {}", quote(&reader), quote(role.as_str())),
        )
        .await;
        let error = privileges::check_tenant_role(&mut maintenance, &role)
            .await
            .unwrap_err();
        assert_eq!(error.code(), ErrorCode::Internal);
        assert_eq!(error.reason(), None);
        let source = source_of(&error);
        assert!(source.contains(role.as_str()), "{source}");
        assert!(
            source.contains(&format!("reaches role {reader}")),
            "{source}"
        );

        // A maintenance membership with INHERIT.
        let tenant = common::new_tenant();
        let inheriting = cluster.names().role(tenant);
        cluster
            .create_role_named(inheriting.as_str(), "LOGIN NOINHERIT")
            .await;
        admin_sql(
            &cluster,
            &format!(
                "GRANT {} TO {} WITH INHERIT TRUE, SET TRUE",
                quote(inheriting.as_str()),
                quote(cluster.maintenance_role())
            ),
        )
        .await;
        let error = privileges::check_tenant_role(&mut maintenance, &inheriting)
            .await
            .unwrap_err();
        assert!(
            source_of(&error).contains("INHERIT"),
            "{}",
            source_of(&error)
        );

        // A member of the maintenance role reaches every role it holds.
        let (held, _) = tenant_role(&cluster).await;
        let operator = cluster.create_role("operator", "").await;
        admin_sql(
            &cluster,
            &format!(
                "GRANT {} TO {}",
                quote(cluster.maintenance_role()),
                quote(&operator)
            ),
        )
        .await;
        let error = privileges::check_tenant_role(&mut maintenance, &held)
            .await
            .unwrap_err();
        let source = source_of(&error);
        assert!(
            source.contains(&format!("role {operator} reaches it")),
            "{source}"
        );

        // A role that is not there.
        let absent = cluster.names().role(common::new_tenant());
        let error = privileges::check_tenant_role(&mut maintenance, &absent)
            .await
            .unwrap_err();
        assert_eq!(error.code(), ErrorCode::Internal);
        assert!(source_of(&error).contains("does not exist"));
        maintenance.close().await.unwrap();
    })
    .await;
}

#[tokio::test]
#[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
#[allow(clippy::too_many_lines)]
async fn a_database_without_an_acl_is_read_as_its_defaults() {
    let Some(cluster) = Cluster::start().await else {
        return;
    };
    common::guard(async {
        let owner = cluster.create_role("owner", "").await;
        let database = cluster.create_database("defaults", &owner).await;
        let mut admin = cluster.admin().await;
        let acl_is_null: bool =
            sqlx::query_scalar("SELECT datacl IS NULL FROM pg_database WHERE datname = $1")
                .bind(database.as_str())
                .fetch_one(&mut admin)
                .await
                .unwrap();
        assert!(acl_is_null, "a fresh database has no ACL of its own");

        let facts = privileges::database_facts(&mut admin, &database)
            .await
            .unwrap()
            .expect("the database exists");
        assert_eq!(facts.name, database);
        assert_eq!(facts.owner, owner);
        assert!(!facts.template);
        let expected: BTreeSet<(String, DatabasePrivilege)> = [
            ("PUBLIC".to_owned(), DatabasePrivilege::Connect),
            ("PUBLIC".to_owned(), DatabasePrivilege::Temporary),
            (owner.clone(), DatabasePrivilege::Connect),
            (owner.clone(), DatabasePrivilege::Temporary),
            (owner.clone(), DatabasePrivilege::Create),
        ]
        .into_iter()
        .collect();
        assert_eq!(grant_set(&facts.grants), expected);
        assert_eq!(facts.grants.len(), 5);
        assert!(
            facts
                .grants
                .iter()
                .filter(|grant| grant.grantee == Grantee::Public)
                .all(|grant| !grant.grantable)
        );
        assert_eq!(
            privileges::public_may_connect(&mut admin, &database)
                .await
                .unwrap(),
            Some(true)
        );

        // TEMPORARY alone is not CONNECT; a grant option is read.
        let reader = cluster.create_role("reader", "").await;
        admin_sql(
            &cluster,
            &format!(
                "REVOKE CONNECT ON DATABASE {db} FROM PUBLIC; \
                 GRANT CONNECT ON DATABASE {db} TO {reader} WITH GRANT OPTION; \
                 ALTER DATABASE {db} IS_TEMPLATE true",
                db = quote(&database),
                reader = quote(&reader)
            ),
        )
        .await;
        assert_eq!(
            privileges::public_may_connect(&mut admin, &database)
                .await
                .unwrap(),
            Some(false)
        );
        let facts = privileges::database_facts(&mut admin, &database)
            .await
            .unwrap()
            .unwrap();
        assert!(facts.template);
        let set = grant_set(&facts.grants);
        assert!(set.contains(&("PUBLIC".to_owned(), DatabasePrivilege::Temporary)));
        assert!(!set.contains(&("PUBLIC".to_owned(), DatabasePrivilege::Connect)));
        assert!(facts.grants.contains(&DatabaseGrant {
            grantee: Grantee::Role(reader.clone()),
            privilege: DatabasePrivilege::Connect,
            grantable: true,
        }));

        // An empty ACL holds no grant at all.
        let empty = cluster.create_database("empty", &owner).await;
        admin_sql(
            &cluster,
            &format!(
                "REVOKE ALL ON DATABASE {db} FROM PUBLIC; REVOKE ALL ON DATABASE {db} FROM {owner}",
                db = quote(&empty),
                owner = quote(&owner)
            ),
        )
        .await;
        let facts = privileges::database_facts(&mut admin, &empty)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(facts.grants, [] as [DatabaseGrant; 0]);
        assert_eq!(
            privileges::public_may_connect(&mut admin, &empty)
                .await
                .unwrap(),
            Some(false)
        );

        // A database that is not there.
        let absent = cluster.unique_name("absent");
        assert_eq!(
            privileges::database_facts(&mut admin, &absent)
                .await
                .unwrap(),
            None
        );
        assert_eq!(
            privileges::public_may_connect(&mut admin, &absent)
                .await
                .unwrap(),
            None
        );
        assert_eq!(
            privileges::roles_that_may_connect(&mut admin, &absent)
                .await
                .unwrap(),
            None
        );
        assert_eq!(
            privileges::role_may_connect(&mut admin, &owner, &absent)
                .await
                .unwrap(),
            None
        );
        admin.close().await.unwrap();
    })
    .await;
}

#[tokio::test]
#[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
#[allow(clippy::too_many_lines)]
async fn effective_connect_follows_grants_public_inheritance_and_superusers() {
    let Some(cluster) = Cluster::start().await else {
        return;
    };
    common::guard(async {
        let owner = cluster.create_role("owner", "").await;
        let restricted = cluster.create_database("restricted", &owner).await;
        let open = cluster.create_database("open", &owner).await;
        admin_sql(
            &cluster,
            &format!(
                "REVOKE CONNECT, TEMPORARY ON DATABASE {} FROM PUBLIC",
                quote(&restricted)
            ),
        )
        .await;

        let (granted, _) = cluster.create_login_role("granted").await;
        let (stranger, _) = cluster.create_login_role("stranger").await;
        let group = cluster.create_role("group", "").await;
        let (inheriting, _) = cluster.create_login_role("inheriting").await;
        let no_inherit = cluster.create_role("noinherit", "LOGIN NOINHERIT").await;
        let (option_off, _) = cluster.create_login_role("optionoff").await;
        let (chained, _) = cluster.create_login_role("chained").await;
        let middle = cluster.create_role("middle", "").await;
        let superuser = cluster.create_role("superuser", "LOGIN SUPERUSER").await;
        for statement in [
            format!(
                "GRANT CONNECT ON DATABASE {} TO {}",
                quote(&restricted),
                quote(&granted)
            ),
            format!(
                "GRANT CONNECT ON DATABASE {} TO {}",
                quote(&restricted),
                quote(&group)
            ),
            format!("GRANT {} TO {}", quote(&group), quote(&inheriting)),
            format!("GRANT {} TO {}", quote(&group), quote(&no_inherit)),
            format!(
                "GRANT {} TO {} WITH INHERIT FALSE",
                quote(&group),
                quote(&option_off)
            ),
            format!("GRANT {} TO {}", quote(&group), quote(&middle)),
            format!("GRANT {} TO {}", quote(&middle), quote(&chained)),
        ] {
            admin_sql(&cluster, &statement).await;
        }

        let mut admin = cluster.admin().await;
        let conn = &mut admin;
        assert!(may(conn, &granted, &restricted).await, "an own grant");
        assert!(!may(conn, &stranger, &restricted).await, "no grant at all");
        assert!(may(conn, &stranger, &open).await, "PUBLIC's default grant");
        assert!(
            may(conn, &inheriting, &restricted).await,
            "an inherited membership"
        );
        assert!(
            may(conn, &chained, &restricted).await,
            "a chain of inherited memberships"
        );
        assert!(
            !may(conn, &no_inherit, &restricted).await,
            "a NOINHERIT member"
        );
        assert!(
            !may(conn, &option_off, &restricted).await,
            "a membership without INHERIT"
        );
        assert!(may(conn, &superuser, &restricted).await, "a superuser");
        assert!(may(conn, &owner, &restricted).await, "the owner");

        let connecting = privileges::roles_that_may_connect(&mut admin, &restricted)
            .await
            .unwrap()
            .expect("the database exists");
        let mut in_order = connecting.clone();
        in_order.sort();
        assert_eq!(connecting, in_order, "sorted by name");
        for role in [&granted, &inheriting, &chained, &superuser] {
            assert!(connecting.contains(role), "{role} may connect");
        }
        for role in [&stranger, &no_inherit, &option_off, &group, &middle, &owner] {
            assert!(!connecting.contains(role), "{role} is not listed");
        }
        assert_eq!(
            privileges::role_may_connect(&mut admin, &cluster.unique_name("absent"), &restricted)
                .await
                .unwrap(),
            None
        );
        admin.close().await.unwrap();
    })
    .await;
}

#[tokio::test]
#[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
async fn a_tenant_database_passes_until_it_drifts() {
    let Some(cluster) = Cluster::start().await else {
        return;
    };
    common::guard(async {
        let (role, database) = tenant_role(&cluster).await;
        cluster
            .create_database_named(database.as_str(), role.as_str())
            .await;
        let mut maintenance = maintenance_session(&cluster).await;
        privileges::check_tenant_database(&mut maintenance, &database, &role)
            .await
            .expect("an owned database with the default ACL passes");

        admin_sql(
            &cluster,
            &format!(
                "GRANT CONNECT ON DATABASE {} TO {}",
                quote(database.as_str()),
                quote(cluster.maintenance_role())
            ),
        )
        .await;
        let error = privileges::check_tenant_database(&mut maintenance, &database, &role)
            .await
            .unwrap_err();
        assert_eq!(error.code(), ErrorCode::Internal);
        assert_eq!(error.reason(), None);
        let source = source_of(&error);
        assert!(source.contains(database.as_str()), "{source}");
        assert!(
            source.contains(&format!(
                "role {} holds CONNECT",
                cluster.maintenance_role()
            )),
            "{source}"
        );

        let (other, absent) = tenant_role(&cluster).await;
        let error = privileges::check_tenant_database(&mut maintenance, &absent, &other)
            .await
            .unwrap_err();
        assert_eq!(error.code(), ErrorCode::Internal);
        assert!(source_of(&error).contains("does not exist"));
        maintenance.close().await.unwrap();
    })
    .await;
}
