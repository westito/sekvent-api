//! `SEKVENT_COMPONENT_<C>_ENDPOINT`: validation and the tonic endpoint built
//! from it.

use std::net::{Ipv4Addr, Ipv6Addr};
use std::time::Duration;

use sekvent_config::{ConfigError, ConfigSource};
use tonic::transport::Endpoint;

const EXPECTED: &str = "http://host:port";
const TLS: &str = "TLS endpoints are not supported yet; use http:// on a private network or \
                   behind a TLS-terminating proxy";

/// Read and validate the endpoint URL at `key`: `http://` + host + `:` +
/// port, optionally followed by one `/`. Returns the URL without that `/`.
/// `Ok(None)` when unset. Errors name the key, never the value.
pub(crate) fn read(source: &dyn ConfigSource, key: &str) -> Result<Option<String>, ConfigError> {
    source
        .get(key)
        .map(|raw| parse(&raw).map_err(|problem| problem.into_error(source.describe(key))))
        .transpose()
}

/// Why an endpoint value was rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Problem {
    Malformed,
    Invalid(&'static str),
}

impl Problem {
    fn into_error(self, key: String) -> ConfigError {
        match self {
            Self::Malformed => ConfigError::Malformed {
                key,
                expected: EXPECTED.to_owned(),
            },
            Self::Invalid(reason) => ConfigError::Invalid {
                key,
                reason: reason.to_owned(),
            },
        }
    }
}

fn parse(raw: &str) -> Result<String, Problem> {
    if raw
        .get(..8)
        .is_some_and(|scheme| scheme.eq_ignore_ascii_case("https://"))
    {
        return Err(Problem::Invalid(TLS));
    }
    let rest = raw.strip_prefix("http://").ok_or(Problem::Malformed)?;
    if rest.contains('@') {
        return Err(Problem::Invalid("must not carry user information"));
    }
    if rest.contains('?') || rest.contains('#') {
        return Err(Problem::Invalid("must not have a query or a fragment"));
    }
    let authority = rest.strip_suffix('/').unwrap_or(rest);
    if authority.contains('/') {
        return Err(Problem::Invalid("must not have a path"));
    }
    let (host, port) = split_host_port(authority).ok_or(Problem::Malformed)?;
    if !valid_host(host) || !valid_port(port) {
        return Err(Problem::Malformed);
    }
    Ok(format!("http://{authority}"))
}

/// `host:port` or `[v6]:port`; the host keeps its brackets.
fn split_host_port(authority: &str) -> Option<(&str, &str)> {
    if authority.starts_with('[') {
        let close = authority.find(']')?;
        let port = authority[close + 1..].strip_prefix(':')?;
        Some((&authority[..=close], port))
    } else {
        authority.rsplit_once(':')
    }
}

fn valid_host(host: &str) -> bool {
    if let Some(inner) = host.strip_prefix('[') {
        return inner
            .strip_suffix(']')
            .is_some_and(|address| address.parse::<Ipv6Addr>().is_ok());
    }
    if host.parse::<Ipv4Addr>().is_ok() {
        return true;
    }
    !host.is_empty()
        && host.len() <= 253
        && host.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        })
}

fn valid_port(port: &str) -> bool {
    !port.is_empty()
        && port.bytes().all(|byte| byte.is_ascii_digit())
        && port.parse::<u16>().is_ok_and(|port| port > 0)
}

/// The tonic endpoint for a validated URL: 5 s connect timeout, no Nagle,
/// HTTP/2 keep-alive pings every 30 s with a 10 s timeout. No I/O.
pub(crate) fn endpoint(url: &str) -> Result<Endpoint, tonic::transport::Error> {
    Ok(Endpoint::from_shared(url.to_owned())?
        .connect_timeout(Duration::from_secs(5))
        .tcp_nodelay(true)
        .http2_keep_alive_interval(Duration::from_secs(30))
        .keep_alive_timeout(Duration::from_secs(10)))
}

#[cfg(test)]
mod tests {
    use sekvent_config::MapSource;

    use super::*;

    const KEY: &str = "SEKVENT_COMPONENT_INVENTORY_ENDPOINT";

    fn read_one(value: &str) -> Result<Option<String>, ConfigError> {
        read(&MapSource::from_iter([(KEY, value)]), KEY)
    }

    #[test]
    fn unset_is_none() {
        assert_eq!(read(&MapSource::new(), KEY).unwrap(), None);
    }

    #[test]
    fn accepted_forms() {
        for (value, url) in [
            ("http://inventory:50051", "http://inventory:50051"),
            (
                "http://inventory.svc.local:1/",
                "http://inventory.svc.local:1",
            ),
            ("http://127.0.0.1:65535", "http://127.0.0.1:65535"),
            ("http://[::1]:8080", "http://[::1]:8080"),
            ("http://[::1]:8080/", "http://[::1]:8080"),
            ("http://a-b.c:80", "http://a-b.c:80"),
        ] {
            assert_eq!(read_one(value).unwrap().as_deref(), Some(url), "{value}");
            assert!(endpoint(url).is_ok(), "{url}");
        }
    }

    #[test]
    fn invalid_forms_name_the_key_and_a_reason() {
        for (value, reason) in [
            (
                "https://inventory:50051",
                "TLS endpoints are not supported yet",
            ),
            (
                "HTTPS://inventory:50051",
                "TLS endpoints are not supported yet",
            ),
            ("http://user:pw@inventory:50051", "user information"),
            ("http://inventory:50051/v1", "path"),
            ("http://inventory:50051//", "path"),
            ("http://inventory:50051?x=1", "query"),
            ("http://inventory:50051#top", "fragment"),
        ] {
            let error = read_one(value).unwrap_err();
            let ConfigError::Invalid { key, reason: text } = &error else {
                panic!("{value}: {error}");
            };
            assert_eq!(key, KEY);
            assert!(text.contains(reason), "{value}: {text}");
            assert!(!error.to_string().contains(value), "{error}");
        }
    }

    #[test]
    fn malformed_forms_expect_host_and_port() {
        for value in [
            "inventory:50051",
            "grpc://inventory:50051",
            "ftp://inventory:50051",
            "http://",
            "http://inventory",
            "http://inventory:",
            "http://inventory:0",
            "http://inventory:65536",
            "http://inventory:+80",
            "http://:80",
            "http://inv_entory:80",
            "http://-inventory:80",
            "http://a..b:80",
            "http://[::1]",
            "http://[::1]80",
            "http://[zz]:80",
            "http://[::1:80",
            "http://::1:80",
        ] {
            let error = read_one(value).unwrap_err();
            assert!(
                matches!(&error, ConfigError::Malformed { key, expected } if key == KEY && expected == EXPECTED),
                "{value}: {error}"
            );
        }
    }
}
