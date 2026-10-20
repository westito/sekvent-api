# Typed extensions on `EndUser` (`sekvent-context`)

> Status: implemented. Builds on
> [`component-end-user.md`](component-end-user.md). The user guide is
> [modules/context.md](../modules/context.md#concepts) and
> [modules/components.md: Read the end user in a handler](../modules/components.md#read-the-end-user-in-a-handler).

## 1. Problem

An end-user authenticator often learns more than subject, tenant and roles.
An API token for an automated client carries a scope (the permissions it
may use, the resources it may touch); a session row carries a session id or
a device. The authenticator loads that record anyway, to check revocation,
but `EndUser` has nowhere to put it, so every handler that needs it reads
the same record again. Encoding it into roles (`scope:orders:read`) loses
structure and mixes grants with restrictions.

## 2. Scope

### In

- `EndUser::with_extension<T>(self, value: T) -> Self` and
  `EndUser::extension<T>(&self) -> Option<&T>`: at most one value per type.
- `EndUser` stays `Clone`, `PartialEq`, `Eq` and `Debug`.

### Out

- Extensions on `CallContext` itself or on service callers.
- Carrying extensions across a component call, a process boundary or a
  serialized binding.
- Mutable access after construction; the authenticator builds the end user
  once.

## 3. Decisions

### 3.1 Storage: `http::Extensions`

`sekvent-context` already depends on `http`, whose `Extensions` is a
type-map that is `Clone` (it requires `T: Clone + Send + Sync + 'static`
on insert) and allocates nothing while empty. Reusing it adds no
dependency and no unsafe code. It stays a private field: the public surface
is the two methods, so the storage can change without a breaking release.

The insert bound is `Clone + Send + Sync + 'static` because `EndUser` is
`Clone` and crosses tasks inside an `Arc` in `CallContext`. The getter
needs only `Send + Sync + 'static`, which is what `Extensions::get`
requires. One value per type, the last insert wins; the docs recommend a
newtype per meaning so two sources cannot collide on `String`.

### 3.2 Equality ignores extensions

`PartialEq` was derived. Extension types need not implement `PartialEq`,
and comparing type-erased values would need a second bound on every
insert. Equality therefore compares subject, tenant and roles only, and
`Eq` holds. The type doc says so. `EndUser` never implemented `Hash`.

### 3.3 Never logged, never propagated

- `Debug` is written by hand and prints the number of extensions
  (`extensions: 2`), never their values or type names: an extension may
  hold a token id or a scope worth keeping out of logs, and the count is
  stable across `http` releases. `CallContext`'s derived `Debug` goes
  through it.
- Nothing serializes `EndUser`: the header codec writes subject and tenant
  from the context, `child()` and `detached()` drop the end user, and only
  the gRPC serving boundary sets it (from the authenticator). Extensions
  follow the end user, so they reach the handler that served the end user
  and nothing below it, the same rule roles already follow.

## 4. Tests

`sekvent-context` unit tests: typed get, an absent type is `None`, the last
insert of a type wins, clones keep extensions, equality ignores them,
`Debug` shows the count and no value or type name, and a context with an
end user that has extensions writes none of them into injected or
propagated headers, its `Debug` or a child or detached context.
`sekvent-component` (grpc): an authenticator's extension reaches the
handler's `cx.end_user()`.
