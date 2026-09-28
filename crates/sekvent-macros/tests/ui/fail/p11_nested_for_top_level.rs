//! The RPC returns the top-level `check.echo.v1.Line`; the method returns
//! the nested `check.echo.v1.Order.Line`, which prost names the same.

#![allow(dead_code, non_upper_case_globals)]

/// The top-level `check.echo.v1.Line`.
#[derive(Clone, PartialEq, prost::Message)]
pub struct Line {
    #[prost(string, tag = "1")]
    pub text: String,
}

impl prost::Name for Line {
    const NAME: &'static str = "Line";
    const PACKAGE: &'static str = "check.echo.v1";
}

/// Messages nested in `check.echo.v1.Order`, as prost generates them.
pub mod order {
    #[derive(Clone, PartialEq, prost::Message)]
    pub struct Line {
        #[prost(string, tag = "1")]
        pub text: String,
    }

    impl prost::Name for Line {
        const NAME: &'static str = "Line";
        const PACKAGE: &'static str = "check.echo.v1";
    }
}

mod proto {
    pub const __sekvent_service_Echo: (&str, &[(&str, &str, &str, bool)]) = (
        "check.echo.v1.Echo",
        &[("Ping", "check.echo.v1.Line", "check.echo.v1.Line", false)],
    );
    #[allow(non_camel_case_types, dead_code)]
    pub type __sekvent_rpc_Echo__Ping = (super::Line, super::Line);
}

#[sekvent_component::component(name = "echo", package = "check.echo.v1", proto = "crate::proto")]
pub trait Echo {
    #[call]
    async fn ping(
        &self,
        cx: &sekvent_component::CallContext,
        req: Line,
    ) -> Result<order::Line, sekvent_component::AppError>;
}

fn main() {}
