# Testing (`sekvent-testing`)

`sekvent-testing` runs database-backed tests against real Postgres and
MySQL servers in Docker containers: one server per test binary, started on
first use, and a fresh, uniquely named database per test. Containers carry
labels and a watchdog that removes them when the test process ends, even
after `SIGKILL`, and `cargo sekvent harness-clean` sweeps any leftovers. The
crate also provides `await_until!`, a bounded poll for side effects that have
no completion signal of their own.

This page also collects the testing rules sekvent itself follows and
recommends for services built on it: deterministic time, sockets, global
state, tracing capture and the coverage gate.

## Enable it

`sekvent-testing` is not re-exported by the facade: it belongs under
`[dev-dependencies]`.

| Feature | Adds |
|---|---|
| (none) | `Harness`, `Reaper`, `await_until!`, `docker_tests_enabled`, address helpers, sweeps |
| `postgres` | `PostgresHarness` (pulls sqlx with Postgres) |
| `mysql` | `MySqlHarness` (pulls sqlx with MySQL) |

```toml
[dev-dependencies]
sekvent-testing = { git = "https://github.com/westito/sekvent-api", branch = "master", features = ["postgres"] }
tokio = { version = "1", features = ["macros", "rt-multi-thread", "test-util"] }
```

Use the same source as your `sekvent-api` dependency (git, or a path into a
checkout: `{ path = "vendor/sekvent-api/crates/sekvent-testing" }`). In a
workspace, declare it once under `[workspace.dependencies]` and write
`sekvent-testing = { workspace = true, features = ["postgres"] }` in each
crate. `use sekvent_testing::…;`.

## Quick example

```rust
use std::path::Path;

use sekvent::db::{Pool, migrate_on_boot};
use sekvent_testing::{PostgresHarness, await_until, docker_tests_enabled};

#[tokio::test]
#[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --include-ignored"]
async fn orders_are_persisted() {
    if !docker_tests_enabled() {
        return;
    }
    let server = PostgresHarness::shared().await;          // one container per test binary
    let db = server.create_database().await.unwrap();       // fresh database per test
    let pg = sqlx::PgPool::connect(&db.url).await.unwrap();
    migrate_on_boot(&pg, Path::new("migrations")).await.unwrap();   // feature db-migrate

    let repo = OrdersRepo::new(Pool::Postgres(pg));
    repo.enqueue_import().await.unwrap();
    await_until!(repo.count().await.unwrap() == 1);         // only for effects without a signal
}
```

Run it:

```sh
SEKVENT_DOCKER_TESTS=1 cargo test -- --include-ignored
```

## Concepts

- **Opt-in.** Docker tests are `#[ignore]`d *and* return early unless
  `SEKVENT_DOCKER_TESTS` is `1` or `true`. A plain `cargo test` never
  touches Docker, and `cargo test -- --ignored` on a machine without a
  daemon stays green.
- **Shared server, isolated databases.** `PostgresHarness::shared()` /
  `MySqlHarness::shared()` start one container lazily per test binary and
  hand out a `&'static` reference; every test calls `create_database()` for
  its own empty database. Tests stay isolated without paying a container
  start each.
- **Labels.** Every container gets three labels under a namespace
  (default `io.sekvent.harness`): `<ns>=true`, `<ns>.run=<run id>` and
  `<ns>.created=<unix seconds>`. Cleanup selects by these labels only, never
  by image, so it cannot touch containers you started yourself. It does not
  separate projects: every project that keeps the default namespace shares
  it, so on a shared Docker daemon `harness-clean --all --yes` in one
  project removes another project's running harness containers, and a stale
  sweep removes its long-running ones. Give each project its own namespace
  ([below](#clean-up-leftovers)).
- **Reaper.** A detached `sh` watchdog per container holds the read end of
  a pipe only the test process writes to. When the process exits for any
  reason, the pipe closes and the watchdog runs
  `docker rm --force --volumes <id>`.
- **Throwaway credentials.** Each server gets a fresh random admin
  password; URLs carry it, so `TestDatabase`'s and the harnesses' `Debug`
  output never print a URL.

## How to …

### Start a database server

```rust
impl PostgresHarness {
    pub async fn shared() -> &'static Self;                                        // panics if it cannot start
    pub async fn start(harness: &Harness, image: ServerImage) -> Result<Self, HarnessError>;
    pub fn default_image() -> ServerImage;                                         // postgres:17-alpine
    pub async fn create_database(&self) -> Result<TestDatabase, HarnessError>;
    pub fn url(&self) -> String;                 // server URL without a database
    pub fn admin_url(&self) -> String;           // the `postgres` database
    pub fn url_for(&self, name: &str) -> String;
    pub fn host(&self) -> &str;
    pub fn port(&self) -> u16;
}
```

`MySqlHarness` has the same API; its default image is `mysql:8.4`, its
admin user `root` and admin database `mysql`. Postgres connects as user
`sekvent`.

- `shared()` uses `Harness::from_env()` and the image from
  `SEKVENT_TEST_POSTGRES_IMAGE` / `SEKVENT_TEST_MYSQL_IMAGE` (`name:tag`) or
  the default. It lives until the process exits; its reaper removes it.
- `start()` gives a dedicated server, removed when the value is dropped or,
  failing that, when the process exits. Use it when a test must stop or
  break the server.
- `create_database()` returns `TestDatabase { name, url }`: name `t_` plus
  32 hex digits (valid unquoted on both backends), URL with the admin
  credentials.
- The servers trade durability for speed (Postgres: `fsync=off`,
  `synchronous_commit=off`, `full_page_writes=off`, `max_connections=500`;
  MySQL: `--skip-log-bin`, `--innodb-flush-log-at-trx-commit=0`,
  `--max-connections=500`). Startup waits for the server's readiness log and
  a successful connection (Postgres up to 120 s, MySQL 180 s).

`ServerImage { name, tag }`: `ServerImage::new("postgres", "16")` or
`ServerImage::parse("registry.example:5000/db/postgres:16")` (a reference
without a tag means `latest`; a registry port is not mistaken for a tag).

### Gate tests on Docker

```rust
pub const DOCKER_TESTS_ENV: &str = "SEKVENT_DOCKER_TESTS";
pub fn docker_tests_enabled() -> bool;   // "1" or "true" (any case, trimmed)
```

The convention, as in sekvent's own `crates/sekvent-db/tests/lease.rs`:

```rust
fn skip() -> bool {
    if docker_tests_enabled() {
        return false;
    }
    eprintln!("skipped: set SEKVENT_DOCKER_TESTS=1 to run Docker-backed tests");
    true
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
async fn racing_acquirers_get_exactly_one_lease() {
    if skip() {
        return;
    }
    // …
}
```

To run one scenario on both backends, write it as `async fn(pool: &Pool)`
and generate a `postgres` and a `mysql` test module that call it with a pool
from the respective shared harness (`lease.rs` does this with a small
`macro_rules!`).

With `cargo sekvent`, set `[harness] docker_tests = true` in `sekvent.toml`
and `gate`, `test` and `coverage` run these tests: the CLI exports
`SEKVENT_DOCKER_TESTS=1` (unless the environment sets it) and passes
`--include-ignored` to the test binaries; doctests run in a separate step
without it. `SEKVENT_DOCKER_TESTS=0` in the environment switches both off
for one run. See [CLI](../cli.md).

### Use a remote Docker daemon

Export `DOCKER_HOST` (or a `DOCKER_CONTEXT`) as usual. When the daemon's
idea of its host is wrong for you (a tunnel, a remote host), set
`TESTCONTAINERS_HOST_OVERRIDE` (`HOST_OVERRIDE_ENV`) to the host that
publishes the ports. Ports are published on all of the daemon host's
interfaces, so a test process in a sibling container reaches them through
the Docker bridge gateway. The reaper inherits the environment, so it talks
to the same daemon.

For your own containers:

```rust
pub async fn container_addr<I: Image>(container: &ContainerAsync<I>) -> Result<String, HarnessError>;
pub async fn mapped_addr<I: Image>(container: &ContainerAsync<I>, port: ContainerPort) -> Result<(String, u16), HarnessError>;
pub fn resolve_host(override_value: Option<&str>, reported: &str) -> String;   // pure; brackets IPv6
```

### Label, reap and sweep your own containers

```rust
impl Harness {
    pub fn from_env() -> Result<Self, HarnessError>;                         // SEKVENT_HARNESS_NAMESPACE, SEKVENT_TEST_RUN_ID
    pub fn with_namespace(namespace: &str) -> Result<Self, HarnessError>;
    pub fn with_run_id(namespace: &str, run_id: Option<&str>) -> Result<Self, HarnessError>;
    pub fn namespace(&self) -> &str;
    pub fn run_id(&self) -> &str;
    pub fn labels(&self) -> Vec<(String, String)>;                           // stamp these on a container
    pub fn labels_at(&self, now: SystemTime) -> Vec<(String, String)>;
    pub fn run_label_key(&self) -> String;                                   // "<ns>.run"
    pub fn created_label_key(&self) -> String;                               // "<ns>.created"
    pub fn namespace_filter(&self) -> String;                                // docker ps filter
    pub fn run_filter(&self, run_id: &str) -> String;
    pub fn sweep_run(&self, run_id: &str) -> Result<SweepReport, HarnessError>;
    pub fn sweep_stale(&self, older_than: Duration) -> Result<SweepReport, HarnessError>;
    pub fn sweep_stale_at(&self, older_than: Duration, now: SystemTime) -> Result<SweepReport, HarnessError>;
}
pub fn generate_run_id(now: SystemTime, pid: u32, random: [u8; 4]) -> String;

impl Reaper {
    pub fn spawn(container_id: &str) -> Result<Self, HarnessError>;
    pub fn pid(&self) -> u32;
    pub fn release_and_wait(self) -> Result<ExitStatus, HarnessError>;     // remove now and wait
}
```

Namespaces, run ids and container ids are validated before use: 1–128
characters of ASCII letters, digits, `.`, `-` and `_`, not starting with `-`
or `.` (`HarnessError::Invalid` otherwise). A run id defaults to
`SEKVENT_TEST_RUN_ID` (`RUN_ID_ENV`) or a generated
`<millis hex>-<pid hex>-<random hex>`. `sweep_stale` spares the harness's
own run and containers without a readable creation label; pick an age
longer than your slowest test binary. `SweepReport { removed }` lists the
removed ids. Dropping a `Reaper` closes its pipe and the container is removed
in the background.

### Clean up leftovers

```sh
cargo sekvent harness-clean --stale        # other runs' containers older than [harness].stale_after (default 6h)
cargo sekvent harness-clean --run <ID>     # one run's containers
cargo sekvent harness-clean --all --yes    # every container of the namespace
```

`cargo sekvent gate` and `coverage` also sweep stale containers before the
run and that run's own containers after it, exporting a fresh
`SEKVENT_TEST_RUN_ID` and `SEKVENT_HARNESS_NAMESPACE` to the tests. Without
Docker installed the sweeps are no-ops. Configure the namespace and age in
`sekvent.toml`:

```toml
[harness]
label_namespace = "com.example.orders.harness"   # default "io.sekvent.harness"
stale_after = "6h"
docker_tests = false
```

Sweeps and `harness-clean` act on the whole namespace, not on one project,
so pick a namespace unique to the project whenever several projects share a
Docker daemon (a CI runner, a remote build host). Tests run directly with
`cargo test` do not read `sekvent.toml`: the harness takes the namespace
from `SEKVENT_HARNESS_NAMESPACE`, so export the same value there, for
example `SEKVENT_HARNESS_NAMESPACE=com.example.orders.harness
SEKVENT_DOCKER_TESTS=1 cargo test -- --include-ignored`. Otherwise their
containers carry the default namespace, which `harness-clean` in this
project does not see and in a default-namespace project removes.
A container whose test process was killed while it was still starting has
no reaper yet; its labels make it visible to these sweeps.

### Wait for an observable condition

```rust
await_until!(repo.count().await.unwrap() == 3);
await_until!(done.load(Ordering::SeqCst), timeout = Duration::from_secs(2));
await_until!(ready().await, timeout = Duration::from_secs(30), interval = Duration::from_millis(100));
```

The condition is any `bool` expression and may contain `.await`; it is
re-evaluated every `interval` (default `DEFAULT_AWAIT_INTERVAL`, 20 ms;
at least 1 ms) until it holds, and the macro panics with the condition's
source text after `timeout` (default `DEFAULT_AWAIT_TIMEOUT`, 10 s). It
sleeps on tokio's clock, so under `tokio::time::pause` the wait is instant
and deterministic.

Use it only to observe **external** state — a row appearing, a health
endpoint flipping, a listener closing. When the code under test can signal
completion (a channel, a `JoinHandle`, `RuntimeHandle::wait`), await that
instead.

## Deterministic tests

These rules keep sekvent's own suite reproducible; they apply equally to
services.

### Time

- **No fixed sleeps.** Never `sleep(100ms)` and hope. Wait on completion
  signals: channels, `oneshot` fired from a `Drop` guard, a `Semaphore`,
  join handles, `RuntimeHandle::wait`.
- **Paused tokio time for in-process timing.** `#[tokio::test(start_paused
  = true)]` (or `tokio::time::pause()`, feature `test-util`) makes timers
  advance only when every task is idle, so timeouts, backoff, job schedules,
  lease heartbeats and caches run instantly and exactly:

  ```rust
  #[tokio::test(start_paused = true)]
  async fn retries_until_an_attempt_succeeds() {
      let start = tokio::time::Instant::now();
      // … code that backs off 50 ms, then 100 ms …
      assert_eq!(start.elapsed(), Duration::from_millis(150));
  }
  ```

  Caller deadlines in paused tests come from the tokio clock:
  `cx.with_deadline((tokio::time::Instant::now() + d).into_std())`.
- **Inject wall-clock time.** Code that reads the wall clock takes a
  `sekvent::context::Clock`; tests pass a `ManualClock` and move it:

  ```rust
  use std::time::{Duration, SystemTime};
  use sekvent::context::ManualClock;

  let clock = ManualClock::new(SystemTime::UNIX_EPOCH + Duration::from_secs(1_000));
  let auth = BearerAuth::new(keys, Validation::new().with_leeway(0), Arc::new(clock.clone()));
  clock.advance(Duration::from_secs(60));   // the token is now expired
  ```

  `ManualClock::set(time)` jumps to an exact time; clones share the time.
  Monotonic users in `sekvent::resilience` (breaker, cache) take a
  `MonotonicClock`; `TokioClock` follows paused tokio time. JWT and login
  APIs take `now_unix_secs` directly.
- **Inject randomness** (seedable RNGs) where behaviour depends on it.

### Sockets

- Bind `127.0.0.1:0` and read the assigned port; never a fixed port.
- An address nobody listens on is `127.0.0.1:1`. Never bind a port, drop
  the listener and reuse the number: another test may grab it.
- Anything over a socket runs on the **real clock**: with paused time,
  auto-advance would fire timers while loopback I/O is still pending. Assert
  lower time bounds and structure, not exact durations, and guard against
  hangs with a 30 s `tokio::time::timeout`:

  ```rust
  let handle = tokio::time::timeout(Duration::from_secs(30), builder.build()?.start())
      .await
      .expect("started within the guard")?;
  ```

- Warm lazy gRPC channels (one successful call) before asserting on
  deadlines, so connection setup does not eat the budget.
- A timed scenario that runs both in-process and over a socket: write it
  once as `async fn(profile)`, call it from a `start_paused = true` test for
  the in-process profiles and from a plain `#[tokio::test]` twin wrapped in
  the 30 s guard for the socket profile.

### Global state gets its own test binary

Each file under `tests/` is its own binary (process). A test that changes
process-wide state goes in a file of its own, so it cannot leak into tests
running in parallel threads:

- process environment (`std::env::set_var`);
- a global tracing subscriber, or a level filter (see below);
- the rustls process-wide crypto provider
  (`crates/sekvent-client/tests/crypto_provider.rs`);
- signal handlers such as Ctrl-C (`crates/sekvent-tasks/tests/interrupt.rs`).

### Capturing logs

Install a subscriber per test with `tracing::subscriber::set_default`
(thread-local, dropped with the guard), never `set_global_default`. The
telemetry `LogBuffer` makes a convenient capture layer:

```rust
use sekvent::telemetry::LogBuffer;
use tracing_subscriber::{Registry, layer::SubscriberExt};

let buffer = LogBuffer::new(256);
let _guard = tracing::subscriber::set_default(Registry::default().with(buffer.layer()));
// … run the code …
let records = buffer.snapshot();
assert!(records.iter().any(|record| record.target == "sekvent::access"));
```

Caveats:

- `set_default` covers the current thread only. Keep such tests on the
  default current-thread `#[tokio::test]` runtime; events from tasks on other
  worker threads are not captured.
- tracing keeps a **process-wide** maximum level and caches each callsite's
  interest. Installing a level-filtered subscriber (`LevelFilter::WARN`) in
  one test lowers that maximum while it is installed, which can hide `info`
  events from tests running next to it. Put such tests in their own binary,
  as `crates/sekvent-telemetry/tests/access_log_warn_filter.rs` does.
- Assert that secrets are **absent** from captured output (tokens,
  passwords, URLs), not only that the expected event is present.

### Components and fakes

Install fakes through the same fail-closed `App` builder as the real
component (`Handle::install(&mut builder, |_deps| Ok(fake))`) and run one
scenario across binding profiles with `rstest` cases. See
[components](components.md).

## Coverage gate

`cargo sekvent coverage` runs the tests instrumented and enforces a line
coverage floor per workspace package; `cargo sekvent gate` is fmt, clippy
with warnings denied and the tests. Defaults, in `sekvent.toml`:

```toml
[coverage]
fail_under_lines = 95.0                                # per package, percent
ignore = ['(^|/)src/main\.rs$', '(^|/)build\.rs$']     # extra filename regexes (the scaffold writes these)
exclude = []                                           # packages not measured

[coverage.thresholds]
"orders-proto" = 0.0                                   # per-package override
```

`tests/`, `benches/` and `examples/` under the workspace root, the target
directory and build-script output are always left out. `--misses <package>`
prints uncovered line ranges; `--lcov <path>` writes an LCOV report. With
`[harness] docker_tests = true`, coverage includes the Docker tests, which
is usually what covers your SQL. Keep `main.rs` thin so the logic it would
hold lives in a measured library.

## Pitfalls

- **`#[ignore]` without the early return** makes `cargo test -- --ignored`
  fail on machines without Docker. Use both.
- **Forgetting `create_database()`**: sharing the admin database between
  tests couples them. One database per test.
- **Leaking the URL**: `TestDatabase.url` contains the admin password. Do not
  print it in assertions; `Debug` of `TestDatabase` already hides it.
- **Polling for something you could await**: `await_until!` hides races
  that a completion signal would make explicit, and slows the suite.
- **Exact timings over sockets**: assert lower bounds only.
- **`std::thread::sleep` in async tests** blocks the runtime and defeats
  paused time.

## See also

- [Databases](db.md) — pools, leases and their Docker tests
- [Context](context.md) — `Clock`, `ManualClock`, `CallContext` deadlines
- [Telemetry](telemetry.md) — `LogBuffer`
- [Components](components.md) — fakes and binding profiles
- [CLI](../cli.md) — `gate`, `test`, `coverage`, `harness-clean`, `sekvent.toml`
