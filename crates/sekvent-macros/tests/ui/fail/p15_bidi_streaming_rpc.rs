//! A `#[call(stream)]` method whose RPC streams requests and replies.

#![allow(dead_code, non_upper_case_globals)]

use sekvent_component::{AppError, CallContext, CallStream};

#[derive(Clone, PartialEq, prost::Message)]
pub struct PingRequest {
    #[prost(string, tag = "1")]
    pub text: String,
}

impl prost::Name for PingRequest {
    const NAME: &'static str = "PingRequest";
    const PACKAGE: &'static str = "check.echo.v1";
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct PingReply {
    #[prost(string, tag = "1")]
    pub text: String,
}

impl prost::Name for PingReply {
    const NAME: &'static str = "PingReply";
    const PACKAGE: &'static str = "check.echo.v1";
}

mod proto {
    pub const __sekvent_service_Echo: (&str, &[(&str, &str, &str, bool, bool)]) = (
        "check.echo.v1.Echo",
        &[(
            "Ping",
            "check.echo.v1.PingRequest",
            "check.echo.v1.PingReply",
            true,
            true,
        )],
    );
    #[allow(non_camel_case_types, dead_code)]
    pub type __sekvent_rpc_Echo__Ping = (super::PingRequest, super::PingReply);
}

#[sekvent_component::component(name = "echo", package = "check.echo.v1", proto = "crate::proto")]
pub trait Echo {
    #[call(stream)]
    async fn ping(
        &self,
        cx: &CallContext,
        req: PingRequest,
    ) -> Result<CallStream<PingReply, AppError>, AppError>;
}

fn main() {}
