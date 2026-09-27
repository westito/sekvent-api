use std::fmt;

use sekvent_config::Secret;
use sekvent_context::ServiceIdentity;
use subtle::{Choice, ConditionallySelectable, ConstantTimeEq};

use crate::LinkError;

/// Minimum token length in characters.
pub const MIN_TOKEN_LEN: usize = 32;

/// Number of random bytes behind a [`random_token`].
const RANDOM_TOKEN_BYTES: usize = 32;

/// One inbound link: who presents `token` and whether they are trusted.
#[derive(Debug, Clone)]
pub struct InboundLink {
    /// Link name, e.g. `billing`.
    pub name: String,
    /// The token this link presents.
    pub token: Secret,
    /// Whether the link may assert end-user identity.
    pub trusted: bool,
}

impl InboundLink {
    /// A link that may not assert end-user identity.
    pub fn untrusted(name: impl Into<String>, token: Secret) -> Self {
        Self {
            name: name.into(),
            token,
            trusted: false,
        }
    }

    /// A link that may assert end-user identity.
    pub fn trusted(name: impl Into<String>, token: Secret) -> Self {
        Self {
            name: name.into(),
            token,
            trusted: true,
        }
    }
}

struct Entry {
    identity: ServiceIdentity,
    token: Secret,
}

/// The accepted inbound tokens and the service each one identifies.
///
/// Lookups compare the presented token with every entry in constant time
/// and never stop early, so the time taken does not depend on which entry
/// matched or on how many leading characters were right. Comparing tokens
/// of different lengths does reveal that the lengths differ; that is an
/// accepted limitation, and canonical tokens of one generator all have the
/// same length anyway.
pub struct TokenMap {
    entries: Vec<Entry>,
}

impl TokenMap {
    /// Build the map, refusing blank names, blank, short or non-canonical
    /// tokens, a name used twice and a token shared by two links.
    pub fn new(links: impl IntoIterator<Item = InboundLink>) -> Result<Self, LinkError> {
        let mut entries: Vec<Entry> = Vec::new();
        for link in links {
            if link.name.trim().is_empty() {
                return Err(LinkError::BlankName);
            }
            if entries.iter().any(|entry| entry.identity.name == link.name) {
                return Err(LinkError::DuplicateName { link: link.name });
            }
            validate_token(&link.name, link.token.expose())?;
            if let Some(existing) = entries
                .iter()
                .find(|entry| same_token(&entry.token, &link.token))
            {
                return Err(LinkError::DuplicateToken {
                    first: existing.identity.name.clone(),
                    second: link.name,
                });
            }
            entries.push(Entry {
                identity: ServiceIdentity {
                    name: link.name,
                    trusted: link.trusted,
                },
                token: link.token,
            });
        }
        Ok(Self { entries })
    }

    /// The service presenting `token`, if any. Empty input is refused
    /// before any comparison.
    pub fn authenticate(&self, token: &str) -> Option<ServiceIdentity> {
        if token.is_empty() {
            return None;
        }
        let presented = token.as_bytes();
        let mut matched = Choice::from(0);
        let mut index = 0_u32;
        for (position, entry) in self.entries.iter().enumerate() {
            let equal = entry.token.expose().as_bytes().ct_eq(presented);
            let position = u32::try_from(position).unwrap_or(u32::MAX);
            index.conditional_assign(&position, equal);
            matched |= equal;
        }
        if bool::from(matched) {
            let index = usize::try_from(index).ok()?;
            self.entries.get(index).map(|entry| entry.identity.clone())
        } else {
            None
        }
    }

    /// Number of links.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the map accepts no token at all.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The identities of all links, in insertion order.
    pub fn identities(&self) -> impl Iterator<Item = &ServiceIdentity> {
        self.entries.iter().map(|entry| &entry.identity)
    }
}

impl fmt::Debug for TokenMap {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TokenMap")
            .field("links", &self.identities().collect::<Vec<_>>())
            .finish_non_exhaustive()
    }
}

/// Check that `token` is usable for `link`: not blank, at least
/// [`MIN_TOKEN_LEN`] characters, only `[A-Za-z0-9_-]`.
pub fn validate_token(link: &str, token: &str) -> Result<(), LinkError> {
    if token.trim().is_empty() {
        return Err(LinkError::BlankToken {
            link: link.to_owned(),
        });
    }
    if token.len() < MIN_TOKEN_LEN {
        return Err(LinkError::TooShort {
            link: link.to_owned(),
        });
    }
    if !token
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
    {
        return Err(LinkError::NonCanonical {
            link: link.to_owned(),
        });
    }
    Ok(())
}

/// A fresh canonical token: 32 random bytes from the operating system,
/// base64url-encoded without padding (43 characters).
pub fn random_token() -> Result<Secret, LinkError> {
    let mut bytes = [0_u8; RANDOM_TOKEN_BYTES];
    getrandom::fill(&mut bytes).map_err(|_| LinkError::Randomness)?;
    let token = Secret::new(base64url(&bytes));
    bytes.fill(0);
    Ok(token)
}

/// Check that no token is accepted by two maps, e.g. by two services
/// hosted in one process. Errors name `<map>/<link>`, never the token.
pub fn validate_unique(maps: &[(&str, &TokenMap)]) -> Result<(), LinkError> {
    for (position, (first_map, first)) in maps.iter().enumerate() {
        for (second_map, second) in &maps[position + 1..] {
            for left in &first.entries {
                if let Some(right) = second
                    .entries
                    .iter()
                    .find(|right| same_token(&left.token, &right.token))
                {
                    return Err(LinkError::DuplicateToken {
                        first: format!("{first_map}/{}", left.identity.name),
                        second: format!("{second_map}/{}", right.identity.name),
                    });
                }
            }
        }
    }
    Ok(())
}

fn same_token(left: &Secret, right: &Secret) -> bool {
    bool::from(left.expose().as_bytes().ct_eq(right.expose().as_bytes()))
}

fn base64url(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let group = chunk.iter().enumerate().fold(0_u32, |group, (i, byte)| {
            group | (u32::from(*byte) << (16 - 8 * i))
        });
        for i in 0..=chunk.len() {
            let sextet = (group >> (18 - 6 * i)) & 0x3f;
            out.push(char::from(ALPHABET[sextet as usize]));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const BILLING: &str = "billing-token-0123456789abcdefABCDEF";
    const ORDERS: &str = "orders_token_0123456789abcdefABCDEF";
    const REPORTS: &str = "reports-token-0123456789abcdefABCDE";

    fn map() -> TokenMap {
        TokenMap::new([
            InboundLink::trusted("billing", Secret::new(BILLING)),
            InboundLink::untrusted("orders", Secret::new(ORDERS)),
            InboundLink::untrusted("reports", Secret::new(REPORTS)),
        ])
        .unwrap()
    }

    fn error_for(token: &str) -> LinkError {
        TokenMap::new([InboundLink::untrusted("billing", Secret::new(token))]).unwrap_err()
    }

    #[test]
    fn every_entry_is_found() {
        let map = map();
        assert_eq!(
            map.authenticate(BILLING),
            Some(ServiceIdentity::trusted("billing"))
        );
        assert_eq!(
            map.authenticate(ORDERS),
            Some(ServiceIdentity::untrusted("orders"))
        );
        assert_eq!(
            map.authenticate(REPORTS),
            Some(ServiceIdentity::untrusted("reports"))
        );
        assert_eq!(map.len(), 3);
        assert!(!map.is_empty());
    }

    #[test]
    fn near_misses_are_refused() {
        let map = map();
        let truncated = &BILLING[..BILLING.len() - 1];
        let owned = [
            format!("{BILLING}x"),
            format!(" {BILLING}"),
            format!("{BILLING} "),
            BILLING.to_uppercase(),
            format!("{truncated}X"),
        ];
        let borrowed = ["", "x", truncated];
        for presented in borrowed.into_iter().chain(owned.iter().map(String::as_str)) {
            assert_eq!(map.authenticate(presented), None, "{presented:?}");
        }
    }

    #[test]
    fn empty_map_refuses_everything() {
        let map = TokenMap::new([]).unwrap();
        assert!(map.is_empty());
        assert_eq!(map.authenticate(BILLING), None);
    }

    #[test]
    fn rejections_name_the_link_only() {
        let cases = [
            (
                "",
                LinkError::BlankToken {
                    link: "billing".into(),
                },
            ),
            (
                "    \t   ",
                LinkError::BlankToken {
                    link: "billing".into(),
                },
            ),
            (
                "short-token",
                LinkError::TooShort {
                    link: "billing".into(),
                },
            ),
            (
                "has whitespace inside 0123456789abcdef",
                LinkError::NonCanonical {
                    link: "billing".into(),
                },
            ),
            (
                "0123456789abcdef0123456789abcdef\n",
                LinkError::NonCanonical {
                    link: "billing".into(),
                },
            ),
            (
                "0123456789abcdef0123456789abcdef+/=",
                LinkError::NonCanonical {
                    link: "billing".into(),
                },
            ),
            (
                "0123456789abcdef0123456789abcdéf",
                LinkError::NonCanonical {
                    link: "billing".into(),
                },
            ),
        ];
        for (token, expected) in cases {
            let error = error_for(token);
            assert_eq!(error, expected, "{token:?}");
            let message = error.to_string();
            assert!(message.contains("billing"), "{message}");
            if !token.trim().is_empty() {
                assert!(!message.contains(token.trim()), "{message}");
            }
        }
    }

    #[test]
    fn blank_and_duplicate_names() {
        assert_eq!(
            TokenMap::new([InboundLink::untrusted(" ", Secret::new(BILLING))]).unwrap_err(),
            LinkError::BlankName
        );
        assert_eq!(
            TokenMap::new([
                InboundLink::untrusted("billing", Secret::new(BILLING)),
                InboundLink::trusted("billing", Secret::new(ORDERS)),
            ])
            .unwrap_err(),
            LinkError::DuplicateName {
                link: "billing".into()
            }
        );
    }

    #[test]
    fn shared_token_is_refused() {
        let error = TokenMap::new([
            InboundLink::untrusted("billing", Secret::new(BILLING)),
            InboundLink::untrusted("orders", Secret::new(BILLING)),
        ])
        .unwrap_err();
        assert_eq!(
            error,
            LinkError::DuplicateToken {
                first: "billing".into(),
                second: "orders".into()
            }
        );
        assert!(!error.to_string().contains(BILLING));
    }

    #[test]
    fn debug_never_shows_tokens() {
        let debug = format!("{:?}", map());
        assert!(debug.contains("billing"));
        for token in [BILLING, ORDERS, REPORTS] {
            assert!(!debug.contains(token));
        }
        let link = format!(
            "{:?}",
            InboundLink::trusted("billing", Secret::new(BILLING))
        );
        assert!(!link.contains(BILLING));
    }

    #[test]
    fn random_tokens_are_canonical_and_distinct() {
        let first = random_token().unwrap();
        let second = random_token().unwrap();
        assert_eq!(first.expose().len(), 43);
        validate_token("generated", first.expose()).unwrap();
        assert_ne!(first.expose(), second.expose());
        let map = TokenMap::new([InboundLink::untrusted("generated", first.clone())]).unwrap();
        assert_eq!(
            map.authenticate(first.expose()),
            Some(ServiceIdentity::untrusted("generated"))
        );
    }

    #[test]
    fn base64url_vectors() {
        assert_eq!(base64url(b""), "");
        assert_eq!(base64url(b"f"), "Zg");
        assert_eq!(base64url(b"fo"), "Zm8");
        assert_eq!(base64url(b"foo"), "Zm9v");
        assert_eq!(base64url(b"foobar"), "Zm9vYmFy");
        assert_eq!(base64url(&[0xfb, 0xff, 0xbf]), "-_-_");
        assert_eq!(base64url(&[0_u8; 32]).len(), 43);
    }

    #[test]
    fn uniqueness_across_maps() {
        let orders_service = map();
        let shipping_service = TokenMap::new([InboundLink::untrusted(
            "gateway",
            Secret::new("gateway-token-0123456789abcdefABCDEF"),
        )])
        .unwrap();
        assert!(
            validate_unique(&[("orders", &orders_service), ("shipping", &shipping_service)])
                .is_ok()
        );
        assert!(validate_unique(&[]).is_ok());

        let reusing =
            TokenMap::new([InboundLink::untrusted("payments", Secret::new(ORDERS))]).unwrap();
        let error = validate_unique(&[
            ("orders", &orders_service),
            ("shipping", &shipping_service),
            ("payments", &reusing),
        ])
        .unwrap_err();
        assert_eq!(
            error,
            LinkError::DuplicateToken {
                first: "orders/orders".into(),
                second: "payments/payments".into()
            }
        );
        assert!(!error.to_string().contains(ORDERS));
    }
}
