use sekvent_config::Secret;

use crate::DbError;

/// Where a database URL points: enough to tell two pools apart, nothing
/// secret.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Target {
    /// Backend family: `postgres`, `mysql`, or the scheme as written.
    pub scheme: String,
    /// Lower-cased host; loopback spellings collapse to `localhost`. A Unix
    /// socket directory or file is kept as its path.
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
/// ports (5432, 3306) are filled in; the host is lower-cased and
/// `127.0.0.1`, `::1` and `localhost` are the same host; user, host and
/// database are percent-decoded.
///
/// Query parameters that move the connection count: for Postgres `host`,
/// `hostaddr`, `port`, `dbname` and `user`, for MySQL `socket` (a Unix
/// socket path stands in for the host). With the `sqlx-postgres` or
/// `sqlx-mysql` feature the driver's own URL parser decides host, port,
/// user and database, so the identity is exactly where the pool connects,
/// including the driver's environment defaults (`PGHOST`, `PGPORT`, …).
/// Anything either parser rejects is an error: the check fails closed.
pub fn parse_target(url: &str) -> Result<Target, &'static str> {
    let target = parse_url(url.trim())?;
    match target.scheme.as_str() {
        #[cfg(feature = "sqlx-postgres")]
        "postgres" => postgres_target(url.trim()),
        #[cfg(feature = "sqlx-mysql")]
        "mysql" => mysql_target(url.trim()),
        _ => Ok(target),
    }
}

/// The target as the sqlx Postgres driver resolves it.
#[cfg(feature = "sqlx-postgres")]
fn postgres_target(url: &str) -> Result<Target, &'static str> {
    let options: sqlx::postgres::PgConnectOptions =
        url.parse().map_err(|_| "rejected by the Postgres driver")?;
    let host = match options.get_socket() {
        Some(socket) => socket.to_string_lossy().into_owned(),
        None => percent_decode(options.get_host())?,
    };
    let database = options
        .get_database()
        .unwrap_or_else(|| options.get_username());
    Ok(Target {
        scheme: "postgres".to_owned(),
        host: normalise_host(&host),
        port: options.get_port(),
        database: database.to_owned(),
    })
}

/// The target as the sqlx MySQL driver resolves it.
#[cfg(feature = "sqlx-mysql")]
fn mysql_target(url: &str) -> Result<Target, &'static str> {
    let options: sqlx::mysql::MySqlConnectOptions =
        url.parse().map_err(|_| "rejected by the MySQL driver")?;
    let host = match options.get_socket() {
        Some(socket) => socket.to_string_lossy().into_owned(),
        None => percent_decode(options.get_host())?,
    };
    Ok(Target {
        scheme: "mysql".to_owned(),
        host: normalise_host(&host),
        port: options.get_port(),
        database: options.get_database().unwrap_or_default().to_owned(),
    })
}

/// The backend-neutral parser: the URL's authority ends at the first `/`,
/// `?` or `#`, and the last `@` inside it ends the user info (as drivers
/// read URLs).
fn parse_url(url: &str) -> Result<Target, &'static str> {
    let (scheme, rest) = url.split_once("://").ok_or("missing scheme")?;
    let scheme = match scheme.to_ascii_lowercase().as_str() {
        "postgres" | "postgresql" => "postgres".to_owned(),
        "mysql" | "mariadb" => "mysql".to_owned(),
        "" => return Err("missing scheme"),
        other => other.to_owned(),
    };
    let rest = rest.split('#').next().unwrap_or_default();
    let (rest, query) = rest.split_once('?').unwrap_or((rest, ""));
    let (authority, path) = rest.split_once('/').unwrap_or((rest, ""));
    let (mut user, host_port) = match authority.rsplit_once('@') {
        Some((user_info, host_port)) => {
            let user = user_info.split(':').next().unwrap_or_default();
            (percent_decode(user)?, host_port)
        }
        None => (String::new(), authority),
    };
    let (host, port) = split_host_port(host_port)?;
    let port = match port {
        Some(port) => port,
        None => default_port(&scheme).ok_or("missing port")?,
    };
    let mut target = Target {
        scheme,
        host: percent_decode(host)?,
        port,
        database: percent_decode(path.trim_start_matches('/'))?,
    };
    for pair in query.split('&').filter(|pair| !pair.is_empty()) {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        let value = query_decode(value)?;
        match (target.scheme.as_str(), query_decode(key)?.as_str()) {
            ("postgres", "host" | "hostaddr") | ("mysql", "socket") => target.host = value,
            ("postgres", "port") => target.port = value.parse().map_err(|_| "invalid port")?,
            ("postgres", "dbname") => target.database = value,
            ("postgres", "user") => user = value,
            _ => {}
        }
    }
    if target.database.is_empty() && target.scheme == "postgres" {
        target.database = user;
    }
    target.host = normalise_host(&target.host);
    Ok(target)
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
    // A Unix socket directory is a path: case matters and it has no port
    // spelling to collapse.
    if host.starts_with('/') {
        return host.trim_end_matches('/').to_owned();
    }
    let host = host.to_ascii_lowercase();
    let host = host
        .strip_prefix('[')
        .and_then(|inner| inner.strip_suffix(']'))
        .unwrap_or(&host);
    let host = host.strip_suffix('.').unwrap_or(host);
    match host {
        "" | "127.0.0.1" | "::1" | "0:0:0:0:0:0:0:1" | "localhost" => "localhost".to_owned(),
        _ => host.to_owned(),
    }
}

/// Decode `%XX` escapes; the result must be UTF-8.
fn percent_decode(value: &str) -> Result<String, &'static str> {
    const BAD: &str = "invalid percent-encoding";
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let hex = bytes
                .get(index + 1..index + 3)
                .filter(|hex| hex.iter().all(u8::is_ascii_hexdigit))
                .ok_or(BAD)?;
            let hex = std::str::from_utf8(hex).map_err(|_| BAD)?;
            out.push(u8::from_str_radix(hex, 16).map_err(|_| BAD)?);
            index += 3;
        } else {
            out.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(out).map_err(|_| BAD)
}

/// Decode a query key or value: `+` is a space, then `%XX` escapes.
fn query_decode(value: &str) -> Result<String, &'static str> {
    percent_decode(&value.replace('+', " "))
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

    /// The Postgres driver fills a missing host, port, user or database from
    /// these; with any of them set its answer depends on the machine.
    fn driver_env_is_clean() -> bool {
        ["PGHOST", "PGHOSTADDR", "PGPORT", "PGUSER", "PGDATABASE"]
            .iter()
            .all(|var| std::env::var_os(var).is_none())
    }

    /// Both parsers must agree on these: the driver-backed one (all
    /// features) through `parse_target`, the neutral one directly.
    fn both(url: &str) -> Vec<Result<Target, &'static str>> {
        let mut parsed = vec![parse_url(url)];
        if driver_env_is_clean() {
            parsed.push(parse_target(url));
        }
        parsed
    }

    fn assert_both(url: &str, expected: &Target) {
        for parsed in both(url) {
            assert_eq!(parsed.as_ref(), Ok(expected), "{url}");
        }
    }

    #[test]
    fn urls_parse_into_targets() {
        assert_both(
            "postgres://app:pw@DB.Example.com:6543/orders?sslmode=require",
            &target("postgres", "db.example.com", 6543, "orders"),
        );
        assert_both(
            "postgresql://app@db/orders",
            &target("postgres", "db", 5432, "orders"),
        );
        assert_both(
            "mysql://root@db/billing#frag",
            &target("mysql", "db", 3306, "billing"),
        );
        assert_both("mariadb://db:3307", &target("mysql", "db", 3307, ""));
        assert_both(
            "postgres://app@db.example.com./orders",
            &target("postgres", "db.example.com", 5432, "orders"),
        );
    }

    #[test]
    fn postgres_defaults_the_database_to_the_user() {
        assert_both(
            "postgres://reports:secret@db",
            &target("postgres", "db", 5432, "reports"),
        );
        assert_both(
            "postgres://app@db?user=reports",
            &target("postgres", "db", 5432, "reports"),
        );
    }

    #[test]
    fn passwords_with_at_signs_do_not_confuse_the_host() {
        assert_both(
            "postgres://u:p@ss@db:5432/x",
            &target("postgres", "db", 5432, "x"),
        );
        // An unencoded `/` ends the authority, as it does for the drivers.
        for parsed in both("postgres://u:p/ss@db/x") {
            assert!(parsed.is_err());
        }
    }

    #[test]
    fn names_are_percent_decoded() {
        assert_both(
            "postgres://app@db/%6Frders",
            &target("postgres", "db", 5432, "orders"),
        );
        assert_both(
            "postgres://%72eports@db",
            &target("postgres", "db", 5432, "reports"),
        );
        assert_both(
            "mysql://root@db/bill%69ng",
            &target("mysql", "db", 3306, "billing"),
        );
        assert_eq!(
            parse_url("postgres://app@%64b/x"),
            Ok(target("postgres", "db", 5432, "x"))
        );
    }

    #[test]
    fn postgres_query_parameters_move_the_target() {
        assert_both(
            "postgres://app@db/audit?dbname=orders",
            &target("postgres", "db", 5432, "orders"),
        );
        assert_both(
            "postgres://app@db/orders?host=Replica&port=5433",
            &target("postgres", "replica", 5433, "orders"),
        );
        assert_both(
            "postgres://app@db/orders?hostaddr=10.0.0.7",
            &target("postgres", "10.0.0.7", 5432, "orders"),
        );
        assert_both(
            "postgres://app@db/orders?host=%2Fvar%2Frun%2Fpostgresql%2F",
            &target("postgres", "/var/run/postgresql", 5432, "orders"),
        );
        assert_both(
            "postgres://app@db/x?dbname=my+db%21&sslmode=disable",
            &target("postgres", "db", 5432, "my db!"),
        );
        for parsed in both("postgres://app@db/orders?port=many") {
            assert!(parsed.is_err());
        }
    }

    #[test]
    fn mysql_sockets_stand_in_for_the_host() {
        assert_both(
            "mysql://root@localhost/billing?socket=%2Ftmp%2Fmysql.sock",
            &target("mysql", "/tmp/mysql.sock", 3306, "billing"),
        );
        // Postgres-only parameters do not move a MySQL target.
        assert_both(
            "mysql://root@db/billing?dbname=orders&host=other",
            &target("mysql", "db", 3306, "billing"),
        );
    }

    #[test]
    fn loopback_spellings_are_one_host() {
        for url in [
            "postgres://app@127.0.0.1/x",
            "postgres://app@[::1]/x",
            "postgres://app@LOCALHOST:5432/x",
            "postgres://app@localhost./x",
        ] {
            assert_both(url, &target("postgres", "localhost", 5432, "x"));
        }
        // Without a host the driver may pick a socket directory, so only the
        // neutral parser has a fixed answer.
        assert_eq!(
            parse_url("postgres:///x"),
            Ok(target("postgres", "localhost", 5432, "x"))
        );
        assert_both(
            "postgres://app@[fe80::1]:6000/x",
            &target("postgres", "fe80::1", 6000, "x"),
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
        for bad in [
            "postgres://db/%",
            "postgres://db/%6",
            "postgres://db/%zz",
            "postgres://db/%+1x",
            "postgres://db/%FF",
            "postgres://db/x?dbname=%",
            "postgres://db/x?%=y",
        ] {
            assert_eq!(parse_url(bad), Err("invalid percent-encoding"), "{bad}");
            assert!(parse_target(bad).is_err(), "{bad}");
        }
    }

    #[cfg(feature = "sqlx-postgres")]
    #[test]
    fn the_postgres_driver_rejections_are_errors() {
        assert_eq!(
            postgres_target("postgres://app@db/x?hostaddr=not-an-ip"),
            Err("rejected by the Postgres driver")
        );
        if driver_env_is_clean() {
            assert_eq!(
                postgres_target("postgres://app@db/x?host=%2Frun%2Fpg"),
                Ok(target("postgres", "/run/pg", 5432, "x"))
            );
        }
    }

    #[cfg(feature = "sqlx-mysql")]
    #[test]
    fn the_mysql_driver_rejections_are_errors() {
        assert_eq!(
            mysql_target("mysql://root@db:99999/x"),
            Err("rejected by the MySQL driver")
        );
    }

    #[test]
    fn distinct_targets_pass() {
        let orders = Secret::new("postgres://u:p@db/orders");
        let audit = Secret::new("postgres://u:p@db/audit");
        let other_port = Secret::new("postgres://u:p@db:5433/orders");
        let other_host = Secret::new("postgres://u:p@db/orders?host=replica");
        let mysql = Secret::new("mysql://u:p@db/orders");
        let unset = Secret::default();
        assert!(
            assert_distinct_targets(&[
                ("orders", &orders),
                ("audit", &audit),
                ("replica", &other_port),
                ("standby", &other_host),
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
    fn disguised_duplicates_are_rejected() {
        for (first, second) in [
            (
                "postgres://u:pw1@db/orders",
                "postgres://u:pw2@db/audit?dbname=orders",
            ),
            ("postgres://u@db/orders", "postgres://u@db/%6Frders"),
            (
                "postgres://u@db/orders",
                "postgres://u@elsewhere/orders?host=db&port=5432",
            ),
            ("mysql://u@db/billing", "mysql://u@DB:3306/bill%69ng"),
        ] {
            let first = Secret::new(first);
            let second = Secret::new(second);
            let error =
                assert_distinct_targets(&[("orders", &first), ("audit", &second)]).unwrap_err();
            assert!(matches!(error, DbError::SameTarget { .. }), "{error}");
            assert!(!error.to_string().contains("pw"));
        }
    }

    #[test]
    fn an_unparseable_url_names_the_pool() {
        let bad = Secret::new("not a url with password hunter2");
        let error = assert_distinct_targets(&[("orders", &bad)]).unwrap_err();
        assert_eq!(
            error.to_string(),
            "database pool `orders` has an invalid URL: missing scheme"
        );
        let bad = Secret::new("postgres://u:hunter2@db/x?port=hunter2");
        let error = assert_distinct_targets(&[("orders", &bad)]).unwrap_err();
        assert!(!error.to_string().contains("hunter2"));
    }
}
