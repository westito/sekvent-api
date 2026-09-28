#[cfg(any(feature = "postgres", feature = "mysql"))]
use std::fmt::Write as _;
use testcontainers::core::ContainerPort;
use testcontainers::{ContainerAsync, Image};

use crate::HarnessError;

/// Environment variable naming the host that publishes container ports, for
/// a Docker daemon that is not on this machine.
///
/// testcontainers reports the daemon's own idea of the host, which is wrong
/// when the daemon is reached through a tunnel or a remote `DOCKER_HOST`.
pub const HOST_OVERRIDE_ENV: &str = "TESTCONTAINERS_HOST_OVERRIDE";

/// The host `container` is reachable on: [`HOST_OVERRIDE_ENV`] when set,
/// otherwise the host testcontainers reports. Never a hardcoded loopback.
///
/// The result is ready to embed in a URL (IPv6 addresses are bracketed).
pub async fn container_addr<I: Image>(
    container: &ContainerAsync<I>,
) -> Result<String, HarnessError> {
    let reported = container.get_host().await?.to_string();
    Ok(resolve_host(
        std::env::var(HOST_OVERRIDE_ENV).ok().as_deref(),
        &reported,
    ))
}

/// The `(host, port)` pair on which `internal_port` of `container` is
/// published.
pub async fn mapped_addr<I: Image>(
    container: &ContainerAsync<I>,
    internal_port: ContainerPort,
) -> Result<(String, u16), HarnessError> {
    let host = container_addr(container).await?;
    let port = container.get_host_port_ipv4(internal_port).await?;
    Ok((host, port))
}

/// Pure host selection behind [`container_addr`]: a non-blank override wins
/// over the reported host; a bare IPv6 address is bracketed for URL use.
pub fn resolve_host(override_value: Option<&str>, reported: &str) -> String {
    let chosen = match override_value.map(str::trim) {
        Some(value) if !value.is_empty() => value,
        _ => reported.trim(),
    };
    if chosen.contains(':') && !chosen.starts_with('[') {
        format!("[{chosen}]")
    } else {
        chosen.to_owned()
    }
}

/// `scheme://user:password@host:port/path`, with the user info
/// percent-encoded.
#[cfg(any(feature = "postgres", feature = "mysql"))]
pub(crate) fn server_url(
    scheme: &str,
    user: &str,
    password: &str,
    host: &str,
    port: u16,
    path: &str,
) -> String {
    format!(
        "{scheme}://{}:{}@{host}:{port}/{path}",
        encode_userinfo(user),
        encode_userinfo(password)
    )
}

#[cfg(any(feature = "postgres", feature = "mysql"))]
fn encode_userinfo(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            out.push(char::from(byte));
        } else {
            let _ = write!(out, "%{byte:02X}");
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_override_wins_when_set() {
        assert_eq!(
            resolve_host(Some("builder.lan"), "localhost"),
            "builder.lan"
        );
        assert_eq!(resolve_host(Some("  10.0.0.7 "), "localhost"), "10.0.0.7");
    }

    #[test]
    fn a_blank_or_missing_override_falls_back_to_the_reported_host() {
        assert_eq!(resolve_host(None, "localhost"), "localhost");
        assert_eq!(
            resolve_host(Some("   "), "docker.internal"),
            "docker.internal"
        );
    }

    #[test]
    fn ipv6_hosts_are_bracketed_once() {
        assert_eq!(resolve_host(Some("::1"), "localhost"), "[::1]");
        assert_eq!(resolve_host(None, "[fe80::1]"), "[fe80::1]");
    }

    #[test]
    #[cfg(any(feature = "postgres", feature = "mysql"))]
    fn urls_encode_user_info() {
        assert_eq!(
            server_url("postgres", "app", "p@ss:w/rd", "db.test", 5432, "orders"),
            "postgres://app:p%40ss%3Aw%2Frd@db.test:5432/orders"
        );
        assert_eq!(
            server_url("mysql", "root", "Ab-9._~", "[::1]", 3306, ""),
            "mysql://root:Ab-9._~@[::1]:3306/"
        );
    }
}
