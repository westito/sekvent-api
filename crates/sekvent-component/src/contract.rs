//! Compile-time checks that a component trait and its proto service agree.
//!
//! `#[component(proto = ...)]` evaluates [`assert_service`] and one
//! [`assert_rpc`] per method in `const` items, so a mismatch fails the build
//! with one of the messages below.

use crate::__private::WireMessage;

/// One RPC as sekvent-proto-build emits it: (RPC name, request full name,
/// reply full name, streaming).
pub type ProtoRpc = (&'static str, &'static str, &'static str, bool);
/// A proto service as sekvent-proto-build emits it: (full name, RPCs).
pub type ProtoService = (&'static str, &'static [ProtoRpc]);

/// A request or reply that knows its protobuf type name.
#[diagnostic::on_unimplemented(
    message = "`{Self}` has no protobuf type name",
    note = "compile the messages with sekvent-proto-build, which enables prost's type names, or implement `prost::Name`"
)]
pub trait ContractMessage: WireMessage + prost::Name {}
impl<T: WireMessage + prost::Name> ContractMessage for T {}

/// Const-panics unless `proto.0 == service` and `proto.1.len() == methods`.
pub const fn assert_service(proto: ProtoService, service: &str, methods: usize) {
    assert!(
        str_eq(proto.0, service),
        "component contract: the proto service is not <package>.<Trait>; check package and proto"
    );
    assert!(
        proto.1.len() == methods,
        "component contract: the proto service and the trait declare different sets of RPCs"
    );
}

/// Const-panics unless `proto` has a non-streaming RPC `rpc` whose request
/// is `Req` and whose reply is `Rep` (full names from `prost::Name`; an
/// empty `PACKAGE` means the bare `NAME`).
pub const fn assert_rpc<Req: ContractMessage, Rep: ContractMessage>(
    proto: ProtoService,
    rpc: &str,
) {
    let rpcs = proto.1;
    let mut index = 0;
    while index < rpcs.len() {
        let (name, request, reply, streaming) = rpcs[index];
        if str_eq(name, rpc) {
            assert!(
                !streaming,
                "component contract: the RPC named after this method is streaming; component methods are unary"
            );
            assert!(
                full_name_eq(request, Req::PACKAGE, Req::NAME),
                "component contract: this method's request type is not the RPC's input type"
            );
            assert!(
                full_name_eq(reply, Rep::PACKAGE, Rep::NAME),
                "component contract: this method's reply type is not the RPC's output type"
            );
            return;
        }
        index += 1;
    }
    panic!("component contract: the proto service has no RPC named after this method");
}

/// Byte-wise string equality, usable in `const` evaluation.
const fn str_eq(left: &str, right: &str) -> bool {
    let (left, right) = (left.as_bytes(), right.as_bytes());
    if left.len() != right.len() {
        return false;
    }
    bytes_eq_at(left, 0, right)
}

/// Whether `haystack[at..at + needle.len()] == needle`; `haystack` must be
/// long enough.
const fn bytes_eq_at(haystack: &[u8], at: usize, needle: &[u8]) -> bool {
    let mut index = 0;
    while index < needle.len() {
        if haystack[at + index] != needle[index] {
            return false;
        }
        index += 1;
    }
    true
}

/// Whether `full` is `package.name`, or `name` when `package` is empty.
const fn full_name_eq(full: &str, package: &str, name: &str) -> bool {
    if package.is_empty() {
        return str_eq(full, name);
    }
    let (full, package, name) = (full.as_bytes(), package.as_bytes(), name.as_bytes());
    full.len() == package.len() + 1 + name.len()
        && bytes_eq_at(full, 0, package)
        && full[package.len()] == b'.'
        && bytes_eq_at(full, package.len() + 1, name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone, PartialEq, prost::Message)]
    struct Ask {}
    impl prost::Name for Ask {
        const NAME: &'static str = "Ask";
        const PACKAGE: &'static str = "shop.v1";
    }

    #[derive(Clone, PartialEq, prost::Message)]
    struct Answer {}
    impl prost::Name for Answer {
        const NAME: &'static str = "Answer";
        const PACKAGE: &'static str = "shop.v1";
    }

    #[derive(Clone, PartialEq, prost::Message)]
    struct Bare {}
    impl prost::Name for Bare {
        const NAME: &'static str = "Bare";
        const PACKAGE: &'static str = "";
    }

    #[derive(Clone, PartialEq, prost::Message)]
    struct Money {}
    impl prost::Name for Money {
        const NAME: &'static str = "Money";
        const PACKAGE: &'static str = "common.money.v1";
    }

    const SERVICE: ProtoService = (
        "shop.v1.Desk",
        &[
            ("Ask", "shop.v1.Ask", "shop.v1.Answer", false),
            ("Stream", "shop.v1.Ask", "shop.v1.Answer", true),
            ("Plain", "Bare", "common.money.v1.Money", false),
        ],
    );

    // The passing cases are checked where the macro checks them: in consts.
    const _: () = {
        assert_service(SERVICE, "shop.v1.Desk", 3);
        assert_rpc::<Ask, Answer>(SERVICE, "Ask");
        assert_rpc::<Bare, Money>(SERVICE, "Plain");
    };

    #[test]
    fn passing_checks_also_run_at_run_time() {
        assert_service(SERVICE, "shop.v1.Desk", 3);
        assert_rpc::<Ask, Answer>(SERVICE, "Ask");
        assert_rpc::<Bare, Money>(SERVICE, "Plain");
    }

    #[test]
    #[should_panic(
        expected = "component contract: the proto service is not <package>.<Trait>; check package and proto"
    )]
    fn another_service_name() {
        assert_service(SERVICE, "shop.v2.Desk", 3);
    }

    #[test]
    #[should_panic(
        expected = "component contract: the proto service is not <package>.<Trait>; check package and proto"
    )]
    fn a_service_name_of_another_length() {
        assert_service(SERVICE, "shop.v1.Desks", 3);
    }

    #[test]
    #[should_panic(
        expected = "component contract: the proto service and the trait declare different sets of RPCs"
    )]
    fn another_rpc_count() {
        assert_service(SERVICE, "shop.v1.Desk", 2);
    }

    #[test]
    #[should_panic(
        expected = "component contract: the proto service has no RPC named after this method"
    )]
    fn a_missing_rpc() {
        assert_rpc::<Ask, Answer>(SERVICE, "Tell");
    }

    #[test]
    #[should_panic(
        expected = "component contract: the RPC named after this method is streaming; component methods are unary"
    )]
    fn a_streaming_rpc() {
        assert_rpc::<Ask, Answer>(SERVICE, "Stream");
    }

    #[test]
    #[should_panic(
        expected = "component contract: this method's request type is not the RPC's input type"
    )]
    fn another_request() {
        assert_rpc::<Answer, Answer>(SERVICE, "Ask");
    }

    #[test]
    #[should_panic(
        expected = "component contract: this method's reply type is not the RPC's output type"
    )]
    fn another_reply() {
        assert_rpc::<Ask, Ask>(SERVICE, "Ask");
    }

    #[test]
    fn full_names() {
        assert!(full_name_eq("a.b.C", "a.b", "C"));
        assert!(full_name_eq("C", "", "C"));
        assert!(!full_name_eq("a.b.C", "", "C"));
        assert!(!full_name_eq("a.bxC", "a.b", "C"));
        assert!(!full_name_eq("a.c.C", "a.b", "C"));
        assert!(!full_name_eq("a.b.D", "a.b", "C"));
        assert!(!full_name_eq("a.b.CC", "a.b", "C"));
        assert!(str_eq("", ""));
        assert!(!str_eq("a", "b"));
    }
}
