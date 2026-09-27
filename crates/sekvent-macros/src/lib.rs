//! Procedural macros for sekvent.
//!
//! Use them through the crates that re-export them (`sekvent_config::EnvConfig`);
//! the generated code refers to those crates by absolute path.

#![forbid(unsafe_code)]

mod env_config;

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
/// Container attribute: `#[config(prefix = "BILLING_")]` scopes every key.
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
