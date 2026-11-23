//! What a verified admin token says.

use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::TenantId;

/// The longest lifetime (`exp - iat`) of an admin token the verifier accepts.
pub const MAX_LIFETIME: Duration = Duration::from_secs(60);

/// No extra claims: the default of [`AdminClaims`] and the [`Verifier`](super::Verifier).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NoExtra {}

/// What a verified admin token says. `X` is the product's extra claims, flattened into the token.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AdminClaims<X = NoExtra> {
    /// The token's id (`jti`): recorded before the body runs, so the token is usable once.
    pub token_id: Uuid,
    /// Who the operator service acts for (`sub`), as the product's subject check accepted it.
    pub subject: String,
    /// The tenant the call addresses (`tid`); `None` only for a call that addresses no tenant.
    pub tenant: Option<TenantId>,
    /// The RPC the token was minted for (`op`): its path without the leading slash.
    pub op: String,
    /// When the token expires (`exp`).
    pub expires_at: SystemTime,
    /// The product's extra claims.
    pub extra: X,
}
