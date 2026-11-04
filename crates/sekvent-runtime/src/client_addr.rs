//! Which address a request comes from: the TCP peer, or behind trusted
//! proxies the client named in a forwarding header.

use std::net::{IpAddr, Ipv6Addr, SocketAddrV4};
use std::str::FromStr;

use http::{HeaderMap, HeaderName, HeaderValue};
pub use ipnet::IpNet;
use sekvent_config::{ConfigError, ConfigSource};
use sekvent_error::AppError;

/// Key suffixes [`ClientAddrPolicy::from_config`] reads.
const KEYS: [&str; 2] = ["TRUSTED_PROXIES", "CLIENT_ADDR_HEADER"];

const X_FORWARDED_FOR: &str = "x-forwarded-for";
const FORWARDED: &str = "forwarded";
const X_REAL_IP: &str = "x-real-ip";

/// The header a trusted proxy names the client in.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ForwardedHeader {
    /// `X-Forwarded-For: client, proxy1, proxy2`.
    XForwardedFor,
    /// `Forwarded: for=client, for=proxy1` (RFC 7239; the `for` parameter).
    Forwarded,
    /// `X-Real-IP: client`, one address.
    XRealIp,
    /// Another header carrying a comma-separated list like
    /// `X-Forwarded-For`, such as a CDN's own.
    Named(HeaderName),
}

impl ForwardedHeader {
    /// The header's name.
    pub fn name(&self) -> &str {
        match self {
            Self::XForwardedFor => X_FORWARDED_FOR,
            Self::Forwarded => FORWARDED,
            Self::XRealIp => X_REAL_IP,
            Self::Named(name) => name.as_str(),
        }
    }

    /// The configuration spelling: one of the three names, case-insensitive,
    /// or any other valid header name.
    fn parse(raw: &str) -> Option<Self> {
        let lower = raw.trim().to_ascii_lowercase();
        Some(match lower.as_str() {
            X_FORWARDED_FOR => Self::XForwardedFor,
            FORWARDED => Self::Forwarded,
            X_REAL_IP => Self::XRealIp,
            _ => Self::Named(HeaderName::from_str(&lower).ok()?),
        })
    }
}

/// How the server tells a request's client address, which it puts into the
/// request's [`CallContext`](sekvent_context::CallContext) and, as
/// [`ClientAddr`](sekvent_context::ClientAddr), into its extensions.
///
/// - [`peer`](Self::peer) (the default): the TCP peer.
/// - [`behind_proxies`](Self::behind_proxies): when the TCP peer is one of
///   the trusted networks, the header's addresses are walked from the
///   right, skipping trusted ones; the first untrusted address is the
///   client (all trusted: the leftmost). A peer outside the trusted
///   networks is the client and its header is ignored, and so is a header
///   that is missing or malformed where it is read.
///
/// An IPv4-mapped IPv6 peer (a dual-stack socket) counts as its IPv4
/// address.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ClientAddrPolicy {
    trusted: Vec<IpNet>,
    header: Option<ForwardedHeader>,
}

impl ClientAddrPolicy {
    /// The TCP peer is the client.
    pub fn peer() -> Self {
        Self::default()
    }

    /// Behind the proxies in `trusted`, which name the client in `header`.
    /// [`ServerBuilder::bind`](crate::ServerBuilder::bind) refuses an empty
    /// list.
    pub fn behind_proxies(trusted: Vec<IpNet>, header: ForwardedHeader) -> Self {
        Self {
            trusted: trusted.into_iter().map(|net| net.trunc()).collect(),
            header: Some(header),
        }
    }

    /// Read the policy from `<prefix>TRUSTED_PROXIES` (comma-separated
    /// networks such as `10.0.0.0/8, 2001:db8::/32`, or single addresses)
    /// and `<prefix>CLIENT_ADDR_HEADER` (`x-forwarded-for`, the default,
    /// `forwarded`, `x-real-ip`, or another header name).
    ///
    /// Without `<prefix>TRUSTED_PROXIES` (or with it blank) the policy is
    /// [`peer`](Self::peer), and `<prefix>CLIENT_ADDR_HEADER` is then
    /// invalid. Every problem is reported at once; an invalid network is
    /// named by its position in the list, counting from 1.
    pub fn from_config(source: &dyn ConfigSource, prefix: &str) -> Result<Self, ConfigError> {
        let proxies_key = format!("{prefix}{}", KEYS[0]);
        let header_key = format!("{prefix}{}", KEYS[1]);
        let proxies = source
            .get(&proxies_key)
            .filter(|raw| !raw.trim().is_empty());
        let raw_header = source.get(&header_key);
        let mut errors = Vec::new();
        let header = match raw_header.as_deref().map(ForwardedHeader::parse) {
            None => Some(ForwardedHeader::XForwardedFor),
            Some(Some(header)) => Some(header),
            Some(None) => {
                errors.push(ConfigError::Malformed {
                    key: source.describe(&header_key),
                    expected: "x-forwarded-for, forwarded, x-real-ip or another header name"
                        .to_owned(),
                });
                None
            }
        };
        let Some(proxies) = proxies else {
            if raw_header.is_some() {
                errors.push(ConfigError::Invalid {
                    key: source.describe(&header_key),
                    reason: format!("a client address header needs {proxies_key}"),
                });
            }
            sekvent_config::collect(errors.into_iter().map(Err))?;
            return Ok(Self::peer());
        };
        let mut trusted = Vec::new();
        for (index, entry) in proxies.split(',').enumerate() {
            match parse_net(entry.trim()) {
                Some(net) => trusted.push(net),
                None => errors.push(ConfigError::Invalid {
                    key: source.describe(&proxies_key),
                    reason: format!(
                        "entry {} is not a network such as 10.0.0.0/8 or an address",
                        index + 1
                    ),
                }),
            }
        }
        sekvent_config::collect(errors.into_iter().map(Err))?;
        // `header` is set whenever no error was recorded.
        Ok(Self::behind_proxies(
            trusted,
            header.unwrap_or(ForwardedHeader::XForwardedFor),
        ))
    }

    /// Every key [`from_config`](Self::from_config) reads under `prefix`.
    pub fn config_keys(prefix: &str) -> Vec<String> {
        KEYS.iter()
            .map(|suffix| format!("{prefix}{suffix}"))
            .collect()
    }

    /// The trusted proxy networks (empty for [`peer`](Self::peer)).
    pub fn trusted(&self) -> &[IpNet] {
        &self.trusted
    }

    /// The forwarding header, `None` for [`peer`](Self::peer).
    pub fn header(&self) -> Option<&ForwardedHeader> {
        self.header.as_ref()
    }

    /// The check [`ServerBuilder::bind`](crate::ServerBuilder::bind) runs:
    /// `INVALID_ARGUMENT` for proxies without a trusted network.
    pub fn validate(&self) -> Result<(), AppError> {
        if self.header.is_some() && self.trusted.is_empty() {
            return Err(AppError::invalid_argument(
                "a client address policy behind proxies needs at least one trusted network",
            ));
        }
        Ok(())
    }

    /// The client address of a request from `peer` with `headers`.
    pub fn resolve(&self, peer: IpAddr, headers: &HeaderMap) -> IpAddr {
        let peer = peer.to_canonical();
        let Some(header) = &self.header else {
            return peer;
        };
        if !self.is_trusted(peer) {
            return peer;
        }
        self.walk(header, headers).unwrap_or(peer)
    }

    fn is_trusted(&self, addr: IpAddr) -> bool {
        self.trusted.iter().any(|net| net.contains(&addr))
    }

    /// The client named by `header`, walking from the right; `None` when
    /// the header is missing or malformed where it is read.
    fn walk(&self, header: &ForwardedHeader, headers: &HeaderMap) -> Option<IpAddr> {
        let values: Vec<&HeaderValue> = headers.get_all(header.name()).iter().collect();
        if *header == ForwardedHeader::XRealIp && values.len() > 1 {
            return None;
        }
        let mut leftmost = None;
        for value in values.into_iter().rev() {
            let value = value.to_str().ok()?;
            for element in value.rsplit(',') {
                let addr = match header {
                    ForwardedHeader::Forwarded => forwarded_for(element)?,
                    _ => parse_node(element.trim())?,
                };
                if !self.is_trusted(addr) {
                    return Some(addr);
                }
                leftmost = Some(addr);
            }
        }
        leftmost
    }
}

/// A network, or a single address as a /32 or /128.
fn parse_net(raw: &str) -> Option<IpNet> {
    IpNet::from_str(raw)
        .ok()
        .or_else(|| IpAddr::from_str(raw).ok().map(IpNet::from))
}

/// The `for` parameter of one `Forwarded` element.
fn forwarded_for(element: &str) -> Option<IpAddr> {
    element.split(';').find_map(|pair| {
        let (name, value) = pair.split_once('=')?;
        if !name.trim().eq_ignore_ascii_case("for") {
            return None;
        }
        let value = value.trim();
        let value = value
            .strip_prefix('"')
            .and_then(|inner| inner.strip_suffix('"'))
            .unwrap_or(value);
        Some(parse_node(value))
    })?
}

/// An address with an optional port: `192.0.2.1`, `192.0.2.1:443`,
/// `2001:db8::1`, `[2001:db8::1]` or `[2001:db8::1]:443`.
fn parse_node(raw: &str) -> Option<IpAddr> {
    let addr = if let Some(rest) = raw.strip_prefix('[') {
        let (inner, after) = rest.split_once(']')?;
        let port_ok = after.is_empty()
            || after
                .strip_prefix(':')
                .is_some_and(|port| port.parse::<u16>().is_ok());
        if !port_ok {
            return None;
        }
        IpAddr::V6(Ipv6Addr::from_str(inner).ok()?)
    } else if let Ok(addr) = IpAddr::from_str(raw) {
        addr
    } else {
        IpAddr::V4(*SocketAddrV4::from_str(raw).ok()?.ip())
    };
    Some(addr.to_canonical())
}

#[cfg(test)]
mod tests {
    use sekvent_config::MapSource;

    use super::*;

    const PROXY: &str = "10.0.0.5";
    const CLIENT: &str = "203.0.113.9";

    fn ip(raw: &str) -> IpAddr {
        raw.parse().unwrap()
    }

    fn nets(raw: &[&str]) -> Vec<IpNet> {
        raw.iter().map(|net| net.parse().unwrap()).collect()
    }

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.append(
                HeaderName::from_str(name).unwrap(),
                HeaderValue::from_str(value).unwrap(),
            );
        }
        map
    }

    fn xff() -> ClientAddrPolicy {
        ClientAddrPolicy::behind_proxies(
            nets(&["10.0.0.0/8", "2001:db8:ffff::/48"]),
            ForwardedHeader::XForwardedFor,
        )
    }

    #[test]
    fn the_peer_policy_ignores_every_header() {
        let policy = ClientAddrPolicy::peer();
        let map = headers(&[(X_FORWARDED_FOR, CLIENT), (X_REAL_IP, CLIENT)]);
        assert_eq!(policy.resolve(ip(PROXY), &map), ip(PROXY));
        assert_eq!(
            policy.resolve(ip("::ffff:192.0.2.7"), &map),
            ip("192.0.2.7"),
            "a mapped peer is its IPv4 address"
        );
        assert!(policy.validate().is_ok());
        assert_eq!(policy.header(), None);
        assert_eq!(policy.trusted(), <&[IpNet]>::default());
    }

    #[test]
    fn a_trusted_chain_is_walked_from_the_right() {
        let policy = xff();
        let resolve =
            |value: &str| policy.resolve(ip(PROXY), &headers(&[(X_FORWARDED_FOR, value)]));
        assert_eq!(resolve(CLIENT), ip(CLIENT));
        assert_eq!(resolve("198.51.100.1, 203.0.113.9, 10.1.1.1"), ip(CLIENT));
        assert_eq!(
            resolve("garbage, 203.0.113.9"),
            ip(CLIENT),
            "what the client wrote to the left is never read"
        );
        assert_eq!(resolve("10.0.0.7, 10.0.0.8"), ip("10.0.0.7"), "all trusted");
        assert_eq!(resolve(" 203.0.113.9:4711 "), ip(CLIENT));
        assert_eq!(resolve("[2001:db8::1]:443"), ip("2001:db8::1"));
        assert_eq!(resolve("2001:db8::1, 2001:db8:ffff::2"), ip("2001:db8::1"));
        assert_eq!(resolve("::ffff:203.0.113.9"), ip(CLIENT));

        let lines = headers(&[
            (X_FORWARDED_FOR, "198.51.100.1"),
            (X_FORWARDED_FOR, "203.0.113.9, 10.1.1.1"),
        ]);
        assert_eq!(policy.resolve(ip(PROXY), &lines), ip(CLIENT));
        assert_eq!(
            policy.resolve(
                ip("2001:db8:ffff::9"),
                &headers(&[(X_FORWARDED_FOR, CLIENT)])
            ),
            ip(CLIENT),
            "an IPv6 proxy"
        );
    }

    #[test]
    fn an_untrusted_peer_or_a_malformed_header_leaves_the_peer() {
        let policy = xff();
        let forged = headers(&[(X_FORWARDED_FOR, "198.51.100.1")]);
        assert_eq!(policy.resolve(ip(CLIENT), &forged), ip(CLIENT));
        for bad in [
            "",
            "unknown",
            "203.0.113.9, nonsense",
            "203.0.113.9,",
            "[2001:db8::1",
            "[2001:db8::1]x",
            "[2001:db8::1]:port",
            "[203.0.113.9]",
            "203.0.113.9:99999",
        ] {
            let map = headers(&[(X_FORWARDED_FOR, bad)]);
            assert_eq!(policy.resolve(ip(PROXY), &map), ip(PROXY), "{bad:?}");
        }
        assert_eq!(policy.resolve(ip(PROXY), &HeaderMap::new()), ip(PROXY));
        let mut opaque = HeaderMap::new();
        opaque.insert(X_FORWARDED_FOR, HeaderValue::from_bytes(b"\xff").unwrap());
        assert_eq!(policy.resolve(ip(PROXY), &opaque), ip(PROXY));
    }

    #[test]
    fn forwarded_reads_the_for_parameter() {
        let policy =
            ClientAddrPolicy::behind_proxies(nets(&["10.0.0.0/8"]), ForwardedHeader::Forwarded);
        let resolve = |value: &str| policy.resolve(ip(PROXY), &headers(&[(FORWARDED, value)]));
        assert_eq!(resolve("for=203.0.113.9;proto=https"), ip(CLIENT));
        assert_eq!(
            resolve("for=198.51.100.1, For=\"[2001:db8::17]:4711\";by=10.0.0.1, for=10.0.0.2"),
            ip("2001:db8::17")
        );
        assert_eq!(resolve("proto=https;for=\"203.0.113.9:80\""), ip(CLIENT));
        for bad in [
            "for=unknown",
            "for=_hidden",
            "proto=https",
            "for",
            "for=\"[::1\"",
        ] {
            assert_eq!(resolve(bad), ip(PROXY), "{bad:?}");
        }
    }

    #[test]
    fn x_real_ip_and_named_headers() {
        let real =
            ClientAddrPolicy::behind_proxies(nets(&["10.0.0.0/8"]), ForwardedHeader::XRealIp);
        assert_eq!(
            real.resolve(ip(PROXY), &headers(&[(X_REAL_IP, CLIENT)])),
            ip(CLIENT)
        );
        let twice = headers(&[(X_REAL_IP, CLIENT), (X_REAL_IP, "198.51.100.1")]);
        assert_eq!(real.resolve(ip(PROXY), &twice), ip(PROXY));

        let name = HeaderName::from_static("cf-connecting-ip");
        let named = ClientAddrPolicy::behind_proxies(
            nets(&["10.0.0.0/8"]),
            ForwardedHeader::Named(name.clone()),
        );
        assert_eq!(named.header().unwrap().name(), "cf-connecting-ip");
        assert_eq!(
            named.resolve(ip(PROXY), &headers(&[("cf-connecting-ip", CLIENT)])),
            ip(CLIENT)
        );
        assert_eq!(
            named.resolve(ip(PROXY), &headers(&[(X_FORWARDED_FOR, CLIENT)])),
            ip(PROXY),
            "only the named header is read"
        );
    }

    #[test]
    fn behind_proxies_needs_a_trusted_network() {
        let empty = ClientAddrPolicy::behind_proxies(Vec::new(), ForwardedHeader::XForwardedFor);
        assert!(empty.validate().is_err());
        let policy =
            ClientAddrPolicy::behind_proxies(nets(&["10.1.2.3/8"]), ForwardedHeader::XForwardedFor);
        assert_eq!(policy.trusted(), nets(&["10.0.0.0/8"]).as_slice());
        assert!(policy.validate().is_ok());
    }

    #[test]
    fn config_reads_networks_and_the_header() {
        let source: MapSource = [
            (
                "EDGE_TRUSTED_PROXIES",
                "10.0.0.0/8, 192.0.2.1 ,2001:db8::/32",
            ),
            ("EDGE_CLIENT_ADDR_HEADER", "X-Real-IP"),
        ]
        .into_iter()
        .collect();
        let policy = ClientAddrPolicy::from_config(&source, "EDGE_").unwrap();
        assert_eq!(
            policy.trusted(),
            nets(&["10.0.0.0/8", "192.0.2.1/32", "2001:db8::/32"]).as_slice()
        );
        assert_eq!(policy.header(), Some(&ForwardedHeader::XRealIp));

        let defaulted: MapSource = [("TRUSTED_PROXIES", "10.0.0.0/8")].into_iter().collect();
        let policy = ClientAddrPolicy::from_config(&defaulted, "").unwrap();
        assert_eq!(policy.header(), Some(&ForwardedHeader::XForwardedFor));

        for (raw, expected) in [
            ("forwarded", ForwardedHeader::Forwarded),
            ("X-Forwarded-For", ForwardedHeader::XForwardedFor),
            (
                "True-Client-IP",
                ForwardedHeader::Named(HeaderName::from_static("true-client-ip")),
            ),
        ] {
            let source: MapSource = [
                ("TRUSTED_PROXIES", "10.0.0.0/8"),
                ("CLIENT_ADDR_HEADER", raw),
            ]
            .into_iter()
            .collect();
            let policy = ClientAddrPolicy::from_config(&source, "").unwrap();
            assert_eq!(policy.header(), Some(&expected));
        }

        let unset = MapSource::default();
        assert_eq!(
            ClientAddrPolicy::from_config(&unset, "").unwrap(),
            ClientAddrPolicy::peer()
        );
        let blank: MapSource = [("TRUSTED_PROXIES", " ")].into_iter().collect();
        assert_eq!(
            ClientAddrPolicy::from_config(&blank, "").unwrap(),
            ClientAddrPolicy::peer()
        );
        assert_eq!(
            ClientAddrPolicy::config_keys("EDGE_"),
            ["EDGE_TRUSTED_PROXIES", "EDGE_CLIENT_ADDR_HEADER"]
        );
    }

    #[test]
    fn config_errors_name_their_keys() {
        let bad: MapSource = [
            ("TRUSTED_PROXIES", "10.0.0.0/8,,not-a-net,10.0.0.0/33"),
            ("CLIENT_ADDR_HEADER", "bad header"),
        ]
        .into_iter()
        .collect();
        let ConfigError::Multiple(errors) = ClientAddrPolicy::from_config(&bad, "").unwrap_err()
        else {
            panic!("expected several errors");
        };
        let text: Vec<String> = errors.iter().map(ToString::to_string).collect();
        assert_eq!(errors.len(), 4, "{text:?}");
        assert!(text[0].contains("CLIENT_ADDR_HEADER"), "{text:?}");
        for (error, entry) in text[1..].iter().zip(["entry 2", "entry 3", "entry 4"]) {
            assert!(
                error.contains("TRUSTED_PROXIES") && error.contains(entry),
                "{error}"
            );
            assert!(!error.contains("not-a-net"), "{error}");
        }

        let orphan: MapSource = [("APP_CLIENT_ADDR_HEADER", "x-real-ip")]
            .into_iter()
            .collect();
        let error = ClientAddrPolicy::from_config(&orphan, "APP_").unwrap_err();
        assert!(
            matches!(&error, ConfigError::Invalid { key, reason }
                if key == "APP_CLIENT_ADDR_HEADER" && reason.contains("APP_TRUSTED_PROXIES")),
            "{error}"
        );
    }
}
