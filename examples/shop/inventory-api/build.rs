//! Generates the message types of `shop.inventory.v1` and its service contract
//! constant (checked by `#[component(proto = …)]`) from `proto/`.

fn main() {
    sekvent_proto_build::ProtoBuild::new("proto")
        .messages_only()
        .compile()
        .unwrap_or_else(|error| panic!("{error}"));
}
