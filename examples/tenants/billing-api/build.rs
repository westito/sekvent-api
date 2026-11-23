//! Generates the message types of `billing.invoices.v1` and `billing.admin.v1`
//! and their service contract constants (checked by `#[component(proto = …)]`)
//! from `proto/`.

fn main() {
    sekvent_proto_build::ProtoBuild::new("proto")
        .messages_only()
        .compile()
        .unwrap_or_else(|error| panic!("{error}"));
}
