# Jobs (`sekvent::runtime::JobSpec`)

Jobs are periodic or on-demand background work run by the
[runtime](runtime.md): every N minutes, at the times of a cron pattern (UTC),
or only when triggered. Each job is one supervised unit with a fixed
cadence, no overlapping runs, a per-run timeout, drain-aware cancellation,
a handle for "run now" and status, and optional *singleton* execution across
replicas through a guard such as a database lease. A failed or panicking run
is logged and recorded; the next tick still comes.

## Enable it

| | |
|---|---|
| Facade feature | `runtime` (on by default) |
| Cron schedules | `runtime-cron` (`JobSpec::cron`, UTC) |
| Database singletons | `db-lease` plus `runtime` and a sqlx backend (`LeaseGuard`, see [Database](db.md)) |
| Module | `use sekvent::runtime::{JobSpec, JobContext, JobHandle, JobStatus, JobState, RunTrigger, CancelReason, TriggerErrorKind, JobGuard, JobPermit, TokioWallClock};` |
| Reasons | `sekvent::runtime::reasons` |
| Prelude | `JobSpec`, `JobContext` |

## Quick example

```rust
use std::time::Duration;

use sekvent::prelude::*;

let cleanup = JobSpec::interval(Duration::from_secs(600)) // start + 10 min, + 20 min, …
    .jitter(Duration::from_secs(30))                      // below the interval
    .timeout(Duration::from_secs(120));
let reindex = JobSpec::manual();                          // only when triggered
let reindex_now = reindex.handle();                       // give it to the admin API

let runtime = Runtime::builder()
    .job("sessions-cleanup", Stage::Workers, cleanup, {
        let repo = repo.clone();
        move |cx: JobContext| {
            let repo = repo.clone();
            async move { repo.delete_expired(&cx.call_context()).await }
        }
    })
    .job("search-reindex", Stage::Workers, reindex, move |cx: JobContext| search.clone().reindex(cx))
    .build()?;

// Later, in an admin handler:
let started = reindex_now.trigger().await.map_err(AppError::from)?;
tracing::info!(run_id = %started.run_id, "reindex started");
```

## Concepts

### A job is a unit

`RuntimeBuilder::job(name, stage, spec, run)` registers one unit with policy
`UnitPolicy::Critical`:

```rust
pub fn job<F, Fut>(self, name: impl Into<String>, stage: Stage, spec: JobSpec, run: F) -> Self
where
    F: Fn(JobContext) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<(), AppError>> + Send + 'static;
```

- The unit reports ready at once, so a job never holds up its stage. Jobs
  usually live in `Stage::Workers`: they start before ingress opens and drain
  after it has stopped.
- The unit **never fails because a run failed**. An `Err`, a panic, a timeout
  or a lost guard is logged, recorded in `JobStatus`, and the next tick still
  runs. Failed runs are **not retried**; the next tick is the retry.
- `run` is called once per run with a fresh `JobContext`. It is a `Fn`, so
  clone captured state into each future.
- The job's unit cannot be restarted (it is critical, so a second start is an
  `INTERNAL` error, `job <name> cannot be started twice`).

### Schedules

| Constructor | Ticks | Clock |
|---|---|---|
| `JobSpec::interval(period)` | `start + initial_delay + k × period`; the first run comes one period after the unit starts unless `initial_delay` says otherwise | tokio's clock |
| `JobSpec::interval(period).singleton(guard)` | the multiples of `period` since the Unix epoch (`:00`, `:15`, `:30`, `:45` for 15 min), the same on every instance | the job's wall clock |
| `JobSpec::cron(pattern)?` (`runtime-cron`) | the occurrences of the pattern in **UTC**: five fields, or six with leading seconds; no year field | the job's wall clock |
| `JobSpec::manual()` | none; runs only through `JobHandle::trigger` | — |

The cadence is **fixed**: ticks do not move with how long runs take. A
pattern that does not parse fails `JobSpec::cron` with `INVALID_ARGUMENT`
naming the pattern. `spec.schedule()` returns the `Schedule`
(`Interval(Duration)`, `Cron(String)`, `Manual`; `#[non_exhaustive]`).

Wall-clock schedules (cron and singleton intervals) never sleep more than a
minute at a time, so clock corrections are noticed.

### What happens at a tick

When a tick is due (at its time plus its jitter), the job decides:

1. **Overlap.** A run of this job is still in progress here (or a guard is
   being asked): the tick is **skipped**. Runs never overlap.
2. **Misfire.** The tick starts later than `misfire_grace` (default 1 min,
   at least 1 s) after its jittered time: it is **skipped**.
3. Otherwise it **runs**; a singleton first asks its guard.

Skipped ticks count in `JobStatus::skipped` and are logged (overlaps at
`debug`, misfires at `info`).

### Coalescing after stalls and clock jumps

If the process stalls or the wall clock jumps, many ticks may be due at
once. They never produce a burst:

- After a tick was handled on time, every further tick due by now is
  **passed over**: one run per stall, then the regular cadence resumes.
- After a misfire, ticks that are also beyond the grace are passed over and
  the oldest tick still within the grace runs (a tick exactly at the grace
  still runs).

Passed-over ticks count as skipped and each pass-over logs one `info` event
(`ticks passed over: …`, with `passed_over`, `first_tick_ms`,
`last_tick_ms`). Example: a 10 s interval stalled for 100 s with the default
grace runs once (for the tick 60 s late), then continues at the next regular
tick; nine ticks are counted as skipped.

### Catch-up for singletons

A guarded job checks once, when its unit starts, whether it missed a tick
while no instance was running: it asks the guard for the last tick any
instance started (`JobGuard::last_tick`). If the latest tick at or before now
is newer (or there is no record) **and** still within the misfire grace, it
runs that tick once with `RunTrigger::CatchUp`. Older ticks never run. If the
last tick cannot be read in time, there is no catch-up (logged at `warn`).

### Jitter

`jitter(max)` delays each scheduled run by a uniform random amount in
`[0, max]`, drawn per tick. The ticks themselves do not move, `tick()` still
reports the scheduled time, and catch-up and manual runs are never delayed.
For interval schedules the jitter must be below the period; for cron at most
1 h. `jitter_rng(rng)` replaces the generator (any `rand` 0.10 `Rng`; seeded
from the thread RNG by default).

## How to …

### Configure a spec

| Method | Default | Notes |
|---|---|---|
| `jitter(max)` | none | see above |
| `jitter_rng(rng)` | thread-seeded `SmallRng` | for deterministic tests |
| `initial_delay(d)` | one period for in-process intervals, zero otherwise | earliest start after the unit starts; anchors in-process intervals. `Duration::ZERO` runs an interval job at once. |
| `timeout(limit)` | none | cancel and drop a run that takes longer; it fails with `DEADLINE_EXCEEDED` |
| `misfire_grace(d)` | 1 min | how late a tick may start before it is skipped; **at least 1 s** (timers fire slightly late, so a shorter grace would misfire every tick) |
| `singleton(guard)` | none | run on one instance at a time, as the guard decides; intervals become epoch-aligned |
| `clock(Arc<dyn Clock>)` | `SystemClock` | the wall clock for cron and singleton ticks, and for the times in `JobStatus` |
| `handle()` | — | a `JobHandle`, usable before registration |

### Validation rules

`RuntimeBuilder::build` fails with `INVALID_ARGUMENT`, naming the job, when:

- the name is not 1–100 characters of `A–Z a–z 0–9 . _ : -`;
- an interval is zero;
- a singleton interval is below 1 s or not a whole number of milliseconds;
- the jitter is not below the interval (interval schedules) or exceeds 1 h
  (cron);
- a cron pattern has no occurrence ahead;
- the timeout is zero;
- the misfire grace is below 1 s;
- a manual job has a jitter, an initial delay or a misfire grace.

Job names must also be unique among all unit names.

### Write the run

`JobContext` is what one run gets (cheap to clone):

| Method | Returns |
|---|---|
| `job()` | the registered name |
| `run_id()` | a fresh UUID v7 per run, also on the run's `job` span |
| `trigger()` | `RunTrigger::Schedule`, `CatchUp` or `Manual` (`#[non_exhaustive]`) |
| `tick()` | `Option<SystemTime>`: the scheduled time of a scheduled or catch-up run |
| `fence()` | `Option<u64>`: the guard's fencing token, for a singleton run |
| `is_cancelled()` / `cancelled().await` | whether / when the run was cancelled |
| `cancel_token()` | a child `CancellationToken` for tasks the run spawns |
| `cancel_reason()` | `Option<CancelReason>`, set before the token fires |
| `call_context()` | a root `CallContext`: request id = run id, the run's cancellation, the timeout as deadline |

Pass `cx.call_context()` to everything the run calls, so clients, database
calls and components see the deadline and the cancellation, and logs
correlate by the run id:

```rust
async fn sync_orders(cx: JobContext, repo: OrdersRepo, upstream: UpstreamClient) -> Result<(), AppError> {
    let ctx = cx.call_context();
    let mut cursor = repo.sync_cursor(&ctx).await?;
    loop {
        if cx.is_cancelled() {
            return Ok(());                       // drain, timeout or lost lease: stop between batches
        }
        let page = upstream.changes(&ctx, &cursor).await?;
        if page.is_empty() {
            return Ok(());
        }
        cursor = repo.apply(&ctx, page).await?;
    }
}
```

### Understand cancellation

`CancelReason` (`#[non_exhaustive]`) says why the run's token fired; the
first reason wins:

| Reason | When | What happens to the run | Outcome |
|---|---|---|---|
| `Shutdown` | the job's stage drains | it may finish until the unit's stop deadline, then the unit is aborted | whatever the run returns |
| `Timeout` | the run passed `timeout` | the token fires, then the future is **dropped** at once | `DEADLINE_EXCEEDED`, reason `JOB_TIMED_OUT` |
| `LeaseLost` | the guard withdrew the right to run | the token fires, then the future is **dropped** at once | `ABORTED`, reason `LEASE_LOST` |

A timeout or a lost guard wins over a run that ends at the same moment. When
either has already happened by the time the run would start, the closure is
never called. A panicking run ends as `INTERNAL`, logged with reason
`JOB_PANICKED`.

On drain, check `cx.is_cancelled()` between batches, or race
`cx.cancelled()` in a `select!`, and return promptly. A run that ignores the
token is aborted at the stop deadline: the unit is reported `Aborted` and
code after its current `.await` never runs.

### Trigger runs and read status

```rust
let spec = JobSpec::manual();
let handle: JobHandle = spec.handle();     // before or after registration; cheap to clone

match handle.trigger().await {
    Ok(started) => tracing::info!(run_id = %started.run_id, fence = ?started.fence, "started"),
    Err(error) => match error.kind() {
        TriggerErrorKind::AlreadyRunning => { /* a run is in progress here */ }
        TriggerErrorKind::HeldElsewhere => { /* another instance holds the singleton */ }
        TriggerErrorKind::NotRunning => { /* not started, draining or stopped */ }
        TriggerErrorKind::GuardFailed(code) => { /* the guard failed with `code` */ }
        _ => {}
    },
}
```

`trigger()` starts a run now and resolves once it has started
(`RunStarted { run_id, fence }`), or with a `TriggerError`. It works for
every schedule, not just manual jobs:

- A manual run gets no jitter and never shifts the schedule; a tick that
  falls while it runs is an overlap and is skipped.
- For a singleton the guard is asked without a tick and gets at most 5 s to
  answer; past that the trigger fails with
  `GuardFailed(DEADLINE_EXCEEDED)`.
- Triggers wait in a small queue (16) while the job is asking its guard;
  triggers still queued when the unit stops answer `NotRunning`.

`TriggerError` has `job()` (empty before registration) and `kind()`, a
readable `Display` (`job reindex is already running`), and converts into
`AppError`:

| `TriggerErrorKind` | `AppError` code | Reason | Metadata |
|---|---|---|---|
| `AlreadyRunning` | `FAILED_PRECONDITION` | `JOB_ALREADY_RUNNING` | `job` |
| `HeldElsewhere` | `FAILED_PRECONDITION` | `JOB_HELD_ELSEWHERE` | `job` |
| `NotRunning` | `UNAVAILABLE` | `JOB_NOT_RUNNING` | `job` |
| `GuardFailed(code)` | `UNAVAILABLE` | `JOB_GUARD_FAILED` | `job`, `guard_code` |

`handle.status()` returns a `JobStatus` snapshot (`#[non_exhaustive]`):

| Field | Meaning |
|---|---|
| `state: JobState` | `NotStarted`, `Idle`, `Running` or `Stopped` |
| `current: Option<JobRun>` | the run in progress |
| `last: Option<JobRunOutcome>` | the last finished run: `run`, `finished_at`, `error: Option<ErrorCode>` (`None` for `Ok`) |
| `next_tick: Option<SystemTime>` | the next scheduled tick on the job's wall clock |
| `runs` | runs started |
| `failures` | runs that returned an error, panicked, timed out or lost their guard |
| `skipped` | ticks that did not run here: overlaps, misfires, passed-over ticks, ticks held by another instance, ticks the guard kept failing on |

`JobRun` holds `run_id`, `trigger`, `tick`, `started_at` and `fence`.
`handle.name()` is `None` until the spec is registered with `.job(..)`.
Status is per process: another instance's runs are not visible here.

### Run a job on one instance (singletons)

`singleton(guard)` makes the job ask a `JobGuard` before every run. With the
database lease guard from [Database](db.md):

```rust
use sekvent::db::{LeaseGuard, LeaseStore};

let store = LeaseStore::new(pools.get("orders_db")?);
store.verify_schema().await?;

let spec = JobSpec::interval(Duration::from_secs(15 * 60))   // :00, :15, :30, :45 UTC on every instance
    .singleton(LeaseGuard::new(store.clone()));              // lease named after the job, TTL 30 s
let builder = builder.job("orders-sync", Stage::Workers, spec, move |cx: JobContext| sync(cx, store.clone()));
```

Each tick then runs on exactly one instance; the fence reaches the run as
`cx.fence()` for fencing writes, a lost lease cancels the run with
`LeaseLost`, and the lease is released after the run. `LeaseGuard` details
(TTL, shared lease names, fencing checks) are on the [Database](db.md) page.

How the job uses its guard:

- **Scheduled and catch-up ticks**: `acquire(job, Some(tick))`. `Ok(None)`
  means another instance holds the job or the tick already ran: the tick is
  skipped. An error is retried every `misfire_grace / 4` (between 100 ms and
  5 s) until the tick's grace runs out, then the tick is skipped (`warn`).
  An attempt still unanswered at the end of the grace fails with
  `DEADLINE_EXCEEDED`; a permit that arrives after the grace is released and
  the tick skipped.
- **Manual triggers**: `acquire(job, None)` once, at most 5 s.
- **Catch-up**: `last_tick(job)` once at start, within the grace.
- After every run the permit's release runs, for at most 5 s; a release
  that hangs is left to the guard (for a lease, its expiry).
- A guard that panics counts as a guard error (`INTERNAL`, reason
  `GUARD_PANICKED`), never a crash.

### Write your own guard

```rust
use std::time::SystemTime;

use futures::future::BoxFuture;
use sekvent::error::AppError;
use sekvent::runtime::{JobGuard, JobPermit};
use tokio_util::sync::CancellationToken;

struct LockServiceGuard {
    locks: LockClient,
}

impl JobGuard for LockServiceGuard {
    fn acquire<'a>(
        &'a self,
        job: &'a str,
        tick: Option<SystemTime>,
    ) -> BoxFuture<'a, Result<Option<JobPermit>, AppError>> {
        Box::pin(async move {
            let Some(lock) = self.locks.try_lock(job, tick).await? else {
                return Ok(None);                         // held elsewhere, or this tick already ran
            };
            let lost: CancellationToken = lock.lost_token();
            let fence = Some(lock.generation());
            Ok(Some(JobPermit::new(fence, lost, move || -> BoxFuture<'static, ()> {
                Box::pin(async move { lock.unlock().await })
            })))
        })
    }

    fn last_tick<'a>(&'a self, job: &'a str) -> BoxFuture<'a, Result<Option<SystemTime>, AppError>> {
        Box::pin(async move { self.locks.last_tick(job).await })
    }
}
```

`JobGuard` and `JobPermit` use `futures::future::BoxFuture` and
`tokio_util::sync::CancellationToken` in their signatures, and the facade
re-exports neither, so a crate implementing a guard needs `futures` (0.3) and
`tokio-util` (0.7) in its own manifest (the workspace pins `futures` 0.3.34
and `tokio-util` 0.7.19).

- `acquire` takes the job for **one run**. For a scheduled or catch-up run
  it must also remember `tick`, so the same tick never runs twice across
  instances, and return `Ok(None)` when it already ran.
- `last_tick` returns the scheduled time of the last tick any instance
  started (`None` when unknown), for catch-up.
- `JobPermit::new(fence, lost, release)`: `fence` is an optional fencing
  token handed to the run, `lost` fires when the right to run is withdrawn
  (the run is cancelled with `LeaseLost` and dropped), and `release` runs
  once after the run. Dropping a permit without releasing it drops the
  release closure; the guard's own cleanup must take over.
- Without a guard a job runs on every instance.

### Read the logs

Each run is wrapped in a `job` span with `job`, `run_id` and `trigger`
(`schedule`, `catch_up`, `manual`). Inside it:

| Event | Level | Fields |
|---|---|---|
| `job run started` | `info` | |
| `job run finished` | `info` | `duration_ms` |
| `job run failed` | `warn` | `duration_ms`, `code`, `reason`, `error` (the error's message) |
| `job run panicked` | `error` | `duration_ms`, `code` (`INTERNAL`), `reason` (`JOB_PANICKED`), `panic` |

Outside runs: skipped ticks (`tick skipped: …`, `debug` for overlaps and
ticks held elsewhere, `info` for misfires and late permits, `warn` when the
guard kept failing), pass-overs (`info`), catch-up (`info`), guard panics
(`error`, `GUARD_PANICKED`). Tick times appear as `tick_ms` (Unix
milliseconds).

## Reason constants

`sekvent::runtime::reasons`, as `AppError::reason()` or log `reason` values:

| Constant | Meaning |
|---|---|
| `JOB_ALREADY_RUNNING` | a trigger arrived while a run was in progress here |
| `JOB_HELD_ELSEWHERE` | the guard answered that another instance holds the job |
| `JOB_NOT_RUNNING` | the job's unit is not running: not started, draining or stopped |
| `JOB_GUARD_FAILED` | the guard failed to answer a trigger |
| `GUARD_PANICKED` | the guard panicked; counted as a guard error |
| `JOB_TIMED_OUT` | a run exceeded its timeout and was dropped |
| `JOB_PANICKED` | a run panicked |
| `LEASE_LOST` | the guard withdrew the right to run and the run was dropped |

## Testing

Jobs are fully deterministic on tokio's paused clock:

```rust
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use sekvent::runtime::{JobContext, JobSpec, RunTrigger, Runtime, Stage, TokioWallClock};
use tokio::sync::mpsc;
use tokio::time::Instant;

#[tokio::test(start_paused = true)]
async fn the_cleanup_runs_every_ten_minutes() {
    let t0 = Instant::now();
    let (tx, mut runs) = mpsc::unbounded_channel();
    let spec = JobSpec::interval(Duration::from_secs(600))
        .clock(Arc::new(TokioWallClock::new(SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000))));
    let job = spec.handle();
    let handle = Runtime::builder()
        .without_signals()
        .job("cleanup", Stage::Workers, spec, move |cx: JobContext| {
            let tx = tx.clone();
            async move {
                tx.send((Instant::now(), cx.trigger())).unwrap();
                Ok(())
            }
        })
        .build()
        .unwrap()
        .start()
        .await
        .unwrap();

    let (at, trigger) = runs.recv().await.unwrap();
    assert_eq!(at, t0 + Duration::from_secs(600));
    assert_eq!(trigger, RunTrigger::Schedule);
    assert_eq!(job.status().runs, 1);

    handle.shutdown();
    handle.wait().await.unwrap();
}
```

- **`#[tokio::test(start_paused = true)]`** and a runtime built with
  **`without_signals()`**. Paused time auto-advances to the next timer, so a
  ten-minute interval takes no wall time; assert exact instants against
  `tokio::time::Instant`.
- **Report from the run through channels** (`mpsc`, `oneshot`, `Notify`)
  rather than polling; a drop guard inside the run tells you when and why
  (`cx.cancel_reason()`) it was dropped.
- **Wall-clock schedules** (cron, singletons) and the `tick()` /
  `JobStatus` times read the job's `clock`. `TokioWallClock::new(start)`
  reads `start` plus the tokio time elapsed since it was created, so under
  `pause` cron and singleton ticks advance exactly with the paused clock.
- **Clock jumps**: use `sekvent::context::ManualClock` (`new`, `advance`,
  `set`) as the job's clock and move it yourself; combine with
  `tokio::time::advance` to model a stalled process.
- **Jitter**: `jitter_rng(..)` with a seeded or fixed `rand` 0.10 generator
  makes the delays exact.
- **Singletons**: implement a small in-memory `JobGuard` that scripts its
  answers (grant, `Ok(None)`, an error, never answering) and counts
  releases; the runtime's bounds (grace, 5 s trigger cap, 5 s release cap)
  then show up as exact paused-clock instants.
- **Triggers** are accepted as soon as the job's unit starts, which can be
  before `start()` returns (later stages may still be starting); before the
  unit starts, and once it has stopped, they answer `NotRunning`. After
  `start()` has returned they are accepted until the job's stage drains.
- Real `LeaseGuard` tests need a database container; see
  [Testing](testing.md).

## Pitfalls

- **The first interval run comes one period after start**, not at start.
  Use `.initial_delay(Duration::ZERO)` for "run now, then every N".
- **Cron is UTC**, always. `0 0 2 * * *` is 02:00 UTC, whatever the host's
  time zone.
- **Six cron fields start with seconds**: `0 30 2 * * *` is 02:30:00 UTC;
  `30 2 * * *` (five fields) is the same time without the seconds field.
- **Failed runs are not retried.** Retry inside the run (see
  [Resilience](resilience.md)) or let the next tick do it.
- **A run that ignores cancellation** still stops at the stop deadline, as an
  aborted unit, and its cleanup never runs.
- **Timeouts drop the future.** Code after an `.await` that was pending at the
  timeout does not run; use transactions or fencing so a dropped run leaves
  consistent state.
- **Singleton intervals change shape.** Adding `.singleton(..)` aligns ticks
  to the Unix epoch on the wall clock instead of the unit's start.
- **`misfire_grace` also bounds the guard.** A slow lease store that answers
  after the grace skips the tick.
- **Error messages are logged** in `job run failed`; keep secrets out of
  `AppError` messages (as everywhere).
- **Status is local.** `JobStatus` on one instance does not show runs that
  happened on another.

## See also

- [Runtime](runtime.md): units, stages, stop deadlines and shutdown.
- [Database](db.md): `LeaseStore`, `LeaseGuard` and fencing tokens.
- [Call context](context.md): `Clock`, `ManualClock`, `CallContext`.
- [Resilience](resilience.md): retrying inside a run.
- [Design notes: service essentials](../design/p8-service-essentials.md).
