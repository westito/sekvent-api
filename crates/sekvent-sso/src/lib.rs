//! Browser single sign-on with OAuth 2.0 identity providers.
//!
//! [`Sso`] serves two routes per provider — `GET <path>/<id>/login` and
//! `GET <path>/<id>/callback` — that run the authorization code flow for a
//! browser: a signed, short-lived state cookie carries `state`, the PKCE
//! verifier and a validated return path between the two; the callback
//! exchanges the code, asks the provider for the user, hands the
//! [`SsoIdentity`] to the application's [`LoginHook`], and sends the browser
//! back to the application with a one-time handoff code in the URL
//! fragment. The application's frontend posts that code to one of its own
//! endpoints, which calls [`Sso::redeem`] and mints the application's
//! session (for example a `sekvent-auth` JWT). Provider tokens never leave
//! the server, and nothing but constant error codes ([`SsoErrorCode`])
//! reaches the browser.
//!
//! Providers implement [`IdentityProvider`]; [`BitbucketProvider`] is built
//! in and admits only members of one Bitbucket Cloud workspace.
//!
//! ```no_run
//! # fn demo() -> Result<(), Box<dyn std::error::Error>> {
//! use sekvent_config::{EnvSource, FromConfig, Prefixed};
//! use sekvent_context::CallContext;
//! use sekvent_error::AppError;
//! use sekvent_sso::{BitbucketProvider, Sso, SsoConfig, SsoIdentity};
//!
//! let settings = SsoConfig::from_config(&Prefixed::new(&EnvSource, "SSO_"))?;
//! let bitbucket = BitbucketProvider::from_config(&EnvSource, "SSO_BITBUCKET_")?;
//! let sso = Sso::from_config(settings)
//!     .provider(bitbucket)
//!     .on_login(|_ctx: CallContext, identity: SsoIdentity| async move {
//!         // Look up or create the user for (provider, subject).
//!         Ok::<_, AppError>(format!("{}:{}", identity.provider, identity.subject))
//!     })
//!     .build()?;
//! let routes = sso.router(); // mount with ServerBuilder::rest
//! let user = sso.redeem("code from the frontend"); // in the exchange endpoint
//! # let _ = (routes, user);
//! # Ok(())
//! # }
//! ```
//!
//! Handoff codes live in process memory: run one instance, or route the
//! callback and the redemption to the same one.

#![forbid(unsafe_code)]

mod bitbucket;
mod handoff;
mod provider;
mod redirect;
mod router;
mod state;

pub use bitbucket::{
    BITBUCKET_API_URL, BITBUCKET_AUTHORIZE_URL, BITBUCKET_TOKEN_URL, BitbucketConfig,
    BitbucketProvider,
};
pub use handoff::{DEFAULT_HANDOFF_CAPACITY, DEFAULT_HANDOFF_TTL, HandoffStore, MAX_HANDOFF_TTL};
pub use provider::{
    AuthorizationRequest, CodeExchange, IdentityProvider, ProviderTokens, SsoIdentity,
};
pub use redirect::{MAX_REDIRECT_LEN, is_safe_redirect};
pub use router::{
    DEFAULT_CALLBACK_TIMEOUT, DEFAULT_STATE_TTL, ERROR_PARAM, HANDOFF_PARAM, INSECURE_COOKIE_NAME,
    LoginHook, MAX_STATE_TTL, SECURE_COOKIE_NAME, Sso, SsoBuilder, SsoConfig, SsoErrorCode,
};
pub use state::MIN_STATE_KEY_LEN;
