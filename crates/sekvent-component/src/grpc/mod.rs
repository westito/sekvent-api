//! The `grpc` binding and gRPC serving: one generic byte-level client and
//! one generic byte-level service carry every component.
//!
//! Serving reads everything that decides whether a call runs from the
//! request headers, before the body: the link token, the exact
//! `/<service>/<Rpc>` path, the call context, the hop limit and the
//! deadline (the caller's, narrowed by the method's timeout). Request
//! trailers never reach the context. A call still running at its deadline
//! is answered `DEADLINE_EXCEEDED`, and a client that resets the stream
//! cancels the call's token.
//!
//! What a served method's own failure becomes on the wire:
//!
//! - A panic is `INTERNAL` with reason
//!   [`HANDLER_PANICKED`](crate::reasons::HANDLER_PANICKED); the payload is
//!   never shown, and the caller does not retry it.
//! - A transient error that came from the method's own call to another
//!   component (marked with metadata `downstream`, the callee's name) is
//!   `INTERNAL` with reason
//!   [`DOWNSTREAM_FAILURE`](crate::reasons::DOWNSTREAM_FAILURE), keeping
//!   `downstream`, the original code as `downstream_code` and the original
//!   reason as `downstream_reason`. The caller neither retries it nor counts
//!   it against the component in between, which is healthy.
//! - Any other error crosses unchanged. Errors of the serving side itself
//!   (not started, draining, bulkhead full, deadline) keep their codes.

pub(crate) mod client;
pub(crate) mod codec;
pub(crate) mod endpoint;
pub(crate) mod service;

use std::collections::HashMap;
use std::sync::{Arc, OnceLock};

use tonic::transport::{Channel, Endpoint};

use crate::ComponentDescriptor;

/// The gRPC service name of a component: `<package>.<Trait>`, or the bare
/// trait name without a package.
pub(crate) fn service_name(descriptor: &ComponentDescriptor) -> String {
    descriptor
        .full_service_name()
        .unwrap_or_else(|| descriptor.service().to_owned())
}

/// A channel created on first use: [`Endpoint::connect_lazy`] needs a tokio
/// runtime, building the App does not.
#[derive(Debug)]
pub(crate) struct LazyChannel {
    endpoint: Endpoint,
    channel: OnceLock<Channel>,
}

impl LazyChannel {
    pub(crate) fn new(endpoint: Endpoint) -> Self {
        Self {
            endpoint,
            channel: OnceLock::new(),
        }
    }

    /// The channel, connecting lazily on the first call.
    pub(crate) fn get(&self) -> Channel {
        self.channel
            .get_or_init(|| self.endpoint.connect_lazy())
            .clone()
    }
}

/// One channel per endpoint URL, shared by the components behind it.
#[derive(Debug, Default)]
pub(crate) struct ChannelCache {
    channels: HashMap<String, Arc<LazyChannel>>,
}

impl ChannelCache {
    /// The channel for `url` (already validated), created on first request.
    pub(crate) fn channel(
        &mut self,
        url: &str,
    ) -> Result<Arc<LazyChannel>, tonic::transport::Error> {
        if let Some(channel) = self.channels.get(url) {
            return Ok(Arc::clone(channel));
        }
        let channel = Arc::new(LazyChannel::new(endpoint::endpoint(url)?));
        self.channels.insert(url.to_owned(), Arc::clone(&channel));
        Ok(channel)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MethodDescriptor;

    #[test]
    fn service_names() {
        const METHODS: &[MethodDescriptor] = &[MethodDescriptor::call("reserve", "Reserve")];
        let bare = ComponentDescriptor::new("inventory", "Inventory", METHODS);
        assert_eq!(service_name(&bare), "Inventory");
        let packaged = bare.with_package("shop.inventory.v1");
        assert_eq!(service_name(&packaged), "shop.inventory.v1.Inventory");
    }

    #[test]
    fn components_on_one_endpoint_share_a_channel() {
        let mut cache = ChannelCache::default();
        let first = cache.channel("http://127.0.0.1:50051").unwrap();
        let second = cache.channel("http://127.0.0.1:50051").unwrap();
        let other = cache.channel("http://127.0.0.1:50052").unwrap();
        assert!(Arc::ptr_eq(&first, &second));
        assert!(!Arc::ptr_eq(&first, &other));
    }

    #[tokio::test]
    async fn the_channel_is_created_once() {
        let lazy = LazyChannel::new(endpoint::endpoint("http://127.0.0.1:1").unwrap());
        assert!(lazy.channel.get().is_none());
        drop(lazy.get());
        drop(lazy.get());
        assert!(lazy.channel.get().is_some());
        assert!(format!("{lazy:?}").contains("LazyChannel"));
    }
}
