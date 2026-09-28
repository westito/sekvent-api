//! Compile-time checks that a component trait and its proto service agree.
//!
//! `#[component(proto = ...)]` evaluates [`assert_service`] once and
//! [`assert_rpc`] and [`assert_rpc_types`] once per method in `const` items,
//! so a mismatch fails the build with one of the messages below. Together
//! they enforce an exact one-to-one mapping between the trait's methods and
//! the service's RPCs, with identical request and reply types.

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

/// The Rust request and reply types of one RPC, as sekvent-proto-build
/// emits them: `__sekvent_rpc_<Service>__<Rpc> = (Request, Reply)`.
pub trait RpcTypes {
    /// The RPC's input type.
    type Request;
    /// The RPC's output type.
    type Reply;
}

impl<Req, Rep> RpcTypes for (Req, Rep) {
    type Request = Req;
    type Reply = Rep;
}

/// Implemented only by `Proto` itself.
#[diagnostic::on_unimplemented(
    message = "this method's request type `{Self}` is not the RPC's input type `{Proto}`",
    label = "the proto service expects `{Proto}` here",
    note = "a nested message or a message of another package is a different type even when its name matches"
)]
pub trait SameRequest<Proto> {}
impl<T> SameRequest<T> for T {}

/// Implemented only by `Proto` itself.
#[diagnostic::on_unimplemented(
    message = "this method's reply type `{Self}` is not the RPC's output type `{Proto}`",
    label = "the proto service expects `{Proto}` here",
    note = "`google.protobuf.Empty` is `()`; a nested message or a message of another package is a different type even when its name matches"
)]
pub trait SameReply<Proto> {}
impl<T> SameReply<T> for T {}

/// Const-panics unless `proto.0 == service`, neither side names an RPC
/// twice and every RPC of `proto` is in `rpcs` (the trait's RPC names, one
/// per method). [`assert_rpc`] checks the other direction per method.
pub const fn assert_service(proto: ProtoService, service: &str, rpcs: &[&str]) {
    assert!(
        str_eq(proto.0, service),
        "component contract: the proto service is not <package>.<Trait>; check package and proto"
    );
    let mut index = 0;
    while index < rpcs.len() {
        assert!(
            count(rpcs, rpcs[index]) == 1,
            "component contract: two methods of the trait map to the same RPC name"
        );
        index += 1;
    }
    let declared = proto.1;
    let mut index = 0;
    while index < declared.len() {
        let name = declared[index].0;
        assert!(
            count_rpc(declared, name) == 1,
            "component contract: the proto service declares an RPC name twice"
        );
        assert!(
            count(rpcs, name) == 1,
            "component contract: the proto service has an RPC the trait has no method for"
        );
        index += 1;
    }
}

/// Const-panics unless `proto` has a non-streaming RPC `rpc` whose request
/// and reply full names agree with `Req` and `Rep` (see [`full_name_eq`]).
/// [`assert_rpc_types`] then checks that the types are the very ones the
/// proto service declares.
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

/// Compiles only when `Req` and `Rep` are exactly the request and reply
/// types of `Rpc`, the `(Request, Reply)` alias sekvent-proto-build emits
/// for the RPC. This tells a nested message from a top-level message of the
/// same name, which protobuf names alone cannot do in `const` evaluation.
pub const fn assert_rpc_types<Req, Rep, Rpc>()
where
    Rpc: RpcTypes,
    Req: SameRequest<Rpc::Request>,
    Rep: SameReply<Rpc::Reply>,
{
}

/// How many RPCs of `rpcs` are named `name`.
const fn count_rpc(rpcs: &[ProtoRpc], name: &str) -> usize {
    let mut found = 0;
    let mut index = 0;
    while index < rpcs.len() {
        if str_eq(rpcs[index].0, name) {
            found += 1;
        }
        index += 1;
    }
    found
}

/// How many entries of `names` are `name`.
const fn count(names: &[&str], name: &str) -> usize {
    let mut found = 0;
    let mut index = 0;
    while index < names.len() {
        if str_eq(names[index], name) {
            found += 1;
        }
        index += 1;
    }
    found
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

/// Whether `full` can be the full name of a message whose `prost::Name` is
/// (`package`, `name`): `package.` (unless `package` is empty), then any
/// enclosing messages (`Outer.`), then `name`. prost gives a nested message
/// its leaf name and its file's package, so the enclosing messages are not
/// visible here; [`assert_rpc_types`] settles those by type identity.
const fn full_name_eq(full: &str, package: &str, name: &str) -> bool {
    let (full, package, name) = (full.as_bytes(), package.as_bytes(), name.as_bytes());
    let start = if package.is_empty() {
        0
    } else {
        if full.len() <= package.len() + 1
            || !bytes_eq_at(full, 0, package)
            || full[package.len()] != b'.'
        {
            return false;
        }
        package.len() + 1
    };
    let rest = full.len() - start;
    if rest == name.len() {
        return bytes_eq_at(full, start, name);
    }
    rest > name.len()
        && full[full.len() - name.len() - 1] == b'.'
        && bytes_eq_at(full, full.len() - name.len(), name)
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

    /// `shop.v1.Answer.Line`, as prost names a nested message.
    #[derive(Clone, PartialEq, prost::Message)]
    struct Line {}
    impl prost::Name for Line {
        const NAME: &'static str = "Line";
        const PACKAGE: &'static str = "shop.v1";
    }

    /// A top-level `shop.v1.Line`, which only differs from the nested one
    /// by type.
    mod top {
        #[derive(Clone, PartialEq, prost::Message)]
        pub(super) struct Line {}
        impl prost::Name for Line {
            const NAME: &'static str = "Line";
            const PACKAGE: &'static str = "shop.v1";
        }
    }

    const SERVICE: ProtoService = (
        "shop.v1.Desk",
        &[
            ("Ask", "shop.v1.Ask", "shop.v1.Answer", false),
            ("Stream", "shop.v1.Ask", "shop.v1.Answer", true),
            ("Plain", "Bare", "common.money.v1.Money", false),
            ("Lines", "shop.v1.Ask", "shop.v1.Answer.Line", false),
            ("Clear", "shop.v1.Ask", "google.protobuf.Empty", false),
            (
                "Touch",
                "google.protobuf.Empty",
                "google.protobuf.Empty",
                false,
            ),
        ],
    );

    const ALL: &[&str] = &["Ask", "Stream", "Plain", "Lines", "Clear", "Touch"];

    // The passing cases are checked where the macro checks them: in consts.
    const _: () = {
        assert_service(SERVICE, "shop.v1.Desk", ALL);
        assert_rpc::<Ask, Answer>(SERVICE, "Ask");
        assert_rpc::<Bare, Money>(SERVICE, "Plain");
        assert_rpc::<Ask, Line>(SERVICE, "Lines");
        assert_rpc::<Ask, ()>(SERVICE, "Clear");
        assert_rpc::<(), ()>(SERVICE, "Touch");
        assert_rpc_types::<Ask, Answer, (Ask, Answer)>();
        assert_rpc_types::<Ask, Line, (Ask, Line)>();
        assert_rpc_types::<(), (), ((), ())>();
    };

    #[test]
    fn passing_checks_also_run_at_run_time() {
        assert_service(SERVICE, "shop.v1.Desk", ALL);
        assert_service(
            SERVICE,
            "shop.v1.Desk",
            &["Touch", "Clear", "Lines", "Plain", "Stream", "Ask"],
        );
        assert_rpc::<Ask, Answer>(SERVICE, "Ask");
        assert_rpc::<Bare, Money>(SERVICE, "Plain");
        assert_rpc::<Ask, Line>(SERVICE, "Lines");
        assert_rpc::<Ask, ()>(SERVICE, "Clear");
        assert_rpc::<(), ()>(SERVICE, "Touch");
        assert_rpc_types::<Ask, top::Line, (Ask, top::Line)>();
        assert_service(("Idle", &[]), "Idle", &[]);
    }

    #[test]
    #[should_panic(
        expected = "component contract: the proto service is not <package>.<Trait>; check package and proto"
    )]
    fn another_service_name() {
        assert_service(SERVICE, "shop.v2.Desk", ALL);
    }

    #[test]
    #[should_panic(
        expected = "component contract: the proto service is not <package>.<Trait>; check package and proto"
    )]
    fn a_service_name_of_another_length() {
        assert_service(SERVICE, "shop.v1.Desks", ALL);
    }

    #[test]
    #[should_panic(
        expected = "component contract: the proto service has an RPC the trait has no method for"
    )]
    fn an_rpc_without_a_method() {
        assert_service(
            SERVICE,
            "shop.v1.Desk",
            &["Ask", "Stream", "Plain", "Lines", "Clear"],
        );
    }

    #[test]
    #[should_panic(
        expected = "component contract: two methods of the trait map to the same RPC name"
    )]
    fn a_method_counted_twice_does_not_hide_a_missing_rpc() {
        // Same count as the proto service, but `Touch` is replaced by a
        // second `Ask`.
        assert_service(
            SERVICE,
            "shop.v1.Desk",
            &["Ask", "Stream", "Plain", "Lines", "Clear", "Ask"],
        );
    }

    #[test]
    #[should_panic(
        expected = "component contract: two methods of the trait map to the same RPC name"
    )]
    fn two_methods_with_one_rpc_name() {
        assert_service(
            ("a.v1.S", &[("Get", "a.v1.R", "a.v1.R", false)]),
            "a.v1.S",
            &["Get", "Get"],
        );
    }

    #[test]
    #[should_panic(expected = "component contract: the proto service declares an RPC name twice")]
    fn a_proto_service_with_one_rpc_name_twice() {
        assert_service(
            (
                "a.v1.S",
                &[
                    ("Get", "a.v1.R", "a.v1.R", false),
                    ("Get", "a.v1.R", "a.v1.R", false),
                ],
            ),
            "a.v1.S",
            &["Get"],
        );
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
    #[should_panic(
        expected = "component contract: this method's reply type is not the RPC's output type"
    )]
    fn a_unit_reply_is_empty_and_nothing_else() {
        assert_rpc::<Ask, ()>(SERVICE, "Ask");
    }

    #[test]
    #[should_panic(
        expected = "component contract: this method's request type is not the RPC's input type"
    )]
    fn a_message_of_another_package_with_the_same_name() {
        assert_rpc::<Money, Answer>(
            (
                "x.v1.S",
                &[("Ask", "billing.v1.Money", "shop.v1.Answer", false)],
            ),
            "Ask",
        );
    }

    #[test]
    fn full_names() {
        assert!(full_name_eq("a.b.C", "a.b", "C"));
        assert!(full_name_eq("C", "", "C"));
        assert!(full_name_eq("a.b.Outer.C", "a.b", "C"));
        assert!(full_name_eq("a.b.Outer.Mid.C", "a.b", "C"));
        assert!(full_name_eq("Outer.C", "", "C"));
        assert!(!full_name_eq("a.b.C", "", "D"));
        assert!(!full_name_eq("a.b.OuterC", "a.b", "C"));
        assert!(!full_name_eq("a.bxC", "a.b", "C"));
        assert!(!full_name_eq("a.c.C", "a.b", "C"));
        assert!(!full_name_eq("a.b.D", "a.b", "C"));
        assert!(!full_name_eq("a.b.CC", "a.b", "C"));
        assert!(!full_name_eq("a.b.", "a.b", "C"));
        assert!(!full_name_eq("a.b", "a.b", "C"));
        assert!(!full_name_eq("", "", "C"));
        assert!(!full_name_eq("XC", "", "C"));
        assert!(str_eq("", ""));
        assert!(!str_eq("a", "b"));
    }

    #[test]
    fn counts() {
        assert_eq!(count(&["a", "b", "a"], "a"), 2);
        assert_eq!(count(&[], "a"), 0);
        assert_eq!(count_rpc(SERVICE.1, "Ask"), 1);
        assert_eq!(count_rpc(SERVICE.1, "Nope"), 0);
    }
}
