# Single sign-on (`sekvent::sso`)

`sekvent::sso` signs browser users in with an external identity provider
over the OAuth 2.0 authorization code flow. It serves a login and a callback
route per provider, keeps the flow's state in a signed short-lived cookie,
asks the provider who the user is, lets your code admit or refuse them, and
sends the browser back to your frontend with a **one-time handoff code** in
the URL fragment. Your frontend posts the code to one of your own endpoints,
which redeems it and mints your own session (for example a JWT from
[auth](auth.md)). Provider tokens never leave the server.

Bitbucket Cloud is built in (`BitbucketProvider`), admitting only members
of one workspace. Other providers implement `IdentityProvider`.

## Enable it

| | |
|---|---|
| Facade feature | `sso` |
| Module | `use sekvent::sso::…;` |
| Internal crate | `sekvent-sso` |
| Needs | a tokio runtime; mount the routes on the [server](server.md) |

```toml
sekvent-api = { git = "https://github.com/westito/sekvent-api", branch = "master", features = [
    "sso",
    "auth",               # to mint your session JWT
    "component-grpc",     # if the exchange endpoint is a component RPC
    "runtime-grpc-web",
] }
```

## Quick example

```rust
use std::sync::Arc;

use sekvent::config::{EnvSource, FromConfig, Prefixed};
use sekvent::context::CallContext;
use sekvent::error::AppError;
use sekvent::sso::{BitbucketProvider, Sso, SsoConfig, SsoIdentity};

/// What a finished sign-in hands to the exchange endpoint.
#[derive(Debug, Clone)]
struct SignedIn { user_id: String, roles: Vec<String> }

// SSO_APP_URL, SSO_STATE_KEY, … and SSO_BITBUCKET_CLIENT_ID, … (tables below)
let settings = SsoConfig::from_config(&Prefixed::new(&EnvSource, "SSO_"))?;
let bitbucket = BitbucketProvider::from_config(&EnvSource, "SSO_BITBUCKET_")?;

let users = users.clone();                                   // your repository
let sso = Sso::from_config(settings)
    .provider(bitbucket)
    .on_login(move |ctx: CallContext, identity: SsoIdentity| {
        let users = users.clone();
        async move {
            // Link on (provider, subject), never on the email address.
            let user = users.find_or_create(&ctx, &identity).await?;
            if user.disabled {
                return Err(AppError::permission_denied("account disabled"));
            }
            Ok(SignedIn { user_id: user.id, roles: user.roles })
        }
    })
    .build()?;

let server = Server::builder()
    .prefix("/api")
    .rest(sso.router())          // GET /api/sso/bitbucket/login, /api/sso/bitbucket/callback
    .grpc_routes(app.grpc_routes())
    .bind(addr)
    .await?;
```

The exchange endpoint, here an anonymous component method (see
[components](components.md#how-to-serve-components-to-end-users-browsers-apps)):

```rust
#[call(anonymous)]
async fn sso_exchange(&self, cx: &CallContext, request: SsoExchangeRequest) -> Result<Session, AuthError> {
    let signed_in = self.sso.redeem(&request.code).ok_or(AuthError::InvalidCode)?;
    let now = self.clock.now_unix_millis() / 1000;
    let claims = Claims::new(signed_in.user_id, now, 3600, Profile { roles: signed_in.roles });
    Ok(Session { token: self.keys.issue(&claims)? })
}
```

The frontend:

```ts
// Start: a full-page navigation, not fetch().
location.href = `/api/sso/bitbucket/login?redirect=${encodeURIComponent(location.pathname)}`;

// On return: read and remove the fragment before anything else.
const params = new URLSearchParams(location.hash.slice(1));
history.replaceState(null, "", location.pathname + location.search);
const code = params.get("sso_code");
const error = params.get("sso_error");   // access_denied, invalid_state, …
if (code) { const { token } = await auth.ssoExchange({ code }); /* store the session */ }
```

## Concepts

- **The flow.** `GET <path>/<provider>/login?redirect=/orders` checks the
  return path, creates `state` and a PKCE verifier from the operating
  system's random source, stores both with the return path in the state
  cookie and answers `302` to the provider. The provider sends the browser
  to `GET <path>/<provider>/callback?code=…&state=…`, which checks the
  cookie, exchanges the code, fetches the identity, runs your hook, stores
  its result under a handoff code and answers `302` to
  `<app_url>/orders#sso_code=<code>`. Every failure answers `302` to the
  return path (or the default path when the state is unusable) with
  `#sso_error=<code>`.
- **`SsoIdentity`**: `provider`, `subject` (the provider's stable account
  id), `email` (only a verified address, else `None`), `display_name`,
  `username` (a non-unique handle), `groups` (memberships the provider
  confirmed) and `attributes` (provider extras).
- **The login hook** decides admission and what the handoff carries: any
  `Fn(CallContext, SsoIdentity) -> impl Future<Output = Result<O, AppError>>`,
  or a type implementing `LoginHook<O>`. `O` is yours.
- **Handoff codes** are 43 characters of base64url (32 random bytes), work
  once and expire after 60 s. They live in the memory of the process that
  issued them.
- **Errors in the browser are constants** (`SsoErrorCode`); upstream text
  never reaches the browser. Each failure is logged once on the server.

## How to …

### Configure the router

```rust
impl<O: Send + 'static> Sso<O> {
    pub fn builder(app_url: impl Into<String>, state_key: Secret) -> SsoBuilder<O>;
    pub fn from_config(config: SsoConfig) -> SsoBuilder<O>;
    pub fn router(&self) -> axum::Router;
    pub fn redeem(&self, code: &str) -> Option<O>;
    pub fn handoff(&self) -> HandoffStore<O>;
}
```

| Builder | Default | Rule |
|---|---|---|
| `provider(p)`, `provider_arc(arc)` | — | at least one; ids unique |
| `on_login(hook)` | — | required |
| `secure_cookies(bool)` | `true` | `false` only for plain-http development |
| `state_ttl(d)` | 10 min | whole seconds, 1 s to 10 min |
| `handoff_ttl(d)` | 60 s | more than zero, at most 10 min |
| `handoff_capacity(n)` | 10 000 | at least 1 |
| `callback_timeout(d)` | 30 s | more than zero; bounds the provider calls and the hook |
| `path(p)` | `/sso` | `/a/b`: unreserved characters, no trailing `/` |
| `default_redirect(p)` | `/` | a safe relative path |
| `clock(c)` | `SystemClock` | cookie, replay and handoff expiry |

`app_url` is absolute `http`/`https` without query, fragment or user info
(`https://app.example.com`, or with a path such as
`https://example.com/console`). The state key must be at least 32 bytes;
give it a key of its own rather than the session JWT secret. `build`
returns `INVALID_ARGUMENT` naming the setting, never a value.

Configuration keys of `SsoConfig` (no prefix of its own; read it under
yours with `Prefixed`, e.g. `SSO_`):

| Key | Type | Default |
|---|---|---|
| `APP_URL` | URL | required |
| `STATE_KEY` | secret, ≥ 32 bytes | required |
| `SECURE_COOKIES` | bool | `on` |
| `STATE_TTL` | duration | `10m` |
| `HANDOFF_TTL` | duration | `60s` |
| `HANDOFF_CAPACITY` | integer | `10000` |
| `CALLBACK_TIMEOUT` | duration | `30s` |

### Sign in with Bitbucket Cloud

1. In the Bitbucket workspace settings, add an **OAuth consumer** with
   - the **Callback URL** where the callback route is reachable from the
     browser, e.g. `https://app.example.com/api/sso/bitbucket/callback`
     (server prefix + `path` + `/bitbucket/callback`);
   - the permissions **Account: Read** (scope `account`: the user and the
     workspace permission) and **Account: Email** (scope `email`). Nothing
     else is needed.
2. Configure the provider (keys under a prefix you choose):

| Key | Meaning | Default |
|---|---|---|
| `CLIENT_ID` | the consumer's key | required |
| `CLIENT_SECRET` | the consumer's secret | required |
| `CALLBACK_URL` | the callback URL registered on the consumer | required |
| `WORKSPACE` | slug (`acme`) or `{uuid}` of the workspace whose members may sign in | required |
| `AUTHORIZE_URL` | authorization endpoint | `https://bitbucket.org/site/oauth2/authorize` |
| `TOKEN_URL` | token endpoint | `https://bitbucket.org/site/oauth2/access_token` |
| `API_URL` | REST API base | `https://api.bitbucket.org/2.0` |
| `TIMEOUT` | timeout of each request to Bitbucket | `10s` |

URLs must be `https` unless they point at a loopback host (tests). A
missing `WORKSPACE` is a startup error: there is no "any account" mode.

```rust
let bitbucket = BitbucketProvider::from_config(&EnvSource, "SSO_BITBUCKET_")?;
// or: BitbucketProvider::new(BitbucketConfig::load(&EnvSource, "SSO_BITBUCKET_")?)?
```

What it does on a callback:

| Step | Request | Outcome |
|---|---|---|
| exchange | `POST <TOKEN_URL>`, HTTP Basic, `grant_type=authorization_code`, `code`, `redirect_uri` | never retried; a refusal is `server_error` |
| user | `GET <API_URL>/user` | `subject` = `uuid`; `display_name`, `username` = `nickname`, attribute `account_id` |
| membership | `GET <API_URL>/user/workspaces/<WORKSPACE>/permission` | `200`: member, `groups = [WORKSPACE]`; `403`/`404`: `access_denied` (reason `SSO_NOT_A_MEMBER`); anything else: `server_error` |
| email | `GET <API_URL>/user/emails?pagelen=100` | the address that is both primary and confirmed, else no email |

Bitbucket Cloud takes scopes from the consumer, so none is requested, and
it does not enforce PKCE, so no challenge is sent; `state` and the client
secret protect the flow.

### Redeem a handoff code

`sso.redeem(code)` (or `sso.handoff().redeem(code)`) returns the hook's
outcome once; an unknown, expired, used or malformed code gives `None`.
Answer every `None` the same way (for example `UNAUTHENTICATED`). The
endpoint must be callable without a session (`#[call(anonymous)]` or a
public REST route) and should be rate limited like any login endpoint.

`HandoffStore::issue(outcome)` stores an outcome yourself (tests, other
flows); it fails with `RESOURCE_EXHAUSTED` (`SSO_HANDOFF_FULL`) when the
store is full of live codes.

### Add another provider

```rust
impl IdentityProvider for Oidc {
    fn id(&self) -> &str { "google" }                        // [a-z0-9-], ≤ 32
    fn authorization_url(&self, request: &AuthorizationRequest<'_>) -> Result<String, AppError> {
        // request.state, request.code_challenge (S256): include both if supported
    }
    fn exchange_code<'a>(&'a self, ctx: &'a CallContext, exchange: CodeExchange<'a>)
        -> BoxFuture<'a, Result<ProviderTokens, AppError>> { /* exchange.code, exchange.code_verifier */ }
    fn identity<'a>(&'a self, ctx: &'a CallContext, tokens: &'a ProviderTokens)
        -> BoxFuture<'a, Result<SsoIdentity, AppError>> {
        // Refuse with AppError::permission_denied(..) to answer access_denied.
    }
}
```

Use [`sekvent::client::HttpClient`](client.md) for the calls, keep tokens
in `Secret`, and never put upstream text in error messages.

## Security rules

- **Return paths**: one leading `/` (not `//`), printable ASCII without
  spaces, no `\`, no `#`, at most 2 KiB (`is_safe_redirect`). Anything else
  answers `invalid_request` at the default path and sets no cookie.
- **State cookie**: `__Host-sekvent-sso` (`sekvent-sso` with
  `secure_cookies(false)`), `HttpOnly; SameSite=Lax; Path=/; Secure`,
  `Max-Age` = the state TTL. Value `v1.<payload>.<HMAC-SHA256>`; tampering,
  another key, expiry, another provider's cookie, a `state` that differs
  (compared in constant time) or appears twice, and a second callback with
  the same state all answer `invalid_state`. Every callback clears the
  cookie.
- **Responses** are `302 Found` with `Cache-Control: no-store` and
  `Referrer-Policy: no-referrer`. The handoff code is in the fragment,
  which browsers never send to a server.
- **Logging**: target `sekvent::sso`. Refusals (`invalid_state`, provider
  `error`, denials) at `info` with the reason; failures at `warn` with
  step, code, reason and the source chain (the HTTP client's sources carry
  no bodies or URLs). Codes, tokens, the cookie and emails are never
  logged.

## Errors

| `#sso_error=` | When |
|---|---|
| `access_denied` | the user cancelled at the provider; the provider refused them (not a workspace member); the hook returned `PERMISSION_DENIED` or `UNAUTHENTICATED` |
| `invalid_request` | unsafe or repeated `redirect` at login; a callback without exactly one `code` |
| `invalid_state` | the state cookie is missing, tampered, expired, for another provider, already used, or does not match `state` |
| `temporarily_unavailable` | a transient failure: timeout, provider unreachable or `503`, handoff store full, or the provider said `temporarily_unavailable` |
| `server_error` | anything else: token exchange refused, unexpected API answer, hook error, a provider `error` other than the two above |

`SsoErrorCode::for_error(&AppError)` is the mapping for provider and hook
errors. Unknown provider ids are not routed (`404`). A provider URL that
is not a valid header value answers `500`.

## Testing tips

- Drive `sso.router()` with `tower::ServiceExt::oneshot`: call login, take
  `state` from `Location` and the `name=value` part of `Set-Cookie`, then
  call the callback with both. No browser needed.
- A fake provider implementing `IdentityProvider` tests your hook and
  exchange endpoint without HTTP. For Bitbucket itself, point
  `AUTHORIZE_URL`, `TOKEN_URL` and `API_URL` at an axum fake on
  `127.0.0.1:0` (loopback `http` is allowed).
- Give the builder a `ManualClock` to expire cookies and codes without
  sleeping.

## Pitfalls

- **One process.** Codes are in memory: with several instances, route the
  callback and the exchange to the same one (sticky sessions), or run one.
- **Link accounts on `(provider, subject)`.** Email addresses change and
  may be absent; never key users on them.
- **Start login with a navigation**, not `fetch`: the browser must follow
  the redirects and store the cookie.
- **Read and drop the fragment first** (`history.replaceState`) so the code
  does not linger in the history or get copied with the URL.
- **The callback URL must match exactly** what the consumer has registered,
  including the server prefix.
- **`secure_cookies(false)` is for `http://localhost` only**; production
  must keep `Secure`.

## See also

- [Auth](auth.md): `JwtKeys`, `BearerAuth::end_user` for the session the
  exchange mints.
- [Components](components.md#how-to-serve-components-to-end-users-browsers-apps):
  `SERVE_AUTH=bearer`, `#[call(anonymous)]`.
- [Server](server.md): mounting REST routes under a prefix, CORS.
- [Client](client.md): the HTTP client the providers use.
- [Design: single sign-on](../design/sso.md).
