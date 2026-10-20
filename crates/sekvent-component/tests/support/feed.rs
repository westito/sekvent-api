//! A `local_only` component with one streaming method, expanded by hand the
//! way `#[component(name = "feed", local_only)]` expands
//! `#[call(stream)] async fn lines(&self, cx: &CallContext, req: String)
//! -> Result<CallStream<String, AppError>, AppError>` (streaming note,
//! section 11.3, snapshot `component__local_only_stream`).
#![allow(dead_code, missing_docs)]

use sekvent_component::{AppError, CallContext, CallStream};

pub trait Feed: Send + Sync + 'static {
    fn lines(
        &self,
        cx: &CallContext,
        req: String,
    ) -> impl ::core::future::Future<Output = Result<CallStream<String, AppError>, AppError>>
    + ::core::marker::Send;
}

#[doc(hidden)]
pub trait __FeedDyn: ::core::marker::Send + ::core::marker::Sync + 'static {
    fn __lines<'a>(
        &'a self,
        cx: &'a CallContext,
        req: String,
    ) -> ::sekvent_component::__private::BoxFuture<'a, Result<CallStream<String, AppError>, AppError>>;
}

#[allow(clippy::all, clippy::pedantic)]
impl<__SekventImpl: Feed> __FeedDyn for __SekventImpl {
    fn __lines<'a>(
        &'a self,
        cx: &'a CallContext,
        req: String,
    ) -> ::sekvent_component::__private::BoxFuture<'a, Result<CallStream<String, AppError>, AppError>>
    {
        ::std::boxed::Box::pin(<__SekventImpl as Feed>::lines(self, cx, req))
    }
}

/// Cloneable handle to the [`Feed`] component (`feed`).
#[derive(Clone, Debug)]
pub struct FeedHandle(::sekvent_component::__private::Endpoint<dyn __FeedDyn>);

#[allow(clippy::all, clippy::pedantic)]
impl FeedHandle {
    #[doc(hidden)]
    pub const __METHODS: &'static [::sekvent_component::MethodDescriptor] =
        &[::sekvent_component::MethodDescriptor::call("lines", "Lines").with_stream()];

    pub fn lines<'a>(
        &'a self,
        cx: &'a CallContext,
        req: String,
    ) -> impl ::core::future::Future<Output = Result<CallStream<String, AppError>, AppError>>
    + ::core::marker::Send
    + 'a {
        self.0.call_stream_local(
            0usize,
            cx,
            req,
            |imp: ::std::sync::Arc<dyn __FeedDyn>,
             cx: ::sekvent_component::CallContext,
             req: String| async move { __FeedDyn::__lines(&*imp, &cx, req).await },
        )
    }

    /// The binding this handle's calls use.
    pub fn binding(&self) -> ::sekvent_component::Binding {
        self.0.binding()
    }

    pub fn install<__SekventImpl, __SekventFactory>(
        app: &mut ::sekvent_component::AppBuilder<'_>,
        factory: __SekventFactory,
    ) -> ::core::result::Result<(), ::sekvent_component::BuildError>
    where
        __SekventImpl: Feed,
        __SekventFactory: ::core::ops::FnOnce(
                &mut ::sekvent_component::Deps<'_>,
            ) -> ::core::result::Result<
                __SekventImpl,
                ::sekvent_component::AppError,
            > + ::core::marker::Send
            + 'static,
    {
        ::sekvent_component::__private::install_local(app, FeedHandle, move |deps| {
            let imp: ::std::sync::Arc<dyn __FeedDyn> = ::std::sync::Arc::new(factory(deps)?);
            ::core::result::Result::Ok(::sekvent_component::__private::Local::new(imp))
        })
    }
}

#[allow(clippy::all, clippy::pedantic)]
impl ::sekvent_component::ComponentHandle for FeedHandle {
    const DESCRIPTOR: &'static ::sekvent_component::ComponentDescriptor =
        &::sekvent_component::ComponentDescriptor::new("feed", "Feed", FeedHandle::__METHODS)
            .with_mode(::sekvent_component::ComponentMode::LocalOnly);
}

const _: () = {
    ::sekvent_component::__private::assert_local::<String>();
    ::sekvent_component::__private::assert_local::<String>();
    ::sekvent_component::__private::assert_error::<AppError>();
    ::sekvent_component::__private::assert_error::<AppError>();
};

/// Splits its request into lines, one item each; an empty request fails the
/// open, a line `boom` ends the stream with an error.
pub struct Lines;

impl Feed for Lines {
    async fn lines(
        &self,
        _cx: &CallContext,
        req: String,
    ) -> Result<CallStream<String, AppError>, AppError> {
        if req.is_empty() {
            return Err(AppError::invalid_argument("nothing to feed").with_reason("EMPTY_FEED"));
        }
        let items: Vec<Result<String, AppError>> =
            req.lines()
                .map(|line| match line {
                    "boom" => Err(AppError::internal(std::io::Error::other("boom"))
                        .with_reason("FEED_BROKEN")),
                    line => Ok(line.to_owned()),
                })
                .collect();
        Ok(CallStream::iter(items))
    }
}
