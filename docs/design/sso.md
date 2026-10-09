# Browser single sign-on (`sekvent-sso`)

> Status: implemented. Builds on
> [`component-end-user.md`](component-end-user.md) (serving components to
> browsers with `SERVE_AUTH=bearer`) and
> [`p8-service-essentials.md`](p8-service-essentials.md) (server, CORS,
> REST routes under a prefix). The user guide is
> [modules/sso.md](../modules/sso.md).

## 1. Problem

An application with a browser frontend and sekvent components behind
`SERVE_AUTH=bearer` needs its users to sign in with an external account
(a code host, a workspace directory) without running an identity broker in
between. The application keeps its own session: it mints its own JWT
(`sekvent-auth` `JwtKeys`) and authorizes with its end-user authenticator.
What is missing is the browser leg of OAuth 2.0: sending the user to the
provider, coming back safely, proving who they are, and getting that
proof to the frontend without putting a token in a URL.

Doing this by hand per application is where the classic mistakes live:
no or reusable `state`, open redirects through a `returnTo` parameter,
tokens in query strings, upstream error text echoed to the browser,
provider tokens leaking into logs.

## 2. Scope

### In

- `IdentityProvider`: a dyn-compatible async trait (authorization URL, code
  exchange, identity) so GitHub, Google or generic OIDC can be added later.
- `BitbucketProvider` (Bitbucket Cloud), admitting only members of one
  workspace.
- `Sso`: per provider `GET <path>/<id>/login` and `…/callback` as an axum
  `Router` the application mounts with `ServerBuilder::rest`.
- A signed state cookie (state, PKCE verifier, return path, expiry), single
  use.
- The application's `LoginHook`, and a bounded in-memory `HandoffStore` of
  one-time codes the frontend redeems through the application's own
  endpoint.

### Out

| Item | Note |
|---|---|
| Session tokens, refresh, logout | the application's: it mints its JWT when it redeems the code |
| Other providers | the trait is ready; only Bitbucket is built in |
| A shared handoff store (Redis, database) | codes live in the process; one instance or sticky routing (decision 6) |
| ID-token validation (OIDC) | arrives with the first OIDC provider; Bitbucket has no ID token |
| Keeping provider tokens | they are dropped after the identity is read (decision 2) |

## 3. Placement

A new crate `sekvent-sso`, facade feature `sso`, module `sekvent::sso`.
It depends on `sekvent-config`, `-error`, `-context` and `-client`, plus
axum.

- Not inside `sekvent-auth`: the auth core is runtime-free and compiles for
  wasm; SSO needs axum, an outbound HTTP client and tokio.
- No dependency on `sekvent-auth`: SSO never issues or verifies a JWT. The
  application mints its own session from the handoff outcome, with
  whatever claims it wants. Keeping the crates apart also avoids a state
  cookie that could be mistaken for a session token (decision 4).
- No dependency on `sekvent-runtime`: the routes are a plain `Router`. The
  callback reads the `CallContext` the server's context layer stores in the
  request extensions when it is there and starts a fresh one otherwise, so
  the router also works outside `Server` (tests, other hosts). The `Ctx`
  extractor would refuse to run there by design.

## 4. Public API

```rust
pub trait IdentityProvider: Send + Sync + 'static {
    fn id(&self) -> &str;                                   // route segment, e.g. "bitbucket"
    fn authorization_url(&self, request: &AuthorizationRequest<'_>) -> Result<String, AppError>;
    fn exchange_code<'a>(&'a self, ctx: &'a CallContext, exchange: CodeExchange<'a>)
        -> BoxFuture<'a, Result<ProviderTokens, AppError>>;
    fn identity<'a>(&'a self, ctx: &'a CallContext, tokens: &'a ProviderTokens)
        -> BoxFuture<'a, Result<SsoIdentity, AppError>>;
}

#[non_exhaustive]
pub struct SsoIdentity {
    pub provider: String, pub subject: String, pub email: Option<String>,
    pub display_name: Option<String>, pub username: Option<String>,
    pub groups: Vec<String>, pub attributes: BTreeMap<String, String>,
}

pub trait LoginHook<O>: Send + Sync + 'static {   // also any Fn(CallContext, SsoIdentity) -> impl Future
    fn on_login(&self, ctx: CallContext, identity: SsoIdentity) -> BoxFuture<'static, Result<O, AppError>>;
}

let sso: Sso<O> = Sso::builder(app_url, state_key)       // or Sso::from_config(SsoConfig)
    .provider(BitbucketProvider::from_config(&EnvSource, "SSO_BITBUCKET_")?)
    .on_login(hook)
    .build()?;
sso.router();             // axum::Router
sso.redeem(&code);        // Option<O>, once
sso.handoff();            // HandoffStore<O>
```

`BoxFuture` keeps the provider trait object-safe so one router holds
several providers. The hook is generic over its outcome `O`: the
application decides what a handoff carries (a user id, roles, a tenant).

`SsoIdentity.email` is only ever a verified address; the brief's separate
`email_verified` flag was dropped because no built-in provider would ever
set it to `false` with an address present.

## 5. The flow

```text
browser                     app server (Sso)                      provider
  │ GET /api/sso/bitbucket/login?redirect=/orders
  │──────────────────────────▶ validate redirect; state, verifier ← OS RNG
  │◀────────────────────────── 302 provider URL, Set-Cookie state (signed, 10 min)
  │ ───────────────────────────────────────────────────────────────▶ consent
  │ GET /api/sso/bitbucket/callback?code&state  ◀─────────────────── 302
  │──────────────────────────▶ verify cookie, match state, mark used
  │                            exchange code ─────────────────────▶ token endpoint
  │                            identity, admission ───────────────▶ REST API
  │                            hook(identity) → O; handoff.issue(O) → code
  │◀────────────────────────── 302 <app>/orders#sso_code=<code>, clear cookie
  │ POST (gRPC-Web) Auth.SsoExchange{code}  — the application's anonymous RPC
  │──────────────────────────▶ sso.redeem(code) → O → JwtKeys::issue → session
```

## 6. Decisions

1. **Return path.** `redirect` must be a relative path: one leading `/`
   (never `//`), printable ASCII only (tabs and line breaks would be
   stripped by the browser and could turn `/\t/x` into `//x`), no `\`, no
   `#`, at most 2 KiB. It is appended to the configured `app_url`, so even a
   path that slipped through could not leave the origin. An invalid or
   repeated `redirect` sends the browser to the default path with
   `invalid_request`; no cookie is set.
2. **Tokens stay on the server.** Provider tokens are `Secret`s, used for
   the identity calls and dropped. Only the handoff code travels to the
   browser, in the URL **fragment**, which browsers never send to servers
   or put in `Referer`. Redirects carry `Cache-Control: no-store` and
   `Referrer-Policy: no-referrer`.
3. **Constant errors.** The browser sees one of `access_denied`,
   `invalid_request`, `invalid_state`, `server_error`,
   `temporarily_unavailable` (`#sso_error=…`). A provider's `error` is
   reduced to these; `error_description` is ignored. Each failure is logged
   once on the server (target `sekvent::sso`): denials at `info`, failures
   at `warn` with code, reason and the redacted source chain.
4. **State cookie.** `v1.<payload>.<HMAC-SHA256>` in base64url, payload
   JSON `{provider, state, verifier, redirect, expires_at}`. The MAC key is
   derived from the configured state key (≥ 32 bytes) with a fixed label,
   so a key reused elsewhere still yields a distinct MAC key, and the
   format is not a JWT, so a state cookie can never pass as a session token.
   The cookie is signed, not encrypted: the PKCE verifier is visible to the
   browser that owns it, which gains nothing from it. Attributes:
   `HttpOnly; SameSite=Lax; Path=/; Max-Age=<ttl>`, `Secure` and the
   `__Host-` name prefix unless `secure_cookies(false)` (plain-http
   development). `Lax` is required: the callback is a top-level cross-site
   navigation from the provider.
5. **Callback checks**, in order: a cookie that verifies at the current
   clock (planted extra cookies of the same name are skipped), the route's
   provider, the `state` parameter (exactly once, compared in constant
   time), not used before. A used state is remembered (digest only) until
   the cookie would have expired, in a bounded set that evicts the oldest
   entry when full; the cookie is also cleared on every callback answer.
   Replay is additionally stopped by the provider, which accepts a code
   once.
6. **Handoff store.** In memory, keyed by SHA-256 of the code (a lookup's
   timing reveals nothing about stored codes), 32-byte codes from the OS
   RNG, single use, TTL 60 s (≤ 10 min), capacity 10 000. Expired codes are
   swept lazily in insertion order on every call; no background task. A full
   store refuses new codes (`RESOURCE_EXHAUSTED` → `temporarily_unavailable`)
   rather than evicting someone's pending sign-in. Being in memory, a code
   must be redeemed on the instance that issued it.
7. **Admission belongs to the provider and the hook.** A provider refuses
   with `PERMISSION_DENIED` (Bitbucket: `SSO_NOT_A_MEMBER`); the hook may
   refuse with `PERMISSION_DENIED`/`UNAUTHENTICATED`. Both become
   `access_denied`. Transient codes become `temporarily_unavailable`,
   everything else `server_error`.
8. **Time.** Cookie expiry, handoff TTL and the replay set use the injected
   `Clock`. The callback narrows its call context to `callback_timeout`
   (30 s) for the provider calls and the hook; each Bitbucket request has
   its own `TIMEOUT` (10 s). A `POST` to the token endpoint is never
   retried; idempotent API reads follow the client's default policy.
9. **Fail closed at startup.** Missing or invalid keys name the key; no
   provider, a duplicate provider id, a missing hook, a short state key, a
   non-http(s) `app_url`, or an `http` provider URL to a non-loopback host
   are build errors.

## 7. Bitbucket Cloud

Checked against Atlassian's documentation and the published API
specification (`api.bitbucket.org/swagger.json`) in October 2026:

| Step | Request |
|---|---|
| Authorize | `GET https://bitbucket.org/site/oauth2/authorize?client_id&response_type=code&state&redirect_uri` |
| Token | `POST https://bitbucket.org/site/oauth2/access_token`, HTTP Basic `client_id:secret`, form `grant_type=authorization_code&code&redirect_uri` |
| User | `GET https://api.bitbucket.org/2.0/user` (scope `account`) → `uuid`, `account_id`, `display_name`, `nickname` |
| Membership | `GET /2.0/user/workspaces/{workspace}/permission` (scope `account`) → `200` the caller's role in the workspace, `403` no access |
| Email | `GET /2.0/user/emails?pagelen=100` (scope `email`) → `values[]` with `email`, `is_primary`, `is_confirmed` |

- Scopes are fixed by the consumer's permissions; the authorization URL
  sends none. The consumer needs **Account: Read** and **Account: Email**.
- Bitbucket Cloud accepts but does not enforce PKCE (a wrong verifier is
  accepted), so the provider sends no challenge rather than suggesting a
  protection that is not there; `state` and the confidential client secret
  carry the security. The trait still passes a challenge and verifier, for
  providers that honour them.
- `redirect_uri` is sent on both requests: Bitbucket rejects a token
  request whose `redirect_uri` differs from the authorization request's.
- `GET /2.0/user/permissions/workspaces` and `/2.0/workspaces` are gone from
  the specification (Atlassian's cross-workspace API removal); the
  per-workspace permission endpoint is the replacement. `403` and `404`
  both mean "not a member"; any other failure is a server error, never an
  admission.
- Access tokens last one to two hours; they are not kept.
- The subject is `uuid` (stable, never reused); `nickname` is not unique
  and is only a display hint.

## 8. Testing

- Unit: cookie sealing (tamper, other key, expiry, malformed), PKCE against
  RFC 7636 appendix B, redirect validation (open-redirect corpus), the
  expiring map (sweep, eviction, compaction), handoff single use and TTL on
  a `ManualClock`, config validation naming keys.
- `tests/flow.rs`: the routes against an in-process fake provider on a
  `ManualClock`: happy path with PKCE checked, missing / tampered / expired
  / foreign-provider cookie, state mismatch, replay, provider errors, hook
  denial, full store, settings validation.
- `tests/bitbucket.rs`: a fake Bitbucket on `127.0.0.1:0` (real clock, 30 s
  guards): end to end including the exact token form and Basic credentials,
  unconfirmed email ignored, non-member `403`/`404`, membership `500`,
  token `400` without upstream text, a hanging token endpoint against a
  300 ms timeout, an unreachable host, and the routes behind
  `Server::prefix("/api")`.
