//! Cluster operations: they take checked names and verifiers and know no
//! tenant row.
//!
//! [`privileges`] and [`preflight`] read the catalogs on any connection and
//! change nothing. With `lifecycle`, the [`maintenance`] connection runs
//! [`roles`], [`databases`] and the [`handover`] of an adoption as a role that
//! is not a superuser.

pub mod preflight;
pub mod privileges;

#[cfg(feature = "lifecycle")]
pub mod databases;
#[cfg(feature = "lifecycle")]
pub mod handover;
#[cfg(feature = "lifecycle")]
pub mod maintenance;
#[cfg(feature = "lifecycle")]
pub mod roles;
#[cfg(feature = "lifecycle")]
mod sql;
#[cfg(all(test, feature = "lifecycle"))]
mod testing;

#[cfg(feature = "lifecycle")]
pub use maintenance::{Maintenance, MaintenanceConn, MaintenancePool};

#[cfg(test)]
mod tests {
    use super::{preflight, privileges};

    /// Every catalog statement of the module, by name.
    fn statements() -> Vec<(&'static str, &'static str)> {
        let mut all = Vec::new();
        all.extend(preflight::STATEMENTS);
        all.extend(privileges::STATEMENTS);
        #[cfg(feature = "lifecycle")]
        {
            all.extend(super::databases::STATEMENTS);
            all.extend(super::handover::STATEMENTS);
            all.extend(super::maintenance::STATEMENTS);
            all.extend(super::roles::STATEMENTS);
        }
        all
    }

    /// The words a statement may put right before `(` without naming a
    /// function: syntax, and the column lists of a CTE or an alias.
    const NOT_FUNCTIONS: [&str; 7] = [
        "coalesce", "extract", "operator", "reaching", "reached", "a", "exists",
    ];

    /// `sql` with the inside of every string literal blanked, so that a
    /// pattern or a word in a literal is not read as code.
    fn code_of(sql: &str) -> String {
        let mut in_literal = false;
        sql.chars()
            .map(|c| {
                if c == '\'' {
                    in_literal = !in_literal;
                    c
                } else if in_literal {
                    ' '
                } else {
                    c
                }
            })
            .collect()
    }

    /// The identifier that ends right before byte `end` of `text`, and
    /// whether `pg_catalog.` qualifies it.
    fn word_before(text: &str, end: usize) -> (&str, bool) {
        let head = &text[..end];
        let start = head
            .rfind(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
            .map_or(0, |at| at + 1);
        (&head[start..], head[..start].ends_with("pg_catalog."))
    }

    #[test]
    fn the_scan_sees_every_module() {
        let names: Vec<&str> = statements().into_iter().map(|(name, _)| name).collect();
        for expected in ["CONNECTING_ROLES", "ROLE_FACTS", "DATABASE_FACTS"] {
            assert!(names.contains(&expected), "{names:?}");
        }
        #[cfg(feature = "lifecycle")]
        for expected in [
            "FOREIGN_OBJECTS",
            "OTHER_SESSIONS",
            "DROP_FACTS",
            "SESSION_STATE",
            "ROLE_EXISTS",
        ] {
            assert!(names.contains(&expected), "{names:?}");
        }
    }

    #[test]
    fn a_class_is_never_compared_as_a_bare_regclass() {
        for (name, sql) in statements() {
            for (at, _) in sql.match_indices("::pg_catalog.regclass") {
                let rest = &sql[at + "::pg_catalog.regclass".len()..];
                assert!(
                    rest.starts_with("::pg_catalog.oid"),
                    "{name}: a regclass not cast on to oid"
                );
            }
            assert!(
                !sql.to_ascii_lowercase().contains("::regclass"),
                "{name}: an unqualified regclass"
            );
        }
    }

    #[test]
    fn every_cast_names_a_pg_catalog_type() {
        for (name, sql) in statements() {
            for (at, _) in sql.match_indices("::") {
                assert!(
                    sql[at + 2..].starts_with("pg_catalog."),
                    "{name}: an unqualified cast at byte {at}"
                );
            }
        }
    }

    #[test]
    fn every_function_is_qualified() {
        for (name, sql) in statements() {
            let code = code_of(sql);
            for (at, _) in code.match_indices('(') {
                let (word, qualified) = word_before(&code, at);
                if word.is_empty() || qualified {
                    continue;
                }
                assert!(
                    NOT_FUNCTIONS.contains(&word.to_ascii_lowercase().as_str()),
                    "{name}: an unqualified function {word}"
                );
            }
        }
    }

    #[test]
    fn a_pattern_match_uses_pg_catalogs_operator_by_name() {
        for (name, sql) in statements() {
            let rest = sql.replace("OPERATOR(pg_catalog.~)", "");
            assert!(!rest.contains('~'), "{name}: an unnamed pattern operator");
        }
    }

    #[test]
    fn the_scan_finds_what_it_looks_for() {
        assert_eq!(word_before("x pg_catalog.count(*)", 18), ("count", true));
        assert_eq!(word_before("SELECT count(*)", 12), ("count", false));
        assert_eq!(word_before("EXISTS (", 7), ("", false));
        assert_eq!(word_before("(", 0), ("", false));
        assert_eq!(
            code_of("a ~ '^pg_(x)?$' AND f('d')"),
            "a ~ '         ' AND f(' ')"
        );
    }
}
