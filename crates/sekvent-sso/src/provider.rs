//! The identity-provider abstraction and the values that cross it.

use std::collections::BTreeMap;
use std::fmt;
use std::time::Duration;

use futures::future::BoxFuture;
use sekvent_config::Secret;
use sekvent_context::CallContext;
use sekvent_error::AppError;

/// What a provider needs to build the URL that sends the browser to it.
#[derive(Debug, Clone, Copy)]
#[non_exhaustive]
pub struct AuthorizationRequest<'a> {
    /// The opaque `state` value; the provider must send it back unchanged.
    pub state: &'a str,
    /// The PKCE (RFC 7636) S256 code challenge. A provider that does not
    /// support PKCE leaves it out of the URL.
    pub code_challenge: &'a str,
}

impl<'a> AuthorizationRequest<'a> {
    /// A request with this state and PKCE challenge.
    pub fn new(state: &'a str, code_challenge: &'a str) -> Self {
        Self {
            state,
            code_challenge,
        }
    }
}

/// What a provider needs to exchange an authorization code for tokens.
#[non_exhaustive]
pub struct CodeExchange<'a> {
    /// The authorization code from the callback.
    pub code: &'a str,
    /// The PKCE code verifier matching the challenge sent with the
    /// authorization request. A provider without PKCE ignores it.
    pub code_verifier: &'a str,
}

impl<'a> CodeExchange<'a> {
    /// An exchange of `code`, proven with `code_verifier`.
    pub fn new(code: &'a str, code_verifier: &'a str) -> Self {
        Self {
            code,
            code_verifier,
        }
    }
}

impl fmt::Debug for CodeExchange<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CodeExchange").finish_non_exhaustive()
    }
}

/// Tokens a provider issued for one sign-in. They stay on the server: the
/// router uses them to fetch the identity and then drops them.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct ProviderTokens {
    /// The access token for the provider's API.
    pub access_token: Secret,
    /// The refresh token, when the provider issued one.
    pub refresh_token: Option<Secret>,
    /// The access token's lifetime, when the provider said.
    pub expires_in: Option<Duration>,
}

impl ProviderTokens {
    /// Tokens with only an access token.
    pub fn new(access_token: Secret) -> Self {
        Self {
            access_token,
            refresh_token: None,
            expires_in: None,
        }
    }

    /// Set the refresh token.
    #[must_use]
    pub fn with_refresh_token(mut self, token: Secret) -> Self {
        self.refresh_token = Some(token);
        self
    }

    /// Set the access token's lifetime.
    #[must_use]
    pub fn with_expires_in(mut self, expires_in: Duration) -> Self {
        self.expires_in = Some(expires_in);
        self
    }
}

/// A signed-in user as a provider reported them, normalised across
/// providers.
///
/// `email` is only ever an address the provider says the user has
/// confirmed; a provider that has none leaves it empty, and the application
/// decides whether it can do without.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct SsoIdentity {
    /// The provider id, e.g. `bitbucket`.
    pub provider: String,
    /// The provider's stable, never-reused id for the account. Link accounts
    /// on `(provider, subject)`, never on the email address.
    pub subject: String,
    /// The verified primary email address, if the provider has one.
    pub email: Option<String>,
    /// The display name.
    pub display_name: Option<String>,
    /// The account's handle, e.g. the Bitbucket nickname. Not unique; for
    /// display only.
    pub username: Option<String>,
    /// The memberships the provider checked and confirmed (for Bitbucket,
    /// the required workspace).
    pub groups: Vec<String>,
    /// Further provider-specific attributes (for Bitbucket, `account_id`).
    pub attributes: BTreeMap<String, String>,
}

impl SsoIdentity {
    /// An identity with only provider and subject.
    pub fn new(provider: impl Into<String>, subject: impl Into<String>) -> Self {
        Self {
            provider: provider.into(),
            subject: subject.into(),
            email: None,
            display_name: None,
            username: None,
            groups: Vec::new(),
            attributes: BTreeMap::new(),
        }
    }

    /// Set the verified email address.
    #[must_use]
    pub fn with_verified_email(mut self, email: impl Into<String>) -> Self {
        self.email = Some(email.into());
        self
    }

    /// Set the display name.
    #[must_use]
    pub fn with_display_name(mut self, name: impl Into<String>) -> Self {
        self.display_name = Some(name.into());
        self
    }

    /// Set the handle.
    #[must_use]
    pub fn with_username(mut self, username: impl Into<String>) -> Self {
        self.username = Some(username.into());
        self
    }

    /// Add a confirmed membership.
    #[must_use]
    pub fn with_group(mut self, group: impl Into<String>) -> Self {
        self.groups.push(group.into());
        self
    }

    /// Add a provider-specific attribute.
    #[must_use]
    pub fn with_attribute(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.attributes.insert(key.into(), value.into());
        self
    }
}

/// An identity provider (plain OAuth 2.0 or OIDC) the router can sign
/// users in with.
///
/// The router owns the browser side (state, PKCE, cookies, redirects); a
/// provider only builds its authorization URL, exchanges a code and turns
/// tokens into an [`SsoIdentity`]. A provider enforces its own admission
/// rules (for Bitbucket, workspace membership) in [`identity`]: a user it
/// refuses is an `AppError` with code `PERMISSION_DENIED`, which the router
/// answers with `access_denied`.
///
/// Errors must not carry upstream text in their messages; the router never
/// shows them to the browser, but logs them once.
///
/// [`identity`]: IdentityProvider::identity
pub trait IdentityProvider: Send + Sync + 'static {
    /// The provider id used in routes and in [`SsoIdentity::provider`]:
    /// lowercase ASCII letters, digits and `-`, e.g. `bitbucket`.
    fn id(&self) -> &str;

    /// The absolute URL to send the browser to.
    fn authorization_url(&self, request: &AuthorizationRequest<'_>) -> Result<String, AppError>;

    /// Exchange an authorization code for tokens.
    fn exchange_code<'a>(
        &'a self,
        ctx: &'a CallContext,
        exchange: CodeExchange<'a>,
    ) -> BoxFuture<'a, Result<ProviderTokens, AppError>>;

    /// Fetch the signed-in user and check the provider's admission rules.
    fn identity<'a>(
        &'a self,
        ctx: &'a CallContext,
        tokens: &'a ProviderTokens,
    ) -> BoxFuture<'a, Result<SsoIdentity, AppError>>;
}

/// Whether `id` is a valid provider id.
pub(crate) fn valid_provider_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 32
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_builder_sets_every_field() {
        let identity = SsoIdentity::new("bitbucket", "{u-1}")
            .with_verified_email("ada@example.com")
            .with_display_name("Ada")
            .with_username("ada")
            .with_group("acme")
            .with_attribute("account_id", "a-1");
        assert_eq!(identity.provider, "bitbucket");
        assert_eq!(identity.subject, "{u-1}");
        assert_eq!(identity.email.as_deref(), Some("ada@example.com"));
        assert_eq!(identity.display_name.as_deref(), Some("Ada"));
        assert_eq!(identity.username.as_deref(), Some("ada"));
        assert_eq!(identity.groups, ["acme"]);
        assert_eq!(identity.attributes["account_id"], "a-1");
    }

    #[test]
    fn tokens_debug_hides_secrets() {
        let tokens = ProviderTokens::new(Secret::new("access-xyz"))
            .with_refresh_token(Secret::new("refresh-xyz"))
            .with_expires_in(Duration::from_secs(60));
        let debug = format!("{tokens:?}");
        assert!(!debug.contains("access-xyz"));
        assert!(!debug.contains("refresh-xyz"));
        assert_eq!(tokens.expires_in, Some(Duration::from_secs(60)));
    }

    #[test]
    fn exchange_debug_hides_the_code() {
        let exchange = CodeExchange::new("code-xyz", "verifier-xyz");
        let debug = format!("{exchange:?}");
        assert!(!debug.contains("code-xyz"));
        assert!(!debug.contains("verifier-xyz"));
        assert_eq!(exchange.code, "code-xyz");
        assert_eq!(exchange.code_verifier, "verifier-xyz");
        let request = AuthorizationRequest::new("s", "c");
        assert_eq!((request.state, request.code_challenge), ("s", "c"));
    }

    #[test]
    fn provider_ids() {
        assert!(valid_provider_id("bitbucket"));
        assert!(valid_provider_id("oidc-2"));
        assert!(!valid_provider_id(""));
        assert!(!valid_provider_id("Bitbucket"));
        assert!(!valid_provider_id("a/b"));
        assert!(!valid_provider_id(&"a".repeat(33)));
    }
}
