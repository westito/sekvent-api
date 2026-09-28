//! Procedural macros for sekvent.
//!
//! Use them through the crates that re-export them (`sekvent_config::EnvConfig`,
//! `sekvent_component::component`, `sekvent_component::ComponentError`, or
//! the facade's `sekvent::EnvConfig`, `sekvent::component` and
//! `sekvent::ComponentError`). The generated code refers to the runtime crate
//! by absolute path, found in the calling crate's manifest.

#![forbid(unsafe_code)]

mod component;
mod component_error;
mod env_config;
mod paths;

use proc_macro::TokenStream;

/// Derive `sekvent_config::FromConfig` for a struct with named fields.
///
/// Every field is read from one key, by default the field name in
/// `UPPER_SNAKE_CASE`. The field type picks the reader:
///
/// | Type               | Reading                                  |
/// |--------------------|------------------------------------------|
/// | `Secret`           | required secret (unset or blank fails)   |
/// | `Option<Secret>`   | optional secret (blank still fails)      |
/// | `Option<T>`        | optional, parsed like `T`                |
/// | `Duration`         | humantime (`5s`, `250ms`) or whole seconds |
/// | `bool`             | `true/false/1/0/yes/no/on/off`           |
/// | anything else      | `FromStr`, required unless defaulted     |
///
/// Container attributes:
///
/// - `prefix = "BILLING_"` scopes every key;
/// - `crate = "::path::to::config"` names the `sekvent_config` runtime
///   explicitly. Without it the path is taken from the calling crate's
///   `Cargo.toml`: a direct `sekvent-config` dependency (renames included),
///   else `<sekvent>::config` through the facade.
///
/// Field attributes, combinable inside one `#[config(...)]`:
///
/// - `key = "DB_URL"` overrides the key name;
/// - `default = "5s"` is used when the key is unset and is parsed like a
///   value would be (not allowed on secrets, `Option` fields or `nested`);
/// - `secret` asserts the field is a `Secret` or `Option<Secret>`;
/// - `nested` reads a type that itself implements `FromConfig` under the
///   prefix `<KEY>_`;
/// - `validate = path::to_fn` runs `fn(&FieldType) -> Result<(), String>`
///   after a successful read; `Err` becomes `ConfigError::Invalid`.
///
/// Reading reports every failing field at once. `keys()` lists every key,
/// including the container prefix and flattened nested keys.
#[proc_macro_derive(EnvConfig, attributes(config))]
pub fn derive_env_config(input: TokenStream) -> TokenStream {
    let input = syn::parse_macro_input!(input as syn::DeriveInput);
    env_config::expand(&input)
        .unwrap_or_else(syn::Error::into_compile_error)
        .into()
}

/// Declare a component: a trait whose methods other components call through
/// a generated handle, in-process or across a serialization boundary.
///
/// ```text
/// #[component(name = "billing", package = "shop.billing.v1", proto = "crate::proto::shop::billing::v1")]
/// pub trait Billing {
///     /// Charge an order.
///     #[call(idempotent, timeout = "2s", bulkhead = 16)]
///     async fn charge(&self, cx: &CallContext, req: ChargeRequest)
///         -> Result<ChargeReply, BillingError>;
/// }
/// ```
///
/// Arguments:
///
/// - `name = "billing"` (required): the component name, `[a-z][a-z0-9_]*`,
///   used in configuration keys (`SEKVENT_COMPONENT_BILLING_BINDING`);
/// - `package = "shop.billing.v1"`: the protobuf package of the messages,
///   required unless `local_only`;
/// - `proto = "crate::proto::shop::billing::v1"`: the module
///   `sekvent-proto-build` generated for that package, required unless
///   `local_only` (and forbidden with it). The trait is checked at compile
///   time against the proto `service` named after it (the constant
///   `__sekvent_service_<Trait>` and the aliases `__sekvent_rpc_<Trait>__<Rpc>`
///   in that module): the same full service name and exactly one unary RPC
///   per method, named in `UpperCamelCase` (`charge` is `Charge`), with the
///   method's request and reply types; two methods may not map to one RPC
///   name (`get_v2` and `get_v_2` both map to `GetV2`);
/// - `local_only`: requests and replies may be any `Send + 'static` type and
///   the component can only be bound `local`;
/// - `remote_only`: the component never runs in this binary and is declared
///   with `install_remote`;
/// - `crate = "::path::to::component"` names the `sekvent_component`
///   runtime; without it the path comes from the calling crate's manifest
///   (a direct `sekvent-component` dependency, else `<sekvent>::component`).
///
/// Every method is an `async fn(&self, cx: &CallContext, req: Request) ->
/// Result<Reply, Error>` marked `#[call]`, optionally with `idempotent`,
/// `timeout = "<humantime>"` and `bulkhead = <max concurrent calls>`.
/// Requests and replies are prost messages that implement `prost::Name`
/// (sekvent-proto-build enables prost's type names), or `()` for
/// `google.protobuf.Empty`; each error type implements `ComponentError`.
///
/// The macro keeps the trait (its methods become `fn -> impl Future + Send`,
/// so implementations still write `async fn`) and generates `<Trait>Handle`
/// with one method per trait method plus `binding`, `install` and
/// `install_with_lifecycle` (or `install_remote`).
#[proc_macro_attribute]
pub fn component(args: TokenStream, item: TokenStream) -> TokenStream {
    component::expand(args.into(), item.into(), || {
        paths::resolve("sekvent-component", "sekvent_component", "component")
    })
    .unwrap_or_else(syn::Error::into_compile_error)
    .into()
}

/// Derive `sekvent_component::ComponentError` for an enum of typed errors.
///
/// ```text
/// #[derive(Debug, ComponentError)]
/// #[component_error(domain = "shop.billing.v1")]
/// pub enum BillingError {
///     #[reason("CARD_DECLINED", code = FailedPrecondition, message = "card {card} declined")]
///     CardDeclined { card: String, retry_in: Option<u32> },
///     #[reason("BILLING_CLOSED", code = Unavailable)]
///     Closed,
///     #[other]
///     Other(AppError),
/// }
/// ```
///
/// A `#[reason]` variant travels as an `AppError` with its code, the stable
/// reason, the optional domain and its named fields as metadata (`ToString`
/// out, `FromStr` back; an `Option` field is written only when `Some`). The
/// message is the `message` format string, which may name the fields, or the
/// reason in lower case. Decoding a known reason (and domain) with parseable
/// fields yields that variant; anything else yields the `#[other]` variant,
/// so decoding never fails. `From` conversions to and from `AppError` are
/// generated too. `#[component_error(crate = "...")]` names the runtime
/// explicitly, as for `#[component]`.
#[proc_macro_derive(ComponentError, attributes(reason, other, component_error))]
pub fn derive_component_error(input: TokenStream) -> TokenStream {
    let input = syn::parse_macro_input!(input as syn::DeriveInput);
    component_error::expand(&input, || {
        paths::resolve("sekvent-component", "sekvent_component", "component")
    })
    .unwrap_or_else(syn::Error::into_compile_error)
    .into()
}
