//! Code generation for `#[component]` (the items of spec section 3.3).

use proc_macro2::{Ident, Span, TokenStream};
use quote::{format_ident, quote, quote_spanned};
use syn::ext::IdentExt;
use syn::spanned::Spanned;

use super::parse::{Args, Component, Method, Mode, rpc_name};

/// The generic parameter the expansion introduces for an implementation.
/// Generic parameters resolve at the call site, so the name must not be
/// one a user would pick for a type of their own.
const IMPL: &str = "__SekventImpl";
/// The generic parameter of an `install` factory.
const FACTORY: &str = "__SekventFactory";

/// A local variable or parameter of the expansion. The mixed-site span
/// keeps it apart from the user's items and bindings of the same name, as
/// in a `macro_rules!` macro, without changing how it reads.
fn local(name: &str) -> Ident {
    Ident::new(name, Span::mixed_site())
}

/// A generic parameter of the expansion.
fn generic(name: &str) -> Ident {
    Ident::new(name, Span::call_site())
}

/// Identifiers of the generated items.
struct Names {
    handle: Ident,
    object: Ident,
    dispatcher: Ident,
}

/// Every item generated for one component, in declaration order.
pub(crate) fn generate(args: &Args, component: &Component, krate: &TokenStream) -> TokenStream {
    let names = Names {
        handle: format_ident!("{}Handle", component.ident),
        object: format_ident!("__{}Dyn", component.ident),
        dispatcher: format_ident!("__{}Dispatcher", component.ident),
    };
    let definition = trait_definition(component);
    let object = object_trait(component, &names, krate);
    let handle = handle(args, component, &names, krate);
    let descriptor = descriptor(args, component, &names, krate);
    let dispatcher = (args.mode != Mode::LocalOnly).then(|| dispatcher(component, &names, krate));
    let assertions = assertions(args.mode, component, krate);
    let contract = contract(args, component, krate);
    quote! {
        #definition
        #object
        #handle
        #descriptor
        #dispatcher
        #assertions
        #contract
    }
}

/// The trait itself: `async fn` becomes `fn -> impl Future + Send`, kind
/// attributes are dropped and missing supertraits are added.
fn trait_definition(component: &Component) -> TokenStream {
    let Component {
        attrs,
        vis,
        ident,
        supertraits,
        methods,
    } = component;
    let methods = methods.iter().map(|method| {
        let Method {
            docs,
            ident,
            inputs,
            output,
            ..
        } = method;
        quote! {
            #(#docs)*
            fn #ident(#inputs)
                -> impl ::core::future::Future<Output = #output> + ::core::marker::Send;
        }
    });
    quote! {
        #(#attrs)*
        #vis trait #ident: #(#supertraits)+* {
            #(#methods)*
        }
    }
}

/// The object-safe twin with boxed futures, and its blanket impl.
fn object_trait(component: &Component, names: &Names, krate: &TokenStream) -> TokenStream {
    let vis = &component.vis;
    let contract = &component.ident;
    let object = &names.object;
    let implementation = generic(IMPL);
    let (cx, req) = (local("cx"), local("req"));
    let signatures: Vec<TokenStream> = component
        .methods
        .iter()
        .map(|method| {
            let hidden = hidden_name(method);
            let context = &method.context;
            let request = &method.request;
            let output = &method.output;
            quote! {
                fn #hidden<'a>(&'a self, #cx: &'a #context, #req: #request)
                    -> #krate::__private::BoxFuture<'a, #output>
            }
        })
        .collect();
    let bodies = component
        .methods
        .iter()
        .zip(&signatures)
        .map(|(method, signature)| {
            let ident = &method.ident;
            quote! {
                #signature {
                    ::std::boxed::Box::pin(<#implementation as #contract>::#ident(self, #cx, #req))
                }
            }
        });
    quote! {
        #[doc(hidden)]
        #vis trait #object: ::core::marker::Send + ::core::marker::Sync + 'static {
            #(#signatures;)*
        }

        #[allow(clippy::all, clippy::pedantic)]
        impl<#implementation: #contract> #object for #implementation {
            #(#bodies)*
        }
    }
}

/// The handle struct and its inherent impl.
fn handle(args: &Args, component: &Component, names: &Names, krate: &TokenStream) -> TokenStream {
    let vis = &component.vis;
    let handle = &names.handle;
    let object = &names.object;
    let summary = format!(
        " Cloneable handle to the [`{}`] component (`{}`).",
        component.ident.unraw(),
        args.name
    );
    let descriptors = component
        .methods
        .iter()
        .map(|method| method_descriptor(method, krate));
    let calls = component
        .methods
        .iter()
        .enumerate()
        .map(|(index, method)| handle_method(args.mode, index, method, names, krate));
    let installs = installs(args.mode, component, names, krate);
    quote! {
        #[doc = #summary]
        ///
        /// Get it from `Deps::handle` in a factory or from `App::handle`; the App
        /// builder decides whether its calls run in-process or across a
        /// serialization boundary.
        #[derive(Clone, Debug)]
        #vis struct #handle(#krate::__private::Endpoint<dyn #object>);

        #[allow(clippy::all, clippy::pedantic)]
        impl #handle {
            #[doc(hidden)]
            pub const __METHODS: &'static [#krate::MethodDescriptor] = &[
                #(#descriptors),*
            ];

            #(#calls)*

            /// The binding this handle's calls use.
            pub fn binding(&self) -> #krate::Binding {
                self.0.binding()
            }

            #installs
        }
    }
}

/// `MethodDescriptor::call("reserve", "Reserve")` plus the `#[call]` policy.
fn method_descriptor(method: &Method, krate: &TokenStream) -> TokenStream {
    let name = method.ident.to_string();
    let rpc = rpc_name(&name);
    let policy = &method.policy;
    let idempotent = policy.idempotent.then(|| quote!(.with_idempotent()));
    let timeout = policy.timeout.map(|timeout| {
        let secs = timeout.as_secs();
        let nanos = timeout.subsec_nanos();
        quote!(.with_timeout(::core::time::Duration::new(#secs, #nanos)))
    });
    let bulkhead = policy.bulkhead.map(|limit| quote!(.with_bulkhead(#limit)));
    let anonymous = policy.anonymous.then(|| quote!(.with_anonymous()));
    quote! {
        #krate::MethodDescriptor::call(#name, #rpc)
            #idempotent
            #timeout
            #bulkhead
            #anonymous
    }
}

/// One handle method: route the call through the endpoint.
fn handle_method(
    mode: Mode,
    index: usize,
    method: &Method,
    names: &Names,
    krate: &TokenStream,
) -> TokenStream {
    let Method {
        ident,
        context,
        request,
        output,
        ..
    } = method;
    let docs = if method.docs.is_empty() {
        let text = format!(" Call `{ident}` on the component.");
        quote!(#[doc = #text])
    } else {
        let docs = &method.docs;
        quote!(#(#docs)*)
    };
    let object = &names.object;
    let hidden = hidden_name(method);
    let route = if mode == Mode::LocalOnly {
        quote!(call_local)
    } else {
        quote!(call)
    };
    let (cx, req, imp) = (local("cx"), local("req"), local("imp"));
    quote! {
        #docs
        pub fn #ident<'a>(&'a self, #cx: &'a #context, #req: #request)
            -> impl ::core::future::Future<Output = #output> + ::core::marker::Send + 'a
        {
            self.0.#route(#index, #cx, #req,
                |#imp: ::std::sync::Arc<dyn #object>,
                 #cx: #krate::CallContext,
                 #req: #request| async move { #object::#hidden(&*#imp, &#cx, #req).await })
        }
    }
}

/// `install` and `install_with_lifecycle`, or `install_remote`.
fn installs(mode: Mode, component: &Component, names: &Names, krate: &TokenStream) -> TokenStream {
    let Names {
        handle,
        object,
        dispatcher,
    } = names;
    let app = local("app");
    if mode == Mode::RemoteOnly {
        return quote! {
            /// Declare this remote-only component; it must be bound to a remote transport.
            pub fn install_remote(#app: &mut #krate::AppBuilder<'_>)
                -> ::core::result::Result<(), #krate::BuildError>
            {
                #krate::__private::install_remote(#app, #handle)
            }
        };
    }
    let contract = &component.ident;
    let (implementation, factory_type) = (generic(IMPL), generic(FACTORY));
    let (factory, deps, imp, concrete, dispatch) = (
        local("factory"),
        local("deps"),
        local("imp"),
        local("concrete"),
        local("dispatch"),
    );
    let (make_dispatch, with_dispatch) = if mode == Mode::LocalOnly {
        (None, None)
    } else {
        (
            Some(quote! {
                let #dispatch = ::std::sync::Arc::new(#dispatcher(::std::sync::Arc::clone(&#imp)));
            }),
            Some(quote!(.with_dispatch(#dispatch))),
        )
    };
    let lifecycle_summary =
        format!(" Like [`{handle}::install`], and also run the implementation's");
    quote! {
        /// Install the implementation `factory` builds. The factory runs during
        /// `AppBuilder::build`, in install order, and only when the component is
        /// bound `local` or `local-serialized`.
        pub fn install<#implementation, #factory_type>(
            #app: &mut #krate::AppBuilder<'_>,
            #factory: #factory_type,
        ) -> ::core::result::Result<(), #krate::BuildError>
        where
            #implementation: #contract,
            #factory_type: ::core::ops::FnOnce(&mut #krate::Deps<'_>)
                -> ::core::result::Result<#implementation, #krate::AppError>
                + ::core::marker::Send + 'static,
        {
            #krate::__private::install_local(#app, #handle, move |#deps| {
                let #imp: ::std::sync::Arc<dyn #object> = ::std::sync::Arc::new(#factory(#deps)?);
                #make_dispatch
                ::core::result::Result::Ok(
                    #krate::__private::Local::new(#imp) #with_dispatch)
            })
        }

        #[doc = #lifecycle_summary]
        /// `Lifecycle` hooks when the App starts and stops.
        pub fn install_with_lifecycle<#implementation, #factory_type>(
            #app: &mut #krate::AppBuilder<'_>,
            #factory: #factory_type,
        ) -> ::core::result::Result<(), #krate::BuildError>
        where
            #implementation: #contract + #krate::Lifecycle,
            #factory_type: ::core::ops::FnOnce(&mut #krate::Deps<'_>)
                -> ::core::result::Result<#implementation, #krate::AppError>
                + ::core::marker::Send + 'static,
        {
            #krate::__private::install_local(#app, #handle, move |#deps| {
                let #concrete = ::std::sync::Arc::new(#factory(#deps)?);
                let #imp: ::std::sync::Arc<dyn #object> =
                    ::std::sync::Arc::<#implementation>::clone(&#concrete);
                #make_dispatch
                ::core::result::Result::Ok(#krate::__private::Local::new(#imp)
                    #with_dispatch
                    .with_lifecycle(#concrete))
            })
        }
    }
}

/// `impl ComponentHandle`: the static descriptor.
fn descriptor(
    args: &Args,
    component: &Component,
    names: &Names,
    krate: &TokenStream,
) -> TokenStream {
    let handle = &names.handle;
    let name = &args.name;
    let service = component.ident.unraw().to_string();
    let package = args
        .package
        .as_ref()
        .map(|package| quote!(.with_package(#package)));
    let mode = match args.mode {
        Mode::Standard => None,
        Mode::LocalOnly => Some(quote!(.with_mode(#krate::ComponentMode::LocalOnly))),
        Mode::RemoteOnly => Some(quote!(.with_mode(#krate::ComponentMode::RemoteOnly))),
    };
    quote! {
        #[allow(clippy::all, clippy::pedantic)]
        impl #krate::ComponentHandle for #handle {
            const DESCRIPTOR: &'static #krate::ComponentDescriptor =
                &#krate::ComponentDescriptor::new(#name, #service, #handle::__METHODS)
                    #package
                    #mode;
        }
    }
}

/// The byte-level dispatcher (standard and `remote_only` components).
fn dispatcher(component: &Component, names: &Names, krate: &TokenStream) -> TokenStream {
    let vis = &component.vis;
    let Names {
        handle,
        object,
        dispatcher,
    } = names;
    let (method, cx, body, imp, req) = (
        local("method"),
        local("cx"),
        local("body"),
        local("imp"),
        local("req"),
    );
    let arms = component.methods.iter().enumerate().map(|(index, one)| {
        let request = &one.request;
        let hidden = hidden_name(one);
        quote! {
            #index => #krate::__private::serve(#cx, #body,
                move |#cx: #krate::CallContext, #req: #request| async move {
                    #object::#hidden(&*#imp, &#cx, #req).await
                }),
        }
    });
    quote! {
        #[doc(hidden)]
        #vis struct #dispatcher(::std::sync::Arc<dyn #object>);

        #[allow(clippy::all, clippy::pedantic)]
        impl #krate::__private::Dispatch for #dispatcher {
            fn dispatch(&self, #method: usize, #cx: #krate::CallContext,
                        #body: #krate::__private::Bytes)
                -> #krate::__private::BoxFuture<'static,
                    ::core::result::Result<#krate::__private::Bytes, #krate::AppError>>
            {
                let #imp = ::std::sync::Arc::clone(&self.0);
                match #method {
                    #(#arms)*
                    _ => #krate::__private::unknown_method(
                        <#handle as #krate::ComponentHandle>::DESCRIPTOR, #method),
                }
            }
        }
    }
}

/// Type assertions rustc checks for the macro: messages (or local types)
/// and component errors.
fn assertions(mode: Mode, component: &Component, krate: &TokenStream) -> TokenStream {
    let message = if mode == Mode::LocalOnly {
        quote!(assert_local)
    } else {
        quote!(assert_wire)
    };
    let checks = component.methods.iter().map(|method| {
        let Method {
            request,
            reply,
            error,
            ..
        } = method;
        let request = quote_spanned!(request.span()=>
            #krate::__private::#message::<#request>();
        );
        let reply = quote_spanned!(reply.span()=>
            #krate::__private::#message::<#reply>();
        );
        let error = quote_spanned!(error.span()=>
            #krate::__private::assert_error::<#error>();
        );
        quote!(#request #reply #error)
    });
    quote! {
        const _: () = {
            #(#checks)*
        };
    }
}

/// The checks tying the trait to what sekvent-proto-build emitted into the
/// `proto` module: the `__sekvent_service_<Trait>` constant (service name,
/// one RPC per method and no other, unary) and, per method, the
/// `__sekvent_rpc_<Trait>__<Rpc>` alias of the RPC's Rust request and reply
/// types, compared by type identity. Nothing for a `local_only` component,
/// which has no `proto`.
fn contract(args: &Args, component: &Component, krate: &TokenStream) -> TokenStream {
    let (Some(proto), Some(package)) = (&args.proto, &args.package) else {
        return TokenStream::new();
    };
    let service_name = component.ident.unraw().to_string();
    let constant = format_ident!("__sekvent_service_{}", service_name);
    let full_name = format!("{package}.{service_name}");
    let rpcs: Vec<String> = component
        .methods
        .iter()
        .map(|method| rpc_name(&method.ident.to_string()))
        .collect();
    let service = quote_spanned! {component.ident.span()=>
        const _: () = #krate::__private::assert_service(
            #proto::#constant,
            #full_name,
            &[#(#rpcs),*],
        );
    };
    let checks = component.methods.iter().zip(&rpcs).map(|(method, rpc)| {
        let Method { request, reply, .. } = method;
        let types = format_ident!("__sekvent_rpc_{}__{}", service_name, rpc);
        quote_spanned! {method.signature=>
            const _: () = #krate::__private::assert_rpc::<#request, #reply>(
                #proto::#constant,
                #rpc,
            );
            const _: () = #krate::__private::assert_rpc_types::<#request, #reply, #proto::#types>();
        }
    });
    quote! {
        #service
        #(#checks)*
    }
}

/// `__reserve` for `reserve`.
fn hidden_name(method: &Method) -> Ident {
    format_ident!("__{}", method.ident)
}
