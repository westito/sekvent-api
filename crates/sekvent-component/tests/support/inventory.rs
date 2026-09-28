//! The reference component of the C1 spec (section 3.4), expanded by hand
//! exactly as `#[component]` expands it (with the C2 contract check of C2
//! section 5.2), with hand-written prost messages, a hand-written service
//! contract and a hand-expanded `#[derive(ComponentError)]` (C1 section 3.7),
//! so the framework is tested without the macro.
#![allow(dead_code, missing_docs)]

use sekvent_component::{AppError, CallContext};

// ---------------------------------------------------------------------------
// Messages (what `sekvent-proto-build` would generate for shop.inventory.v1).
// ---------------------------------------------------------------------------

/// Reserve stock for an order.
#[derive(Clone, PartialEq, prost::Message)]
pub struct ReserveRequest {
    #[prost(string, tag = "1")]
    pub order_id: String,
    #[prost(string, tag = "2")]
    pub sku: String,
    #[prost(uint32, tag = "3")]
    pub quantity: u32,
}

/// A reservation.
#[derive(Clone, PartialEq, prost::Message)]
pub struct ReserveReply {
    #[prost(string, tag = "1")]
    pub reservation_id: String,
    #[prost(uint32, tag = "2")]
    pub remaining: u32,
}

/// Release a reservation.
#[derive(Clone, PartialEq, prost::Message)]
pub struct ReleaseRequest {
    #[prost(string, tag = "1")]
    pub reservation_id: String,
}

/// Whether a reservation was released.
#[derive(Clone, PartialEq, prost::Message)]
pub struct ReleaseReply {
    #[prost(bool, tag = "1")]
    pub released: bool,
}

impl prost::Name for ReserveRequest {
    const NAME: &'static str = "ReserveRequest";
    const PACKAGE: &'static str = "shop.inventory.v1";
}

impl prost::Name for ReserveReply {
    const NAME: &'static str = "ReserveReply";
    const PACKAGE: &'static str = "shop.inventory.v1";
}

impl prost::Name for ReleaseRequest {
    const NAME: &'static str = "ReleaseRequest";
    const PACKAGE: &'static str = "shop.inventory.v1";
}

impl prost::Name for ReleaseReply {
    const NAME: &'static str = "ReleaseReply";
    const PACKAGE: &'static str = "shop.inventory.v1";
}

/// What `sekvent-proto-build` would generate for the package's service.
pub mod proto {
    /// Contract of `shop.inventory.v1.Inventory`, checked by `#[component(proto = …)]`.
    #[doc(hidden)]
    #[allow(non_upper_case_globals, dead_code)]
    pub const __sekvent_service_Inventory: (&str, &[(&str, &str, &str, bool)]) = (
        "shop.inventory.v1.Inventory",
        &[
            (
                "Reserve",
                "shop.inventory.v1.ReserveRequest",
                "shop.inventory.v1.ReserveReply",
                false,
            ),
            (
                "Release",
                "shop.inventory.v1.ReleaseRequest",
                "shop.inventory.v1.ReleaseReply",
                false,
            ),
        ],
    );
}

// ---------------------------------------------------------------------------
// #[derive(Debug, ComponentError)]
// #[component_error(domain = "shop.inventory.v1")]
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub enum InventoryError {
    OutOfStock {
        sku: String,
        available: u32,
    },
    ReservationNotFound {
        reservation_id: String,
        hint: Option<String>,
    },
    Closed,
    Other(AppError),
}

const _: () = {
    ::sekvent_component::__private::assert_metadata::<String>();
    ::sekvent_component::__private::assert_metadata::<u32>();
    ::sekvent_component::__private::assert_metadata::<String>();
    ::sekvent_component::__private::assert_metadata::<String>();
};

#[automatically_derived]
#[allow(clippy::all, clippy::pedantic)]
impl ::sekvent_component::ComponentError for InventoryError {
    fn into_app_error(self) -> ::sekvent_component::AppError {
        match self {
            Self::OutOfStock { sku, available } => ::sekvent_component::AppError::new(
                ::sekvent_component::ErrorCode::FailedPrecondition,
                ::std::format!("only {available} of {sku} left"),
            )
            .with_reason("OUT_OF_STOCK")
            .with_domain("shop.inventory.v1")
            .with_metadata("sku", ::std::string::ToString::to_string(&sku))
            .with_metadata("available", ::std::string::ToString::to_string(&available)),
            Self::ReservationNotFound {
                reservation_id,
                hint,
            } => {
                let error = ::sekvent_component::AppError::new(
                    ::sekvent_component::ErrorCode::NotFound,
                    ::std::string::String::from("reservation not found"),
                )
                .with_reason("RESERVATION_NOT_FOUND")
                .with_domain("shop.inventory.v1")
                .with_metadata(
                    "reservation_id",
                    ::std::string::ToString::to_string(&reservation_id),
                );
                match hint {
                    ::core::option::Option::Some(value) => {
                        error.with_metadata("hint", ::std::string::ToString::to_string(&value))
                    }
                    ::core::option::Option::None => error,
                }
            }
            Self::Closed => ::sekvent_component::AppError::new(
                ::sekvent_component::ErrorCode::Unavailable,
                ::std::string::String::from("inventory closed"),
            )
            .with_reason("INVENTORY_CLOSED")
            .with_domain("shop.inventory.v1"),
            Self::Other(error) => error,
        }
    }

    fn from_app_error(error: ::sekvent_component::AppError) -> Self {
        if ::sekvent_component::__private::matches(
            &error,
            "OUT_OF_STOCK",
            ::core::option::Option::Some("shop.inventory.v1"),
        ) {
            match (
                ::sekvent_component::__private::field::<String>(&error, "sku"),
                ::sekvent_component::__private::field::<u32>(&error, "available"),
            ) {
                (::core::option::Option::Some(sku), ::core::option::Option::Some(available)) => {
                    return Self::OutOfStock { sku, available };
                }
                _ => {}
            }
        }
        if ::sekvent_component::__private::matches(
            &error,
            "RESERVATION_NOT_FOUND",
            ::core::option::Option::Some("shop.inventory.v1"),
        ) {
            match (
                ::sekvent_component::__private::field::<String>(&error, "reservation_id"),
                ::sekvent_component::__private::optional_field::<String>(&error, "hint"),
            ) {
                (
                    ::core::option::Option::Some(reservation_id),
                    ::core::option::Option::Some(hint),
                ) => {
                    return Self::ReservationNotFound {
                        reservation_id,
                        hint,
                    };
                }
                _ => {}
            }
        }
        if ::sekvent_component::__private::matches(
            &error,
            "INVENTORY_CLOSED",
            ::core::option::Option::Some("shop.inventory.v1"),
        ) {
            return Self::Closed;
        }
        Self::Other(error)
    }
}

#[automatically_derived]
impl ::core::convert::From<::sekvent_component::AppError> for InventoryError {
    fn from(error: ::sekvent_component::AppError) -> Self {
        <Self as ::sekvent_component::ComponentError>::from_app_error(error)
    }
}

#[automatically_derived]
impl ::core::convert::From<InventoryError> for ::sekvent_component::AppError {
    fn from(error: InventoryError) -> Self {
        <InventoryError as ::sekvent_component::ComponentError>::into_app_error(error)
    }
}

// ---------------------------------------------------------------------------
// #[sekvent::component(
//     name = "inventory",
//     package = "shop.inventory.v1",
//     proto = "crate::support::inventory::proto"
// )]
// pub trait Inventory: Send + Sync + 'static { ... }
// ---------------------------------------------------------------------------

pub trait Inventory: Send + Sync + 'static {
    /// Reserve stock for an order.
    fn reserve(
        &self,
        cx: &CallContext,
        req: ReserveRequest,
    ) -> impl ::core::future::Future<Output = Result<ReserveReply, InventoryError>> + ::core::marker::Send;
    /// Release a reservation.
    fn release(
        &self,
        cx: &CallContext,
        req: ReleaseRequest,
    ) -> impl ::core::future::Future<Output = Result<ReleaseReply, InventoryError>> + ::core::marker::Send;
}

#[doc(hidden)]
pub trait __InventoryDyn: ::core::marker::Send + ::core::marker::Sync + 'static {
    fn __reserve<'a>(
        &'a self,
        cx: &'a CallContext,
        req: ReserveRequest,
    ) -> ::sekvent_component::__private::BoxFuture<'a, Result<ReserveReply, InventoryError>>;
    fn __release<'a>(
        &'a self,
        cx: &'a CallContext,
        req: ReleaseRequest,
    ) -> ::sekvent_component::__private::BoxFuture<'a, Result<ReleaseReply, InventoryError>>;
}

#[allow(clippy::all, clippy::pedantic)]
impl<T: Inventory> __InventoryDyn for T {
    fn __reserve<'a>(
        &'a self,
        cx: &'a CallContext,
        req: ReserveRequest,
    ) -> ::sekvent_component::__private::BoxFuture<'a, Result<ReserveReply, InventoryError>> {
        ::std::boxed::Box::pin(<T as Inventory>::reserve(self, cx, req))
    }
    fn __release<'a>(
        &'a self,
        cx: &'a CallContext,
        req: ReleaseRequest,
    ) -> ::sekvent_component::__private::BoxFuture<'a, Result<ReleaseReply, InventoryError>> {
        ::std::boxed::Box::pin(<T as Inventory>::release(self, cx, req))
    }
}

/// Cloneable handle to the [`Inventory`] component (`inventory`).
///
/// Get it from `Deps::handle` in a factory or from `App::handle`; the App
/// builder decides whether its calls run in-process or across a
/// serialization boundary.
#[derive(Clone, Debug)]
pub struct InventoryHandle(::sekvent_component::__private::Endpoint<dyn __InventoryDyn>);

#[allow(clippy::all, clippy::pedantic)]
impl InventoryHandle {
    #[doc(hidden)]
    pub const __METHODS: &'static [::sekvent_component::MethodDescriptor] = &[
        ::sekvent_component::MethodDescriptor::call("reserve", "Reserve")
            .with_idempotent()
            .with_timeout(::core::time::Duration::new(2u64, 0u32))
            .with_bulkhead(16u32),
        ::sekvent_component::MethodDescriptor::call("release", "Release")
            .with_timeout(::core::time::Duration::new(0u64, 500000000u32)),
    ];

    /// Reserve stock for an order.
    pub fn reserve<'a>(
        &'a self,
        cx: &'a CallContext,
        req: ReserveRequest,
    ) -> impl ::core::future::Future<Output = Result<ReserveReply, InventoryError>>
    + ::core::marker::Send
    + 'a {
        self.0.call(
            0usize,
            cx,
            req,
            |imp: ::std::sync::Arc<dyn __InventoryDyn>,
             cx: ::sekvent_component::CallContext,
             req: ReserveRequest| async move {
                __InventoryDyn::__reserve(&*imp, &cx, req).await
            },
        )
    }

    /// Release a reservation.
    pub fn release<'a>(
        &'a self,
        cx: &'a CallContext,
        req: ReleaseRequest,
    ) -> impl ::core::future::Future<Output = Result<ReleaseReply, InventoryError>>
    + ::core::marker::Send
    + 'a {
        self.0.call(
            1usize,
            cx,
            req,
            |imp: ::std::sync::Arc<dyn __InventoryDyn>,
             cx: ::sekvent_component::CallContext,
             req: ReleaseRequest| async move {
                __InventoryDyn::__release(&*imp, &cx, req).await
            },
        )
    }

    /// The binding this handle's calls use.
    pub fn binding(&self) -> ::sekvent_component::Binding {
        self.0.binding()
    }

    /// Install the implementation `factory` builds. The factory runs during
    /// `AppBuilder::build`, in install order, and only when the component is
    /// bound `local` or `local-serialized`.
    pub fn install<T, F>(
        app: &mut ::sekvent_component::AppBuilder<'_>,
        factory: F,
    ) -> ::core::result::Result<(), ::sekvent_component::BuildError>
    where
        T: Inventory,
        F: ::core::ops::FnOnce(
                &mut ::sekvent_component::Deps<'_>,
            )
                -> ::core::result::Result<T, ::sekvent_component::AppError>
            + ::core::marker::Send
            + 'static,
    {
        ::sekvent_component::__private::install_local(app, InventoryHandle, move |deps| {
            let imp: ::std::sync::Arc<dyn __InventoryDyn> = ::std::sync::Arc::new(factory(deps)?);
            let dispatch =
                ::std::sync::Arc::new(__InventoryDispatcher(::std::sync::Arc::clone(&imp)));
            ::core::result::Result::Ok(
                ::sekvent_component::__private::Local::new(imp).with_dispatch(dispatch),
            )
        })
    }

    /// Like [`InventoryHandle::install`], and also run the implementation's
    /// `Lifecycle` hooks when the App starts and stops.
    pub fn install_with_lifecycle<T, F>(
        app: &mut ::sekvent_component::AppBuilder<'_>,
        factory: F,
    ) -> ::core::result::Result<(), ::sekvent_component::BuildError>
    where
        T: Inventory + ::sekvent_component::Lifecycle,
        F: ::core::ops::FnOnce(
                &mut ::sekvent_component::Deps<'_>,
            )
                -> ::core::result::Result<T, ::sekvent_component::AppError>
            + ::core::marker::Send
            + 'static,
    {
        ::sekvent_component::__private::install_local(app, InventoryHandle, move |deps| {
            let concrete = ::std::sync::Arc::new(factory(deps)?);
            let imp: ::std::sync::Arc<dyn __InventoryDyn> = ::std::sync::Arc::<T>::clone(&concrete);
            let dispatch =
                ::std::sync::Arc::new(__InventoryDispatcher(::std::sync::Arc::clone(&imp)));
            ::core::result::Result::Ok(
                ::sekvent_component::__private::Local::new(imp)
                    .with_dispatch(dispatch)
                    .with_lifecycle(concrete),
            )
        })
    }
}

#[allow(clippy::all, clippy::pedantic)]
impl ::sekvent_component::ComponentHandle for InventoryHandle {
    const DESCRIPTOR: &'static ::sekvent_component::ComponentDescriptor =
        &::sekvent_component::ComponentDescriptor::new(
            "inventory",
            "Inventory",
            InventoryHandle::__METHODS,
        )
        .with_package("shop.inventory.v1");
}

#[doc(hidden)]
pub struct __InventoryDispatcher(::std::sync::Arc<dyn __InventoryDyn>);

#[allow(clippy::all, clippy::pedantic)]
impl ::sekvent_component::__private::Dispatch for __InventoryDispatcher {
    fn dispatch(
        &self,
        method: usize,
        cx: ::sekvent_component::CallContext,
        body: ::sekvent_component::__private::Bytes,
    ) -> ::sekvent_component::__private::BoxFuture<
        'static,
        ::core::result::Result<
            ::sekvent_component::__private::Bytes,
            ::sekvent_component::AppError,
        >,
    > {
        let imp = ::std::sync::Arc::clone(&self.0);
        match method {
            0usize => ::sekvent_component::__private::serve(
                cx,
                body,
                move |cx: ::sekvent_component::CallContext, req: ReserveRequest| async move {
                    __InventoryDyn::__reserve(&*imp, &cx, req).await
                },
            ),
            1usize => ::sekvent_component::__private::serve(
                cx,
                body,
                move |cx: ::sekvent_component::CallContext, req: ReleaseRequest| async move {
                    __InventoryDyn::__release(&*imp, &cx, req).await
                },
            ),
            _ => ::sekvent_component::__private::unknown_method(
                <InventoryHandle as ::sekvent_component::ComponentHandle>::DESCRIPTOR,
                method,
            ),
        }
    }
}

const _: () = {
    ::sekvent_component::__private::assert_wire::<ReserveRequest>();
    ::sekvent_component::__private::assert_wire::<ReserveReply>();
    ::sekvent_component::__private::assert_error::<InventoryError>();
    ::sekvent_component::__private::assert_wire::<ReleaseRequest>();
    ::sekvent_component::__private::assert_wire::<ReleaseReply>();
    ::sekvent_component::__private::assert_error::<InventoryError>();
};

const _: () = ::sekvent_component::__private::assert_service(
    crate::support::inventory::proto::__sekvent_service_Inventory,
    "shop.inventory.v1.Inventory",
    2usize,
);
const _: () = ::sekvent_component::__private::assert_rpc::<ReserveRequest, ReserveReply>(
    crate::support::inventory::proto::__sekvent_service_Inventory,
    "Reserve",
);
const _: () = ::sekvent_component::__private::assert_rpc::<ReleaseRequest, ReleaseReply>(
    crate::support::inventory::proto::__sekvent_service_Inventory,
    "Release",
);

// ---------------------------------------------------------------------------
// Not part of the expansion: lets tests pair the handle with another
// dispatcher (the handle's constructor is private to this module, as it is
// in generated code).
// ---------------------------------------------------------------------------

pub fn install_with_dispatch<T: Inventory>(
    app: &mut ::sekvent_component::AppBuilder<'_>,
    imp: T,
    dispatch: ::std::sync::Arc<dyn ::sekvent_component::__private::Dispatch>,
) -> ::core::result::Result<(), ::sekvent_component::BuildError> {
    ::sekvent_component::__private::install_local(app, InventoryHandle, move |_deps| {
        let imp: ::std::sync::Arc<dyn __InventoryDyn> = ::std::sync::Arc::new(imp);
        ::core::result::Result::Ok(
            ::sekvent_component::__private::Local::new(imp).with_dispatch(dispatch),
        )
    })
}

/// The generated dispatcher of an implementation, for byte-level tests.
pub fn dispatcher<T: Inventory>(imp: T) -> __InventoryDispatcher {
    __InventoryDispatcher(::std::sync::Arc::new(imp))
}

/// Declare the component as `install_remote` does for a `remote_only`
/// component: no factory, bound to a remote transport.
pub fn install_remote(
    app: &mut ::sekvent_component::AppBuilder<'_>,
) -> ::core::result::Result<(), ::sekvent_component::BuildError> {
    ::sekvent_component::__private::install_remote(app, InventoryHandle)
}
