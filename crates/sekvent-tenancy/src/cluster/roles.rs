//! Tenant roles: created and altered with a verifier only, checked and never
//! repaired, dropped after their database.

use sekvent_db::to_app_error;
use sekvent_error::AppError;
use sqlx::PgConnection;
use tokio::time::timeout;

use crate::cluster::maintenance::{Failure, OPERATION_TIMEOUT, begin, end_transaction, own_role};
use crate::cluster::{MaintenanceConn, privileges, sql};
use crate::{ScramVerifier, TenantRole};

/// The highest connection limit: Postgres stores it as an `int4`.
const CONNECTION_LIMIT_MAX: u32 = i32::MAX.unsigned_abs();

/// Whether a role of the name exists.
const ROLE_EXISTS: &str = "SELECT EXISTS (SELECT 1 FROM pg_catalog.pg_roles WHERE rolname = $1)";

/// Every catalog statement of this module, by name, for the scan of
/// `cluster::tests`.
#[cfg(test)]
pub(super) const STATEMENTS: [(&str, &str); 1] = [("ROLE_EXISTS", ROLE_EXISTS)];

/// Whether `role` exists.
pub async fn exists(conn: &mut MaintenanceConn, role: &TenantRole) -> Result<bool, AppError> {
    own_role(conn.names(), role)?;
    let outcome = timeout(OPERATION_TIMEOUT, async {
        let session = conn.live()?;
        role_exists(session, role).await
    })
    .await;
    conn.bounded(outcome)
}

/// Creates `role`, or converges an existing one that passes the tenant-role
/// check on `verifier` and `connection_limit`, and grants it to the
/// maintenance role `WITH INHERIT FALSE, SET TRUE`. `connection_limit` is 1
/// to `i32::MAX`; any other value is `INTERNAL`.
///
/// Everything runs in one transaction, which ends with the tenant-role check
/// of the result: an existing role with drift is refused and left exactly as
/// it was, never repaired.
pub async fn create(
    conn: &mut MaintenanceConn,
    role: &TenantRole,
    verifier: &ScramVerifier,
    connection_limit: u32,
) -> Result<(), AppError> {
    check_connection_limit(connection_limit)?;
    own_role(conn.names(), role)?;
    let outcome = timeout(OPERATION_TIMEOUT, async {
        let session = conn.live()?;
        create_in_transaction(session, role, verifier, connection_limit).await
    })
    .await;
    conn.bounded(outcome)
}

/// `DROP ROLE IF EXISTS`, after the role's database is gone.
pub async fn drop(conn: &mut MaintenanceConn, role: &TenantRole) -> Result<(), AppError> {
    own_role(conn.names(), role)?;
    let outcome = timeout(OPERATION_TIMEOUT, async {
        let session = conn.live()?;
        sql::run(session, &sql::drop_role(role))
            .await
            .map_err(to_app_error)
    })
    .await;
    conn.bounded(outcome)
}

/// `INTERNAL` unless `connection_limit` is 1 to `i32::MAX`: 0 locks the role
/// out and -1 (which `u32` cannot carry anyway) is unlimited.
fn check_connection_limit(connection_limit: u32) -> Result<(), AppError> {
    if (1..=CONNECTION_LIMIT_MAX).contains(&connection_limit) {
        Ok(())
    } else {
        Err(AppError::internal(
            "the role's connection limit is outside 1 to i32::MAX",
        ))
    }
}

/// Whether a role named like `role` exists, on `session`.
async fn role_exists(session: &mut PgConnection, role: &TenantRole) -> Result<bool, AppError> {
    sqlx::query_scalar::<_, bool>(ROLE_EXISTS)
        .bind(role.as_str())
        .fetch_one(session)
        .await
        .map_err(to_app_error)
}

/// The body of [`create`] in one transaction, committed only when every
/// step passed and rolled back (and the rollback awaited) on any refusal.
async fn create_in_transaction(
    session: &mut PgConnection,
    role: &TenantRole,
    verifier: &ScramVerifier,
    connection_limit: u32,
) -> Result<(), Failure> {
    let mut tx = begin(session).await?;
    let result = create_steps(&mut tx, role, verifier, connection_limit).await;
    end_transaction(tx, result).await
}

/// Read, check an existing role, create or alter, grant, check the result.
async fn create_steps(
    tx: &mut PgConnection,
    role: &TenantRole,
    verifier: &ScramVerifier,
    connection_limit: u32,
) -> Result<(), AppError> {
    let statement = if role_exists(tx, role).await? {
        privileges::check_tenant_role(tx, role).await?;
        sql::alter_role(role, verifier, connection_limit)
    } else {
        sql::create_role(role, verifier, connection_limit)
    };
    sql::run(tx, statement.expose())
        .await
        .map_err(to_app_error)?;
    sql::run(tx, &sql::grant_to_current_user(role))
        .await
        .map_err(to_app_error)?;
    privileges::check_tenant_role(tx, role).await
}

#[cfg(test)]
mod tests {
    use std::error::Error as _;

    use sekvent_config::Secret;
    use sekvent_error::ErrorCode;
    use uuid::Uuid;

    use super::*;
    use crate::{ObjectNames, TenantId};

    /// The verifier of RFC 7677's example: a 16-byte salt, two 32-byte keys.
    const VERIFIER: &str = "SCRAM-SHA-256$4096:W22ZaJ0SNY7soEsUEjb6gQ==$\
                            WG5d8oPm3OtcPnkdi4Uo7BkeZkBFzpcXkuLmtbsT4qY=:\
                            wfPLwcE6nTWhTAmQ7tl2KeoiWGPlZqQxSrmfPwDl2dU=";

    fn names() -> ObjectNames {
        ObjectNames::new("app_t_").expect("the prefix is valid")
    }

    fn tenant() -> TenantId {
        TenantId::from_uuid(Uuid::from_u128(0x0b9c_2f4e_51a8_4d3b_9e6f_7c1d_2a3b_4c5d))
    }

    fn verifier() -> ScramVerifier {
        ScramVerifier::parse(Secret::new(VERIFIER)).expect("a valid verifier")
    }

    fn source(error: &AppError) -> String {
        error.source().map(ToString::to_string).unwrap_or_default()
    }

    #[test]
    fn the_connection_limit_is_one_to_i32_max() {
        for refused in [0, CONNECTION_LIMIT_MAX + 1, u32::MAX] {
            let error = check_connection_limit(refused).expect_err("out of range");
            assert_eq!(error.code(), ErrorCode::Internal);
            assert_eq!(error.reason(), None);
        }
        for accepted in [1, 20, CONNECTION_LIMIT_MAX] {
            check_connection_limit(accepted).expect("in range");
        }
        assert_eq!(CONNECTION_LIMIT_MAX, 2_147_483_647);
    }

    #[tokio::test]
    async fn arguments_are_refused_before_the_connection_is_looked_at() {
        let names = names();
        let mut conn = MaintenanceConn::detached(names.clone(), "orders_maintenance");
        conn.mark_role_switched();
        let role = names.role(tenant());
        let foreign = ObjectNames::new("billing_t_")
            .expect("the prefix is valid")
            .role(tenant());

        let error = create(&mut conn, &role, &verifier(), 0)
            .await
            .expect_err("a limit of 0");
        assert!(
            source(&error).contains("connection limit"),
            "{}",
            source(&error)
        );
        let error = create(&mut conn, &foreign, &verifier(), 5)
            .await
            .expect_err("another prefix");
        assert!(source(&error).contains("not an object name"));
        let error = exists(&mut conn, &foreign)
            .await
            .expect_err("another prefix");
        assert!(source(&error).contains("not an object name"));
        let error = drop(&mut conn, &foreign).await.expect_err("another prefix");
        assert!(source(&error).contains("not an object name"));

        for error in [
            create(&mut conn, &role, &verifier(), 5).await,
            exists(&mut conn, &role).await.map(|_| ()),
            drop(&mut conn, &role).await,
        ] {
            let error = error.expect_err("the connection is poisoned");
            assert_eq!(error.code(), ErrorCode::Internal);
            assert!(source(&error).contains("poisoned"), "{}", source(&error));
        }
    }
}
