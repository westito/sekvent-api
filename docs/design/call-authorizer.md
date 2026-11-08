# The call authorizer

One policy hook per App that decides, for every call a component of the
process serves, whether it runs, keyed by component and method.

## 1. Problem

An app often wants one central rule over all its methods: "machine
principals may call the read methods of `inventory` only with the scope
`inventory:r`", "only administrators call the write methods of `billing`".
Without a framework hook the check is repeated in every handler, and a new
method that forgets it is open.

Handlers are also the wrong place under the `local` bindings: the callee
context is narrowed (`CallContext::child`), so an in-process handler sees
the trusted caller `local` and the forwarded subject, never the end user of
the outermost call. The hook must run under every binding and see exactly
the context the handler will get, so the app can key on whatever identity
that context carries (an end user, a link, a forwarded subject, the tenant).

## 2. Scope

### In

- A `CallAuthorizer` registered once on the `AppBuilder`.
- It runs on the serving side of every call, under every binding (`grpc`
  served, `local`, `local-serialized`) and for every caller (end users,
  links, anonymous and in-process callers), unary calls and stream opens.
- A build-time `validate` per component, so an app can refuse to start when
  its policy table does not cover an installed component.

### Out

- Asynchronous decisions (I/O, a remote policy engine): the hook is
  synchronous. An app that needs I/O loads its policy before building and
  shares it with the authorizer (e.g. through an `ArcSwap`).
- Per-item decisions on a stream: only the open is authorized.
- The caller side of a component bound `grpc`: the serving process decides.
- Calls made on an implementation directly rather than through a handle.

## 3. Public API (`sekvent-component`, no feature needed)

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CallTarget { /* component descriptor + method index */ }

impl CallTarget {
    pub fn new(component: &'static ComponentDescriptor, index: usize) -> Option<Self>;
    pub const fn component(&self) -> &'static ComponentDescriptor;
    pub fn method(&self) -> &'static MethodDescriptor;
    pub const fn index(&self) -> usize;
}

pub trait CallAuthorizer: Send + Sync + 'static {
    fn authorize(&self, target: CallTarget, cx: &CallContext) -> Result<(), AppError>;
    fn validate(&self, component: &'static ComponentDescriptor) -> Result<(), AppError> {
        Ok(())
    }
}

impl<F> CallAuthorizer for F
where
    F: Fn(CallTarget, &CallContext) -> Result<(), AppError> + Send + Sync + 'static;

impl AppBuilder<'_> {
    pub fn call_authorizer(&mut self, authorizer: impl CallAuthorizer) -> Result<(), BuildError>;
}

// BuildError (non-exhaustive) gains:
DuplicateCallAuthorizer,
CallAuthorizerRefused { component: String, source: AppError },
```

`CallTarget` carries the static descriptors, so a policy can match on the
component name, the method name or RPC, and the declared flags
(`is_idempotent`, `is_stream`, `is_anonymous`, …) without allocating.

## 4. Where it runs

| Binding | Point | Context | Panic |
|---|---|---|---|
| served over gRPC / gRPC-Web | after link authentication, routing, the end-user authenticator, both client limits and the hop limit; before the body is read and before the gate | as decoded from the head, with the end user or link caller | `INTERNAL`, `HANDLER_PANICKED`, tagged; the payload is never exposed |
| `local` | in the caller's task, before the gate | caller `local` (trusted), forwarded subject and tenant, no end user | propagates, like a handler panic |
| `local-serialized` | in the serving task, after the context is decoded from the headers, before the gate | as under `local` | `INTERNAL`, like a handler panic |
| caller side of `grpc` | never | | |

Under gRPC the hook sits last among the head-only checks, so a request
refused by the authenticator, a client limit or the hop limit never costs a
policy decision, and a refused request never has its body read: a caller
cannot make the server buffer up to 4 MiB for a call it may not make. It
runs before the gate, so a denial takes no admission or bulkhead permit,
does not count against the breaker and does not shed.

## 5. Decisions

- **The error is the answer, unchanged.** No `component`/`method`
  metadata, no normalisation: the app chooses the code
  (`PERMISSION_DENIED` or `UNAUTHENTICATED`), the reason and the metadata
  the caller sees, and a browser receives exactly that over gRPC-Web. The
  framework only tags its own failures (an authorizer panic).
- **Every caller, no implicit allow.** The hook also sees link callers,
  anonymous calls and in-process calls. An app whose policy only concerns
  end users must allow the others explicitly (`cx.end_user().is_none()`).
  Exempting them silently would make the hook a weaker guarantee than its
  name.
- **Synchronous.** The check runs on every call, including in-process ones
  that are otherwise a function call; an async hook would box a future per
  call and invite I/O on the hot path.
- **`validate` at build.** It runs once per installed component that has
  an implementation in this binary, whatever its binding (a component bound
  `grpc` here may be served by the same binary elsewhere), never for a
  `remote_only` install. Refusals are reported together with the
  configuration errors and no factory runs, like every other build error.
- **One per App.** Composition (several policies) is the app's business;
  a second registration is `DuplicateCallAuthorizer`, like the end-user
  authenticator.

## 6. Tests

- Unit (`authorizer.rs`, `server.rs`): closure impl, default `validate`,
  out-of-range targets, the server's `authorize` with and without one.
- Builder (`tests/app.rs`): duplicate registration, `validate` once per
  component with an implementation and never for remote-only installs,
  refusals reported with configuration errors and no factory run.
- Bindings (`tests/authorizer.rs`): allowed call reaches the handler, a
  denial returns unchanged and the handler never runs, what the authorizer
  sees under `local` and `local-serialized`, nested calls authorized at
  the nested component, stream opens denied, panic per binding, and the
  caller side of a `grpc` binding never asking.
- gRPC service (`grpc/service.rs`): after authentication with the roles,
  after the hop limit, never for unknown paths, for anonymous, optional and
  link callers, before the body (a body that never arrives is still
  refused), a panic caught.
- gRPC-Web (`tests/grpc_end_user.rs`): a role-based denial arrives as
  `grpc-status` 7 with the app's message, reason and metadata.
- Facade (`sekvent-facade-check`): a function registered through
  `sekvent::component`.
