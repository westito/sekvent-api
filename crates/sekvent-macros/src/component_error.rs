//! `#[derive(ComponentError)]`: typed component errors that travel as
//! `AppError` with a stable reason and their fields as metadata.

use std::collections::HashSet;

use proc_macro2::{Span, TokenStream};
use quote::{ToTokens, quote, quote_spanned};
use syn::ext::IdentExt;
use syn::parse::ParseStream;
use syn::spanned::Spanned;
use syn::{
    Attribute, Data, DeriveInput, Error, Fields, GenericArgument, Ident, LitStr, Path,
    PathArguments, Result, Token, Type, Variant,
};

use crate::component::Errors;

/// Every `ErrorCode` variant except `Ok`, in numeric order.
const CODES: [&str; 16] = [
    "Cancelled",
    "Unknown",
    "InvalidArgument",
    "DeadlineExceeded",
    "NotFound",
    "AlreadyExists",
    "PermissionDenied",
    "ResourceExhausted",
    "FailedPrecondition",
    "Aborted",
    "OutOfRange",
    "Unimplemented",
    "Internal",
    "Unavailable",
    "DataLoss",
    "Unauthenticated",
];

/// Longest accepted reason.
const MAX_REASON_LEN: usize = 63;

const BAD_OTHER: &str =
    "the #[other] variant must be a tuple variant holding one AppError, e.g. `Other(AppError)`";
const MARKED_TWICE: &str = "a variant has either one #[reason(...)] or #[other]";
const BAD_REASON: &str =
    "reason must be UPPER_SNAKE_CASE ([A-Z][A-Z0-9_]*) and at most 63 characters";
const MISSING_CODE: &str = "missing `code = ...` (an ErrorCode variant such as NotFound)";

/// Expand the derive. `runtime` yields the path of the `sekvent_component`
/// crate when `#[component_error(crate = "...")]` does not name one.
pub(crate) fn expand(
    input: &DeriveInput,
    runtime: impl FnOnce() -> TokenStream,
) -> Result<TokenStream> {
    let Data::Enum(data) = &input.data else {
        return Err(Error::new_spanned(
            &input.ident,
            "ComponentError can only be derived for an enum",
        ));
    };
    let mut errors = Errors::default();
    let generics = &input.generics;
    if !generics.params.is_empty() || generics.where_clause.is_some() {
        let spanned = if generics.params.is_empty() {
            generics.where_clause.to_token_stream()
        } else {
            generics.to_token_stream()
        };
        errors.push(Error::new_spanned(
            spanned,
            "ComponentError cannot be derived for a generic enum",
        ));
    }
    let container = errors.take(container(&input.attrs)).unwrap_or_default();
    let variants = variants(&input.ident, data.variants.iter(), &mut errors);
    errors.finish()?;
    let Some((reasons, other)) = variants else {
        return Err(Error::new_spanned(&input.ident, BAD_OTHER));
    };
    let krate = match &container.krate {
        Some(path) => path.to_token_stream(),
        None => runtime(),
    };
    Ok(generate(
        &input.ident,
        container.domain.as_ref(),
        &reasons,
        &other,
        &krate,
    ))
}

/// `#[component_error(domain = "...", crate = "...")]`.
#[derive(Default)]
struct Container {
    domain: Option<LitStr>,
    krate: Option<Path>,
}

fn container(attrs: &[Attribute]) -> Result<Container> {
    let mut out = Container::default();
    let mut krate: Option<LitStr> = None;
    let mut errors = Errors::default();
    for attr in attrs
        .iter()
        .filter(|attr| attr.path().is_ident("component_error"))
    {
        attr.parse_nested_meta(|meta| {
            let key = meta
                .path
                .get_ident()
                .map_or_else(|| meta.path.to_token_stream().to_string(), Ident::to_string);
            let taken = match key.as_str() {
                "domain" => out.domain.is_some(),
                "crate" => krate.is_some(),
                _ => {
                    return Err(meta.error(format!(
                        "unknown #[component_error] argument `{key}`; expected domain or crate"
                    )));
                }
            };
            if taken {
                return Err(meta.error(format!("duplicate #[component_error] argument `{key}`")));
            }
            let lit: LitStr = meta.value()?.parse()?;
            if key == "domain" {
                let domain = lit.value();
                if domain.is_empty() || domain.chars().any(char::is_whitespace) {
                    errors.push(Error::new_spanned(
                        &lit,
                        "domain must be non-empty and contain no whitespace",
                    ));
                }
                out.domain = Some(lit);
            } else {
                krate = Some(lit);
            }
            Ok(())
        })?;
    }
    if let Some(lit) = krate {
        out.krate = errors.take(lit.parse::<Path>());
    }
    errors.finish()?;
    Ok(out)
}

/// A variant with `#[reason(...)]`.
struct Reason {
    ident: Ident,
    /// `None` for a unit variant, else the named fields (possibly none).
    fields: Option<Vec<MetadataField>>,
    text: LitStr,
    code: Ident,
    message: Option<LitStr>,
}

/// A named field carried as metadata.
struct MetadataField {
    ident: Ident,
    /// The field type, or `U` for a field of type `Option<U>`.
    ty: Type,
    optional: bool,
}

impl MetadataField {
    fn key(&self) -> String {
        self.ident.unraw().to_string()
    }
}

/// The `#[reason]` variants and the `#[other]` variant, when both are valid.
fn variants<'a>(
    enum_ident: &Ident,
    variants: impl Iterator<Item = &'a Variant>,
    errors: &mut Errors,
) -> Option<(Vec<Reason>, Ident)> {
    let mut reasons = Vec::new();
    let mut other: Option<Ident> = None;
    let mut others = 0_usize;
    let mut seen_reasons = HashSet::new();
    for variant in variants {
        let mut marks = 0_usize;
        for attr in &variant.attrs {
            let is_reason = attr.path().is_ident("reason");
            if !is_reason && !attr.path().is_ident("other") {
                continue;
            }
            marks += 1;
            if marks > 1 {
                errors.push(Error::new_spanned(attr, MARKED_TWICE));
                continue;
            }
            if is_reason {
                let Some(reason) = errors.take(reason(variant, attr)) else {
                    continue;
                };
                if !seen_reasons.insert(reason.text.value()) {
                    errors.push(Error::new_spanned(
                        &reason.text,
                        format!(
                            "reason `{}` is used by more than one variant",
                            reason.text.value()
                        ),
                    ));
                }
                reasons.push(reason);
                continue;
            }
            others += 1;
            if others > 1 {
                errors.push(Error::new_spanned(attr, "only one variant can be #[other]"));
            }
            if let Err(error) = attr.meta.require_path_only() {
                errors.push(error);
            }
            match &variant.fields {
                Fields::Unnamed(fields) if fields.unnamed.len() == 1 => {
                    other.get_or_insert_with(|| variant.ident.clone());
                }
                _ => errors.push(Error::new_spanned(variant, BAD_OTHER)),
            }
        }
        if marks == 0 {
            errors.push(Error::new_spanned(
                &variant.ident,
                format!(
                    "variant `{}` needs #[reason(\"REASON\", code = ...)] or #[other]",
                    variant.ident
                ),
            ));
        }
    }
    if others == 0 {
        errors.push(Error::new_spanned(
            enum_ident,
            "exactly one variant needs #[other] to catch unknown reasons",
        ));
    }
    other.map(|other| (reasons, other))
}

/// Parse and validate `#[reason("REASON", code = X, message = "...")]`.
fn reason(variant: &Variant, attr: &Attribute) -> Result<Reason> {
    let mut errors = Errors::default();
    let (reason, code, message) = attr.parse_args_with(reason_args)?;
    if !is_reason(&reason.value()) {
        errors.push(Error::new_spanned(&reason, BAD_REASON));
    }
    let code = match code {
        None => {
            errors.push(Error::new_spanned(attr, MISSING_CODE));
            None
        }
        Some(path) => errors.take(code_ident(&path)),
    };
    let fields = match &variant.fields {
        Fields::Unit => Some(Vec::new()),
        Fields::Named(named) => Some(
            named
                .named
                .iter()
                .filter_map(|field| {
                    let ident = field.ident.clone()?;
                    let (ty, optional) = match option_inner(&field.ty) {
                        Some(inner) => (inner.clone(), true),
                        None => (field.ty.clone(), false),
                    };
                    Some(MetadataField {
                        ident,
                        ty,
                        optional,
                    })
                })
                .collect(),
        ),
        Fields::Unnamed(fields) => {
            errors.push(Error::new_spanned(
                fields,
                "use named fields (they become error metadata keys) or no fields",
            ));
            None
        }
    };
    errors.finish()?;
    let (Some(code), Some(fields)) = (code, fields) else {
        return Err(Error::new_spanned(attr, MISSING_CODE));
    };
    Ok(Reason {
        ident: variant.ident.clone(),
        fields: (!matches!(variant.fields, Fields::Unit)).then_some(fields),
        text: reason,
        code,
        message,
    })
}

type ReasonArgs = (LitStr, Option<Path>, Option<LitStr>);

fn reason_args(input: ParseStream<'_>) -> Result<ReasonArgs> {
    let reason: LitStr = input.parse()?;
    let mut code: Option<Path> = None;
    let mut message: Option<LitStr> = None;
    while !input.is_empty() {
        input.parse::<Token![,]>()?;
        if input.is_empty() {
            break;
        }
        let key: Ident = input.call(Ident::parse_any)?;
        let name = key.unraw().to_string();
        let taken = match name.as_str() {
            "code" => code.is_some(),
            "message" => message.is_some(),
            _ => {
                return Err(Error::new_spanned(
                    &key,
                    format!("unknown #[reason] argument `{name}`; expected code or message"),
                ));
            }
        };
        if taken {
            return Err(Error::new_spanned(
                &key,
                format!("duplicate #[reason] argument `{name}`"),
            ));
        }
        input.parse::<Token![=]>()?;
        if name == "code" {
            code = Some(input.parse()?);
        } else {
            message = Some(input.parse()?);
        }
    }
    Ok((reason, code, message))
}

/// The `ErrorCode` variant a `code = ...` value names (`NotFound` or
/// `ErrorCode::NotFound`).
fn code_ident(path: &Path) -> Result<Ident> {
    let last = path
        .segments
        .last()
        .ok_or_else(|| Error::new_spanned(path, MISSING_CODE))?;
    let name = last.ident.to_string();
    if name == "Ok" {
        return Err(Error::new_spanned(path, "`Ok` is not an error code"));
    }
    if !CODES.contains(&name.as_str()) || !last.arguments.is_none() {
        return Err(Error::new_spanned(
            path,
            format!(
                "unknown error code `{name}`; expected one of {}",
                CODES.join(", ")
            ),
        ));
    }
    Ok(last.ident.clone())
}

/// `[A-Z][A-Z0-9_]*`, at most 63 characters.
pub(crate) fn is_reason(reason: &str) -> bool {
    reason.starts_with(|c: char| c.is_ascii_uppercase())
        && reason
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
        && reason.len() <= MAX_REASON_LEN
}

/// `OUT_OF_STOCK` becomes `out of stock`.
pub(crate) fn default_message(reason: &str) -> String {
    reason.to_ascii_lowercase().replace('_', " ")
}

/// `U` for a type written `Option<U>` (any path ending in `Option`).
fn option_inner(ty: &Type) -> Option<&Type> {
    let Type::Path(path) = ty else {
        return None;
    };
    if path.qself.is_some() {
        return None;
    }
    let last = path.path.segments.last()?;
    if last.ident != "Option" {
        return None;
    }
    let PathArguments::AngleBracketed(arguments) = &last.arguments else {
        return None;
    };
    let mut types = arguments.args.iter();
    match (types.next(), types.next()) {
        (Some(GenericArgument::Type(inner)), None) => Some(inner),
        _ => None,
    }
}

fn generate(
    name: &Ident,
    domain: Option<&LitStr>,
    reasons: &[Reason],
    other: &Ident,
    krate: &TokenStream,
) -> TokenStream {
    let assertions: Vec<TokenStream> = reasons
        .iter()
        .flat_map(|reason| reason.fields.iter().flatten())
        .map(|field| {
            let ty = &field.ty;
            quote_spanned!(ty.span()=> #krate::__private::assert_metadata::<#ty>();)
        })
        .collect();
    let assertions = (!assertions.is_empty()).then(|| {
        quote! {
            const _: () = {
                #(#assertions)*
            };
        }
    });
    let encode = reasons
        .iter()
        .map(|reason| encode_arm(reason, domain, krate));
    let decode = reasons
        .iter()
        .map(|reason| decode_branch(reason, domain, krate));
    let error = local("error");
    quote! {
        #assertions

        #[automatically_derived]
        #[allow(clippy::all, clippy::pedantic)]
        impl #krate::ComponentError for #name {
            fn into_app_error(self) -> #krate::AppError {
                match self {
                    #(#encode)*
                    Self::#other(#error) => #error,
                }
            }

            fn from_app_error(#error: #krate::AppError) -> Self {
                #(#decode)*
                Self::#other(#error)
            }
        }

        #[automatically_derived]
        impl ::core::convert::From<#krate::AppError> for #name {
            fn from(#error: #krate::AppError) -> Self {
                <Self as #krate::ComponentError>::from_app_error(#error)
            }
        }

        #[automatically_derived]
        impl ::core::convert::From<#name> for #krate::AppError {
            fn from(#error: #name) -> Self {
                <#name as #krate::ComponentError>::into_app_error(#error)
            }
        }
    }
}

/// A local variable or parameter of the expansion. The mixed-site span
/// keeps it apart from a field of the same name (`error`, `value`), as in
/// a `macro_rules!` macro, without changing how it reads.
fn local(name: &str) -> Ident {
    Ident::new(name, Span::mixed_site())
}

/// `Self::X`, `Self::X {}` or `Self::X { a, b }`.
fn construct(reason: &Reason) -> TokenStream {
    let ident = &reason.ident;
    match &reason.fields {
        None => quote!(Self::#ident),
        Some(fields) => {
            let idents = fields.iter().map(|field| &field.ident);
            quote!(Self::#ident { #(#idents),* })
        }
    }
}

/// One `into_app_error` match arm.
fn encode_arm(reason: &Reason, domain: Option<&LitStr>, krate: &TokenStream) -> TokenStream {
    let pattern = construct(reason);
    let code = &reason.code;
    let text = &reason.text;
    let message = if let Some(format) = &reason.message {
        quote!(::std::format!(#format))
    } else {
        let message = default_message(&text.value());
        quote!(::std::string::String::from(#message))
    };
    let domain = domain.map(|domain| quote!(.with_domain(#domain)));
    let fields = reason.fields.as_deref().unwrap_or_default();
    let required = fields.iter().filter(|field| !field.optional).map(|field| {
        let ident = &field.ident;
        let key = field.key();
        quote!(.with_metadata(#key, ::std::string::ToString::to_string(&#ident)))
    });
    let base = quote! {
        #krate::AppError::new(#krate::ErrorCode::#code, #message)
            .with_reason(#text)
            #domain
            #(#required)*
    };
    let optional: Vec<&MetadataField> = fields.iter().filter(|field| field.optional).collect();
    let Some((&last, rest)) = optional.split_last() else {
        return quote!(#pattern => #base,);
    };
    let (error, value) = (local("error"), local("value"));
    let attach = |field: &MetadataField| {
        let ident = &field.ident;
        let key = field.key();
        quote! {
            match #ident {
                ::core::option::Option::Some(#value) =>
                    #error.with_metadata(#key, ::std::string::ToString::to_string(&#value)),
                ::core::option::Option::None => #error,
            }
        }
    };
    let rest = rest.iter().map(|&field| {
        let attach = attach(field);
        quote!(let #error = #attach;)
    });
    let last = attach(last);
    quote! {
        #pattern => {
            let #error = #base;
            #(#rest)*
            #last
        }
    }
}

/// One `from_app_error` branch: a matching reason with parseable fields
/// returns the variant.
fn decode_branch(reason: &Reason, domain: Option<&LitStr>, krate: &TokenStream) -> TokenStream {
    let text = &reason.text;
    let domain = if let Some(domain) = domain {
        quote!(::core::option::Option::Some(#domain))
    } else {
        quote!(::core::option::Option::None)
    };
    let construct = construct(reason);
    let fields = reason.fields.as_deref().unwrap_or_default();
    let error = local("error");
    let body = if fields.is_empty() {
        quote!(return #construct;)
    } else {
        let reads = fields.iter().map(|field| {
            let ty = &field.ty;
            let key = field.key();
            if field.optional {
                quote!(#krate::__private::optional_field::<#ty>(&#error, #key))
            } else {
                quote!(#krate::__private::field::<#ty>(&#error, #key))
            }
        });
        let idents = fields.iter().map(|field| &field.ident);
        quote! {
            match (#(#reads,)*) {
                (#(::core::option::Option::Some(#idents),)*) => return #construct,
                _ => {}
            }
        }
    };
    quote! {
        if #krate::__private::matches(&#error, #text, #domain) {
            #body
        }
    }
}

#[cfg(test)]
mod tests {
    use quote::quote;
    use syn::parse_quote;

    use super::*;

    fn runtime() -> TokenStream {
        quote!(::sekvent_component)
    }

    fn compact(text: &str) -> String {
        text.chars().filter(|c| !c.is_whitespace()).collect()
    }

    fn expand_ok(input: &DeriveInput) -> String {
        match expand(input, runtime) {
            Ok(tokens) => compact(&tokens.to_string()),
            Err(error) => panic!("unexpected error: {error}"),
        }
    }

    fn messages(input: &DeriveInput) -> Vec<String> {
        match expand(input, runtime) {
            Ok(tokens) => panic!("expected an error, got {tokens}"),
            Err(error) => error.into_iter().map(|error| error.to_string()).collect(),
        }
    }

    fn assert_error(input: &DeriveInput, expected: &str) {
        let all = messages(input);
        assert!(
            all.iter().any(|message| message == expected),
            "`{expected}` not in {all:?}"
        );
    }

    #[test]
    fn reasons() {
        assert!(is_reason("OUT_OF_STOCK"));
        assert!(is_reason("V2_GONE"));
        assert!(is_reason(&"A".repeat(63)));
        assert!(!is_reason(&"A".repeat(64)));
        assert!(!is_reason(""));
        assert!(!is_reason("_LEADING"));
        assert!(!is_reason("2FA"));
        assert!(!is_reason("out_of_stock"));
        assert!(!is_reason("OUT-OF-STOCK"));
    }

    #[test]
    fn default_messages() {
        assert_eq!(default_message("OUT_OF_STOCK"), "out of stock");
        assert_eq!(default_message("CLOSED"), "closed");
    }

    #[test]
    fn option_fields_are_recognised_syntactically() {
        assert!(option_inner(&parse_quote!(Option<String>)).is_some());
        assert!(option_inner(&parse_quote!(::core::option::Option<u32>)).is_some());
        assert!(option_inner(&parse_quote!(String)).is_none());
        assert!(option_inner(&parse_quote!(Option<u8, u8>)).is_none());
        assert!(option_inner(&parse_quote!(Option<'static>)).is_none());
        assert!(option_inner(&parse_quote!(Option)).is_none());
        assert!(option_inner(&parse_quote!(<T as Trait>::Option)).is_none());
        assert!(option_inner(&parse_quote!(&Option<u8>)).is_none());
    }

    #[test]
    fn one_field_uses_a_one_element_tuple() {
        let input: DeriveInput = parse_quote! {
            enum E {
                #[reason("GONE", code = NotFound)]
                Gone { id: String },
                #[other]
                Other(AppError),
            }
        };
        let out = expand_ok(&input);
        assert!(
            out.contains(&compact(
                "match (::sekvent_component::__private::field::<String>(&error, \"id\"),) {\
                 (::core::option::Option::Some(id),) => return Self::Gone { id },"
            )),
            "{out}"
        );
    }

    #[test]
    fn code_paths_and_empty_named_variants() {
        let input: DeriveInput = parse_quote! {
            #[component_error(crate = "::fw")]
            enum E {
                #[reason("EMPTY", code = ErrorCode::InvalidArgument)]
                Empty {},
                #[reason("A_B", code = Internal)]
                Both { a: Option<u8>, b: Option<u8> },
                #[other]
                Other(AppError),
            }
        };
        let out = expand_ok(&input);
        for needle in [
            "Self::Empty {} => ::fw::AppError::new(::fw::ErrorCode::InvalidArgument",
            "return Self::Empty {};",
            "::fw::__private::matches(&error, \"EMPTY\", ::core::option::Option::None)",
            "let error = match a {",
            "match b {",
            "String::from(\"a b\")",
        ] {
            assert!(
                out.contains(&compact(needle)),
                "missing `{needle}` in {out}"
            );
        }
        assert!(!out.contains("sekvent_component"), "{out}");
        assert!(!out.contains("with_domain"), "{out}");
    }

    #[test]
    fn shape_errors() {
        assert_error(
            &parse_quote!(
                struct S;
            ),
            "ComponentError can only be derived for an enum",
        );
        assert_error(
            &parse_quote!(
                enum E<T> {
                    #[other]
                    Other(T),
                }
            ),
            "ComponentError cannot be derived for a generic enum",
        );
        assert_error(
            &parse_quote!(
                enum E
                where
                    u8: Copy,
                {
                    #[other]
                    Other(AppError),
                }
            ),
            "ComponentError cannot be derived for a generic enum",
        );
        assert_error(
            &parse_quote!(
                enum E {
                    #[reason("A", code = Internal)]
                    A,
                }
            ),
            "exactly one variant needs #[other] to catch unknown reasons",
        );
        assert_error(
            &parse_quote!(
                enum E {
                    #[other]
                    A(AppError),
                    #[other]
                    B(AppError),
                }
            ),
            "only one variant can be #[other]",
        );
        let inputs: [DeriveInput; 3] = [
            parse_quote!(
                enum E {
                    #[other]
                    Other,
                }
            ),
            parse_quote!(
                enum E {
                    #[other]
                    Other(AppError, u8),
                }
            ),
            parse_quote!(
                enum E {
                    #[other]
                    Other { error: AppError },
                }
            ),
        ];
        for input in inputs {
            assert_error(&input, BAD_OTHER);
        }
    }

    #[test]
    fn variant_attribute_errors() {
        assert_error(
            &parse_quote!(
                enum E {
                    Plain,
                    #[other]
                    Other(AppError),
                }
            ),
            "variant `Plain` needs #[reason(\"REASON\", code = ...)] or #[other]",
        );
        assert_error(
            &parse_quote!(
                enum E {
                    #[reason("A", code = Internal)]
                    #[other]
                    Other(AppError),
                }
            ),
            MARKED_TWICE,
        );
        assert_error(
            &parse_quote!(
                enum E {
                    #[reason("A", code = Internal)]
                    #[reason("B", code = Internal)]
                    A,
                    #[other]
                    Other(AppError),
                }
            ),
            MARKED_TWICE,
        );
        assert_error(
            &parse_quote!(
                enum E {
                    #[reason("A", code = Internal)]
                    A(String),
                    #[other]
                    Other(AppError),
                }
            ),
            "use named fields (they become error metadata keys) or no fields",
        );
    }

    #[test]
    fn reason_argument_errors() {
        let case = |attr: TokenStream| -> DeriveInput {
            parse_quote! {
                enum E {
                    #attr
                    A,
                    #[other]
                    Other(AppError),
                }
            }
        };
        assert_error(&case(quote!(#[reason("bad", code = Internal)])), BAD_REASON);
        assert_error(&case(quote!(#[reason("A")])), MISSING_CODE);
        assert_error(
            &case(quote!(#[reason("A", code = Teapot)])),
            "unknown error code `Teapot`; expected one of Cancelled, Unknown, InvalidArgument, \
             DeadlineExceeded, NotFound, AlreadyExists, PermissionDenied, ResourceExhausted, \
             FailedPrecondition, Aborted, OutOfRange, Unimplemented, Internal, Unavailable, \
             DataLoss, Unauthenticated",
        );
        assert_error(
            &case(quote!(#[reason("A", code = Ok)])),
            "`Ok` is not an error code",
        );
        assert_error(
            &case(quote!(#[reason("A", code = Internal, colour = "red")])),
            "unknown #[reason] argument `colour`; expected code or message",
        );
        assert_error(
            &case(quote!(#[reason("A", code = Internal, code = NotFound)])),
            "duplicate #[reason] argument `code`",
        );
        let input: DeriveInput = parse_quote! {
            enum E {
                #[reason("SAME", code = Internal)]
                A,
                #[reason("SAME", code = Internal)]
                B,
                #[other]
                Other(AppError),
            }
        };
        assert_error(&input, "reason `SAME` is used by more than one variant");
    }

    #[test]
    fn container_argument_errors() {
        let case = |attr: TokenStream| -> DeriveInput {
            parse_quote! {
                #attr
                enum E {
                    #[other]
                    Other(AppError),
                }
            }
        };
        assert_error(
            &case(quote!(#[component_error(colour = "red")])),
            "unknown #[component_error] argument `colour`; expected domain or crate",
        );
        assert_error(
            &case(quote!(#[component_error(domain = "a", domain = "b")])),
            "duplicate #[component_error] argument `domain`",
        );
        for domain in ["", "shop inventory", " x"] {
            assert_error(
                &case(quote!(#[component_error(domain = #domain)])),
                "domain must be non-empty and contain no whitespace",
            );
        }
        let error = messages(&case(quote!(#[component_error(crate = "not a path")])));
        assert_eq!(error.len(), 1, "{error:?}");
    }

    #[test]
    fn every_error_is_reported() {
        let input: DeriveInput = parse_quote! {
            #[component_error(domain = "")]
            enum E {
                #[reason("bad", code = Teapot)]
                A,
                B,
            }
        };
        assert_eq!(messages(&input).len(), 5, "{:?}", messages(&input));
    }
}
