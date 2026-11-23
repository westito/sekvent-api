//! Minting admin tokens.

use std::fmt;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use sekvent_auth::{Claims, EdDsaSigningKey};
use sekvent_config::Secret;
use sekvent_context::Clock;
use sekvent_error::AppError;
use serde::Serialize;
use serde_json::{Map, Value};
use uuid::Uuid;

use crate::TenantId;

/// How long a minted token lives.
pub const TOKEN_LIFETIME: Duration = Duration::from_secs(60);

/// Mints admin tokens under one issuer and one signing key. `Debug` prints the issuer and the key
/// id only.
#[derive(Clone)]
pub struct Minter {
    issuer: String,
    key: EdDsaSigningKey,
    clock: Arc<dyn Clock>,
}

impl fmt::Debug for Minter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Minter")
            .field("issuer", &self.issuer)
            .field("kid", &self.key.kid())
            .finish_non_exhaustive()
    }
}

impl Minter {
    /// Tokens of `issuer`, signed by `key`, timed by `clock`.
    pub fn new(issuer: &str, key: EdDsaSigningKey, clock: Arc<dyn Clock>) -> Self {
        Self {
            issuer: issuer.to_owned(),
            key,
            clock,
        }
    }

    /// A token for the product instance `audience`, the RPC `op` (its path without the leading
    /// slash), at most one `tenant`, on behalf of `subject`, with the product's `extra` claims
    /// flattened: lifetime [`TOKEN_LIFETIME`], a fresh random id. Absent optional claims are
    /// omitted, never `null`.
    pub fn mint<X: Serialize>(
        &self,
        audience: &str,
        tenant: Option<TenantId>,
        op: &str,
        subject: &str,
        extra: &X,
    ) -> Result<Minted, AppError> {
        let extra = extra_claims(extra)?;
        let now = self
            .clock
            .now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| AppError::internal("the clock is before the Unix epoch"))?
            .as_secs();
        let token_id = Uuid::new_v4();
        let claims = Claims::new(
            subject,
            now,
            TOKEN_LIFETIME.as_secs(),
            AdminCustom {
                tid: tenant.map(|tenant| tenant.to_string()),
                op,
                extra,
            },
        )
        .with_issuer(self.issuer.as_str())
        .with_audience(audience)
        .with_jti(token_id.hyphenated().to_string());
        let expires_at = UNIX_EPOCH + Duration::from_secs(claims.exp);
        let token = self.key.issue(&claims)?;
        Ok(Minted {
            token: Secret::new(token),
            token_id,
            expires_at,
        })
    }

    /// `<key id>:<base64url public key>`: one entry of the list `admin::parse_keys` reads.
    pub fn public_key_entry(&self) -> String {
        format!(
            "{}:{}",
            self.key.kid(),
            URL_SAFE_NO_PAD.encode(self.key.public_key())
        )
    }
}

/// The claim names the minter sets itself; the product's extra claims may use none of them.
const RESERVED_CLAIMS: [&str; 9] = ["sub", "iss", "aud", "iat", "exp", "nbf", "jti", "tid", "op"];

/// The admin claims beside the registered ones.
#[derive(Serialize)]
struct AdminCustom<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    tid: Option<String>,
    op: &'a str,
    #[serde(flatten)]
    extra: Map<String, Value>,
}

/// `extra` as the object flattened into the token. `INVALID_ARGUMENT` when it does not serialize
/// to an object, or names a claim the minter sets.
fn extra_claims<X: Serialize>(extra: &X) -> Result<Map<String, Value>, AppError> {
    let Ok(Value::Object(object)) = serde_json::to_value(extra) else {
        return Err(AppError::invalid_argument(
            "the extra admin claims must serialize to an object",
        ));
    };
    if RESERVED_CLAIMS
        .iter()
        .any(|name| object.contains_key(*name))
    {
        return Err(AppError::invalid_argument(
            "the extra admin claims name a claim the minter sets",
        ));
    }
    Ok(object)
}

/// A minted token. `Debug` prints the id and the expiry, never the token.
pub struct Minted {
    /// The token.
    pub token: Secret,
    /// Its id (`jti`).
    pub token_id: Uuid,
    /// When it expires.
    pub expires_at: SystemTime,
}

impl fmt::Debug for Minted {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Minted")
            .field("token_id", &self.token_id)
            .field("expires_at", &self.expires_at)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use sekvent_auth::NoClaims;
    use sekvent_context::ManualClock;
    use sekvent_error::ErrorCode;
    use serde_json::json;

    use super::*;

    const START: Duration = Duration::from_hours(500_000);
    const OP: &str = "billing.admin.v1.TenantAdmin/Suspend";

    fn key(kid: &str) -> EdDsaSigningKey {
        EdDsaSigningKey::from_seed(kid, &[7; 32]).unwrap()
    }

    fn minter_at(at: SystemTime) -> Minter {
        Minter::new("ops-console", key("ops-1"), Arc::new(ManualClock::new(at)))
    }

    fn minter() -> Minter {
        minter_at(UNIX_EPOCH + START)
    }

    /// The JSON of a token's header and payload.
    fn parts(token: &Secret) -> (Value, Value) {
        let segments: Vec<&str> = token.expose().split('.').collect();
        assert_eq!(segments.len(), 3);
        let decode = |segment: &str| -> Value {
            serde_json::from_slice(&URL_SAFE_NO_PAD.decode(segment).unwrap()).unwrap()
        };
        (decode(segments[0]), decode(segments[1]))
    }

    #[derive(Serialize)]
    struct Ticket {
        ticket: &'static str,
        #[serde(skip_serializing_if = "Option::is_none")]
        reviewer: Option<&'static str>,
    }

    #[test]
    fn a_token_carries_exactly_the_admin_claims() {
        let tenant = TenantId::from_uuid(Uuid::new_v4());
        let minted = minter()
            .mint(
                "orders-eu",
                Some(tenant),
                OP,
                "operator-1",
                &Ticket {
                    ticket: "T-1",
                    reviewer: None,
                },
            )
            .unwrap();
        let (header, payload) = parts(&minted.token);
        assert_eq!(header["alg"], "EdDSA");
        assert_eq!(header["kid"], "ops-1");
        let now = START.as_secs();
        assert_eq!(
            payload,
            json!({
                "sub": "operator-1",
                "iat": now,
                "exp": now + 60,
                "iss": "ops-console",
                "aud": "orders-eu",
                "jti": minted.token_id.hyphenated().to_string(),
                "tid": tenant.to_string(),
                "op": OP,
                "ticket": "T-1",
            })
        );
        assert_eq!(minted.expires_at, UNIX_EPOCH + START + TOKEN_LIFETIME);
        assert_eq!(minted.token_id.get_version_num(), 4);
    }

    #[test]
    fn absent_claims_are_omitted_and_ids_are_fresh() {
        let minter = minter();
        let first = minter
            .mint("orders-eu", None, OP, "operator-1", &NoClaims {})
            .unwrap();
        let second = minter
            .mint("orders-eu", None, OP, "operator-1", &NoClaims {})
            .unwrap();
        let (_, payload) = parts(&first.token);
        let object = payload.as_object().unwrap();
        assert!(!object.contains_key("tid"));
        assert!(!object.contains_key("nbf"));
        assert!(object.values().all(|value| !value.is_null()));
        assert_eq!(
            object["exp"].as_u64().unwrap() - object["iat"].as_u64().unwrap(),
            60
        );
        assert_ne!(first.token_id, second.token_id);
        let jti = object["jti"].as_str().unwrap();
        assert_eq!(jti, jti.to_ascii_lowercase());
        assert_eq!(jti.len(), 36);
    }

    #[test]
    fn extra_claims_must_be_an_object_without_reserved_names() {
        let signer = minter();
        for name in RESERVED_CLAIMS {
            let mut extra = Map::new();
            extra.insert(name.to_owned(), json!("x"));
            let error = signer
                .mint("orders-eu", None, OP, "operator-1", &extra)
                .unwrap_err();
            assert_eq!(error.code(), ErrorCode::InvalidArgument, "{name}");
            assert!(!error.message().contains(name), "{name}");
        }
        for extra in [json!("text"), json!(3), json!([1]), Value::Null] {
            let error = signer
                .mint("orders-eu", None, OP, "operator-1", &extra)
                .unwrap_err();
            assert_eq!(error.code(), ErrorCode::InvalidArgument);
        }
        let minted = signer
            .mint(
                "orders-eu",
                None,
                OP,
                "operator-1",
                &json!({ "ticket": "T-2", "level": 2 }),
            )
            .unwrap();
        let (_, payload) = parts(&minted.token);
        assert_eq!(payload["ticket"], "T-2");
        assert_eq!(payload["level"], 2);
    }

    #[test]
    fn a_clock_before_the_epoch_is_internal() {
        let minter = minter_at(UNIX_EPOCH - Duration::from_secs(1));
        let error = minter
            .mint("orders-eu", None, OP, "operator-1", &NoClaims {})
            .unwrap_err();
        assert_eq!(error.code(), ErrorCode::Internal);
    }

    #[test]
    fn the_public_key_entry_names_the_key() {
        let signing = key("ops-1");
        let entry = minter().public_key_entry();
        assert_eq!(
            entry,
            format!("ops-1:{}", URL_SAFE_NO_PAD.encode(signing.public_key()))
        );
        assert_eq!(entry.split_once(':').unwrap().1.len(), 43);
    }

    #[test]
    fn debug_prints_no_token() {
        let signer = minter();
        let minted = signer
            .mint("orders-eu", None, OP, "operator-1", &NoClaims {})
            .unwrap();
        let printed = format!("{minted:?}");
        assert!(!printed.contains(minted.token.expose()));
        assert!(printed.contains(&minted.token_id.to_string()));
        let printed = format!("{signer:?}");
        assert!(printed.contains("ops-console"), "{printed}");
        assert!(printed.contains("ops-1"), "{printed}");
    }
}
