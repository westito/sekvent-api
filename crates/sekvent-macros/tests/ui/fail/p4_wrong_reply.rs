//! The method's reply is not the RPC's output type.

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

#[derive(Clone, PartialEq, prost::Message)]
pub struct PingReply {
    #[prost(string, tag = "1")]
    pub text: String,
}

impl prost::Name for PingReply {
    const NAME: &'static str = "PingReply";
    const PACKAGE: &'static str = "check.echo.v1";
}

/// The RPC's actual output type: same leaf name, another package.
mod other {
    #[derive(Clone, PartialEq, prost::Message)]
    pub struct PingReply {
        #[prost(string, tag = "1")]
        pub text: String,
    }

    impl prost::Name for PingReply {
        const NAME: &'static str = "PingReply";
        const PACKAGE: &'static str = "other.v1";
    }
}

mod proto {
    pub const __sekvent_service_Echo: (&str, &[(&str, &str, &str, bool)]) = (
        "check.echo.v1.Echo",
        &[(
            "Ping",
            "check.echo.v1.PingRequest",
            "other.v1.PingReply",
            false,
        )],
    );
    #[allow(non_camel_case_types, dead_code)]
    pub type __sekvent_rpc_Echo__Ping = (super::PingRequest, super::other::PingReply);
}

#[sekvent_component::component(name = "echo", package = "check.echo.v1", proto = "crate::proto")]
pub trait Echo {
    #[call]
    async fn ping(
        &self,
        cx: &sekvent_component::CallContext,
        req: PingRequest,
    ) -> Result<PingReply, sekvent_component::AppError>;
}

fn main() {}
