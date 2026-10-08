# `cargo sekvent` reference

`cargo sekvent` is the command line tool of sekvent. It scaffolds
workspaces and services, runs the quality gate and coverage, keeps the
sekvent dependency and its third-party pins current, checks protobuf
contracts and installs CI templates and agent skills. The binary is
`cargo-sekvent`; cargo runs it as `cargo sekvent <command>`, and
`cargo-sekvent <command>` works too. `cargo sekvent --help` and
`cargo sekvent <command> --help` print the same information in short form.

Installation is covered in [Getting started](getting-started.md#install-the-cli).

- [Where commands run](#where-commands-run)
- [Commands](#commands): [gate](#cargo-sekvent-gate), [check, clippy,
  test](#cargo-sekvent-check-clippy-test), [coverage](#cargo-sekvent-coverage),
  [boundaries](#cargo-sekvent-boundaries), [contract](#cargo-sekvent-contract),
  [harness-clean](#cargo-sekvent-harness-clean), [run](#cargo-sekvent-run),
  [new](#cargo-sekvent-new), [init](#cargo-sekvent-init), [add](#cargo-sekvent-add),
  [deps](#cargo-sekvent-deps), [sdk](#cargo-sekvent-sdk), [ci](#cargo-sekvent-ci-generate),
  [agents](#cargo-sekvent-agents), [skills](#cargo-sekvent-skills-install),
  [self-update](#cargo-sekvent-self-update), [config](#cargo-sekvent-config-show),
  [other names](#unknown-and-reserved-commands)
- [Exit codes](#exit-codes)
- [Environment variables](#environment-variables)
- [`sekvent.toml` reference](#sekventtoml-reference)

## Where commands run

Commands that compile Rust are **compiling commands**. By default they are
not run on your machine but forwarded to a remote builder:
`cargo sekvent gate` becomes `rrb run sekvent gate`, executed in the project
root. `rrb` syncs the project's git file set to the builder and runs the
`[run.commands].sekvent` entry of `.remote-build.toml`, which is
`.sekvent/run.sh`. That script installs `cargo-sekvent` at exactly the
sekvent revision `Cargo.lock` pins (once per revision, into
`$CARGO_HOME/sekvent-cli/<rev>`) and runs the command there, where it
executes locally. The exit code of `rrb` becomes the exit code of the
command.

A compiling command runs on the current machine instead when any of these
holds, checked in this order:

1. `RRB_CONTAINER` is set (the builder's containers set it);
2. `CI` is set;
3. `SEKVENT_LOCAL` is set;
4. `[remote].mode = "local"` in `sekvent.toml`.

For the three variables, any value except empty, `0`, `false`, `no` and
`off` (case-insensitive, surrounding whitespace ignored) counts as set, so
`CI=false` does not switch to local mode.

There is no automatic fallback: when forwarding is selected and `rrb` is
missing or fails, the command fails and says how to opt into local builds.

| Runs | Commands |
|---|---|
| Compiling: forwarded unless local | `gate`, `check`, `clippy`, `test`, `coverage`, `harness-clean`, `run <task>` for tasks with `remote = true`, and commands forwarded to an `xtask` package |
| Always on this machine, writes files | `new`, `init`, `add`, `deps sync`, `sdk update` (writes `Cargo.lock`), `ci generate`, `agents`, `skills install`, `self-update`, `contract emit` |
| Always on this machine, read-only | `deps check`, `sdk status`, `config show`, `boundaries` (reads `cargo metadata`), `contract check` (compiles protos in-process, not Rust), `run` without a task, `run <task>` for tasks without `remote` |

`cargo fmt --all` (the writing form) is not a `cargo sekvent` command; run it
yourself on your machine.

Commands that need a project find it by walking up from the current
directory to the nearest `sekvent.toml`; that directory is the **project
root**. `deps` and `sdk` fall back to the nearest Cargo workspace root when
there is no `sekvent.toml`. `new` and `init` do not need one.

## Commands

### `cargo sekvent gate`

```text
cargo sekvent gate
```

The full quality gate over the selected packages (every workspace member
except `[gate].exclude`). Compiling. Steps, in order; the first failing step
stops the gate and its exit code becomes the result:

| Step | Runs | When |
|---|---|---|
| `pre_gate[n]` | each `[hooks].pre_gate` command | always |
| `fmt` | `cargo fmt --check --all`, or `-p <member>` for each selected member when something is excluded | `[gate].fmt = true` (default) |
| `clippy` | `cargo clippy --workspace [--exclude …] --no-deps --all-targets <features> --locked -- -D warnings <clippy_args>` | always |
| `test` | `cargo test --workspace [--exclude …] <features> --locked` | always |
| `doctest` | `cargo test … --doc` | only when ignored tests are selected (see below) |
| `doc` | `cargo doc --workspace [--exclude …] --no-deps <features> --locked` with `RUSTDOCFLAGS=-Dwarnings` | `[gate].doc = true` |
| `boundaries` | the [boundary check](#cargo-sekvent-boundaries) | `[[gate.boundaries]]` present |
| `contract` | the [contract check](#cargo-sekvent-contract), in-process | `[contract].roots` set and `[contract].gate = true` |
| `post_gate[n]` | each `[hooks].post_gate` command | only after every step passed |

`<features>` is `--all-features`, nothing, or `--features a,b`, from
`[gate].features`. Every cargo command passes `--locked`, so `Cargo.lock`
must exist and be current.

Test runs get `RUST_TEST_THREADS=[gate].test_threads` unless the environment
already sets it. With `[harness].docker_tests = true` (and
`SEKVENT_DOCKER_TESTS` not set to something other than `1` or `true`), they
also get `SEKVENT_DOCKER_TESTS=1` and `--include-ignored`; the test step then
selects `--lib --bins --tests --examples` (no `--lib` when no selected member
has a library) and doctests run in the separate `doctest` step without the
flag, so rustdoc never runs a code block marked `ignore`.

Around the steps:

- Before anything, the gate prints a one-line hint when the locked sekvent
  revision is behind its branch head (`git ls-remote`, best effort, at most
  5 s; skipped when `SEKVENT_NO_UPDATE_CHECK` is non-empty, offline, or for a
  non-git source).
- A harness session starts: containers of the `[harness].label_namespace`
  older than `[harness].stale_after` are removed, and one fresh
  `SEKVENT_TEST_RUN_ID` is generated for the whole invocation; every command
  of it gets that id and `SEKVENT_HARNESS_NAMESPACE`. After the steps, pass
  or fail, that run's containers are removed. Without Docker
  installed, the sweeps do nothing.
- Ctrl-C stops after the current step with exit code 130.

On success it prints `==> gate: ok`.

### `cargo sekvent check`, `clippy`, `test`

```text
cargo sekvent check
cargo sekvent clippy
cargo sekvent test [-- <test binary args>…]
```

One gate step on its own, over the same package selection and features.
Compiling.

- `check`: `cargo check --workspace [--exclude …] --all-targets <features> --locked`.
- `clippy`: the gate's clippy step (warnings denied, `clippy_args` appended).
- `test`: the gate's test steps, in a harness session. Everything after `--`
  goes to the test binaries, e.g. `cargo sekvent test -- --nocapture` or
  `cargo sekvent test -- my_test_name`. Passing `--ignored` or
  `--include-ignored` yourself suppresses the automatic `--include-ignored`;
  with `--ignored` the doctest step is skipped.

### `cargo sekvent coverage`

```text
cargo sekvent coverage [--lcov PATH] [--misses PACKAGE]
```

| Flag | Meaning |
|---|---|
| `--lcov PATH` | Also write an LCOV report to `PATH` (relative to the current directory; written where the command runs) |
| `--misses PACKAGE` | Print the uncovered line ranges of `PACKAGE`, which must be a selected workspace member |

Instrumented tests and per-package line coverage floors. Compiling. Needs
`cargo-llvm-cov` and the `llvm-tools-preview` component where it runs.
Steps, in a harness session:

1. `[hooks].pre_coverage` commands;
2. `cargo llvm-cov clean --workspace`;
3. `cargo llvm-cov --workspace [--exclude …] <features> --locked --no-report`,
   with the same test environment and `--include-ignored` rule as the gate;
4. `cargo llvm-cov report --json` (summary only unless `--misses`) with the
   ignore filter;
5. with `--lcov`, `cargo llvm-cov report --lcov`.

It then prints a table with one row per selected package (minus
`[coverage].exclude`) and its floor (`[coverage.thresholds]`, else
`[coverage].fail_under_lines`). A package without any instrumented line
passes; when no package has any, the command fails and shows the filter. The
`[hooks].post_coverage` commands run after the table. The command exits 1
when any package is below its floor.

Always left out of the report: files under `tests/`, `benches/` and
`examples/` inside the workspace, the target directory and build-script
output (`OUT_DIR`). `[coverage].ignore` adds more patterns.

### `cargo sekvent boundaries`

```text
cargo sekvent boundaries
```

Checks every `[[gate.boundaries]]` rule against the dependency graph from
`cargo metadata`: package `from` must not depend on `to`, directly or
transitively, counting normal dependencies only (dev and build dependencies
are ignored). Each violation is reported with the dependency path that
breaks it, and a `to` that matches no package in the graph is flagged as a
possible typo. A `from` that is not a workspace member is an error. Prints
`boundaries: none configured` when there are no rules. Runs on this machine;
it does not compile. The gate runs the same check.

### `cargo sekvent contract`

```text
cargo sekvent contract emit [SERVICE…]
cargo sekvent contract check [SERVICE…]
```

Protobuf service contracts for components that cross a process boundary.
Both subcommands compile every `.proto` below each `[contract].roots`
directory in-process (no `protoc`, no cargo build) and treat every
`service` there as a contract. `SERVICE` is a full service name such as
`billing.v1.Billing`; without names, all services are used.

- `emit` writes one canonical JSON baseline per service,
  `<baseline>/<service>.json`, and prints `wrote <path>` for each. Baselines
  of other services are left alone. Commit them.
- `check` compares each baseline with the current protos and prints one
  line per wire-breaking change and per service without a baseline, then a
  summary. A baseline whose service disappeared is a breaking change. Exit 1
  on any finding or missing baseline, else 0.

Both fail with `no [contract] roots in sekvent.toml` when no roots are
configured, and when a root declares no service or a service is defined in
two roots. See [components](modules/components.md) and
[docs/design/component-c2.md](design/component-c2.md).

### `cargo sekvent harness-clean`

```text
cargo sekvent harness-clean --run ID
cargo sekvent harness-clean --stale
cargo sekvent harness-clean --all --yes
```

Removes test containers that `sekvent-testing` started, selected by the
labels of `[harness].label_namespace` only. Exactly one scope is required:

| Flag | Removes |
|---|---|
| `--run ID` | The containers of one run (`SEKVENT_TEST_RUN_ID`) |
| `--stale` | Containers older than `[harness].stale_after` |
| `--all` | Every container of the namespace; refused without `--yes` |
| `--yes` | Confirms `--all` |

Compiling (forwarded), because the containers live where the tests ran.
Prints `harness-clean: removed N container(s)`.

### `cargo sekvent run`

```text
cargo sekvent run
cargo sekvent run TASK [ARGS…]
```

Without a task, lists the `[tasks.<name>]` of `sekvent.toml` with their
descriptions (`(remote)` marks remote tasks). With a task, runs its `run`
argv in the project root with `ARGS` appended verbatim (arguments starting
with `-` are passed through too). No shell is involved. A task with
`remote = true` is a compiling command and is forwarded like the gate.

### `cargo sekvent new`

```text
cargo sekvent new NAME [--kind grpc|http|worker] [--dir PATH] [--sekvent-path PATH]
                       [--ci github|bitbucket|none] [--no-git]
```

Creates a new workspace with one service.

| Argument | Default | Meaning |
|---|---|---|
| `NAME` | required | Project name and first service crate name: 1–64 characters of `a-z`, `0-9` and `-`, starting with a letter, not ending with `-`, no `--` |
| `--kind` | `http` | `grpc`, `http` or `worker` |
| `--dir PATH` | `./NAME` | Target directory; must not exist or be empty |
| `--sekvent-path PATH` | — | Write path dependencies on this sekvent checkout (it must contain `crates/sekvent`) instead of git dependencies on `master` |
| `--ci` | `github` | `github` writes `.github/workflows/gate.yml`, `bitbucket` writes `bitbucket-pipelines.yml`, `none` writes neither |
| `--no-git` | off | Do not run `git init` |

Prints every written file and the next steps (`cargo generate-lockfile`,
`cargo sekvent gate`). [Getting started](getting-started.md#what-was-generated)
explains each generated file.

### `cargo sekvent init`

```text
cargo sekvent init [--kind grpc|http|worker] [--force]
```

Puts the Cargo workspace at or above the current directory (the nearest
`Cargo.toml` with a `[workspace]` table) on sekvent:

- writes `sekvent.toml`, with `[project].name` derived from the directory
  name, and `.sekvent/run.sh`;
- creates `.remote-build.toml`, or adds `[run.commands].sekvent =
  [".sekvent/run.sh"]` to an existing one (keeping its comments and layout;
  a different existing value is reported and replaced only with `--force`);
- inserts or refreshes the managed sekvent section of `AGENTS.md`.

Existing `sekvent.toml` and `run.sh` are kept unless `--force`. `init` does
not edit `Cargo.toml`; add the sekvent dependencies yourself. `--kind`
(default `http`) only picks the suggested next command,
`cargo sekvent add <name> --kind …`. Without a workspace it fails and points
to `cargo sekvent new`.

### `cargo sekvent add`

```text
cargo sekvent add NAME --kind grpc|http|worker [--force]
```

Renders a service into the project: `crates/NAME` (plus, for `grpc`,
`crates/NAME-proto` and `proto/NAME/v1/NAME.proto` with `-` in `NAME`
replaced by `_` in the proto path). `--kind` is required. Without `--force`
it refuses when a crate directory already exists; with it, files are
rendered over existing ones. New crate directories are added to
`[workspace].members` unless an existing pattern (an exact path, `dir/*` or
`*`) covers them. The project name comes from `sekvent.toml`.

### `cargo sekvent deps`

```text
cargo sekvent deps check [--strict]
cargo sekvent deps sync [--only A,B]
```

Compares the root `[workspace.dependencies]` with the third-party versions
sekvent pins in its own root manifest. Those pins are compiled into the CLI,
so they are the pins of the sekvent revision your `cargo-sekvent` was built
from; `self-update` first if you want the latest. Entries named `sekvent-*`,
and git or path entries without a version, are not compared.

`check` prints one line per finding:

| Kind | Meaning |
|---|---|
| `older` | Your version is lower than the pin |
| `newer` | Your version is higher than the pin |
| `feature-mismatch` | You lack features sekvent enables |
| `missing` | You do not declare the dependency |
| `unparsed` | Your requirement is not a plain version (a range or wildcard) |

It exits 0, or with `--strict` exits 1 when anything is `older` or
`missing`.

`sync` rewrites `Cargo.toml` in place, keeping comments and formatting: sets
versions to the pins, appends missing features and adds missing
dependencies verbatim. It never removes a dependency or a feature you added.
`--only tokio,serde` limits it to those names. Afterwards run
`cargo update --workspace` to refresh `Cargo.lock`.

### `cargo sekvent sdk`

```text
cargo sekvent sdk status [--remote]
cargo sekvent sdk update
```

- `status` reads `Cargo.lock` and prints the locked `sekvent-api` version,
  source and commit. With `--remote` it also asks the repository for the
  branch head (`git ls-remote`, never prompting) and prints `up to date` or
  `behind`.
- `update` runs `cargo update -p sekvent-api -p sekvent-testing -p
  sekvent-proto-build` (for those present in `Cargo.lock`) in the workspace
  root. It moves the locked sekvent revision only; it does not compile and
  does not change third-party pins (use `deps`).

### `cargo sekvent ci generate`

```text
cargo sekvent ci generate github|bitbucket [--force]
```

Writes the gate-only CI template: `.github/workflows/gate.yml` or
`bitbucket-pipelines.yml`. Both run `.sekvent/run.sh gate` and
`.sekvent/run.sh coverage` with `CI=true` and `SEKVENT_DOCKER_TESTS=1`, pin
the Rust toolchain to sekvent's (the channel compiled into the running CLI,
which is also what `new` writes into `rust-toolchain.toml`; edit the file if
your project uses another one) and cache the per-revision CLI builds. The
environment variable alone does not run the container-backed tests: the
gate and coverage add `--include-ignored` only with `[harness].docker_tests
= true`, which the generated `sekvent.toml` leaves `false`. Existing files
are replaced only with `--force`.

### `cargo sekvent agents`

```text
cargo sekvent agents [--file PATH]
```

Inserts or refreshes the sekvent section of `AGENTS.md` (default: in the
project root; `--file` picks another file, created if missing). Only the
text between `<!-- sekvent:begin -->` and `<!-- sekvent:end -->` is
replaced; without markers the section is appended. A file with an opening
marker but no closing one is left alone and reported with its line number.

### `cargo sekvent skills install`

```text
cargo sekvent skills install [--dest DIR]
```

Copies the agent skills shipped with this CLI (`sekvent`,
`sekvent-new-project`, `sekvent-migrate`) into `~/.kodein/skills`, plus
`~/.claude/skills` when that directory exists, or only into `--dest DIR`.
Only directories whose names start with `sekvent` are written or replaced.

### `cargo sekvent self-update`

```text
cargo sekvent self-update
```

Replaces the running binary with the latest build of the rolling
`cli-latest` release for the target it was built for: downloads
`cargo-sekvent-<target>.tar.gz` and its `.sha256` with `curl`, verifies the
digest with `sha256sum` or `shasum`, unpacks with `tar` and swaps the
executable atomically. A checksum mismatch aborts.

### `cargo sekvent config show`

```text
cargo sekvent config show
```

Prints the effective `sekvent.toml` with every default filled in.

### Unknown and reserved commands

- `component`, `extract`, `queue` and `schedule` are reserved for the
  component model: they print a notice and exit 2.
- Any other unknown command is forwarded to the workspace's `xtask` package
  as `cargo run -q -p xtask -- <command> <args…>`, as a compiling command.
- Without an `xtask` member (or outside a project), the CLI prints
  `unknown command` and the help, and exits 2.

## Exit codes

| Code | Meaning |
|---|---|
| `0` | Success |
| the step's code | A gate, test or coverage step failed; its command's exit code is passed through |
| `1` | Coverage below a floor, a breaking or missing contract, `deps check --strict` findings, or a CLI error (printed as `error: …`) |
| `2` | Usage error, unknown command or reserved command |
| `130` | Interrupted with Ctrl-C |

When forwarded, the exit code is that of `rrb`, which reports the remote
command's code.

## Environment variables

Read by the CLI:

| Variable | Effect |
|---|---|
| `CI` | Enabled: compiling commands run locally |
| `SEKVENT_LOCAL` | Enabled: compiling commands run locally |
| `RRB_CONTAINER` | Enabled: already inside the builder, run locally |
| `SEKVENT_NO_UPDATE_CHECK` | Any non-empty value skips the gate's "sekvent moved on" hint |
| `SEKVENT_DOCKER_TESTS` | With `[harness].docker_tests`: left as is when set; a value other than `1`/`true` turns the container tests and `--include-ignored` off for one run |
| `RUST_TEST_THREADS` | Left as is when set; otherwise set from `[gate].test_threads` |
| `HOME` | Expands `~` in `[remote].rrb`; base of the default skill directories |

Set by the CLI for the commands it runs:

| Variable | Value |
|---|---|
| `SEKVENT_TEST_RUN_ID` | A fresh id per gate, test or coverage run; containers are labelled with it |
| `SEKVENT_HARNESS_NAMESPACE` | `[harness].label_namespace` |
| `SEKVENT_DOCKER_TESTS` | `1` when `[harness].docker_tests` and not already set |
| `RUST_TEST_THREADS` | `[gate].test_threads` when not already set |
| `RUSTDOCFLAGS` | `-Dwarnings` for the `doc` step |

Read by the scripts around the CLI:

| Variable | Read by | Effect |
|---|---|---|
| `SEKVENT_INSTALL_DIR` | `scripts/install.sh` | Install directory (default `$CARGO_HOME/bin`) |
| `CARGO_HOME` | `install.sh`, `.sekvent/run.sh` | Default install directory; root of the per-revision CLI builds (`$CARGO_HOME/sekvent-cli/`) |
| `SEKVENT_SRC` | `.sekvent/run.sh` | Path of a sekvent checkout, required when the project depends on sekvent by path |
| `TESTCONTAINERS_HOST_OVERRIDE` | `sekvent-testing` | Host at which test containers are reached (used by the Bitbucket template) |

## `sekvent.toml` reference

`sekvent.toml` sits in the project root. Every table rejects unknown keys,
so a typo is a load error naming the file and position. Only
`[project].name` is required; `cargo sekvent config show` prints the
effective values. Paths are relative to the project root, and hooks and
tasks run there.

### `[project]`

| Key | Type | Default | Meaning |
|---|---|---|---|
| `name` | string | required, non-empty | Project name; used by `add`, `ci generate` and `agents` when rendering templates |

### `[remote]`

| Key | Type | Default | Meaning |
|---|---|---|---|
| `mode` | `"rrb"` \| `"local"` | `"rrb"` | `"rrb"` forwards compiling commands to the remote builder; `"local"` runs them on this machine |
| `rrb` | string | `"~/.kodein/skills/build-on-rtx/bin/rrb"` | Path of the `rrb` executable; a leading `~` is the home directory, a bare name (no `/`) is looked up on `PATH`. Must not be empty |

### `[gate]`

| Key | Type | Default | Meaning |
|---|---|---|---|
| `exclude` | array of strings | `[]` | Workspace packages left out of fmt, clippy, test, doc and coverage. Each must be a workspace member, and at least one member must remain |
| `features` | `"all"` \| `"default"` \| array of strings | `"all"` | `--all-features`, the packages' default features, or `--features a,b`, for check, clippy, test, doc and coverage. List entries must not be empty |
| `test_threads` | integer ≥ 1 | `8` | `RUST_TEST_THREADS` for test and coverage runs, unless the environment sets it |
| `clippy_args` | array of strings | `[]` | Extra clippy arguments after `-- -D warnings` |
| `fmt` | bool | `true` | Run `cargo fmt --check` in the gate |
| `doc` | bool | `false` | Run `cargo doc --no-deps` with `RUSTDOCFLAGS=-Dwarnings` in the gate |

### `[[gate.boundaries]]`

Zero or more dependency rules, checked by the gate and
`cargo sekvent boundaries`.

| Key | Type | Default | Meaning |
|---|---|---|---|
| `from` | string | required | The workspace member the rule constrains |
| `to` | string | required | A package `from` must not reach through normal dependencies, directly or transitively; must differ from `from` |

### `[coverage]`

| Key | Type | Default | Meaning |
|---|---|---|---|
| `fail_under_lines` | float, 0–100 | `95.0` | Line coverage floor in percent for every measured package |
| `ignore` | array of regexes | `[]` | Extra filename patterns left out of the report, matched as written against absolute paths; each must be a valid regex |
| `exclude` | array of strings | `[]` | Packages that are built and tested but not measured |

### `[coverage.thresholds]`

A table of package name to floor (float, 0–100) overriding
`fail_under_lines` for that package. Default empty. An entry for a package
that does not exist has no effect.

### `[harness]`

| Key | Type | Default | Meaning |
|---|---|---|---|
| `label_namespace` | string | `"io.sekvent.harness"` | Label namespace stamped on, and used to select, test containers; must be a valid namespace for `sekvent-testing` |
| `stale_after` | duration string | `"6h"` | Age (humantime: `30m`, `6h`, `2d`) after which other runs' containers are swept before a gate, test or coverage run, and by `harness-clean --stale` |
| `docker_tests` | bool | `false` | Run the container-backed (`#[ignore]`d) tests in gate, test and coverage: exports `SEKVENT_DOCKER_TESTS=1` unless set and adds `--include-ignored`. Needs Docker where the tests run |

### `[hooks]`

Each key is an array of argv arrays, run in the project root without a
shell; the first element must not be empty. A failing hook fails the run.
The program is looked up on `PATH`. Under `.sekvent/run.sh` (CI and the
remote builder) the CLI runs from its per-revision root, which is not on
`PATH`, so a hook that calls `cargo sekvent …` works only where
`cargo-sekvent` is also installed normally; prefer plain commands or a
script in the repository (`["bash", "scripts/check.sh"]`).
| Key | Default | Runs |
|---|---|---|
| `pre_gate` | `[]` | Before the first gate step |
| `post_gate` | `[]` | After the last gate step, only when every step passed |
| `pre_coverage` | `[]` | Before the coverage run |
| `post_coverage` | `[]` | After the coverage table is printed |

### `[contract]`

The feature is off while `roots` is empty.

| Key | Type | Default | Meaning |
|---|---|---|---|
| `roots` | array of paths | `[]` | Directories whose `.proto` files (recursively) are compiled; every `service` in them is a contract. No empty or repeated paths |
| `includes` | array of paths | `[]` | Extra import directories for every root. No empty or repeated paths |
| `baseline` | path | `"contracts"` | Directory of the committed JSON baselines; must not be empty |
| `gate` | bool | `true` | Run `contract check` as a gate step when `roots` is not empty |

### `[tasks.<name>]`

Custom tasks for `cargo sekvent run <name>`. The name must not be empty or
contain whitespace.

| Key | Type | Default | Meaning |
|---|---|---|---|
| `run` | array of strings | required, first element non-empty | The argv to run; arguments given to `cargo sekvent run` are appended |
| `remote` | bool | `false` | The task compiles: forward it to the remote builder like the gate |
| `description` | string | none | One line shown when listing tasks |

### Annotated example

```toml
[project]
name = "orders"                     # required

[remote]
mode = "local"                      # no remote builder: compile here
# rrb = "rrb"                       # or: forward through an rrb found on PATH

[gate]
exclude = ["vendored-sdk"]          # left out of every step
features = "all"                    # or "default", or ["postgres", "metrics"]
test_threads = 8
clippy_args = ["-A", "clippy::too_many_lines"]
fmt = true
doc = true                          # also deny rustdoc warnings

[[gate.boundaries]]                 # the portable proto crate must stay free of tonic
from = "orders-proto"
to = "tonic"

[[gate.boundaries]]
from = "orders-domain"
to = "sqlx"

[coverage]
fail_under_lines = 95.0
ignore = ['(^|/)src/main\.rs$', '(^|/)build\.rs$']
exclude = []

[coverage.thresholds]
"orders-proto" = 0.0                # generated code
"orders-cli" = 80.0

[harness]
label_namespace = "io.sekvent.harness"
stale_after = "6h"
docker_tests = true                 # run the Postgres-backed tests in the gate

[hooks]
pre_gate = [["bash", "scripts/check-migrations.sh"]]
post_coverage = [["echo", "coverage done"]]

[contract]
roots = ["crates/orders-api/proto"]
includes = ["third_party/proto"]
baseline = "contracts"
gate = true

[tasks.migrate]
run = ["cargo", "run", "-p", "orders", "--", "migrate"]
remote = false
description = "Apply database migrations against ORDERS_DB_URL"

[tasks.bench]
run = ["cargo", "bench", "-p", "orders-domain"]
remote = true
description = "Benchmarks on the builder"
```

The `sekvent.toml` that `cargo sekvent new` generates writes out the
defaults, with two differences from the built-in ones: `[coverage].ignore`
leaves out `src/main.rs` and `build.rs`, and `[coverage.thresholds]` sets
the first service's `-proto` crate to `0.0`.
