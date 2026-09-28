//! build.rs helpers for prost/tonic code generation.
//!
//! Add this crate under `[build-dependencies]` and drive it from `build.rs`.
//! It covers the layout where message types live in one portable crate
//! (usable without tonic, for example from wasm or from a client library)
//! and gRPC stubs live in the server crate, which refers back to the
//! messages instead of generating them twice.
//!
//! # Messages crate
//!
//! ```no_run
//! // billing-proto/build.rs
//! sekvent_proto_build::ProtoBuild::new("proto")
//!     .messages_only()
//!     .compile()
//!     .unwrap_or_else(|error| panic!("{error}"));
//! ```
//!
//! ```ignore
//! // billing-proto/src/lib.rs
//! include!(concat!(env!("OUT_DIR"), "/sekvent_protos.rs"));
//! // now `billing_proto::billing::v1::Invoice` exists
//! ```
//!
//! Messages are compiled with prost's `enable_type_names`, so every message
//! implements `prost::Name` and `Any` type URLs work.
//!
//! # Server crate
//!
//! ```no_run
//! // billing-server/build.rs
//! sekvent_proto_build::ProtoBuild::new("../billing-proto/proto")
//!     .services_only("::billing_proto")
//!     .client(false)
//!     .file_descriptor_set("billing_descriptor.bin")
//!     .compile()
//!     .unwrap_or_else(|error| panic!("{error}"));
//! ```
//!
//! Every package the compiled files declare or (transitively) import is
//! mapped with prost's `extern_path`, `.billing.v1` to
//! `::billing_proto::billing::v1`, so the generated stubs use the messages
//! crate's types. `google.protobuf` is left to prost, which maps it to
//! `prost_types`. The server crate needs `tonic` and `tonic-prost` as
//! dependencies; the stubs name `tonic_prost::ProstCodec`.
//!
//! # Rebuilds
//!
//! Unless [`ProtoBuild::emit_rerun_if_changed`] turns it off, the build
//! script is rerun when a compiled file, a file it imports from the proto
//! root or an [`include`](ProtoBuild::include) directory, a discovered
//! directory or `PROTOC` changes. Files served by protoc's own include
//! directory (the `google/protobuf` well-known types) are not watched.
//!
//! # Including generated code
//!
//! [`ProtoBuild::compile`] writes a wrapper (default `sekvent_protos.rs`) that
//! declares one nested module per package and `include!`s the generated file
//! under `#[allow(clippy::all, clippy::pedantic, missing_docs, ...)]`, so a
//! workspace that denies warnings still builds. Including the wrapper once
//! is enough.
//!
//! To include a single package instead, `tonic::include_proto!("billing.v1")`
//! works as usual (without the lint allowances). A build dependency cannot
//! export macros to the crate being built, so if you want a local helper,
//! copy this one:
//!
//! ```ignore
//! macro_rules! include_proto {
//!     ($package:literal) => {
//!         include!(concat!(env!("OUT_DIR"), "/", $package, ".rs"));
//!     };
//! }
//! ```
//!
//! # Service generator hooks
//!
//! [`ProtoBuild::service_generator_hook`] adds a
//! [`prost_build::ServiceGenerator`] that runs after tonic's for every
//! service, appending to the same generated file. Hooks also run in
//! messages-only mode, where tonic's generator is off.
//!
//! # Component contracts
//!
//! In messages-only and combined mode, every proto `service` also gets a
//! constant `__sekvent_service_<Service>` in its package's module: the
//! service's full name and, per RPC, the RPC name, the request and reply
//! full names and whether it streams. Every RPC also gets a type alias
//! `__sekvent_rpc_<Service>__<Rpc>` of its Rust `(Request, Reply)` types
//! (`google.protobuf.Empty` is `()`), which tells a nested message from a
//! top-level one of the same name. `#[component(proto = "…")]` reads both
//! to check at compile time that a component trait matches its proto
//! service. The generated code names no sekvent crate.
//! [`ProtoBuild::service_contracts`] turns it off; services-only mode never
//! emits it, since the messages crate already has it.
//!
//! # Reserved names
//!
//! The proto package `sekvent.v1` is reserved for the framework's own
//! messages. Do not declare it in application protos, and do not name
//! anything in a proto package `__sekvent_*`.
//!
//! # protoc
//!
//! `protoc` is taken from the `PROTOC` environment variable, else from
//! `PATH`. When it cannot be run, [`ProtoBuild::compile`] fails with
//! [`ProtoBuildError::ProtocMissing`], which says how to install it.

#![forbid(unsafe_code)]

mod contract;
mod discover;
mod generator;
mod wrapper;

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;

pub use prost_build::ServiceGenerator;

/// The proto package reserved for sekvent itself.
pub const RESERVED_PACKAGE: &str = "sekvent.v1";

/// Default file name of the generated wrapper module.
pub const DEFAULT_WRAPPER_FILE: &str = "sekvent_protos.rs";

/// Prefix of the service contract constants, `__sekvent_service_<Service>`.
pub const SERVICE_CONTRACT_PREFIX: &str = "__sekvent_service_";

/// Prefix of the per-RPC type aliases, `__sekvent_rpc_<Service>__<Rpc>`.
pub const RPC_TYPES_PREFIX: &str = "__sekvent_rpc_";

/// The well-known types' package; prost maps it to `prost_types` itself.
const WELL_KNOWN_PACKAGE: &str = "google.protobuf";

/// Why code generation failed.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ProtoBuildError {
    /// `protoc` could not be run.
    #[error(
        "could not run protoc (`{protoc}`). Set the PROTOC environment variable to the protoc \
         binary, or install it: macOS `brew install protobuf`, Debian/Ubuntu \
         `apt-get install protobuf-compiler`, Alpine `apk add protobuf-dev`, others \
         https://github.com/protocolbuffers/protobuf/releases"
    )]
    ProtocMissing {
        /// The executable that was tried.
        protoc: String,
    },
    /// Neither an explicit output directory nor `OUT_DIR` is set.
    #[error("OUT_DIR is not set; call from build.rs or set ProtoBuild::out_dir")]
    NoOutDir,
    /// No `.proto` files were given or found.
    #[error("no .proto files found under {}", root.display())]
    NoProtoFiles {
        /// The proto root that was searched.
        root: PathBuf,
    },
    /// A file's `package` declaration could not be read.
    #[error("{}: {reason}", file.display())]
    Package {
        /// The offending file.
        file: PathBuf,
        /// What is wrong with it.
        reason: &'static str,
    },
    /// Reading or writing a file failed.
    #[error("{}: {source}", path.display())]
    Io {
        /// The path involved.
        path: PathBuf,
        /// The underlying error.
        #[source]
        source: std::io::Error,
    },
    /// prost or protoc reported an error.
    #[error("protobuf compilation failed: {0}")]
    Compile(#[source] std::io::Error),
}

/// What to generate.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Mode {
    /// prost message types only, with type names enabled.
    MessagesOnly,
    /// tonic stubs only; messages come from the crate at the given path.
    ServicesOnly {
        /// Rust path of the messages crate, e.g. `::billing_proto`.
        messages_crate: String,
    },
    /// Messages and stubs in one crate.
    Both,
}

/// The result of a successful [`ProtoBuild::compile`].
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Compiled {
    /// Where the code was written.
    pub out_dir: PathBuf,
    /// The `.proto` files compiled.
    pub files: Vec<PathBuf>,
    /// The distinct packages declared by those files and by every file they
    /// import, sorted, without `google.protobuf`. A file without a package
    /// contributes the empty string.
    pub packages: Vec<String>,
    /// The generated `.rs` files, one per package.
    pub generated: Vec<PathBuf>,
    /// The wrapper module including every generated file.
    pub wrapper: PathBuf,
    /// The file descriptor set, if requested.
    pub descriptor_set: Option<PathBuf>,
}

/// A code-generation run, configured builder-style.
#[allow(
    clippy::struct_excessive_bools,
    reason = "each flag is an independent builder toggle, not a state machine"
)]
pub struct ProtoBuild {
    root: PathBuf,
    files: Vec<PathBuf>,
    includes: Vec<PathBuf>,
    mode: Mode,
    server: bool,
    client: bool,
    descriptor_set: Option<String>,
    bytes: Vec<String>,
    type_attributes: Vec<(String, String)>,
    field_attributes: Vec<(String, String)>,
    hooks: Vec<Box<dyn ServiceGenerator>>,
    service_contracts: bool,
    out_dir: Option<PathBuf>,
    wrapper_file: String,
    emit_rerun: bool,
    protoc: Option<PathBuf>,
}

impl ProtoBuild {
    /// Generate from the `.proto` files under `proto_root`, which is also the
    /// import root. Defaults: [`Mode::Both`], server and client stubs on,
    /// every `*.proto` below the root.
    pub fn new(proto_root: impl AsRef<Path>) -> Self {
        Self {
            root: proto_root.as_ref().to_path_buf(),
            files: Vec::new(),
            includes: Vec::new(),
            mode: Mode::Both,
            server: true,
            client: true,
            descriptor_set: None,
            bytes: Vec::new(),
            type_attributes: Vec::new(),
            field_attributes: Vec::new(),
            hooks: Vec::new(),
            service_contracts: true,
            out_dir: None,
            wrapper_file: DEFAULT_WRAPPER_FILE.to_owned(),
            emit_rerun: true,
            protoc: None,
        }
    }

    /// Compile only these files (relative to the root, or absolute) instead
    /// of discovering every `*.proto`.
    #[must_use]
    pub fn files<P: AsRef<Path>>(mut self, files: impl IntoIterator<Item = P>) -> Self {
        self.files = files
            .into_iter()
            .map(|file| self.root.join(file.as_ref()))
            .collect();
        self
    }

    /// An extra import path, e.g. a vendored `googleapis` checkout.
    #[must_use]
    pub fn include(mut self, dir: impl AsRef<Path>) -> Self {
        self.includes.push(dir.as_ref().to_path_buf());
        self
    }

    /// Generate prost messages only, for a portable messages crate.
    #[must_use]
    pub fn messages_only(mut self) -> Self {
        self.mode = Mode::MessagesOnly;
        self
    }

    /// Generate tonic stubs only, taking every message from the crate at
    /// `messages_crate` (for example `::billing_proto`).
    #[must_use]
    pub fn services_only(mut self, messages_crate: impl Into<String>) -> Self {
        self.mode = Mode::ServicesOnly {
            messages_crate: messages_crate.into(),
        };
        self
    }

    /// Generate messages and stubs together (the default).
    #[must_use]
    pub fn both(mut self) -> Self {
        self.mode = Mode::Both;
        self
    }

    /// Whether to generate server stubs (default on).
    #[must_use]
    pub fn server(mut self, enable: bool) -> Self {
        self.server = enable;
        self
    }

    /// Whether to generate client stubs (default on).
    #[must_use]
    pub fn client(mut self, enable: bool) -> Self {
        self.client = enable;
        self
    }

    /// Also write a file descriptor set named `file_name` into the output
    /// directory, for gRPC reflection.
    #[must_use]
    pub fn file_descriptor_set(mut self, file_name: impl Into<String>) -> Self {
        self.descriptor_set = Some(file_name.into());
        self
    }

    /// Map `bytes` fields under these proto paths to `bytes::Bytes`
    /// (`["."]` for all).
    #[must_use]
    pub fn bytes<S: Into<String>>(mut self, paths: impl IntoIterator<Item = S>) -> Self {
        self.bytes.extend(paths.into_iter().map(Into::into));
        self
    }

    /// Add an attribute to generated types matching `path`.
    #[must_use]
    pub fn type_attribute(mut self, path: impl Into<String>, attribute: impl Into<String>) -> Self {
        self.type_attributes.push((path.into(), attribute.into()));
        self
    }

    /// Add an attribute to generated fields matching `path`.
    #[must_use]
    pub fn field_attribute(
        mut self,
        path: impl Into<String>,
        attribute: impl Into<String>,
    ) -> Self {
        self.field_attributes.push((path.into(), attribute.into()));
        self
    }

    /// Run `generator` for every service after tonic's generator, appending
    /// to the same output.
    #[must_use]
    pub fn service_generator_hook(mut self, generator: Box<dyn ServiceGenerator>) -> Self {
        self.hooks.push(generator);
        self
    }

    /// Emit the service constants `#[component(proto = …)]` checks (default
    /// on; only in messages-only and combined mode).
    #[must_use]
    pub fn service_contracts(mut self, enable: bool) -> Self {
        self.service_contracts = enable;
        self
    }

    /// Write into `dir` instead of `OUT_DIR`.
    #[must_use]
    pub fn out_dir(mut self, dir: impl AsRef<Path>) -> Self {
        self.out_dir = Some(dir.as_ref().to_path_buf());
        self
    }

    /// Name of the wrapper module file (default [`DEFAULT_WRAPPER_FILE`]).
    #[must_use]
    pub fn wrapper_file(mut self, file_name: impl Into<String>) -> Self {
        self.wrapper_file = file_name.into();
        self
    }

    /// Whether to print `cargo:rerun-if-changed` lines (default on).
    #[must_use]
    pub fn emit_rerun_if_changed(mut self, enable: bool) -> Self {
        self.emit_rerun = enable;
        self
    }

    /// Use this `protoc` instead of `PROTOC` or `PATH`.
    #[must_use]
    pub fn protoc(mut self, executable: impl AsRef<Path>) -> Self {
        self.protoc = Some(executable.as_ref().to_path_buf());
        self
    }

    /// Run protoc and prost, then write the wrapper module.
    pub fn compile(self) -> Result<Compiled, ProtoBuildError> {
        let out_dir = match &self.out_dir {
            Some(dir) => dir.clone(),
            None => std::env::var_os("OUT_DIR")
                .map(PathBuf::from)
                .ok_or(ProtoBuildError::NoOutDir)?,
        };
        let (files, watched_dirs) = if self.files.is_empty() {
            discover::discover(&self.root)?
        } else {
            (self.files.clone(), Vec::new())
        };
        if files.is_empty() {
            return Err(ProtoBuildError::NoProtoFiles {
                root: self.root.clone(),
            });
        }
        let file_packages = read_packages(&files)?;
        if matches!(self.mode, Mode::ServicesOnly { .. })
            && let Some((file, _)) = file_packages.iter().find(|(_, package)| package.is_empty())
        {
            return Err(ProtoBuildError::Package {
                file: file.clone(),
                reason: "services-only generation needs a package declaration to map messages",
            });
        }

        let protoc = self
            .protoc
            .clone()
            .unwrap_or_else(prost_build::protoc_from_env);
        check_protoc(&protoc)?;

        let emit_rerun = self.emit_rerun;
        if emit_rerun {
            for line in rerun_lines(&files, &watched_dirs) {
                println!("{line}");
            }
        }

        let descriptor_set = self.descriptor_set.as_ref().map(|name| out_dir.join(name));
        let wrapper = out_dir.join(&self.wrapper_file);
        let mut includes = vec![self.root.clone()];
        includes.extend(self.includes.iter().cloned());
        let mode = self.mode.clone();
        let mut config = self.prost_config(&protoc, &out_dir, descriptor_set.as_deref());
        let descriptors = config
            .load_fds(&files, &includes)
            .map_err(ProtoBuildError::Compile)?;
        let described: Vec<(String, String)> = descriptors
            .file
            .iter()
            .map(|file| (file.name().to_owned(), file.package().to_owned()))
            .collect();
        if matches!(mode, Mode::ServicesOnly { .. }) {
            check_imported_packages(&described)?;
        }
        if emit_rerun {
            for line in changed_lines(&imported_files(&described, &includes, &files)) {
                println!("{line}");
            }
        }
        let packages = descriptor_packages(&described);
        for (proto_path, rust_path) in extern_paths(&mode, &packages) {
            config.extern_path(proto_path, rust_path);
        }
        config
            .compile_fds(descriptors)
            .map_err(ProtoBuildError::Compile)?;

        let generated: Vec<(String, PathBuf)> = packages
            .iter()
            .map(|package| (package.clone(), out_dir.join(generated_file_name(package))))
            .filter(|(_, file)| file.exists())
            .collect();
        std::fs::write(&wrapper, wrapper::render(&generated)).map_err(|source| {
            ProtoBuildError::Io {
                path: wrapper.clone(),
                source,
            }
        })?;

        Ok(Compiled {
            out_dir,
            files,
            packages,
            generated: generated.into_iter().map(|(_, file)| file).collect(),
            wrapper,
            descriptor_set,
        })
    }

    fn prost_config(
        self,
        protoc: &Path,
        out_dir: &Path,
        descriptor_set: Option<&Path>,
    ) -> prost_build::Config {
        let mut config = prost_build::Config::new();
        config.protoc_executable(protoc);
        config.out_dir(out_dir);
        if !matches!(self.mode, Mode::ServicesOnly { .. }) {
            config.enable_type_names();
        }
        if !self.bytes.is_empty() {
            config.bytes(&self.bytes);
        }
        for (path, attribute) in &self.type_attributes {
            config.type_attribute(path, attribute);
        }
        for (path, attribute) in &self.field_attributes {
            config.field_attribute(path, attribute);
        }
        if let Some(path) = descriptor_set {
            config.file_descriptor_set_path(path);
        }
        let mut generators: Vec<Box<dyn ServiceGenerator>> = Vec::new();
        if emits_service_contracts(&self.mode, self.service_contracts) {
            generators.push(Box::new(contract::ServiceContracts));
        }
        if !matches!(self.mode, Mode::MessagesOnly) && (self.server || self.client) {
            generators.push(
                tonic_prost_build::configure()
                    .build_server(self.server)
                    .build_client(self.client)
                    .service_generator(),
            );
        }
        generators.extend(self.hooks);
        if !generators.is_empty() {
            config.service_generator(Box::new(generator::Chain::new(generators)));
        }
        config
    }
}

/// Whether `mode` emits the service contract constants when they are
/// `enabled`: services-only output refers to a messages crate that already
/// has them.
fn emits_service_contracts(mode: &Mode, enabled: bool) -> bool {
    enabled && !matches!(mode, Mode::ServicesOnly { .. })
}

/// The `extern_path` pairs for `mode`: one per package in services-only
/// mode, none otherwise. The empty package and `google.protobuf` (which
/// prost already maps to `prost_types`) are skipped.
pub fn extern_paths(mode: &Mode, packages: &[String]) -> Vec<(String, String)> {
    match mode {
        Mode::ServicesOnly { messages_crate } => packages
            .iter()
            .filter(|package| !package.is_empty() && *package != WELL_KNOWN_PACKAGE)
            .map(|package| {
                (
                    format!(".{package}"),
                    discover::rust_path(messages_crate, package),
                )
            })
            .collect(),
        Mode::MessagesOnly | Mode::Both => Vec::new(),
    }
}

/// The packages declared by `files`, as `(file, package)` pairs; a file
/// without a package pairs with the empty string.
pub fn read_packages(files: &[PathBuf]) -> Result<Vec<(PathBuf, String)>, ProtoBuildError> {
    files
        .iter()
        .map(|file| {
            let source = std::fs::read_to_string(file).map_err(|source| ProtoBuildError::Io {
                path: file.clone(),
                source,
            })?;
            let package = discover::package_of(&source)
                .map_err(|reason| ProtoBuildError::Package {
                    file: file.clone(),
                    reason,
                })?
                .unwrap_or_default();
            Ok((file.clone(), package))
        })
        .collect()
}

/// Fail with install hints unless `protoc --version` runs successfully.
/// Returns the reported version.
pub fn check_protoc(protoc: &Path) -> Result<String, ProtoBuildError> {
    let missing = || ProtoBuildError::ProtocMissing {
        protoc: protoc.display().to_string(),
    };
    let output = Command::new(protoc)
        .arg("--version")
        .output()
        .map_err(|_| missing())?;
    if !output.status.success() {
        return Err(missing());
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn generated_file_name(package: &str) -> String {
    if package.is_empty() {
        "_.rs".to_owned()
    } else {
        format!("{package}.rs")
    }
}

/// The distinct packages of `(file name, package)` descriptor pairs, sorted,
/// without the well-known types' package.
fn descriptor_packages(described: &[(String, String)]) -> Vec<String> {
    described
        .iter()
        .map(|(_, package)| package)
        .filter(|package| *package != WELL_KNOWN_PACKAGE)
        .cloned()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

/// Services-only generation maps messages by package, so an imported file
/// without one would have its messages generated again.
fn check_imported_packages(described: &[(String, String)]) -> Result<(), ProtoBuildError> {
    match described.iter().find(|(_, package)| package.is_empty()) {
        Some((name, _)) => Err(ProtoBuildError::Package {
            file: PathBuf::from(name),
            reason: "services-only generation needs a package declaration in every imported file",
        }),
        None => Ok(()),
    }
}

/// The files behind descriptor names (relative to an import path) that
/// exist under one of `includes`, searched in order like protoc does,
/// excluding those in `known`. Names that resolve nowhere come from protoc's
/// own include directory (the well-known types) and are skipped.
fn imported_files(
    described: &[(String, String)],
    includes: &[PathBuf],
    known: &[PathBuf],
) -> Vec<PathBuf> {
    let mut found: Vec<PathBuf> = Vec::new();
    for (name, _) in described {
        let Some(path) = includes
            .iter()
            .map(|include| include.join(name))
            .find(|path| path.is_file())
        else {
            continue;
        };
        if !known.contains(&path) && !found.contains(&path) {
            found.push(path);
        }
    }
    found
}

fn changed_lines(paths: &[PathBuf]) -> Vec<String> {
    paths
        .iter()
        .map(|path| format!("cargo:rerun-if-changed={}", path.display()))
        .collect()
}

fn rerun_lines(files: &[PathBuf], dirs: &[PathBuf]) -> Vec<String> {
    let mut lines = changed_lines(files);
    lines.extend(changed_lines(dirs));
    lines.push("cargo:rerun-if-env-changed=PROTOC".to_owned());
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    fn packages(names: &[&str]) -> Vec<String> {
        names.iter().map(|name| (*name).to_owned()).collect()
    }

    #[test]
    fn services_only_maps_every_package_to_the_messages_crate() {
        let mode = Mode::ServicesOnly {
            messages_crate: "::billing_proto".to_owned(),
        };
        assert_eq!(
            extern_paths(&mode, &packages(&["", "billing.v1", "common.v1"])),
            [
                (
                    ".billing.v1".to_owned(),
                    "::billing_proto::billing::v1".to_owned()
                ),
                (
                    ".common.v1".to_owned(),
                    "::billing_proto::common::v1".to_owned()
                ),
            ]
        );
    }

    #[test]
    fn services_only_leaves_the_well_known_types_to_prost() {
        let mode = Mode::ServicesOnly {
            messages_crate: "::m".to_owned(),
        };
        assert_eq!(
            extern_paths(&mode, &packages(&["google.protobuf", "orders.v1"])),
            [(".orders.v1".to_owned(), "::m::orders::v1".to_owned())]
        );
    }

    fn described(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(name, package)| ((*name).to_owned(), (*package).to_owned()))
            .collect()
    }

    #[test]
    fn descriptor_packages_cover_imports_without_the_well_known_types() {
        let set = described(&[
            ("google/protobuf/timestamp.proto", "google.protobuf"),
            ("common/v1/money.proto", "common.v1"),
            ("billing/v1/billing.proto", "billing.v1"),
            ("billing/v1/extra.proto", "billing.v1"),
            ("loose.proto", ""),
        ]);
        assert_eq!(
            descriptor_packages(&set),
            packages(&["", "billing.v1", "common.v1"])
        );
    }

    #[test]
    fn services_only_rejects_package_less_imports() {
        let fine = described(&[("a.proto", "a.v1"), ("b.proto", "b.v1")]);
        assert!(check_imported_packages(&fine).is_ok());
        let error =
            check_imported_packages(&described(&[("a.proto", "a.v1"), ("dep/loose.proto", "")]))
                .unwrap_err();
        let message = error.to_string();
        assert!(message.starts_with("dep/loose.proto: "), "{message}");
        assert!(message.contains("every imported file"), "{message}");
    }

    #[test]
    fn imported_files_resolve_under_the_includes_in_order() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("proto");
        let vendor = temp.path().join("vendor");
        std::fs::create_dir_all(root.join("billing/v1")).unwrap();
        std::fs::create_dir_all(root.join("common/v1")).unwrap();
        std::fs::create_dir_all(vendor.join("common/v1")).unwrap();
        std::fs::create_dir_all(vendor.join("google/api")).unwrap();
        std::fs::write(root.join("billing/v1/billing.proto"), "").unwrap();
        std::fs::write(root.join("common/v1/money.proto"), "").unwrap();
        std::fs::write(vendor.join("common/v1/money.proto"), "").unwrap();
        std::fs::write(vendor.join("google/api/http.proto"), "").unwrap();

        let set = described(&[
            ("google/protobuf/descriptor.proto", "google.protobuf"),
            ("google/api/http.proto", "google.api"),
            ("common/v1/money.proto", "common.v1"),
            ("common/v1/money.proto", "common.v1"),
            ("billing/v1/billing.proto", "billing.v1"),
        ]);
        let includes = [root.clone(), vendor.clone()];
        let listed = [root.join("billing/v1/billing.proto")];
        assert_eq!(
            imported_files(&set, &includes, &listed),
            [
                vendor.join("google/api/http.proto"),
                root.join("common/v1/money.proto"),
            ]
        );
        assert_eq!(
            changed_lines(&[PathBuf::from("v/x.proto")]),
            ["cargo:rerun-if-changed=v/x.proto"]
        );
    }

    #[test]
    fn other_modes_map_nothing() {
        let names = packages(&["billing.v1"]);
        assert!(extern_paths(&Mode::MessagesOnly, &names).is_empty());
        assert!(extern_paths(&Mode::Both, &names).is_empty());
    }

    #[test]
    fn service_contracts_are_emitted_unless_services_only_or_disabled() {
        assert!(emits_service_contracts(&Mode::MessagesOnly, true));
        assert!(emits_service_contracts(&Mode::Both, true));
        assert!(!emits_service_contracts(&Mode::MessagesOnly, false));
        assert!(!emits_service_contracts(&Mode::Both, false));
        let services_only = Mode::ServicesOnly {
            messages_crate: "::m".to_owned(),
        };
        assert!(!emits_service_contracts(&services_only, true));
        assert!(!emits_service_contracts(&services_only, false));
        assert_eq!(SERVICE_CONTRACT_PREFIX, "__sekvent_service_");
        assert_eq!(RPC_TYPES_PREFIX, "__sekvent_rpc_");
    }

    #[test]
    fn builder_options_are_recorded() {
        let build = ProtoBuild::new("proto")
            .files(["a/v1/a.proto"])
            .include("vendor")
            .messages_only()
            .server(false)
            .client(false)
            .file_descriptor_set("fds.bin")
            .bytes(["."])
            .type_attribute(".a", "#[derive(Eq)]")
            .field_attribute(".a.A.b", "#[doc = \"b\"]")
            .out_dir("/tmp/out")
            .wrapper_file("protos.rs")
            .emit_rerun_if_changed(false)
            .protoc("/usr/bin/protoc")
            .service_contracts(false);
        assert_eq!(build.files, [PathBuf::from("proto/a/v1/a.proto")]);
        assert_eq!(build.includes, [PathBuf::from("vendor")]);
        assert_eq!(build.mode, Mode::MessagesOnly);
        assert!(!build.server && !build.client);
        assert_eq!(build.descriptor_set.as_deref(), Some("fds.bin"));
        assert_eq!(build.bytes, ["."]);
        assert_eq!(build.type_attributes.len(), 1);
        assert_eq!(build.field_attributes.len(), 1);
        assert_eq!(build.out_dir.as_deref(), Some(Path::new("/tmp/out")));
        assert_eq!(build.wrapper_file, "protos.rs");
        assert!(!build.emit_rerun);
        assert_eq!(build.protoc.as_deref(), Some(Path::new("/usr/bin/protoc")));
        assert!(!build.service_contracts);
        assert!(ProtoBuild::new("p").service_contracts);
        assert_eq!(ProtoBuild::new("p").both().mode, Mode::Both);
        assert_eq!(
            ProtoBuild::new("p").services_only("::m").mode,
            Mode::ServicesOnly {
                messages_crate: "::m".to_owned()
            }
        );
    }

    #[test]
    fn a_missing_protoc_names_the_variable_and_install_hints() {
        let error = check_protoc(Path::new("/nonexistent/protoc-for-sekvent-tests")).unwrap_err();
        let message = error.to_string();
        assert!(message.contains("/nonexistent/protoc-for-sekvent-tests"));
        assert!(message.contains("PROTOC"));
        assert!(message.contains("brew install protobuf"));
        assert!(message.contains("apt-get install protobuf-compiler"));
    }

    #[test]
    fn a_failing_protoc_is_reported_as_missing() {
        let error = check_protoc(Path::new("false")).unwrap_err();
        assert!(matches!(error, ProtoBuildError::ProtocMissing { .. }));
    }

    #[test]
    fn generated_files_are_named_after_packages() {
        assert_eq!(generated_file_name("billing.v1"), "billing.v1.rs");
        assert_eq!(generated_file_name(""), "_.rs");
    }

    #[test]
    fn rerun_lines_cover_files_dirs_and_protoc() {
        let lines = rerun_lines(&[PathBuf::from("p/a.proto")], &[PathBuf::from("p")]);
        assert_eq!(
            lines,
            [
                "cargo:rerun-if-changed=p/a.proto",
                "cargo:rerun-if-changed=p",
                "cargo:rerun-if-env-changed=PROTOC"
            ]
        );
    }

    #[test]
    fn packages_are_read_per_file() {
        let dir = tempfile::tempdir().unwrap();
        let with = dir.path().join("with.proto");
        let without = dir.path().join("without.proto");
        let broken = dir.path().join("broken.proto");
        std::fs::write(&with, "package orders.v1;").unwrap();
        std::fs::write(&without, "syntax = \"proto3\";").unwrap();
        std::fs::write(&broken, "package 9;").unwrap();
        assert_eq!(
            read_packages(&[with.clone(), without.clone()]).unwrap(),
            [(with, "orders.v1".to_owned()), (without, String::new())]
        );
        let error = read_packages(&[broken]).unwrap_err();
        assert!(error.to_string().ends_with("malformed package declaration"));
        let error = read_packages(&[dir.path().join("absent.proto")]).unwrap_err();
        assert!(matches!(error, ProtoBuildError::Io { .. }));
    }

    #[test]
    fn compile_without_an_out_dir_fails_early() {
        // Cargo sets OUT_DIR only for build scripts and crates with one; this
        // crate has none, so the variable is absent in its unit tests.
        if std::env::var_os("OUT_DIR").is_some() {
            return;
        }
        let error = ProtoBuild::new("proto").compile().unwrap_err();
        assert!(matches!(error, ProtoBuildError::NoOutDir));
    }

    #[test]
    fn compile_with_no_files_names_the_root() {
        let dir = tempfile::tempdir().unwrap();
        let error = ProtoBuild::new(dir.path())
            .out_dir(dir.path())
            .compile()
            .unwrap_err();
        assert!(matches!(error, ProtoBuildError::NoProtoFiles { .. }));
    }

    #[test]
    fn services_only_rejects_package_less_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("x.proto"), "syntax = \"proto3\";").unwrap();
        let error = ProtoBuild::new(dir.path())
            .services_only("::m")
            .out_dir(dir.path())
            .compile()
            .unwrap_err();
        assert!(error.to_string().contains("needs a package declaration"));
    }

    #[test]
    fn compile_with_a_missing_protoc_fails_before_generating() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("x.proto"),
            "syntax = \"proto3\"; package x.v1;",
        )
        .unwrap();
        let error = ProtoBuild::new(dir.path())
            .out_dir(dir.path())
            .protoc("/nonexistent/protoc-for-sekvent-tests")
            .compile()
            .unwrap_err();
        assert!(matches!(error, ProtoBuildError::ProtocMissing { .. }));
        assert!(!dir.path().join(DEFAULT_WRAPPER_FILE).exists());
    }
}
