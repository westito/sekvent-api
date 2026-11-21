//! Connect options for tenant pools.

use std::fmt;

use sekvent_db::ConnectCandidates;
use sekvent_error::AppError;
use sqlx::postgres::PgConnectOptions;

use crate::{DatabaseName, RoleKey, RoleSecrets, TenantId, TenantRole};

/// Connect options for tenant pools. `Debug` prints the type name only.
#[derive(Clone)]
pub struct Connector {
    /// The server: host, port or socket, TLS settings, application name.
    /// Its user, password and database are never used.
    base: PgConnectOptions,
    /// The master secrets the role passwords are derived from.
    secrets: RoleSecrets,
}

impl Connector {
    /// `base` gives the server: host, port or socket, TLS settings,
    /// application name. Its user, password and database are never used.
    pub fn new(base: PgConnectOptions, secrets: RoleSecrets) -> Self {
        Self { base, secrets }
    }

    /// The master secrets.
    pub fn secrets(&self) -> &RoleSecrets {
        &self.secrets
    }

    /// The options of one tenant under one key.
    ///
    /// A clone of the base with user, password and database always replaced:
    /// the role, the password [`RoleSecrets::derive`] gives for `tenant`
    /// under `key`, and the database. Every other option of the base is
    /// kept. `INTERNAL` when the key derives no password.
    pub fn options(
        &self,
        tenant: TenantId,
        role: &TenantRole,
        database: &DatabaseName,
        key: &RoleKey,
    ) -> Result<PgConnectOptions, AppError> {
        let password = self.secrets.derive(tenant, key)?;
        Ok(self
            .base
            .clone()
            .username(role.as_str())
            .password(password.expose())
            .database(database.as_str()))
    }

    /// One candidate per key, in order. `INTERNAL` for no key.
    pub fn candidates(
        &self,
        tenant: TenantId,
        role: &TenantRole,
        database: &DatabaseName,
        keys: &[RoleKey],
    ) -> Result<ConnectCandidates, AppError> {
        let mut options = self
            .candidate_options(tenant, role, database, keys)?
            .into_iter();
        let Some(first) = options.next() else {
            return Err(AppError::internal(
                "a tenant connection needs at least one role key",
            ));
        };
        Ok(options.fold(ConnectCandidates::new(first), ConnectCandidates::or))
    }

    /// The options of every key, in the keys' order.
    fn candidate_options(
        &self,
        tenant: TenantId,
        role: &TenantRole,
        database: &DatabaseName,
        keys: &[RoleKey],
    ) -> Result<Vec<PgConnectOptions>, AppError> {
        keys.iter()
            .map(|key| self.options(tenant, role, database, key))
            .collect()
    }
}

impl fmt::Debug for Connector {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Connector(..)")
    }
}

#[cfg(test)]
mod tests {
    use sekvent_config::Secret;
    use sekvent_error::ErrorCode;
    use sqlx::ConnectOptions as _;
    use sqlx::postgres::PgSslMode;
    use uuid::Uuid;

    use super::*;
    use crate::ObjectNames;

    const SECRET: &str = "correct horse battery staple 936";
    const OTHER_SECRET: &str = "another secret value for testing";
    const CONTEXT: &str = "example-app.role.v1";
    const TENANT: &str = "0f8fad5b-d9cb-469f-a165-70867728950e";
    const PASSWORD: &str = "OYf0aWaTG6covLgwbEBL-T_7D3AQj2pe2UOV6g1yxFo";
    const BASE_PASSWORD: &str = "base-password-of-the-admin";

    fn tenant() -> TenantId {
        TenantId::from_uuid(Uuid::parse_str(TENANT).expect("a test UUID"))
    }

    fn key(key_id: &str, generation: u32) -> RoleKey {
        RoleKey {
            key_id: key_id.to_owned(),
            generation,
        }
    }

    fn base() -> PgConnectOptions {
        PgConnectOptions::new_without_pgpass()
            .host("db.example.com")
            .port(6543)
            .username("cluster_admin")
            .password(BASE_PASSWORD)
            .database("admin_database")
            .application_name("orders-api")
            .ssl_mode(PgSslMode::Require)
            .options([("lock_timeout", "5s")])
    }

    fn connector() -> Connector {
        let secrets = RoleSecrets::parse(
            &Secret::new(format!("k1:{SECRET},k2:{OTHER_SECRET}")),
            CONTEXT,
        )
        .expect("the test secrets parse");
        Connector::new(base(), secrets)
    }

    fn names() -> (TenantRole, DatabaseName) {
        let names = ObjectNames::new("app_t_").expect("a valid prefix");
        (names.role(tenant()), names.database(tenant()))
    }

    /// The password `options` carries, read back through its lossy URL.
    fn password_in(options: &PgConnectOptions) -> Option<String> {
        let url = options.to_url_lossy();
        let escaped = url.password()?.as_bytes().to_vec();
        let mut bytes = Vec::with_capacity(escaped.len());
        let mut at = 0;
        while at < escaped.len() {
            if escaped[at] == b'%' {
                let digits = std::str::from_utf8(&escaped[at + 1..at + 3]).expect("ASCII escape");
                bytes.push(u8::from_str_radix(digits, 16).expect("a hex escape"));
                at += 3;
            } else {
                bytes.push(escaped[at]);
                at += 1;
            }
        }
        Some(String::from_utf8(bytes).expect("a UTF-8 password"))
    }

    #[test]
    fn options_replace_the_base_credentials_and_keep_the_server() {
        let connector = connector();
        let (role, database) = names();
        let options = connector
            .options(tenant(), &role, &database, &key("k1", 1))
            .expect("options for the tenant");
        assert_eq!(options.get_username(), role.as_str());
        assert_eq!(options.get_database(), Some(database.as_str()));
        assert_eq!(options.get_host(), "db.example.com");
        assert_eq!(options.get_port(), 6543);
        assert_eq!(options.get_application_name(), Some("orders-api"));
        assert!(matches!(options.get_ssl_mode(), PgSslMode::Require));
        assert!(
            options
                .get_options()
                .is_some_and(|text| text.contains("lock_timeout=5s"))
        );
        assert_eq!(password_in(&options).as_deref(), Some(PASSWORD));
        let url = options.to_url_lossy();
        assert!(!url.as_str().contains(BASE_PASSWORD));
        assert!(!url.as_str().contains("cluster_admin"));
        assert!(!url.as_str().contains("admin_database"));
    }

    #[test]
    fn options_follow_the_key() {
        let connector = connector();
        let (role, database) = names();
        let password_of = |role_key: &RoleKey| {
            let options = connector
                .options(tenant(), &role, &database, role_key)
                .expect("options for the tenant");
            password_in(&options)
        };
        let first = password_of(&key("k1", 1));
        assert_eq!(first.as_deref(), Some(PASSWORD));
        assert_ne!(password_of(&key("k1", 2)), first);
        assert_ne!(password_of(&key("k2", 1)), first);
    }

    #[test]
    fn a_key_without_a_password_is_internal() {
        let connector = connector();
        let (role, database) = names();
        for role_key in [key("k1", 0), key("k9", 1)] {
            let error = connector
                .options(tenant(), &role, &database, &role_key)
                .expect_err("no options");
            assert_eq!(error.code(), ErrorCode::Internal);
        }
    }

    #[test]
    fn candidates_follow_the_keys_in_order() {
        let connector = connector();
        let (role, database) = names();
        let one = connector
            .candidates(tenant(), &role, &database, &[key("k1", 1)])
            .expect("one candidate");
        assert_eq!(format!("{one:?}"), "ConnectCandidates { count: 1 }");
        let three = connector
            .candidates(
                tenant(),
                &role,
                &database,
                &[key("k1", 2), key("k2", 1), key("k1", 1)],
            )
            .expect("three candidates");
        assert_eq!(format!("{three:?}"), "ConnectCandidates { count: 3 }");

        let keys = [key("k1", 2), key("k2", 1), key("k1", 1)];
        let ordered = connector
            .candidate_options(tenant(), &role, &database, &keys)
            .expect("three candidates");
        let expected: Vec<Option<String>> = keys
            .iter()
            .map(|role_key| {
                let options = connector
                    .options(tenant(), &role, &database, role_key)
                    .expect("options for the tenant");
                password_in(&options)
            })
            .collect();
        assert_eq!(
            ordered.iter().map(password_in).collect::<Vec<_>>(),
            expected
        );
        assert_eq!(expected[2].as_deref(), Some(PASSWORD), "k1:1 comes last");
        assert_ne!(expected[0], expected[1]);
        assert_ne!(expected[0], expected[2]);
    }

    #[test]
    fn candidates_without_a_key_or_with_a_bad_key_are_internal() {
        let connector = connector();
        let (role, database) = names();
        let none = connector
            .candidates(tenant(), &role, &database, &[])
            .expect_err("no key");
        assert_eq!(none.code(), ErrorCode::Internal);
        for keys in [
            vec![key("k9", 1), key("k1", 1)],
            vec![key("k1", 1), key("k1", 0)],
        ] {
            let error = connector
                .candidates(tenant(), &role, &database, &keys)
                .expect_err("a key derives no password");
            assert_eq!(error.code(), ErrorCode::Internal);
        }
    }

    #[test]
    fn debug_prints_nothing() {
        let connector = connector();
        assert_eq!(format!("{connector:?}"), "Connector(..)");
        assert_eq!(format!("{:?}", connector.clone()), "Connector(..)");
        assert_eq!(connector.secrets().len(), 2);
    }
}
