//! The one spelling of a host this crate stores and compares, and the
//! origins of browser callers.
//!
//! Origins are compared as canonical hosts, never by suffix or substring.

use std::net::Ipv6Addr;

/// The longest input [`canonical_host`] accepts, in bytes.
pub const HOST_INPUT_MAX: usize = 255;

/// Why a host was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum HostError {
    /// Nothing was given.
    Empty,
    /// More than [`HOST_INPUT_MAX`] bytes, or a name over 253 bytes.
    TooLong,
    /// A character outside ASCII.
    NotAscii,
    /// Not a host name, an IPv4 address or a bracketed IPv6 address, with an
    /// optional valid port.
    Malformed,
}

/// The one spelling of a host this crate stores and compares: lowered, one
/// trailing dot removed, IPv6 in its canonical form, a port kept only on a
/// loopback host.
pub fn canonical_host(input: &str) -> Result<String, HostError> {
    let (host, port) = parse(input)?;
    Ok(with_loopback_port(host, port))
}

/// `localhost`, `*.localhost`, `127.0.0.0/8`, `[::1]`, with or without a
/// port.
pub fn is_loopback_host(canonical: &str) -> bool {
    if canonical.starts_with('[') {
        return canonical == "[::1]" || canonical.starts_with("[::1]:");
    }
    let host = canonical
        .split_once(':')
        .map_or(canonical, |(head, _)| head);
    if host == "localhost" {
        return true;
    }
    if let Some(head) = host.strip_suffix(".localhost") {
        return !head.is_empty();
    }
    ipv4_octets(host).is_some_and(|octets| octets[0] == 127)
}

/// The canonical host of an `Origin` value this crate accepts.
pub fn host_of_origin(origin: &str) -> Option<String> {
    let (secure, authority) = if let Some(rest) = origin.strip_prefix("https://") {
        (true, rest)
    } else {
        (false, origin.strip_prefix("http://")?)
    };
    if authority
        .bytes()
        .any(|b| matches!(b, b'/' | b'?' | b'#' | b'@' | b'\\'))
    {
        return None;
    }
    let (host, port) = parse(authority).ok()?;
    if is_loopback_host(&host) {
        return Some(with_loopback_port(host, port));
    }
    (secure && matches!(port, None | Some(HTTPS_PORT))).then_some(host)
}

/// `https://<host>`; `http://<host>` on loopback.
pub fn origin_of_host(canonical: &str) -> String {
    if is_loopback_host(canonical) {
        format!("http://{canonical}")
    } else {
        format!("https://{canonical}")
    }
}

/// Whether a canonical host without a port is an IP address: a bracketed
/// IPv6 address or a dotted IPv4 address.
pub(crate) fn is_ip_address(canonical: &str) -> bool {
    canonical.starts_with('[') || ipv4_octets(canonical).is_some()
}

/// The longest host name, without a trailing dot.
const NAME_MAX: usize = 253;

/// The longest label of a host name.
const LABEL_MAX: usize = 63;

/// The port an `https` origin may name.
const HTTPS_PORT: u16 = 443;

/// The canonical host without its port, and the validated port.
fn parse(input: &str) -> Result<(String, Option<u16>), HostError> {
    if input.is_empty() {
        return Err(HostError::Empty);
    }
    if input.len() > HOST_INPUT_MAX {
        return Err(HostError::TooLong);
    }
    if !input.is_ascii() {
        return Err(HostError::NotAscii);
    }
    let lowered = input.to_ascii_lowercase();
    if let Some(rest) = lowered.strip_prefix('[') {
        let (address, after) = rest.split_once(']').ok_or(HostError::Malformed)?;
        let port = if after.is_empty() {
            None
        } else {
            Some(parse_port(
                after.strip_prefix(':').ok_or(HostError::Malformed)?,
            )?)
        };
        let address: Ipv6Addr = address.parse().map_err(|_| HostError::Malformed)?;
        return Ok((format!("[{address}]"), port));
    }
    let (name, port) = if let Some((name, port)) = lowered.split_once(':') {
        (name, Some(parse_port(port)?))
    } else {
        (lowered.as_str(), None)
    };
    let name = name.strip_suffix('.').unwrap_or(name);
    check_name(name)?;
    Ok((name.to_owned(), port))
}

/// Decimal 1 to 65535 without a leading zero.
fn parse_port(text: &str) -> Result<u16, HostError> {
    if text.is_empty() || text.starts_with('0') || !text.bytes().all(|b| b.is_ascii_digit()) {
        return Err(HostError::Malformed);
    }
    text.parse().map_err(|_| HostError::Malformed)
}

/// Labels of 1 to 63 letters, digits and interior hyphens, at most 253
/// bytes in all; a numeric last label makes the whole name an IPv4 address.
fn check_name(name: &str) -> Result<(), HostError> {
    if name.len() > NAME_MAX {
        return Err(HostError::TooLong);
    }
    let last = name.rsplit('.').next().unwrap_or(name);
    if !last.is_empty() && last.bytes().all(|b| b.is_ascii_digit()) {
        return ipv4_octets(name).map(|_| ()).ok_or(HostError::Malformed);
    }
    if name.split('.').all(is_dns_label) {
        Ok(())
    } else {
        Err(HostError::Malformed)
    }
}

/// 1 to 63 of `[a-z0-9-]`, no hyphen at either end.
fn is_dns_label(label: &str) -> bool {
    (1..=LABEL_MAX).contains(&label.len())
        && !label.starts_with('-')
        && !label.ends_with('-')
        && label
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// A dotted IPv4 address of four decimal octets without leading zeros.
fn ipv4_octets(text: &str) -> Option<[u8; 4]> {
    let mut octets = [0_u8; 4];
    let mut parts = text.split('.');
    for octet in &mut octets {
        let part = parts.next()?;
        let valid = (1..=3).contains(&part.len())
            && part.bytes().all(|b| b.is_ascii_digit())
            && !(part.len() > 1 && part.starts_with('0'));
        if !valid {
            return None;
        }
        *octet = part.parse().ok()?;
    }
    parts.next().is_none().then_some(octets)
}

/// The host with its port on loopback, without it elsewhere.
fn with_loopback_port(host: String, port: Option<u16>) -> String {
    match port {
        Some(port) if is_loopback_host(&host) => format!("{host}:{port}"),
        _ => host,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ip_addresses_are_told_from_host_names() {
        for canonical in ["10.0.0.1", "192.0.2.1", "[2001:db8::1]", "[::1]"] {
            assert!(is_ip_address(canonical), "{canonical:?}");
        }
        for canonical in ["example.com", "10.example.com", "localhost", "x"] {
            assert!(!is_ip_address(canonical), "{canonical:?}");
        }
    }

    #[test]
    fn canonical_host_lowers_and_removes_one_trailing_dot() {
        for (input, expected) in [
            ("Example.COM", "example.com"),
            ("billing.example.com.", "billing.example.com"),
            ("a-b.example.com", "a-b.example.com"),
            ("x", "x"),
            ("xn--bcher-kva.example", "xn--bcher-kva.example"),
            ("orders.example.com.:8443", "orders.example.com"),
        ] {
            assert_eq!(canonical_host(input).as_deref(), Ok(expected), "{input:?}");
        }
        for input in ["example.com..", ".", ".example.com", "a..b", ".:80"] {
            assert_eq!(
                canonical_host(input),
                Err(HostError::Malformed),
                "{input:?}"
            );
        }
    }

    #[test]
    fn canonical_host_checks_the_input() {
        assert_eq!(canonical_host(""), Err(HostError::Empty));
        let long_input = "a".repeat(HOST_INPUT_MAX + 1);
        assert_eq!(canonical_host(&long_input), Err(HostError::TooLong));
        assert_eq!(
            canonical_host("b\u{fc}cher.example"),
            Err(HostError::NotAscii)
        );
        for input in [
            "exa mple.com",
            "exa_mple.com",
            "-billing.example.com",
            "billing-.example.com",
            "example.com/",
            "user@example.com",
            "example.com:80:80",
            "[::1",
            "[::1]x",
            "[::1]:",
            "[fe80::1%eth0]",
            "[not-an-address]",
            "::1",
        ] {
            assert_eq!(
                canonical_host(input),
                Err(HostError::Malformed),
                "{input:?}"
            );
        }
    }

    #[test]
    fn canonical_host_label_and_name_limits() {
        let label = "a".repeat(LABEL_MAX);
        let host = format!("{label}.example.com");
        assert_eq!(canonical_host(&host).as_deref(), Ok(host.as_str()));
        let over = format!("{}.example.com", "a".repeat(LABEL_MAX + 1));
        assert_eq!(canonical_host(&over), Err(HostError::Malformed));

        // Three labels of 63, one of 61 and three dots: 253 bytes.
        let tail = "a".repeat(61);
        let longest = format!("{label}.{label}.{label}.{tail}");
        assert_eq!(longest.len(), NAME_MAX);
        assert_eq!(canonical_host(&longest).as_deref(), Ok(longest.as_str()));
        let with_dot = format!("{longest}.");
        assert_eq!(canonical_host(&with_dot).as_deref(), Ok(longest.as_str()));
        let tail = "a".repeat(62);
        let over = format!("{label}.{label}.{label}.{tail}");
        assert_eq!(over.len(), NAME_MAX + 1);
        assert_eq!(canonical_host(&over), Err(HostError::TooLong));
    }

    #[test]
    fn a_numeric_last_label_must_be_an_ipv4_address() {
        for (input, expected) in [
            ("192.0.2.1", "192.0.2.1"),
            ("0.0.0.0", "0.0.0.0"),
            ("255.255.255.255", "255.255.255.255"),
            ("192.0.2.1.", "192.0.2.1"),
            ("192.0.2.1:8080", "192.0.2.1"),
            ("example.c0m", "example.c0m"),
            ("1example.com", "1example.com"),
        ] {
            assert_eq!(canonical_host(input).as_deref(), Ok(expected), "{input:?}");
        }
        for input in [
            "example.123",
            "123",
            "1.2.3",
            "1.2.3.4.5",
            "256.1.1.1",
            "01.2.3.4",
            "1.2.3.04",
            "1.2.3.0004",
            "a.2.3.4",
        ] {
            assert_eq!(
                canonical_host(input),
                Err(HostError::Malformed),
                "{input:?}"
            );
        }
    }

    #[test]
    fn ipv6_is_bracketed_and_canonical() {
        for (input, expected) in [
            ("[2001:DB8:0:0:0:0:0:1]", "[2001:db8::1]"),
            ("[2001:0db8::0001]", "[2001:db8::1]"),
            ("[0:0:0:0:0:0:0:1]", "[::1]"),
            ("[::1]", "[::1]"),
            ("[::1]:8080", "[::1]:8080"),
            ("[2001:db8::1]:443", "[2001:db8::1]"),
        ] {
            assert_eq!(canonical_host(input).as_deref(), Ok(expected), "{input:?}");
        }
    }

    #[test]
    fn ports_are_validated_everywhere_and_kept_on_loopback_only() {
        for (input, expected) in [
            ("localhost:5173", "localhost:5173"),
            ("billing.localhost:3000", "billing.localhost:3000"),
            ("127.0.0.1:1", "127.0.0.1:1"),
            ("127.1.2.3:65535", "127.1.2.3:65535"),
            ("example.com:8443", "example.com"),
            ("example.com:443", "example.com"),
            ("localhost", "localhost"),
        ] {
            assert_eq!(canonical_host(input).as_deref(), Ok(expected), "{input:?}");
        }
        for input in [
            "example.com:0",
            "example.com:080",
            "localhost:08080",
            "localhost:65536",
            "localhost:99999999999",
            "localhost:",
            "localhost:+80",
            "localhost:8o",
            "[::1]:0443",
        ] {
            assert_eq!(
                canonical_host(input),
                Err(HostError::Malformed),
                "{input:?}"
            );
        }
    }

    #[test]
    fn loopback_hosts() {
        for host in [
            "localhost",
            "localhost:8080",
            "billing.localhost",
            "a.b.localhost:1",
            "127.0.0.1",
            "127.255.255.254:80",
            "[::1]",
            "[::1]:8080",
        ] {
            assert!(is_loopback_host(host), "{host:?}");
        }
        for host in [
            "example.com",
            "localhost.example.com",
            "notlocalhost",
            ".localhost",
            "128.0.0.1",
            "10.0.0.1:80",
            "[::2]",
            "[2001:db8::1]:8080",
            "[::1",
            "",
        ] {
            assert!(!is_loopback_host(host), "{host:?}");
        }
    }

    #[test]
    fn host_of_origin_table() {
        for (origin, expected) in [
            ("https://example.com", "example.com"),
            ("https://Billing.Example.com", "billing.example.com"),
            ("https://example.com:443", "example.com"),
            ("https://[2001:db8::1]", "[2001:db8::1]"),
            ("http://localhost:5173", "localhost:5173"),
            ("https://localhost:8443", "localhost:8443"),
            ("http://localhost", "localhost"),
            ("http://billing.localhost:3000", "billing.localhost:3000"),
            ("http://127.0.0.1:8080", "127.0.0.1:8080"),
            ("http://[::1]:8080", "[::1]:8080"),
        ] {
            assert_eq!(
                host_of_origin(origin).as_deref(),
                Some(expected),
                "{origin:?}"
            );
        }
        for origin in [
            "null",
            "",
            "example.com",
            "http://example.com",
            "HTTPS://example.com",
            "Https://example.com",
            "ftp://example.com",
            "https://",
            "https://example.com:8443",
            "https://example.com:0443",
            "http://localhost:05173",
            "https://example.com/",
            "https://example.com/path",
            "https://example.com?query",
            "https://example.com#fragment",
            "https://user@example.com",
            "https://user:secret@example.com",
            "http://user@localhost:5173",
            "https://example.com\\",
            "https://b\u{fc}cher.example",
            "https://exa mple.com",
        ] {
            assert_eq!(host_of_origin(origin), None, "{origin:?}");
        }
    }

    #[test]
    fn origin_of_host_uses_http_on_loopback_only() {
        assert_eq!(origin_of_host("example.com"), "https://example.com");
        assert_eq!(origin_of_host("[2001:db8::1]"), "https://[2001:db8::1]");
        assert_eq!(origin_of_host("localhost:5173"), "http://localhost:5173");
        assert_eq!(origin_of_host("[::1]:8080"), "http://[::1]:8080");
        for host in ["example.com", "localhost:5173", "127.0.0.1:80"] {
            assert_eq!(host_of_origin(&origin_of_host(host)).as_deref(), Some(host));
        }
    }
}
