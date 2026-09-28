//! Unit and snapshot tests of `#[component]` and `#[derive(ComponentError)]`.

use proc_macro2::TokenStream;
use quote::quote;
use syn::{DeriveInput, parse_quote};

use super::parse::{
    borrows_or_impl, bulkhead_value, is_component_name, is_package, is_snake_name, proto_path,
    rpc_name, timeout_value,
};

fn runtime() -> TokenStream {
    quote!(::sekvent_component)
}

fn compact(text: &str) -> String {
    text.chars().filter(|c| !c.is_whitespace()).collect()
}

fn standard_args() -> TokenStream {
    quote!(
        name = "echo",
        package = "check.echo.v1",
        proto = "crate::proto"
    )
}

fn ping() -> TokenStream {
    quote! {
        #[call]
        async fn ping(&self, cx: &CallContext, req: PingRequest) -> Result<PingReply, AppError>;
    }
}

fn expand_ok(args: TokenStream, item: TokenStream) -> String {
    match super::expand(args, item, runtime) {
        Ok(tokens) => compact(&tokens.to_string()),
        Err(error) => panic!("unexpected error: {error}"),
    }
}

fn messages(args: TokenStream, item: TokenStream) -> Vec<String> {
    match super::expand(args, item, runtime) {
        Ok(tokens) => panic!("expected an error, got {tokens}"),
        Err(error) => error.into_iter().map(|error| error.to_string()).collect(),
    }
}

fn assert_error(args: TokenStream, item: TokenStream, expected: &str) {
    let all = messages(args, item);
    assert!(
        all.iter().any(|message| message == expected),
        "`{expected}` not in {all:?}"
    );
}

fn assert_trait_error(item: TokenStream, expected: &str) {
    assert_error(standard_args(), item, expected);
}

fn assert_method_error(method: &TokenStream, expected: &str) {
    assert_trait_error(quote!(pub trait Echo { #method }), expected);
}

fn pretty(tokens: TokenStream) -> String {
    let file: syn::File = match syn::parse2(tokens) {
        Ok(file) => file,
        Err(error) => panic!("the expansion is not a file: {error}"),
    };
    prettyplease::unparse(&file)
}

fn snapshot(name: &str, tokens: TokenStream) {
    let text = pretty(tokens);
    let mut settings = insta::Settings::clone_current();
    settings.set_prepend_module_to_snapshot(false);
    settings.bind(|| insta::assert_snapshot!(name, text));
}

fn component_snapshot(name: &str, args: TokenStream, item: TokenStream) {
    match super::expand(args, item, runtime) {
        Ok(tokens) => snapshot(name, tokens),
        Err(error) => panic!("unexpected error: {error}"),
    }
}

fn component_error_snapshot(name: &str, input: &DeriveInput) {
    match crate::component_error::expand(input, runtime) {
        Ok(tokens) => snapshot(name, tokens),
        Err(error) => panic!("unexpected error: {error}"),
    }
}

// --- snapshots -------------------------------------------------------------

#[test]
fn snapshot_standard() {
    component_snapshot(
        "component__standard",
        quote!(
            name = "inventory",
            package = "shop.inventory.v1",
            proto = "crate::proto::shop::inventory::v1"
        ),
        quote! {
            pub trait Inventory: Send + Sync + 'static {
                /// Reserve stock for an order.
                #[call(idempotent, timeout = "2s", bulkhead = 16)]
                async fn reserve(&self, cx: &CallContext, req: ReserveRequest)
                    -> Result<ReserveReply, InventoryError>;

                /// Release a reservation.
                #[call(timeout = "500ms")]
                async fn release(&self, cx: &CallContext, req: ReleaseRequest)
                    -> Result<ReleaseReply, InventoryError>;
            }
        },
    );
}

#[test]
fn snapshot_local_only() {
    component_snapshot(
        "component__local_only",
        quote!(name = "notes", local_only),
        quote! {
            pub trait Notes {
                /// Store a note and return how many are stored.
                #[call(bulkhead = 4)]
                async fn add(&self, cx: &CallContext, req: String) -> Result<usize, AppError>;

                /// Count the stored notes.
                #[call(idempotent)]
                async fn count(&self, _: &CallContext, _: ()) -> Result<usize, AppError>;
            }
        },
    );
}

#[test]
fn snapshot_remote_only() {
    component_snapshot(
        "component__remote_only",
        quote!(
            name = "ledger",
            package = "shop.ledger.v1",
            proto = "::ledger_api::proto::shop::ledger::v1",
            remote_only
        ),
        quote! {
            pub(crate) trait Ledger: Send {
                /// Record an entry.
                #[call(idempotent, timeout = "1m 30s")]
                async fn record(&self, cx: &CallContext, req: RecordRequest)
                    -> Result<RecordReply, LedgerError>;
            }
        },
    );
}

#[test]
fn snapshot_component_error_inventory() {
    let input: DeriveInput = parse_quote! {
        #[component_error(domain = "shop.inventory.v1")]
        pub enum InventoryError {
            #[reason("OUT_OF_STOCK", code = FailedPrecondition, message = "only {available} of {sku} left")]
            OutOfStock { sku: String, available: u32 },
            #[reason("RESERVATION_NOT_FOUND", code = NotFound)]
            ReservationNotFound { reservation_id: String, hint: Option<String> },
            #[reason("INVENTORY_CLOSED", code = Unavailable)]
            Closed,
            #[other]
            Other(AppError),
        }
    };
    component_error_snapshot("component_error__inventory", &input);
}

#[test]
fn snapshot_component_error_no_domain() {
    let input: DeriveInput = parse_quote! {
        pub enum NotesError {
            #[reason("NOTE_TOO_LONG", code = InvalidArgument, message = "a note has at most {limit} characters")]
            TooLong { limit: usize },
            #[reason("NOTES_FULL", code = ResourceExhausted)]
            Full {},
            #[reason("NOTE_MISSING", code = NotFound)]
            Missing { id: u64, author: Option<String>, tag: Option<String> },
            #[other]
            Other(AppError),
        }
    };
    component_error_snapshot("component_error__no_domain", &input);
}

// --- validation helpers ----------------------------------------------------

#[test]
fn component_names() {
    for name in ["inventory", "order_history", "a", "v2", "a1_b2"] {
        assert!(is_component_name(name), "{name}");
    }
    assert!(is_component_name(&"a".repeat(48)));
    for name in [
        "",
        "Inventory",
        "1st",
        "_x",
        "a__b",
        "a_",
        "a-b",
        "a.b",
        "r#type",
        "ünï",
    ] {
        assert!(!is_component_name(name), "{name}");
    }
    assert!(!is_component_name(&"a".repeat(49)));
}

#[test]
fn method_names() {
    assert!(is_snake_name("get_invoice"));
    assert!(is_snake_name(&"a".repeat(100)));
    assert!(!is_snake_name("getInvoice"));
    assert!(!is_snake_name("get__invoice"));
    assert!(!is_snake_name("get_"));
}

#[test]
fn packages() {
    for package in ["shop.inventory.v1", "a", "a_b.c2", "x.y_"] {
        assert!(is_package(package), "{package}");
    }
    for package in [
        "", ".", "shop..v1", "shop.", ".shop", "Shop.v1", "shop.1v", "shop-x",
    ] {
        assert!(!is_package(package), "{package}");
    }
}

#[test]
fn rpc_names_are_upper_camel_case() {
    assert_eq!(rpc_name("reserve"), "Reserve");
    assert_eq!(rpc_name("get_invoice"), "GetInvoice");
    assert_eq!(rpc_name("v2_list_all"), "V2ListAll");
    assert_eq!(rpc_name("a1b"), "A1b");
}

#[test]
fn the_token_scan_finds_borrows_lifetimes_and_impl() {
    for ty in [
        quote!(&str),
        quote!(Vec<&'static str>),
        quote!(Cow<'static, str>),
        quote!(impl Message),
        quote!(Box<dyn Fn(impl Sized)>),
        quote!((u8, [&u8; 2])),
    ] {
        assert!(borrows_or_impl(ty.clone()), "{ty}");
    }
    for ty in [
        quote!(String),
        quote!(Vec<u8>),
        quote!(::std::collections::BTreeMap<String, Vec<u32>>),
        quote!((u8, [char; 2])),
        quote!(Implied),
    ] {
        assert!(!borrows_or_impl(ty.clone()), "{ty}");
    }
}

#[test]
fn timeouts_parse_with_humantime() {
    let parse = |value: syn::Expr| timeout_value(&value).map_err(|error| error.to_string());
    assert_eq!(
        parse(parse_quote!("2s")),
        Ok(std::time::Duration::from_secs(2))
    );
    assert_eq!(
        parse(parse_quote!("1s 500ms")),
        Ok(std::time::Duration::from_millis(1500))
    );
    let bad: [syn::Expr; 5] = [
        parse_quote!("0s"),
        parse_quote!("soon"),
        parse_quote!(""),
        parse_quote!(2),
        parse_quote!(two),
    ];
    for bad in bad {
        assert_eq!(
            parse(bad),
            Err("timeout must be a positive duration such as \"2s\" or \"250ms\"".to_owned())
        );
    }
}

#[test]
fn bulkheads_are_positive_u32() {
    let parse = |value: syn::Expr| bulkhead_value(&value).map_err(|error| error.to_string());
    assert_eq!(parse(parse_quote!(1)), Ok(1));
    assert_eq!(parse(parse_quote!(16u32)), Ok(16));
    assert_eq!(parse(parse_quote!(4294967295)), Ok(u32::MAX));
    let bad: [syn::Expr; 6] = [
        parse_quote!(0),
        parse_quote!(4294967296),
        parse_quote!(-1),
        parse_quote!(16u8),
        parse_quote!("16"),
        parse_quote!(limit),
    ];
    for bad in bad {
        assert_eq!(
            parse(bad),
            Err("bulkhead must be an integer from 1 to 4294967295".to_owned())
        );
    }
}

#[test]
fn durations_are_emitted_as_seconds_and_nanoseconds() {
    let method = |timeout: &str| {
        quote! {
            #[call(timeout = #timeout)]
            async fn ping(&self, cx: &CallContext, req: PingRequest) -> Result<PingReply, AppError>;
        }
    };
    for (timeout, emitted) in [
        ("500ms", "Duration::new(0u64,500000000u32)"),
        ("1h 30m", "Duration::new(5400u64,0u32)"),
        ("2us", "Duration::new(0u64,2000u32)"),
        ("1s 1ns", "Duration::new(1u64,1u32)"),
    ] {
        let method = method(timeout);
        let out = expand_ok(standard_args(), quote!(pub trait Echo { #method }));
        assert!(out.contains(emitted), "{timeout}: {out}");
    }
}

// --- generated shape -------------------------------------------------------

#[test]
fn missing_supertraits_are_added() {
    let ping = ping();
    let out = expand_ok(standard_args(), quote!(pub trait Echo { #ping }));
    assert!(
        out.contains("pubtraitEcho:::core::marker::Send+::core::marker::Sync+'static{"),
        "{out}"
    );
    let out = expand_ok(standard_args(), quote!(trait Echo: Sync { #ping }));
    assert!(
        out.contains("traitEcho:Sync+::core::marker::Send+'static{"),
        "{out}"
    );
    let out = expand_ok(
        standard_args(),
        quote!(trait Echo: 'static + ::core::marker::Send + std::marker::Sync { #ping }),
    );
    assert!(
        out.contains("traitEcho:'static+::core::marker::Send+std::marker::Sync{"),
        "{out}"
    );
}

#[test]
fn an_explicit_crate_path_replaces_the_runtime_path() {
    let ping = ping();
    let out = match super::expand(
        quote!(
            name = "echo",
            package = "check.echo.v1",
            proto = "crate::proto",
            crate = "::fw"
        ),
        quote!(pub trait Echo { #ping }),
        || panic!("the manifest is not consulted"),
    ) {
        Ok(tokens) => compact(&tokens.to_string()),
        Err(error) => panic!("unexpected error: {error}"),
    };
    assert!(!out.contains("sekvent_component"), "{out}");
    for needle in [
        "::fw::__private::Endpoint<dyn__EchoDyn>",
        "impl::fw::ComponentHandleforEchoHandle",
        "impl::fw::__private::Dispatchfor__EchoDispatcher",
        "::fw::__private::assert_wire::<PingRequest>();",
        "::fw::__private::assert_service(",
        "::fw::__private::assert_rpc::<PingRequest,PingReply>(",
        "::fw::__private::assert_rpc_types::<PingRequest,PingReply,crate::proto::__sekvent_rpc_Echo__Ping>();",
    ] {
        assert!(out.contains(needle), "missing `{needle}` in {out}");
    }
}

#[test]
fn wildcard_arguments_get_names_in_generated_code() {
    let out = expand_ok(
        standard_args(),
        quote! {
            pub trait Echo {
                #[call]
                async fn ping(&self, _: &sekvent::CallContext, _: PingRequest)
                    -> ::core::result::Result<PingReply, AppError>;
            }
        },
    );
    for needle in [
        "fnping(&self,_:&sekvent::CallContext,_:PingRequest)",
        "fn__ping<'a>(&'aself,cx:&'asekvent::CallContext,req:PingRequest)",
        "pubfnping<'a>(&'aself,cx:&'asekvent::CallContext,req:PingRequest)",
        "Output=::core::result::Result<PingReply,AppError>",
    ] {
        assert!(out.contains(needle), "missing `{needle}` in {out}");
    }
}

#[test]
fn undocumented_methods_get_a_handle_doc() {
    let ping = ping();
    let out = expand_ok(standard_args(), quote!(pub trait Echo { #ping }));
    assert!(
        out.contains("#[doc=\"Call`ping`onthecomponent.\"]"),
        "{out}"
    );
}

#[test]
fn local_only_descriptors() {
    let item = quote! {
        trait Notes {
            #[call]
            async fn add(&self, cx: &CallContext, req: String) -> Result<usize, AppError>;
        }
    };
    let out = expand_ok(quote!(name = "notes", local_only), item.clone());
    assert!(
        out.contains(
            "ComponentDescriptor::new(\"notes\",\"Notes\",NotesHandle::__METHODS)\
             .with_mode(::sekvent_component::ComponentMode::LocalOnly);"
        ),
        "{out}"
    );
    assert!(out.contains("self.0.call_local(0usize"), "{out}");
    assert!(!out.contains("Dispatch"), "{out}");
    assert!(!out.contains("with_dispatch"), "{out}");
    assert!(out.contains("assert_local::<String>();"), "{out}");
    assert!(out.contains("assert_local::<usize>();"), "{out}");

    let out = expand_ok(quote!(local_only, package = "a.v1", name = "notes"), item);
    assert!(
        out.contains(
            ".with_package(\"a.v1\").with_mode(::sekvent_component::ComponentMode::LocalOnly);"
        ),
        "{out}"
    );
}

#[test]
fn remote_only_has_no_local_install() {
    let ping = ping();
    let out = expand_ok(
        quote!(
            name = "echo",
            package = "check.echo.v1",
            proto = "crate::proto",
            remote_only
        ),
        quote!(pub trait Echo { #ping }),
    );
    assert!(
        out.contains("::sekvent_component::__private::install_remote(app,EchoHandle)"),
        "{out}"
    );
    assert!(!out.contains("install_local"), "{out}");
    assert!(!out.contains("install_with_lifecycle"), "{out}");
    assert!(out.contains("impl::sekvent_component::__private::Dispatchfor__EchoDispatcher"));
    assert!(out.contains(".with_mode(::sekvent_component::ComponentMode::RemoteOnly)"));
    assert!(
        out.contains(
            "const_:()=::sekvent_component::__private::assert_service(\
             crate::proto::__sekvent_service_Echo,\"check.echo.v1.Echo\",&[\"Ping\"],);"
        ),
        "{out}"
    );
}

// --- contract (proto) -------------------------------------------------------

#[test]
fn the_contract_checks_every_method_against_the_proto_service() {
    let out = expand_ok(
        quote!(
            name = "orders",
            package = "shop.orders.v1",
            proto = "::orders_api::proto::shop::orders::v1"
        ),
        quote! {
            pub trait Orders {
                #[call]
                async fn place_order(&self, cx: &CallContext, req: PlaceOrderRequest)
                    -> Result<PlaceOrderReply, AppError>;
                #[call(idempotent)]
                async fn get(&self, cx: &CallContext, req: common::Id)
                    -> Result<::orders_api::Order, AppError>;
            }
        },
    );
    let service = "::orders_api::proto::shop::orders::v1::__sekvent_service_Orders";
    for needle in [
        format!(
            "const_:()=::sekvent_component::__private::assert_service({service},\
             \"shop.orders.v1.Orders\",&[\"PlaceOrder\",\"Get\"],);"
        ),
        format!(
            "const_:()=::sekvent_component::__private::assert_rpc::<PlaceOrderRequest,\
             PlaceOrderReply>({service},\"PlaceOrder\",);"
        ),
        format!(
            "const_:()=::sekvent_component::__private::assert_rpc::<common::Id,\
             ::orders_api::Order>({service},\"Get\",);"
        ),
        "const_:()=::sekvent_component::__private::assert_rpc_types::<PlaceOrderRequest,\
         PlaceOrderReply,::orders_api::proto::shop::orders::v1::__sekvent_rpc_Orders__PlaceOrder>();"
            .to_owned(),
        "const_:()=::sekvent_component::__private::assert_rpc_types::<common::Id,\
         ::orders_api::Order,::orders_api::proto::shop::orders::v1::__sekvent_rpc_Orders__Get>();"
            .to_owned(),
    ] {
        assert!(out.contains(&needle), "missing `{needle}` in {out}");
    }
}

#[test]
fn the_contract_follows_the_assertions_and_uses_the_bare_trait_name() {
    let ping = ping();
    let out = expand_ok(
        quote!(
            name = "echo",
            package = "check.echo.v1",
            proto = "super::pb"
        ),
        quote!(pub trait r#Echo { #ping }),
    );
    let assertions = out
        .find("assert_wire::<PingRequest>")
        .unwrap_or_else(|| panic!("no assertions in {out}"));
    let service = out
        .find("assert_service(super::pb::__sekvent_service_Echo,\"check.echo.v1.Echo\"")
        .unwrap_or_else(|| panic!("no contract in {out}"));
    assert!(assertions < service, "{out}");
    assert!(
        out.contains(
            "assert_rpc_types::<PingRequest,PingReply,super::pb::__sekvent_rpc_Echo__Ping>();"
        ),
        "{out}"
    );
}

#[test]
fn local_only_has_no_contract() {
    let out = expand_ok(
        quote!(name = "notes", package = "a.v1", local_only),
        quote! {
            trait Notes {
                #[call]
                async fn add(&self, cx: &CallContext, req: String) -> Result<usize, AppError>;
            }
        },
    );
    assert!(!out.contains("assert_service"), "{out}");
    assert!(!out.contains("assert_rpc"), "{out}");
    assert!(!out.contains("__sekvent_service_"), "{out}");
    assert!(!out.contains("__sekvent_rpc_"), "{out}");
}

#[test]
fn proto_paths_are_module_paths() {
    let parse = |text: &str| {
        proto_path(&syn::LitStr::new(text, proc_macro2::Span::call_site()))
            .map(|path| compact(&quote!(#path).to_string()))
            .map_err(|error| error.to_string())
    };
    for (text, parsed) in [
        (
            "crate::proto::shop::inventory::v1",
            "crate::proto::shop::inventory::v1",
        ),
        ("::inventory_api::proto", "::inventory_api::proto"),
        ("super::proto", "super::proto"),
        ("self::pb", "self::pb"),
        ("proto", "proto"),
        (" crate :: proto ", "crate::proto"),
    ] {
        assert_eq!(parse(text), Ok(parsed.to_owned()), "{text}");
    }
    let bad = "proto must be a module path such as \"crate::proto::shop::inventory::v1\"";
    for text in [
        "",
        "crate::",
        "crate::proto::",
        "crate::proto::Service<T>",
        "crate::proto::{a, b}",
        "shop.inventory.v1",
        "crate proto",
        "1proto",
        "crate::proto; fn x() {}",
    ] {
        assert_eq!(parse(text), Err(bad.to_owned()), "{text:?}");
    }
}

#[test]
fn c13_to_c15_proto_errors() {
    let ping = ping();
    let item = quote!(pub trait Echo { #ping });
    let missing = "missing `proto = \"...\"`: the module generated for the component's proto \
                   package, such as \"crate::proto::shop::inventory::v1\"; only a local_only \
                   component may omit it";
    let on_local_only = "a local_only component has no contract; remove `proto`";
    let bad = "proto must be a module path such as \"crate::proto::shop::inventory::v1\"";
    let cases = [
        (quote!(name = "echo", package = "check.echo.v1"), missing),
        (
            quote!(name = "echo", package = "check.echo.v1", remote_only),
            missing,
        ),
        (
            quote!(name = "echo", proto = "crate::proto", local_only),
            on_local_only,
        ),
        (
            quote!(
                name = "echo",
                package = "check.echo.v1",
                proto = "check.echo.v1"
            ),
            bad,
        ),
        (
            quote!(
                name = "echo",
                package = "check.echo.v1",
                proto = "crate::proto<u8>"
            ),
            bad,
        ),
        (
            quote!(
                name = "echo",
                package = "check.echo.v1",
                proto = "crate::proto",
                proto = "x"
            ),
            "duplicate component argument `proto`",
        ),
    ];
    for (args, expected) in cases {
        assert_error(args, item.clone(), expected);
    }
    let all = messages(
        quote!(
            name = "echo",
            package = "check.echo.v1",
            proto = crate::proto
        ),
        item,
    );
    assert!(
        all.iter()
            .any(|message| message.contains("expected string literal")),
        "{all:?}"
    );
}

#[test]
fn a_missing_proto_is_the_only_error_of_an_otherwise_valid_component() {
    let ping = ping();
    let all = messages(
        quote!(name = "echo", package = "check.echo.v1"),
        quote!(pub trait Echo { #ping }),
    );
    assert_eq!(all.len(), 1, "{all:?}");
    let all = messages(
        quote!(name = "notes", proto = "crate::proto", local_only),
        quote!(pub trait Notes { #ping }),
    );
    assert_eq!(all.len(), 1, "{all:?}");
}

// --- argument errors (C1-C7) -----------------------------------------------

#[test]
fn c1_only_traits() {
    for item in [
        quote!(
            struct Echo;
        ),
        quote!(
            fn echo() {}
        ),
        quote!(
            impl Echo for X {}
        ),
    ] {
        assert_error(standard_args(), item, "#[component] applies to a trait");
    }
}

#[test]
fn c2_to_c7_argument_errors() {
    let ping = ping();
    let item = quote!(pub trait Echo { #ping });
    let cases = [
        (
            quote!(package = "check.echo.v1"),
            "missing `name = \"...\"`",
        ),
        (
            quote!(name = "Echo", package = "check.echo.v1"),
            "component name must match [a-z][a-z0-9_]*, without `__` or a trailing `_`, at most 48 characters",
        ),
        (
            quote!(name = "echo"),
            "missing `package = \"...\"`; only a local_only component may omit it",
        ),
        (
            quote!(name = "echo", remote_only),
            "missing `package = \"...\"`; only a local_only component may omit it",
        ),
        (
            quote!(name = "echo", package = "Check.Echo"),
            "package must be dot-separated lowercase segments such as `shop.inventory.v1`",
        ),
        (
            quote!(name = "echo", package = "a.v1", local_only, remote_only),
            "`local_only` and `remote_only` exclude each other",
        ),
        (
            quote!(name = "echo", colour = "red"),
            "unknown component argument `colour`; expected name, package, proto, local_only, remote_only or crate",
        ),
        (
            quote!(name = "echo", name = "other"),
            "duplicate component argument `name`",
        ),
        (
            quote!(name = "echo", local_only, local_only),
            "duplicate component argument `local_only`",
        ),
    ];
    for (args, expected) in cases {
        assert_error(args, item.clone(), expected);
    }
}

#[test]
fn argument_value_errors_come_from_syn() {
    let ping = ping();
    let item = quote!(pub trait Echo { #ping });
    for (args, needle) in [
        (
            quote!(name = echo, package = "a.v1"),
            "expected string literal",
        ),
        (
            quote!(name = "echo", package = "a.v1", crate = "not a path"),
            "unexpected token",
        ),
    ] {
        let all = messages(args, item.clone());
        assert!(
            all.iter().any(|message| message.contains(needle)),
            "{all:?}"
        );
    }
}

// --- trait errors (C8-C12) -------------------------------------------------

#[test]
fn c8_to_c12_trait_errors() {
    let ping = ping();
    let cases = [
        (
            quote!(pub trait Echo<T> { #ping }),
            "a component trait cannot have generic parameters or a where clause",
        ),
        (
            quote!(pub trait Echo where Self: Sized { #ping }),
            "a component trait cannot have generic parameters or a where clause",
        ),
        (
            quote!(pub unsafe trait Echo { #ping }),
            "a component trait cannot be unsafe or auto",
        ),
        (
            quote!(pub auto trait Echo { #ping }),
            "a component trait cannot be unsafe or auto",
        ),
        (
            quote!(pub trait Echo: Clone { #ping }),
            "a component trait may only have `Send`, `Sync` and `'static` as supertraits",
        ),
        (
            quote!(pub trait Echo: 'a { #ping }),
            "a component trait may only have `Send`, `Sync` and `'static` as supertraits",
        ),
        (
            quote!(pub trait Echo: my::Send { #ping }),
            "a component trait may only have `Send`, `Sync` and `'static` as supertraits",
        ),
        (
            quote!(pub trait Echo { type Item; #ping }),
            "a component trait may only contain methods",
        ),
        (
            quote!(pub trait Echo { const LIMIT: u8; #ping }),
            "a component trait may only contain methods",
        ),
        (
            quote!(pub trait Echo { tokens!(); #ping }),
            "a component trait may only contain methods",
        ),
        (
            quote!(
                pub trait Echo {}
            ),
            "a component needs at least one method",
        ),
    ];
    for (item, expected) in cases {
        assert_trait_error(item, expected);
    }
}

// --- method errors (M1-M21) ------------------------------------------------

#[test]
fn m1_to_m7_attribute_errors() {
    let signature = quote! {
        async fn ping(&self, cx: &CallContext, req: PingRequest) -> Result<PingReply, AppError>;
    };
    let cases = [
        (quote!(), "method `ping` needs a kind attribute: #[call]"),
        (
            quote!(#[async_call]),
            "#[async_call] is planned for milestone C3 and not available yet",
        ),
        (
            quote!(#[deferred]),
            "#[deferred] is planned for milestone C3 and not available yet",
        ),
        (
            quote!(#[call] #[call]),
            "a method has exactly one kind attribute",
        ),
        (
            quote!(#[call] #[async_call]),
            "a method has exactly one kind attribute",
        ),
        (
            quote!(#[call(retry)]),
            "unknown #[call] argument `retry`; expected idempotent, timeout or bulkhead",
        ),
        (
            quote!(#[call(idempotent, idempotent)]),
            "duplicate #[call] argument `idempotent`",
        ),
        (
            quote!(#[call(timeout = "1s", timeout = "2s")]),
            "duplicate #[call] argument `timeout`",
        ),
        (
            quote!(#[call(timeout = "0ms")]),
            "timeout must be a positive duration such as \"2s\" or \"250ms\"",
        ),
        (
            quote!(#[call(bulkhead = 0)]),
            "bulkhead must be an integer from 1 to 4294967295",
        ),
        (
            quote!(#[call] #[allow(unused)]),
            "only doc comments and the kind attribute are allowed on a component method",
        ),
    ];
    for (attrs, expected) in cases {
        assert_method_error(&quote!(#attrs #signature), expected);
    }
}

#[test]
fn m8_to_m11_signature_errors() {
    let cases = [
        (
            quote!(
                fn ping(&self, cx: &CallContext, req: PingRequest) -> Result<PingReply, AppError>;
            ),
            "component methods must be `async fn`",
        ),
        (
            quote!(
                const async fn ping(
                    &self,
                    cx: &CallContext,
                    req: PingRequest,
                ) -> Result<PingReply, AppError>;
            ),
            "component methods cannot be const, unsafe or extern",
        ),
        (
            quote!(
                async unsafe fn ping(
                    &self,
                    cx: &CallContext,
                    req: PingRequest,
                ) -> Result<PingReply, AppError>;
            ),
            "component methods cannot be const, unsafe or extern",
        ),
        (
            quote!(
                async extern "C" fn ping(
                    &self,
                    cx: &CallContext,
                    req: PingRequest,
                ) -> Result<PingReply, AppError>;
            ),
            "component methods cannot be const, unsafe or extern",
        ),
        (
            quote!(
                async fn ping(
                    &self,
                    cx: &CallContext,
                    req: PingRequest,
                ) -> Result<PingReply, AppError> {
                    todo!()
                }
            ),
            "component methods cannot have a default body",
        ),
        (
            quote!(
                async fn ping<T>(
                    &self,
                    cx: &CallContext,
                    req: PingRequest,
                ) -> Result<PingReply, AppError>;
            ),
            "component methods cannot have generic parameters, lifetimes or a where clause",
        ),
        (
            quote!(
                async fn ping<'a>(
                    &self,
                    cx: &CallContext,
                    req: PingRequest,
                ) -> Result<PingReply, AppError>;
            ),
            "component methods cannot have generic parameters, lifetimes or a where clause",
        ),
        (
            quote!(
                async fn ping(
                    &self,
                    cx: &CallContext,
                    req: PingRequest,
                ) -> Result<PingReply, AppError>
                where
                    Self: Sized;
            ),
            "component methods cannot have generic parameters, lifetimes or a where clause",
        ),
    ];
    for (signature, expected) in cases {
        assert_method_error(&quote!(#[call] #signature), expected);
    }
}

#[test]
fn m12_to_m16_argument_errors() {
    let receiver = "component methods take `&self`";
    let arity = "component methods take `&self`, `cx: &CallContext` and one request";
    let context = "the first argument must be `cx: &CallContext`";
    let pattern = "arguments must be plain identifiers";
    let request = "the request must be an owned type without lifetimes or `impl Trait`";
    let cases = [
        (quote!((self, cx: &CallContext, req: PingRequest)), receiver),
        (
            quote!((&mut self, cx: &CallContext, req: PingRequest)),
            receiver,
        ),
        (
            quote!((&'static self, cx: &CallContext, req: PingRequest)),
            receiver,
        ),
        (
            quote!((self: Box<Self>, cx: &CallContext, req: PingRequest)),
            receiver,
        ),
        (quote!((cx: &CallContext, req: PingRequest)), receiver),
        (quote!(()), receiver),
        (quote!(()), arity),
        (quote!((&self, cx: &CallContext)), arity),
        (
            quote!((&self, cx: &CallContext, req: PingRequest, extra: u8)),
            arity,
        ),
        (quote!((&self, cx: CallContext, req: PingRequest)), context),
        (
            quote!((&self, cx: &mut CallContext, req: PingRequest)),
            context,
        ),
        (
            quote!((&self, cx: &'static CallContext, req: PingRequest)),
            context,
        ),
        (quote!((&self, cx: &Context, req: PingRequest)), context),
        (
            quote!((&self, cx: &CallContext<u8>, req: PingRequest)),
            context,
        ),
        (
            quote!((&self, mut cx: &CallContext, req: PingRequest)),
            pattern,
        ),
        (quote!((&self, cx: &CallContext, (a, b): (u8, u8))), pattern),
        (
            quote!((&self, cx: &CallContext, ref req: PingRequest)),
            pattern,
        ),
        (
            quote!((&self, cx: &CallContext, req: &PingRequest)),
            request,
        ),
        (
            quote!((&self, cx: &CallContext, req: Vec<&'static str>)),
            request,
        ),
        (
            quote!((&self, cx: &CallContext, req: impl Message)),
            request,
        ),
    ];
    for (arguments, expected) in cases {
        assert_method_error(
            &quote! {
                #[call]
                async fn ping #arguments -> Result<PingReply, AppError>;
            },
            expected,
        );
    }
}

#[test]
fn m17_to_m19_return_errors() {
    let returns = "the return type must be Result<Reply, Error>";
    let borrowed = "reply and error types cannot contain lifetimes or `impl Trait`";
    let cases = [
        (quote!(), returns),
        (quote!(-> PingReply), returns),
        (quote!(-> Option<PingReply>), returns),
        (quote!(-> Result<PingReply>), returns),
        (quote!(-> Result<PingReply, AppError, u8>), returns),
        (quote!(-> Result<'static, PingReply>), returns),
        (
            quote!(-> <T as Trait>::Result<PingReply, AppError>),
            returns,
        ),
        (quote!(-> io<u8>::Result<PingReply, AppError>), returns),
        (quote!(-> Result<&'static str, AppError>), borrowed),
        (quote!(-> Result<PingReply, impl Error>), borrowed),
        (
            quote!(-> Result<PingReply, Box<dyn Error + 'static>>),
            borrowed,
        ),
    ];
    for (output, expected) in cases {
        assert_method_error(
            &quote! {
                #[call]
                async fn ping(&self, cx: &CallContext, req: PingRequest) #output;
            },
            expected,
        );
    }
}

#[test]
fn m20_m21_method_names() {
    for name in [quote!(Ping), quote!(ping_), quote!(pi__ng), quote!(r#type)] {
        assert_method_error(
            &quote! {
                #[call]
                async fn #name(&self, cx: &CallContext, req: PingRequest) -> Result<PingReply, AppError>;
            },
            "method names must be snake_case ([a-z][a-z0-9_]*, without `__` or a trailing `_`)",
        );
    }
    for name in [
        "install",
        "install_with_lifecycle",
        "install_remote",
        "binding",
        "clone",
    ] {
        let ident = syn::Ident::new(name, proc_macro2::Span::call_site());
        assert_method_error(
            &quote! {
                #[call]
                async fn #ident(&self, cx: &CallContext, req: PingRequest) -> Result<PingReply, AppError>;
            },
            &format!("method name `{name}` is reserved on the generated handle"),
        );
    }
}

#[test]
fn independent_errors_are_combined() {
    let all = messages(
        quote!(name = "Bad", package = "Bad", proto = "crate::proto"),
        quote! {
            pub trait Echo<T>: Clone {
                type Item;
                async fn missing_kind(&self, cx: &CallContext, req: PingRequest)
                    -> Result<PingReply, AppError>;
                #[call(timeout = "never", bulkhead = 0)]
                fn not_async(&self, cx: &CallContext, req: &PingRequest) -> Result<(), AppError>;
                #[call]
                async fn get_v2(&self, cx: &CallContext, req: PingRequest) -> Result<(), AppError>;
                #[call]
                async fn get_v_2(&self, cx: &CallContext, req: PingRequest) -> Result<(), AppError>;
            }
        },
    );
    for expected in [
        "component name must match [a-z][a-z0-9_]*, without `__` or a trailing `_`, at most 48 characters",
        "package must be dot-separated lowercase segments such as `shop.inventory.v1`",
        "a component trait cannot have generic parameters or a where clause",
        "a component trait may only have `Send`, `Sync` and `'static` as supertraits",
        "a component trait may only contain methods",
        "method `missing_kind` needs a kind attribute: #[call]",
        "timeout must be a positive duration such as \"2s\" or \"250ms\"",
        "bulkhead must be an integer from 1 to 4294967295",
        "component methods must be `async fn`",
        "the request must be an owned type without lifetimes or `impl Trait`",
        "methods `get_v2` and `get_v_2` both map to the RPC name `GetV2`; rename one",
    ] {
        assert!(
            all.iter().any(|message| message == expected),
            "`{expected}` not in {all:?}"
        );
    }
    assert_eq!(all.len(), 11, "{all:?}");
}

#[test]
fn an_unparseable_item_is_a_syn_error() {
    let all = messages(standard_args(), quote!(pub trait));
    assert_eq!(all.len(), 1, "{all:?}");
}

#[test]
fn methods_mapping_to_one_rpc_name_are_rejected() {
    let method = |name: &str| {
        let ident = syn::Ident::new(name, proc_macro2::Span::call_site());
        quote! {
            #[call]
            async fn #ident(&self, cx: &CallContext, req: PingRequest) -> Result<PingReply, AppError>;
        }
    };
    let (v2, v_2, a1b) = (method("get_v2"), method("get_v_2"), method("a1b"));
    let all = messages(standard_args(), quote!(pub trait Echo { #v2 #v_2 #a1b }));
    assert_eq!(
        all,
        ["methods `get_v2` and `get_v_2` both map to the RPC name `GetV2`; rename one"]
    );
    // Every later duplicate is reported against the first method.
    let all = messages(standard_args(), quote!(pub trait Echo { #v2 #v_2 #v_2 }));
    assert_eq!(all.len(), 2, "{all:?}");
    // `a1b` (`A1b`) and `a1_b` (`A1B`) differ.
    let a1_underscore_b = method("a1_b");
    expand_ok(
        standard_args(),
        quote!(pub trait Echo { #a1b #a1_underscore_b }),
    );
    // A local_only component has no RPCs but still maps names the same way.
    assert_error(
        quote!(name = "notes", local_only),
        quote!(pub trait Notes { #v2 #v_2 }),
        "methods `get_v2` and `get_v_2` both map to the RPC name `GetV2`; rename one",
    );
}

#[test]
fn a_unit_reply_and_request_are_google_protobuf_empty() {
    let out = expand_ok(
        standard_args(),
        quote! {
            pub trait Echo {
                #[call]
                async fn clear(&self, cx: &CallContext, req: PingRequest) -> Result<(), AppError>;
                #[call]
                async fn tick(&self, cx: &CallContext, req: ()) -> Result<(), AppError>;
            }
        },
    );
    for needle in [
        "assert_wire::<()>();",
        "assert_rpc::<PingRequest,()>(crate::proto::__sekvent_service_Echo,\"Clear\",);",
        "assert_rpc_types::<PingRequest,(),crate::proto::__sekvent_rpc_Echo__Clear>();",
        "assert_rpc::<(),()>(crate::proto::__sekvent_service_Echo,\"Tick\",);",
        "Output=Result<(),AppError>",
    ] {
        assert!(out.contains(needle), "missing `{needle}` in {out}");
    }
}

#[test]
fn generated_generics_do_not_capture_user_types() {
    let out = expand_ok(
        standard_args(),
        quote! {
            pub trait Echo {
                #[call]
                async fn ping(&self, cx: &CallContext, req: T) -> Result<F, AppError>;
            }
        },
    );
    for needle in [
        "impl<__SekventImpl:Echo>__EchoDynfor__SekventImpl{",
        "<__SekventImplasEcho>::ping(self,cx,req)",
        "pubfninstall<__SekventImpl,__SekventFactory>(",
        "pubfninstall_with_lifecycle<__SekventImpl,__SekventFactory>(",
        "::std::sync::Arc::<__SekventImpl>::clone(&concrete)",
        "fn__ping<'a>(&'aself,cx:&'aCallContext,req:T)",
    ] {
        assert!(out.contains(needle), "missing `{needle}` in {out}");
    }
    assert!(!out.contains("<T:"), "{out}");
    assert!(!out.contains("install<T,F>"), "{out}");
}
