# Components served to end users

> Status: implemented. Builds on [`component-c2.md`](component-c2.md)
> (gRPC serving, link authentication) and
> [`p8-service-essentials.md`](p8-service-essentials.md) (server, CORS,
> gRPC-Web). Where this note is more specific, it wins for serving to end
> users; everything else in C2 stays as it is.

## 1. Problem

C2 serves a component over gRPC to other services only: an exposed
component accepts the process's inbound link tokens
(`SEKVENT_COMPONENT_<C>_SERVE_AUTH=link`) or nothing at all (`none`). A
browser (gRPC-Web) or a native app cannot hold a link token, so an
application that wants its frontend to call its components has to write a
hand-made tonic service per RPC that checks a session token and delegates
to the handle. That duplicates the contract, the error mapping and the
gate, and it is easy to get the order of checks wrong.

This note adds a third way to authenticate a served component's callers:
an **end-user authenticator** registered on the App, chosen per component by
configuration, with a per-method opt-out for the few public RPCs (sign-in).

## 2. Scope

### In

- `SERVE_AUTH` accepts `bearer` and the combination `link,bearer`.
- `AppBuilder::end_user_authenticator` registers one authenticator for the
  App; sync closures and async implementations are both supported.
- `EndUser` (in `sekvent-context`) and `CallContext::end_user`: handlers
  tell an end user from a peer service and read subject, tenant and roles.
- `#[call(anonymous)]` and `MethodDescriptor::with_anonymous`.
- A ready-made JWT adapter on `sekvent-auth`'s `BearerAuth`
  (`verify_headers`, `end_user`, `EndUserClaims`) and `HasRoles` for
  `EndUser`.
- gRPC-Web reachability of `App::grpc_routes()` through the runtime server,
  under a prefix, with CORS preflights (already wired by P8; pinned by a
  loopback test).

### Out (later, or never)

| Item | Note |
|---|---|
| Per-component authenticators | one authenticator per App; it can look at the request path if components need different rules |
| Optional authentication on anonymous methods | an anonymous method never sees an end user, even when the request carries a valid token (decision 4) |
| Propagating roles to further components | roles stay with the component that authenticated the end user (decision 3) |
| `SEKVENT_COMPONENT_<C>_MAX_MESSAGE_SIZE` | the 4 MiB request cap stays fixed |
| Cookies, CSRF | an authenticator may read cookies from the request head; CSRF protection is the application's (gRPC-Web requests need a custom content type, which a plain form cannot send) |

## 3. Public API

### 3.1 `sekvent-context`

```rust
/// An end user authenticated at a serving boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EndUser { /* private */ }

impl EndUser {
    pub fn new(subject: impl Into<String>) -> Self;
    #[must_use] pub fn with_tenant(self, tenant: impl Into<String>) -> Self;
    #[must_use] pub fn with_roles<I, S>(self, roles: I) -> Self
    where I: IntoIterator<Item = S>, S: Into<String>;
    pub fn subject(&self) -> &str;
    pub fn tenant(&self) -> Option<&str>;
    pub fn roles(&self) -> &[String];
    pub fn has_role(&self, role: &str) -> bool;
}

impl CallContext {
    /// The end user is the direct caller: sets subject and tenant from it
    /// and clears `caller`.
    #[must_use] pub fn with_end_user(self, user: EndUser) -> Self;
    /// The authenticated end user, when an end user made this call directly.
    pub fn end_user(&self) -> Option<&EndUser>;
}
```

- The direct caller is either a service (`caller()`) or an end user
  (`end_user()`), never both: `with_end_user` clears the caller and
  `with_caller` clears the end user.
- `child()` and `detached()` drop the end user, as they drop the caller.
  Subject and tenant stay, so a component the handler calls sees them
  exactly as a trusted link would assert them.
- `sanitize_for_caller` keeps subject and tenant when an end user is set.
- The end user is never encoded in headers; roles do not leave the
  component that authenticated the end user.

### 3.2 `sekvent-component`

```rust
pub use sekvent_context::EndUser;

/// Message of every end-user authentication failure.
pub const END_USER_REJECTED_MESSAGE: &str = "invalid or expired credentials";

/// Authenticates the end users of components served with
/// `SERVE_AUTH=bearer` or `link,bearer`.
pub trait EndUserAuthenticator: Send + Sync + 'static {
    fn authenticate<'a>(
        &'a self,
        request: &'a http::request::Parts,
    ) -> Pin<Box<dyn Future<Output = Result<EndUser, AppError>> + Send + 'a>>;
}

/// Any sync closure is an authenticator.
impl<F> EndUserAuthenticator for F
where F: Fn(&http::request::Parts) -> Result<EndUser, AppError> + Send + Sync + 'static;

impl AppBuilder<'_> {
    /// Register the App's end-user authenticator; a second one is
    /// `BuildError::DuplicateEndUserAuthenticator`.
    pub fn end_user_authenticator(
        &mut self,
        authenticator: impl EndUserAuthenticator,
    ) -> Result<(), BuildError>;
}

impl MethodDescriptor {
    #[must_use] pub const fn with_anonymous(self) -> Self;
    pub const fn is_anonymous(&self) -> bool;
}

// BuildError gains:
#[error("component {component} is served with end-user authentication ({key}), \
         but no end-user authenticator is registered; call AppBuilder::end_user_authenticator")]
EndUserAuthenticatorMissing { component: String, key: String },
#[error("an end-user authenticator is registered twice")]
DuplicateEndUserAuthenticator,
```

The authenticator sees the request **head** only (method, URI, headers,
extensions); the body has not been read. It is available without the
`grpc` feature, so application code compiles the same way under every
feature set.

The authenticator's error decides the answer:

| Authenticator returns | The caller gets |
|---|---|
| `Ok(user)` | the call runs with `cx.end_user() == Some(&user)` |
| `Err(e)` with code `UNAUTHENTICATED` | `UNAUTHENTICATED`, message `END_USER_REJECTED_MESSAGE`, no reason, no metadata — the same answer for a missing, malformed, expired or unknown token |
| `Err(e)` with any other code | `e` unchanged (for example `UNAVAILABLE` when a session store is down, so a client retries instead of signing out) |

Nothing about the token is logged by the framework.

### 3.3 `sekvent-auth` (features `axum` or `tonic`)

```rust
impl BearerAuth {
    /// Verify the `authorization` header of `headers`.
    pub fn verify_headers<T: DeserializeOwned>(&self, headers: &http::HeaderMap)
        -> Result<Claims<T>, AppError>;
    /// The end user a valid token names: `sub` is the subject, tenant and
    /// roles come from the custom claims.
    pub fn end_user<T: DeserializeOwned + EndUserClaims>(&self, headers: &http::HeaderMap)
        -> Result<EndUser, AppError>;
}

/// Custom claims that name an end user's tenant and roles.
pub trait EndUserClaims {
    fn tenant(&self) -> Option<&str> { None }
    fn roles(&self) -> &[String] { &[] }
}
impl EndUserClaims for NoClaims {}
impl HasRoles for EndUser {
    fn has_role(&self, role: &str) -> bool { EndUser::has_role(self, role) }
}
```

Every rejection is `UNAUTHENTICATED` with the module's single message, so
the component's own normalisation (3.2) keeps the answer constant.

```rust
let auth = BearerAuth::new(keys, Validation::new().with_audience("web"), Arc::new(SystemClock));
builder.end_user_authenticator(move |request: &http::request::Parts| {
    auth.end_user::<Profile>(&request.headers)
})?;
```

### 3.4 The macro

`#[call(anonymous)]` marks a method that end users may call without
credentials. It combines with the other options
(`#[call(anonymous, timeout = "2s")]`), expands to `.with_anonymous()` on
the method descriptor, and is a compile error on a `local_only` component
(which is never served): `` `anonymous` has no effect on a local_only
component, which is never served; remove it``.

## 4. Configuration

| `SEKVENT_COMPONENT_<C>_SERVE_AUTH` | Who may call a non-anonymous method | Anonymous methods |
|---|---|---|
| `link` (default) | a peer with an inbound link token | same: a link token is required |
| `bearer` | an end user the authenticator accepts | anyone; no identity |
| `link,bearer` (or `bearer,link`) | a peer with an inbound link token, else an end user the authenticator accepts | a peer with a valid link token keeps its identity; anyone else is served without one |
| `none` | anyone (logged once at `warn`) | anyone |

Spellings are exact; anything else is a malformed key. The key set is
unchanged, so one environment still builds under every binding.

Build rules (reported together with every other configuration problem,
before any factory runs):

| Condition | Error |
|---|---|
| `SERVE=grpc`, `SERVE_AUTH` includes `bearer`, no authenticator registered | `EndUserAuthenticatorMissing { component, key: <SERVE_AUTH key> }` |
| `SERVE=grpc`, `SERVE_AUTH` includes `link`, no inbound token | `Missing { key: SEKVENT_LINK_INBOUND_<CALLER> }` (C2, unchanged) |
| `SERVE_AUTH=bearer` alone | needs no `SEKVENT_LINK_*` key |

An authenticator registered while no component uses it is not an error
(the App may expose components only in some environments).

## 5. Serving order

Everything is still decided from the request head before the body is read
(C2 2.4). For an exposed component:

1. **Link.** When the mode includes `link`, the bearer token is checked
   against the inbound tokens (constant time). A match makes the caller
   that link; subject and tenant survive only for a trusted link (C2).
2. **Link-only rejection.** Under `link` alone, no match is
   `UNAUTHENTICATED` with `sekvent_link::REJECTED_MESSAGE`, before routing
   (unchanged).
3. **Route.** The path must be exactly `/<service>/<Rpc>`; anything else is
   `UNIMPLEMENTED` / `UNKNOWN_METHOD`. Under the bearer modes routing comes
   before the end-user authenticator, so an unknown path never costs a
   token verification (the path is public anyway).
4. **Context.** `headers::from_headers(&headers, link_caller)`; the
   deadline is narrowed by the method's timeout.
5. **End user.** When the mode includes `bearer`, no link matched and the
   method is not anonymous, the authenticator runs, bounded by the call's
   deadline: an authenticator that exceeds it yields `DEADLINE_EXCEEDED` with
   the message "the call deadline was exceeded". Its answer is mapped as in
   3.2; on success the context becomes `cx.with_end_user(user)`. Subject and
   tenant headers sent by the client were already dropped in step 4 (the
   client is not a trusted link), so an end user cannot assert another
   identity.
6. **Hop limit**, then the gate, bulkhead, method timeout and handler,
   exactly as for a link caller.

`anonymous` waives step 5 only. It never waives link authentication: under
`link` an anonymous method still requires a link token, and under local
bindings it has no effect. A component that must stay closed to the public
therefore cannot be opened by the attribute alone; the operator has to
choose a bearer mode too.

## 6. Composition with the runtime server

- **gRPC-Web.** `ServerBuilder::grpc_routes(app.grpc_routes())` puts the
  component routes behind the server's gRPC-Web translation like every
  other gRPC route (runtime feature `grpc-web`; facade feature
  `runtime-grpc-web`). Under `.prefix("/api")` the browser calls
  `/api/<service>/<Rpc>`; native gRPC stays at the root unless
  `grpc_at_root(false)`.
- **CORS.** Preflights are answered by the server's CORS layer before
  authentication, so they never reach a component. The default allowed and
  exposed headers already include `authorization`, `x-grpc-web`,
  `grpc-timeout`, `grpc-status`, `grpc-message` and
  `grpc-status-details-bin`.
- **`ServerBuilder::authenticator`.** Its identity only feeds the
  `CallContext` the server stores in request extensions for REST handlers
  and hand-written tonic services. It never rejects a request, and the
  component service never reads it: component calls are authenticated by
  `SERVE_AUTH` alone. The end-user authenticator receives the request head
  with its extensions, so it may read that context if an application wants
  to reuse the server's decision.
- **Global layers** (`ServerBuilder::layer`) wrap component calls too. A
  layer that rejects requests without its own credentials rejects link
  callers and anonymous methods as well; mount such layers on the REST
  router instead, or let gRPC paths through.

## 7. Decisions the owner might override

1. **One authenticator per App** rather than one per component. Most
   applications have one session format; a per-component registry can be
   added later without breaking this API.
2. **The head, not the body.** The authenticator gets
   `&http::request::Parts`. It cannot read the message, which keeps the
   "decide before the body" rule.
3. **Roles stay local.** `EndUser` carries roles to the authenticating
   component's handlers only; components further down see subject and
   tenant, the same as behind a trusted link. Propagating roles over links
   would let any trusted link assert roles.
4. **Anonymous means anonymous.** A token sent to an anonymous method is
   ignored instead of being verified optionally; a method that personalises
   for signed-in users is not anonymous.
5. **Non-`UNAUTHENTICATED` errors pass through** the authenticator
   boundary unchanged, so infrastructure failures stay retryable.
6. **Combined mode tries the link first.** Link tokens and end-user tokens
   share the `authorization` header; a link match never reaches the
   authenticator.

## 8. Tests

- `sekvent-context`: `EndUser` builders and getters; `with_end_user`,
  `with_caller` exclusivity; `child`, `detached` and `sanitize_for_caller`.
- `sekvent-auth`: `verify_headers` and `end_user` (valid, missing, wrong,
  expired, custom tenant and roles); `HasRoles` for `EndUser`.
- `sekvent-component` unit tests: `SERVE_AUTH` grammar and build rules;
  the serving decisions of section 5 for every mode, anonymous methods,
  error normalisation and the authenticator deadline; the duplicate
  registration error; `MethodDescriptor` anonymous.
- Loopback (`tests/grpc_end_user.rs`, 127.0.0.1:0, 30 s guard, real clock):
  a gRPC-Web request over HTTP/1.1 under `.prefix("/api")` with a bearer
  token reaching the handler with the end user; a missing and a wrong
  token answered identically before the handler runs; an anonymous method
  without a token; a link caller next to an end user under `link,bearer`;
  a CORS preflight.
- `sekvent-macros`: snapshot (`release` is anonymous in the standard
  case), parse tests, the trybuild fail case `m22_anonymous_on_local_only`.
