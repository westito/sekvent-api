# Runtime (`sekvent::runtime`)

`sekvent::runtime` runs a service process: a staged, supervised set of
long-running *units* (listeners, consumers, jobs, component hosts) that start
in a fixed order, report readiness, and drain in reverse order within bounded
grace periods when a signal arrives or a critical unit stops. It also owns the
process-wide health state (liveness, readiness, per-service gRPC status, build
version) and polls dependency probes that feed readiness.

The same crate holds the combined gRPC/gRPC-Web/REST listener and the job
scheduler; they have their own pages: [Server](server.md) and
[Jobs](jobs.md).

## Enable it

| | |
|---|---|
| Facade feature | `runtime` (on by default) |
| Optional features | `runtime-grpc-web` (gRPC-Web on the listener, see [Server](server.md)), `runtime-cron` (cron jobs, see [Jobs](jobs.md)) |
| Module | `use sekvent::runtime::{Runtime, RuntimeBuilder, RuntimeHandle, Stage, UnitContext, UnitPolicy, RestartPolicy};` |
| Prelude | `Runtime`, `RuntimeBuilder`, `RuntimeHandle`, `ShutdownTrigger`, `Stage`, `UnitContext`, `UnitPolicy` (plus `Server`, `ServerBuilder`, `Ctx`, `JobSpec`, `JobContext`) |
| Internal crate | `sekvent-runtime` |

`runtime` also switches on the runtime integrations of other crates you
enable: with `db`, pool readiness probes and (with `db-lease`) the
`LeaseGuard` for singleton jobs; with `component`, `App::register` runs an
App as a runtime unit.

## Quick example

```rust
use sekvent::prelude::*;

#[tokio::main]
async fn main() -> Result<(), AppError> {
    let report = Runtime::builder()
        .unit("orders-consumer", Stage::Workers, UnitPolicy::Critical, |ctx: UnitContext| async move {
            // connect, subscribe, …
            ctx.ready();                        // this stage may now finish starting
            ctx.shutdown().cancelled().await;   // the stage drains
            // finish in-flight work, close connections
            Ok(())
        })
        .build()?
        .run()                                  // until SIGTERM/SIGINT, then drain
        .await?;
    tracing::info!(reason = %report.reason, "stopped");
    Ok(())
}
```

## Concepts

### Units

A unit is a named future produced by a **factory**
(`FnMut(UnitContext) -> impl Future<Output = Result<(), AppError>>`). The
runtime calls the factory once per run, so a restarted unit gets a fresh
future and a fresh [`UnitContext`](#use-the-unit-context). A well-behaved
long-running unit:

1. sets itself up,
2. calls `ctx.ready()`,
3. works until `ctx.shutdown()` fires,
4. finishes in-flight work and returns `Ok(())` before its stop deadline.

A panic, in the future or in the factory that builds it, is a failure like
any other (`INTERNAL`, message `unit <name> panicked`).

### Stages and their order

Every unit belongs to a `Stage`. Stages start in this order, each only once
the previous one has fully started, and drain in the reverse order:

| Start order | `Stage` | For |
|---|---|---|
| 1 | `Infrastructure` | connections, pools, caches, the dependency probes |
| 2 | `Components` | application components built on the infrastructure |
| 3 | `Workers` | background jobs, consumers, schedulers |
| 4 | `Ingress` | listeners that accept outside traffic |

So ingress stops taking traffic before the workers and components it feeds,
and those stop before the infrastructure they use. `Stage::ALL` lists them in
start order; `stage.as_str()` and `Display` give the lowercase name used in
logs (`infrastructure`, `components`, `workers`, `ingress`). `Stage` is
`#[non_exhaustive]`.

A stage counts as **started** once every unit in it has called
`ctx.ready()`, or, for a `Critical` or `BestEffort` unit, has exited. A
`Restart` unit holds its stage until one of its runs reports ready. A stage
that does not start within the start timeout (default 30 s) fails startup
with `DEADLINE_EXCEEDED`, naming the units still waiting.

When every stage has started, the health registry is marked started and
readiness can turn true.

### Unit policies

`UnitPolicy` decides what happens when a unit's future completes **on its
own**, before its stage was asked to drain:

| Policy | On `Ok(())` | On `Err` or panic |
|---|---|---|
| `Critical` (the default) | shut the runtime down (`ShutdownReason::UnitExited`) | shut down, and `run()` returns the error (`ShutdownReason::UnitFailed`) |
| `Restart(RestartPolicy)` | run again after a backoff | run again after a backoff |
| `BestEffort` | log a warning and carry on without the unit | log a warning and carry on |

A `Restart` unit that has used up `max_restarts` is treated as a critical
failure on its next exit (`UNAVAILABLE`, `unit <name> exited after
exhausting its <n> restarts`, with the last error as source), even if that
exit was `Ok`. An exit before the unit reported ready counts as a restart
too.

`RestartPolicy` is a plain struct:

```rust
use std::time::Duration;
use sekvent::runtime::{RestartPolicy, UnitPolicy};

let policy = UnitPolicy::Restart(RestartPolicy {
    initial: Duration::from_millis(200), // delay before the first restart
    max: Duration::from_secs(30),        // cap for any delay
    multiplier: 2.0,                     // at least 1.0, finite
    max_restarts: Some(5),               // None: restart forever
});
```

`RestartPolicy::default()` is 100 ms doubling up to 30 s, restarting
forever. `policy.delay(n)` returns the delay before restart `n + 1`
(`delay(0) == initial`), capped at `max`. A run that lasts at least
`restart_reset_after` (default 60 s) counts as healthy and resets both the
restart count and the backoff, so `max_restarts` bounds a crash loop rather
than restarts over the process lifetime. `UnitReport::restarts` still counts
every restart.

A failure while the unit is already stopping is logged; for a `Critical`
unit it also makes `run()` return the error.

### Shutdown sequence

Shutdown begins on the first of:

- an operating-system signal (`SIGTERM` or `SIGINT` on unix, Ctrl-C
  elsewhere), registered when `run` or `start` begins so a signal sent
  during startup is caught;
- a future given to `shutdown_on` or `shutdown_on_signal` resolving;
- `ShutdownTrigger::shutdown()` or `RuntimeHandle::shutdown()`, or a dropped
  `RuntimeHandle`;
- a `Critical` unit exiting, a `Restart` unit running out of restarts, or a
  stage failing to start in time.

Then, in order:

1. Health turns not serving: readiness becomes false, every named gRPC
   service reports `NOT_SERVING`.
2. The runtime waits `shutdown_delay` (default 0) so load balancers notice.
3. Stages drain in reverse start order. For each stage, the stop deadline is
   set, then every unit's `shutdown()` token fires; the units of the stage
   are awaited together and those still running at the end of the stage
   grace (default 10 s) are aborted (`UnitExit::Aborted`).
4. The whole shutdown, delay included, is bounded by `shutdown_deadline`
   (default 30 s): each stage's grace is cut short so that it never runs
   past it.

If shutdown begins during startup, stages not yet started are skipped and
their units are reported as `UnitExit::NotStarted`.

### Health

`HealthRegistry` is the process-wide health state. Every clone shares it;
`RuntimeBuilder::health()`, `Runtime::health()`, `RuntimeHandle::health()`
and `UnitContext::health()` all return the same registry.

- **Liveness** is true unless something called `mark_fatal(reason)`.
- **Readiness** is true when every stage has started, shutdown has not
  begun, the process is not fatal, and every *required* dependency probe last
  reported `ProbeStatus::Up`.

The [server](server.md#serve-health-endpoints) serves it as `/livez`, `/readyz`,
`/healthz` and `grpc.health.v1.Health`.

### Dependency probes

A `DependencyProbe` checks one external dependency. The runtime polls every
registered probe concurrently on an interval (default 10 s), each with a
timeout (default 2 s), in a `BestEffort` infrastructure unit named
`PROBE_UNIT` (`"health-probes"`). That unit reports ready after its first
round, so later stages, and readiness, start from real results rather than
"unknown". A probe that times out or panics is
`Down(Unreachable("probe timed out"))` / `Down(Unreachable("probe panicked"))`
for that round and is asked again on the next. Transitions are logged once
(`dependency is up` at `info`, `dependency is down` at `warn` with `kind` and
`detail`).

An *optional* probe (`required()` returns `false`) is reported in full health
output but never makes the service unready.

## How to …

### Register units

```rust
RuntimeBuilder::unit<F, Fut>(self, name: impl Into<String>, stage: Stage, policy: UnitPolicy, factory: F) -> Self
where
    F: FnMut(UnitContext) -> Fut + Send + 'static,
    Fut: Future<Output = Result<(), AppError>> + Send + 'static;
```

Unit names must be non-empty and unique (the probe unit's name
`health-probes` is taken once you add a probe). Other registrations that
produce units:

- `RuntimeBuilder::job(name, stage, spec, run)` — see [Jobs](jobs.md).
- `Server::into_unit()` — the listener, usually in `Stage::Ingress`; see
  [Server](server.md).
- `app.register(builder)` — the component App (`App::register(&self,
  RuntimeBuilder) -> RuntimeBuilder`, so `let builder = app.register(builder);`),
  in `Stage::Components`; see [Components](components.md).

A factory that captures state clones it per run:

```rust
use std::sync::Arc;
use sekvent::runtime::RestartPolicy;

let consumer = Arc::new(OrdersConsumer::new(config));
let builder = Runtime::builder().unit(
    "orders-consumer",
    Stage::Workers,
    UnitPolicy::Restart(RestartPolicy::default()),
    move |ctx: UnitContext| {
        let consumer = Arc::clone(&consumer);
        async move { consumer.run(ctx).await }
    },
);
```

### Use the unit context

`UnitContext` is handed to every run (clones share the token and the ready
flag):

| Method | Returns |
|---|---|
| `name()` | the registered name |
| `stage()` | the unit's `Stage` |
| `attempt()` | restarts before this run; `0` on the first |
| `ready()` | report that the unit is up; idempotent |
| `shutdown()` | a `CancellationToken` that fires when the unit's stage begins draining |
| `is_shutting_down()` | whether it has fired |
| `stage_grace()` | the runtime's stage grace, a sensible bound for cleanup |
| `stop_deadline()` | `Option<tokio::time::Instant>`: when the unit is aborted; `None` until its stage drains, set before `shutdown()` fires |
| `health()` | the shared `HealthRegistry` |

`stop_deadline()` is the earlier of the end of the stage grace and the
overall shutdown deadline, so it can be sooner than `stage_grace()` from now.
A unit that stops in several steps (stop consuming, flush, close) sizes the
steps from it:

```rust
async fn run(ctx: UnitContext, mut consumer: Consumer) -> Result<(), AppError> {
    ctx.ready();
    let shutdown = ctx.shutdown();
    loop {
        tokio::select! {
            biased;
            () = shutdown.cancelled() => break,
            message = consumer.next() => handle(message?).await?,
        }
    }
    let flush_by = ctx.stop_deadline().expect("set before shutdown fires");
    tokio::time::timeout_at(flush_by, consumer.flush()).await.ok();
    Ok(())
}
```

### Tune timeouts and deadlines

| Builder method | Default | Meaning |
|---|---|---|
| `start_timeout(d)` | 30 s | longest one stage may take to start; must be positive |
| `shutdown_delay(d)` | 0 | pause between health turning not serving and the first drain |
| `stage_grace(d)` | 10 s | longest the units of one stage may take to stop before they are aborted |
| `shutdown_deadline(d)` | 30 s | bound on the whole shutdown, delay included |
| `restart_reset_after(d)` | 60 s | a run this long resets a restarting unit's count and backoff; must be positive |
| `probe_interval(d)` | 10 s | how often probes run; must be positive when probes exist |
| `probe_timeout(d)` | 2 s | longest one probe may take; must be positive when probes exist |

The runtime does not read configuration itself; read these from your own
config struct (see [Configuration](config.md)) and pass them in, as in the
[full example](#a-full-mainrs).

`build()` fails with `INVALID_ARGUMENT` on an empty or duplicate unit or
probe name, an invalid `RestartPolicy` (a multiplier below 1.0 or not
finite, `initial > max`), a zero start timeout, restart reset period, probe
interval or probe timeout, and on any invalid [job](jobs.md#validation-rules).

### Run it

Two ways:

```rust
// Block until shutdown and drain; the usual main().
let report: RunReport = runtime.run().await?;

// Start every stage, then keep running in a background task.
let handle: RuntimeHandle = runtime.start().await?;
// … the process is up; e.g. tests talk to it here …
handle.shutdown();
let report = handle.wait().await?;
```

- `run()` returns the **first critical failure** as `Err` (a critical unit's
  error or panic, exhausted restarts, a stage that did not start in time);
  otherwise `Ok(RunReport)`.
- `start()` returns once every stage has started. If shutdown begins during
  startup, it drains and returns the critical failure, or a `CANCELLED`
  error (`shutdown began during startup: <reason>`) when there was none.
- Dropping the `run()` or `start()` future stops the runtime on the spot:
  every stage's token fires and every unit still running is aborted.
- `RuntimeHandle::wait()` waits for the drain without requesting it; it is
  cancel-safe (dropping it drops the handle, which requests shutdown).

### Trigger shutdown

```rust
let builder = Runtime::builder();
let trigger: ShutdownTrigger = builder.shutdown_trigger(); // also Runtime::shutdown_trigger, RuntimeHandle::trigger

// e.g. from an admin endpoint
trigger.shutdown();                 // idempotent, ShutdownReason::Requested
trigger.is_triggered();             // any reason
trigger.triggered().await;          // resolves once shutdown began
```

`RuntimeHandle` offers `shutdown()`, `is_shutting_down()`, `trigger()` and
`health()`. **Dropping a `RuntimeHandle` requests shutdown**; the runtime
still drains in the background. Keep the handle alive for as long as the
process should run.

External sources:

```rust
let builder = Runtime::builder()
    .without_signals()                                      // host owns the signals
    .shutdown_on(async move { stop_rx.await.ok(); })        // any future
    .shutdown_on_signal("ctrl-c", tokio::signal::ctrl_c()); // io::Result<()> source
```

`shutdown_on_signal` treats `Err` as "the handler could not be installed":
it is logged at `error` and that source is ignored from then on, never
mistaken for a delivered signal. Both report `ShutdownReason::Signal`.

### Read the run report

`RunReport { reason: ShutdownReason, units: Vec<UnitReport> }` lists every
unit in start order; `report.unit("api")` finds one.

`ShutdownReason` (`#[non_exhaustive]`, `Display`):

| Variant | When |
|---|---|
| `Signal` | an OS signal or a `shutdown_on`/`shutdown_on_signal` source |
| `Requested` | `ShutdownTrigger::shutdown`, `RuntimeHandle::shutdown`, a dropped handle |
| `UnitExited { unit }` | a critical unit returned `Ok` |
| `UnitFailed { unit }` | a critical unit failed, or a restarting unit ran out of restarts |
| `StartupTimeout { stage }` | a stage did not start within the start timeout |

`UnitReport { name, stage, restarts, exit }` with `UnitExit`
(`#[non_exhaustive]`):

| Variant | Meaning |
|---|---|
| `Completed` | returned `Ok` |
| `Failed(String)` | returned an error or panicked; the error's caller-visible text |
| `Aborted` | did not stop within its grace period and was aborted |
| `NotStarted` | never started, because shutdown began during startup |

### Report health

```rust
use sekvent::runtime::{HealthRegistry, Readiness, ServiceStatus};

let health: HealthRegistry = builder.health();
health.set_version(env!("CARGO_PKG_VERSION"));          // shown by full health output
health.set_status("orders.v1.OrdersService", ServiceStatus::Serving).await; // a named gRPC service
health.mark_fatal("ledger corrupted").await;            // liveness fails: the orchestrator restarts us

let ready: bool = health.is_ready();
let live: bool = health.is_live();
let snapshot: Readiness = health.readiness();           // ready, live, started, draining, version, probes
let mut changes = health.watch_ready();                 // tokio::sync::watch::Receiver<bool>
changes.wait_for(|ready| *ready).await.ok();
```

- `set_status` takes the fully qualified gRPC service name. The empty name
  is the overall status, derived from readiness; setting it is ignored with
  a warning. `status(name)` reads a status back.
- On shutdown the runtime calls `set_all_not_serving()`, which also turns
  every named service to `NOT_SERVING`.
- `mark_fatal` takes a `&'static str`. It is logged, never served.
- `Readiness` and `ProbeState { name, required, status }` are
  `#[non_exhaustive]` snapshots; `status` is `None` until the first probe
  round completes.
- `grpc_service()` returns the `grpc.health.v1.Health` tonic service and
  `http_routes(visibility)` the axum routes; the [server](server.md) mounts
  both for you.

### Add dependency probes

```rust
use futures::future::BoxFuture;
use sekvent::runtime::{DependencyProbe, ProbeFailure, ProbeStatus};

struct InventoryProbe {
    client: InventoryClient,
}

impl DependencyProbe for InventoryProbe {
    fn name(&self) -> &str {
        "inventory"                 // stable; shown in full health output
    }

    fn required(&self) -> bool {
        false                       // default true; optional probes never make us unready
    }

    fn probe(&self) -> BoxFuture<'_, ProbeStatus> {
        Box::pin(async move {
            match self.client.ping().await {
                Ok(()) => ProbeStatus::Up,
                Err(error) if error.is_auth() => ProbeStatus::Down(ProbeFailure::Rejected("credentials rejected")),
                Err(_) => ProbeStatus::Down(ProbeFailure::Unreachable("ping failed")),
            }
        })
    }
}

let builder = Runtime::builder()
    .probe(InventoryProbe { client })
    .probe_interval(Duration::from_secs(5))
    .probe_timeout(Duration::from_secs(1));
```

- `ProbeStatus` is `Up` or `Down(ProbeFailure)`; `is_up()` tells them apart.
- `ProbeFailure` (`#[non_exhaustive]`) is `Rejected(&'static str)` (the
  dependency answered but refused us) or `Unreachable(&'static str)`;
  `kind()` returns `"rejected"`/`"unreachable"`, `detail()` the text.
- The detail is a **static, caller-safe** string chosen by you. Never put
  upstream response text into it: it may hold credentials or internal
  addresses.
- Probe names must be non-empty and unique.

Database pools come with ready-made probes: register every pool's probe from
the [database registry](db.md) with
`registry.probes().into_iter().fold(Runtime::builder(), |b, p| b.probe(p))`.

### A full `main.rs`

A service with a database, a REST/gRPC listener, a cleanup job and
configurable shutdown timing. `OrdersApi`, `OrdersServiceServer` (generated
by tonic from your proto) and `rest_routes()` are your code.

```rust
use std::net::SocketAddr;
use std::time::Duration;

use sekvent::config::EnvSource;
use sekvent::prelude::*;
use sekvent::runtime::Cors;
use sekvent::telemetry::TelemetryOptions;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    sekvent::telemetry::init("orders", TelemetryOptions::default())?;
    let source = EnvSource;

    let addr: SocketAddr = sekvent::config::opt_parse(&source, "ORDERS_ADDR", "0.0.0.0:8080".parse()?)?;
    let shutdown_delay = sekvent::config::opt_duration(&source, "ORDERS_SHUTDOWN_DELAY", Duration::from_secs(5))?;
    let stage_grace = sekvent::config::opt_duration(&source, "ORDERS_STAGE_GRACE", Duration::from_secs(10))?;

    let repo = OrdersRepo::connect(&source).await?;

    let mut server = Server::builder()
        .add_service(OrdersServiceServer::new(OrdersApi::new(repo.clone())))
        .rest(rest_routes(repo.clone()))
        .prefix("/api");
    if let Some(cors) = Cors::from_config(&source, "ORDERS_CORS_")? {
        server = server.cors(cors);
    }
    let server = server.bind(addr).await?;

    let cleanup = JobSpec::interval(Duration::from_secs(600)).timeout(Duration::from_secs(120));

    let builder = Runtime::builder()
        .shutdown_delay(shutdown_delay)
        .stage_grace(stage_grace)
        .shutdown_deadline(shutdown_delay + 3 * stage_grace)
        .job("orders-cleanup", Stage::Workers, cleanup, move |cx: JobContext| {
            let repo = repo.clone();
            async move { repo.delete_expired(&cx.call_context()).await }
        })
        .unit("api", Stage::Ingress, UnitPolicy::Critical, server.into_unit());
    builder.health().set_version(env!("CARGO_PKG_VERSION"));

    let report = builder.build()?.run().await?;
    tracing::info!(reason = %report.reason, "orders stopped");
    Ok(())
}
```

`Server::builder().rest(..)` takes an `axum::Router` and `add_service`
takes a tonic service, so the service's own manifest depends on `axum` and
`tonic` at the versions the workspace pins (axum 0.8, tonic 0.14).

## Testing

- **Build with `without_signals()`** so tests never install OS signal
  handlers, then `start()` and drive shutdown yourself:

  ```rust
  #[tokio::test(start_paused = true)]
  async fn the_consumer_stops_on_drain() {
      let handle = Runtime::builder()
          .without_signals()
          .unit("consumer", Stage::Workers, UnitPolicy::Critical, |ctx: UnitContext| async move {
              ctx.ready();
              ctx.shutdown().cancelled().await;
              Ok(())
          })
          .build()
          .unwrap()
          .start()
          .await
          .unwrap();

      handle.shutdown();
      let report = handle.wait().await.unwrap();
      assert_eq!(report.unit("consumer").unwrap().exit, UnitExit::Completed);
  }
  ```

- **Paused time** (`#[tokio::test(start_paused = true)]`) makes restart
  backoff, start timeouts, stage grace and probe intervals exact and instant
  when no sockets are involved. Assert against `tokio::time::Instant`.
- **Observe readiness** through the registry: take `builder.health()` before
  `build()`, then `health.readiness()` or
  `health.watch_ready().wait_for(|ready| *ready)`. Probes in tests are small
  structs whose answer the test switches (`Arc<Mutex<ProbeStatus>>`).
- **Report from units through channels** (`tokio::sync::mpsc`, `oneshot`)
  instead of sleeping; drop guards inside a unit's future tell you whether it
  was aborted.
- **Anything with a socket** binds `127.0.0.1:0`, runs on the real clock, and
  wraps waits in a 30 s `tokio::time::timeout`; see
  [Server testing](server.md#testing).
- Tests that install a global tracing subscriber belong in their own test
  binary.

## Pitfalls

- **Dropping the `RuntimeHandle` shuts the service down.** `let _ =
  runtime.start().await?;` starts and immediately drains. Bind it to a named
  variable and keep it.
- **A unit that never calls `ready()` stalls startup** until the start
  timeout fails the run. A `Critical` or `BestEffort` unit that exits
  unblocks its stage; a `Restart` unit does not until one run reports ready.
- **A `Critical` unit returning `Ok` shuts everything down.** A unit that
  finishes its work and should not take the process with it is
  `BestEffort`.
- **Ignoring `shutdown()`** gets the unit aborted at the stop deadline and
  reported as `Aborted`; anything after its last `.await` never runs.
- **The stage grace is per stage, the shutdown deadline is global.** With the
  defaults (10 s grace, 30 s deadline) and a 5 s shutdown delay, the last
  stage to drain can get less than its full grace.
- **`shutdown_delay` only helps if something watches readiness.** Point the
  load balancer or orchestrator at `/readyz` (or gRPC health), not `/livez`.
- **Probe details are served** (with full visibility) and logged: keep them
  static and free of upstream text.
- **`mark_fatal` is permanent** for the process: liveness stays false until
  it is restarted.

## See also

- [Server](server.md): the listener unit, health endpoints and their visibility.
- [Jobs](jobs.md): interval, cron and manual jobs as units.
- [Database](db.md): pool probes and leases.
- [Components](components.md): running an App as a unit.
- [Configuration](config.md) and [Telemetry](telemetry.md).
- [Getting started](../getting-started.md).
