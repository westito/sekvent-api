//! Bitbucket Cloud as an identity provider.

use std::fmt;
use std::time::Duration;

use futures::FutureExt as _;
use futures::future::BoxFuture;
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};
use sekvent_client::{HttpClient, RedirectPolicy};
use sekvent_config::{ConfigError, ConfigSource, EnvConfig, FromConfig, Prefixed, Secret};
use sekvent_context::CallContext;
use sekvent_error::AppError;
use serde::Deserialize;
use url::Url;

use crate::provider::{
    AuthorizationRequest, CodeExchange, IdentityProvider, ProviderTokens, SsoIdentity,
};
use crate::redirect::is_insecure;

/// Bitbucket Cloud's authorization endpoint.
pub const BITBUCKET_AUTHORIZE_URL: &str = "https://bitbucket.org/site/oauth2/authorize";
/// Bitbucket Cloud's token endpoint.
pub const BITBUCKET_TOKEN_URL: &str = "https://bitbucket.org/site/oauth2/access_token";
/// Bitbucket Cloud's REST API base.
pub const BITBUCKET_API_URL: &str = "https://api.bitbucket.org/2.0";

/// Characters left as they are in a path segment: RFC 3986 unreserved.
const SEGMENT: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~');

/// Settings of the Bitbucket Cloud provider.
///
/// The struct has no prefix of its own: read it under one you choose with
/// [`BitbucketConfig::load`], e.g. `SSO_BITBUCKET_` for
/// `SSO_BITBUCKET_CLIENT_ID`.
#[derive(Debug, EnvConfig)]
pub struct BitbucketConfig {
    /// The OAuth consumer's key.
    #[config(validate = not_blank)]
    pub client_id: String,
    /// The OAuth consumer's secret.
    pub client_secret: Secret,
    /// The callback URL registered on the consumer: where the router's
    /// `…/sso/bitbucket/callback` route is reachable from the browser.
    #[config(validate = secure_url)]
    pub callback_url: String,
    /// The workspace (slug or `{uuid}`) whose members may sign in.
    #[config(validate = valid_workspace)]
    pub workspace: String,
    /// Authorization endpoint (tests point it elsewhere).
    #[config(default = "https://bitbucket.org/site/oauth2/authorize", validate = secure_url)]
    pub authorize_url: String,
    /// Token endpoint.
    #[config(default = "https://bitbucket.org/site/oauth2/access_token", validate = secure_url)]
    pub token_url: String,
    /// REST API base.
    #[config(default = "https://api.bitbucket.org/2.0", validate = secure_url)]
    pub api_url: String,
    /// Timeout of each request to Bitbucket.
    #[config(default = "10s", validate = positive)]
    pub timeout: Duration,
}

impl BitbucketConfig {
    /// Read the settings under `prefix` (used verbatim, include the
    /// separator), e.g. `SSO_BITBUCKET_`. Errors name the full key.
    pub fn load(source: &dyn ConfigSource, prefix: &str) -> Result<Self, ConfigError> {
        Self::from_config(&Prefixed::new(source, prefix))
    }
}

#[allow(clippy::ptr_arg)] // validators receive `&T`.
fn not_blank(value: &String) -> Result<(), String> {
    if value.trim().is_empty() {
        Err("must not be blank".into())
    } else {
        Ok(())
    }
}

#[allow(clippy::ptr_arg)]
fn secure_url(value: &String) -> Result<(), String> {
    let url = Url::parse(value).map_err(|_| "must be an absolute URL".to_owned())?;
    if !matches!(url.scheme(), "http" | "https") || url.fragment().is_some() {
        return Err("must be an http(s) URL without a fragment".into());
    }
    if is_insecure(&url) {
        return Err("must use https unless it points at a loopback host".into());
    }
    Ok(())
}

#[allow(clippy::ptr_arg)]
fn valid_workspace(value: &String) -> Result<(), String> {
    let slug = !value.is_empty()
        && value.len() <= 100
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'));
    let uuid = value.len() == 38
        && value.starts_with('{')
        && value.ends_with('}')
        && value[1..37]
            .bytes()
            .all(|b| b.is_ascii_hexdigit() || b == b'-');
    if slug || uuid {
        Ok(())
    } else {
        Err("must be a workspace slug or {uuid}".into())
    }
}

#[allow(clippy::trivially_copy_pass_by_ref)]
fn positive(value: &Duration) -> Result<(), String> {
    if value.is_zero() {
        Err("must be longer than zero".into())
    } else {
        Ok(())
    }
}

/// Bitbucket Cloud: OAuth 2.0 authorization code grant, then the account,
/// workspace membership and verified primary email from the REST API.
///
/// Only members of the configured workspace are admitted
/// (`GET /2.0/user/workspaces/{workspace}/permission`; `403` or `404`
/// refuses with `PERMISSION_DENIED`, reason `SSO_NOT_A_MEMBER`). The
/// subject is the account's `uuid`. The email is the address marked
/// primary and confirmed in `GET /2.0/user/emails`, or none.
///
/// Bitbucket Cloud takes its scopes from the consumer's permissions, so the
/// authorization URL names none, and it does not enforce PKCE, so none is
/// sent. The consumer needs the permissions **Account: Read** (scope
/// `account`: the user and the workspace permission) and **Account: Email**
/// (scope `email`).
pub struct BitbucketProvider {
    client_id: String,
    callback_url: String,
    workspace: String,
    workspace_segment: String,
    authorize_url: Url,
    token_url: String,
    token: HttpClient,
    api: HttpClient,
}

impl fmt::Debug for BitbucketProvider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BitbucketProvider")
            .field("client_id", &self.client_id)
            .field("workspace", &self.workspace)
            .finish_non_exhaustive()
    }
}

impl BitbucketProvider {
    /// The provider id, also its route segment.
    pub const ID: &'static str = "bitbucket";

    /// Validate `config` and build the HTTP clients. Errors name the
    /// setting (`CALLBACK_URL`, …), never its value.
    pub fn new(config: BitbucketConfig) -> Result<Self, AppError> {
        let checks: [(&str, Result<(), String>); 7] = [
            ("CLIENT_ID", not_blank(&config.client_id)),
            ("CALLBACK_URL", secure_url(&config.callback_url)),
            ("WORKSPACE", valid_workspace(&config.workspace)),
            ("AUTHORIZE_URL", secure_url(&config.authorize_url)),
            ("TOKEN_URL", secure_url(&config.token_url)),
            ("API_URL", secure_url(&config.api_url)),
            ("TIMEOUT", positive(&config.timeout)),
        ];
        for (key, check) in checks {
            if let Err(reason) = check {
                return Err(AppError::invalid_argument(format!(
                    "the Bitbucket SSO setting {key} {reason}"
                )));
            }
        }
        if config.client_secret.is_blank() {
            return Err(AppError::invalid_argument(
                "the Bitbucket SSO setting CLIENT_SECRET must not be blank",
            ));
        }
        let built = |error: sekvent_client::BuildError| {
            AppError::invalid_argument(format!("the Bitbucket SSO HTTP client: {error}"))
        };
        let token = HttpClient::builder()
            .basic_auth(config.client_id.clone(), config.client_secret)
            .request_timeout(config.timeout)
            .connect_timeout(config.timeout)
            .redirects(RedirectPolicy::None)
            .propagate_context(false)
            .build()
            .map_err(built)?;
        let api = HttpClient::builder()
            .base_url(config.api_url)
            .request_timeout(config.timeout)
            .connect_timeout(config.timeout)
            .redirects(RedirectPolicy::None)
            .propagate_context(false)
            .build()
            .map_err(built)?;
        Ok(Self {
            workspace_segment: utf8_percent_encode(&config.workspace, SEGMENT).to_string(),
            authorize_url: Url::parse(&config.authorize_url).expect("validated above"),
            client_id: config.client_id,
            callback_url: config.callback_url,
            workspace: config.workspace,
            token_url: config.token_url,
            token,
            api,
        })
    }

    /// Read [`BitbucketConfig`] under `prefix` and build the provider.
    pub fn from_config(source: &dyn ConfigSource, prefix: &str) -> Result<Self, AppError> {
        let config = BitbucketConfig::load(source, prefix)
            .map_err(|error| AppError::invalid_argument(error.to_string()))?;
        Self::new(config)
    }

    /// The required workspace.
    pub fn workspace(&self) -> &str {
        &self.workspace
    }

    async fn exchange(&self, ctx: &CallContext, code: &str) -> Result<ProviderTokens, AppError> {
        let form: String = form_urlencoded::Serializer::new(String::new())
            .append_pair("grant_type", "authorization_code")
            .append_pair("code", code)
            .append_pair("redirect_uri", &self.callback_url)
            .finish();
        let response: TokenResponse = self
            .token
            .post(&self.token_url)
            .body(form, "application/x-www-form-urlencoded")
            .header("accept", "application/json")
            .send_json(ctx)
            .await?;
        let access = response
            .access_token
            .filter(|token| !token.trim().is_empty())
            .ok_or_else(|| AppError::internal("the token response has no access token"))?;
        if response
            .token_type
            .as_deref()
            .is_some_and(|kind| !kind.eq_ignore_ascii_case("bearer"))
        {
            return Err(AppError::internal(
                "the token response is not a bearer token",
            ));
        }
        let mut tokens = ProviderTokens::new(Secret::new(access));
        if let Some(refresh) = response.refresh_token.filter(|t| !t.is_empty()) {
            tokens = tokens.with_refresh_token(Secret::new(refresh));
        }
        if let Some(seconds) = response.expires_in {
            tokens = tokens.with_expires_in(Duration::from_secs(seconds));
        }
        Ok(tokens)
    }

    async fn fetch_identity(
        &self,
        ctx: &CallContext,
        tokens: &ProviderTokens,
    ) -> Result<SsoIdentity, AppError> {
        let bearer = format!("Bearer {}", tokens.access_token.expose());
        let user: User = self
            .api
            .get("user")
            .header("authorization", &bearer)
            .send_json(ctx)
            .await?;
        let subject = user
            .uuid
            .filter(|uuid| !uuid.trim().is_empty())
            .ok_or_else(|| AppError::internal("the Bitbucket user has no uuid"))?;

        let membership = self
            .api
            .get(&format!(
                "user/workspaces/{}/permission",
                self.workspace_segment
            ))
            .header("authorization", &bearer)
            .send(ctx)
            .await;
        if let Err(error) = membership {
            let status = error.metadata().get("upstream_status").map(String::as_str);
            return Err(if matches!(status, Some("403" | "404")) {
                AppError::permission_denied("the account is not a member of the required workspace")
                    .with_reason("SSO_NOT_A_MEMBER")
            } else {
                error
            });
        }

        let emails: Emails = self
            .api
            .get("user/emails")
            .query(&[("pagelen", "100")])
            .header("authorization", &bearer)
            .send_json(ctx)
            .await?;
        let email = emails
            .values
            .into_iter()
            .find(|email| email.is_primary && email.is_confirmed)
            .map(|email| email.email)
            .filter(|email| !email.is_empty());

        let mut identity = SsoIdentity::new(Self::ID, subject).with_group(self.workspace.clone());
        if let Some(email) = email {
            identity = identity.with_verified_email(email);
        }
        if let Some(name) = user.display_name.filter(|n| !n.is_empty()) {
            identity = identity.with_display_name(name);
        }
        if let Some(nickname) = user.nickname.filter(|n| !n.is_empty()) {
            identity = identity.with_username(nickname);
        }
        if let Some(account_id) = user.account_id.filter(|a| !a.is_empty()) {
            identity = identity.with_attribute("account_id", account_id);
        }
        Ok(identity)
    }
}

impl IdentityProvider for BitbucketProvider {
    fn id(&self) -> &str {
        Self::ID
    }

    fn authorization_url(&self, request: &AuthorizationRequest<'_>) -> Result<String, AppError> {
        let mut url = self.authorize_url.clone();
        url.query_pairs_mut()
            .append_pair("client_id", &self.client_id)
            .append_pair("response_type", "code")
            .append_pair("state", request.state)
            .append_pair("redirect_uri", &self.callback_url);
        Ok(url.into())
    }

    fn exchange_code<'a>(
        &'a self,
        ctx: &'a CallContext,
        exchange: CodeExchange<'a>,
    ) -> BoxFuture<'a, Result<ProviderTokens, AppError>> {
        self.exchange(ctx, exchange.code).boxed()
    }

    fn identity<'a>(
        &'a self,
        ctx: &'a CallContext,
        tokens: &'a ProviderTokens,
    ) -> BoxFuture<'a, Result<SsoIdentity, AppError>> {
        self.fetch_identity(ctx, tokens).boxed()
    }
}

/// The token endpoint's answer. Not `Debug`: it holds tokens.
#[derive(Deserialize)]
struct TokenResponse {
    access_token: Option<String>,
    token_type: Option<String>,
    refresh_token: Option<String>,
    expires_in: Option<u64>,
}

#[derive(Deserialize)]
struct User {
    uuid: Option<String>,
    account_id: Option<String>,
    display_name: Option<String>,
    nickname: Option<String>,
}

#[derive(Deserialize)]
struct Emails {
    #[serde(default)]
    values: Vec<Address>,
}

#[derive(Deserialize)]
struct Address {
    email: String,
    #[serde(default)]
    is_primary: bool,
    #[serde(default)]
    is_confirmed: bool,
}

#[cfg(test)]
mod tests {
    use sekvent_config::MapSource;
    use sekvent_error::ErrorCode;

    use super::*;

    fn source() -> MapSource {
        MapSource::new()
            .with("SSO_BB_CLIENT_ID", "consumer-key")
            .with("SSO_BB_CLIENT_SECRET", "consumer-secret")
            .with(
                "SSO_BB_CALLBACK_URL",
                "https://app.example.com/api/sso/bitbucket/callback",
            )
            .with("SSO_BB_WORKSPACE", "acme")
    }

    #[test]
    fn config_reads_keys_under_the_prefix_with_defaults() {
        let config = BitbucketConfig::load(&source(), "SSO_BB_").unwrap();
        assert_eq!(config.client_id, "consumer-key");
        assert_eq!(config.workspace, "acme");
        assert_eq!(config.authorize_url, BITBUCKET_AUTHORIZE_URL);
        assert_eq!(config.token_url, BITBUCKET_TOKEN_URL);
        assert_eq!(config.api_url, BITBUCKET_API_URL);
        assert_eq!(config.timeout, Duration::from_secs(10));
        assert!(!format!("{config:?}").contains("consumer-secret"));
    }

    #[test]
    fn a_missing_workspace_names_the_full_key() {
        let source: MapSource = [
            ("SSO_BB_CLIENT_ID", "k"),
            ("SSO_BB_CLIENT_SECRET", "s"),
            ("SSO_BB_CALLBACK_URL", "https://app.example.com/cb"),
        ]
        .into_iter()
        .collect();
        let error = BitbucketConfig::load(&source, "SSO_BB_").unwrap_err();
        assert!(error.to_string().contains("SSO_BB_WORKSPACE"), "{error}");
        let error = BitbucketProvider::from_config(&source, "SSO_BB_").unwrap_err();
        assert_eq!(error.code(), ErrorCode::InvalidArgument);
        assert!(error.message().contains("SSO_BB_WORKSPACE"));
    }

    #[test]
    fn invalid_values_are_refused_by_key() {
        for (key, value) in [
            ("SSO_BB_CALLBACK_URL", "http://app.example.com/cb"),
            ("SSO_BB_CALLBACK_URL", "not a url"),
            ("SSO_BB_TOKEN_URL", "ftp://bitbucket.org/x"),
            ("SSO_BB_API_URL", "https://api.bitbucket.org/2.0#x"),
            ("SSO_BB_WORKSPACE", "acme/../x"),
            ("SSO_BB_WORKSPACE", ""),
            ("SSO_BB_CLIENT_ID", "  "),
            ("SSO_BB_TIMEOUT", "0s"),
        ] {
            let error = BitbucketConfig::load(&source().with(key, value), "SSO_BB_").unwrap_err();
            assert!(error.to_string().contains(key), "{key}: {error}");
        }
    }

    fn config() -> BitbucketConfig {
        BitbucketConfig::load(&source(), "SSO_BB_").unwrap()
    }

    #[test]
    fn new_revalidates_settings_built_in_code() {
        type Break = fn(&mut BitbucketConfig);
        let cases: [(&str, Break); 8] = [
            ("CLIENT_ID", |c| {
                c.client_id = String::new();
            }),
            ("CALLBACK_URL", |c| {
                c.callback_url = "http://example.com".into();
            }),
            ("WORKSPACE", |c| {
                c.workspace = "a b".into();
            }),
            ("AUTHORIZE_URL", |c| {
                c.authorize_url = "x".into();
            }),
            ("TOKEN_URL", |c| {
                c.token_url = "x".into();
            }),
            ("API_URL", |c| {
                c.api_url = "x".into();
            }),
            ("TIMEOUT", |c| {
                c.timeout = Duration::ZERO;
            }),
            ("CLIENT_SECRET", |c| {
                c.client_secret = Secret::new(" ");
            }),
        ];
        for (key, break_it) in cases {
            let mut config = config();
            break_it(&mut config);
            let error = BitbucketProvider::new(config).unwrap_err();
            assert_eq!(error.code(), ErrorCode::InvalidArgument);
            assert!(error.message().contains(key), "{key}: {}", error.message());
        }
    }

    #[test]
    fn workspace_uuids_are_accepted_and_encoded() {
        let mut config = config();
        config.workspace = "{470c176d-3574-44ea-bb41-89e8638bcca4}".into();
        let provider = BitbucketProvider::new(config).unwrap();
        assert_eq!(
            provider.workspace_segment,
            "%7B470c176d-3574-44ea-bb41-89e8638bcca4%7D"
        );
        assert_eq!(
            provider.workspace(),
            "{470c176d-3574-44ea-bb41-89e8638bcca4}"
        );
        assert!(valid_workspace(&"{zzzz}".to_owned()).is_err());
    }

    #[test]
    fn the_authorization_url_carries_client_state_and_callback_only() {
        let provider = BitbucketProvider::from_config(&source(), "SSO_BB_").unwrap();
        assert_eq!(provider.id(), "bitbucket");
        let url = provider
            .authorization_url(&AuthorizationRequest::new("st&te", "challenge"))
            .unwrap();
        let url = Url::parse(&url).unwrap();
        assert_eq!(url.host_str(), Some("bitbucket.org"));
        assert_eq!(url.path(), "/site/oauth2/authorize");
        let pairs: Vec<(String, String)> = url.query_pairs().into_owned().collect();
        assert_eq!(
            pairs,
            [
                ("client_id".to_owned(), "consumer-key".to_owned()),
                ("response_type".to_owned(), "code".to_owned()),
                ("state".to_owned(), "st&te".to_owned()),
                (
                    "redirect_uri".to_owned(),
                    "https://app.example.com/api/sso/bitbucket/callback".to_owned()
                ),
            ]
        );
        let debug = format!("{provider:?}");
        assert!(debug.contains("acme"));
        assert!(!debug.contains("consumer-secret"));
    }
}
