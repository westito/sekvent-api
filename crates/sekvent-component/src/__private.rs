//! Support code for `#[component]` and `#[derive(ComponentError)]`.
//!
//! Not a stable API: only generated code should use it.

use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::str::FromStr;
use std::sync::Arc;

pub use bytes::Bytes;
pub use prost;

use sekvent_context::CallContext;
use sekvent_error::AppError;

use crate::app::{Built, Handle, RemoteFactory, erase};
use crate::lifecycle::LifecycleDyn;
use crate::link::Route;
use crate::{
    AppBuilder, Binding, BuildError, ComponentDescriptor, ComponentError, ComponentHandle, Deps,
    Lifecycle, reasons,
};

pub use crate::contract::{ContractMessage, ProtoRpc, ProtoService, assert_rpc, assert_service};
pub use crate::link::Endpoint;

/// A boxed, sendable future.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// A request or reply of a standard component.
#[diagnostic::on_unimplemented(
    message = "`{Self}` is not a protobuf message",
    note = "component requests and replies must be prost messages; declare the component `local_only` to use plain Rust types"
)]
pub trait WireMessage: prost::Message + Default + Send + 'static {}
impl<T: prost::Message + Default + Send + 'static> WireMessage for T {}

/// A request or reply of a `local_only` component.
#[diagnostic::on_unimplemented(
    message = "`{Self}` cannot be a local_only request or reply",
    note = "local_only requests and replies must be `Send + 'static`"
)]
pub trait LocalMessage: Send + 'static {}
impl<T: Send + 'static> LocalMessage for T {}

/// A field of a `ComponentError` variant, carried as error metadata.
#[diagnostic::on_unimplemented(
    message = "`{Self}` cannot be carried as error metadata",
    note = "ComponentError fields must implement Display and FromStr, or be an Option of such a type"
)]
pub trait MetadataValue: fmt::Display + FromStr {}
impl<T: fmt::Display + FromStr> MetadataValue for T {}

/// Compile-time check that `T` is a [`WireMessage`].
pub const fn assert_wire<T: WireMessage>() {}
/// Compile-time check that `T` is a [`LocalMessage`].
pub const fn assert_local<T: LocalMessage>() {}
/// Compile-time check that `T` is a [`ComponentError`](trait@ComponentError).
pub const fn assert_error<T: ComponentError>() {}
/// Compile-time check that `T` is a [`MetadataValue`].
pub const fn assert_metadata<T: MetadataValue>() {}

/// Byte-level dispatcher of one component, generated per component.
pub trait Dispatch: Send + Sync + 'static {
    /// Decode the request of method `method`, run it and encode the reply.
    fn dispatch(
        &self,
        method: usize,
        cx: CallContext,
        body: Bytes,
    ) -> BoxFuture<'static, Result<Bytes, AppError>>;
}

/// Decode `Req` (`INVALID_ARGUMENT` / `MALFORMED_REQUEST` on failure), run
/// `call`, map its error with [`ComponentError::into_app_error`] and encode
/// `Rep`.
pub fn serve<Req, Rep, E, F, Fut>(
    cx: CallContext,
    body: Bytes,
    call: F,
) -> BoxFuture<'static, Result<Bytes, AppError>>
where
    Req: WireMessage,
    Rep: WireMessage,
    E: ComponentError,
    F: FnOnce(CallContext, Req) -> Fut + Send + 'static,
    Fut: Future<Output = Result<Rep, E>> + Send + 'static,
{
    Box::pin(async move {
        let request = Req::decode(body).map_err(|_| {
            AppError::invalid_argument("the request could not be decoded")
                .with_reason(reasons::MALFORMED_REQUEST)
        })?;
        let reply = call(cx, request)
            .await
            .map_err(ComponentError::into_app_error)?;
        Ok(Bytes::from(reply.encode_to_vec()))
    })
}

/// `UNIMPLEMENTED` naming the component and the method index.
pub fn unknown_method(
    descriptor: &'static ComponentDescriptor,
    method: usize,
) -> BoxFuture<'static, Result<Bytes, AppError>> {
    Box::pin(std::future::ready(Err(AppError::unimplemented(format!(
        "component {} has no method with index {method}",
        descriptor.name()
    )))))
}

/// What a factory produced, assembled by generated code.
pub struct Local<D: ?Sized> {
    imp: Arc<D>,
    dispatch: Option<Arc<dyn Dispatch>>,
    lifecycle: Option<Arc<dyn LifecycleDyn>>,
}

impl<D: ?Sized + Send + Sync + 'static> Local<D> {
    /// The implementation, without a dispatcher or lifecycle hooks.
    pub fn new(imp: Arc<D>) -> Self {
        Self {
            imp,
            dispatch: None,
            lifecycle: None,
        }
    }

    /// The byte-level dispatcher that serves the `local-serialized` binding.
    #[must_use]
    pub fn with_dispatch(mut self, dispatch: Arc<dyn Dispatch>) -> Self {
        self.dispatch = Some(dispatch);
        self
    }

    /// Lifecycle hooks to run when the App starts and stops.
    #[must_use]
    pub fn with_lifecycle<L: Lifecycle>(mut self, lifecycle: Arc<L>) -> Self {
        self.lifecycle = Some(lifecycle);
        self
    }
}

/// Install a component that may run in this process. `factory` runs during
/// [`AppBuilder::build`] when the component is bound `local` or
/// `local-serialized`.
pub fn install_local<H, D, F>(
    app: &mut AppBuilder<'_>,
    make_handle: fn(Endpoint<D>) -> H,
    factory: F,
) -> Result<(), BuildError>
where
    H: ComponentHandle,
    D: ?Sized + Send + Sync + 'static,
    F: FnOnce(&mut Deps<'_>) -> Result<Local<D>, AppError> + Send + 'static,
{
    let descriptor = H::DESCRIPTOR;
    let build = erase(move |deps, link| {
        let local = factory(deps)?;
        let route = match link.binding() {
            Binding::LocalSerialized => match &local.dispatch {
                Some(dispatch) => Route::Serialized(Arc::clone(dispatch)),
                None => {
                    return Err(AppError::failed_precondition(format!(
                        "component {} has no serialized dispatcher and can only be bound local",
                        descriptor.name()
                    )));
                }
            },
            _ => Route::Local(local.imp),
        };
        Ok(Built {
            handle: Box::new(make_handle(Endpoint::new(link, route))),
            lifecycle: local.lifecycle,
            dispatch: local.dispatch,
        })
    });
    app.install::<H>(Some(build), remote_factory(make_handle))
}

/// Declare a `remote_only` component. It has no factory; it must be bound to
/// a remote transport (`grpc`).
pub fn install_remote<H, D>(
    app: &mut AppBuilder<'_>,
    make_handle: fn(Endpoint<D>) -> H,
) -> Result<(), BuildError>
where
    H: ComponentHandle,
    D: ?Sized + Send + Sync + 'static,
{
    app.install::<H>(None, remote_factory(make_handle))
}

/// The handle of a component whose calls go to another process.
fn remote_factory<H, D>(make_handle: fn(Endpoint<D>) -> H) -> RemoteFactory
where
    H: ComponentHandle,
    D: ?Sized + Send + Sync + 'static,
{
    Box::new(move |link| {
        Endpoint::remote(link).map(|endpoint| -> Handle { Box::new(make_handle(endpoint)) })
    })
}

/// Whether `error` carries `reason` and, when `domain` is `Some`, that
/// domain.
pub fn matches(error: &AppError, reason: &str, domain: Option<&str>) -> bool {
    error.reason() == Some(reason) && domain.is_none_or(|domain| error.domain() == Some(domain))
}

/// The metadata value `key` of `error`, parsed; `None` when absent or
/// unparseable.
pub fn field<T: FromStr>(error: &AppError, key: &str) -> Option<T> {
    error.metadata().get(key)?.parse().ok()
}

/// An optional metadata value: `Some(None)` when absent, `Some(Some(value))`
/// when present and parseable, `None` when present but unparseable.
pub fn optional_field<T: FromStr>(error: &AppError, key: &str) -> Option<Option<T>> {
    match error.metadata().get(key) {
        None => Some(None),
        Some(raw) => raw.parse().ok().map(Some),
    }
}

#[cfg(test)]
mod tests {
    use prost::Message;
    use sekvent_error::ErrorCode;

    use super::*;
    use crate::MethodDescriptor;

    #[derive(Clone, PartialEq, prost::Message)]
    struct Text {
        #[prost(string, tag = "1")]
        text: String,
    }

    const ECHO: &ComponentDescriptor =
        &ComponentDescriptor::new("echo", "Echo", &[MethodDescriptor::call("ping", "Ping")]);

    fn body(text: &str) -> Bytes {
        Bytes::from(
            Text {
                text: text.to_owned(),
            }
            .encode_to_vec(),
        )
    }

    async fn echo(_cx: CallContext, req: Text) -> Result<Text, AppError> {
        if req.text.is_empty() {
            Err(AppError::invalid_argument("empty").with_reason("EMPTY_TEXT"))
        } else {
            Ok(req)
        }
    }

    #[tokio::test]
    async fn serve_decodes_runs_and_encodes() {
        let reply = serve::<Text, Text, AppError, _, _>(CallContext::new(), body("hi"), echo)
            .await
            .unwrap();
        assert_eq!(Text::decode(reply).unwrap().text, "hi");

        let error = serve::<Text, Text, AppError, _, _>(CallContext::new(), body(""), echo)
            .await
            .unwrap_err();
        assert_eq!(error.reason(), Some("EMPTY_TEXT"));

        let garbage = serve::<Text, Text, AppError, _, _>(
            CallContext::new(),
            Bytes::from_static(&[0x0a, 0xff]),
            echo,
        )
        .await
        .unwrap_err();
        assert_eq!(garbage.code(), ErrorCode::InvalidArgument);
        assert_eq!(garbage.reason(), Some(reasons::MALFORMED_REQUEST));
    }

    #[tokio::test]
    async fn an_unknown_method_is_unimplemented() {
        let error = unknown_method(ECHO, 3).await.unwrap_err();
        assert_eq!(error.code(), ErrorCode::Unimplemented);
        assert!(error.message().contains("echo"), "{error}");
        assert!(error.message().contains('3'), "{error}");
    }

    #[test]
    fn assertions_are_callable() {
        const _: () = {
            assert_wire::<Text>();
            assert_local::<String>();
            assert_error::<AppError>();
            assert_metadata::<u32>();
        };
    }

    #[test]
    fn matching_by_reason_and_domain() {
        let error = AppError::not_found("x")
            .with_reason("GONE")
            .with_domain("shop.v1");
        assert!(matches(&error, "GONE", None));
        assert!(matches(&error, "GONE", Some("shop.v1")));
        assert!(!matches(&error, "GONE", Some("other.v1")));
        assert!(!matches(&error, "HERE", None));
        let bare = AppError::not_found("x").with_reason("GONE");
        assert!(!matches(&bare, "GONE", Some("shop.v1")));
    }

    #[test]
    fn fields_parse_or_fall_through() {
        let error = AppError::not_found("x")
            .with_metadata("count", "3")
            .with_metadata("bad", "three");
        assert_eq!(field::<u32>(&error, "count"), Some(3));
        assert_eq!(field::<u32>(&error, "bad"), None);
        assert_eq!(field::<u32>(&error, "absent"), None);
        assert_eq!(optional_field::<u32>(&error, "count"), Some(Some(3)));
        assert_eq!(optional_field::<u32>(&error, "absent"), Some(None));
        assert_eq!(optional_field::<u32>(&error, "bad"), None);
    }
}
