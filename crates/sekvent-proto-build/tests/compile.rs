//! End-to-end generation from `tests/fixtures` into a temporary directory.
//!
//! Needs `protoc` (from `PROTOC` or `PATH`); each test skips with a message
//! when it is absent.

use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

use sekvent_proto_build::{ProtoBuild, ServiceGenerator, check_protoc};

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

fn protoc_available() -> bool {
    match check_protoc(&prost_build::protoc_from_env()) {
        Ok(_) => true,
        Err(error) => {
            eprintln!("skipped: {error}");
            false
        }
    }
}

fn read(path: &Path) -> String {
    fs::read_to_string(path).unwrap_or_else(|error| panic!("read {}: {error}", path.display()))
}

#[test]
fn messages_only_generates_named_types_and_a_wrapper() {
    if !protoc_available() {
        return;
    }
    let out = tempfile::tempdir().unwrap();
    let compiled = ProtoBuild::new(fixtures())
        .messages_only()
        .bytes(["."])
        .type_attribute(".common.v1.Money", "#[derive(Eq, Hash)]")
        .out_dir(out.path())
        .emit_rerun_if_changed(false)
        .compile()
        .unwrap();

    assert_eq!(compiled.packages, ["billing.v1", "common.v1"]);
    assert_eq!(compiled.files.len(), 2);
    assert_eq!(compiled.generated.len(), 2);
    assert_eq!(compiled.descriptor_set, None);

    let billing = read(&out.path().join("billing.v1.rs"));
    assert!(billing.contains("pub struct Invoice"));
    assert!(
        billing.contains("::prost::Name for Invoice"),
        "type names are enabled"
    );
    assert!(
        billing.contains("::prost::bytes::Bytes"),
        "bytes mapping applies"
    );
    assert!(!billing.contains("invoices_server"), "no service stubs");
    assert!(read(&out.path().join("common.v1.rs")).contains("Eq, Hash"));

    let wrapper = read(&compiled.wrapper);
    assert!(wrapper.contains("pub mod billing {"));
    assert!(wrapper.contains("pub mod v1 {"));
    assert!(wrapper.contains("clippy::pedantic"));
    assert!(wrapper.contains("billing.v1.rs"));
}

#[test]
fn services_only_points_at_the_messages_crate() {
    if !protoc_available() {
        return;
    }
    let out = tempfile::tempdir().unwrap();
    let compiled = ProtoBuild::new(fixtures())
        .files(["billing/v1/billing.proto", "common/v1/money.proto"])
        .services_only("::billing_proto")
        .client(false)
        .file_descriptor_set("billing.bin")
        .out_dir(out.path())
        .emit_rerun_if_changed(false)
        .compile()
        .unwrap();

    let billing = read(&out.path().join("billing.v1.rs"));
    assert!(
        billing.contains("invoices_server"),
        "server stubs are generated"
    );
    assert!(!billing.contains("invoices_client"), "client stubs are off");
    assert!(
        !billing.contains("pub struct Invoice "),
        "messages are not regenerated"
    );
    assert!(billing.contains("::billing_proto::billing::v1::Invoice"));

    let descriptors = compiled.descriptor_set.unwrap();
    assert!(fs::metadata(descriptors).unwrap().len() > 0);
}

#[test]
fn services_only_maps_packages_imported_by_listed_files() {
    if !protoc_available() {
        return;
    }
    let out = tempfile::tempdir().unwrap();
    let compiled = ProtoBuild::new(fixtures())
        .files(["billing/v1/billing.proto"])
        .services_only("::billing_proto")
        .out_dir(out.path())
        .emit_rerun_if_changed(false)
        .compile()
        .unwrap();

    assert_eq!(compiled.files.len(), 1);
    assert_eq!(compiled.packages, ["billing.v1", "common.v1"]);
    assert!(
        !out.path().join("common.v1.rs").exists(),
        "imported messages come from the messages crate"
    );
    assert_eq!(compiled.generated, [out.path().join("billing.v1.rs")]);
    let billing = read(&out.path().join("billing.v1.rs"));
    assert!(billing.contains("::billing_proto::billing::v1::Invoice"));
    assert!(!billing.contains("pub struct Money"));
}

#[test]
fn listed_files_bring_their_imports_into_the_wrapper() {
    if !protoc_available() {
        return;
    }
    let out = tempfile::tempdir().unwrap();
    let compiled = ProtoBuild::new(fixtures())
        .files(["billing/v1/billing.proto"])
        .messages_only()
        .out_dir(out.path())
        .compile()
        .unwrap();

    assert_eq!(compiled.packages, ["billing.v1", "common.v1"]);
    assert_eq!(compiled.generated.len(), 2);
    assert!(read(&out.path().join("common.v1.rs")).contains("pub struct Money"));
    assert!(read(&compiled.wrapper).contains("common.v1.rs"));
}

#[test]
fn services_only_rejects_a_package_less_import() {
    if !protoc_available() {
        return;
    }
    let src = tempfile::tempdir().unwrap();
    fs::write(
        src.path().join("loose.proto"),
        "syntax = \"proto3\"; message Loose {}",
    )
    .unwrap();
    fs::write(
        src.path().join("svc.proto"),
        "syntax = \"proto3\"; package svc.v1; import \"loose.proto\"; \
         service Svc { rpc Get(Loose) returns (Loose); }",
    )
    .unwrap();
    let error = ProtoBuild::new(src.path())
        .files(["svc.proto"])
        .services_only("::m")
        .out_dir(src.path())
        .emit_rerun_if_changed(false)
        .compile()
        .unwrap_err();
    assert!(error.to_string().starts_with("loose.proto: "), "{error}");
}

struct Marker;

impl ServiceGenerator for Marker {
    fn generate(&mut self, service: prost_build::Service, buf: &mut String) {
        // prost formats its output, which drops plain comments, so the
        // marker has to be an item.
        let _ = writeln!(
            buf,
            "pub const HOOK_SAW: &str = \"{}.{}\";",
            service.package, service.proto_name
        );
    }
}

#[test]
fn both_runs_tonic_and_the_hook() {
    if !protoc_available() {
        return;
    }
    let out = tempfile::tempdir().unwrap();
    ProtoBuild::new(fixtures())
        .both()
        .service_generator_hook(Box::new(Marker))
        .wrapper_file("all.rs")
        .out_dir(out.path())
        .emit_rerun_if_changed(false)
        .compile()
        .unwrap();

    let billing = read(&out.path().join("billing.v1.rs"));
    assert!(billing.contains("pub struct Invoice"));
    assert!(billing.contains("invoices_server"));
    assert!(billing.contains("invoices_client"));
    assert!(billing.contains(r#"HOOK_SAW: &str = "billing.v1.Invoices";"#));
    assert!(out.path().join("all.rs").exists());
}

/// `text` without whitespace or trailing commas, so generated code compares
/// the same whether or not prost formatted it.
fn compact(text: &str) -> String {
    let dense: String = text.chars().filter(|c| !c.is_whitespace()).collect();
    dense.replace(",]", "]").replace(",)", ")")
}

/// A package with two services, a streaming RPC, a type from another
/// package and a nested message next to a top-level one of the same name,
/// importing the fixtures' `common.v1`.
fn contract_fixture(dir: &Path) {
    fs::create_dir_all(dir.join("ledger/v1")).unwrap();
    fs::write(
        dir.join("ledger/v1/ledger.proto"),
        "syntax = \"proto3\";\n\
         package ledger.v1;\n\
         import \"common/v1/money.proto\";\n\
         message Entry {\n\
           string id = 1;\n\
           common.v1.Money amount = 2;\n\
           message Line { string text = 1; }\n\
         }\n\
         message Line { string text = 1; }\n\
         message GetEntryRequest { string id = 1; }\n\
         message Ack {}\n\
         service Entries {\n\
           rpc GetEntry(GetEntryRequest) returns (Entry);\n\
           rpc Balance(GetEntryRequest) returns (common.v1.Money);\n\
           rpc Follow(GetEntryRequest) returns (stream Entry);\n\
           rpc Relabel(Entry.Line) returns (Line);\n\
         }\n\
         service Audit { rpc Record(Entry) returns (Ack); }\n",
    )
    .unwrap();
}

const ENTRIES_CONTRACT: &str = r#"
    /// Contract of `ledger.v1.Entries`, checked by `#[component(proto = …)]`.
    #[doc(hidden)]
    #[allow(non_upper_case_globals, dead_code)]
    pub const __sekvent_service_Entries: (&str, &[(&str, &str, &str, bool)]) = (
        "ledger.v1.Entries",
        &[
            ("GetEntry", "ledger.v1.GetEntryRequest", "ledger.v1.Entry", false),
            ("Balance", "ledger.v1.GetEntryRequest", "common.v1.Money", false),
            ("Follow", "ledger.v1.GetEntryRequest", "ledger.v1.Entry", true),
            ("Relabel", "ledger.v1.Entry.Line", "ledger.v1.Line", false),
        ],
    );
"#;

/// The per-RPC type aliases of `ledger.v1.Entries`, with the Rust paths
/// prost resolves from the package module.
const ENTRIES_TYPES: [&str; 4] = [
    "pub type __sekvent_rpc_Entries__GetEntry = (GetEntryRequest, Entry);",
    "pub type __sekvent_rpc_Entries__Balance = (GetEntryRequest, super::super::common::v1::Money);",
    "pub type __sekvent_rpc_Entries__Follow = (GetEntryRequest, Entry);",
    "pub type __sekvent_rpc_Entries__Relabel = (entry::Line, Line);",
];

const AUDIT_CONTRACT: &str = r#"
    pub const __sekvent_service_Audit: (&str, &[(&str, &str, &str, bool)]) = (
        "ledger.v1.Audit",
        &[("Record", "ledger.v1.Entry", "ledger.v1.Ack", false)],
    );
    /// Request and reply of `ledger.v1.Audit.Record`, checked by `#[component(proto = …)]`.
    #[doc(hidden)]
    #[allow(non_camel_case_types, dead_code)]
    pub type __sekvent_rpc_Audit__Record = (Entry, Ack);
"#;

#[test]
fn messages_only_emits_a_contract_per_service() {
    if !protoc_available() {
        return;
    }
    let src = tempfile::tempdir().unwrap();
    contract_fixture(src.path());
    let out = tempfile::tempdir().unwrap();
    ProtoBuild::new(src.path())
        .include(fixtures())
        .messages_only()
        .out_dir(out.path())
        .emit_rerun_if_changed(false)
        .compile()
        .unwrap();

    let ledger = compact(&read(&out.path().join("ledger.v1.rs")));
    assert!(ledger.contains(&compact(ENTRIES_CONTRACT)), "{ledger}");
    for alias in ENTRIES_TYPES {
        assert!(
            ledger.contains(&compact(alias)),
            "missing `{alias}` in {ledger}"
        );
    }
    assert!(ledger.contains(&compact(AUDIT_CONTRACT)), "{ledger}");
    // prost names a nested message by its leaf and its file's package; only
    // the full name (and the Rust path in the aliases) keeps the nesting.
    assert!(
        ledger.contains(&compact(r#"const NAME: &'static str = "Line";"#)),
        "{ledger}"
    );
    assert!(ledger.contains("ledger.v1.Entry.Line"), "{ledger}");
    assert!(!ledger.contains("sekvent_component"), "{ledger}");
    assert!(!ledger.contains("entries_server"), "{ledger}");
    let common = read(&out.path().join("common.v1.rs"));
    assert!(!common.contains("__sekvent_service_"), "{common}");
}

#[test]
fn both_emits_contracts_next_to_the_stubs() {
    if !protoc_available() {
        return;
    }
    let out = tempfile::tempdir().unwrap();
    ProtoBuild::new(fixtures())
        .both()
        .out_dir(out.path())
        .emit_rerun_if_changed(false)
        .compile()
        .unwrap();

    let billing = read(&out.path().join("billing.v1.rs"));
    assert!(billing.contains("invoices_server"));
    assert!(
        compact(&billing).contains(&compact(
            r#"pub const __sekvent_service_Invoices: (&str, &[(&str, &str, &str, bool)]) = (
                "billing.v1.Invoices",
                &[("GetInvoice", "billing.v1.GetInvoiceRequest", "billing.v1.Invoice", false)],
            );"#
        )),
        "{billing}"
    );
    assert!(
        compact(&billing).contains(&compact(
            "pub type __sekvent_rpc_Invoices__GetInvoice = (GetInvoiceRequest, Invoice);"
        )),
        "{billing}"
    );
}

#[test]
fn empty_is_the_unit_type_in_the_rpc_types() {
    if !protoc_available() {
        return;
    }
    let src = tempfile::tempdir().unwrap();
    fs::create_dir_all(src.path().join("chores/v1")).unwrap();
    fs::write(
        src.path().join("chores/v1/chores.proto"),
        "syntax = \"proto3\";\n\
         package chores.v1;\n\
         import \"google/protobuf/empty.proto\";\n\
         message Chore { string name = 1; }\n\
         service Chores {\n\
           rpc Clear(Chore) returns (google.protobuf.Empty);\n\
           rpc Tick(google.protobuf.Empty) returns (Chore);\n\
         }\n",
    )
    .unwrap();
    let out = tempfile::tempdir().unwrap();
    ProtoBuild::new(src.path())
        .messages_only()
        .out_dir(out.path())
        .emit_rerun_if_changed(false)
        .compile()
        .unwrap();

    let chores = compact(&read(&out.path().join("chores.v1.rs")));
    for needle in [
        r#"("Clear","chores.v1.Chore","google.protobuf.Empty",false)"#,
        r#"("Tick","google.protobuf.Empty","chores.v1.Chore",false)"#,
        "pubtype__sekvent_rpc_Chores__Clear=(Chore,());",
        "pubtype__sekvent_rpc_Chores__Tick=((),Chore);",
    ] {
        assert!(chores.contains(needle), "missing `{needle}` in {chores}");
    }
}

#[test]
fn contracts_can_be_turned_off_and_services_only_never_emits_them() {
    if !protoc_available() {
        return;
    }
    let src = tempfile::tempdir().unwrap();
    contract_fixture(src.path());
    let off = tempfile::tempdir().unwrap();
    ProtoBuild::new(src.path())
        .include(fixtures())
        .messages_only()
        .service_contracts(false)
        .out_dir(off.path())
        .emit_rerun_if_changed(false)
        .compile()
        .unwrap();
    let ledger = read(&off.path().join("ledger.v1.rs"));
    assert!(ledger.contains("pub struct Entry"));
    assert!(!ledger.contains("__sekvent_service_"), "{ledger}");

    let services = tempfile::tempdir().unwrap();
    ProtoBuild::new(src.path())
        .include(fixtures())
        .files(["ledger/v1/ledger.proto"])
        .services_only("::ledger_proto")
        .out_dir(services.path())
        .emit_rerun_if_changed(false)
        .compile()
        .unwrap();
    let ledger = read(&services.path().join("ledger.v1.rs"));
    assert!(ledger.contains("entries_server"));
    assert!(!ledger.contains("__sekvent_service_"), "{ledger}");
}

#[test]
fn a_syntax_error_is_a_compile_error() {
    if !protoc_available() {
        return;
    }
    let src = tempfile::tempdir().unwrap();
    fs::write(
        src.path().join("bad.proto"),
        "syntax = \"proto3\"; package bad.v1; message {",
    )
    .unwrap();
    let error = ProtoBuild::new(src.path())
        .out_dir(src.path())
        .emit_rerun_if_changed(false)
        .compile()
        .unwrap_err();
    assert!(error.to_string().starts_with("protobuf compilation failed"));
}
