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
#[cfg(test)]
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
    const NOT_FUNCTIONS: [&str; 12] = [
        "a", "any", "as", "coalesce", "exists", "extract", "not", "operator", "owned", "reached",
        "reaching", "select",
    ];

    /// The characters Postgres builds operator names from.
    const OPERATOR_CHARACTERS: &str = "=<>!~+-*/%^|&@#";

    /// Keywords that compare with an operator the `search_path` resolves:
    /// `IN` and `NOT IN`, the pattern matches, `BETWEEN`, `IS DISTINCT
    /// FROM` (and `DISTINCT` with it), `NULLIF` and `OVERLAPS`.
    const COMPARING_KEYWORDS: [&str; 8] = [
        "between", "distinct", "ilike", "in", "like", "nullif", "overlaps", "similar",
    ];

    /// The prefix of an operator named in `pg_catalog`.
    const NAMED: &str = "OPERATOR(pg_catalog.";

    /// `sql` with the inside of every string literal blanked, so that a
    /// pattern or a word in a literal is not read as code. Right only for
    /// a statement [`unscannable`] passes.
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

    /// Why [`code_of`] could misread `sql`, if it could: a comment, an
    /// escape string or a dollar quote, each of which can hide a quote from
    /// it or add one. Every `$` must start a parameter.
    fn unscannable(sql: &str) -> Option<String> {
        let code = code_of(sql);
        if let Some(at) = code.find("--") {
            return Some(format!("a -- comment at byte {at}"));
        }
        if let Some(at) = code.find("/*") {
            return Some(format!("a /* comment at byte {at}"));
        }
        let bytes = code.as_bytes();
        let byte_at = |at: Option<usize>| at.and_then(|at| bytes.get(at)).copied();
        for (at, byte) in bytes.iter().enumerate() {
            let escape = matches!(byte_at(at.checked_sub(1)), Some(b'E' | b'e'))
                && !byte_at(at.checked_sub(2))
                    .is_some_and(|b| b.is_ascii_alphanumeric() || b == b'_');
            if *byte == b'\'' && escape {
                return Some(format!("an escape string at byte {}", at - 1));
            }
            if *byte == b'$' && !bytes.get(at + 1).is_some_and(u8::is_ascii_digit) {
                return Some(format!("a $ that starts no parameter at byte {at}"));
            }
        }
        None
    }

    /// The operator characters of `code` left once every operator named in
    /// `pg_catalog` and the `*` of `count(*)` are taken out, with their
    /// byte offsets: each one is an operator the `search_path` resolves.
    fn unnamed_operators(code: &str) -> Vec<(usize, char)> {
        let code = code.replace("count(*)", "count( )");
        let mut rest = code.as_str();
        let mut kept = String::new();
        while let Some(at) = rest.find(NAMED) {
            let after = &rest[at + NAMED.len()..];
            match after.find(')').map(|end| &after[..end]) {
                Some(name)
                    if !name.is_empty()
                        && name.chars().all(|c| OPERATOR_CHARACTERS.contains(c)) =>
                {
                    kept.push_str(&rest[..at]);
                    kept.push_str(&" ".repeat(NAMED.len() + name.len() + 1));
                    rest = &after[name.len() + 1..];
                }
                _ => {
                    kept.push_str(&rest[..at + NAMED.len()]);
                    rest = after;
                }
            }
        }
        kept.push_str(rest);
        kept.char_indices()
            .filter(|(_, c)| OPERATOR_CHARACTERS.contains(*c))
            .collect()
    }

    /// The comparing keywords of `code`, and each `CASE` that is not a
    /// searched one (`CASE WHEN`): a simple `CASE x WHEN` compares with `=`.
    fn comparing_keywords(code: &str) -> Vec<String> {
        let words: Vec<String> = code
            .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
            .filter(|word| !word.is_empty())
            .map(str::to_ascii_lowercase)
            .collect();
        let mut found: Vec<String> = words
            .iter()
            .filter(|word| COMPARING_KEYWORDS.contains(&word.as_str()))
            .cloned()
            .collect();
        for pair in words.windows(2) {
            if pair[0] == "case" && pair[1] != "when" {
                found.push(format!("case {}", pair[1]));
            }
        }
        if words.last().is_some_and(|word| word == "case") {
            found.push("case".to_owned());
        }
        found
    }

    /// The identifier that ends right before byte `end` of `text`, spaces
    /// between it and `end` skipped, and whether `pg_catalog.` qualifies it.
    fn word_before(text: &str, end: usize) -> (&str, bool) {
        let head = text[..end].trim_end();
        let start = head
            .rfind(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
            .map_or(0, |at| at + 1);
        (&head[start..], head[..start].ends_with("pg_catalog."))
    }

    #[test]
    fn the_scan_sees_every_module() {
        let names: Vec<&str> = statements().into_iter().map(|(name, _)| name).collect();
        for expected in [
            "CONNECTING_ROLES",
            "SESSION_ROLE_ATTRIBUTES",
            "SESSION_ROLE_REACH",
            "ROLE_FACTS",
            "DATABASE_FACTS",
        ] {
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
    fn every_statement_can_be_scanned() {
        for (name, sql) in statements() {
            assert_eq!(unscannable(sql), None, "{name}");
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
    fn every_operator_is_pg_catalogs_by_name() {
        for (name, sql) in statements() {
            let code = code_of(sql);
            assert_eq!(
                unnamed_operators(&code),
                [] as [(usize, char); 0],
                "{name}: an operator the search_path resolves"
            );
            assert_eq!(
                comparing_keywords(&code),
                [] as [String; 0],
                "{name}: a comparison the search_path resolves"
            );
        }
    }

    #[test]
    fn the_scan_finds_what_it_looks_for() {
        assert_eq!(word_before("x pg_catalog.count(*)", 18), ("count", true));
        assert_eq!(word_before("SELECT count(*)", 12), ("count", false));
        assert_eq!(word_before("EXISTS (", 7), ("EXISTS", false));
        assert_eq!(word_before("f (x)", 2), ("f", false));
        assert_eq!(word_before("pg_catalog.f\n  (x)", 15), ("f", true));
        assert_eq!(word_before("(", 0), ("", false));
        assert_eq!(word_before("a) (", 3), ("", false));
        assert_eq!(
            code_of("a ~ '^pg_(x)?$' AND f('d')"),
            "a ~ '         ' AND f(' ')"
        );
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn the_operator_scan_finds_every_unnamed_operator() {
        for named in [
            "a OPERATOR(pg_catalog.=) b",
            "a OPERATOR(pg_catalog.<>) $1",
            "a OPERATOR(pg_catalog.>=) 1::pg_catalog.oid",
            "n OPERATOR(pg_catalog.~) '^x+$'",
            "SELECT pg_catalog.count(*) FROM t",
            "x OPERATOR(pg_catalog.=) ANY (ARRAY[$1::pg_catalog.text])",
        ] {
            assert_eq!(
                unnamed_operators(&code_of(named)),
                [] as [(usize, char); 0],
                "{named}"
            );
        }
        let cases: [(&str, Vec<(usize, char)>); 12] = [
            ("a = b", vec![(2, '=')]),
            ("a <> b", vec![(2, '<'), (3, '>')]),
            ("a OPERATOR(public.=) b", vec![(18, '=')]),
            ("a operator(pg_catalog.=) b", vec![(22, '=')]),
            ("a OPERATOR(pg_catalog.=) b = c", vec![(27, '=')]),
            ("a OPERATOR(pg_catalog.) b", Vec::new()),
            ("a OPERATOR(pg_catalog.x) - b", vec![(25, '-')]),
            ("a OPERATOR(pg_catalog.=", vec![(22, '=')]),
            ("SELECT * FROM t", vec![(7, '*')]),
            ("x # y @ z", vec![(2, '#'), (6, '@')]),
            ("a || b", vec![(2, '|'), (3, '|')]),
            (
                "1 + 2 / 3 % 4 ^ 5 & 6 ! 7",
                vec![
                    (2, '+'),
                    (6, '/'),
                    (10, '%'),
                    (14, '^'),
                    (18, '&'),
                    (22, '!'),
                ],
            ),
        ];
        for (bare, found) in cases {
            assert_eq!(unnamed_operators(&code_of(bare)), found, "{bare}");
        }
        assert_eq!(
            comparing_keywords("x IN (1) AND y NOT LIKE z OR w BETWEEN a AND b"),
            ["in", "like", "between"]
        );
        assert_eq!(
            comparing_keywords("a IS DISTINCT FROM b, NULLIF(a, b), a ILIKE b"),
            ["distinct", "nullif", "ilike"]
        );
        assert_eq!(
            comparing_keywords("x SIMILAR TO y, (a, b) OVERLAPS (c, d)"),
            ["similar", "overlaps"]
        );
        assert_eq!(
            comparing_keywords("CASE x WHEN 1 THEN 2 END, CASE WHEN p THEN 1 END, case"),
            ["case x", "case"]
        );
        assert_eq!(
            comparing_keywords("SELECT m.member FROM t JOIN u ON true WHERE r.rolinherit"),
            [] as [String; 0]
        );
    }

    #[test]
    fn a_statement_the_scan_could_misread_is_refused() {
        for (sql, why) in [
            ("SELECT 1 -- a comment", "a -- comment at byte 9"),
            ("SELECT /* a */ 1", "a /* comment at byte 7"),
            ("SELECT E'\\'' = 1", "an escape string at byte 7"),
            ("SELECT e'x'", "an escape string at byte 7"),
            ("SELECT $$x$$", "a $ that starts no parameter at byte 7"),
            (
                "SELECT $tag$x$tag$",
                "a $ that starts no parameter at byte 7",
            ),
            (
                "SELECT 1 WHERE x = $",
                "a $ that starts no parameter at byte 19",
            ),
        ] {
            assert_eq!(unscannable(sql).as_deref(), Some(why), "{sql}");
        }
        for sql in [
            "SELECT 'a -- b', '/* c */', 'E''x'''",
            "SELECT name'x', type_e'x'",
            "SELECT $1, $2::pg_catalog.text",
            "SELECT 1",
        ] {
            assert_eq!(unscannable(sql), None, "{sql}");
        }
    }
}
