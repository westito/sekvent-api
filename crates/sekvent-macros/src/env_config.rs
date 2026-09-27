//! `#[derive(EnvConfig)]`: attribute parsing and code generation.

use proc_macro2::{Span, TokenStream};
use quote::{format_ident, quote, quote_spanned};
use syn::ext::IdentExt;
use syn::spanned::Spanned;
use syn::{
    Attribute, Data, DeriveInput, Error, Expr, ExprLit, Field, Fields, GenericArgument, Ident, Lit,
    LitStr, Meta, Path, PathArguments, PathSegment, Result, Type,
};

const TRUE_WORDS: [&str; 4] = ["true", "1", "yes", "on"];
const FALSE_WORDS: [&str; 4] = ["false", "0", "no", "off"];

/// Expand the derive for one input item.
pub(crate) fn expand(input: &DeriveInput) -> Result<TokenStream> {
    let prefix = container_prefix(&input.attrs)?;
    let named = match &input.data {
        Data::Struct(data) => match &data.fields {
            Fields::Named(named) => &named.named,
            Fields::Unnamed(fields) => {
                return Err(Error::new_spanned(
                    fields,
                    "`EnvConfig` requires a struct with named fields; tuple structs are not supported",
                ));
            }
            Fields::Unit => {
                return Err(Error::new_spanned(
                    &input.ident,
                    "`EnvConfig` requires a struct with named fields; unit structs are not supported",
                ));
            }
        },
        Data::Enum(_) => {
            return Err(Error::new_spanned(
                &input.ident,
                "`EnvConfig` can only be derived for structs with named fields, not enums",
            ));
        }
        Data::Union(_) => {
            return Err(Error::new_spanned(
                &input.ident,
                "`EnvConfig` can only be derived for structs with named fields, not unions",
            ));
        }
    };

    let mut fields = Vec::with_capacity(named.len());
    let mut errors = Vec::new();
    for field in named {
        match FieldSpec::parse(field) {
            Ok(spec) => fields.push(spec),
            Err(error) => errors.push(error),
        }
    }
    if let Some(error) = errors.into_iter().reduce(|mut all, next| {
        all.combine(next);
        all
    }) {
        return Err(error);
    }
    Ok(generate(input, &prefix, &fields))
}

/// How a field is read, decided by its type (or by `nested`).
#[derive(Debug)]
enum Shape {
    Secret,
    OptionalSecret,
    OptionalDuration,
    OptionalBool,
    Optional(Box<Type>),
    Duration,
    Bool,
    Parsed,
    Nested,
}

impl Shape {
    fn is_secret(&self) -> bool {
        matches!(self, Self::Secret | Self::OptionalSecret)
    }

    fn is_optional(&self) -> bool {
        matches!(
            self,
            Self::OptionalSecret | Self::OptionalDuration | Self::OptionalBool | Self::Optional(_)
        )
    }
}

struct FieldSpec {
    ident: Ident,
    ty: Type,
    /// Key relative to the (possibly prefixed) source.
    key: String,
    shape: Shape,
    default: Option<LitStr>,
    validate: Option<Path>,
    doc: Option<String>,
}

#[derive(Default)]
struct FieldAttrs {
    key: Option<LitStr>,
    default: Option<LitStr>,
    secret: Option<Span>,
    nested: Option<Span>,
    validate: Option<Path>,
}

impl FieldSpec {
    fn parse(field: &Field) -> Result<Self> {
        let ident = field
            .ident
            .clone()
            .ok_or_else(|| Error::new_spanned(field, "`EnvConfig` requires named fields"))?;
        let attrs = field_attrs(&field.attrs)?;

        let key = match &attrs.key {
            Some(lit) if lit.value().is_empty() => {
                return Err(Error::new_spanned(lit, "`key` must not be empty"));
            }
            Some(lit) => lit.value(),
            None => upper_snake(&ident.unraw().to_string()),
        };

        let shape = if attrs.nested.is_some() {
            Shape::Nested
        } else {
            classify(&field.ty)
        };

        if let Some(span) = attrs.secret {
            if attrs.nested.is_some() {
                return Err(Error::new(
                    span,
                    "`secret` cannot be combined with `nested`",
                ));
            }
            if !shape.is_secret() {
                return Err(Error::new_spanned(
                    &field.ty,
                    "`#[config(secret)]` requires the field type to be `Secret` or `Option<Secret>`",
                ));
            }
        }

        if let Some(lit) = &attrs.default {
            check_default(&shape, lit)?;
        }

        Ok(Self {
            ident,
            ty: field.ty.clone(),
            key,
            shape,
            default: attrs.default,
            validate: attrs.validate,
            doc: doc_of(&field.attrs),
        })
    }
}

fn check_default(shape: &Shape, lit: &LitStr) -> Result<()> {
    let message = match shape {
        Shape::Nested => {
            "`default` cannot be combined with `nested`; declare defaults on the fields of the nested config"
        }
        _ if shape.is_secret() => {
            "a secret cannot have a default; it must come from the environment"
        }
        _ if shape.is_optional() => {
            "`default` has no effect on an `Option` field; drop either the `Option` or the default"
        }
        Shape::Bool if bool_word(&lit.value()).is_none() => {
            "the default of a `bool` field must be one of true/false/1/0/yes/no/on/off"
        }
        _ => return Ok(()),
    };
    Err(Error::new_spanned(lit, message))
}

fn container_prefix(attrs: &[Attribute]) -> Result<String> {
    let mut prefix: Option<LitStr> = None;
    for attr in attrs.iter().filter(|attr| attr.path().is_ident("config")) {
        attr.parse_nested_meta(|meta| {
            let name = path_name(&meta.path);
            if name != "prefix" {
                return Err(meta.error(format!(
                    "unknown config attribute `{name}` on a struct; expected `prefix`"
                )));
            }
            if prefix.is_some() {
                return Err(meta.error("duplicate config attribute `prefix`"));
            }
            prefix = Some(meta.value()?.parse()?);
            Ok(())
        })?;
    }
    Ok(prefix.as_ref().map(LitStr::value).unwrap_or_default())
}

fn field_attrs(attrs: &[Attribute]) -> Result<FieldAttrs> {
    let mut out = FieldAttrs::default();
    for attr in attrs.iter().filter(|attr| attr.path().is_ident("config")) {
        attr.parse_nested_meta(|meta| {
            let name = path_name(&meta.path);
            let taken = match name.as_str() {
                "key" => out.key.is_some(),
                "default" => out.default.is_some(),
                "secret" => out.secret.is_some(),
                "nested" => out.nested.is_some(),
                "validate" => out.validate.is_some(),
                _ => {
                    return Err(meta.error(format!(
                        "unknown config attribute `{name}`; expected one of `key`, `default`, \
                         `secret`, `nested`, `validate`"
                    )));
                }
            };
            if taken {
                return Err(meta.error(format!("duplicate config attribute `{name}`")));
            }
            match name.as_str() {
                "key" => out.key = Some(meta.value()?.parse()?),
                "default" => out.default = Some(meta.value()?.parse()?),
                "secret" => out.secret = Some(meta.path.span()),
                "nested" => out.nested = Some(meta.path.span()),
                _ => out.validate = Some(meta.value()?.parse()?),
            }
            Ok(())
        })?;
    }
    Ok(out)
}

fn path_name(path: &Path) -> String {
    path.segments
        .iter()
        .map(|segment| segment.ident.to_string())
        .collect::<Vec<_>>()
        .join("::")
}

fn classify(ty: &Type) -> Shape {
    if is_named(ty, "Secret") {
        return Shape::Secret;
    }
    if let Some(inner) = option_inner(ty) {
        return if is_named(inner, "Secret") {
            Shape::OptionalSecret
        } else if is_named(inner, "Duration") {
            Shape::OptionalDuration
        } else if is_named(inner, "bool") {
            Shape::OptionalBool
        } else {
            Shape::Optional(Box::new(inner.clone()))
        };
    }
    if is_named(ty, "Duration") {
        Shape::Duration
    } else if is_named(ty, "bool") {
        Shape::Bool
    } else {
        Shape::Parsed
    }
}

fn last_segment(ty: &Type) -> Option<&PathSegment> {
    match ty {
        Type::Group(group) => last_segment(&group.elem),
        Type::Paren(paren) => last_segment(&paren.elem),
        Type::Path(path) if path.qself.is_none() => path.path.segments.last(),
        _ => None,
    }
}

fn is_named(ty: &Type, name: &str) -> bool {
    last_segment(ty).is_some_and(|segment| segment.ident == name && segment.arguments.is_none())
}

fn option_inner(ty: &Type) -> Option<&Type> {
    let segment = last_segment(ty)?;
    if segment.ident != "Option" {
        return None;
    }
    let PathArguments::AngleBracketed(args) = &segment.arguments else {
        return None;
    };
    if args.args.len() != 1 {
        return None;
    }
    match args.args.first()? {
        GenericArgument::Type(inner) => Some(inner),
        _ => None,
    }
}

fn bool_word(raw: &str) -> Option<bool> {
    let word = raw.trim().to_ascii_lowercase();
    if TRUE_WORDS.contains(&word.as_str()) {
        Some(true)
    } else if FALSE_WORDS.contains(&word.as_str()) {
        Some(false)
    } else {
        None
    }
}

/// `dbUrl` and `db_url` both become `DB_URL`.
fn upper_snake(name: &str) -> String {
    let mut out = String::with_capacity(name.len() + 4);
    let mut previous: Option<char> = None;
    for c in name.chars() {
        if c.is_uppercase() && previous.is_some_and(|p| p.is_lowercase() || p.is_ascii_digit()) {
            out.push('_');
        }
        out.extend(c.to_uppercase());
        previous = Some(c);
    }
    out
}

fn doc_of(attrs: &[Attribute]) -> Option<String> {
    let lines: Vec<String> = attrs
        .iter()
        .filter(|attr| attr.path().is_ident("doc"))
        .filter_map(|attr| match &attr.meta {
            Meta::NameValue(pair) => match &pair.value {
                Expr::Lit(ExprLit {
                    lit: Lit::Str(text),
                    ..
                }) => Some(text.value()),
                _ => None,
            },
            _ => None,
        })
        .collect();
    let joined = lines
        .iter()
        .map(|line| line.strip_prefix(' ').unwrap_or(line).trim_end())
        .collect::<Vec<_>>()
        .join("\n");
    let trimmed = joined.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_owned())
}

fn generate(input: &DeriveInput, prefix: &str, fields: &[FieldSpec]) -> TokenStream {
    let name = &input.ident;
    let (impl_generics, ty_generics, where_clause) = input.generics.split_for_impl();

    let scope = (!prefix.is_empty()).then(|| {
        quote! {
            let __sekvent_scoped = ::sekvent_config::Prefixed::new(__sekvent_source, #prefix);
            let __sekvent_source: &dyn ::sekvent_config::ConfigSource = &__sekvent_scoped;
        }
    });

    let body = if fields.is_empty() {
        quote!(::core::result::Result::Ok(Self {}))
    } else {
        let vars: Vec<Ident> = (0..fields.len())
            .map(|index| format_ident!("__sekvent_field_{}", index))
            .collect();
        let reads = fields.iter().zip(&vars).map(|(field, var)| {
            let expr = read_expr(field);
            quote! {
                let #var = ::sekvent_config::__private::take(&mut __sekvent_errors, #expr);
            }
        });
        let idents = fields.iter().map(|field| &field.ident);
        quote! {
            let mut __sekvent_errors: ::std::vec::Vec<::sekvent_config::ConfigError> =
                ::std::vec::Vec::new();
            #(#reads)*
            match (#(#vars,)*) {
                (#(::core::option::Option::Some(#vars),)*) => {
                    ::core::result::Result::Ok(Self { #(#idents: #vars),* })
                }
                _ => ::core::result::Result::Err(
                    ::sekvent_config::__private::finish(__sekvent_errors),
                ),
            }
        }
    };

    let keys_body = if fields.is_empty() {
        quote!(::std::vec::Vec::new())
    } else {
        let pushes = fields.iter().map(|field| key_info(field, prefix));
        quote! {
            let mut __sekvent_keys = ::std::vec::Vec::new();
            #(#pushes)*
            __sekvent_keys
        }
    };

    quote! {
        #[automatically_derived]
        impl #impl_generics ::sekvent_config::FromConfig for #name #ty_generics #where_clause {
            fn from_config(
                __sekvent_source: &dyn ::sekvent_config::ConfigSource,
            ) -> ::core::result::Result<Self, ::sekvent_config::ConfigError> {
                #scope
                #body
            }

            fn keys() -> ::std::vec::Vec<::sekvent_config::KeyInfo> {
                #keys_body
            }
        }
    }
}

fn read_expr(field: &FieldSpec) -> TokenStream {
    let key = &field.key;
    let ty = &field.ty;
    let base = match (&field.shape, &field.default) {
        (Shape::Nested, _) => {
            let nested_prefix = format!("{key}_");
            let from =
                quote_spanned!(ty.span()=> <#ty as ::sekvent_config::FromConfig>::from_config);
            quote!(#from(&::sekvent_config::Prefixed::new(__sekvent_source, #nested_prefix)))
        }
        (Shape::Secret, _) => quote!(::sekvent_config::req_secret(__sekvent_source, #key)),
        (Shape::OptionalSecret, _) => quote!(::sekvent_config::opt_secret(__sekvent_source, #key)),
        (Shape::OptionalDuration, _) => {
            quote!(::sekvent_config::__private::opt_duration_value(__sekvent_source, #key))
        }
        (Shape::OptionalBool, _) => {
            quote!(::sekvent_config::__private::opt_bool_value(__sekvent_source, #key))
        }
        (Shape::Optional(inner), _) => {
            let read =
                quote_spanned!(inner.span()=> ::sekvent_config::__private::opt_value::<#inner>);
            quote!(#read(__sekvent_source, #key))
        }
        (Shape::Duration, None) => quote!(::sekvent_config::req_duration(__sekvent_source, #key)),
        (Shape::Duration, Some(lit)) => {
            quote!(::sekvent_config::__private::duration_or(__sekvent_source, #key, #lit))
        }
        (Shape::Bool, None) => quote!(::sekvent_config::req_bool(__sekvent_source, #key)),
        (Shape::Bool, Some(lit)) => {
            // `check_default` already rejected anything that is not a bool word.
            let value = bool_word(&lit.value()).unwrap_or_default();
            quote!(::sekvent_config::opt_bool(__sekvent_source, #key, #value))
        }
        (Shape::Parsed, None) => {
            let read = quote_spanned!(ty.span()=> ::sekvent_config::req_parse::<#ty>);
            quote!(#read(__sekvent_source, #key))
        }
        (Shape::Parsed, Some(lit)) => {
            let read = quote_spanned!(ty.span()=> ::sekvent_config::__private::parse_or::<#ty>);
            quote!(#read(__sekvent_source, #key, #lit))
        }
    };
    match &field.validate {
        Some(check) => quote! {
            ::core::result::Result::and_then(#base, |__sekvent_value| {
                ::sekvent_config::__private::validate(__sekvent_source, #key, __sekvent_value, #check)
            })
        },
        None => base,
    }
}

fn key_info(field: &FieldSpec, prefix: &str) -> TokenStream {
    let full = format!("{prefix}{}", field.key);
    if matches!(field.shape, Shape::Nested) {
        let ty = &field.ty;
        let nested_prefix = format!("{full}_");
        let keys = quote_spanned!(ty.span()=> <#ty as ::sekvent_config::FromConfig>::keys);
        return quote! {
            for mut __sekvent_key in #keys() {
                __sekvent_key.key = ::std::format!("{}{}", #nested_prefix, __sekvent_key.key);
                __sekvent_keys.push(__sekvent_key);
            }
        };
    }
    let secret = field.shape.is_secret();
    let required = field.default.is_none() && !field.shape.is_optional();
    let default_text = field
        .default
        .as_ref()
        .filter(|_| !secret)
        .map(LitStr::value);
    let default = option_string(default_text.as_deref());
    let doc = option_string(field.doc.as_deref());
    quote! {
        __sekvent_keys.push(::sekvent_config::KeyInfo {
            key: ::std::string::String::from(#full),
            required: #required,
            secret: #secret,
            default: #default,
            doc: #doc,
        });
    }
}

fn option_string(value: Option<&str>) -> TokenStream {
    if let Some(text) = value {
        quote!(::core::option::Option::Some(::std::string::String::from(#text)))
    } else {
        quote!(::core::option::Option::None)
    }
}

#[cfg(test)]
mod tests {
    use proc_macro2::{Delimiter, Group};
    use syn::parse_quote;

    use super::*;

    fn expand_ok(input: &DeriveInput) -> String {
        match expand(input) {
            Ok(tokens) => tokens.to_string(),
            Err(error) => panic!("unexpected error: {error}"),
        }
    }

    fn expand_err(input: &DeriveInput) -> String {
        match expand(input) {
            Ok(tokens) => panic!("expected an error, got {tokens}"),
            Err(error) => error.to_string(),
        }
    }

    fn compact(text: &str) -> String {
        text.chars().filter(|c| !c.is_whitespace()).collect()
    }

    #[test]
    fn full_struct_expands() {
        let input: DeriveInput = parse_quote! {
            #[config(prefix = "BILLING_")]
            struct Config<T: Clone> where T: Default {
                /// Listen port.
                ///
                /// Second paragraph.
                #[config(default = "8080")]
                port: u16,
                #[config(secret, key = "DB_PASSWORD")]
                password: Secret,
                api_key: Option<sekvent_config::Secret>,
                timeout: std::time::Duration,
                #[config(default = "5s")]
                grace: Duration,
                retry: Option<Duration>,
                #[config(default = "YES")]
                verbose: bool,
                strict: bool,
                dry_run: Option<bool>,
                name: Option<String>,
                #[config(validate = checks::positive)]
                workers: usize,
                #[config(nested, key = "DB")]
                database: DbConfig,
                r#type: String,
                marker: T,
            }
        };
        let out = compact(&expand_ok(&input));
        for needle in [
            "impl<T:Clone>::sekvent_config::FromConfigforConfig<T>whereT:Default",
            "Prefixed::new(__sekvent_source,\"BILLING_\")",
            "__private::parse_or::<u16>(__sekvent_source,\"PORT\",\"8080\")",
            "req_secret(__sekvent_source,\"DB_PASSWORD\")",
            "opt_secret(__sekvent_source,\"API_KEY\")",
            "req_duration(__sekvent_source,\"TIMEOUT\")",
            "duration_or(__sekvent_source,\"GRACE\",\"5s\")",
            "opt_duration_value(__sekvent_source,\"RETRY\")",
            "opt_bool(__sekvent_source,\"VERBOSE\",true)",
            "req_bool(__sekvent_source,\"STRICT\")",
            "opt_bool_value(__sekvent_source,\"DRY_RUN\")",
            "opt_value::<String>(__sekvent_source,\"NAME\")",
            "validate(__sekvent_source,\"WORKERS\",__sekvent_value,checks::positive)",
            "<DbConfigas::sekvent_config::FromConfig>::from_config(&::sekvent_config::Prefixed::new(__sekvent_source,\"DB_\"))",
            "req_parse::<String>(__sekvent_source,\"TYPE\")",
            "req_parse::<T>(__sekvent_source,\"MARKER\")",
            "key:::std::string::String::from(\"BILLING_PORT\"),required:false,secret:false,default:::core::option::Option::Some(::std::string::String::from(\"8080\"))",
            "doc:::core::option::Option::Some(::std::string::String::from(\"Listenport.\\n\\nSecondparagraph.\"))",
            "key:::std::string::String::from(\"BILLING_DB_PASSWORD\"),required:true,secret:true,default:::core::option::Option::None",
            "key:::std::string::String::from(\"BILLING_API_KEY\"),required:false,secret:true",
            "<DbConfigas::sekvent_config::FromConfig>::keys()",
            "::std::format!(\"{}{}\",\"BILLING_DB_\",__sekvent_key.key)",
        ] {
            assert!(
                out.contains(&compact(needle)),
                "missing `{needle}` in\n{out}"
            );
        }
    }

    #[test]
    fn empty_struct_expands_without_accumulators() {
        let input: DeriveInput = parse_quote! {
            #[config(prefix = "X_")]
            struct Empty {}
        };
        let out = compact(&expand_ok(&input));
        assert!(out.contains("Ok(Self{})"), "{out}");
        assert!(!out.contains("__sekvent_errors"), "{out}");
        assert!(!out.contains("__sekvent_keys"), "{out}");
    }

    #[test]
    fn no_prefix_means_no_scoped_source() {
        let input: DeriveInput = parse_quote! {
            struct Plain { port: u16 }
        };
        let out = expand_ok(&input);
        assert!(!out.contains("__sekvent_scoped"), "{out}");
        assert!(compact(&out).contains("String::from(\"PORT\")"), "{out}");
    }

    #[test]
    fn shape_errors() {
        let cases: [(DeriveInput, &str); 4] = [
            (
                parse_quote!(
                    struct Tuple(String);
                ),
                "tuple structs are not supported",
            ),
            (
                parse_quote!(
                    struct Unit;
                ),
                "unit structs are not supported",
            ),
            (
                parse_quote!(
                    enum Mode {
                        A,
                        B,
                    }
                ),
                "not enums",
            ),
            (parse_quote!(union Bits { a: u8, b: i8 }), "not unions"),
        ];
        for (input, needle) in cases {
            let error = expand_err(&input);
            assert!(error.contains(needle), "{error}");
        }
    }

    #[test]
    fn attribute_errors() {
        let cases: [(DeriveInput, &str); 7] = [
            (
                parse_quote!(
                    struct C {
                        #[config(secret)]
                        token: String,
                    }
                ),
                "`#[config(secret)]` requires the field type to be `Secret` or `Option<Secret>`",
            ),
            (
                parse_quote!(
                    struct C {
                        #[config(nested, default = "x")]
                        db: Db,
                    }
                ),
                "`default` cannot be combined with `nested`",
            ),
            (
                parse_quote!(
                    struct C {
                        #[config(nested, secret)]
                        db: Secret,
                    }
                ),
                "`secret` cannot be combined with `nested`",
            ),
            (
                parse_quote!(
                    struct C {
                        #[config(colour = "red")]
                        a: u8,
                    }
                ),
                "unknown config attribute `colour`; expected one of",
            ),
            (
                parse_quote!(
                    #[config(colour = "red")]
                    struct C {
                        a: u8,
                    }
                ),
                "unknown config attribute `colour` on a struct; expected `prefix`",
            ),
            (
                parse_quote!(
                    #[config(prefix = "A_", prefix = "B_")]
                    struct C {
                        a: u8,
                    }
                ),
                "duplicate config attribute `prefix`",
            ),
            (
                parse_quote!(
                    struct C {
                        #[config(key = "A")]
                        #[config(key = "B")]
                        a: u8,
                    }
                ),
                "duplicate config attribute `key`",
            ),
        ];
        for (input, needle) in cases {
            let error = expand_err(&input);
            assert!(error.contains(needle), "`{needle}` not in `{error}`");
        }
    }

    #[test]
    fn attribute_value_errors() {
        let cases: [(DeriveInput, &str); 6] = [
            (
                parse_quote!(
                    struct C {
                        #[config(key = "")]
                        a: u8,
                    }
                ),
                "`key` must not be empty",
            ),
            (
                parse_quote!(
                    struct C {
                        #[config(default = "x")]
                        a: Secret,
                    }
                ),
                "a secret cannot have a default",
            ),
            (
                parse_quote!(
                    struct C {
                        #[config(default = "x")]
                        a: Option<Secret>,
                    }
                ),
                "a secret cannot have a default",
            ),
            (
                parse_quote!(
                    struct C {
                        #[config(default = "1")]
                        a: Option<u8>,
                    }
                ),
                "`default` has no effect on an `Option` field",
            ),
            (
                parse_quote!(
                    struct C {
                        #[config(default = "maybe")]
                        a: bool,
                    }
                ),
                "the default of a `bool` field must be one of",
            ),
            (
                parse_quote!(
                    struct C {
                        #[config(key = 5)]
                        a: u8,
                    }
                ),
                "expected string literal",
            ),
        ];
        for (input, needle) in cases {
            let error = expand_err(&input);
            assert!(error.contains(needle), "`{needle}` not in `{error}`");
        }
    }

    #[test]
    fn every_field_error_is_reported() {
        let input: DeriveInput = parse_quote! {
            struct C {
                #[config(secret)] a: String,
                #[config(bogus)] b: u8,
                ok: u8,
            }
        };
        let errors: Vec<String> = match expand(&input) {
            Ok(tokens) => panic!("expected errors, got {tokens}"),
            Err(error) => error.into_iter().map(|error| error.to_string()).collect(),
        };
        assert_eq!(errors.len(), 2, "{errors:?}");
        assert!(errors[1].contains("`bogus`"), "{errors:?}");
    }

    #[test]
    fn classification() {
        let group = Group::new(Delimiter::None, quote!(Secret));
        let grouped: Type = syn::parse2(quote!(#group)).expect("a none-delimited group is a type");
        assert!(matches!(classify(&grouped), Shape::Secret));
        assert!(matches!(classify(&parse_quote!((Secret))), Shape::Secret));
        assert!(matches!(
            classify(&parse_quote!(Option<Secret>)),
            Shape::OptionalSecret
        ));
        assert!(matches!(
            classify(&parse_quote!(core::option::Option<core::time::Duration>)),
            Shape::OptionalDuration
        ));
        assert!(matches!(
            classify(&parse_quote!(Option<bool>)),
            Shape::OptionalBool
        ));
        assert!(matches!(
            classify(&parse_quote!(Option<u8>)),
            Shape::Optional(_)
        ));
        assert!(matches!(classify(&parse_quote!(bool)), Shape::Bool));
        assert!(matches!(classify(&parse_quote!(Duration)), Shape::Duration));
        let parsed: [Type; 7] = [
            parse_quote!(Secret<u8>),
            parse_quote!(Option<u8, u8>),
            parse_quote!(Option<'static>),
            parse_quote!(Option),
            parse_quote!(<T as Trait>::Secret),
            parse_quote!(&'static str),
            parse_quote!(Vec<String>),
        ];
        for ty in parsed {
            assert!(matches!(classify(&ty), Shape::Parsed), "{}", quote!(#ty));
        }
    }

    #[test]
    fn upper_snake_names() {
        assert_eq!(upper_snake("db_url"), "DB_URL");
        assert_eq!(upper_snake("dbUrl"), "DB_URL");
        assert_eq!(upper_snake("http2Port"), "HTTP2_PORT");
        assert_eq!(upper_snake("TLS"), "TLS");
        assert_eq!(upper_snake("port"), "PORT");
    }

    #[test]
    fn doc_extraction() {
        let field: Field = parse_quote! {
            /// First line.
            ///   indented
            #[doc(hidden)]
            #[doc = concat!("x", "y")]
            a: u8
        };
        assert_eq!(
            doc_of(&field.attrs).as_deref(),
            Some("First line.\n  indented")
        );
        let blank: Field = parse_quote! {
            ///
            a: u8
        };
        assert_eq!(doc_of(&blank.attrs), None);
    }

    #[test]
    fn bool_words() {
        assert_eq!(bool_word(" On "), Some(true));
        assert_eq!(bool_word("0"), Some(false));
        assert_eq!(bool_word("maybe"), None);
    }

    #[test]
    fn false_bool_default_and_validated_option() {
        let input: DeriveInput = parse_quote! {
            struct C {
                #[config(default = "off")]
                a: bool,
                #[config(validate = check)]
                b: Option<u8>,
            }
        };
        let out = compact(&expand_ok(&input));
        assert!(
            out.contains("opt_bool(__sekvent_source,\"A\",false)"),
            "{out}"
        );
        assert!(
            out.contains("and_then(::sekvent_config::__private::opt_value::<u8>"),
            "{out}"
        );
    }
}
