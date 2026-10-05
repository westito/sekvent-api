//! Where a macro's runtime crate lives, as seen from the crate being compiled.

use proc_macro_crate::FoundCrate;
use proc_macro2::{Span, TokenStream};
use quote::quote;
use syn::Ident;

/// The path of the runtime library `lib` (for example `sekvent_config`):
/// a direct dependency first (`direct`, under its possibly renamed name),
/// else the `module` of the facade (`facade`), else `::<lib>`
/// (inside the runtime crate itself, which aliases itself under that name,
/// and when the manifest cannot be read).
pub(crate) fn runtime_path(
    direct: Option<FoundCrate>,
    facade: impl FnOnce() -> Option<FoundCrate>,
    lib: &str,
    module: &str,
) -> TokenStream {
    let ident = |name: &str| Ident::new(name, Span::call_site());
    match direct {
        Some(FoundCrate::Name(name)) => {
            let name = ident(&name);
            quote!(::#name)
        }
        Some(FoundCrate::Itself) => {
            let lib = ident(lib);
            quote!(::#lib)
        }
        None => match facade() {
            Some(FoundCrate::Name(name)) => {
                let name = ident(&name);
                let module = ident(module);
                quote!(::#name::#module)
            }
            Some(FoundCrate::Itself) => {
                let module = ident(module);
                quote!(crate::#module)
            }
            None => {
                let lib = ident(lib);
                quote!(::#lib)
            }
        },
    }
}

/// The facade's Cargo package. Its library is named `sekvent`.
const FACADE_PACKAGE: &str = "sekvent-api";

/// The extern crate name of the facade dependency `found`.
///
/// proc-macro-crate derives a name from the dependency key (`-` becomes
/// `_`), not from the library name, so an unrenamed `sekvent-api`
/// dependency, like the facade's own integration tests, comes back as
/// `sekvent_api` while the crate is `sekvent`. A renamed key is the extern
/// name as is; only a key spelled `sekvent_api` is ambiguous, and it is read
/// as the unrenamed dependency.
fn facade_name(found: Option<FoundCrate>) -> Option<FoundCrate> {
    match found {
        Some(FoundCrate::Name(name)) if name == "sekvent_api" => {
            Some(FoundCrate::Name("sekvent".to_owned()))
        }
        other => other,
    }
}

/// [`runtime_path`] for the Cargo package `package`, read from the calling
/// crate's manifest.
pub(crate) fn resolve(package: &str, lib: &str, module: &str) -> TokenStream {
    runtime_path(
        proc_macro_crate::crate_name(package).ok(),
        || facade_name(proc_macro_crate::crate_name(FACADE_PACKAGE).ok()),
        lib,
        module,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn name(name: &str) -> FoundCrate {
        FoundCrate::Name(name.to_owned())
    }

    fn unreachable() -> Option<FoundCrate> {
        panic!("the facade is not consulted")
    }

    fn path(tokens: &TokenStream) -> String {
        tokens
            .to_string()
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect()
    }

    #[test]
    fn the_config_path_follows_the_dependency_graph() {
        let config = |direct, facade: fn() -> Option<FoundCrate>| {
            path(&runtime_path(direct, facade, "sekvent_config", "config"))
        };
        assert_eq!(
            config(Some(name("sekvent_config")), unreachable),
            "::sekvent_config"
        );
        assert_eq!(config(Some(name("cfg")), unreachable), "::cfg");
        assert_eq!(
            config(Some(FoundCrate::Itself), unreachable),
            "::sekvent_config"
        );
        assert_eq!(config(None, || Some(name("sekvent"))), "::sekvent::config");
        assert_eq!(config(None, || Some(name("fw"))), "::fw::config");
        assert_eq!(config(None, || Some(FoundCrate::Itself)), "crate::config");
        assert_eq!(config(None, || None), "::sekvent_config");
    }

    #[test]
    fn the_component_path_follows_the_dependency_graph() {
        let component = |direct, facade: fn() -> Option<FoundCrate>| {
            path(&runtime_path(
                direct,
                facade,
                "sekvent_component",
                "component",
            ))
        };
        assert_eq!(
            component(Some(name("sekvent_component")), unreachable),
            "::sekvent_component"
        );
        assert_eq!(component(Some(name("parts")), unreachable), "::parts");
        assert_eq!(
            component(Some(FoundCrate::Itself), unreachable),
            "::sekvent_component"
        );
        assert_eq!(
            component(None, || Some(name("sekvent"))),
            "::sekvent::component"
        );
        assert_eq!(component(None, || Some(name("fw"))), "::fw::component");
        assert_eq!(
            component(None, || Some(FoundCrate::Itself)),
            "crate::component"
        );
        assert_eq!(component(None, || None), "::sekvent_component");
    }

    #[test]
    fn the_facade_is_named_by_its_library() {
        assert_eq!(
            facade_name(Some(name("sekvent_api"))),
            Some(name("sekvent"))
        );
        assert_eq!(facade_name(Some(name("fw"))), Some(name("fw")));
        assert_eq!(facade_name(Some(name("sekvent"))), Some(name("sekvent")));
        assert_eq!(
            facade_name(Some(FoundCrate::Itself)),
            Some(FoundCrate::Itself)
        );
        assert_eq!(facade_name(None), None);
        let config = path(&runtime_path(
            None,
            || facade_name(Some(name("sekvent_api"))),
            "sekvent_config",
            "config",
        ));
        assert_eq!(config, "::sekvent::config");
    }

    #[test]
    fn resolve_reads_the_manifest_of_the_crate_being_compiled() {
        // Under `cargo test` the manifest is this crate's; it depends on
        // neither name directly, so the result is a well-formed path either way.
        let resolved = path(&resolve(
            "sekvent-component",
            "sekvent_component",
            "component",
        ));
        assert!(
            resolved.starts_with("::") || resolved.starts_with("crate::"),
            "{resolved}"
        );
    }
}
