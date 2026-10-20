//! The RPC named after the method streams its requests.

#![allow(dead_code, non_upper_case_globals)]

use sekvent_component::{AppError, CallContext};

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
            false,
        )],
    );
    #[allow(non_camel_case_types, dead_code)]
    pub type __sekvent_rpc_Echo__Ping = (super::PingRequest, super::PingReply);
}

#[sekvent_component::component(name = "echo", package = "check.echo.v1", proto = "crate::proto")]
pub trait Echo {
    #[call]
    async fn ping(&self, cx: &CallContext, req: PingRequest) -> Result<PingReply, AppError>;
}

fn main() {}
