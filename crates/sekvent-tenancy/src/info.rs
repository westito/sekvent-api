//! What serving knows of a tenant: its state, its info and how a request names it.

use std::fmt;

use crate::host::origin_of_host;
use crate::{RoleKey, TenantId};

/// A tenant's lifecycle state. Stored and parsed in lower case.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum TenantState {
    /// Being set up; not served yet.
    Provisioning,
    /// Served.
    Active,
    /// Kept, but not served.
    Suspended,
    /// Being removed; not served.
    Purging,
    /// Removed; only the control row is left.
    Purged,
}

impl TenantState {
    /// `provisioning`, `active`, `suspended`, `purging`, `purged`.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Provisioning => "provisioning",
            Self::Active => "active",
            Self::Suspended => "suspended",
            Self::Purging => "purging",
            Self::Purged => "purged",
        }
    }

    /// The state of exactly one of the five lower-case texts.
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "provisioning" => Some(Self::Provisioning),
            "active" => Some(Self::Active),
            "suspended" => Some(Self::Suspended),
            "purging" => Some(Self::Purging),
            "purged" => Some(Self::Purged),
            _ => None,
        }
    }
}

/// What serving needs to know of a tenant. `Debug` prints id, subdomain and state only.
#[derive(Clone, PartialEq, Eq)]
pub struct TenantInfo {
    /// The tenant's id.
    pub id: TenantId,
    /// The tenant's label, as stored.
    pub subdomain: String,
    /// The name people see.
    pub display_name: String,
    /// The lifecycle state.
    pub state: TenantState,
    /// Empty only for a tenant without hosts (the fixed scope's, or a purging or purged row).
    pub primary_host: String,
    /// Canonical, byte-sorted, without duplicates; contains `primary_host` when that is not empty.
    pub hosts: Vec<String>,
    /// The key the tenant's role password is derived under.
    pub role_key: RoleKey,
}

impl TenantInfo {
    /// The fixed scope's: `TenantId::LOCAL`, `Active`, empty texts, no hosts,
    /// `RoleKey { key_id: String::new(), generation: 0 }`.
    pub fn local() -> Self {
        Self {
            id: TenantId::LOCAL,
            subdomain: String::new(),
            display_name: String::new(),
            state: TenantState::Active,
            primary_host: String::new(),
            hosts: Vec::new(),
            role_key: RoleKey {
                key_id: String::new(),
                generation: 0,
            },
        }
    }

    /// `origin_of_host(primary_host)`; `None` when there is no primary host.
    pub fn origin(&self) -> Option<String> {
        (!self.primary_host.is_empty()).then(|| origin_of_host(&self.primary_host))
    }
}

// `TenantInfo { id: .., subdomain: .., state: .., .. }`
impl fmt::Debug for TenantInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TenantInfo")
            .field("id", &self.id)
            .field("subdomain", &self.subdomain)
            .field("state", &self.state)
            .finish_non_exhaustive()
    }
}

/// How a request names a tenant.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Selector {
    /// The tenant's id.
    Id(TenantId),
    /// A canonical host.
    Host(String),
    /// A path segment on a listener host.
    PathLabel {
        /// The label of the path segment after the prefix (or after `.well-known/<name>/`), in
        /// the canonical form the product's label rules return (lowered), not as it was in the
        /// request.
        segment: String,
        /// The canonical host the request was sent to.
        listener_host: String,
    },
}

#[cfg(test)]
mod tests {
    use uuid::Uuid;

    use super::*;

    const ALL: [TenantState; 5] = [
        TenantState::Provisioning,
        TenantState::Active,
        TenantState::Suspended,
        TenantState::Purging,
        TenantState::Purged,
    ];

    fn billing() -> TenantInfo {
        TenantInfo {
            id: TenantId::from_uuid(Uuid::new_v4()),
            subdomain: "billing".to_owned(),
            display_name: "Billing Department".to_owned(),
            state: TenantState::Suspended,
            primary_host: "billing.example.com".to_owned(),
            hosts: vec![
                "billing.example.com".to_owned(),
                "pay.example.com".to_owned(),
            ],
            role_key: RoleKey {
                key_id: "k1".to_owned(),
                generation: 2,
            },
        }
    }

    #[test]
    fn every_state_round_trips_through_its_text() {
        let texts = ["provisioning", "active", "suspended", "purging", "purged"];
        for (state, text) in ALL.into_iter().zip(texts) {
            assert_eq!(state.as_str(), text);
            assert_eq!(TenantState::parse(text), Some(state));
        }
    }

    #[test]
    fn parse_refuses_every_other_text() {
        for text in [
            "Active", "ACTIVE", " active", "active ", "purge", "deleted", "",
        ] {
            assert_eq!(TenantState::parse(text), None, "{text:?}");
        }
    }

    #[test]
    fn the_local_info_is_the_fixed_scope_with_empty_texts() {
        let local = TenantInfo::local();
        assert_eq!(local.id, TenantId::LOCAL);
        assert_eq!(local.subdomain, "");
        assert_eq!(local.display_name, "");
        assert_eq!(local.state, TenantState::Active);
        assert_eq!(local.primary_host, "");
        assert_eq!(local.hosts, [] as [String; 0]);
        assert_eq!(local.role_key.key_id, "");
        assert_eq!(local.role_key.generation, 0);
        assert_eq!(local.origin(), None);
    }

    #[test]
    fn the_origin_is_https_for_a_public_host_and_http_on_loopback() {
        let public = billing();
        assert_eq!(
            public.origin().as_deref(),
            Some("https://billing.example.com")
        );

        let loopback = TenantInfo {
            primary_host: "billing.localhost:5173".to_owned(),
            hosts: vec!["billing.localhost:5173".to_owned()],
            ..billing()
        };
        assert_eq!(
            loopback.origin().as_deref(),
            Some("http://billing.localhost:5173")
        );

        let without = TenantInfo {
            primary_host: String::new(),
            hosts: Vec::new(),
            state: TenantState::Purged,
            ..billing()
        };
        assert_eq!(without.origin(), None);
    }

    #[test]
    fn debug_prints_the_id_the_subdomain_and_the_state_only() {
        let info = billing();
        let printed = format!("{info:?}");
        assert!(printed.contains(&info.id.to_string()), "{printed}");
        assert!(printed.contains("billing"), "{printed}");
        assert!(printed.contains("Suspended"), "{printed}");
        assert!(printed.ends_with(", .. }"), "{printed}");
        for hidden in ["Billing Department", "example.com", "k1"] {
            assert!(!printed.contains(hidden), "{printed}");
        }
    }

    #[test]
    fn selectors_compare_by_value() {
        let id = TenantId::from_uuid(Uuid::new_v4());
        assert_eq!(Selector::Id(id), Selector::Id(id));
        assert_ne!(
            Selector::Host("orders.example.com".to_owned()),
            Selector::PathLabel {
                segment: "orders".to_owned(),
                listener_host: "localhost:8080".to_owned(),
            }
        );
    }
}
