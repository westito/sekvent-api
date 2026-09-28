//! A remote_only component checks its contract and can be declared; left
//! unbound, the build fails and names the binding key.

use sekvent_component::{
    App, AppError, BuildError, CallContext, ComponentHandle, ComponentMode, component,
};
use sekvent_config::MapSource;

#[derive(Clone, PartialEq, prost::Message)]
pub struct RecordRequest {
    #[prost(string, tag = "1")]
    pub entry: String,
}

impl prost::Name for RecordRequest {
    const NAME: &'static str = "RecordRequest";
    const PACKAGE: &'static str = "check.ledger.v1";
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct RecordReply {
    #[prost(uint64, tag = "1")]
    pub sequence: u64,
}

impl prost::Name for RecordReply {
    const NAME: &'static str = "RecordReply";
    const PACKAGE: &'static str = "check.ledger.v1";
}

/// The contract sekvent-proto-build would generate for `check.ledger.v1`.
pub mod ledger_proto {
    #[allow(non_upper_case_globals)]
    pub const __sekvent_service_Ledger: (&str, &[(&str, &str, &str, bool)]) = (
        "check.ledger.v1.Ledger",
        &[(
            "Record",
            "check.ledger.v1.RecordRequest",
            "check.ledger.v1.RecordReply",
            false,
        )],
    );
    #[allow(non_camel_case_types, dead_code)]
    pub type __sekvent_rpc_Ledger__Record = (super::RecordRequest, super::RecordReply);
}

/// A ledger served by another process.
#[component(
    name = "ledger",
    package = "check.ledger.v1",
    proto = "self::ledger_proto",
    remote_only
)]
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
