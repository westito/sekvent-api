//! The serving side of gRPC: one generic service per exposed component,
//! routed by its service name, answering through the component's gate and
//! byte-level dispatcher.

use std::convert::Infallible;
use std::sync::Arc;
use std::task::{Context, Poll};

use bytes::Bytes;
use sekvent_context::headers;
use sekvent_error::AppError;
use sekvent_link::TokenMap;
use tonic::body::Body;
use tonic::server::Grpc;
use tonic::service::Routes;
use tracing::Instrument as _;

use super::codec::BytesCodec;
use crate::__private::{BoxFuture, Dispatch};
use crate::link::depth_exceeded;
use crate::server::{Server, tag};
use crate::{ComponentDescriptor, reasons};

/// One exposed component.
pub(crate) struct Served {
    server: Arc<Server>,
    dispatch: Arc<dyn Dispatch>,
    /// The accepted inbound tokens; `None` with `SERVE_AUTH=none`.
    inbound: Option<Arc<TokenMap>>,
    max_hops: u32,
}

impl std::fmt::Debug for Served {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Served")
            .field("component", &self.descriptor().name())
            .field("authenticated", &self.inbound.is_some())
            .field("max_hops", &self.max_hops)
            .finish_non_exhaustive()
    }
}

impl Served {
    pub(crate) fn new(
        server: Arc<Server>,
        dispatch: Arc<dyn Dispatch>,
        inbound: Option<Arc<TokenMap>>,
        max_hops: u32,
    ) -> Self {
        Self {
            server,
            dispatch,
            inbound,
            max_hops,
        }
    }

    pub(crate) fn descriptor(&self) -> &'static ComponentDescriptor {
        self.server.descriptor()
    }

    /// The method whose RPC name ends `path` (`/<service>/<Rpc>`).
    fn method_of(&self, path: &str) -> Option<usize> {
        let rpc = path.rsplit_once('/')?.1;
        self.descriptor()
            .methods()
            .iter()
            .position(|method| method.rpc() == rpc)
    }

    /// Steps 1 to 5 of serving one call: route, authenticate, build the
    /// context, run through the gate, answer.
    async fn handle(
        &self,
        method: Option<usize>,
        request: tonic::Request<Bytes>,
    ) -> Result<Bytes, AppError> {
        let descriptor = self.descriptor();
        let Some(method) = method else {
            let error = AppError::unimplemented(format!(
                "component {} has no such method",
                descriptor.name()
            ))
            .with_reason(reasons::UNKNOWN_METHOD)
            .with_metadata("component", descriptor.name());
            return Err(error);
        };
        let (metadata, _, body) = request.into_parts();
        let wire_headers = metadata.into_headers();
        let caller = match &self.inbound {
            Some(inbound) => Some(
                sekvent_link::authenticate_headers(inbound, &wire_headers)
                    .ok_or_else(|| AppError::unauthenticated(sekvent_link::REJECTED_MESSAGE))?,
            ),
            None => None,
        };
        let mut cx = headers::from_headers(&wire_headers, caller);
        if cx.hops() > self.max_hops {
            return Err(tag(descriptor, depth_exceeded(self.max_hops), method));
        }
        if let Some(deadline) = self
            .server
            .timeout(method)
            .and_then(|timeout| tokio::time::Instant::now().checked_add(timeout))
        {
            cx = cx.with_deadline(deadline.into_std());
        }
        // Cancels the call's token when the client resets the stream and
        // this future is dropped.
        let guard = cx.cancel_token().clone().drop_guard();
        let dispatch = Arc::clone(&self.dispatch);
        let outcome = self
            .server
            .run(method, cx, move |cx| dispatch.dispatch(method, cx, body))
            .await;
        drop(guard.disarm());
        match outcome {
            Ok(Ok(reply)) => Ok(reply),
            Ok(Err(error)) | Err(error) => Err(error),
        }
    }
}

/// The tower service mounted at `/<service>/{*rest}`.
#[derive(Debug, Clone)]
pub(crate) struct ComponentService(Arc<Served>);

impl<B> tower::Service<http::Request<B>> for ComponentService
where
    B: http_body::Body<Data = Bytes> + Send + 'static,
    B::Error: Into<Box<dyn std::error::Error + Send + Sync>> + Send,
{
    type Response = http::Response<Body>;
    type Error = Infallible;
    type Future = BoxFuture<'static, Result<Self::Response, Infallible>>;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Infallible>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, request: http::Request<B>) -> Self::Future {
        let served = Arc::clone(&self.0);
        let method = served.method_of(request.uri().path());
        let descriptor = served.descriptor();
        let span = tracing::debug_span!(
            "component_serve",
            component = descriptor.name(),
            method = method
                .and_then(|method| descriptor.methods().get(method))
                .map_or("?", |method| method.name()),
        );
        Box::pin(
            async move {
                let mut grpc = Grpc::new(BytesCodec);
                Ok(grpc.unary(Unary { served, method }, request).await)
            }
            .instrument(span),
        )
    }
}

/// One call, as tonic's unary server sees it.
struct Unary {
    served: Arc<Served>,
    method: Option<usize>,
}

impl tower::Service<tonic::Request<Bytes>> for Unary {
    type Response = tonic::Response<Bytes>;
    type Error = tonic::Status;
    type Future = BoxFuture<'static, Result<tonic::Response<Bytes>, tonic::Status>>;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), tonic::Status>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, request: tonic::Request<Bytes>) -> Self::Future {
        let served = Arc::clone(&self.served);
        let method = self.method;
        Box::pin(async move {
            served
                .handle(method, request)
                .await
                .map(tonic::Response::new)
                .map_err(|error| sekvent_error::grpc::to_status(&error))
        })
    }
}

/// tonic routes serving every component in `served`; unknown paths get
/// tonic's `UNIMPLEMENTED`.
pub(crate) fn routes<'a>(served: impl IntoIterator<Item = &'a Arc<Served>>) -> Routes {
    let mut router = Routes::default().into_axum_router();
    for served in served {
        let path = format!("/{}/{{*rest}}", super::service_name(served.descriptor()));
        router = router.route_service(&path, ComponentService(Arc::clone(served)));
    }
    Routes::from(router)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MethodDescriptor;
    use crate::server::MethodPolicy;

    const METHODS: &[MethodDescriptor] = &[
        MethodDescriptor::call("reserve", "Reserve"),
        MethodDescriptor::call("release", "Release"),
    ];
    const INVENTORY: &ComponentDescriptor =
        &ComponentDescriptor::new("inventory", "Inventory", METHODS).with_package("shop.v1");

    struct Echo;

    impl Dispatch for Echo {
        fn dispatch(
            &self,
            _method: usize,
            _cx: sekvent_context::CallContext,
            body: Bytes,
        ) -> BoxFuture<'static, Result<Bytes, AppError>> {
            Box::pin(async move { Ok(body) })
        }
    }

    fn served() -> Served {
        let server = Arc::new(Server::new(
            INVENTORY,
            &[MethodPolicy::default(), MethodPolicy::default()],
            false,
        ));
        Served::new(server, Arc::new(Echo), None, 16)
    }

    #[test]
    fn methods_are_found_by_rpc_name() {
        let served = served();
        assert_eq!(served.method_of("/shop.v1.Inventory/Reserve"), Some(0));
        assert_eq!(served.method_of("/shop.v1.Inventory/Release"), Some(1));
        assert_eq!(served.method_of("/shop.v1.Inventory/Stock"), None);
        assert_eq!(served.method_of("no-slash"), None);
        let debug = format!("{served:?}");
        assert!(debug.contains("inventory"), "{debug}");
    }
}
