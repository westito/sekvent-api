use std::fmt::Write as _;

use prost_build::{Service, ServiceGenerator};

use crate::{RPC_TYPES_PREFIX, SERVICE_CONTRACT_PREFIX};

/// Appends `__sekvent_service_<Service>` after every service's position in
/// its package's generated file: the service's full name and, per RPC, its
/// name, request and reply full names and whether it streams. Each RPC also
/// gets `__sekvent_rpc_<Service>__<Rpc>`, an alias of its Rust
/// `(Request, Reply)` types. The `#[component(proto = …)]` macro checks a
/// component trait against both.
pub(crate) struct ServiceContracts;

impl ServiceGenerator for ServiceContracts {
    fn generate(&mut self, service: Service, buf: &mut String) {
        buf.push_str(&render(&service));
    }
}

/// The constant and the per-RPC type aliases for `service`, as Rust source.
pub(crate) fn render(service: &Service) -> String {
    let full_name = qualified(&service.package, &service.proto_name);
    let mut out = String::new();
    let _ = writeln!(
        out,
        "/// Contract of `{full_name}`, checked by `#[component(proto = …)]`."
    );
    out.push_str("#[doc(hidden)]\n");
    out.push_str("#[allow(non_upper_case_globals, dead_code)]\n");
    let _ = writeln!(
        out,
        "pub const {SERVICE_CONTRACT_PREFIX}{}: (&str, &[(&str, &str, &str, bool)]) = (",
        service.proto_name
    );
    let _ = writeln!(out, "    {full_name:?},");
    if service.methods.is_empty() {
        out.push_str("    &[],\n");
    } else {
        out.push_str("    &[\n");
        for method in &service.methods {
            let _ = writeln!(
                out,
                "        ({:?}, {:?}, {:?}, {}),",
                method.proto_name,
                type_name(&method.input_proto_type),
                type_name(&method.output_proto_type),
                method.client_streaming || method.server_streaming,
            );
        }
        out.push_str("    ],\n");
    }
    out.push_str(");\n");
    for method in &service.methods {
        let _ = writeln!(
            out,
            "/// Request and reply of `{full_name}.{}`, checked by `#[component(proto = …)]`.",
            method.proto_name
        );
        out.push_str("#[doc(hidden)]\n");
        out.push_str("#[allow(non_camel_case_types, dead_code)]\n");
        let _ = writeln!(
            out,
            "pub type {} = ({}, {});",
            rpc_types_name(&service.proto_name, &method.proto_name),
            method.input_type,
            method.output_type,
        );
    }
    out
}

/// `__sekvent_rpc_<Service>__<Rpc>`.
fn rpc_types_name(service: &str, rpc: &str) -> String {
    format!("{RPC_TYPES_PREFIX}{service}__{rpc}")
}

/// `package.name`, or `name` alone in the empty package.
fn qualified(package: &str, name: &str) -> String {
    if package.is_empty() {
        name.to_owned()
    } else {
        format!("{package}.{name}")
    }
}

/// A descriptor's fully qualified type (`.shop.v1.Order.Item`) without the
/// leading dot: the full name, with enclosing messages, that
/// `prost::Name::full_name` returns.
fn type_name(proto_type: &str) -> &str {
    proto_type.strip_prefix('.').unwrap_or(proto_type)
}

#[cfg(test)]
mod tests {
    use prost_build::{Comments, Method};

    use super::*;

    /// An RPC: its name, then (protobuf type, Rust type) of the request
    /// and of the reply, as prost resolves them from the package module.
    fn method(
        name: &str,
        input: (&str, &str),
        output: (&str, &str),
        streaming: (bool, bool),
    ) -> Method {
        Method {
            name: name.to_lowercase(),
            proto_name: name.to_owned(),
            comments: Comments::default(),
            input_type: input.1.to_owned(),
            output_type: output.1.to_owned(),
            input_proto_type: input.0.to_owned(),
            output_proto_type: output.0.to_owned(),
            options: prost_types::MethodOptions::default(),
            client_streaming: streaming.0,
            server_streaming: streaming.1,
        }
    }

    fn service(package: &str, name: &str, methods: Vec<Method>) -> Service {
        Service {
            name: name.to_owned(),
            proto_name: name.to_owned(),
            package: package.to_owned(),
            comments: Comments::default(),
            methods,
            options: prost_types::ServiceOptions::default(),
        }
    }

    #[test]
    fn a_service_renders_its_full_name_and_every_rpc() {
        let invoices = service(
            "billing.v1",
            "Invoices",
            vec![
                method(
                    "GetInvoice",
                    (".billing.v1.GetInvoiceRequest", "GetInvoiceRequest"),
                    (".billing.v1.Invoice", "Invoice"),
                    (false, false),
                ),
                method(
                    "Total",
                    (".billing.v1.GetInvoiceRequest", "GetInvoiceRequest"),
                    (".common.v1.Money", "super::super::common::v1::Money"),
                    (false, false),
                ),
                method(
                    "Watch",
                    (".billing.v1.GetInvoiceRequest", "GetInvoiceRequest"),
                    (".billing.v1.Invoice.Line", "invoice::Line"),
                    (false, true),
                ),
                method(
                    "Upload",
                    (
                        ".google.protobuf.BytesValue",
                        "::prost::alloc::vec::Vec<u8>",
                    ),
                    (".google.protobuf.Empty", "()"),
                    (true, false),
                ),
            ],
        );
        assert_eq!(
            render(&invoices),
            "/// Contract of `billing.v1.Invoices`, checked by `#[component(proto = …)]`.\n\
             #[doc(hidden)]\n\
             #[allow(non_upper_case_globals, dead_code)]\n\
             pub const __sekvent_service_Invoices: (&str, &[(&str, &str, &str, bool)]) = (\n\
             \x20   \"billing.v1.Invoices\",\n\
             \x20   &[\n\
             \x20       (\"GetInvoice\", \"billing.v1.GetInvoiceRequest\", \"billing.v1.Invoice\", false),\n\
             \x20       (\"Total\", \"billing.v1.GetInvoiceRequest\", \"common.v1.Money\", false),\n\
             \x20       (\"Watch\", \"billing.v1.GetInvoiceRequest\", \"billing.v1.Invoice.Line\", true),\n\
             \x20       (\"Upload\", \"google.protobuf.BytesValue\", \"google.protobuf.Empty\", true),\n\
             \x20   ],\n\
             );\n\
             /// Request and reply of `billing.v1.Invoices.GetInvoice`, checked by `#[component(proto = …)]`.\n\
             #[doc(hidden)]\n\
             #[allow(non_camel_case_types, dead_code)]\n\
             pub type __sekvent_rpc_Invoices__GetInvoice = (GetInvoiceRequest, Invoice);\n\
             /// Request and reply of `billing.v1.Invoices.Total`, checked by `#[component(proto = …)]`.\n\
             #[doc(hidden)]\n\
             #[allow(non_camel_case_types, dead_code)]\n\
             pub type __sekvent_rpc_Invoices__Total = (GetInvoiceRequest, super::super::common::v1::Money);\n\
             /// Request and reply of `billing.v1.Invoices.Watch`, checked by `#[component(proto = …)]`.\n\
             #[doc(hidden)]\n\
             #[allow(non_camel_case_types, dead_code)]\n\
             pub type __sekvent_rpc_Invoices__Watch = (GetInvoiceRequest, invoice::Line);\n\
             /// Request and reply of `billing.v1.Invoices.Upload`, checked by `#[component(proto = …)]`.\n\
             #[doc(hidden)]\n\
             #[allow(non_camel_case_types, dead_code)]\n\
             pub type __sekvent_rpc_Invoices__Upload = (::prost::alloc::vec::Vec<u8>, ());\n"
        );
    }

    #[test]
    fn a_nested_message_keeps_its_enclosing_message_in_both_names() {
        let orders = service(
            "shop.v1",
            "Orders",
            vec![
                method(
                    "Nested",
                    (".shop.v1.Order.Line", "order::Line"),
                    (".shop.v1.Line", "Line"),
                    (false, false),
                ),
                method(
                    "Deep",
                    (".shop.v1.Order.Line.Tax", "order::line::Tax"),
                    (".other.v1.Line", "super::super::other::v1::Line"),
                    (false, false),
                ),
            ],
        );
        let rendered = render(&orders);
        for needle in [
            "(\"Nested\", \"shop.v1.Order.Line\", \"shop.v1.Line\", false),",
            "(\"Deep\", \"shop.v1.Order.Line.Tax\", \"other.v1.Line\", false),",
            "pub type __sekvent_rpc_Orders__Nested = (order::Line, Line);",
            "pub type __sekvent_rpc_Orders__Deep = (order::line::Tax, super::super::other::v1::Line);",
        ] {
            assert!(
                rendered.contains(needle),
                "missing `{needle}` in {rendered}"
            );
        }
    }

    #[test]
    fn a_service_without_a_package_or_rpcs_renders_bare_names() {
        assert_eq!(
            render(&service("", "Idle", Vec::new())),
            "/// Contract of `Idle`, checked by `#[component(proto = …)]`.\n\
             #[doc(hidden)]\n\
             #[allow(non_upper_case_globals, dead_code)]\n\
             pub const __sekvent_service_Idle: (&str, &[(&str, &str, &str, bool)]) = (\n\
             \x20   \"Idle\",\n\
             \x20   &[],\n\
             );\n"
        );
        let loose = service(
            "",
            "Loose",
            vec![method(
                "Get",
                (".Req", "Req"),
                ("Rep", "Rep"),
                (false, false),
            )],
        );
        let rendered = render(&loose);
        assert!(
            rendered.contains("(\"Get\", \"Req\", \"Rep\", false),"),
            "{rendered}"
        );
        assert!(
            rendered.contains("/// Request and reply of `Loose.Get`"),
            "{rendered}"
        );
        assert!(
            rendered.contains("pub type __sekvent_rpc_Loose__Get = (Req, Rep);"),
            "{rendered}"
        );
    }

    #[test]
    fn rpc_type_aliases_separate_service_and_rpc() {
        assert_eq!(rpc_types_name("Orders", "Get"), "__sekvent_rpc_Orders__Get");
        assert_ne!(rpc_types_name("A_B", "C"), rpc_types_name("A", "B_C"));
    }

    #[test]
    fn the_generator_appends_one_constant_per_service() {
        let mut generator = ServiceContracts;
        let mut buf = String::from("pub struct Before;\n");
        generator.generate(service("a.v1", "First", Vec::new()), &mut buf);
        generator.generate(service("a.v1", "Second", Vec::new()), &mut buf);
        assert!(buf.starts_with("pub struct Before;\n/// Contract of `a.v1.First`"));
        let first = buf.find("__sekvent_service_First:").unwrap();
        let second = buf.find("__sekvent_service_Second:").unwrap();
        assert!(first < second, "{buf}");
    }
}
