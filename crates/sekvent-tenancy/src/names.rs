//! Key ids, the object-name prefix and the checked role and database names.
//!
//! Utility statements take no bind parameters, so every role and database
//! name is formatted into SQL. The checked types below are the only way to a
//! formatted name, and neither grammar admits a quote, a space or an
//! upper-case letter.

use sekvent_error::AppError;

use uuid::Uuid;

use crate::TenantId;

/// The longest key id: a role-secret key id or an admin key id.
pub const KEY_ID_MAX: usize = 32;

/// `^[A-Za-z0-9_-]{1,32}$`: a role-secret key id, an admin key id.
pub fn is_key_id(text: &str) -> bool {
    (1..=KEY_ID_MAX).contains(&text.len())
        && text
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// The shortest object-name prefix.
const PREFIX_MIN: usize = 2;

/// The longest object-name prefix: with the 32 hex digits a name stays
/// within Postgres' 63 bytes.
const PREFIX_MAX: usize = 31;

/// The hex digits after the prefix.
const SUFFIX_LEN: usize = 32;

/// The longest database name Postgres keeps whole.
const DATABASE_NAME_MAX: usize = 63;

/// Databases every cluster has, never formatted into a statement.
const SYSTEM_DATABASES: [&str; 3] = ["postgres", "template0", "template1"];

/// `[a-z0-9_]`.
const fn is_name_byte(b: u8) -> bool {
    b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_'
}

/// Exactly 32 lower-case hex digits.
fn is_suffix(text: &str) -> bool {
    text.len() == SUFFIX_LEN
        && text
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// The product's object-name prefix and the names made from it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ObjectNames {
    /// Passed `^[a-z][a-z0-9_]{0,29}_$` and does not start with `pg_`.
    prefix: String,
}

impl ObjectNames {
    /// `^[a-z][a-z0-9_]{0,29}_$`, not starting with `pg_`.
    /// `INVALID_ARGUMENT` for any other text.
    pub fn new(prefix: &str) -> Result<Self, AppError> {
        let bytes = prefix.as_bytes();
        let valid = (PREFIX_MIN..=PREFIX_MAX).contains(&bytes.len())
            && bytes[0].is_ascii_lowercase()
            && bytes.iter().copied().all(is_name_byte)
            && prefix.ends_with('_')
            && !prefix.starts_with("pg_");
        if !valid {
            return Err(AppError::invalid_argument(
                "an object-name prefix is 2 to 31 characters of a-z, 0-9 and _, starts with a letter, ends with _ and does not start with pg_",
            ));
        }
        Ok(Self {
            prefix: prefix.to_owned(),
        })
    }

    /// The prefix, as given to [`ObjectNames::new`].
    pub fn prefix(&self) -> &str {
        &self.prefix
    }

    /// `<prefix><32 lower-case hex digits of the id>`.
    pub fn role(&self, tenant: TenantId) -> TenantRole {
        TenantRole {
            name: self.name_of(tenant),
        }
    }

    /// The same text as the role: a tenant's database is named like its role.
    pub fn database(&self, tenant: TenantId) -> DatabaseName {
        DatabaseName {
            name: self.name_of(tenant),
        }
    }

    /// Exactly a name [`ObjectNames::role`] can return.
    pub fn is_object_name(&self, name: &str) -> bool {
        self.suffix_of(name).is_some()
    }

    /// What the preflight treats as tenant-named: the name starts with the
    /// prefix.
    pub fn has_prefix(&self, name: &str) -> bool {
        name.starts_with(&self.prefix)
    }

    /// A stored role name, checked. `INTERNAL` for any other text.
    pub fn parse_role(&self, name: &str) -> Result<TenantRole, AppError> {
        if self.is_object_name(name) {
            Ok(TenantRole {
                name: name.to_owned(),
            })
        } else {
            Err(AppError::internal(
                "a stored role name is not an object name of this prefix",
            ))
        }
    }

    /// The tenant whose role or database `name` is, if it is an object name.
    pub fn tenant_of(&self, name: &str) -> Option<TenantId> {
        let suffix = self.suffix_of(name)?;
        Uuid::try_parse(suffix).ok().map(TenantId::from_uuid)
    }

    /// `<prefix><32 lower-case hex digits>`.
    fn name_of(&self, tenant: TenantId) -> String {
        let mut buffer = Uuid::encode_buffer();
        let hex = tenant.as_uuid().simple().encode_lower(&mut buffer);
        let mut name = String::with_capacity(self.prefix.len() + SUFFIX_LEN);
        name.push_str(&self.prefix);
        name.push_str(hex);
        name
    }

    /// The 32 hex digits of an object name, if `name` is one.
    fn suffix_of<'a>(&self, name: &'a str) -> Option<&'a str> {
        name.strip_prefix(self.prefix.as_str())
            .filter(|suffix| is_suffix(suffix))
    }
}

/// A tenant role's name. Only [`ObjectNames`] makes one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TenantRole {
    /// An object name of the [`ObjectNames`] that made it.
    name: String,
}

impl TenantRole {
    /// The role name.
    pub fn as_str(&self) -> &str {
        &self.name
    }
}

/// A database name this crate may format into a statement.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DatabaseName {
    /// Passed `^[a-z_][a-z0-9_]{0,62}$` and is no system database name.
    name: String,
}

impl DatabaseName {
    /// `^[a-z_][a-z0-9_]{0,62}$` and none of `postgres`, `template0`,
    /// `template1`. `INTERNAL` for any other text.
    pub fn parse(name: &str) -> Result<Self, AppError> {
        if is_adoptable_name(name) {
            Ok(Self {
                name: name.to_owned(),
            })
        } else {
            Err(AppError::internal(
                "a database name is outside the checked grammar or names a system database",
            ))
        }
    }

    /// The database name.
    pub fn as_str(&self) -> &str {
        &self.name
    }
}

/// The grammar of [`DatabaseName::parse`] without the three system names.
///
/// That is, the grammar with the three system names taken out of it: a name
/// this accepts is exactly a name [`DatabaseName::parse`] accepts.
pub fn is_adoptable_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    let grammar = (1..=DATABASE_NAME_MAX).contains(&bytes.len())
        && (bytes[0].is_ascii_lowercase() || bytes[0] == b'_')
        && bytes.iter().copied().all(is_name_byte);
    grammar && !SYSTEM_DATABASES.contains(&name)
}

#[cfg(test)]
mod tests {
    use sekvent_error::ErrorCode;

    use super::*;

    const ID: &str = "0b9c6f1e-2d4a-4c8b-9e3f-5a6b7c8d9e0f";
    const HEX: &str = "0b9c6f1e2d4a4c8b9e3f5a6b7c8d9e0f";

    fn names() -> ObjectNames {
        ObjectNames::new("app_t_").unwrap()
    }

    fn tenant() -> TenantId {
        TenantId::parse(ID).unwrap()
    }

    #[test]
    fn key_id_grammar() {
        let longest = "a".repeat(KEY_ID_MAX);
        let too_long = "a".repeat(KEY_ID_MAX + 1);
        for text in ["k", "K1", "key_2026-01", "A-Z_a-z_0-9", longest.as_str()] {
            assert!(is_key_id(text), "{text:?}");
        }
        for text in [
            "",
            too_long.as_str(),
            "key id",
            "key.1",
            "key:1",
            "k\u{e9}y",
            "'k'",
            "k/1",
        ] {
            assert!(!is_key_id(text), "{text:?}");
        }
    }

    #[test]
    fn prefix_grammar() {
        let longest = format!("{}_", "a".repeat(PREFIX_MAX - 1));
        for prefix in [
            "a_",
            "app_t_",
            "x9_",
            "ab_cd_",
            "pgx_",
            "p_",
            longest.as_str(),
        ] {
            let names = ObjectNames::new(prefix).unwrap();
            assert_eq!(names.prefix(), prefix);
        }
        let too_long = format!("{}_", "a".repeat(PREFIX_MAX));
        for prefix in [
            "",
            "_",
            "a",
            "app",
            "app_t",
            "_app_",
            "1app_",
            "App_",
            "app-t_",
            "app t_",
            "app\"_",
            "pg_",
            "pg_app_",
            "\u{e4}pp_",
            too_long.as_str(),
        ] {
            let error = ObjectNames::new(prefix).unwrap_err();
            assert_eq!(error.code(), ErrorCode::InvalidArgument, "{prefix:?}");
        }
    }

    #[test]
    fn role_and_database_carry_the_prefix_and_the_hex_id() {
        let names = names();
        let role = names.role(tenant());
        let database = names.database(tenant());
        assert_eq!(role.as_str(), format!("app_t_{HEX}"));
        assert_eq!(database.as_str(), role.as_str());
        assert_eq!(role.as_str().len(), 38);
        assert!(names.is_object_name(role.as_str()));
        assert!(DatabaseName::parse(database.as_str()).is_ok());
        assert_eq!(
            names.role(TenantId::LOCAL).as_str(),
            format!("app_t_{}", "0".repeat(32))
        );
    }

    #[test]
    fn the_longest_prefix_keeps_names_within_postgres_limits() {
        let prefix = format!("{}_", "a".repeat(PREFIX_MAX - 1));
        let names = ObjectNames::new(&prefix).unwrap();
        let database = names.database(tenant());
        assert_eq!(database.as_str().len(), DATABASE_NAME_MAX);
        assert_eq!(DatabaseName::parse(database.as_str()).unwrap(), database);
    }

    #[test]
    fn object_name_grammar() {
        let names = names();
        let upper = format!("app_t_{}", HEX.to_ascii_uppercase());
        let short = format!("app_t_{}", &HEX[1..]);
        let long = format!("app_t_{HEX}0");
        let other = format!("app_u_{HEX}");
        let longer_prefix = format!("app_t_x{}", &HEX[1..]);
        let non_hex = format!("app_t_{}g", &HEX[1..]);
        let hyphenated = format!("app_t_{ID}");
        for name in [
            upper.as_str(),
            short.as_str(),
            long.as_str(),
            other.as_str(),
            longer_prefix.as_str(),
            non_hex.as_str(),
            hyphenated.as_str(),
            HEX,
            "app_t_",
            "",
        ] {
            assert!(!names.is_object_name(name), "{name:?}");
            assert_eq!(names.tenant_of(name), None, "{name:?}");
            assert_eq!(
                names.parse_role(name).unwrap_err().code(),
                ErrorCode::Internal,
                "{name:?}"
            );
        }
    }

    #[test]
    fn has_prefix_is_a_plain_prefix_test() {
        let names = names();
        assert!(names.has_prefix("app_t_"));
        assert!(names.has_prefix("app_t_anything"));
        assert!(names.has_prefix(&format!("app_t_{HEX}")));
        assert!(!names.has_prefix("app_"));
        assert!(!names.has_prefix("APP_T_x"));
        assert!(!names.has_prefix("billing"));
    }

    #[test]
    fn a_stored_role_name_parses_back_to_its_tenant() {
        let names = names();
        let role = names.role(tenant());
        assert_eq!(names.parse_role(role.as_str()).unwrap(), role);
        assert_eq!(names.tenant_of(role.as_str()), Some(tenant()));
        assert_eq!(
            names.tenant_of(names.role(TenantId::LOCAL).as_str()),
            Some(TenantId::LOCAL)
        );
        let other = ObjectNames::new("orders_").unwrap();
        assert!(!other.is_object_name(role.as_str()));
        assert_eq!(other.tenant_of(role.as_str()), None);
    }

    #[test]
    fn database_name_grammar() {
        let longest = "d".repeat(DATABASE_NAME_MAX);
        let too_long = "d".repeat(DATABASE_NAME_MAX + 1);
        for name in [
            "billing",
            "_orders",
            "inventory_2",
            "a",
            longest.as_str(),
            "postgres_copy",
            "template2",
        ] {
            assert_eq!(DatabaseName::parse(name).unwrap().as_str(), name);
            assert!(is_adoptable_name(name), "{name:?}");
        }
        for name in [
            "",
            "bill\"ing",
            "bill'ing",
            "bill ing",
            " billing",
            "Billing",
            "billing-2",
            "2billing",
            "bill;ing",
            "b\u{e9}lling",
            too_long.as_str(),
            "postgres",
            "template0",
            "template1",
        ] {
            assert_eq!(
                DatabaseName::parse(name).unwrap_err().code(),
                ErrorCode::Internal,
                "{name:?}"
            );
            assert!(!is_adoptable_name(name), "{name:?}");
        }
    }

    #[test]
    fn checked_names_clone_and_compare() {
        let names = names();
        assert_eq!(names.clone(), names);
        let role = names.role(tenant());
        assert_eq!(role.clone(), role);
        assert!(format!("{role:?}").contains(HEX));
        let database = DatabaseName::parse("billing").unwrap();
        assert_eq!(database.clone(), database);
        assert_ne!(database, names.database(tenant()));
    }
}
