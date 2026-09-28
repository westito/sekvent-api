//! A remote_only component can be declared, but a C1 build without a remote
//! transport fails and names the binding key.

use sekvent_component::{
    App, AppError, BuildError, CallContext, ComponentHandle, ComponentMode, component,
};
use sekvent_config::MapSource;

#[derive(Clone, PartialEq, prost::Message)]
pub struct RecordRequest {
    #[prost(string, tag = "1")]
    pub entry: String,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct RecordReply {
    #[prost(uint64, tag = "1")]
    pub sequence: u64,
}

/// A ledger served by another process.
#[component(name = "ledger", package = "check.ledger.v1", remote_only)]
pub trait Ledger {
    /// Record an entry.
    #[call(idempotent)]
    async fn record(&self, cx: &CallContext, req: RecordRequest) -> Result<RecordReply, AppError>;
}

fn main() {
    let descriptor = <LedgerHandle as ComponentHandle>::DESCRIPTOR;
    assert_eq!(descriptor.mode(), ComponentMode::RemoteOnly);

    let source = MapSource::new();
    let mut builder = App::builder(&source);
    LedgerHandle::install_remote(&mut builder).expect("declare");
    match builder.build() {
        Err(BuildError::RemoteOnlyUnbound { component, key }) => {
            assert_eq!(component, "ledger");
            assert_eq!(key, "SEKVENT_COMPONENT_LEDGER_BINDING");
        }
        other => panic!("expected RemoteOnlyUnbound, got {other:?}"),
    }
}
