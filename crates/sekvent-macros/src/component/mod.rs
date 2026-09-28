//! `#[component]`: a trait becomes a component contract with a handle,
//! an object-safe twin, a byte-level dispatcher and a descriptor.

mod expand;
mod parse;
#[cfg(test)]
mod tests;

use proc_macro2::TokenStream;
use quote::ToTokens;
use syn::{Error, Item, Result};

/// Expand `#[component(args)] item`. `runtime` yields the path of the
/// `sekvent_component` crate when the arguments do not name one.
pub(crate) fn expand(
    args: TokenStream,
    item: TokenStream,
    runtime: impl FnOnce() -> TokenStream,
) -> Result<TokenStream> {
    let args = parse::args(args);
    let component = match syn::parse2::<Item>(item)? {
        Item::Trait(item) => parse::component(item),
        other => Err(Error::new_spanned(other, "#[component] applies to a trait")),
    };
    match (args, component) {
        (Ok(args), Ok(component)) => {
            let krate = match &args.krate {
                Some(path) => path.to_token_stream(),
                None => runtime(),
            };
            Ok(expand::generate(&args, &component, &krate))
        }
        (Err(mut error), Err(other)) => {
            error.combine(other);
            Err(error)
        }
        (Err(error), Ok(_)) | (Ok(_), Err(error)) => Err(error),
    }
}

/// Collects independent errors so one compile reports all of them.
#[derive(Default)]
pub(crate) struct Errors(Option<Error>);

impl Errors {
    pub(crate) fn push(&mut self, error: Error) {
        match &mut self.0 {
            Some(all) => all.combine(error),
            None => self.0 = Some(error),
        }
    }

    /// The value of `result`, or `None` after recording its error.
    pub(crate) fn take<T>(&mut self, result: Result<T>) -> Option<T> {
        match result {
            Ok(value) => Some(value),
            Err(error) => {
                self.push(error);
                None
            }
        }
    }

    pub(crate) fn finish(self) -> Result<()> {
        self.0.map_or(Ok(()), Err)
    }
}
