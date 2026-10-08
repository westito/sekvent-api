# Resilience

`sekvent::resilience` (crate `sekvent-resilience`) holds the building blocks for calling a dependency that may be slow, overloaded or down: exponential backoff with jitter, bounded retries paid for by a shared retry budget, a rolling-window rate gate, a deadline-aware timeout, a bulkhead (concurrency cap), a circuit breaker, a `Policy` that composes all of them in a fixed order, a `PolicySpec` that builds a policy from configuration, a tower layer, and a small TTL cache with single flight and stale-if-error. Everything works on `AppError` and `CallContext`, not on a transport, so one policy serves the HTTP client, gRPC clients, database calls or any other async operation. `Policy::call` respects the caller's deadline and cancellation from start to finish; used on their own, the primitives each cover only part of that (see [Deadlines and cancellation](#concepts) below).

## Enable it

```toml
[dependencies]
sekvent-api = { git = "https://github.com/westito/sekvent-api", branch = "master", features = ["resilience"] }
```

```rust
use sekvent::resilience::{Policy, PolicySpec, RetryPolicy, TtlCache};
```

The default facade features (`config`, `error`, `context`, …) stay on, so `sekvent::config::EnvSource`, `sekvent::error::AppError` and `sekvent::context::CallContext` are available next to it. `features = ["full"]` also includes `resilience`.

## Quick example

Build one policy per dependency at startup from environment variables, then wrap each call:

```rust
use std::time::Duration;

use sekvent::config::EnvSource;
use sekvent::context::CallContext;
use sekvent::error::AppError;
use sekvent::resilience::{Policy, PolicySpec};

/// Built once and shared: clones share the budget, gate, slots and breaker.
fn inventory_policy() -> Result<Policy, Box<dyn std::error::Error>> {
    // INVENTORY_TIMEOUT=500ms
    // INVENTORY_RETRY_MAX_ATTEMPTS=3
    // INVENTORY_BREAKER_FAILURE_RATE=50%
    // INVENTORY_BULKHEAD_MAX_CONCURRENT=16
    let spec = PolicySpec::from_config(&EnvSource, "INVENTORY_")?;
    Ok(spec.build("inventory")?)
}

async fn stock_level(policy: &Policy, ctx: &CallContext, sku: &str) -> Result<u32, AppError> {
    // Reading stock is safe to repeat, so the call is declared idempotent.
    policy.call(ctx, true, || fetch_stock(ctx, sku)).await
}

async fn fetch_stock(ctx: &CallContext, sku: &str) -> Result<u32, AppError> {
    todo!("call the inventory service with {sku} under {ctx:?}")
}

async fn handler(policy: &Policy) -> Result<u32, AppError> {
    // The whole logical call, retries included, never outlives this deadline.
    let ctx = CallContext::new().with_timeout(Duration::from_secs(2));
    stock_level(policy, &ctx, "sku-42").await
}
```

With no keys set, `from_config` returns an empty spec and `build` gives a policy that only enforces the context deadline: no per-attempt timeout, no retries, no bulkhead, no breaker and no rate gate.

## Concepts

**Logical call and attempt.** A *logical call* is one `Policy::call`. It may run the operation several times; each run is an *attempt*. The rate gate, bulkhead and breaker act once per logical call. The timeout acts on each attempt.

**Fixed composition order.** `Policy::call` applies its parts outermost first:

```text
rate gate  ->  bulkhead  ->  circuit breaker  ->  retry( timeout( operation ) )
 1 permit      1 slot for     fails fast while    each attempt has its own
 per call      the whole      open; records one   timeout, capped by the
               call           outcome per call    call deadline
```

Every part is optional except the timeout. Without a configured limit, the timeout still enforces the context deadline.

**Deadlines and cancellation in `Policy::call`.** The composed policy reads the deadline and cancellation token from the `CallContext` and, because its parts cover each other, honours both for the whole logical call:

- A call whose context is already cancelled or past its deadline fails before any part runs. It spends no rate permit, bulkhead slot or breaker permit.
- A per-attempt timeout is `min(configured limit, time left before the deadline)`, so no attempt runs past the deadline.
- Retries never sleep past the deadline. If the next attempt would start at or after it, the last error is returned.
- Cancellation stops a running attempt, a backoff sleep and any queue wait with `CANCELLED`.

**The primitives on their own** cover less. Each guarantee above comes from one part, so a primitive used alone has only its own share:

| Primitive | Deadline | Cancellation |
|---|---|---|
| `RateGate::acquire_within` | Honoured: never waits past it, and a dead context takes no permit. | Honoured while queued. |
| `RateGate::acquire`, `try_acquire` | Not read. | Not read. |
| `Timeout::call` | Honoured: the future is bounded by `min(limit, time left)`. | **Not watched.** A cancelled context does not stop the future, and an already cancelled one still runs it. |
| `RetryPolicy::retry` | Checked before the first attempt and before each retry; **not enforced during an attempt**, so a hanging attempt runs past the deadline unless it carries its own timeout. | Checked before the first attempt; stops a running attempt and a backoff sleep. |
| `Bulkhead::acquire` | Bounds the queue wait only. A free slot is handed out at once without looking at the context, even a dead one. | Watched while queued only. |
| `Bulkhead::call` | As `acquire`; the wrapped future then runs **unbounded**. | As `acquire`; the wrapped future is not stopped. |
| `CircuitBreaker::call`, `acquire` | Not read. | Not read. |
| `TtlCache::get_or_try_insert` | Takes no context. A waiter waits as long as the loading caller's load takes; only the loader's own code bounds it. | Not read. |

To get the composed behaviour, use `Policy::call`, or wrap the operation yourself, for example `Timeout::call` inside `RetryPolicy::retry`, and a `Timeout::call` around a cache lookup whose waiters must not outlive their own deadline.

**Idempotency is the caller's promise.** Nothing is retried unless the caller passes `idempotent = true`. Configuration never makes an operation retryable.

**Transient errors.** Retries and stale cache serving look only at the error code:

| Rule | Codes |
|---|---|
| Retryable (`ErrorCode::is_transient`) | `UNAVAILABLE`, `DEADLINE_EXCEEDED`, `RESOURCE_EXHAUSTED`, `ABORTED` |
| Counts against a breaker by default (`ErrorCode::trips_breaker`) | `UNAVAILABLE`, `DEADLINE_EXCEEDED`, `RESOURCE_EXHAUSTED` |

Every other code (`INVALID_ARGUMENT`, `NOT_FOUND`, `INTERNAL`, …) is returned at once. A breaker counts these other codes as successes, because the dependency did answer. See [error.md](error.md) for the full code list.

**Shared state.** `RetryBudget`, `RateGate`, `Bulkhead` and `CircuitBreaker` hold state and are shared through `Arc`. `Policy`, `RetryPolicy` and `TtlCache` are cheap to clone, and clones share their state. Build a policy once per dependency and clone it. Building one per request gives every request a fresh breaker and budget, which defeats both.

**Time.** Every wait uses tokio's clock, so `tokio::time::pause` and `advance` drive them in tests. Deadlines are measured on tokio's clock too: `sekvent::resilience::remaining(&ctx)` returns the time left, and unlike `CallContext::remaining` it follows a paused clock. The breaker and the cache read time from an injectable `MonotonicClock`.

## How to configure backoff and jitter

`Backoff` produces the delay before retry number `attempt`, counting from zero: `initial * multiplier^attempt`, capped at `max`, then jittered.

```rust
use std::time::Duration;
use sekvent::resilience::{Backoff, Jitter};

let standard = Backoff::default();                                     // 100 ms doubling to 5 s, full jitter
let doubling = Backoff::exponential(Duration::from_millis(50), Duration::from_secs(2)); // x2, full jitter
let fixed = Backoff::constant(Duration::from_millis(250));             // always 250 ms, no jitter
let custom = Backoff::new(
    Duration::from_millis(100), Duration::from_secs(10), 1.5, Jitter::Equal,
)?;                                                                    // Result<Backoff, PolicyError>
let quiet = doubling.with_jitter(Jitter::None);
```

| Constructor | Behaviour |
|---|---|
| `Backoff::exponential(initial, max)` | Multiplier 2, full jitter. A `max` below `initial` is raised to `initial`. |
| `Backoff::constant(delay)` | Multiplier 1, no jitter. |
| `Backoff::new(initial, max, multiplier, jitter) -> Result<_, PolicyError>` | Rejects a multiplier that is not finite or is below 1 (`backoff.multiplier`), and a `max` below `initial` (`backoff.max`). |

`Jitter` modes, also used as configuration values (`none`, `full`, `equal`, case-insensitive):

| `Jitter` | Delay |
|---|---|
| `None` | Exactly the exponential delay. |
| `Full` (default) | Uniform in `[0, delay]`. |
| `Equal` | `delay / 2` plus uniform in `[0, delay / 2]`. |

Reading delays:

- `base_delay(attempt)` returns the delay without jitter.
- `delay(attempt, &mut rng)` jitters with any `rand::Rng`.
- `iter()` returns an endless `BackoffIter` seeded from thread-local entropy, and `&Backoff` implements `IntoIterator` the same way. Combine either with `.take(n)`.
- `iter_with_rng(rng)` takes your own generator, for example a seeded `rand::rngs::SmallRng` (rand 0.10) for reproducible delays.

The accessors are `initial()`, `max()`, `multiplier()` and `jitter()`.

A hand-rolled reconnect loop is a typical use outside `RetryPolicy`:

```rust
use std::time::Duration;
use sekvent::resilience::Backoff;

let backoff = Backoff::exponential(Duration::from_secs(1), Duration::from_secs(60));
for delay in backoff.iter().take(10) {
    if try_connect().await.is_ok() {
        break;
    }
    tokio::time::sleep(delay).await;
}
```

## How to retry with a budget

`RetryPolicy` retries a failed attempt only when **all** of these hold:

1. The caller passed `idempotent = true`.
2. The error is transient (see the table above).
3. Fewer than `max_attempts` attempts have run. The count includes the first attempt.
4. The error's `retry_after` hint, if any, is at most `max_retry_after` (default 30 s). An upstream that asks for a longer pause gets it: the error is returned at once and the caller is not parked.
5. The delay ends before the call deadline. The delay is the larger of the backoff delay and the error's `retry_after`.
6. The shared `RetryBudget`, if any, has a token.

```rust
use std::sync::Arc;
use std::time::Duration;
use sekvent::resilience::{Backoff, RetryBudget, RetryPolicy};

let budget = Arc::new(RetryBudget::new(0.2, 10)?);         // 0.2 tokens per success, 10 retries/s floor
let retry = RetryPolicy::new(4, Backoff::exponential(Duration::from_millis(50), Duration::from_secs(1)))
    .with_budget(Arc::clone(&budget))
    .with_max_retry_after(Duration::from_secs(10));

let order = retry.retry(&ctx, true, || orders.get(&ctx, order_id)).await?;
```

| Item | Meaning |
|---|---|
| `RetryPolicy::new(max_attempts, backoff)` | Without a budget. `0` attempts is treated as `1`. |
| `RetryPolicy::none()` | A single attempt that never retries. |
| `RetryPolicy::default()` | 3 attempts, `Backoff::default()`, `RetryBudget::default()`. |
| `with_budget(Arc<RetryBudget>)` / `without_budget()` | Attach or detach a shared budget. |
| `with_max_retry_after(Duration)` | The longest `retry_after` hint worth waiting for. |
| `with_seed(u64)` | Seed the jitter source, for reproducible delays. |
| `retry(&ctx, idempotent, op)` | `op: FnMut() -> Future<Output = Result<T, AppError>>`. Fails before the first attempt with `CANCELLED` if the context is already cancelled, or `DEADLINE_EXCEEDED` if the deadline has already passed. Cancellation stops a running attempt or a backoff sleep with `CANCELLED`. The deadline is not enforced while an attempt runs; give each attempt a `Timeout` (as `Policy` does) if attempts can hang. |
| `max_attempts()`, `backoff()`, `budget()`, `max_retry_after()` | Accessors. |

Clones of a `RetryPolicy` share the budget and the jitter source. Each retry decision is logged at `debug!` with the attempt number, the delay and the error code.

### The retry budget

`RetryBudget` is a token bucket that keeps retries a bounded fraction of successful traffic:

- Each success deposits `ratio` tokens, and each retry withdraws one.
- At most `max_tokens` (default 100) are banked, so a long healthy period cannot fund a retry storm.
- Independently of the balance, a floor of `min_per_sec` retries is always allowed in each one-second window, so a low-traffic caller can still retry. The floor is spent before the banked balance.
- Accounting is in thousandths of a token, so ten deposits of `0.1` make exactly one token.

| Item | Meaning |
|---|---|
| `RetryBudget::new(ratio, min_per_sec) -> Result<_, PolicyError>` | `ratio` in `0.0..=1000.0`. A ratio that rounds to zero together with a zero floor is rejected (`retry_budget.ratio`), because no retry could ever run. Use `RetryPolicy::none()` to disable retries. |
| `RetryBudget::default()` | Ratio 0.2, a floor of 10 per second, 100 tokens. |
| `with_max_tokens(u32)` | Cap the banked balance. |
| `deposit()`, `try_withdraw() -> bool` | Manual accounting; `RetryPolicy` calls these for you. |
| `available()`, `min_per_sec()` | Whole banked tokens (the floor is not included), and the floor. |

A new budget starts **empty**. Until successes have been deposited, only the per-second floor pays for retries.

## How to cap a call rate (RateGate)

`RateGate` allows at most `permits` acquisitions in any rolling `window`. A typical use is an upstream quota such as "50 calls per 60 seconds for the whole process": wrap the gate in an `Arc` and share it. Waiters are served strictly in arrival order.

```rust
use std::sync::Arc;
use std::time::Duration;
use sekvent::resilience::RateGate;

let gate = Arc::new(RateGate::new(50, Duration::from_secs(60))?);

gate.acquire_within(&ctx).await?;   // bounded by the deadline and cancellation
gate.acquire().await;               // waits as long as it takes
if gate.try_acquire() { /* a permit was free and nobody was queued */ }
```

| Item | Meaning |
|---|---|
| `RateGate::new(permits: u32, window) -> Result<_, PolicyError>` | `permits >= 1` (`rate_gate.permits`). `window` must be longer than zero and at most 366 days (`rate_gate.window`). |
| `acquire_within(&ctx) -> Result<(), AppError>` | Fails fast with `RESOURCE_EXHAUSTED` and a `retry_after` hint when the gate cannot open before the deadline. Fails with `DEADLINE_EXCEEDED` if the deadline passes while queued behind other callers, and with `CANCELLED` on cancellation. A dead context never takes a permit. |
| `acquire()` | Waits without a bound. Dropping the future gives up the place in the queue without consuming a permit. |
| `try_acquire() -> bool` | Takes a permit only if one is free now and nobody is queued ahead. |
| `permits()`, `window()` | Accessors. |

The gate remembers the time of each grant inside the window, so memory grows with `permits`.

## How to bound a call's duration (Timeout)

```rust
use std::time::Duration;
use sekvent::resilience::Timeout;

let timeout = Timeout::new(Duration::from_millis(800));
let invoice = timeout.call(&ctx, billing.fetch_invoice(&ctx, id)).await?;
assert_eq!(timeout.effective(&ctx), Some(Duration::from_millis(800)));  // if ctx has more time left
```

| Item | Meaning |
|---|---|
| `Timeout::new(limit)` | Limits each call to `limit` and to the context deadline. |
| `Timeout::deadline_only()` / `Timeout::default()` | Only the context deadline limits the call. |
| `effective(&ctx) -> Option<Duration>` | `min(limit, time left)`, or `None` when neither is set. |
| `call(&ctx, fut)` | Runs `fut` within the effective limit. On expiry it returns `DEADLINE_EXCEEDED` with metadata `timeout_ms` (the effective limit in milliseconds). A zero effective limit, which includes a deadline that has already passed, fails at once without polling `fut`, with `timeout_ms = "0"`. |
| `limit()` | The configured limit. |

On expiry the future is dropped, so the wrapped operation must be cancel-safe. `Timeout::call` does not watch the context's cancellation token: a cancelled caller does not stop the future, and an already cancelled context still runs it. Inside a `Policy`, the retry loop around the timeout handles cancellation.

## How to cap concurrency (Bulkhead)

A `Bulkhead` stops one slow dependency from tying up every task.

- **Without a queue**, a call beyond `max_concurrent` is rejected immediately.
- **With a queue**, up to `max_queue` callers wait in first-come, first-served order. Each waits at most `max_wait`, and never past its own deadline.

```rust
use std::sync::Arc;
use std::time::Duration;
use sekvent::resilience::Bulkhead;

let bulkhead = Arc::new(
    Bulkhead::new(16)?                                   // at most 16 at once
        .with_queue(32, Duration::from_millis(250))      // 32 may wait up to 250 ms each
        .with_retry_after(Duration::from_millis(500)),   // hint on rejections (default 1 s)
);

let report = bulkhead.call(&ctx, build_report(&ctx)).await?;

let permit = bulkhead.acquire(&ctx).await?;              // BulkheadPermit; the slot frees on drop
```

The context only bounds the **queue wait**. A free slot is handed out at once without looking at the context, so a cancelled or expired caller can still take one; and `Bulkhead::call` then awaits the wrapped future with no deadline or cancellation guard. Inside a `Policy` the dead-context check runs before the bulkhead and the timeout bounds the attempts; on its own, bound the future yourself (for example with `Timeout::call`).

| Outcome | Error |
|---|---|
| Full with no queue, queue full, or `max_wait` elapsed | `RESOURCE_EXHAUSTED`, reason `BULKHEAD_FULL`, `retry_after` = the configured hint |
| The deadline ran out first while queued | `DEADLINE_EXCEEDED` |
| Cancelled while queued | `CANCELLED` |

The queue is disabled when `max_queue` is 0 or `max_wait` is zero. Use `max_concurrent()`, `max_queue()`, `in_flight()` and `queued()` for metrics. `Bulkhead::new(0)` fails with `bulkhead.max_concurrent`.

## How to use a circuit breaker

A `CircuitBreaker` stops calling an unhealthy dependency for a while instead of piling more load onto it.

**States** (`BreakerState`, shown as `closed`, `open`, `half_open`):

- **Closed.** Calls flow and their outcomes enter a sliding window. Once the window holds at least `min_calls` and the failure fraction reaches `failure_rate`, the circuit opens.
- **Open.** Calls fail fast with `UNAVAILABLE`, reason `CIRCUIT_OPEN`, metadata `breaker = <name>`, and a `retry_after` equal to the remaining open time. After `wait_in_open`, the next look at the breaker moves it to half-open.
- **Half-open.** Up to `permitted_in_half_open` probe calls run. One failure reopens the circuit, and all of them succeeding closes it. While every probe slot is taken, further calls are rejected with `CIRCUIT_OPEN` and no `retry_after`. Closing clears the window.

**Windows** (`BreakerWindow`):

- `Count { size }`: the last `size` calls.
- `Time { duration }`: the calls of the last `duration`, tracked in ten buckets of `duration / 10`.

**Config** (`CircuitBreakerConfig`; `#[non_exhaustive]` with public fields, so start from `default()` and assign):

| Field | Default | Validation (`PolicyError::parameter`) |
|---|---|---|
| `window` | `Count { size: 20 }` | A count of at least 1, a duration longer than zero (`breaker.window`). |
| `failure_rate` | `0.5` | `0 < rate <= 1` (`breaker.failure_rate`). Resolved to per-mille. |
| `min_calls` | `10` | At least 1, and not above a count window's size, or the circuit could never open (`breaker.min_calls`). |
| `wait_in_open` | 30 s | Longer than zero (`breaker.wait_in_open`). |
| `permitted_in_half_open` | `3` | At least 1 (`breaker.permitted_in_half_open`). |

```rust
use std::sync::Arc;
use std::time::Duration;
use sekvent::error::{AppError, ErrorCode};
use sekvent::resilience::{BreakerWindow, CircuitBreaker, CircuitBreakerConfig, StateTransition};

let mut config = CircuitBreakerConfig::default();
config.window = BreakerWindow::Time { duration: Duration::from_secs(30) };
config.failure_rate = 0.25;
config.min_calls = 20;

let breaker = Arc::new(
    CircuitBreaker::new("billing", config)?
        // Also count INTERNAL answers as failures of the dependency.
        .with_classifier(|error: &AppError| {
            error.code().trips_breaker() || error.code() == ErrorCode::Internal
        })
        .on_state_change(|t: &StateTransition| {
            tracing::warn!(breaker = %t.name, from = %t.from, to = %t.to, "billing breaker moved");
        }),
);

// Simple form: acquire, run, record.
let invoice = breaker.call(billing.fetch_invoice(&ctx, id)).await?;

// Manual form, when you can tell whose fault a failure was.
let permit = breaker.acquire()?;                 // Err(UNAVAILABLE / CIRCUIT_OPEN) while open
let result = billing.fetch_invoice(&ctx, id).await;
let callers_fault = match &result {
    // The caller walked away, or its own deadline ran out: not the dependency's fault.
    Err(error) if error.code() == ErrorCode::Cancelled => ctx.cancel_token().is_cancelled(),
    Err(error) if error.code() == ErrorCode::DeadlineExceeded => {
        sekvent::resilience::remaining(&ctx) == Some(Duration::ZERO)
    }
    _ => false,
};
if callers_fault {
    permit.release();                            // record nothing
} else {
    permit.record(&result);                      // classified by the breaker's classifier
}
```

These are the same checks `Policy::call` makes. Checking only the cancellation token is not enough: a caller whose deadline expired would still count against the dependency. A `Policy` with a breaker does this for you.

| Item | Meaning |
|---|---|
| `CircuitBreaker::new(name, config) -> Result<_, PolicyError>` | The name appears in logs, transitions and the `breaker` metadata. |
| `with_clock(Arc<dyn MonotonicClock>)` | Measure time on another clock (default `TokioClock`). |
| `with_classifier(Fn(&AppError) -> bool)` | Decide which errors are failures. The default is `ErrorCode::trips_breaker`. The stored form is the `FailureClassifier` alias. |
| `on_state_change(Fn(&StateTransition))` | Called after every transition, outside the breaker's lock. Transitions are also logged at `info!`. |
| `acquire() -> Result<BreakerPermit<'_>, AppError>` | Permission for one call. |
| `call(fut)` | Acquire, await, record. This form does not inspect the context. |
| `state()`, `metrics()`, `name()` | `metrics()` returns `BreakerMetrics { state, calls, failures, failure_rate_permille, rejected }`. The window counts describe the closed state, and `rejected` counts since creation. |

`BreakerPermit` is `#[must_use]`. Report through `record(&result)`, `record_success()` or `record_failure()`, or give it back with `release()`. Dropping it unreported is the same as `release()`: nothing is recorded, and a half-open probe slot is freed. A permit issued before a state change is ignored when it reports, so a slow call from the previous period cannot reopen or close the circuit.

Inside a `Policy`, the breaker records **one outcome per logical call**, after all retries. It releases the permit unrecorded when the operation never ran, or when the call ended because of the caller's own cancellation or expired deadline. A per-attempt timeout that fires while the caller still had time **does** count as a failure, because the dependency was too slow. `CircuitBreaker::call` alone does not make this distinction; use the manual form or a `Policy`.

## How to compose a Policy by hand

```rust
use std::sync::Arc;
use std::time::Duration;
use sekvent::resilience::{
    Backoff, Bulkhead, CircuitBreaker, CircuitBreakerConfig, Jitter, Policy, PolicyError, RateGate,
    RetryBudget, RetryPolicy, Timeout,
};

fn billing_policy() -> Result<Policy, PolicyError> {
    let backoff = Backoff::new(Duration::from_millis(50), Duration::from_secs(2), 2.0, Jitter::Equal)?;
    let retry = RetryPolicy::new(3, backoff)
        .with_budget(Arc::new(RetryBudget::new(0.2, 10)?.with_max_tokens(50)));
    Ok(Policy::new("billing")
        .with_timeout(Timeout::new(Duration::from_millis(800)))
        .with_retry(retry)
        .with_rate_gate(Arc::new(RateGate::new(50, Duration::from_secs(60))?))
        .with_bulkhead(Arc::new(Bulkhead::new(16)?.with_queue(32, Duration::from_millis(250))))
        .with_breaker(Arc::new(CircuitBreaker::new("billing", CircuitBreakerConfig::default())?)))
}

let invoice = policy.call(&ctx, true, || billing.fetch_invoice(&ctx, id)).await?;
```

| Item | Meaning |
|---|---|
| `Policy::new(name)` / `Policy::default()` | Deadline-only timeout and no retries. `default()` is named `default`. |
| `with_timeout`, `with_retry`, `with_rate_gate(Arc<_>)`, `with_bulkhead(Arc<_>)`, `with_breaker(Arc<_>)` | Set a part. Passing the same `Arc` to several policies shares the part between them. |
| `call(&ctx, idempotent, op)` | Run `op` under the policy. Without `idempotent`, `op` runs at most once. |
| `name()`, `timeout()`, `retry()`, `rate_gate()`, `bulkhead()`, `breaker()` | Accessors. |

The rate-gate permit is taken before the bulkhead slot. A call that the bulkhead then rejects has still used a permit.

## How to build a Policy from configuration (PolicySpec)

`PolicySpec` is a serializable, layerable description of a `Policy`. Every field is an `Option`, and an unset field means "inherit".

```rust
use std::time::Duration;
use sekvent::config::EnvSource;
use sekvent::resilience::PolicySpec;

// Reads ORDERS_TIMEOUT, ORDERS_RETRY_MAX_ATTEMPTS, … (the prefix is used verbatim).
let spec = PolicySpec::from_config(&EnvSource, "ORDERS_")?;     // Result<_, ConfigError>
let policy = spec.build("orders")?;                             // Result<Policy, PolicyError>
```

**Layering.** `overlay(&over)` returns a copy in which every field that `over` sets replaces the base. `PolicySpec::resolve(layers)` folds layers with the lowest precedence first. The intended order is: defaults, named policy, component override, method override.

```rust
let mut defaults = PolicySpec::default();      // #[non_exhaustive]: start from default() and assign
defaults.timeout = Some(Duration::from_secs(2));
defaults.retry_max_attempts = Some(3);

let slow = PolicySpec::from_config(&EnvSource, "POLICY_SLOW_")?;
let create = PolicySpec::from_config(&EnvSource, "ORDERS_CREATE_")?;
let policy = PolicySpec::resolve([&defaults, &slow, &create]).build("orders.create")?;
```

**Serde.** `PolicySpec` implements `Serialize` and `Deserialize` with the lowercase field names shown in the table below. Unknown fields are rejected. Durations serialize as humantime strings (`"1s 500ms"`) and accept bare integers as seconds. `breaker_window` is a `BreakerWindowSpec`: `Calls(u32)` (the last N calls) or `Duration(Duration)` (the calls of the last duration), which build `BreakerWindow::Count` and `BreakerWindow::Time`. It serializes as a number (calls) or a humantime string (duration); it also implements `FromStr` and `Display`, where bare digits are a call count and anything else is parsed as a duration (`"20"` is 20 calls, `"30s"` is 30 seconds). `retry_jitter` serializes as `"none"`, `"full"` or `"equal"`.

```rust
let spec: PolicySpec = serde_json::from_str(
    r#"{ "timeout": "500ms", "retry_max_attempts": 3, "breaker_window": 20 }"#,
)?;
```

**What turns each part on.**

| Part | Built when |
|---|---|
| Per-attempt timeout | `timeout` is set. Otherwise the timeout is deadline-only. |
| Retries | `retry_max_attempts` is above 1. Each build creates a fresh `RetryBudget` from the budget fields. |
| Bulkhead | `bulkhead_max_concurrent` is set. A queue alone builds nothing. |
| Breaker | `breaker_enabled = true`, or `breaker_failure_rate` is set and `breaker_enabled` is not `false`. |
| Rate gate | `rate_limit_permits` is set. The window defaults to 1 s. |

`build_retry()`, `build_bulkhead()` and `build_breaker(name)` build one part alone and return `Ok(None)` when that part is off. To share one budget between several policies, replace the fresh budget:

```rust
use std::sync::Arc;
use sekvent::resilience::RetryBudget;

let shared = Arc::new(RetryBudget::default());
let mut policy = spec.build("orders")?;
if let Some(retry) = spec.build_retry()? {
    policy = policy.with_retry(retry.with_budget(Arc::clone(&shared)));
}
```

`build` creates new stateful parts every time it is called. Call it once per dependency and clone the result.

`PolicySpec::CONFIG_KEYS` lists the 20 key suffixes. `from_config` ignores any other key under the prefix, so a misspelt key such as `ORDERS_TIMEOT` does nothing. Use `CONFIG_KEYS` if you want to detect unknown keys.

## How to apply a policy to a tower service (PolicyLayer)

`PolicyLayer` wraps any tower `Service` whose request implements `PolicyRequest` and whose error converts `Into<AppError>`. The inner service must be `Clone`, because each attempt drives its own clone to readiness. The wrapped service's error type is `AppError`, and its `poll_ready` is always ready: backpressure comes from the policy's bulkhead.

`http::Request<B>` with a `Clone` body implements `PolicyRequest` out of the box:

- **Context.** It is read from the request extensions. Without one, the context is fresh and has no deadline, so only the retry policy's own limits bound the waits.
- **Idempotency.** `GET`, `HEAD`, `PUT`, `DELETE`, `OPTIONS` and `TRACE` are idempotent, as is any request that carries an `idempotency-key` header.

```rust
use tower::{ServiceBuilder, ServiceExt};
use sekvent::resilience::PolicyLayer;

let service = ServiceBuilder::new()
    .layer(PolicyLayer::new(policy.clone()))
    .service(inner);                               // or PolicyService::new(inner, policy)

let mut request = http::Request::get("/orders/42").body(String::new())?;
request.extensions_mut().insert(ctx.clone());      // the deadline and cancellation travel with it
let response = service.oneshot(request).await?;
```

For your own request types, implement the trait. A `try_clone` that returns `None` disables retries for that request:

```rust
use sekvent::context::CallContext;
use sekvent::resilience::PolicyRequest;

struct ReserveStock {
    ctx: CallContext,
    sku: String,
    quantity: u32,
}

impl PolicyRequest for ReserveStock {
    fn context(&self) -> CallContext {
        self.ctx.clone()
    }
    fn idempotent(&self) -> bool {
        false
    }
    fn try_clone(&self) -> Option<Self> {
        Some(Self { ctx: self.ctx.clone(), sku: self.sku.clone(), quantity: self.quantity })
    }
}
```

`PolicyService::get_ref()` returns the wrapped service.

## How to cache hot values (TtlCache)

`TtlCache<K, V>` is a small in-memory cache for hot sets such as reference data, settings and tokens. It is not meant as a general-purpose cache. It is cheap to clone, and clones share entries. `K: Eq + Hash + Clone + Send + Sync + 'static` and `V: Clone + Send + Sync + 'static`; wrap large values in `Arc`.

```rust
use std::sync::Arc;
use std::time::Duration;
use sekvent::resilience::TtlCache;

let rates: TtlCache<String, Arc<ExchangeRates>> = TtlCache::builder(Duration::from_secs(60))
    .max_entries(1_000)                         // default 10 000
    .stale_if_error(Duration::from_secs(300))   // default 0: never serve stale
    .build()?;                                  // Result<_, PolicyError>

let eur = rates
    .get_or_try_insert(currency.clone(), || fx.load_rates(&ctx, &currency))
    .await?;

rates.invalidate(&currency);                    // after a change
```

> **Never serve authorization data stale.** Do not set `stale_if_error` on a cache of permissions, roles, sessions, API keys or anything else that grants access: during an outage of the source, a revoked grant would keep working for the whole stale window. Cache such data, if at all, with a short TTL and no stale window, so a failed reload fails closed.

`get_or_try_insert` takes no `CallContext`. A caller that waits for another caller's load waits as long as that load takes, whatever its own deadline; wrap the lookup in `Timeout::call(&ctx, …)` when that matters (dropping a waiter is safe).

| Item | Meaning |
|---|---|
| `TtlCache::new(ttl)` | 10 000 entries, no stale serving. A zero TTL fails (`cache.ttl`). |
| `TtlCache::builder(ttl)` → `max_entries(usize)`, `stale_if_error(Duration)`, `clock(Arc<dyn MonotonicClock>)`, `build()` | `build` fails on a zero TTL (`cache.ttl`) or a zero entry limit (`cache.max_entries`). |
| `get(&k) -> Option<V>` | A fresh value only, never a stale one. |
| `insert(k, v)` | Store a value, fresh for the TTL, replacing any entry for the key. |
| `get_or_try_insert(k, load) -> Result<V, AppError>` | Return the fresh value, or run `load` once per key however many callers wait. |
| `invalidate(&k) -> bool` | Drop the entry; returns whether there was one. |
| `clear()` | Drop every entry. |
| `len()`, `is_empty()` | Entries that are fresh or inside their stale window. Dead entries are pruned as a side effect. |

**TTL.** A value is fresh for `ttl` after it is stored, measured on the cache's `MonotonicClock` (`TokioClock` unless another is injected).

**Single flight.** For a missing or expired key:

- The first caller runs its loader, and concurrent callers for the same key wait for it.
- On success, everyone gets the value and it is stored.
- On failure, the loading caller gets the original error. The waiters get a copy rebuilt from the wire form: code, message, reason, domain, metadata, `retry_after` and field violations, but not the internal source chain.
- Errors are never cached, so the next call loads again.
- If the loading caller is dropped or panics mid-load, one of the waiters takes over the load.
- No lock is held across an await.

**Stale if error.** With a window set, a reload that fails with a *transient* error inside `stale_if_error` past expiry returns the expired value instead, to the loading caller and to the waiters. A `warn!` is logged with the error code and reason, never the value. That key is then not reloaded for `min(ttl, 5 s)`, and `get_or_try_insert` keeps returning the stale value meanwhile; `get` still returns `None`. Non-transient errors, an entry past its window, and a key that was never loaded all return the error.

**Max entries.** Inserting a *new* key into a full cache first drops every entry past its stale window. If the cache is still full, it then evicts the entry that expires soonest. Replacing an existing key evicts nothing.

**Invalidate semantics.** `invalidate` and `clear` remove entries, including any stale fallback, but do not cancel a load that is already running: that load still stores its value when it finishes. If you invalidate because the source changed, a load that started before the change can put the old value back for one TTL. Keep the TTL short enough to tolerate that.

## How to choose a clock

| Type | Use |
|---|---|
| `MonotonicClock` | Trait: `fn elapsed(&self) -> Duration` since a fixed origin. Implement it for your own time source. |
| `TokioClock` | The default for the breaker and the cache. It reads tokio's clock, so it follows `tokio::time::pause` and `advance`. Its origin is its creation time (`TokioClock::new()` / `default()`). |
| `WallClock<C: Clock>` | Adapts a wall `sekvent::context::Clock`, typically `ManualClock` in tests, measured from the Unix epoch. Use it to drive a breaker or cache without a tokio runtime. |

Retries, the rate gate, timeouts and the bulkhead always use tokio's clock directly.

## Configuration keys

`PolicySpec::from_config(source, prefix)` reads `<prefix><SUFFIX>` from any `ConfigSource`: environment variables through `EnvSource`, or a `MapSource` in tests. The examples below use the prefix `ORDERS_`. The serde field is the lowercase suffix.

- **Durations** accept humantime (`500ms`, `5s`, `2m`, `1s 500ms`) or bare integer seconds.
- **Counts** are non-negative integers.
- **"Default"** is what `build` uses when the key is unset and its part is enabled.

| Key (env form) | Serde field | Values and validation | Default |
|---|---|---|---|
| `ORDERS_TIMEOUT` | `timeout` | A duration longer than zero. Per attempt, and capped by the call deadline. | None (deadline only) |
| `ORDERS_RETRY_MAX_ATTEMPTS` | `retry_max_attempts` | A count of at least 1, including the first attempt. `1` disables retries. | `1` |
| `ORDERS_RETRY_INITIAL_BACKOFF` | `retry_initial_backoff` | A duration. | 100 ms |
| `ORDERS_RETRY_MAX_BACKOFF` | `retry_max_backoff` | A duration, not below the initial backoff (checked at build: `backoff.max`). | 5 s |
| `ORDERS_RETRY_MULTIPLIER` | `retry_multiplier` | A finite decimal of at least 1. | `2` |
| `ORDERS_RETRY_JITTER` | `retry_jitter` | `none`, `full` or `equal`. | `full` |
| `ORDERS_RETRY_BUDGET_RATIO` | `retry_budget_ratio` | A decimal from 0 to 1000. A ratio of 0 together with a floor of 0 is rejected at build (`retry_budget.ratio`). | `0.2` |
| `ORDERS_RETRY_BUDGET_MIN_PER_SEC` | `retry_budget_min_per_sec` | A count (0 allowed). | `10` |
| `ORDERS_RETRY_MAX_RETRY_AFTER` | `retry_max_retry_after` | A duration. Longer upstream hints are returned without retrying. | 30 s |
| `ORDERS_BULKHEAD_MAX_CONCURRENT` | `bulkhead_max_concurrent` | A count of at least 1. Turns the bulkhead on. | Off |
| `ORDERS_BULKHEAD_MAX_QUEUE` | `bulkhead_max_queue` | A count (0 = no queue). | `0` |
| `ORDERS_BULKHEAD_QUEUE_TIMEOUT` | `bulkhead_queue_timeout` | A duration. Must be longer than zero when the queue is above 0 (checked at build: `bulkhead.queue_timeout`). | 1 s with a queue |
| `ORDERS_BREAKER_ENABLED` | `breaker_enabled` | `true`/`false`, `1`/`0`, `yes`/`no`, `on`/`off`. `false` wins over a set failure rate. | Set if the failure rate is set |
| `ORDERS_BREAKER_FAILURE_RATE` | `breaker_failure_rate` | A fraction above 0 and at most 1 (`0.5`), or a percentage (`50%`). Turns the breaker on. | `0.5` |
| `ORDERS_BREAKER_WINDOW` | `breaker_window` | **Bare digits are a call count** (at least 1). Anything else is a humantime duration longer than zero (`30s`). | 20 calls |
| `ORDERS_BREAKER_MIN_CALLS` | `breaker_min_calls` | A count of at least 1, not above a call-count window. | `10`, or the window size if that is smaller |
| `ORDERS_BREAKER_WAIT_IN_OPEN` | `breaker_wait_in_open` | A duration longer than zero. | 30 s |
| `ORDERS_BREAKER_PERMITTED_IN_HALF_OPEN` | `breaker_permitted_in_half_open` | A count of at least 1. | `3` |
| `ORDERS_RATE_LIMIT_PERMITS` | `rate_limit_permits` | A count of at least 1. Turns the rate gate on. | Off |
| `ORDERS_RATE_LIMIT_WINDOW` | `rate_limit_window` | A duration longer than zero, at most 366 days (checked at build: `rate_gate.window`). | 1 s |

`from_config` checks every key and reports all problems at once: one `ConfigError` for a single bad key, or `ConfigError::Multiple`. Unparsable values give `Malformed`, and out-of-range values give `Invalid`. Messages name the key and never the value. Problems that involve several fields, or limits only the builders know, surface from `build` as a `PolicyError` instead. Its `parameter()` is a dotted name such as `backoff.max`, not the environment key.

Components read the same field grammar under their own keys (`SEKVENT_COMPONENT_<C>_…`, `SEKVENT_POLICY_<N>_…`) with their own defaults and rules. For example, there a method `TIMEOUT` bounds the whole call, and `RATE_LIMIT_*` is not accepted. See [components.md](components.md).

## Errors and reasons emitted by this crate

| Source | Code | Reason / metadata | `retry_after` |
|---|---|---|---|
| `Policy::call`, `RetryPolicy::retry` or `RateGate::acquire_within` with a context already cancelled, or cancellation while waiting (a queued `Bulkhead::acquire` too) | `CANCELLED` | none | none |
| `Policy::call` or `RetryPolicy::retry` with a deadline already passed | `DEADLINE_EXCEEDED` | none | none |
| `Timeout` expiry (and so a per-attempt timeout in a `Policy`) | `DEADLINE_EXCEEDED` | metadata `timeout_ms` (the effective limit) | none |
| `Timeout::call` alone with a deadline already passed | `DEADLINE_EXCEEDED` | metadata `timeout_ms = "0"` | none |
| `RateGate::acquire_within`, no permit before the deadline | `RESOURCE_EXHAUSTED` | none | when the next permit frees up |
| `RateGate::acquire_within`, deadline passed while queued | `DEADLINE_EXCEEDED` | none | none |
| `Bulkhead`, full, queue full or wait elapsed | `RESOURCE_EXHAUSTED` | reason `BULKHEAD_FULL` | the configured hint (default 1 s) |
| `Bulkhead`, deadline passed while queued | `DEADLINE_EXCEEDED` | none | none |
| `CircuitBreaker` open | `UNAVAILABLE` | reason `CIRCUIT_OPEN`, metadata `breaker` | remaining open time |
| `CircuitBreaker` half-open, all probes busy | `UNAVAILABLE` | reason `CIRCUIT_OPEN`, metadata `breaker` | none |
| `RetryPolicy` | The last attempt's error, unchanged | | |
| `TtlCache` waiters | The loader's error, rebuilt with `AppError::from_wire` | | |

The reasons are plain string values; the crate exports no constants for them. Construction problems are `PolicyError` (with `parameter()`, `reason()` and a `Display` of `invalid resilience parameter <parameter>: <reason>`), never `AppError`. The parameter names are:

`backoff.jitter`, `backoff.multiplier`, `backoff.max`, `retry.max_attempts`, `retry_budget.ratio`, `rate_gate.permits`, `rate_gate.window`, `bulkhead.max_concurrent`, `bulkhead.queue_timeout`, `breaker.window`, `breaker.failure_rate`, `breaker.min_calls`, `breaker.wait_in_open`, `breaker.permitted_in_half_open`, `timeout`, `cache.ttl`, `cache.max_entries`.

## Testing tips

- **Pause tokio's clock.** Run async tests with `#[tokio::test(start_paused = true)]`; your dev-dependency on tokio needs the `test-util` feature. Backoff sleeps, rate-gate windows, bulkhead waits, timeouts, `TokioClock` and the retry-budget floor then advance instantly and deterministically. Assert exact elapsed times with `tokio::time::Instant`.
- **Build deadlines from tokio's clock.** Use `CallContext::new().with_deadline(tokio::time::Instant::now().into_std() + d)`. Read the time left with `sekvent::resilience::remaining(&ctx)`, not `CallContext::remaining`, which reads the real clock.
- **Make jitter reproducible.** Use `RetryPolicy::with_seed`, `Backoff::iter_with_rng` with a seeded generator, or `Jitter::None` / `Backoff::constant`.
- **Drive the breaker and cache without a runtime.** Inject `WallClock::new(ManualClock::new(start))` and call `advance` on the `ManualClock`.
- **Validate configuration.** Feed `PolicySpec::from_config` a `MapSource` to test key parsing and error reporting without touching the process environment.
- **Avoid fixed sleeps.** Tests that touch real sockets cannot use a paused clock; see [testing.md](testing.md).

```rust
use std::cell::Cell;
use std::time::Duration;
use sekvent::context::CallContext;
use sekvent::error::AppError;
use sekvent::resilience::{Backoff, RetryPolicy};

#[tokio::test(start_paused = true)]
async fn retries_twice_then_succeeds() {
    let policy = RetryPolicy::new(3, Backoff::constant(Duration::from_millis(100)));
    let calls = Cell::new(0_u32);
    let started = tokio::time::Instant::now();
    let value = policy
        .retry(&CallContext::new(), true, || {
            calls.set(calls.get() + 1);
            let n = calls.get();
            async move { if n < 3 { Err(AppError::unavailable("down")) } else { Ok(n) } }
        })
        .await
        .unwrap();
    assert_eq!(value, 3);
    assert_eq!(started.elapsed(), Duration::from_millis(200));
}
```

```rust
use std::sync::Arc;
use std::time::{Duration, SystemTime};
use sekvent::context::ManualClock;
use sekvent::resilience::{BreakerState, CircuitBreaker, CircuitBreakerConfig, WallClock};

let clock = ManualClock::new(SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000));
let breaker = CircuitBreaker::new("orders", CircuitBreakerConfig::default())?
    .with_clock(Arc::new(WallClock::new(clock.clone())));
// … record failures until it opens …
clock.advance(Duration::from_secs(30));
assert_eq!(breaker.state(), BreakerState::HalfOpen);
```

## Pitfalls and security

- **Declare idempotency honestly.** Retrying a non-idempotent write can duplicate it. Through the tower layer, a `POST` is retried only with an `idempotency-key` header. See [context.md](context.md) for how idempotency keys propagate.
- **Build once, share.** A `Policy`, breaker, budget or gate created per request has no memory, so the breaker never opens and the budget never fills.
- **Budgets start empty.** With `min_per_sec = 0`, a new budget allows no retry until successful calls have deposited tokens.
- **Backoff bounds interact.** Setting `RETRY_INITIAL_BACKOFF` above 5 s without `RETRY_MAX_BACKOFF` fails at build with `backoff.max`, because the default cap is 5 s.
- **`BREAKER_WINDOW=30` means 30 calls**, not 30 seconds; write `30s` for a time window. Every other duration key reads bare integers as seconds.
- **Misspelt keys are ignored** by `from_config`. Check them against `PolicySpec::CONFIG_KEYS` if that matters. The serde form rejects unknown fields.
- **Overload rejections are retryable upstream.** Rate-gate and bulkhead rejections are `RESOURCE_EXHAUSTED`, which is transient and carries `retry_after`. A caller further up may retry them, and a caller's breaker counts them as failures of this service. That is intended for an overloaded service, but keep the hints realistic.
- **Long `retry_after` hints are not waited out.** A hint above `max_retry_after` returns the error immediately. Raise `RETRY_MAX_RETRY_AFTER` only if parking the caller is acceptable.
- **`CircuitBreaker::call` cannot tell the caller's fault from the dependency's.** Prefer a `Policy`, or the manual permit with `release()` for caller-side cancellation and deadlines.
- **Wrapped operations must be cancel-safe.** Timeouts and cancellation drop the operation's future mid-flight.
- **`RateGate::acquire` has no bound.** In request paths prefer `acquire_within(&ctx)`.
- **Cache invalidation races a running load.** See the invalidate semantics above. Errors shared with waiters carry the loader's message and metadata, so do not put data specific to one caller into errors for a key that several callers share.
- **No secrets in logs.** The crate logs codes, reasons, attempt counts, delays and breaker names, never values, request bodies or configuration values. Configuration errors name keys only. Keep secrets out of breaker and policy names, and out of `AppError` messages your loaders return.

## See also

- [../README.md](../README.md), [../features.md](../features.md), [../getting-started.md](../getting-started.md), [../cli.md](../cli.md)
- [error.md](error.md): `ErrorCode`, `AppError`, `retry_after`, transient and breaker rules.
- [context.md](context.md): `CallContext`, deadlines, cancellation, `ManualClock`.
- [config.md](config.md): `ConfigSource`, `EnvSource`, `MapSource`, `ConfigError`.
- [client.md](client.md): the outbound HTTP client takes a `Policy`.
- [components.md](components.md) and [../component-model.md](../component-model.md): per-component and per-method policies under `SEKVENT_COMPONENT_*` and `SEKVENT_POLICY_*`.
- [jobs.md](jobs.md), [runtime.md](runtime.md), [server.md](server.md), [db.md](db.md), [link.md](link.md), [auth.md](auth.md), [telemetry.md](telemetry.md), [testing.md](testing.md), [proto-build.md](proto-build.md)
