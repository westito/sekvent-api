//! A method returns `()` (`google.protobuf.Empty`) where the RPC returns a
//! message.

#![allow(dead_code, non_upper_case_globals)]

#[derive(Clone, PartialEq, prost::Message)]
pub struct PingRequest {
    #[prost(string, tag = "1")]
    pub text: String,
}

impl prost::Name for PingRequest {
    const NAME: &'static str = "PingRequest";
    const PACKAGE: &'static str = "check.echo.v1";
}

mod proto {
    pub const __sekvent_service_Echo: (&str, &[(&str, &str, &str, bool)]) = (
        "check.echo.v1.Echo",
        &[(
            "Ping",
            "check.echo.v1.PingRequest",
            "check.echo.v1.PingRequest",
            false,
        )],
    );
    #[allow(non_camel_case_types, dead_code)]
    pub type __sekvent_rpc_Echo__Ping = (super::PingRequest, super::PingRequest);
}

#[sekvent_component::component(name = "echo", package = "check.echo.v1", proto = "crate::proto")]
pub trait Echo {
    #[call]
    async fn ping(
        &self,
        cx: &sekvent_component::CallContext,
        req: PingRequest,
    ) -> Result<(), sekvent_component::AppError>;
}

fn main() {}
