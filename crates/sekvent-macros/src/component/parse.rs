//! Parsing and validation of `#[component]` arguments and the trait.

use std::collections::HashMap;
use std::time::Duration;

use proc_macro2::{Span, TokenStream, TokenTree};
use quote::{ToTokens, quote};
use syn::parse::Parser;
use syn::punctuated::Punctuated;
use syn::spanned::Spanned;
use syn::{
    Attribute, Error, Expr, ExprLit, FnArg, GenericArgument, Ident, ItemTrait, Lit, LitStr, Pat,
    Path, PathArguments, Receiver, ReceiverKind, Result, ReturnType, Safety, Token, TraitItem,
    TraitItemFn, Type, TypeParamBound, Visibility,
};

use super::Errors;

/// Longest accepted component name.
const MAX_NAME_LEN: usize = 48;

/// Names the generated handle defines itself.
const RESERVED_METHODS: [&str; 5] = [
    "install",
    "install_with_lifecycle",
    "install_remote",
    "binding",
    "clone",
];

const BAD_NAME: &str = "component name must match [a-z][a-z0-9_]*, without `__` or a trailing `_`, at most 48 characters";
const BAD_PACKAGE: &str =
    "package must be dot-separated lowercase segments such as `shop.inventory.v1`";
const BAD_SUPERTRAIT: &str =
    "a component trait may only have `Send`, `Sync` and `'static` as supertraits";
const MULTIPLE_KINDS: &str = "a method has exactly one kind attribute";
const BAD_TIMEOUT: &str = "timeout must be a positive duration such as \"2s\" or \"250ms\"";
const BAD_BULKHEAD: &str = "bulkhead must be an integer from 1 to 4294967295";
const BAD_METHOD_ATTR: &str =
    "only doc comments and the kind attribute are allowed on a component method";
const BAD_MODIFIER: &str = "component methods cannot be const, unsafe or extern";
const BAD_RECEIVER: &str = "component methods take `&self`";
const BAD_ARITY: &str = "component methods take `&self`, `cx: &CallContext` and one request";
const BAD_CONTEXT: &str = "the first argument must be `cx: &CallContext`";
const BAD_PATTERN: &str = "arguments must be plain identifiers";
const BAD_REQUEST: &str = "the request must be an owned type without lifetimes or `impl Trait`";
const BAD_RETURN: &str = "the return type must be Result<Reply, Error>";
const BAD_REPLY_OR_ERROR: &str = "reply and error types cannot contain lifetimes or `impl Trait`";
const BAD_METHOD_NAME: &str =
    "method names must be snake_case ([a-z][a-z0-9_]*, without `__` or a trailing `_`)";
const MISSING_PROTO: &str = "missing `proto = \"...\"`: the module generated for the component's proto package, such as \"crate::proto::shop::inventory::v1\"; only a local_only component may omit it";
const PROTO_ON_LOCAL_ONLY: &str = "a local_only component has no contract; remove `proto`";
const BAD_PROTO: &str = "proto must be a module path such as \"crate::proto::shop::inventory::v1\"";
pub(crate) const ANONYMOUS_ON_LOCAL_ONLY: &str =
    "`anonymous` has no effect on a local_only component, which is never served; remove it";

/// Where a component may run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Mode {
    Standard,
    LocalOnly,
    RemoteOnly,
}

/// The validated `#[component(...)]` arguments.
pub(crate) struct Args {
    pub(crate) name: String,
    pub(crate) package: Option<String>,
    /// `proto = "..."`: the module holding `__sekvent_service_<Trait>`;
    /// present exactly when the component is not `local_only`.
    pub(crate) proto: Option<Path>,
    pub(crate) mode: Mode,
    /// `crate = "..."`: where the `sekvent_component` runtime lives.
    pub(crate) krate: Option<Path>,
}

/// The validated trait.
pub(crate) struct Component {
    pub(crate) attrs: Vec<Attribute>,
    pub(crate) vis: Visibility,
    pub(crate) ident: Ident,
    /// Supertraits as written, followed by the missing `Send`, `Sync`, `'static`.
    pub(crate) supertraits: Vec<TokenStream>,
    pub(crate) methods: Vec<Method>,
}

/// One validated `#[call]` method.
pub(crate) struct Method {
    pub(crate) docs: Vec<Attribute>,
    pub(crate) ident: Ident,
    /// The signature, for errors about the method's contract.
    pub(crate) signature: Span,
    /// The arguments as written (`&self, cx: &CallContext, req: Request`).
    pub(crate) inputs: Punctuated<FnArg, Token![,]>,
    /// The type behind the context reference (`CallContext`).
    pub(crate) context: Type,
    pub(crate) request: Type,
    /// The return type as written (`Result<Reply, Error>`).
    pub(crate) output: Type,
    pub(crate) reply: Type,
    pub(crate) error: Type,
    pub(crate) policy: CallPolicy,
    /// The `anonymous` argument of `#[call]`, if given.
    pub(crate) anonymous: Option<Span>,
}

/// Arguments of `#[call(...)]`.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CallPolicy {
    pub(crate) idempotent: bool,
    pub(crate) timeout: Option<Duration>,
    pub(crate) bulkhead: Option<u32>,
    /// End users may call the method without credentials.
    pub(crate) anonymous: bool,
}

/// Parse and validate the attribute arguments.
pub(crate) fn args(tokens: TokenStream) -> Result<Args> {
    let mut errors = Errors::default();
    let mut name: Option<LitStr> = None;
    let mut package: Option<LitStr> = None;
    let mut proto: Option<LitStr> = None;
    let mut local_only = false;
    let mut remote_only = false;
    let mut krate: Option<LitStr> = None;
    let parser = syn::meta::parser(|meta| {
        let key = path_name(&meta.path);
        let taken = match key.as_str() {
            "name" => name.is_some(),
            "package" => package.is_some(),
            "proto" => proto.is_some(),
            "local_only" => local_only,
            "remote_only" => remote_only,
            "crate" => krate.is_some(),
            _ => {
                return Err(meta.error(format!(
                    "unknown component argument `{key}`; expected name, package, proto, \
                     local_only, remote_only or crate"
                )));
            }
        };
        if taken {
            return Err(meta.error(format!("duplicate component argument `{key}`")));
        }
        match key.as_str() {
            "name" => name = Some(meta.value()?.parse()?),
            "package" => package = Some(meta.value()?.parse()?),
            "proto" => proto = Some(meta.value()?.parse()?),
            "crate" => krate = Some(meta.value()?.parse()?),
            flag => {
                if local_only || remote_only {
                    errors.push(meta.error("`local_only` and `remote_only` exclude each other"));
                }
                if flag == "local_only" {
                    local_only = true;
                } else {
                    remote_only = true;
                }
            }
        }
        Ok(())
    });
    parser.parse2(tokens)?;

    match &name {
        None => errors.push(Error::new(Span::call_site(), "missing `name = \"...\"`")),
        Some(lit) if !is_component_name(&lit.value()) => {
            errors.push(Error::new_spanned(lit, BAD_NAME));
        }
        Some(_) => {}
    }
    match &package {
        None if !local_only => errors.push(Error::new(
            Span::call_site(),
            "missing `package = \"...\"`; only a local_only component may omit it",
        )),
        Some(lit) if !is_package(&lit.value()) => errors.push(Error::new_spanned(lit, BAD_PACKAGE)),
        _ => {}
    }
    let proto = match proto {
        None if !local_only => {
            errors.push(Error::new(Span::call_site(), MISSING_PROTO));
            None
        }
        Some(lit) if local_only => {
            errors.push(Error::new_spanned(lit, PROTO_ON_LOCAL_ONLY));
            None
        }
        Some(lit) => errors.take(proto_path(&lit)),
        None => None,
    };
    let krate = krate.and_then(|lit| errors.take(lit.parse::<Path>()));
    errors.finish()?;
    Ok(Args {
        name: name.as_ref().map(LitStr::value).unwrap_or_default(),
        package: package.as_ref().map(LitStr::value),
        proto,
        mode: if local_only {
            Mode::LocalOnly
        } else if remote_only {
            Mode::RemoteOnly
        } else {
            Mode::Standard
        },
        krate,
    })
}

/// The module path of `proto = "..."`: `::`-separated identifiers (with
/// `crate`, `self` or `super` where Rust allows them), no generic arguments.
pub(crate) fn proto_path(lit: &LitStr) -> Result<Path> {
    lit.parse_with(Path::parse_mod_style)
        .map_err(|_| Error::new_spanned(lit, BAD_PROTO))
}

/// Validate the trait and every method, reporting all problems together.
pub(crate) fn component(item: ItemTrait) -> Result<Component> {
    let mut errors = Errors::default();
    if !item.generics.params.is_empty() || item.generics.where_clause.is_some() {
        let generics = &item.generics;
        let spanned = if generics.params.is_empty() {
            generics.where_clause.to_token_stream()
        } else {
            generics.to_token_stream()
        };
        errors.push(Error::new_spanned(
            spanned,
            "a component trait cannot have generic parameters or a where clause",
        ));
    }
    let modifiers = item
        .modifiers
        .require_empty()
        .err()
        .map(|error| error.span());
    if let Some(span) = item.unsafety.map(|token| token.span).or(modifiers) {
        errors.push(Error::new(
            span,
            "a component trait cannot be unsafe or auto",
        ));
    }
    let supertraits = supertraits(&item, &mut errors);

    let mut methods = Vec::new();
    let mut functions = 0_usize;
    for trait_item in item.items {
        match trait_item {
            TraitItem::Fn(function) => {
                functions += 1;
                if let Some(method) = errors.take(method(function)) {
                    methods.push(method);
                }
            }
            other => errors.push(Error::new_spanned(
                other,
                "a component trait may only contain methods",
            )),
        }
    }
    if functions == 0 {
        errors.push(Error::new_spanned(
            &item.ident,
            "a component needs at least one method",
        ));
    }
    check_rpc_names(&methods, &mut errors);
    errors.finish()?;
    Ok(Component {
        attrs: item.attrs,
        vis: item.vis,
        ident: item.ident,
        supertraits,
        methods,
    })
}

/// Two methods whose names differ only in underscores before digits
/// (`get_v2`, `get_v_2`) map to one RPC name; the second is an error.
fn check_rpc_names(methods: &[Method], errors: &mut Errors) {
    let mut seen: HashMap<String, &Ident> = HashMap::new();
    for method in methods {
        let rpc = rpc_name(&method.ident.to_string());
        if let Some(first) = seen.get(&rpc) {
            errors.push(Error::new_spanned(
                &method.ident,
                format!(
                    "methods `{first}` and `{}` both map to the RPC name `{rpc}`; rename one",
                    method.ident
                ),
            ));
        } else {
            seen.insert(rpc, &method.ident);
        }
    }
}

/// The supertraits as written plus the missing `Send`, `Sync` and `'static`.
fn supertraits(item: &ItemTrait, errors: &mut Errors) -> Vec<TokenStream> {
    let (mut send, mut sync, mut live) = (false, false, false);
    let mut bounds = Vec::new();
    for bound in &item.supertraits {
        match bound {
            TypeParamBound::Trait(trait_bound)
                if trait_bound.paren_token.is_none()
                    && trait_bound.lifetimes.is_none()
                    && trait_bound.maybe.is_none()
                    && marker_name(&trait_bound.path).is_some() =>
            {
                if marker_name(&trait_bound.path) == Some("Send") {
                    send = true;
                } else {
                    sync = true;
                }
            }
            TypeParamBound::Lifetime(lifetime) if lifetime.ident == "static" => live = true,
            other => {
                errors.push(Error::new_spanned(other, BAD_SUPERTRAIT));
                continue;
            }
        }
        bounds.push(bound.to_token_stream());
    }
    if !send {
        bounds.push(quote!(::core::marker::Send));
    }
    if !sync {
        bounds.push(quote!(::core::marker::Sync));
    }
    if !live {
        bounds.push(quote!('static));
    }
    bounds
}

/// `Send` or `Sync` when `path` names one of them (`Send`,
/// `core::marker::Send`, `::std::marker::Sync`, ...).
fn marker_name(path: &Path) -> Option<&'static str> {
    if path
        .segments
        .iter()
        .any(|segment| !segment.arguments.is_none())
    {
        return None;
    }
    let names: Vec<String> = path
        .segments
        .iter()
        .map(|segment| segment.ident.to_string())
        .collect();
    let (last, prefix) = names.split_last()?;
    let prefixed = match prefix {
        [] => path.leading_colon.is_none(),
        [root, marker] => (root == "core" || root == "std") && marker == "marker",
        _ => false,
    };
    if !prefixed {
        return None;
    }
    match last.as_str() {
        "Send" => Some("Send"),
        "Sync" => Some("Sync"),
        _ => None,
    }
}

fn method(function: TraitItemFn) -> Result<Method> {
    let mut errors = Errors::default();
    let ident = function.sig.ident.clone();
    let signature = function.sig.span();
    let name = ident.to_string();
    if !is_snake_name(&name) {
        errors.push(Error::new_spanned(&ident, BAD_METHOD_NAME));
    } else if RESERVED_METHODS.contains(&name.as_str()) {
        errors.push(Error::new_spanned(
            &ident,
            format!("method name `{name}` is reserved on the generated handle"),
        ));
    }
    let (docs, policy, anonymous) = method_attrs(&function, &mut errors);
    check_modifiers(&function, &mut errors);
    let arguments = arguments(&function, &mut errors);
    let returned = returned(&function, &mut errors);
    errors.finish()?;
    let (Some((context, request)), Some((output, reply, error))) = (arguments, returned) else {
        return Err(Error::new_spanned(&ident, BAD_ARITY));
    };
    Ok(Method {
        docs,
        ident,
        signature,
        inputs: function.sig.inputs,
        context,
        request,
        output,
        reply,
        error,
        policy,
        anonymous,
    })
}

/// Doc comments to keep, the `#[call]` policy and the span of its
/// `anonymous` argument.
fn method_attrs(
    function: &TraitItemFn,
    errors: &mut Errors,
) -> (Vec<Attribute>, CallPolicy, Option<Span>) {
    let mut docs = Vec::new();
    let mut policy = CallPolicy::default();
    let mut anonymous = None;
    let mut kinds = 0_usize;
    for attr in &function.attrs {
        let path = attr.path();
        if path.is_ident("doc") {
            docs.push(attr.clone());
            continue;
        }
        let kind = ["call", "async_call", "deferred"]
            .into_iter()
            .find(|kind| path.is_ident(kind));
        let Some(kind) = kind else {
            errors.push(Error::new_spanned(attr, BAD_METHOD_ATTR));
            continue;
        };
        kinds += 1;
        if kinds > 1 {
            errors.push(Error::new_spanned(attr, MULTIPLE_KINDS));
        }
        if kind == "call" {
            if kinds == 1 {
                (policy, anonymous) = errors.take(call_policy(attr)).unwrap_or_default();
            }
        } else {
            errors.push(Error::new_spanned(
                attr,
                format!("#[{kind}] is planned for milestone C3 and not available yet"),
            ));
        }
    }
    if kinds == 0 {
        errors.push(Error::new_spanned(
            &function.sig.ident,
            format!(
                "method `{}` needs a kind attribute: #[call]",
                function.sig.ident
            ),
        ));
    }
    (docs, policy, anonymous)
}

/// `#[call]` or `#[call(idempotent, timeout = "2s", bulkhead = 16, anonymous)]`.
/// Also returns the span of `anonymous`, when given.
pub(crate) fn call_policy(attr: &Attribute) -> Result<(CallPolicy, Option<Span>)> {
    let mut policy = CallPolicy::default();
    let mut anonymous_span = None;
    if matches!(attr.meta, syn::Meta::Path(_)) {
        return Ok((policy, None));
    }
    let mut errors = Errors::default();
    let (mut idempotent, mut timeout, mut bulkhead, mut anonymous) = (false, false, false, false);
    attr.parse_nested_meta(|meta| {
        let key = path_name(&meta.path);
        let seen = match key.as_str() {
            "idempotent" => &mut idempotent,
            "timeout" => &mut timeout,
            "bulkhead" => &mut bulkhead,
            "anonymous" => &mut anonymous,
            _ => {
                return Err(meta.error(format!(
                    "unknown #[call] argument `{key}`; expected idempotent, timeout, bulkhead \
                     or anonymous"
                )));
            }
        };
        if *seen {
            return Err(meta.error(format!("duplicate #[call] argument `{key}`")));
        }
        *seen = true;
        match key.as_str() {
            "idempotent" => policy.idempotent = true,
            "anonymous" => {
                policy.anonymous = true;
                anonymous_span = Some(meta.path.span());
            }
            "timeout" => {
                let value: Expr = meta.value()?.parse()?;
                policy.timeout = errors.take(timeout_value(&value));
            }
            _ => {
                let value: Expr = meta.value()?.parse()?;
                policy.bulkhead = errors.take(bulkhead_value(&value));
            }
        }
        Ok(())
    })?;
    errors.finish()?;
    Ok((policy, anonymous_span))
}

/// A humantime string literal for a positive duration.
pub(crate) fn timeout_value(value: &Expr) -> Result<Duration> {
    let parsed = match value {
        Expr::Lit(ExprLit {
            lit: Lit::Str(text),
            ..
        }) => humantime::parse_duration(&text.value()).ok(),
        _ => None,
    };
    parsed
        .filter(|duration| !duration.is_zero())
        .ok_or_else(|| Error::new_spanned(value, BAD_TIMEOUT))
}

/// An integer literal in `1..=u32::MAX`.
pub(crate) fn bulkhead_value(value: &Expr) -> Result<u32> {
    let parsed = match value {
        Expr::Lit(ExprLit {
            lit: Lit::Int(int), ..
        }) if matches!(int.suffix(), "" | "u32") => int.base10_parse::<u32>().ok(),
        _ => None,
    };
    parsed
        .filter(|limit| *limit >= 1)
        .ok_or_else(|| Error::new_spanned(value, BAD_BULKHEAD))
}

fn check_modifiers(function: &TraitItemFn, errors: &mut Errors) {
    let sig = &function.sig;
    if let Some(constness) = &sig.constness {
        errors.push(Error::new_spanned(constness, BAD_MODIFIER));
    }
    if !matches!(sig.safety, Safety::Default) {
        errors.push(Error::new_spanned(&sig.safety, BAD_MODIFIER));
    }
    if let Some(abi) = &sig.abi {
        errors.push(Error::new_spanned(abi, BAD_MODIFIER));
    }
    if let Err(error) = function.modifiers.require_empty() {
        errors.push(Error::new(error.span(), BAD_MODIFIER));
    }
    if sig.asyncness.is_none() {
        errors.push(Error::new_spanned(
            sig.fn_token,
            "component methods must be `async fn`",
        ));
    }
    if let Some(body) = &function.default {
        errors.push(Error::new_spanned(
            body,
            "component methods cannot have a default body",
        ));
    }
    let generics = &sig.generics;
    if !generics.params.is_empty() || generics.where_clause.is_some() {
        let spanned = if generics.params.is_empty() {
            generics.where_clause.to_token_stream()
        } else {
            generics.to_token_stream()
        };
        errors.push(Error::new_spanned(
            spanned,
            "component methods cannot have generic parameters, lifetimes or a where clause",
        ));
    }
}

/// The context type and the request type, when the arguments are valid.
fn arguments(function: &TraitItemFn, errors: &mut Errors) -> Option<(Type, Type)> {
    let sig = &function.sig;
    let inputs: Vec<&FnArg> = sig.inputs.iter().collect();
    match inputs.first() {
        Some(FnArg::Receiver(receiver)) if is_ref_self(receiver) => {}
        Some(other) => errors.push(Error::new_spanned(other, BAD_RECEIVER)),
        None => errors.push(Error::new(sig.paren_token.span.join(), BAD_RECEIVER)),
    }
    if inputs.len() != 3 || sig.variadic.is_some() {
        errors.push(if inputs.is_empty() {
            Error::new(sig.paren_token.span.join(), BAD_ARITY)
        } else {
            Error::new_spanned(&sig.inputs, BAD_ARITY)
        });
        return None;
    }
    let context = match inputs[1] {
        FnArg::Typed(typed) => {
            check_pattern(&typed.pat, errors);
            let context = context_type(&typed.ty);
            if context.is_none() {
                errors.push(Error::new_spanned(&typed.ty, BAD_CONTEXT));
            }
            context
        }
        FnArg::Receiver(receiver) => {
            errors.push(Error::new_spanned(receiver, BAD_CONTEXT));
            None
        }
    };
    let request = match inputs[2] {
        FnArg::Typed(typed) => {
            check_pattern(&typed.pat, errors);
            if borrows_or_impl(typed.ty.to_token_stream()) {
                errors.push(Error::new_spanned(&typed.ty, BAD_REQUEST));
                None
            } else {
                Some((*typed.ty).clone())
            }
        }
        FnArg::Receiver(receiver) => {
            errors.push(Error::new_spanned(receiver, BAD_REQUEST));
            None
        }
    };
    context.zip(request)
}

fn is_ref_self(receiver: &Receiver) -> bool {
    receiver.attrs.is_empty()
        && receiver.mutability.is_none()
        && matches!(receiver.kind, ReceiverKind::Reference(_, None, None))
}

fn check_pattern(pat: &Pat, errors: &mut Errors) {
    let plain = match pat {
        Pat::Ident(ident) => {
            ident.attrs.is_empty()
                && ident.by_ref.is_none()
                && ident.mutability.is_none()
                && ident.subpat.is_none()
        }
        Pat::Wild(wild) => wild.attrs.is_empty(),
        _ => false,
    };
    if !plain {
        errors.push(Error::new_spanned(pat, BAD_PATTERN));
    }
}

/// `CallContext` for `&CallContext` (any path ending in `CallContext`).
fn context_type(ty: &Type) -> Option<Type> {
    let Type::Reference(reference) = peel(ty) else {
        return None;
    };
    if reference.lifetime.is_some() || reference.mutability.is_some() {
        return None;
    }
    let Type::Path(path) = peel(&reference.elem) else {
        return None;
    };
    let last = path.path.segments.last()?;
    (path.qself.is_none() && last.ident == "CallContext" && last.arguments.is_none())
        .then(|| (*reference.elem).clone())
}

/// The return type, the reply and the error, when the return type is valid.
fn returned(function: &TraitItemFn, errors: &mut Errors) -> Option<(Type, Type, Type)> {
    let sig = &function.sig;
    let ReturnType::Type(_, output) = &sig.output else {
        errors.push(Error::new(sig.paren_token.span.close(), BAD_RETURN));
        return None;
    };
    let Some((reply, error)) = result_arguments(output) else {
        errors.push(Error::new_spanned(output, BAD_RETURN));
        return None;
    };
    let mut valid = true;
    if borrows_or_impl(reply.to_token_stream()) {
        errors.push(Error::new_spanned(reply, BAD_REPLY_OR_ERROR));
        valid = false;
    }
    if borrows_or_impl(error.to_token_stream()) {
        errors.push(Error::new_spanned(error, BAD_REPLY_OR_ERROR));
        valid = false;
    }
    valid.then(|| ((**output).clone(), reply.clone(), error.clone()))
}

/// `(Reply, Error)` of a path ending in `Result<Reply, Error>`.
fn result_arguments(ty: &Type) -> Option<(&Type, &Type)> {
    let Type::Path(path) = peel(ty) else {
        return None;
    };
    if path.qself.is_some() {
        return None;
    }
    let last = path.path.segments.last()?;
    let prefixed_with_arguments = path
        .path
        .segments
        .iter()
        .rev()
        .skip(1)
        .any(|segment| !segment.arguments.is_none());
    if last.ident != "Result" || prefixed_with_arguments {
        return None;
    }
    let PathArguments::AngleBracketed(arguments) = &last.arguments else {
        return None;
    };
    let mut types = arguments.args.iter();
    match (types.next(), types.next(), types.next()) {
        (Some(GenericArgument::Type(reply)), Some(GenericArgument::Type(error)), None) => {
            Some((reply, error))
        }
        _ => None,
    }
}

/// Look through invisible groups and parentheses.
fn peel(ty: &Type) -> &Type {
    match ty {
        Type::Group(group) => peel(&group.elem),
        Type::Paren(paren) => peel(&paren.elem),
        other => other,
    }
}

/// Whether the tokens borrow (`&`, a lifetime) or contain `impl`.
pub(crate) fn borrows_or_impl(tokens: TokenStream) -> bool {
    tokens.into_iter().any(|token| match token {
        TokenTree::Group(group) => borrows_or_impl(group.stream()),
        TokenTree::Punct(punct) => matches!(punct.as_char(), '\'' | '&'),
        TokenTree::Ident(ident) => ident == "impl",
        TokenTree::Literal(_) => false,
    })
}

fn path_name(path: &Path) -> String {
    path.segments
        .iter()
        .map(|segment| segment.ident.to_string())
        .collect::<Vec<_>>()
        .join("::")
}

/// `[a-z][a-z0-9_]*`, without `__` or a trailing `_`.
pub(crate) fn is_snake_name(name: &str) -> bool {
    name.starts_with(|c: char| c.is_ascii_lowercase())
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
        && !name.contains("__")
        && !name.ends_with('_')
}

/// A snake name of at most 48 characters.
pub(crate) fn is_component_name(name: &str) -> bool {
    is_snake_name(name) && name.len() <= MAX_NAME_LEN
}

/// Dot-separated `[a-z][a-z0-9_]*` segments.
pub(crate) fn is_package(package: &str) -> bool {
    package.split('.').all(|segment| {
        segment.starts_with(|c: char| c.is_ascii_lowercase())
            && segment
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
    })
}

/// `get_invoice` becomes `GetInvoice`.
pub(crate) fn rpc_name(method: &str) -> String {
    let mut out = String::with_capacity(method.len());
    for part in method.split('_') {
        let mut chars = part.chars();
        if let Some(first) = chars.next() {
            out.push(first.to_ascii_uppercase());
            out.push_str(chars.as_str());
        }
    }
    out
}
