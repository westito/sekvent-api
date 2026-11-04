# Client addresses and per-client limits

Public, anonymous calls (pricing a cart, placing an order, mailing a
sign-in code) must be protected against floods from one client without
limiting everyone. This note records the decisions; the module pages
([context](../modules/context.md), [server](../modules/server.md),
[resilience](../modules/resilience.md), [components](../modules/components.md))
describe the behaviour.

## Client address

- `CallContext::client_addr` is a context field like subject and tenant:
  `child()` and `detached()` keep it, so in-process component calls see
  the edge's client, and `sanitize_for_caller` drops it with them.
- On the wire it is `x-sekvent-client-addr`, written by `inject` and
  `propagate` (never `propagate_external`) and kept by `from_headers` only
  from a trusted caller: the same trust rule as subject and tenant. A
  server-rendered frontend calling with a trusted link token forwards its
  visitor; an untrusted link cannot pose as someone else.
- The server resolves the transport's address once per request with a
  `ClientAddrPolicy` (TCP peer by default; behind trusted proxies, the
  forwarding header walked from the right) and stores it both in the
  context (unless a trusted link forwarded one) and as `ClientAddr` in the
  extensions. The TCP peer reaches the router as axum's
  `ConnectInfo<SocketAddr>`, so a router served elsewhere with
  `into_make_service_with_connect_info` behaves the same.
- Walking from the right is the only safe reading of `X-Forwarded-For`:
  everything left of the last trusted hop is client-written. A malformed
  element where the walk reads falls back to the peer rather than guessing.
- Networks are `ipnet::IpNet` (already in the dependency tree, re-exported
  from `sekvent::runtime`).
- The address is personal data: never logged, `RateKey`'s `Debug` hides it.

## Keyed rate limiter

- A token bucket per key, stored as the instant it is full again (the
  generic cell rate algorithm): one instant per key, no timers, exact
  `retry_after`. Refusals take nothing, so a client that keeps hammering
  does not push its own recovery further out.
- Memory is bounded twice: entries idle past `max(idle_ttl, fill time)` are
  full again and dropped lazily (oldest first, on every acquisition), and
  at `max_keys` the least recently used key is dropped. Never an error.
- IPv6 counts by its /64; IPv4-mapped IPv6 counts as IPv4.
- A key that cannot be resolved maps to one shared `unknown` bucket: fail
  closed, so hiding the key does not escape the limit.
- `CLIENT_RATE_LIMITED` is `RESOURCE_EXHAUSTED` (so HTTP `429` and
  `retry_after` work everywhere) but is excluded from retries and from the
  breaker's default classifier: retrying drains the same bucket, and one
  client's limit says nothing about the callee's health.

## Component serving policy

- `CLIENT_RATE` / `CLIENT_BURST` / `CLIENT_KEY` / `CLIENT_MAX_KEYS` are
  serving-side fields at method level and in named policies, not at
  component level (a component-wide limit is a named policy referenced
  from the component's `_POLICY`). `RATE_LIMIT_*` (a process-wide gate)
  stays refused.
- One limiter per named policy, built once and shared by every method that
  resolves to it; a method with `CLIENT_*` keys of its own gets its own
  buckets, its keys layered over the policy's.
- The check runs after routing and context decoding, before the body is
  read: `client_addr` before the end-user authenticator (a flood never
  costs a token verification), `end_user*` right after it. In-process
  bindings never apply it; the limit belongs to the network edge.
- `App::client_limiter(component, method)` hands the same `ClientLimiter`
  to `ClientLimitLayer` (crate feature `http`, facade `resilience-http`),
  so a REST facade over a method and its gRPC callers share buckets.
