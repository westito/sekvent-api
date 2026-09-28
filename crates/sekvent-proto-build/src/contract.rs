use std::fmt::Write as _;

use prost_build::{Service, ServiceGenerator};

use crate::SERVICE_CONTRACT_PREFIX;

/// Appends `__sekvent_service_<Service>` after every service's position in
/// its package's generated file: the service's full name and, per RPC, its
/// name, request and reply full names and whether it streams. The
/// `#[component(proto = …)]` macro checks a component trait against it.
pub(crate) struct ServiceContracts;

impl ServiceGenerator for ServiceContracts {
    fn generate(&mut self, service: Service, buf: &mut String) {
        buf.push_str(&render(&service));
    }
}

/// The constant for `service`, as Rust source.
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
    out
}

/// `package.name`, or `name` alone in the empty package.
fn qualified(package: &str, name: &str) -> String {
    if package.is_empty() {
        name.to_owned()
    } else {
        format!("{package}.{name}")
    }
}

/// A descriptor's fully qualified type (`.shop.v1.Item`) without the
/// leading dot, as `prost::Name` spells it.
fn type_name(proto_type: &str) -> &str {
    proto_type.strip_prefix('.').unwrap_or(proto_type)
}

#[cfg(test)]
mod tests {
    use prost_build::{Comments, Method};

    use super::*;

    fn method(name: &str, input: &str, output: &str, streaming: (bool, bool)) -> Method {
        Method {
            name: name.to_lowercase(),
            proto_name: name.to_owned(),
            comments: Comments::default(),
            input_type: String::new(),
            output_type: String::new(),
            input_proto_type: input.to_owned(),
            output_proto_type: output.to_owned(),
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
                    ".billing.v1.GetInvoiceRequest",
                    ".billing.v1.Invoice",
                    (false, false),
                ),
                method(
                    "Total",
                    ".billing.v1.GetInvoiceRequest",
                    ".common.v1.Money",
                    (false, false),
                ),
                method(
                    "Watch",
                    ".billing.v1.GetInvoiceRequest",
                    ".billing.v1.Invoice.Line",
                    (false, true),
                ),
                method(
                    "Upload",
                    ".google.protobuf.BytesValue",
                    ".google.protobuf.Empty",
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
             );\n"
        );
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
            vec![method("Get", ".Req", "Rep", (false, false))],
        );
        assert!(
            render(&loose).contains("(\"Get\", \"Req\", \"Rep\", false),"),
            "{}",
            render(&loose)
        );
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
