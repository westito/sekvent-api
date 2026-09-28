//! `cargo sekvent contract emit | check`: protobuf service contracts.
//!
//! The `.proto` files below the `[contract]` roots are compiled in-process
//! with protox (no `protoc`, no cargo build), so both commands run on any
//! machine: `emit` writes one canonical JSON baseline per service into the
//! tree, `check` compares every baseline with the current protos and
//! reports the wire-breaking changes. The gate runs `check` as an
//! in-process step when roots are configured.

mod compile;
mod model;
mod rules;

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::{self, Write as _};
use std::path::{Path, PathBuf};

use anyhow::{Context as _, bail};

pub use crate::config::ContractConfig;
pub use model::{Cardinality, Contract, Enum, Extension, FORMAT, Field, Message, Rpc};
pub use rules::{Finding, compare};

/// The error of `emit` and `check` without `[contract].roots`.
pub const NOT_CONFIGURED: &str = "no [contract] roots in sekvent.toml";

/// Compile every root and build the contract of each service in them, by
/// full service name. A root that declares no service is an error: roots
/// exist to hold component services.
pub fn load(root: &Path, config: &ContractConfig) -> anyhow::Result<BTreeMap<String, Contract>> {
    if config.roots.is_empty() {
        bail!(NOT_CONFIGURED);
    }
    let mut contracts = BTreeMap::new();
    let mut defined_in: BTreeMap<String, &Path> = BTreeMap::new();
    for dir in &config.roots {
        let compiled = compile::compile_root(root, dir, &config.includes)?;
        let index = model::TypeIndex::new(&compiled.files, &compiled.bundled);
        let mut services = 0_usize;
        for file in compiled
            .files
            .iter()
            .filter(|file| compiled.own.contains(file.name()))
        {
            for service in &file.service {
                services += 1;
                let contract = index.service_contract(file, service)?;
                if let Some(first) = defined_in.insert(contract.service.clone(), dir) {
                    bail!(
                        "service `{}` is defined in two contract roots: `{}` and `{}`",
                        contract.service,
                        first.display(),
                        dir.display()
                    );
                }
                contracts.insert(contract.service.clone(), contract);
            }
        }
        if services == 0 {
            bail!(
                "contract root `{}` declares no service; a contract root holds the .proto \
                 files of component services",
                dir.display()
            );
        }
    }
    Ok(contracts)
}

/// File name of a service's baseline.
pub fn baseline_file(service: &str) -> String {
    format!("{service}.json")
}

/// Write the baselines of every service, or of the services in `only`, and
/// return the written paths. Existing baselines of other services are left
/// alone.
pub fn emit(root: &Path, config: &ContractConfig, only: &[String]) -> anyhow::Result<Vec<PathBuf>> {
    let contracts = load(root, config)?;
    if let Some(unknown) = only.iter().find(|name| !contracts.contains_key(*name)) {
        bail!("no service `{unknown}` in the [contract] roots");
    }
    let dir = root.join(&config.baseline);
    std::fs::create_dir_all(&dir).with_context(|| format!("cannot create {}", dir.display()))?;
    let mut written = Vec::new();
    for (name, contract) in &contracts {
        if !only.is_empty() && !only.contains(name) {
            continue;
        }
        let path = dir.join(baseline_file(name));
        std::fs::write(&path, contract.to_json())
            .with_context(|| format!("cannot write {}", path.display()))?;
        written.push(path);
    }
    Ok(written)
}

/// Read every `*.json` baseline in `dir`; a missing directory holds none.
pub fn read_baselines(dir: &Path) -> anyhow::Result<BTreeMap<String, Contract>> {
    let mut baselines = BTreeMap::new();
    if !dir.exists() {
        return Ok(baselines);
    }
    let entries =
        std::fs::read_dir(dir).with_context(|| format!("cannot read {}", dir.display()))?;
    for entry in entries {
        let path = entry
            .with_context(|| format!("cannot read {}", dir.display()))?
            .path();
        if path.extension().is_none_or(|ext| ext != "json") {
            continue;
        }
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("cannot read {}", path.display()))?;
        let contract =
            Contract::from_json(&text).with_context(|| format!("invalid {}", path.display()))?;
        let file_name = baseline_file(&contract.service);
        if path
            .file_name()
            .is_none_or(|name| name != file_name.as_str())
        {
            bail!(
                "{} holds the baseline of `{}`; name it {file_name}",
                path.display(),
                contract.service
            );
        }
        baselines.insert(contract.service.clone(), contract);
    }
    Ok(baselines)
}

/// A current service without a committed baseline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MissingBaseline {
    /// Full service name.
    pub service: String,
    /// Where its baseline belongs, relative to the project root.
    pub path: PathBuf,
}

impl fmt::Display for MissingBaseline {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "no baseline for `{}`; run `cargo sekvent contract emit {}` and commit `{}`",
            self.service,
            self.service,
            self.path.display()
        )
    }
}

/// The outcome of `contract check`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CheckReport {
    /// Breaking changes, sorted.
    pub findings: Vec<Finding>,
    /// Current services without a baseline, sorted.
    pub missing: Vec<MissingBaseline>,
    /// Services whose baseline and current contract are compatible, sorted.
    pub compatible: Vec<String>,
}

impl CheckReport {
    /// Nothing breaking and no baseline missing.
    pub fn is_ok(&self) -> bool {
        self.findings.is_empty() && self.missing.is_empty()
    }

    /// `0` when [`CheckReport::is_ok`], else `1`.
    pub fn exit_code(&self) -> i32 {
        i32::from(!self.is_ok())
    }

    /// One line per finding and missing baseline, then a summary line.
    pub fn render(&self) -> String {
        let mut out = String::new();
        for finding in &self.findings {
            let _ = writeln!(out, "{finding}");
        }
        for missing in &self.missing {
            let _ = writeln!(out, "{missing}");
        }
        if !self.findings.is_empty() {
            let services: BTreeSet<&str> = self
                .findings
                .iter()
                .map(|finding| finding.service.as_str())
                .collect();
            let _ = writeln!(
                out,
                "contract: {} breaking changes in {} services",
                self.findings.len(),
                services.len()
            );
        }
        if !self.missing.is_empty() {
            let _ = writeln!(
                out,
                "contract: {} services without a baseline",
                self.missing.len()
            );
        }
        if self.is_ok() {
            let _ = writeln!(
                out,
                "contract: {} services compatible",
                self.compatible.len()
            );
        }
        out
    }
}

/// Compare the baselines with the current contracts, all of them or those
/// named in `only`. A baseline whose service is gone is a breaking change;
/// a service without a baseline is reported as missing.
pub fn check(root: &Path, config: &ContractConfig, only: &[String]) -> anyhow::Result<CheckReport> {
    let current = load(root, config)?;
    let baselines = read_baselines(&root.join(&config.baseline))?;
    if let Some(unknown) = only
        .iter()
        .find(|name| !current.contains_key(*name) && !baselines.contains_key(*name))
    {
        bail!("no service `{unknown}` in the [contract] roots or baselines");
    }
    let wanted = |name: &String| only.is_empty() || only.contains(name);
    let mut report = CheckReport::default();
    for (name, baseline) in baselines.iter().filter(|(name, _)| wanted(name)) {
        let findings = compare(baseline, current.get(name));
        if findings.is_empty() {
            report.compatible.push(name.clone());
        } else {
            report.findings.extend(findings);
        }
    }
    for name in current
        .keys()
        .filter(|name| wanted(name) && !baselines.contains_key(*name))
    {
        report.missing.push(MissingBaseline {
            service: name.clone(),
            path: config.baseline.join(baseline_file(name)),
        });
    }
    report.findings.sort();
    Ok(report)
}

/// `cargo sekvent contract emit`: write the baselines and print
/// `wrote <path>` for each.
pub fn run_emit(root: &Path, config: &ContractConfig, only: &[String]) -> anyhow::Result<i32> {
    for path in emit(root, config, only)? {
        println!(
            "wrote {}",
            path.strip_prefix(root).unwrap_or(&path).display()
        );
    }
    Ok(0)
}

/// `cargo sekvent contract check` and the gate step: print the report and
/// return its exit code.
pub fn run_check(root: &Path, config: &ContractConfig, only: &[String]) -> anyhow::Result<i32> {
    let report = check(root, config, only)?;
    print!("{}", report.render());
    Ok(report.exit_code())
}

#[cfg(test)]
pub(crate) mod testing {
    //! Temporary projects of proto sources for the contract tests.

    use std::collections::BTreeMap;
    use std::path::{Path, PathBuf};

    use super::{Contract, ContractConfig, load};

    /// Write `files` (relative path, content) below `dir`.
    pub(crate) fn write_files(dir: &Path, files: &[(&str, &str)]) {
        for (name, content) in files {
            let path = dir.join(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, content).unwrap();
        }
    }

    /// A contract configuration with `roots` and `includes`.
    pub(crate) fn config(roots: &[&str], includes: &[&str]) -> ContractConfig {
        ContractConfig {
            roots: roots.iter().map(PathBuf::from).collect(),
            includes: includes.iter().map(PathBuf::from).collect(),
            ..ContractConfig::default()
        }
    }

    /// [`load`] on `dir` with `roots` and `includes`.
    pub(crate) fn load_project(
        dir: &Path,
        roots: &[&str],
        includes: &[&str],
    ) -> anyhow::Result<BTreeMap<String, Contract>> {
        load(dir, &config(roots, includes))
    }

    /// The contracts of `files`, compiled as the single root `proto`.
    pub(crate) fn contracts(files: &[(&str, &str)]) -> BTreeMap<String, Contract> {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("proto");
        write_files(&root, files);
        load_project(dir.path(), &["proto"], &[]).unwrap_or_else(|error| panic!("{error:#}"))
    }
}

#[cfg(test)]
mod tests {
    use super::testing::{config, write_files};
    use super::*;

    const ORDERS: &str = "syntax = \"proto3\";\npackage orders.v1;\n\
        service Orders { rpc Place(PlaceRequest) returns (PlaceReply); }\n\
        message PlaceRequest { string sku = 1; uint32 quantity = 2; }\n\
        message PlaceReply { string id = 1; }\n";

    const BILLING: &str = "syntax = \"proto3\";\npackage billing.v1;\n\
        service Billing { rpc Charge(ChargeRequest) returns (ChargeReply); }\n\
        message ChargeRequest { string order_id = 1; }\n\
        message ChargeReply {}\n";

    /// A project with the roots `orders/proto` and `billing/proto`.
    fn project() -> (tempfile::TempDir, ContractConfig) {
        let dir = tempfile::tempdir().unwrap();
        write_files(
            dir.path(),
            &[
                ("orders/proto/orders/v1/orders.proto", ORDERS),
                ("billing/proto/billing/v1/billing.proto", BILLING),
            ],
        );
        (dir, config(&["orders/proto", "billing/proto"], &[]))
    }

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|name| (*name).to_owned()).collect()
    }

    #[test]
    fn unconfigured_projects_are_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let config = ContractConfig::default();
        for error in [
            emit(dir.path(), &config, &[]).unwrap_err(),
            check(dir.path(), &config, &[]).unwrap_err(),
            run_emit(dir.path(), &config, &[]).unwrap_err(),
            run_check(dir.path(), &config, &[]).unwrap_err(),
        ] {
            assert_eq!(error.to_string(), NOT_CONFIGURED);
        }
    }

    #[test]
    fn emit_writes_one_baseline_per_service_and_is_repeatable() {
        let (dir, config) = project();
        let written = emit(dir.path(), &config, &[]).unwrap();
        let contracts = dir.path().join("contracts");
        assert_eq!(
            written,
            [
                contracts.join("billing.v1.Billing.json"),
                contracts.join("orders.v1.Orders.json"),
            ]
        );
        let first = std::fs::read(&written[1]).unwrap();
        assert!(first.ends_with(b"}\n"));
        let current = load(dir.path(), &config).unwrap();
        assert_eq!(
            String::from_utf8(first.clone()).unwrap(),
            current["orders.v1.Orders"].to_json()
        );
        assert_eq!(run_emit(dir.path(), &config, &[]).unwrap(), 0);
        assert_eq!(std::fs::read(&written[1]).unwrap(), first);
    }

    #[test]
    fn emit_only_writes_the_named_services() {
        let (dir, config) = project();
        let written = emit(dir.path(), &config, &names(&["orders.v1.Orders"])).unwrap();
        assert_eq!(
            written,
            [dir.path().join("contracts/orders.v1.Orders.json")]
        );
        assert!(
            !dir.path()
                .join("contracts/billing.v1.Billing.json")
                .exists()
        );
        let error = emit(dir.path(), &config, &names(&["nope.v1.Nope"])).unwrap_err();
        assert_eq!(
            error.to_string(),
            "no service `nope.v1.Nope` in the [contract] roots"
        );
    }

    #[test]
    fn check_passes_on_fresh_baselines() {
        let (dir, config) = project();
        emit(dir.path(), &config, &[]).unwrap();
        let report = check(dir.path(), &config, &[]).unwrap();
        assert!(report.is_ok());
        assert_eq!(report.exit_code(), 0);
        assert_eq!(
            report.compatible,
            ["billing.v1.Billing", "orders.v1.Orders"]
        );
        assert_eq!(report.render(), "contract: 2 services compatible\n");
        assert_eq!(run_check(dir.path(), &config, &[]).unwrap(), 0);
    }

    #[test]
    fn check_reports_breaking_changes_and_missing_baselines() {
        let (dir, config) = project();
        emit(dir.path(), &config, &names(&["orders.v1.Orders"])).unwrap();
        let shrunk = ORDERS.replace("uint32 quantity = 2;", "");
        write_files(
            dir.path(),
            &[("orders/proto/orders/v1/orders.proto", shrunk.as_str())],
        );
        let report = check(dir.path(), &config, &[]).unwrap();
        assert_eq!(report.exit_code(), 1);
        assert!(report.compatible.is_empty());
        assert_eq!(
            report.render(),
            "breaking: orders.v1.Orders: field 2 (quantity) of orders.v1.PlaceRequest was \
             removed without reserving its number\n\
             no baseline for `billing.v1.Billing`; run `cargo sekvent contract emit \
             billing.v1.Billing` and commit `contracts/billing.v1.Billing.json`\n\
             contract: 1 breaking changes in 1 services\n\
             contract: 1 services without a baseline\n"
        );
        assert_eq!(run_check(dir.path(), &config, &[]).unwrap(), 1);

        let only_billing = check(dir.path(), &config, &names(&["billing.v1.Billing"])).unwrap();
        assert!(only_billing.findings.is_empty());
        assert_eq!(only_billing.missing.len(), 1);
        let only_orders = check(dir.path(), &config, &names(&["orders.v1.Orders"])).unwrap();
        assert_eq!(only_orders.findings.len(), 1);
        assert!(only_orders.missing.is_empty());
        let error = check(dir.path(), &config, &names(&["nope.v1.Nope"])).unwrap_err();
        assert_eq!(
            error.to_string(),
            "no service `nope.v1.Nope` in the [contract] roots or baselines"
        );
    }

    #[test]
    fn a_baseline_without_a_service_is_breaking() {
        let (dir, config) = project();
        emit(dir.path(), &config, &[]).unwrap();
        std::fs::remove_dir_all(dir.path().join("billing")).unwrap();
        let config = super::testing::config(&["orders/proto"], &[]);
        let report = check(dir.path(), &config, &[]).unwrap();
        assert_eq!(
            report
                .findings
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>(),
            [
                "breaking: billing.v1.Billing: the service is gone (removed, renamed or moved \
                 to another package)"
            ]
        );
        assert_eq!(report.compatible, ["orders.v1.Orders"]);
        assert_eq!(
            report.render().lines().last(),
            Some("contract: 1 breaking changes in 1 services")
        );
    }

    #[test]
    fn a_service_in_two_roots_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        write_files(
            dir.path(),
            &[("a/orders.proto", ORDERS), ("b/orders.proto", ORDERS)],
        );
        let error = load(dir.path(), &config(&["a", "b"], &[])).unwrap_err();
        assert_eq!(
            error.to_string(),
            "service `orders.v1.Orders` is defined in two contract roots: `a` and `b`"
        );
    }

    #[test]
    fn a_root_without_services_is_an_error_naming_it() {
        let (dir, _) = project();
        write_files(
            dir.path(),
            &[(
                "shared/proto/shared/v1/money.proto",
                "syntax = \"proto3\";\npackage shared.v1;\nmessage Money { int64 units = 1; }\n",
            )],
        );
        let config = config(&["orders/proto", "shared/proto"], &[]);
        for error in [
            load(dir.path(), &config).unwrap_err(),
            emit(dir.path(), &config, &[]).unwrap_err(),
            check(dir.path(), &config, &[]).unwrap_err(),
        ] {
            assert_eq!(
                error.to_string(),
                "contract root `shared/proto` declares no service; a contract root holds the \
                 .proto files of component services"
            );
        }
        assert!(!dir.path().join("contracts").exists(), "emit wrote nothing");
    }

    #[test]
    fn baselines_are_read_strictly() {
        let dir = tempfile::tempdir().unwrap();
        assert!(read_baselines(&dir.path().join("none")).unwrap().is_empty());

        let (work, config) = project();
        emit(work.path(), &config, &[]).unwrap();
        let contracts = work.path().join("contracts");
        std::fs::write(contracts.join("README.md"), "not a baseline").unwrap();
        assert_eq!(read_baselines(&contracts).unwrap().len(), 2);

        std::fs::rename(
            contracts.join("orders.v1.Orders.json"),
            contracts.join("orders.json"),
        )
        .unwrap();
        let error = read_baselines(&contracts).unwrap_err();
        assert!(
            error.to_string().ends_with(
                "holds the baseline of `orders.v1.Orders`; name it orders.v1.Orders.json"
            ),
            "{error}"
        );

        std::fs::write(contracts.join("orders.json"), "{").unwrap();
        let error = read_baselines(&contracts).unwrap_err();
        assert!(format!("{error:#}").contains("invalid "), "{error:#}");
        assert!(format!("{error:#}").contains("orders.json"), "{error:#}");
    }

    #[test]
    fn a_proto_that_does_not_compile_fails_emit_and_check() {
        let (dir, config) = project();
        write_files(
            dir.path(),
            &[("orders/proto/orders/v1/broken.proto", "message {")],
        );
        for error in [
            emit(dir.path(), &config, &[]).unwrap_err(),
            check(dir.path(), &config, &[]).unwrap_err(),
        ] {
            assert!(
                error.to_string().starts_with("orders/v1/broken.proto:1:"),
                "{error}"
            );
        }
    }

    #[test]
    fn missing_baselines_display_the_emit_command() {
        let missing = MissingBaseline {
            service: "a.v1.A".into(),
            path: PathBuf::from("contracts/a.v1.A.json"),
        };
        assert_eq!(
            missing.to_string(),
            "no baseline for `a.v1.A`; run `cargo sekvent contract emit a.v1.A` and commit \
             `contracts/a.v1.A.json`"
        );
        assert_eq!(baseline_file("a.v1.A"), "a.v1.A.json");
        assert_eq!(
            CheckReport::default().render(),
            "contract: 0 services compatible\n"
        );
    }
}
