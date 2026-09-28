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
