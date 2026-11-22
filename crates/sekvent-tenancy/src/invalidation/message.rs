//! The notifications of the invalidation channel and their payload texts.
//!
//! Two payloads exist: a change `t:<tenant>:<kind>` and a hint
//! `e:<tenant>:<topic>:<seq>`. None of the field grammars contains a `:`, so a
//! payload splits into its fields at every `:` and nowhere else.

use std::borrow::Cow;

use sekvent_error::AppError;

use crate::{DatabaseName, TenantId};

/// The longest change kind.
const KIND_MAX: usize = 16;
/// The longest topic.
const TOPIC_MAX: usize = 64;

/// The NOTIFY channel a product names. The grammar of `DatabaseName`.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Channel(String);

impl Channel {
    /// `INVALID_ARGUMENT` for a name `DatabaseName::parse` refuses.
    pub fn new(name: &str) -> Result<Self, AppError> {
        if DatabaseName::parse(name).is_ok() {
            Ok(Self(name.to_owned()))
        } else {
            Err(AppError::invalid_argument(
                "the channel name is not a valid identifier",
            ))
        }
    }

    /// The channel's name.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// What changed about a tenant: `^[a-z][a-z0-9_]{0,15}$`.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ChangeKind(Cow<'static, str>);

impl ChangeKind {
    /// The tenant's state changed.
    pub const STATE: Self = Self(Cow::Borrowed("state"));
    /// The tenant's hosts changed.
    pub const HOSTS: Self = Self(Cow::Borrowed("hosts"));

    /// A kind of the product's own. `INVALID_ARGUMENT` outside the grammar.
    pub fn new(kind: &str) -> Result<Self, AppError> {
        Self::checked(kind).ok_or_else(|| {
            AppError::invalid_argument(
                "a change kind is 1 to 16 characters of a-z, 0-9 and _, starting with a letter",
            )
        })
    }

    /// The kind's text.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The kind, when `kind` is in the grammar.
    fn checked(kind: &str) -> Option<Self> {
        let bytes = kind.as_bytes();
        let valid = (1..=KIND_MAX).contains(&bytes.len())
            && bytes[0].is_ascii_lowercase()
            && bytes
                .iter()
                .all(|&b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_');
        valid.then(|| Self(Cow::Owned(kind.to_owned())))
    }
}

/// A product's stream of tenant data: `^[A-Za-z0-9_-]{1,64}$`.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Topic(String);

impl Topic {
    /// `INVALID_ARGUMENT` outside the grammar.
    pub fn new(topic: &str) -> Result<Self, AppError> {
        Self::checked(topic).ok_or_else(|| {
            AppError::invalid_argument("a topic is 1 to 64 characters of A-Z, a-z, 0-9, _ and -")
        })
    }

    /// The topic's text.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The topic, when `topic` is in the grammar.
    fn checked(topic: &str) -> Option<Self> {
        let bytes = topic.as_bytes();
        let valid = (1..=TOPIC_MAX).contains(&bytes.len())
            && bytes
                .iter()
                .all(|&b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-');
        valid.then(|| Self(topic.to_owned()))
    }
}

/// One notification.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Message {
    /// `t:<tenant>:<kind>`
    Change {
        /// The tenant that changed.
        tenant: TenantId,
        /// What changed.
        kind: ChangeKind,
    },
    /// `e:<tenant>:<topic>:<seq>`, `seq` not negative
    Hint {
        /// The tenant whose data moved.
        tenant: TenantId,
        /// The product's stream that moved.
        topic: Topic,
        /// How far it moved.
        seq: i64,
    },
}

impl Message {
    /// Exactly the two shapes: the tenant in `TenantId`'s one text, `seq` in decimal digits
    /// without a sign or a leading zero (except `0`) and at most `i64::MAX`. Anything else is
    /// `None`.
    pub fn parse(payload: &str) -> Option<Self> {
        let mut fields = payload.split(':');
        let head = fields.next()?;
        let tenant = TenantId::parse(fields.next()?)?;
        let message = match head {
            "t" => Self::Change {
                tenant,
                kind: ChangeKind::checked(fields.next()?)?,
            },
            "e" => Self::Hint {
                tenant,
                topic: Topic::checked(fields.next()?)?,
                seq: parse_seq(fields.next()?)?,
            },
            _ => return None,
        };
        fields.next().is_none().then_some(message)
    }

    /// The payload `parse` reads back.
    pub fn encode(&self) -> String {
        match self {
            Self::Change { tenant, kind } => format!("t:{tenant}:{}", kind.as_str()),
            Self::Hint { tenant, topic, seq } => format!("e:{tenant}:{}:{seq}", topic.as_str()),
        }
    }
}

/// Decimal digits without a sign, without a leading zero (except `0` itself), at most
/// `i64::MAX`.
fn parse_seq(text: &str) -> Option<i64> {
    let bytes = text.as_bytes();
    let digits = !bytes.is_empty() && bytes.iter().all(u8::is_ascii_digit);
    let canonical = bytes.len() == 1 || bytes.first() != Some(&b'0');
    if digits && canonical {
        text.parse().ok()
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use sekvent_error::ErrorCode;

    use super::*;

    const ID: &str = "0b9c6f1e-2d4a-4c8b-9e3f-5a6b7c8d9e0f";

    fn tenant() -> TenantId {
        TenantId::parse(ID).unwrap()
    }

    fn topic(text: &str) -> Topic {
        Topic::new(text).unwrap()
    }

    #[test]
    fn a_change_and_a_hint_survive_a_round_trip() {
        let messages = [
            Message::Change {
                tenant: tenant(),
                kind: ChangeKind::STATE,
            },
            Message::Change {
                tenant: tenant(),
                kind: ChangeKind::new("plan_2").unwrap(),
            },
            Message::Hint {
                tenant: tenant(),
                topic: topic("orders"),
                seq: 0,
            },
            Message::Hint {
                tenant: tenant(),
                topic: topic("Stock-Levels_2"),
                seq: 1,
            },
            Message::Hint {
                tenant: tenant(),
                topic: topic("orders"),
                seq: i64::MAX,
            },
        ];
        for message in messages {
            let payload = message.encode();
            assert_eq!(Message::parse(&payload), Some(message), "{payload}");
        }
    }

    #[test]
    fn the_payload_texts_are_exactly_the_two_shapes() {
        let change = Message::Change {
            tenant: tenant(),
            kind: ChangeKind::HOSTS,
        };
        assert_eq!(change.encode(), format!("t:{ID}:hosts"));
        let hint = Message::Hint {
            tenant: tenant(),
            topic: topic("orders"),
            seq: 42,
        };
        assert_eq!(hint.encode(), format!("e:{ID}:orders:42"));
        assert_eq!(
            Message::parse(&format!("t:{ID}:state")),
            Some(Message::Change {
                tenant: tenant(),
                kind: ChangeKind::STATE,
            })
        );
    }

    #[test]
    fn every_other_payload_is_refused() {
        let upper = ID.to_uppercase();
        let simple = ID.replace('-', "");
        let above_max = "9223372036854775808";
        let far_above = u64::MAX.to_string();
        let long_topic = "a".repeat(TOPIC_MAX + 1);
        let payloads = [
            String::new(),
            ":".to_owned(),
            "t".to_owned(),
            format!("x:{ID}:state"),
            format!("T:{ID}:state"),
            format!("t:{ID}"),
            format!("t:{ID}:state:extra"),
            format!("t:{ID}:state:"),
            format!("e:{ID}:orders"),
            format!("e:{ID}:orders:1:2"),
            format!("t:{upper}:state"),
            format!("t:{{{ID}}}:state"),
            format!("t:{simple}:state"),
            format!("t:{ID}:State"),
            format!("t:{ID}:sTATE"),
            format!("t:{ID}:1state"),
            format!("t:{ID}:_state"),
            format!("t:{ID}:"),
            format!("t:{ID}:abcdefghijklmnopq"),
            format!("e:{ID}::1"),
            format!("e:{ID}:{long_topic}:1"),
            format!("e:{ID}:ord.ers:1"),
            format!("e:{ID}:ord:ers:1"),
            format!("e:{ID}:orders:-1"),
            format!("e:{ID}:orders:+1"),
            format!("e:{ID}:orders:01"),
            format!("e:{ID}:orders:00"),
            format!("e:{ID}:orders:"),
            format!("e:{ID}:orders:{above_max}"),
            format!("e:{ID}:orders:{far_above}"),
            format!("e:{ID}:orders:1a"),
            format!("e:{ID}:orders:x"),
            format!("e:{ID}:orders: 1"),
            format!("t:{ID}:state "),
            format!("t:{ID}:state\n"),
            format!(" t:{ID}:state"),
            format!("e:{ID}:orders:1 "),
        ];
        for payload in &payloads {
            assert_eq!(Message::parse(payload), None, "{payload:?}");
        }
    }

    #[test]
    fn the_sequence_number_is_canonical_decimal_up_to_the_largest_i64() {
        assert_eq!(parse_seq("0"), Some(0));
        assert_eq!(parse_seq("10"), Some(10));
        assert_eq!(parse_seq("9223372036854775807"), Some(i64::MAX));
        for text in [
            "",
            "-0",
            "+0",
            "00",
            "07",
            "9223372036854775808",
            "1_000",
            "\u{661}",
        ] {
            assert_eq!(parse_seq(text), None, "{text:?}");
        }
    }

    #[test]
    fn a_channel_has_the_grammar_of_a_database_name() {
        let longest = "a".repeat(63);
        for name in ["app_changes", "_x", "a", "x9", longest.as_str()] {
            assert_eq!(Channel::new(name).unwrap().as_str(), name);
        }
        let too_long = "a".repeat(64);
        for name in [
            "",
            "App",
            "1app",
            "app-changes",
            "app changes",
            "app\"x",
            "postgres",
            "template0",
            "template1",
            too_long.as_str(),
        ] {
            let error = Channel::new(name).unwrap_err();
            assert_eq!(error.code(), ErrorCode::InvalidArgument, "{name:?}");
            assert_eq!(
                error.message(),
                "the channel name is not a valid identifier"
            );
            assert_eq!(error.reason(), None);
        }
    }

    #[test]
    fn a_change_kind_is_a_short_lower_case_identifier() {
        assert_eq!(ChangeKind::STATE.as_str(), "state");
        assert_eq!(ChangeKind::HOSTS.as_str(), "hosts");
        assert_eq!(ChangeKind::new("state").unwrap(), ChangeKind::STATE);
        assert_eq!(ChangeKind::new("hosts").unwrap(), ChangeKind::HOSTS);
        for kind in ["a", "plan", "plan_2", "a_b_c", "abcdefghijklmnop"] {
            assert_eq!(ChangeKind::new(kind).unwrap().as_str(), kind);
        }
        for kind in [
            "",
            "Plan",
            "pLan",
            "2plan",
            "_plan",
            "plan-2",
            "plan.2",
            "plan:2",
            "abcdefghijklmnopq",
        ] {
            let error = ChangeKind::new(kind).unwrap_err();
            assert_eq!(error.code(), ErrorCode::InvalidArgument, "{kind:?}");
        }
    }

    #[test]
    fn a_topic_is_one_to_sixty_four_safe_characters() {
        let longest = "Z".repeat(TOPIC_MAX);
        for text in [
            "o",
            "orders",
            "Stock-Levels_2",
            "-",
            "_",
            "0",
            longest.as_str(),
        ] {
            assert_eq!(topic(text).as_str(), text);
        }
        let too_long = "a".repeat(TOPIC_MAX + 1);
        for text in [
            "",
            "ord ers",
            "ord.ers",
            "ord:ers",
            "ord/ers",
            "caf\u{e9}",
            too_long.as_str(),
        ] {
            let error = Topic::new(text).unwrap_err();
            assert_eq!(error.code(), ErrorCode::InvalidArgument, "{text:?}");
        }
    }
}
