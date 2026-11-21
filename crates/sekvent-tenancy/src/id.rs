//! Tenant ids.

use std::fmt;

use uuid::Uuid;

/// The length of the lower-case hyphenated form.
const HYPHENATED_LEN: usize = 36;

/// A tenant's id. The nil UUID is the one tenant of a process that serves
/// a fixed scope.
///
/// `Display` and `Debug` both print the lower-case hyphenated form, the one
/// text a selector header, a token claim, a notification payload and the
/// request scope carry.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TenantId(Uuid);

impl TenantId {
    /// The tenant of a process that serves a fixed scope: the nil UUID.
    pub const LOCAL: Self = Self(Uuid::nil());

    /// The tenant with this UUID.
    pub const fn from_uuid(id: Uuid) -> Self {
        Self(id)
    }

    /// The UUID of this tenant.
    pub const fn as_uuid(&self) -> &Uuid {
        &self.0
    }

    /// Whether this is [`TenantId::LOCAL`].
    pub const fn is_local(&self) -> bool {
        self.0.is_nil()
    }

    /// The lower-case hyphenated form only (36 characters); any other
    /// spelling of a UUID is `None`.
    pub fn parse(text: &str) -> Option<Self> {
        if text.len() != HYPHENATED_LEN {
            return None;
        }
        let id = Uuid::try_parse(text).ok()?;
        let mut buffer = Uuid::encode_buffer();
        (&*id.hyphenated().encode_lower(&mut buffer) == text).then_some(Self(id))
    }
}

impl From<Uuid> for TenantId {
    fn from(id: Uuid) -> Self {
        Self(id)
    }
}

impl From<TenantId> for Uuid {
    fn from(id: TenantId) -> Self {
        id.0
    }
}

impl fmt::Display for TenantId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut buffer = Uuid::encode_buffer();
        f.write_str(self.0.hyphenated().encode_lower(&mut buffer))
    }
}

impl fmt::Debug for TenantId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEXT: &str = "0b9c6f1e-2d4a-4c8b-9e3f-5a6b7c8d9e0f";

    #[test]
    fn parse_accepts_the_lower_case_hyphenated_form() {
        let id = TenantId::parse(TEXT).unwrap();
        assert_eq!(id.as_uuid(), &Uuid::parse_str(TEXT).unwrap());
        assert!(!id.is_local());
    }

    #[test]
    fn parse_refuses_every_other_spelling() {
        let upper = TEXT.to_ascii_uppercase();
        let mixed = format!("0B{}", &TEXT[2..]);
        let braced = format!("{{{TEXT}}}");
        let simple = TEXT.replace('-', "");
        let urn = format!("urn:uuid:{TEXT}");
        let padded = format!(" {TEXT}");
        let moved = "0b9c6f1e2-d4a-4c8b-9e3f-5a6b7c8d9e0f";
        for text in [
            upper.as_str(),
            mixed.as_str(),
            braced.as_str(),
            simple.as_str(),
            urn.as_str(),
            padded.as_str(),
            moved,
            "",
            "not-a-uuid",
            "0b9c6f1e-2d4a-4c8b-9e3f-5a6b7c8d9e0g",
        ] {
            assert_eq!(TenantId::parse(text), None, "{text:?}");
        }
    }

    #[test]
    fn the_nil_text_parses_to_local() {
        let id = TenantId::parse("00000000-0000-0000-0000-000000000000").unwrap();
        assert_eq!(id, TenantId::LOCAL);
        assert!(id.is_local());
    }

    #[test]
    fn display_and_debug_print_the_text_that_parses_back() {
        let id = TenantId::from_uuid(Uuid::new_v4());
        let text = id.to_string();
        assert_eq!(text.len(), 36);
        assert_eq!(TenantId::parse(&text), Some(id));
        assert_eq!(format!("{id:?}"), text);
        assert_eq!(
            TenantId::LOCAL.to_string(),
            "00000000-0000-0000-0000-000000000000"
        );
    }

    #[test]
    fn conversions_keep_the_uuid() {
        let uuid = Uuid::new_v4();
        let id = TenantId::from(uuid);
        assert_eq!(Uuid::from(id), uuid);
        assert_eq!(id, TenantId::from_uuid(uuid));
    }

    #[cfg(feature = "serving")]
    #[test]
    fn the_text_is_a_valid_tenant_header() {
        let id = TenantId::from_uuid(Uuid::new_v4());
        assert!(sekvent_context::headers::is_valid_tenant(&id.to_string()));
        assert!(sekvent_context::headers::is_valid_tenant(
            &TenantId::LOCAL.to_string()
        ));
    }
}
