# Authentication (`sekvent::auth`)

`sekvent::auth` covers end-user authentication inside a service: password
hashing (argon2id by default, bcrypt for stores shared with applications
that only read bcrypt), a login check that does not reveal whether an
account exists, JSON Web Tokens verified against a time you pass in (HS256
with a shared secret, or EdDSA over Ed25519, signed with a private key and
verified with public keys), role checks, and bearer-token guards for axum
and tonic. The core is pure:
it spawns nothing, reads no environment and never asks the system for the
time, so the same code runs on servers, in tests with a frozen clock and in
the browser (`wasm32-unknown-unknown`).

Service-to-service authentication is a different module: see
[link](link.md).

## Enable it

| | |
|---|---|
| Facade feature | `auth` |
| Module | `use sekvent::auth::…;` |
| Internal crate | `sekvent-auth` |

| Facade feature | `sekvent-auth` feature | Adds | wasm |
|---|---|---|---|
| `auth` | — | `PasswordHasher`, `authenticate`, `JwtKeys`, `EdDsaSigningKey`, `EdDsaVerificationKeys`, `Claims`, `Validation`, `HasRoles`, `require_any_role` | yes, with the facade's default features off ([below](#build-for-the-browser)) |
| `auth-axum` | `axum` | `BearerAuth` (with `verify_headers` and `end_user`), `EndUserClaims`, `HasRoles` for `EndUser`, `sekvent::auth::axum::{Bearer, RequireRole, RoleSet}` | no |
| `auth-tonic` | `tonic` | `BearerAuth` (with `verify_headers` and `end_user`), `EndUserClaims`, `HasRoles` for `EndUser`, `sekvent::auth::tonic::{BearerInterceptor, verified_claims, require_any_role}` | no |
| `auth-tokio` | `tokio` | `PasswordHasher::hash_async`, `verify_async`, `authenticate_async` | no |

```toml
[dependencies]
sekvent-api = { git = "https://github.com/westito/sekvent-api", branch = "master", features = [
    "auth-axum",
    "auth-tokio",
] }
```

## Quick example

```rust
use sekvent::auth::{Claims, JwtKeys, NoClaims, PasswordHasher, Validation, authenticate};
use sekvent::context::Clock;
use sekvent::error::AppError;

let hasher = PasswordHasher::default();                       // argon2id, OWASP parameters
let keys = JwtKeys::hs256("k1", &config.jwt_secret)?;         // a Secret of at least 32 bytes

// Login: one password verification whether or not the account exists.
let account = repo.find_login(&email).await?;                 // Option<Account>
let lookup = account.as_ref().map(|a| (a.user_id.clone(), a.password_hash.as_str(), a.enabled));
let ok = authenticate(&hasher, lookup, &password).map_err(AppError::from)?;
if ok.needs_rehash {
    repo.set_password_hash(&ok.user, &hasher.hash(&password)?).await?;
}

// Session token, with the time from an injected Clock.
let now = clock.now_unix_millis() / 1000;
let token = keys.issue(&Claims::new(ok.user, now, 3600, NoClaims {}))?;
let claims: Claims<NoClaims> = keys.verify(&token, now, &Validation::new())?;
```

`keys.verify` returns `Result<_, TokenRejected>`; `?` into an `AppError`
works because `From<TokenRejected> for AppError` exists.

## Concepts

- **Scheme.** A `PasswordHasher` writes new hashes with one
  `PasswordScheme` and verifies argon2 and bcrypt hashes under either
  scheme. A match against a hash that is not *current* for the configured
  scheme asks for a rehash, so a password store converges on one format as
  users log in.
- **Uniform work.** Every `verify` performs exactly one expensive
  verification. A stored value that cannot be checked (malformed, over the
  cost limits, an empty hash for a single-sign-on-only account) costs a
  dummy verification instead, so it takes as long to reject as a wrong
  password. `authenticate` extends this to unknown accounts.
- **Two algorithms.** HS256 (`JwtKeys`): one shared secret signs and
  verifies, so every verifier could also mint tokens. EdDSA over Ed25519
  (`EdDsaSigningKey`, `EdDsaVerificationKeys`): the issuer alone holds the
  private key, verifiers hold public keys and cannot mint. Both share
  `Claims`, `Validation` and `TokenRejected`, and each verifier accepts its
  own algorithm only.
- **Injected time.** `jsonwebtoken`'s own expiry checks read the system
  clock, so they are off: `JwtKeys::verify` and
  `EdDsaVerificationKeys::verify` check the signature through the library,
  then `exp`, `nbf` and `iat` themselves against the `now_unix_secs` they
  are given. The axum and tonic adapters read the time from an injected
  `sekvent::context::Clock`.
- **One message for every rejection.** A refused token is always
  `UNAUTHENTICATED` with `invalid or expired credentials`; which check
  failed is for server logs only. Unknown account and wrong password share
  `invalid username or password`.

## How to …

### Hash and verify passwords

```rust
impl PasswordHasher {
    pub fn new(params: PasswordParams) -> Result<Self, AppError>;      // argon2id
    pub fn bcrypt(params: BcryptParams) -> Result<Self, AppError>;
    pub fn with_scheme(scheme: PasswordScheme) -> Result<Self, AppError>;
    pub fn scheme(&self) -> PasswordScheme;
    pub fn hash(&self, password: &str) -> Result<String, AppError>;
    pub fn verify(&self, password: &str, stored: &str) -> Verification;
    pub fn dummy_verify(&self, password: &str);
}
impl Default for PasswordHasher { /* argon2id with PasswordParams::OWASP */ }
```

Build one hasher at startup and share it (it is `Clone` and holds no
secrets). `hash` uses a fresh random 16-byte salt from `getrandom`.

`Verification` is `Valid`, `ValidNeedsRehash` or `Invalid`;
`is_valid()` is true for the first two, `needs_rehash()` for the second.
On `ValidNeedsRehash`, store `hasher.hash(password)` of the password just
checked.

**argon2id.** `PasswordParams { memory_kib, iterations, parallelism }`;
`PasswordParams::OWASP` (the default) is 19 MiB, 2 iterations,
parallelism 1. `PasswordHasher::new` fails with `INVALID_ARGUMENT` when
argon2 rejects the parameters or they exceed `PasswordParams::VERIFY_LIMIT`
(1 GiB, 16 iterations, 16 lanes); `params.within_verify_limit()` checks
that. New hashes are PHC strings `$argon2id$v=19$m=…,t=…,p=…$…` with a
32-byte output.

**bcrypt.** For a store shared with applications that read bcrypt (for
example `{bcrypt}$2b$10$…` from a delegating password encoder):

```rust
use sekvent::auth::{BcryptParams, BcryptVersion, PasswordHasher, PasswordScheme};

let hasher = PasswordHasher::bcrypt(BcryptParams::new(10).prefixed())?;   // `{bcrypt}$2b$10$…`
let legacy = PasswordHasher::with_scheme(PasswordScheme::Bcrypt(
    BcryptParams::new(12).version(BcryptVersion::TwoA),                   // `$2a$12$…`
))?;
```

`BcryptParams { cost, version, prefixed }`: `BcryptParams::new(cost)` is
`$2b$` without prefix; `.prefixed()` writes the `{bcrypt}` prefix;
`.version(BcryptVersion::TwoA | TwoB | TwoY)` picks the tag. The cost must
be 4 to `MAX_BCRYPT_COST` (14), else `INVALID_ARGUMENT`.

What verifies as what (password matches):

| Stored value | argon2id scheme | bcrypt scheme |
|---|---|---|
| `$argon2id$v=19$…` at or above the configured cost | `Valid` | `ValidNeedsRehash` |
| `$argon2id$…` below the configured cost, `$argon2i$…`, `$argon2d$…` | `ValidNeedsRehash` | `ValidNeedsRehash` |
| `{argon2}$argon2…` (delegating-encoder prefix) | `ValidNeedsRehash` | `ValidNeedsRehash` |
| `$2a$`/`$2b$`/`$2y$` or `{bcrypt}$2…` in the configured form (prefix or not) at or above the configured cost | `ValidNeedsRehash` | `Valid` |
| bcrypt in the other form, or below the configured cost | `ValidNeedsRehash` | `ValidNeedsRehash` |
| bcrypt in any form with a password longer than 72 bytes | `ValidNeedsRehash` | `Valid` |

The version tag (`2a`, `2b`, `2y`) never asks for a rehash. Anything else
— unknown prefixes, malformed strings, `$2x$`, an empty value, an argon2
cost above `VERIFY_LIMIT`, a bcrypt cost above 14 — is `Invalid`, after a
dummy verification, and is logged at `warn` without the value. Nothing
panics on a bad stored value.

**The 72-byte rule.** bcrypt reads only the first
`BCRYPT_MAX_PASSWORD_BYTES` (72) bytes. Under the bcrypt scheme, `hash`
refuses a longer password with `INVALID_ARGUMENT` / reason
`PASSWORD_TOO_LONG` instead of truncating it silently, so enforce the limit
in sign-up and change-password forms. `verify` does check a longer password
on its first 72 bytes, as the implementations that wrote existing hashes
did; under argon2id that match asks for a rehash (the whole password then
ends up in an argon2id hash), under bcrypt it does not.

**dummy_verify.** Spends the cost of verifying a current hash of the
configured scheme and discards the result. Call it when there is no stored
hash to check, or use `authenticate`, which does.

### Check a login

```rust
pub type LoginOutcome<U> = Result<Authenticated<U>, LoginRejected>;
pub struct Authenticated<U> { pub user: U, pub needs_rehash: bool }

pub fn authenticate<U>(hasher: &PasswordHasher, lookup: Option<(U, &str, bool)>, password: &str) -> LoginOutcome<U>;
pub async fn authenticate_async<U: Send + 'static>(       // feature auth-tokio
    hasher: &PasswordHasher,
    lookup: Option<(U, String, bool)>,
    password: Secret,
) -> LoginOutcome<U>;
```

`lookup` is `None` for an unknown account, otherwise the account, its stored
hash (empty when it has none) and whether it is enabled. Exactly one
password verification runs in every case. A disabled account is reported
only after its password has been proven, so the disabled state cannot be
probed either.

| `LoginRejected` | `error_code()` | `reason()` | `user_message()` |
|---|---|---|---|
| `InvalidCredentials` (unknown account or wrong password) | `UNAUTHENTICATED` | `INVALID_CREDENTIALS` | `invalid username or password` |
| `Disabled` | `PERMISSION_DENIED` | `ACCOUNT_DISABLED` | `this account is disabled` |
| `Locked` | `PERMISSION_DENIED` | `ACCOUNT_LOCKED` | `this account is locked` |

`authenticate` never returns `Locked`: lockout policy (attempt counters,
cool-downs) belongs to the caller, which can return
`AppError::from(LoginRejected::Locked)` itself. `From<LoginRejected> for
AppError` sets code, message and reason.

`authenticate_async` runs the same check on tokio's blocking pool (the
stored hash moves by value). A blocking task that panics or is cancelled is
logged and reported as `InvalidCredentials`.

### Hash off the async workers

With `auth-tokio`:

```rust
let password = Secret::new(body.password);
let lookup = account.map(|a| (a.user_id, a.password_hash, a.enabled));   // Option<(U, String, bool)>
let ok = authenticate_async(&hasher, lookup, password.clone()).await.map_err(AppError::from)?;
if ok.needs_rehash {
    repo.set_password_hash(&ok.user, &hasher.hash_async(password).await?).await?;
}
```

- `hasher.hash_async(password: Secret) -> Result<String, AppError>`; a
  failed blocking task is `INTERNAL` (`password task panicked` or
  `… cancelled`; the panic payload is never logged).
- `hasher.verify_async(password: Secret, stored: String) -> Verification`;
  `Invalid` if the task fails.

argon2id at the OWASP parameters takes tens of milliseconds of CPU; on the
async workers it stalls every other request on that thread.

### Issue and verify JWTs

```rust
impl JwtKeys {
    pub fn hs256(kid: impl Into<String>, secret: &Secret) -> Result<Self, AppError>;
    pub fn with_verification_key(self, kid: impl Into<String>, secret: &Secret) -> Result<Self, AppError>;
    pub fn development() -> Self;
    pub fn signing_kid(&self) -> &str;
    pub fn verification_kids(&self) -> impl Iterator<Item = &str>;
    pub fn issue<T: Serialize>(&self, claims: &Claims<T>) -> Result<String, AppError>;
    pub fn verify<T: DeserializeOwned>(&self, token: &str, now_unix_secs: u64, validation: &Validation)
        -> Result<Claims<T>, TokenRejected>;
}
```

- **Algorithm.** HS256 only. A token whose header names another known JWS
  algorithm (`HS384`, `RS256`, …) is refused as `BadSignature`; a header
  with `alg: none` or an algorithm name the decoder does not know is
  `Malformed`.
- **Key ids.** Every issued token carries a `kid` header naming the signing
  key. Verification is strict: a token without `kid`, or with a `kid` no
  verification key has, is `UnknownKey`.
- **Secrets.** At least `MIN_SECRET_LEN` (32) bytes, and a non-blank `kid`;
  otherwise `INVALID_ARGUMENT`. Errors name the key id, never the secret.
  `JwtKeys`'s `Debug` shows only the key ids.
- **Rotation.** `with_verification_key` adds a key that is accepted but not
  used for signing (a duplicate `kid` is `INVALID_ARGUMENT`). Rotate by
  adding the new key for verification everywhere, switching signing to it,
  and dropping the old one once its last token has expired.

```rust
let keys = JwtKeys::hs256("2026-10", &config.jwt_secret)?
    .with_verification_key("2026-07", &config.jwt_previous_secret)?;
```

`Claims<T>` holds the registered claims plus your own, flattened into the
same JSON object:

```rust
pub struct Claims<T = NoClaims> {
    pub sub: String, pub iat: u64, pub exp: u64,
    pub nbf: Option<u64>, pub iss: Option<String>, pub aud: Option<String>, pub jti: Option<String>,
    pub custom: T,
}
Claims::new(sub, now_unix_secs, ttl_secs, custom)   // iat = now, exp = now + ttl
    .with_not_before(nbf)
    .with_issuer("orders")
    .with_audience("web")
    .with_jti(token_id);
```

Custom field names must not collide with the registered ones. `aud` is a
single string; a token whose `aud` is an array is `Malformed`. Use
`NoClaims {}` when there are none.

`Validation` (non-exhaustive, `Validation::new()` or `default()`) sets
`leeway_secs` (default `DEFAULT_LEEWAY_SECS` = 30), and optionally a
required issuer and audience:

```rust
let validation = Validation::new()
    .with_leeway(5)
    .with_issuer("orders")
    .with_audience("web");
```

`verify` checks, in order: header and `kid`, signature, then times (`exp`
passes while `now < exp + leeway`; `nbf` and `iat` pass while they are at
most `now + leeway`), then issuer and audience when required.

| `TokenRejected` | Meaning |
|---|---|
| `Malformed` | not a well-formed JWT (including `alg: none` and unknown algorithm names), or claims that do not deserialize |
| `BadSignature` | the signature does not verify, or the header names a known algorithm other than HS256 |
| `Expired` | `exp` has passed |
| `NotYetValid` | `nbf` or `iat` is in the future |
| `WrongIssuer`, `WrongAudience` | does not match `Validation` |
| `UnknownKey` | `kid` missing or unknown |

Every variant has `error_code()` = `UNAUTHENTICATED` and the same
`user_message()`: `invalid or expired credentials`. Log the variant
(`Display`), return only the message.

### Sign with a private key, verify with public keys (EdDSA)

Choose EdDSA over HS256 when a verifier must not be able to mint tokens:
one service issues them, others only accept them. The issuer holds an
Ed25519 private key; each verifier holds the public key, which is not a
secret.

```rust
pub const ED25519_KEY_LEN: usize = 32;          // sekvent::auth::jwt::ED25519_KEY_LEN

impl EdDsaSigningKey {
    pub fn from_pkcs8_pem(kid: impl Into<String>, pem: &Secret) -> Result<Self, AppError>;
    pub fn from_seed(kid: impl Into<String>, seed: &[u8; ED25519_KEY_LEN]) -> Result<Self, AppError>;
    pub fn kid(&self) -> &str;
    pub fn public_key(&self) -> [u8; ED25519_KEY_LEN];
    pub fn public_key_pem(&self) -> String;
    pub fn verification_keys(&self) -> EdDsaVerificationKeys;
    pub fn issue<T: Serialize>(&self, claims: &Claims<T>) -> Result<String, AppError>;
}

impl EdDsaVerificationKeys {
    pub fn from_public_key_pem(kid: impl Into<String>, pem: &str) -> Result<Self, AppError>;
    pub fn from_public_key(kid: impl Into<String>, key: &[u8; ED25519_KEY_LEN]) -> Result<Self, AppError>;
    pub fn with_public_key_pem(self, kid: impl Into<String>, pem: &str) -> Result<Self, AppError>;
    pub fn with_public_key(self, kid: impl Into<String>, key: &[u8; ED25519_KEY_LEN]) -> Result<Self, AppError>;
    pub fn kids(&self) -> impl Iterator<Item = &str>;
    pub fn verify<T: DeserializeOwned>(&self, token: &str, now_unix_secs: u64, validation: &Validation)
        -> Result<Claims<T>, TokenRejected>;
}
```

```sh
openssl genpkey -algorithm ed25519 -out signing.pem    # private key, PKCS#8 PEM: the issuer's Secret
openssl pkey -in signing.pem -pubout -out public.pem   # public key, SubjectPublicKeyInfo PEM: for verifiers
```

```rust
use sekvent::auth::{Claims, EdDsaSigningKey, EdDsaVerificationKeys, NoClaims, Validation};

// The issuer.
let signing = EdDsaSigningKey::from_pkcs8_pem("2026-11", &config.signing_key)?;
let now = clock.now_unix_millis() / 1000;
let token = signing.issue(
    &Claims::new("billing", now, 60, NoClaims {}).with_issuer("billing").with_audience("orders"),
)?;

// A verifier: public keys by key id, two during a rotation.
let keys = EdDsaVerificationKeys::from_public_key_pem("2026-11", &config.billing_key)?
    .with_public_key_pem("2026-05", &config.billing_previous_key)?;
let rules = Validation::new().with_issuer("billing").with_audience("orders").with_leeway(0);
let claims: Claims<NoClaims> = keys.verify(&token, now, &rules)?;
```

Accepted inputs:

| Input | Accepted |
|---|---|
| private key | the PKCS#8 PEM `openssl genpkey -algorithm ed25519` writes, in a `Secret`; or its base64 body alone on one line |
| public key | the SubjectPublicKeyInfo PEM `openssl pkey -pubout` writes, as plain text; or its base64 body alone; or the raw 32 bytes (`from_public_key`) |
| seed | 32 bytes (`from_seed`), for tests and for a key you derive |

A PEM must have the expected label lines (`PRIVATE KEY`, `PUBLIC KEY`);
line ends may be `\n` or `\r\n`, whitespace inside the body is ignored, and
the body is standard base64 with its padding. The private key must be the
48-byte Ed25519 PKCS#8 v1 form; an encrypted key, an OpenSSH key, an RSA
key, a PKCS#8 key that also carries its public half and base64url text are
refused. sekvent frames the keys itself in fixed buffers, without
allocating (an oversize body is refused), and wipes the key text and bytes
it handles, public ones too, since a private key may be pasted where a
public one belongs; a private key given to a verifier is refused.
`signing.public_key_pem()` prints the PEM to hand to
verifiers, the same text `openssl pkey -pubout` writes.

Keys are checked when they are loaded, so a bad one is a startup error:
a public key that is not canonical, not a point of the curve or of small
order is refused (see [Errors and reasons](#errors-and-reasons)).

- **Key ids.** Every issued token carries `alg: EdDSA` and the signing
  key's `kid`. A token without `kid`, or with a `kid` no public key has,
  is `UnknownKey`. A blank or duplicate `kid` is `INVALID_ARGUMENT`.
- **Rotation.** Generate a new key pair, add its public key under a new
  `kid` to every verifier (`with_public_key_pem`), switch the issuer to the
  new private key, and drop the old public key once its last token has
  expired.
- **No empty verifier.** The first key comes with the constructor; a
  verifier has no way to sign. `Debug` of both types shows key ids only.

`verify` checks in the same order and answers like `JwtKeys::verify`:

| Step | Refusal |
|---|---|
| the header does not parse, or names an algorithm the library does not know (`none`) | `Malformed` |
| the header's `alg` is not `EdDSA` (an HS256 token, any other known algorithm) | `BadSignature` |
| no `kid`, or a `kid` that names no key | `UnknownKey` |
| the signature does not verify with that key | `BadSignature` |
| the claims do not deserialize | `Malformed` |
| `exp`, `nbf`, `iat` against `now_unix_secs` and the leeway | `Expired`, `NotYetValid` |
| `Validation::issuer`, `Validation::audience` when set | `WrongIssuer`, `WrongAudience` |

The algorithm is pinned both ways: an HS256 token never passes an EdDSA
verifier (not even one signed with the public key as the HMAC secret), and
`JwtKeys::verify` refuses an EdDSA token.

### Use the development key safely

`JwtKeys::development()` builds keys from the published
`DEVELOPMENT_SECRET` under kid `development` (`jwt::DEVELOPMENT_KID`) and
logs a loud warning every time it is called. The secret is in the source of
a public crate: anyone can forge tokens it accepts. Use it only for local
development and tests; production configuration must fail at startup when
the real secret is missing, never fall back to it.

### Check roles

```rust
pub trait HasRoles { fn has_role(&self, role: &str) -> bool; }
pub fn require_any_role<C: HasRoles + ?Sized>(claims: &C, roles: &[&str]) -> Result<(), AppError>;
```

Implement `HasRoles` on your custom claims; `Claims<T>` delegates to `T`,
and `[String]` and `Vec<String>` implement it:

```rust
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct Profile { roles: Vec<String> }

impl HasRoles for Profile {
    fn has_role(&self, role: &str) -> bool { self.roles.has_role(role) }
}

require_any_role(&claims, &["admin", "billing"])?;   // PERMISSION_DENIED, "insufficient permissions"
```

It fails closed: an empty `roles` list is always denied. The error does not
name the roles.

### Guard axum handlers

With `auth-axum`. `BearerAuth` bundles keys, validation rules and a clock;
make it reachable from the router state through `FromRef`:

```rust
use std::sync::Arc;
use axum::{Router, routing::get};
use sekvent::auth::axum::{Bearer, RequireRole, RoleSet};
use sekvent::auth::{BearerAuth, Validation};
use sekvent::context::SystemClock;

let auth = BearerAuth::new(keys, Validation::new(), Arc::new(SystemClock));
let app = Router::new()
    .route("/me", get(me))
    .route("/purge", get(purge))
    .with_state(auth);

async fn me(Bearer(claims): Bearer<Profile>) -> String {
    claims.sub
}

struct Admins;
impl RoleSet for Admins {
    const ROLES: &'static [&'static str] = &["admin"];
}

async fn purge(guard: RequireRole<Admins, Profile>) -> String {
    guard.into_claims().sub
}
```

- `Bearer<T = NoClaims>(pub Claims<T>)` reads `Authorization: Bearer <jwt>`
  (scheme case-insensitive, token taken verbatim) and verifies it at the
  clock's current time.
- `RequireRole<R: RoleSet, T>` additionally demands one of `R::ROLES` (an
  empty set admits nobody); `.claims` or `.into_claims()` gives the claims.
- Rejections are `AppError`s: `UNAUTHENTICATED` for a missing or invalid
  token (one message for all), `PERMISSION_DENIED` for a missing role. The
  failure detail is logged at `debug`.

`BearerAuth` also has `keys()` (to issue tokens from the same server),
`now_unix_secs()`, `verify::<T>(token)` and `verify_headers::<T>(headers)`
([below](#authenticate-end-users-of-components)). It is cheap to clone, and
its `Debug` shows no secret.

### Guard tonic services

With `auth-tonic`:

```rust
use sekvent::auth::tonic::{BearerInterceptor, require_any_role, verified_claims};

let auth = BearerAuth::new(keys, Validation::new(), Arc::new(SystemClock));
let service = OrdersServer::with_interceptor(orders, BearerInterceptor::<Profile>::new(auth));

async fn cancel(&self, request: Request<CancelOrder>) -> Result<Response<()>, Status> {
    let claims = require_any_role::<Profile, _>(&request, &["admin"])?;
    // …
}
```

`BearerInterceptor<T>` verifies the `authorization` metadata of every
request and stores the `Claims<T>` in the request extensions; requests
without a valid token fail with `UNAUTHENTICATED` before the handler runs.
In the handler, `verified_claims::<T, _>(&request)` returns the claims and
`require_any_role::<T, _>(&request, roles)` also checks roles. Both fail
closed with `UNAUTHENTICATED` when no claims are present (the interceptor
was not installed on this service). `T` must be the same type the
interceptor was built with.

### Authenticate end users of components

With `auth-axum` or `auth-tonic`. A component served with
`SEKVENT_COMPONENT_<C>_SERVE_AUTH=bearer` (or `link,bearer`) authenticates
its end users with the App's end-user authenticator; `BearerAuth` is a
ready-made one for JWTs. The full setup is in
[Components: serve to end users](components.md#how-to-serve-components-to-end-users-browsers-apps).

```rust
impl BearerAuth {
    pub fn verify_headers<T: DeserializeOwned>(&self, headers: &http::HeaderMap)
        -> Result<Claims<T>, AppError>;
    pub fn end_user<T: DeserializeOwned + EndUserClaims>(&self, headers: &http::HeaderMap)
        -> Result<EndUser, AppError>;
}

pub trait EndUserClaims {
    fn tenant(&self) -> Option<&str> { None }
    fn roles(&self) -> &[String] { &[] }
}
impl EndUserClaims for NoClaims {}
impl HasRoles for EndUser { /* delegates to EndUser::has_role */ }
```

- `verify_headers` reads `Authorization: Bearer <jwt>` from a header map and
  verifies it at the clock's current time, like the axum and tonic guards.
- `end_user` does the same and builds a `sekvent::context::EndUser`: `sub`
  is the subject, tenant and roles come from your custom claims through
  `EndUserClaims`. With `NoClaims` the end user has a subject only.
- Every rejection is `UNAUTHENTICATED` with `invalid or expired
  credentials`, so the component's own answer stays the same for a
  missing, malformed, expired or unknown token.

```rust
use std::sync::Arc;
use sekvent::auth::{BearerAuth, EndUserClaims, Validation};
use sekvent::context::SystemClock;

#[derive(Debug, serde::Deserialize)]
struct Profile { tenant: String, roles: Vec<String> }

impl EndUserClaims for Profile {
    fn tenant(&self) -> Option<&str> { Some(&self.tenant) }
    fn roles(&self) -> &[String] { &self.roles }
}

let auth = BearerAuth::new(keys, Validation::new().with_audience("web"), Arc::new(SystemClock));
builder.end_user_authenticator(move |request: &http::request::Parts| {
    auth.end_user::<Profile>(&request.headers)
})?;
```

In the component's handler, `EndUser` implements `HasRoles`, so the usual
role check works on it:

```rust
if let Some(user) = cx.end_user() {
    sekvent::auth::require_any_role(user, &["admin"])?;   // PERMISSION_DENIED, "insufficient permissions"
}
```

### Build for the browser

The core (`password`, `jwt`, `login`, `roles`) compiles for
`wasm32-unknown-unknown`. The facade's default features include `runtime`,
which pulls in tokio, axum and tonic and does not build there, so turn the
defaults off and enable only `auth`, or depend on `sekvent-auth` directly:

```toml
sekvent-api = { git = "https://github.com/westito/sekvent-api", branch = "master", default-features = false, features = ["auth"] }
# or
sekvent-auth = { git = "https://github.com/westito/sekvent-api", branch = "master" }
```

Add `"config"` and `"error"` to the facade features if you name `Secret` or
`AppError` through `sekvent::config` / `sekvent::error`; both are
wasm-compatible.

On wasm32 `SystemTime::now()` panics, which is why
every time-dependent call takes `now_unix_secs`. Salts come from
`getrandom`, which has no default backend there; enable it in the consuming
crate:

```toml
getrandom = { version = "0.4", features = ["wasm_js"] }
# jsonwebtoken still pulls getrandom 0.2 on wasm32:
getrandom_02 = { package = "getrandom", version = "0.2", features = ["js"] }
```

The `axum`, `tonic` and `tokio` features are not wasm-compatible.

## Configuration keys

None. The module reads no environment: load the JWT secret (a `Secret`),
key id, token lifetime and hashing parameters through your own config
struct (see [config](config.md)) and pass them in. For EdDSA, the issuer
reads its private key as a `Secret`; a verifier reads public keys as plain
text:

```rust
use sekvent::prelude::*;

#[derive(Debug, EnvConfig)]
#[config(prefix = "BILLING_")]
struct IssuerConfig {
    signing_key: Secret,       // BILLING_SIGNING_KEY: the PKCS#8 PEM or its base64 body
}

#[derive(Debug, EnvConfig)]
#[config(prefix = "ORDERS_")]
struct VerifierConfig {
    billing_key: String,       // ORDERS_BILLING_KEY: the public key PEM or its base64 body
}
```

## Errors and reasons

| Source | Code | Reason |
|---|---|---|
| `LoginRejected::InvalidCredentials` | `UNAUTHENTICATED` | `INVALID_CREDENTIALS` |
| `LoginRejected::Disabled` | `PERMISSION_DENIED` | `ACCOUNT_DISABLED` |
| `LoginRejected::Locked` | `PERMISSION_DENIED` | `ACCOUNT_LOCKED` |
| any `TokenRejected` | `UNAUTHENTICATED` | — |
| `BearerAuth::verify_headers` / `end_user`: missing, malformed or rejected token | `UNAUTHENTICATED` | — |
| `require_any_role` / `RequireRole` | `PERMISSION_DENIED` | — |
| bcrypt `hash` of a password over 72 bytes | `INVALID_ARGUMENT` | `PASSWORD_TOO_LONG` |
| bad hasher parameters, short JWT secret, blank or duplicate `kid` | `INVALID_ARGUMENT` | — |
| EdDSA private key not what [the loader accepts](#sign-with-a-private-key-verify-with-public-keys-eddsa): ``JWT key `<kid>` is not an Ed25519 private key in PKCS#8 PEM`` | `INVALID_ARGUMENT` | — |
| EdDSA public key not what the loader accepts, not canonical or not a point of the curve: ``JWT key `<kid>` is not an Ed25519 public key`` | `INVALID_ARGUMENT` | — |
| EdDSA public key of small order: ``JWT key `<kid>` is a weak Ed25519 public key`` | `INVALID_ARGUMENT` | — |
| a failed `*_async` blocking task | `INTERNAL` | — |

Key errors name the key id, never the key. A weak or non-canonical public
key is refused when it is loaded because the signature check underneath
does not refuse it, and a verifier holding one would accept forged tokens.

## Testing tips

- Use cheap parameters in tests: `PasswordHasher::new(PasswordParams {
  memory_kib: 8, iterations: 1, parallelism: 1 })?` or
  `PasswordHasher::bcrypt(BcryptParams::new(4))?`.
- Pass explicit times to `Claims::new` and `JwtKeys::verify`, and use
  `Validation::new().with_leeway(0)` to test expiry boundaries exactly.
- For the adapters, give `BearerAuth` a `sekvent::context::ManualClock`
  and move it with `clock.advance(..)` to expire a token without sleeping.
- Test JWT secrets must be 32 bytes or more; `JwtKeys::development()` is
  fine in tests (it logs a warning).
- For EdDSA, `EdDsaSigningKey::from_seed("k1", &[7; 32])?` and
  `.verification_keys()` give a matching, deterministic key pair without
  key files.
- `*_async` helpers need a tokio runtime: `#[tokio::test]`.

## Pitfalls and security

- **No early return before `authenticate`.** Looking up the account and
  returning on `None` before verifying a password reveals which accounts
  exist through timing. Pass `None` to `authenticate` instead.
- **Same answer for every failure.** Do not map `TokenRejected` variants or
  `InvalidCredentials` vs. unknown account to different caller messages.
- **Rehash on login.** Handle `needs_rehash`; otherwise legacy and
  low-cost hashes stay forever, and their verification time differs from
  the dummy's.
- **Enforce 72 bytes under bcrypt** in the forms that set passwords.
- **Never the development secret in production.** A missing JWT secret is
  a startup error.
- **Never log tokens, passwords or hashes.** Keep passwords in `Secret`
  where possible (`*_async` take `Secret`).
- **Do not trust `aud`/`iss` unless you ask.** `Validation::new()` accepts
  any issuer and audience; set them when tokens are shared across services.
- **The EdDSA private key only in a `Secret`**, and only at the issuer.
  A verifier's configuration holds public keys, never a private key.
- **A token that must work once.** EdDSA tokens are bearer tokens: give
  them short lifetimes, and when one must be accepted only once, set a
  `jti` and have the receiver record the ones it has seen.
- **`BearerAuth` is HS256 only.** Verify EdDSA tokens with
  `EdDsaVerificationKeys::verify`, for example in your own end-user
  authenticator.

## See also

- [Service links](link.md) — service-to-service tokens
- [Context](context.md) — `Clock`, `SystemClock`, `ManualClock`
- [Errors](error.md) — `AppError` and the HTTP/gRPC mappings
- [Server](server.md) — mounting axum routers and tonic services
- [Components](components.md#how-to-serve-components-to-end-users-browsers-apps) — serving components to end users with `BearerAuth::end_user`
- [Design: components served to end users](../design/component-end-user.md)
- [Design: P8 service essentials](../design/p8-service-essentials.md) — bcrypt and async helpers
- [Design: building blocks for multi-tenant services](../design/multi-tenant-building-blocks.md) — EdDSA tokens
