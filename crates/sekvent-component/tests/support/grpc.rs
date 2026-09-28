//! Loopback fixtures: an App serving the inventory over gRPC on
//! `127.0.0.1:0` under the sekvent runtime, and caller Apps bound to it.
#![allow(dead_code, missing_docs)]

use std::future::Future;
use std::net::SocketAddr;
use std::time::Duration;

use sekvent_component::{App, AppBuilder, BuildError};
use sekvent_config::MapSource;
use sekvent_runtime::{Runtime, RuntimeHandle, Server, Stage, UnitPolicy};

use super::fakes::build_with;
use super::inventory::InventoryHandle;

/// The canonical token the caller presents and the service accepts.
pub const TOKEN: &str = "shop-link-token-0123456789abcdefghijklmn";
/// Another canonical token, accepted nowhere.
pub const OTHER_TOKEN: &str = "other-link-token-0123456789abcdefghijklm";
/// The inventory's full gRPC service name.
pub const SERVICE: &str = "shop.inventory.v1.Inventory";

/// Guard a loopback test against hangs.
pub async fn guarded<T>(test: impl Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(30), test)
        .await
        .expect("the loopback test hung")
}

/// A started App serving the inventory over gRPC.
pub struct Service {
    pub app: App,
    pub runtime: RuntimeHandle,
    pub addr: SocketAddr,
}

impl Service {
    pub fn endpoint(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// Shut the runtime down and wait for it.
    pub async fn stop(self) {
        self.runtime.shutdown();
        self.runtime.wait().await.unwrap();
    }
}

/// The keys of a service that exposes the inventory to link `shop`.
pub fn service_keys() -> Vec<(String, String)> {
    vec![
        ("SEKVENT_COMPONENT_INVENTORY_SERVE".into(), "grpc".into()),
        ("SEKVENT_LINK_INBOUND_SHOP".into(), TOKEN.into()),
    ]
}

/// Build an App from `keys`, mount its routes on a runtime server bound to
/// `127.0.0.1:0` and start both.
pub async fn serve(
    keys: &[(String, String)],
    install: impl FnOnce(&mut AppBuilder<'_>) -> Result<(), BuildError>,
) -> Service {
    let config: MapSource = keys.iter().cloned().collect();
    let app = build_with(&config, install).unwrap();
    let server = Server::builder()
        .grpc_routes(app.grpc_routes())
        .bind(SocketAddr::from(([127, 0, 0, 1], 0)))
        .await
        .unwrap();
    let addr = server.local_addr();
    let runtime = app
        .register(
            Runtime::builder()
                .without_signals()
                .shutdown_delay(Duration::ZERO),
        )
        .unit(
            "grpc",
            Stage::Ingress,
            UnitPolicy::Critical,
            server.into_unit(),
        )
        .build()
        .unwrap()
        .start()
        .await
        .unwrap();
    Service { app, runtime, addr }
}

/// The keys of a caller binding the inventory to `endpoint` with the shop
/// token and 1 ms backoff, plus `extra`.
pub fn caller_keys(endpoint: &str, extra: &[(&str, &str)]) -> Vec<(String, String)> {
    let mut keys: Vec<(String, String)> = [
        ("SEKVENT_COMPONENT_INVENTORY_BINDING", "grpc"),
        ("SEKVENT_COMPONENT_INVENTORY_ENDPOINT", endpoint),
        ("SEKVENT_LINK_OUTBOUND_INVENTORY", TOKEN),
        ("SEKVENT_COMPONENT_INVENTORY_RETRY_INITIAL_BACKOFF", "1ms"),
        ("SEKVENT_COMPONENT_INVENTORY_RETRY_MAX_BACKOFF", "1ms"),
        ("SEKVENT_COMPONENT_INVENTORY_RETRY_JITTER", "none"),
    ]
    .into_iter()
    .map(|(key, value)| (key.to_owned(), value.to_owned()))
    .collect();
    for (key, value) in extra {
        keys.retain(|(existing, _)| existing != key);
        keys.push(((*key).to_owned(), (*value).to_owned()));
    }
    keys.retain(|(_, value)| !value.is_empty());
    keys
}

/// A caller App (not started) built from `keys`, with the inventory
/// installed through its regular `install` (whose factory must not run).
pub fn caller_app(keys: &[(String, String)]) -> Result<App, BuildError> {
    let config: MapSource = keys.iter().cloned().collect();
    build_with(&config, |builder| {
        InventoryHandle::install(builder, |_| -> Result<super::fakes::FakeInventory, _> {
            panic!("a grpc-bound component runs no factory")
        })
    })
}

/// A started caller App bound to `endpoint`, and its inventory handle.
pub async fn caller(endpoint: &str, extra: &[(&str, &str)]) -> (App, InventoryHandle) {
    let app = caller_app(&caller_keys(endpoint, extra)).unwrap();
    app.start().await.unwrap();
    let handle = app.handle::<InventoryHandle>().unwrap();
    (app, handle)
}
