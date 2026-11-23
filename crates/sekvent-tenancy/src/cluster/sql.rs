//! Every utility statement of the crate, built only from [`TenantRole`],
//! [`DatabaseName`] and [`ScramVerifier`].
//!
//! Utility statements take no bind parameters, so names are formatted in;
//! the checked types are the only input. Catalog reads elsewhere are static
//! SQL with bound values. A statement that carries a verifier is a
//! [`Secret`].

use sekvent_config::Secret;
use sqlx::{AssertSqlSafe, PgConnection};

use crate::{DatabaseName, ScramVerifier, TenantRole};

impl TenantRole {
    /// The role name as a quoted identifier, `"<name>"`. The grammar holds
    /// no quote, so nothing is escaped.
    pub(crate) fn quoted(&self) -> String {
        format!("\"{}\"", self.as_str())
    }
}

impl DatabaseName {
    /// The database name as a quoted identifier, `"<name>"`. The grammar
    /// holds no quote, so nothing is escaped.
    pub(crate) fn quoted(&self) -> String {
        format!("\"{}\"", self.as_str())
    }
}

/// `RESET ROLE`.
pub(crate) const RESET_ROLE: &str = "RESET ROLE";

/// The attributes a tenant role is created and altered with.
const TENANT_ROLE_ATTRIBUTES: &str = "LOGIN NOINHERIT NOCREATEDB NOCREATEROLE";

/// `<verb> ROLE "<role>" <attributes> CONNECTION LIMIT <n> PASSWORD
/// '<verifier>'`.
fn role_statement(
    verb: &str,
    role: &TenantRole,
    verifier: &ScramVerifier,
    connection_limit: u32,
) -> Secret {
    Secret::new(format!(
        "{verb} ROLE {} {TENANT_ROLE_ATTRIBUTES} CONNECTION LIMIT {connection_limit} PASSWORD {}",
        role.quoted(),
        verifier.literal().expose()
    ))
}

/// `CREATE ROLE "<role>" LOGIN NOINHERIT NOCREATEDB NOCREATEROLE CONNECTION
/// LIMIT <n> PASSWORD '<verifier>'`. The caller checked the limit.
pub(crate) fn create_role(
    role: &TenantRole,
    verifier: &ScramVerifier,
    connection_limit: u32,
) -> Secret {
    role_statement("CREATE", role, verifier, connection_limit)
}

/// `ALTER ROLE "<role>" LOGIN NOINHERIT NOCREATEDB NOCREATEROLE CONNECTION
/// LIMIT <n> PASSWORD '<verifier>'`. The caller checked the limit.
pub(crate) fn alter_role(
    role: &TenantRole,
    verifier: &ScramVerifier,
    connection_limit: u32,
) -> Secret {
    role_statement("ALTER", role, verifier, connection_limit)
}

/// `GRANT "<role>" TO CURRENT_USER WITH INHERIT FALSE, SET TRUE`: the
/// maintenance membership, and its end state after a handover.
pub(crate) fn grant_to_current_user(role: &TenantRole) -> String {
    format!(
        "GRANT {} TO CURRENT_USER WITH INHERIT FALSE, SET TRUE",
        role.quoted()
    )
}

/// `GRANT "<role>" TO CURRENT_USER WITH INHERIT TRUE, SET TRUE`: the
/// handover's membership inside its transfer transaction only.
pub(crate) fn grant_to_current_user_inheriting(role: &TenantRole) -> String {
    format!(
        "GRANT {} TO CURRENT_USER WITH INHERIT TRUE, SET TRUE",
        role.quoted()
    )
}

/// `DROP ROLE IF EXISTS "<role>"`.
pub(crate) fn drop_role(role: &TenantRole) -> String {
    format!("DROP ROLE IF EXISTS {}", role.quoted())
}

/// `SET ROLE "<role>"`: session level, poisons the connection until
/// [`RESET_ROLE`] succeeded.
pub(crate) fn set_role(role: &TenantRole) -> String {
    format!("SET ROLE {}", role.quoted())
}

/// `SET LOCAL ROLE "<role>"`: until the end of the transaction.
pub(crate) fn set_local_role(role: &TenantRole) -> String {
    format!("SET LOCAL ROLE {}", role.quoted())
}

/// `CREATE DATABASE "<database>" OWNER "<owner>" ALLOW_CONNECTIONS false`.
pub(crate) fn create_database(database: &DatabaseName, owner: &TenantRole) -> String {
    format!(
        "CREATE DATABASE {} OWNER {} ALLOW_CONNECTIONS false",
        database.quoted(),
        owner.quoted()
    )
}

/// `REVOKE CONNECT ON DATABASE "<database>" FROM PUBLIC`.
pub(crate) fn revoke_connect_from_public(database: &DatabaseName) -> String {
    format!(
        "REVOKE CONNECT ON DATABASE {} FROM PUBLIC",
        database.quoted()
    )
}

/// `GRANT CONNECT ON DATABASE "<database>" TO "<role>"`.
pub(crate) fn grant_connect(database: &DatabaseName, role: &TenantRole) -> String {
    format!(
        "GRANT CONNECT ON DATABASE {} TO {}",
        database.quoted(),
        role.quoted()
    )
}

/// `ALTER DATABASE "<database>" WITH ALLOW_CONNECTIONS true`.
pub(crate) fn allow_connections(database: &DatabaseName) -> String {
    format!(
        "ALTER DATABASE {} WITH ALLOW_CONNECTIONS true",
        database.quoted()
    )
}

/// `DROP DATABASE IF EXISTS "<database>" WITH (FORCE)`.
pub(crate) fn drop_database(database: &DatabaseName) -> String {
    format!("DROP DATABASE IF EXISTS {} WITH (FORCE)", database.quoted())
}

/// `ALTER DATABASE "<database>" OWNER TO "<owner>"`.
pub(crate) fn alter_database_owner(database: &DatabaseName, owner: &TenantRole) -> String {
    format!(
        "ALTER DATABASE {} OWNER TO {}",
        database.quoted(),
        owner.quoted()
    )
}

/// `REASSIGN OWNED BY CURRENT_USER TO "<role>"`.
pub(crate) fn reassign_owned_to(role: &TenantRole) -> String {
    format!("REASSIGN OWNED BY CURRENT_USER TO {}", role.quoted())
}

/// Runs one statement built above as a simple query (no prepared-statement
/// cache, no bind parameters). The raw error is returned so the caller can
/// read its SQLSTATE before mapping it with `sekvent_db::to_app_error`.
pub(crate) async fn run(conn: &mut PgConnection, statement: &str) -> Result<(), sqlx::Error> {
    sqlx::raw_sql(AssertSqlSafe(statement))
        .execute(conn)
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use uuid::Uuid;

    use super::*;
    use crate::{ObjectNames, TenantId};

    /// `app_t_` and the 32 hex digits of the sample tenant.
    const NAME: &str = "app_t_0b9c2f4e51a84d3b9e6f7c1d2a3b4c5d";
    /// The verifier of RFC 7677's example: a 16-byte salt, two 32-byte keys.
    const VERIFIER: &str = "SCRAM-SHA-256$4096:W22ZaJ0SNY7soEsUEjb6gQ==$\
                            WG5d8oPm3OtcPnkdi4Uo7BkeZkBFzpcXkuLmtbsT4qY=:\
                            wfPLwcE6nTWhTAmQ7tl2KeoiWGPlZqQxSrmfPwDl2dU=";

    fn tenant() -> TenantId {
        TenantId::from_uuid(Uuid::from_u128(0x0b9c_2f4e_51a8_4d3b_9e6f_7c1d_2a3b_4c5d))
    }

    fn names() -> ObjectNames {
        ObjectNames::new("app_t_").expect("the prefix is valid")
    }

    fn role() -> TenantRole {
        names().role(tenant())
    }

    fn database() -> DatabaseName {
        names().database(tenant())
    }

    fn verifier() -> ScramVerifier {
        ScramVerifier::parse(Secret::new(VERIFIER)).expect("a valid verifier")
    }

    #[test]
    fn names_are_quoted_whole() {
        assert_eq!(role().as_str(), NAME);
        assert_eq!(role().quoted(), format!("\"{NAME}\""));
        assert_eq!(database().quoted(), format!("\"{NAME}\""));
        let adopted = DatabaseName::parse("orders_archive").expect("a valid name");
        assert_eq!(adopted.quoted(), "\"orders_archive\"");
    }

    #[test]
    fn role_statements_carry_the_verifier_only() {
        assert_eq!(
            create_role(&role(), &verifier(), 5).expose(),
            "CREATE ROLE \"app_t_0b9c2f4e51a84d3b9e6f7c1d2a3b4c5d\" LOGIN NOINHERIT NOCREATEDB \
             NOCREATEROLE CONNECTION LIMIT 5 PASSWORD \
             'SCRAM-SHA-256$4096:W22ZaJ0SNY7soEsUEjb6gQ==$\
             WG5d8oPm3OtcPnkdi4Uo7BkeZkBFzpcXkuLmtbsT4qY=:\
             wfPLwcE6nTWhTAmQ7tl2KeoiWGPlZqQxSrmfPwDl2dU='"
        );
        assert_eq!(
            alter_role(&role(), &verifier(), 2_147_483_647).expose(),
            "ALTER ROLE \"app_t_0b9c2f4e51a84d3b9e6f7c1d2a3b4c5d\" LOGIN NOINHERIT NOCREATEDB \
             NOCREATEROLE CONNECTION LIMIT 2147483647 PASSWORD \
             'SCRAM-SHA-256$4096:W22ZaJ0SNY7soEsUEjb6gQ==$\
             WG5d8oPm3OtcPnkdi4Uo7BkeZkBFzpcXkuLmtbsT4qY=:\
             wfPLwcE6nTWhTAmQ7tl2KeoiWGPlZqQxSrmfPwDl2dU='"
        );
        let printed = format!(
            "{:?} {}",
            create_role(&role(), &verifier(), 5),
            alter_role(&role(), &verifier(), 5)
        );
        assert!(!printed.contains("SCRAM"), "{printed}");
        assert!(!printed.contains(NAME), "{printed}");
    }

    #[test]
    fn every_other_statement_has_its_golden_text() {
        let role = role();
        let database = database();
        let cases = [
            (
                grant_to_current_user(&role),
                "GRANT \"app_t_0b9c2f4e51a84d3b9e6f7c1d2a3b4c5d\" TO CURRENT_USER \
                 WITH INHERIT FALSE, SET TRUE",
            ),
            (
                grant_to_current_user_inheriting(&role),
                "GRANT \"app_t_0b9c2f4e51a84d3b9e6f7c1d2a3b4c5d\" TO CURRENT_USER \
                 WITH INHERIT TRUE, SET TRUE",
            ),
            (
                drop_role(&role),
                "DROP ROLE IF EXISTS \"app_t_0b9c2f4e51a84d3b9e6f7c1d2a3b4c5d\"",
            ),
            (
                set_role(&role),
                "SET ROLE \"app_t_0b9c2f4e51a84d3b9e6f7c1d2a3b4c5d\"",
            ),
            (
                set_local_role(&role),
                "SET LOCAL ROLE \"app_t_0b9c2f4e51a84d3b9e6f7c1d2a3b4c5d\"",
            ),
            (
                create_database(&database, &role),
                "CREATE DATABASE \"app_t_0b9c2f4e51a84d3b9e6f7c1d2a3b4c5d\" \
                 OWNER \"app_t_0b9c2f4e51a84d3b9e6f7c1d2a3b4c5d\" ALLOW_CONNECTIONS false",
            ),
            (
                revoke_connect_from_public(&database),
                "REVOKE CONNECT ON DATABASE \"app_t_0b9c2f4e51a84d3b9e6f7c1d2a3b4c5d\" \
                 FROM PUBLIC",
            ),
            (
                grant_connect(&database, &role),
                "GRANT CONNECT ON DATABASE \"app_t_0b9c2f4e51a84d3b9e6f7c1d2a3b4c5d\" \
                 TO \"app_t_0b9c2f4e51a84d3b9e6f7c1d2a3b4c5d\"",
            ),
            (
                allow_connections(&database),
                "ALTER DATABASE \"app_t_0b9c2f4e51a84d3b9e6f7c1d2a3b4c5d\" \
                 WITH ALLOW_CONNECTIONS true",
            ),
            (
                drop_database(&database),
                "DROP DATABASE IF EXISTS \"app_t_0b9c2f4e51a84d3b9e6f7c1d2a3b4c5d\" \
                 WITH (FORCE)",
            ),
            (
                alter_database_owner(&database, &role),
                "ALTER DATABASE \"app_t_0b9c2f4e51a84d3b9e6f7c1d2a3b4c5d\" \
                 OWNER TO \"app_t_0b9c2f4e51a84d3b9e6f7c1d2a3b4c5d\"",
            ),
            (
                reassign_owned_to(&role),
                "REASSIGN OWNED BY CURRENT_USER TO \"app_t_0b9c2f4e51a84d3b9e6f7c1d2a3b4c5d\"",
            ),
            (RESET_ROLE.to_owned(), "RESET ROLE"),
        ];
        for (built, golden) in cases {
            assert_eq!(built, golden);
        }
    }

    #[test]
    fn an_adopted_database_name_is_formatted_like_a_generated_one() {
        let adopted = DatabaseName::parse("inventory").expect("a valid name");
        assert_eq!(
            drop_database(&adopted),
            "DROP DATABASE IF EXISTS \"inventory\" WITH (FORCE)"
        );
        assert_eq!(
            alter_database_owner(&adopted, &role()),
            "ALTER DATABASE \"inventory\" OWNER TO \"app_t_0b9c2f4e51a84d3b9e6f7c1d2a3b4c5d\""
        );
    }
}
