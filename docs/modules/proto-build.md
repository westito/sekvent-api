# Protobuf code generation (`sekvent-proto-build`)

`sekvent-proto-build` turns `.proto` files into Rust code from a crate's
`build.rs`. It wraps `prost-build` and `tonic-prost-build`, compiles the
protos in-process with [protox](https://docs.rs/protox) (no `protoc`
anywhere), and adds three things on top: a split between a portable
messages crate and a stub-only server crate, a single wrapper module you
`include!` once, and the service-contract constants that
`#[component(proto = …)]` checks at compile time.

The crate is build-time only. The `sekvent` facade does **not** re-export
it.

## Enable it

Add it under `[build-dependencies]`, from git or from a checkout:

```toml
[build-dependencies]
sekvent-proto-build = { git = "https://github.com/westito/sekvent-api", branch = "master" }
# or, with a submodule or local checkout:
# sekvent-proto-build = { path = "vendor/sekvent-api/crates/sekvent-proto-build" }
```

The generated code then needs these **normal** dependencies, depending on
what you generate:

| You generate | Add to `[dependencies]` |
|---|---|
| messages (always) | `prost` |
| messages that use well-known types other than `Empty` (`Timestamp`, `Duration`, `Struct`, …) | `prost-types` (prost maps `google.protobuf` to `::prost_types`) |
| gRPC server or client stubs | `tonic` and `tonic-prost` (the stubs name `tonic_prost::ProstCodec`) |
| messages for a component `-api` crate | `prost` and `sekvent-api` with the `component` feature, no tonic |

Keep `prost`, `tonic` and `tonic-prost` on the same minor release line as the
generators this crate pins (prost 0.14, tonic 0.14); generated code from one
release line does not compile against another.

`bytes([...])` maps fields to `::prost::bytes::Bytes`, which comes through
`prost`, so it needs no extra dependency.

## Quick example

A component's `-api` crate, as in `examples/shop/orders-api`:

```text
orders-api/
├── Cargo.toml
├── build.rs
├── proto/shop/orders/v1/orders.proto   # package shop.orders.v1; service Orders { … }
└── src/lib.rs
```

```rust
// orders-api/build.rs
fn main() {
    sekvent_proto_build::ProtoBuild::new("proto")
        .messages_only()
        .compile()
        .unwrap_or_else(|error| panic!("{error}"));
}
```

```rust
// orders-api/src/lib.rs
pub mod proto {
    include!(concat!(env!("OUT_DIR"), "/sekvent_protos.rs"));
}

pub use proto::shop::orders::v1::{GetOrderRequest, Order, PlaceOrderReply, PlaceOrderRequest};
```

`ProtoBuild::new("proto")` finds every `*.proto` below `proto/`, generates
the prost message types (with `prost::Name` implemented), emits the contract
constants for the `Orders` service, and writes `sekvent_protos.rs`, which
nests one module per package: `proto::shop::orders::v1`.

## Concepts

### In-process compilation, no `protoc`

The `.proto` files are parsed, resolved and type-checked by protox, a
protobuf compiler written in Rust. No build machine, CI runner or container
image needs `protoc`, and the `PROTOC` environment variable is ignored. The
well-known types (`google/protobuf/*.proto`: `timestamp`, `duration`,
`empty`, `any`, `struct`, `wrappers`, `field_mask`, `descriptor`, …) are
bundled and resolve after your proto root and include directories, so an
include directory can shadow them if it really has to.

Proto comments are kept and become Rust doc comments on the generated
items.

### Modes

Every run is in one of three modes ([`Mode`](#reference)):

| Mode | Builder call | Generates | Contract constants |
|---|---|---|---|
| combined (default) | `.both()` | messages + tonic stubs in one crate | yes |
| messages only | `.messages_only()` | prost messages only, no tonic | yes |
| services only | `.services_only("::billing_proto")` | tonic stubs only; every message is taken from the named crate | no |

Messages are compiled with prost's `enable_type_names` in combined and
messages-only mode, so every message implements `prost::Name` and `Any`
type URLs work. Components need this.

### What lands in `OUT_DIR`

- one `<package>.rs` per protobuf package, for example `billing.v1.rs`
  (`_.rs` for files without a `package`). Imported packages are generated
  too, except `google.protobuf`, which prost maps to `prost_types`;
- the **wrapper**, `sekvent_protos.rs` by default
  ([`DEFAULT_WRAPPER_FILE`](#reference)), which declares one nested
  `pub mod` per package segment and `include!`s each generated file inside
  it;
- optionally a file descriptor set (see
  [Write a file descriptor set](#write-a-file-descriptor-set)).

The wrapper puts every module under
`#[allow(clippy::all, clippy::pedantic, clippy::nursery, missing_docs, unreachable_pub, unused_qualifications, rust_2018_idioms)]`,
so a workspace that denies warnings still builds. A package segment that is
a Rust keyword becomes a raw identifier (`acme.type.v1` is
`acme::r#type::v1`); `crate`, `self`, `Self` and `super` cannot be raw
identifiers and are not supported as segments. Code from files without a
package goes into a `__root` module that the wrapper re-exports with
`pub use __root::*;`.

Include the wrapper **once**, either at the crate root or inside a module:

```rust
include!(concat!(env!("OUT_DIR"), "/sekvent_protos.rs"));      // billing_proto::billing::v1::Invoice
// or
pub mod proto {
    include!(concat!(env!("OUT_DIR"), "/sekvent_protos.rs"));  // crate::proto::billing::v1::Invoice
}
```

Never commit generated `.rs` files; they are rebuilt from the protos.

## How to …

### Generate a portable messages crate and a separate server crate

This is the layout of the `service-grpc` template (`cargo sekvent new`): the
contract sits in a workspace-level `proto/` directory, a `-proto` crate
holds the messages without any tonic dependency (usable from client
libraries and wasm), and the service crate generates only the stubs.

```text
proto/billing/v1/billing.proto
crates/billing-proto/   build.rs: messages_only()         deps: prost
crates/billing/         build.rs: services_only(…)        deps: billing-proto, tonic, tonic-prost
```

```rust
// crates/billing-proto/build.rs
fn main() {
    sekvent_proto_build::ProtoBuild::new("../../proto")
        .files(["billing/v1/billing.proto"])
        .messages_only()
        .compile()
        .unwrap_or_else(|error| panic!("{error}"));
}
```

```rust
// crates/billing-proto/src/lib.rs: include at the crate ROOT
include!(concat!(env!("OUT_DIR"), "/sekvent_protos.rs"));
```

```rust
// crates/billing/build.rs
fn main() {
    sekvent_proto_build::ProtoBuild::new("../../proto")
        .files(["billing/v1/billing.proto"])
        .services_only("::billing_proto")
        .compile()
        .unwrap_or_else(|error| panic!("{error}"));
}
```

```rust
// crates/billing/src/lib.rs
pub mod proto {
    include!(concat!(env!("OUT_DIR"), "/sekvent_protos.rs"));
}
use proto::billing::v1::invoices_server::{Invoices, InvoicesServer};
use billing_proto::billing::v1::{GetInvoiceRequest, Invoice};
```

In services-only mode, every package that the compiled files declare **or
transitively import** is mapped with prost's `extern_path`:
`.billing.v1` → `::billing_proto::billing::v1`, `.common.v1` →
`::billing_proto::common::v1`. The stubs therefore use the messages crate's
types and no message is generated twice. `google.protobuf` stays with prost
(`prost_types`). The argument is a Rust path to wherever the messages
crate's wrapper is included: `"::billing_proto"` when it is included at the
crate root, `"::billing_proto::proto"` if the messages crate wraps it in
`pub mod proto`. A trailing `::` is tolerated.

Services-only mode needs a `package` declaration in every compiled file and
every imported file (well-known types aside); otherwise it fails with
[`ProtoBuildError::Package`](#errors), because a package-less message could
not be mapped and would be generated again.

The template also writes a small `package_alias.rs` from its `build.rs`
(`pub use self::billing::v1 as api;`) and includes it next to the wrapper,
so code can write `billing_proto::api::Invoice`. That is plain template
code, using [`Compiled::out_dir`](#reference); it is not part of this crate.

### Generate everything in one crate

The default mode compiles messages and stubs together. Add `tonic`,
`tonic-prost` and `prost` to the crate's dependencies.

```rust
// build.rs
fn main() {
    sekvent_proto_build::ProtoBuild::new("proto")
        .compile()
        .unwrap_or_else(|error| panic!("{error}"));
}
```

`.both()` selects this mode explicitly, for example after a conditional
`.messages_only()`.

### Declare a component contract (`-api` crates)

A component that may cross a process boundary declares its `service` in the
`.proto` of its `-api` crate, and the `-api` crate runs `messages_only()`.
That generates the messages and the contract constants, and nothing that
needs tonic: the component's gRPC binding is a generic byte-level transport
over the component dispatch, so there is never per-component tonic code.
Keep `-api` crates free of tonic; do not use `both()` or `services_only()`
for a component's service.

```text
orders-api/                         # examples/shop/orders-api
├── Cargo.toml                      # deps: prost, sekvent-api (features = ["component"]); build-deps: sekvent-proto-build
├── build.rs                        # ProtoBuild::new("proto").messages_only().compile()
├── proto/shop/orders/v1/orders.proto
└── src/lib.rs                      # pub mod proto { include!(…) }, the trait, the error
```

```rust
#[sekvent::component(
    name = "orders",
    package = "shop.orders.v1",
    proto = "crate::proto::shop::orders::v1"
)]
pub trait Orders: Send + Sync + 'static {
    #[call(timeout = "5s")]
    async fn place_order(&self, cx: &CallContext, req: PlaceOrderRequest)
        -> Result<PlaceOrderReply, OrdersError>;
    // …
}
```

`proto = "…"` is the Rust path of the package module inside the wrapper.
How the trait and service must line up is described in
[components.md](components.md) and [../component-model.md](../component-model.md).

### Use the service-contract constants and RPC type aliases

In combined and messages-only mode, every proto `service` gets, in its
package's module:

- a constant `__sekvent_service_<Service>` (prefix
  [`SERVICE_CONTRACT_PREFIX`](#reference)) of type
  `(&str, &[(&str, &str, &str, bool)])`: the service's full name, then per
  RPC its name, the request's full protobuf name, the reply's full protobuf
  name, and whether it streams in either direction;
- per RPC, a type alias `__sekvent_rpc_<Service>__<Rpc>` (prefix
  [`RPC_TYPES_PREFIX`](#reference); service and RPC separated by a double
  underscore) of the Rust `(Request, Reply)` types.

For `service Entries { rpc GetEntry(GetEntryRequest) returns (Entry); rpc Follow(GetEntryRequest) returns (stream Entry); }`
in package `ledger.v1`:

```rust
pub const __sekvent_service_Entries: (&str, &[(&str, &str, &str, bool)]) = (
    "ledger.v1.Entries",
    &[
        ("GetEntry", "ledger.v1.GetEntryRequest", "ledger.v1.Entry", false),
        ("Follow", "ledger.v1.GetEntryRequest", "ledger.v1.Entry", true),
    ],
);
pub type __sekvent_rpc_Entries__GetEntry = (GetEntryRequest, Entry);
pub type __sekvent_rpc_Entries__Follow = (GetEntryRequest, Entry);
```

Full names keep enclosing messages (`ledger.v1.Entry.Line` for a nested
`Line`), and the aliases use the Rust path prost resolves
(`entry::Line`, `super::super::common::v1::Money`), so a nested message is
told apart from a top-level one with the same name. `google.protobuf.Empty`
keeps its name in the constant and is `()` in the alias. Both items are
`#[doc(hidden)]`, carry the lint allowances they need, and name no sekvent
crate, so the messages crate needs no sekvent dependency to compile them.

`#[component(proto = "…")]` reads `<proto>::__sekvent_service_<Trait>` and
`<proto>::__sekvent_rpc_<Trait>__<Rpc>` and fails the build unless the
service is `<package>.<Trait>`, each method maps to exactly one unary RPC
(`place_order` → `PlaceOrder`) and no RPC is left over, and the method's
request and reply types are the alias's types. You never reference these
items yourself.

Turn them off with `.service_contracts(false)`, for example in a plain gRPC
messages crate that no component uses (they are harmless if left on).
Services-only mode never emits them, because the messages crate already
has them.

### Choose which `.proto` files to compile

By default `compile` walks the proto root recursively and compiles every
`*.proto` file it finds, in sorted order, skipping hidden directories (any
directory whose name starts with `.`). To compile only some files, list
them:

```rust
sekvent_proto_build::ProtoBuild::new("../../proto")
    .files(["billing/v1/billing.proto", "billing/v1/refunds.proto"])
```

Paths are relative to the root (absolute paths are kept as they are).
`files` replaces any earlier list. Every listed file must still lie under
the root or an include directory, or protox refuses it. Listed files pull in
their imports: imported packages are generated and appear in the wrapper.

The root is also the first import path: an `import "common/v1/money.proto";`
resolves to `<root>/common/v1/money.proto`.

### Add import paths

```rust
sekvent_proto_build::ProtoBuild::new("proto")
    .include("../vendor/googleapis")
    .include("../shared-protos")
```

Imports are searched in the root first, then the include directories in the
order added, then the bundled well-known types. Files under an include
directory are compiled only when imported (discovery walks the root alone).

### Turn client or server stubs off

```rust
sekvent_proto_build::ProtoBuild::new("proto")
    .services_only("::billing_proto")
    .client(false)  // server stubs only
```

`.server(bool)` and `.client(bool)` both default to `true` and apply to the
combined and services-only modes; messages-only mode never runs tonic.
Turning both off leaves the combined mode with messages and contracts only.

### Write a file descriptor set

```rust
sekvent_proto_build::ProtoBuild::new("../billing-proto/proto")
    .services_only("::billing_proto")
    .file_descriptor_set("billing_descriptor.bin")
```

The file is written into the output directory and its path is returned in
[`Compiled::descriptor_set`](#reference). It contains the compiled files and
everything they import, with source info (comments) kept, which is what a
gRPC reflection service expects:

```rust
pub const DESCRIPTOR_SET: &[u8] =
    include_bytes!(concat!(env!("OUT_DIR"), "/billing_descriptor.bin"));
```

The file name is joined to the output directory as given, so a name with a
missing subdirectory fails with [`ProtoBuildError::Io`](#errors).

### Customize the generated types

```rust
sekvent_proto_build::ProtoBuild::new("proto")
    .messages_only()
    .bytes(["."])                                                 // every `bytes` field as Bytes
    .type_attribute(".common.v1.Money", "#[derive(serde::Serialize)]")
    .field_attribute(".billing.v1.Invoice.note", "#[doc = \"Free text.\"]")
```

- `bytes(paths)` maps `bytes` fields under these proto paths to
  `bytes::Bytes` (`["."]` for all); repeated calls add paths.
- `type_attribute(path, attribute)` and `field_attribute(path, attribute)`
  pass straight to prost's options of the same name; each call adds one
  rule. Paths are fully qualified protobuf paths with a leading dot.
- The attribute is pasted onto the generated item, so the crate that
  includes the code needs what it names: the derive above needs `serde`
  (with its `derive` feature) in that crate's `[dependencies]`, and every
  field type of `Money` must implement `Serialize` too. Do not derive what
  prost already derives: `Clone`, `PartialEq` and `Message` always, and
  `Eq`, `Hash` and `Copy` whenever every field allows them (no floats,
  maps or repeated messages, for example). Deriving one of those again
  fails with a conflicting implementation (`E0119`).

### Add your own service generator

```rust
use std::fmt::Write as _;

use sekvent_proto_build::ServiceGenerator;

struct Routes;

impl ServiceGenerator for Routes {
    fn generate(&mut self, service: prost_build::Service, buf: &mut String) {
        let _ = writeln!(
            buf,
            "pub const {}_FULL_NAME: &str = \"{}.{}\";",
            service.name.to_uppercase(),
            service.package,
            service.proto_name,
        );
    }
}

fn main() {
    sekvent_proto_build::ProtoBuild::new("proto")
        .service_generator_hook(Box::new(Routes))
        .compile()
        .unwrap_or_else(|error| panic!("{error}"));
}
```

`ServiceGenerator` is re-exported from `prost_build`. To name
`prost_build::Service` in the implementation, add `prost-build` (0.14, the
release line this crate uses) to your `[build-dependencies]`.

Generators run per service in a fixed order, all appending to the same
package file: first the contract constants (when enabled), then tonic's
stubs (when enabled), then your hooks in the order added. Their `finalize`
and `finalize_package` methods run too. Hooks also run in messages-only
mode, where tonic is off. prost reformats the generated file, which drops
plain `//` comments, so emit items (or doc attributes), not comments.

### Control where and under what name code is written

```rust
let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").expect("set by cargo");
let out = std::path::Path::new(&manifest_dir).join("generated");
std::fs::create_dir_all(&out).expect("create the output directory");
sekvent_proto_build::ProtoBuild::new("proto")
    .out_dir(&out)                 // instead of $OUT_DIR; absolute
    .wrapper_file("billing.rs")    // instead of sekvent_protos.rs
```

Without `out_dir`, `compile` uses `OUT_DIR` and fails with
[`ProtoBuildError::NoOutDir`](#errors) outside a build script. The wrapper
`include!`s each generated file by the path it wrote it to, and `include!`
resolves a relative path against the file that contains it, the wrapper
itself. A relative `out_dir` such as `"target/generated"` therefore yields
includes that never resolve, wherever the wrapper is included from: always
pass an absolute directory (`OUT_DIR` is always absolute), and create it
first.

To include a single package without the wrapper,
`tonic::include_proto!("billing.v1")` still works, without the lint
allowances. A build dependency cannot export macros to the crate it builds;
if you want a local helper, write it in the consuming crate:

```rust
macro_rules! include_proto {
    ($package:literal) => {
        include!(concat!(env!("OUT_DIR"), "/", $package, ".rs"));
    };
}
```

### Control rebuilds (`rerun-if-changed`)

By default `compile` prints `cargo:rerun-if-changed` lines so Cargo reruns
the build script exactly when the inputs change:

- every compiled `.proto` file;
- when files were discovered rather than listed, every directory visited
  (the root and its non-hidden subdirectories), so adding or removing a
  `.proto` triggers a rebuild;
- every imported file found under the root or an include directory.

The bundled well-known types are not watched. The lines for compiled files
and directories are printed before compiling, so fixing a broken proto
reruns the script.

`.emit_rerun_if_changed(false)` turns all of this off. Once a build script
prints no `rerun-if-changed` line at all, Cargo falls back to rerunning it
whenever any file in the package changes, which misses protos outside the
package (such as a workspace-level `../../proto`). Only turn it off when you
print your own lines or call `compile` outside a build script (tests).

### Inspect the result

`compile` returns [`Compiled`](#reference), whose fields you can read:

```rust
let compiled = sekvent_proto_build::ProtoBuild::new("proto")
    .messages_only()
    .compile()
    .unwrap_or_else(|error| panic!("{error}"));
// compiled.out_dir, compiled.files, compiled.packages,
// compiled.generated, compiled.wrapper, compiled.descriptor_set
```

`packages` lists the distinct packages of the compiled files and of
everything they import, sorted, without `google.protobuf` (the empty string
stands for files without a package). `generated` lists the `.rs` file
written per package; in services-only mode it holds only packages that got
stubs, since imported messages come from the messages crate.

## Errors

`compile` returns `Result<Compiled, ProtoBuildError>`. In `build.rs`, panic
with the `Display` form (`unwrap_or_else(|error| panic!("{error}"))`) so the
message names the file and line. `ProtoBuildError` is `#[non_exhaustive]`:

| Variant | When | Message starts with |
|---|---|---|
| `Protobuf { file, message }` | a `.proto` cannot be read, parsed, resolved or type-checked; an import is missing; a file is outside every import path | `protobuf compilation failed: ` then, for syntax and type errors, `file:line:column:` |
| `NoOutDir` | no `.out_dir(…)` and `OUT_DIR` unset | `OUT_DIR is not set` |
| `NoProtoFiles { root }` | discovery found no `*.proto` under the root | `no .proto files found under <root>` |
| `Package { file, reason }` | a `package` declaration is malformed, or services-only mode meets a compiled or imported file without one | `<file>: ` and the reason |
| `Io { path, source }` | a file or directory cannot be read (including a missing proto root), or the wrapper or descriptor set cannot be written | `<path>: ` and the I/O error |
| `Compile(io::Error)` | prost or tonic failed to generate or write the Rust code | `protobuf code generation failed: ` |

`file` in `Protobuf` is the file named relative to its import path when
protox knows it (`None`, for example, for a file outside every import
path). A failure before code generation leaves no wrapper behind.

## Reference

| Item | Signature / value |
|---|---|
| `ProtoBuild::new` | `fn new(proto_root: impl AsRef<Path>) -> Self`: combined mode, server and client on, every `*.proto` under the root, contracts on, rerun lines on |
| `.files` | `fn files<P: AsRef<Path>>(self, files: impl IntoIterator<Item = P>) -> Self` |
| `.include` | `fn include(self, dir: impl AsRef<Path>) -> Self` |
| `.messages_only` / `.both` | `fn messages_only(self) -> Self` / `fn both(self) -> Self` |
| `.services_only` | `fn services_only(self, messages_crate: impl Into<String>) -> Self` |
| `.server` / `.client` | `fn server(self, enable: bool) -> Self` / `fn client(self, enable: bool) -> Self` |
| `.file_descriptor_set` | `fn file_descriptor_set(self, file_name: impl Into<String>) -> Self` |
| `.bytes` | `fn bytes<S: Into<String>>(self, paths: impl IntoIterator<Item = S>) -> Self` |
| `.type_attribute` / `.field_attribute` | `fn type_attribute(self, path: impl Into<String>, attribute: impl Into<String>) -> Self` (same shape for fields) |
| `.service_generator_hook` | `fn service_generator_hook(self, generator: Box<dyn ServiceGenerator>) -> Self` |
| `.service_contracts` | `fn service_contracts(self, enable: bool) -> Self` |
| `.out_dir` | `fn out_dir(self, dir: impl AsRef<Path>) -> Self` |
| `.wrapper_file` | `fn wrapper_file(self, file_name: impl Into<String>) -> Self` |
| `.emit_rerun_if_changed` | `fn emit_rerun_if_changed(self, enable: bool) -> Self` |
| `.compile` | `fn compile(self) -> Result<Compiled, ProtoBuildError>` |
| `Compiled` | `#[non_exhaustive]` struct, public fields `out_dir: PathBuf`, `files: Vec<PathBuf>`, `packages: Vec<String>`, `generated: Vec<PathBuf>`, `wrapper: PathBuf`, `descriptor_set: Option<PathBuf>` |
| `Mode` | `#[non_exhaustive]` enum: `MessagesOnly`, `ServicesOnly { messages_crate: String }`, `Both` |
| `extern_paths` | `fn extern_paths(mode: &Mode, packages: &[String]) -> Vec<(String, String)>`: the `(".pkg", "::crate::pkg")` pairs services-only mode passes to prost; empty for the other modes |
| `read_packages` | `fn read_packages(files: &[PathBuf]) -> Result<Vec<(PathBuf, String)>, ProtoBuildError>`: each file's `package` (empty if none), ignoring comments and strings |
| `ServiceGenerator` | re-export of `prost_build::ServiceGenerator` |
| `DEFAULT_WRAPPER_FILE` | `"sekvent_protos.rs"` |
| `SERVICE_CONTRACT_PREFIX` | `"__sekvent_service_"` |
| `RPC_TYPES_PREFIX` | `"__sekvent_rpc_"` |
| `RESERVED_PACKAGE` | `"sekvent.v1"` |

## Testing tips

- Test a build configuration outside Cargo's build-script machinery by
  pointing it at a temporary directory and turning the rerun lines off:

  ```rust
  let out = tempfile::tempdir().unwrap();
  let compiled = sekvent_proto_build::ProtoBuild::new("proto")
      .messages_only()
      .out_dir(out.path())
      .emit_rerun_if_changed(false)
      .compile()
      .unwrap();
  assert_eq!(compiled.packages, ["billing.v1", "common.v1"]);
  ```

  Then read the generated files or decode the descriptor set with
  `prost_types::FileDescriptorSet::decode`.
- Compilation needs no `protoc`, so these tests run the same on a laptop,
  in CI and in a container.
- Generated code is excluded from meaningful coverage; the `service-grpc`
  template sets the `-proto` crate's coverage threshold to 0.
- For a component, the real test is that the `-api` crate compiles: a
  mismatch between trait and service is a compile error. Wire-compatibility
  of the protos over time is checked by `cargo sekvent contract emit` and
  `contract check` (see [../cli.md](../cli.md)).

## Pitfalls

- **Wrong messages-crate path in services-only mode.** The path must lead to
  where the messages crate *includes the wrapper*. Including it inside
  `pub mod proto` while passing `"::billing_proto"` yields unresolved
  `::billing_proto::billing::v1::…` paths. The template includes it at the
  crate root for this reason.
- **Package-less files with services-only.** Every compiled and imported
  file needs a `package`.
- **Generating the same package in two crates** creates two unrelated sets
  of types. Generate messages once and use `services_only` (or depend on the
  messages crate) everywhere else.
- **tonic in an `-api` crate.** Component contracts use `messages_only()`;
  the gRPC binding needs no generated stubs.
- **Turning rerun lines off** makes Cargo miss proto edits outside the
  package.
- **A custom hook that writes comments**: prost's formatting drops them;
  write items.
- **Including the wrapper twice in the same module** duplicates every item.
- **Reserved names.** The package `sekvent.v1` is reserved for the
  framework's own messages, and nothing in a proto may be named
  `__sekvent_*`. The builder does not reject them; a clash shows up as a
  compile error in the generated code or in the component checks.
- **`protoc` settings do nothing.** `PROTOC` and `PROTOC_INCLUDE` are not
  read; add import paths with `.include(…)`.

## See also

- [components.md](components.md): `#[component(proto = …)]` and how traits map to services
- [../component-model.md](../component-model.md): the component model overview
- [server.md](server.md): serving tonic services next to HTTP
- [error.md](error.md): mapping `AppError` to gRPC status
- [link.md](link.md): service tokens for gRPC calls between services
- [../cli.md](../cli.md): `cargo sekvent new` (the `service-grpc` template) and `contract emit|check`
- [../getting-started.md](../getting-started.md), [../features.md](../features.md), [../README.md](../README.md)
