use sekvent_config::Secret;

use crate::DbError;

/// Where a database URL points: enough to tell two pools apart, nothing
/// secret.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Target {
    /// Backend family: `postgres`, `mysql`, or the scheme as written.
    pub scheme: String,
    /// Lower-cased host; loopback spellings collapse to `localhost`.
    pub host: String,
    /// Port, with the backend default filled in.
    pub port: u16,
    /// Database name; for Postgres an empty path means the user name.
    pub database: String,
}

/// Parse `url` into a [`Target`]. Errors describe the problem without
/// quoting the URL.
///
/// `postgresql` normalises to `postgres` and `mariadb` to `mysql`; default
/// ports (5432, 3306) are filled in; `127.0.0.1`, `::1` and `localhost` are
/// the same host.
pub fn parse_target(url: &str) -> Result<Target, &'static str> {
    let url = url.trim();
    let (scheme, rest) = url.split_once("://").ok_or("missing scheme")?;
    let scheme = match scheme.to_ascii_lowercase().as_str() {
        "postgres" | "postgresql" => "postgres".to_owned(),
        "mysql" | "mariadb" => "mysql".to_owned(),
        "" => return Err("missing scheme"),
        other => other.to_owned(),
    };
    let rest = rest.split(['?', '#']).next().unwrap_or_default();
    // The last `@` ends the user info, so an unencoded `@` or `/` in a
    // password does not leak into the host or database.
    let (user, after_user) = match rest.rfind('@') {
        Some(at) => {
            let user = rest[..at].split(':').next().unwrap_or_default();
            (user, &rest[at + 1..])
        }
        None => ("", rest),
    };
    let (host_port, path) = after_user.split_once('/').unwrap_or((after_user, ""));
    let (host, port) = split_host_port(host_port)?;
    let port = match port {
        Some(port) => port,
        None => default_port(&scheme).ok_or("missing port")?,
    };
    let mut database = path.trim_matches('/').to_owned();
    if database.is_empty() && scheme == "postgres" {
        user.clone_into(&mut database);
    }
    Ok(Target {
        scheme,
        host: normalise_host(host),
        port,
        database,
    })
}

fn split_host_port(host_port: &str) -> Result<(&str, Option<u16>), &'static str> {
    let (host, port) = if let Some(bracketed) = host_port.strip_prefix('[') {
        let (host, after) = bracketed.split_once(']').ok_or("unterminated IPv6 host")?;
        match after.strip_prefix(':') {
            Some(port) => (host, Some(port)),
            None if after.is_empty() => (host, None),
            None => return Err("unexpected text after IPv6 host"),
        }
    } else {
        match host_port.rsplit_once(':') {
            Some((host, port)) => (host, Some(port)),
            None => (host_port, None),
        }
    };
    let port = match port {
        Some(port) => Some(port.parse::<u16>().map_err(|_| "invalid port")?),
        None => None,
    };
    Ok((host, port))
}

fn default_port(scheme: &str) -> Option<u16> {
    match scheme {
        "postgres" => Some(5432),
        "mysql" => Some(3306),
        _ => None,
    }
}

fn normalise_host(host: &str) -> String {
    let host = host.to_ascii_lowercase();
    match host.as_str() {
        "" | "127.0.0.1" | "::1" | "localhost" | "localhost." => "localhost".to_owned(),
        _ => host,
    }
}

/// Fail if two configured pools point at the same database.
///
/// Guards against a copy-pasted URL making, say, the audit pool write into
/// the orders database. Pools with a blank URL are skipped. The error names
/// both pools and never prints a URL; an unparseable URL is reported by
/// pool name.
pub fn assert_distinct_targets(pools: &[(&str, &Secret)]) -> Result<(), DbError> {
    let mut seen: Vec<(&str, Target)> = Vec::with_capacity(pools.len());
    for (name, url) in pools {
        if url.is_blank() {
            continue;
        }
        let target = parse_target(url.expose()).map_err(|reason| DbError::BadUrl {
            pool: (*name).to_owned(),
            reason,
        })?;
        if let Some((first, _)) = seen.iter().find(|(_, other)| *other == target) {
            return Err(DbError::SameTarget {
                first: (*first).to_owned(),
                second: (*name).to_owned(),
            });
        }
        seen.push((name, target));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(scheme: &str, host: &str, port: u16, database: &str) -> Target {
        Target {
            scheme: scheme.to_owned(),
            host: host.to_owned(),
            port,
            database: database.to_owned(),
        }
    }

    #[test]
    fn urls_parse_into_targets() {
        assert_eq!(
            parse_target("postgres://app:pw@DB.Example.com:6543/orders?sslmode=require").unwrap(),
            target("postgres", "db.example.com", 6543, "orders")
        );
        assert_eq!(
            parse_target("postgresql://db/orders").unwrap(),
            target("postgres", "db", 5432, "orders")
        );
        assert_eq!(
            parse_target("mysql://root@db/billing#frag").unwrap(),
            target("mysql", "db", 3306, "billing")
        );
        assert_eq!(
            parse_target("mariadb://db:3307").unwrap(),
            target("mysql", "db", 3307, "")
        );
    }

    #[test]
    fn postgres_defaults_the_database_to_the_user() {
        assert_eq!(
            parse_target("postgres://reports:secret@db").unwrap(),
            target("postgres", "db", 5432, "reports")
        );
    }

    #[test]
    fn passwords_with_at_signs_do_not_confuse_the_host() {
        assert_eq!(
            parse_target("postgres://u:p@ss@db:5432/x").unwrap(),
            target("postgres", "db", 5432, "x")
        );
        assert_eq!(
            parse_target("postgres://u:p/ss@db/x").unwrap(),
            target("postgres", "db", 5432, "x")
        );
    }

    #[test]
    fn loopback_spellings_are_one_host() {
        for url in [
            "postgres://127.0.0.1/x",
            "postgres://[::1]/x",
            "postgres://LOCALHOST:5432/x",
            "postgres:///x",
        ] {
            assert_eq!(
                parse_target(url).unwrap(),
                target("postgres", "localhost", 5432, "x"),
                "{url}"
            );
        }
        assert_eq!(
            parse_target("postgres://[fe80::1]:6000/x").unwrap(),
            target("postgres", "fe80::1", 6000, "x")
        );
    }

    #[test]
    fn malformed_urls_are_rejected_without_quoting_them() {
        assert_eq!(parse_target("db:5432/x"), Err("missing scheme"));
        assert_eq!(parse_target("://db/x"), Err("missing scheme"));
        assert_eq!(parse_target("postgres://db:port/x"), Err("invalid port"));
        assert_eq!(parse_target("postgres://db:99999/x"), Err("invalid port"));
        assert_eq!(
            parse_target("postgres://[::1/x"),
            Err("unterminated IPv6 host")
        );
        assert_eq!(
            parse_target("postgres://[::1]x/x"),
            Err("unexpected text after IPv6 host")
        );
        assert_eq!(parse_target("sqlite://file/x"), Err("missing port"));
        assert_eq!(
            parse_target("redis://cache:6379/0").unwrap(),
            target("redis", "cache", 6379, "0")
        );
    }

    #[test]
    fn distinct_targets_pass() {
        let orders = Secret::new("postgres://u:p@db/orders");
        let audit = Secret::new("postgres://u:p@db/audit");
        let other_port = Secret::new("postgres://u:p@db:5433/orders");
        let mysql = Secret::new("mysql://u:p@db/orders");
        let unset = Secret::default();
        assert!(
            assert_distinct_targets(&[
                ("orders", &orders),
                ("audit", &audit),
                ("replica", &other_port),
                ("legacy", &mysql),
                ("reports", &unset),
                ("archive", &unset),
            ])
            .is_ok()
        );
    }

    #[test]
    fn the_same_target_is_rejected_naming_both_pools() {
        let orders = Secret::new("postgres://writer:pw1@DB:5432/orders");
        let audit = Secret::new("postgresql://auditor:pw2@db/orders?sslmode=require");
        let error = assert_distinct_targets(&[("orders", &orders), ("audit", &audit)]).unwrap_err();
        let message = error.to_string();
        assert_eq!(
            message,
            "database pools `orders` and `audit` point at the same database"
        );
        assert!(!message.contains("pw1") && !message.contains("pw2"));
    }

    #[test]
    fn an_unparseable_url_names_the_pool() {
        let bad = Secret::new("not a url with password hunter2");
        let error = assert_distinct_targets(&[("orders", &bad)]).unwrap_err();
        assert_eq!(
            error.to_string(),
            "database pool `orders` has an invalid URL: missing scheme"
        );
    }
}
