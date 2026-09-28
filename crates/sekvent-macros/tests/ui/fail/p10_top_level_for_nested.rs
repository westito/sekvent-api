//! The RPC takes the nested message `check.echo.v1.Outer.Inner`; the method
//! takes the top-level `check.echo.v1.Inner`. prost gives both the same
//! `Name::NAME` and `Name::PACKAGE`, so only the types tell them apart.

#![allow(dead_code, non_upper_case_globals)]

/// The top-level `check.echo.v1.Inner`.
#[derive(Clone, PartialEq, prost::Message)]
pub struct Inner {
    #[prost(string, tag = "1")]
    pub text: String,
}

impl prost::Name for Inner {
    const NAME: &'static str = "Inner";
    const PACKAGE: &'static str = "check.echo.v1";
}

/// Messages nested in `check.echo.v1.Outer`, as prost generates them.
pub mod outer {
    #[derive(Clone, PartialEq, prost::Message)]
    pub struct Inner {
        #[prost(string, tag = "1")]
        pub text: String,
    }

    impl prost::Name for Inner {
        const NAME: &'static str = "Inner";
        const PACKAGE: &'static str = "check.echo.v1";
    }
}

mod proto {
    pub const __sekvent_service_Echo: (&str, &[(&str, &str, &str, bool)]) = (
        "check.echo.v1.Echo",
        &[(
            "Ping",
            "check.echo.v1.Outer.Inner",
            "check.echo.v1.Inner",
            false,
        )],
    );
    #[allow(non_camel_case_types, dead_code)]
    pub type __sekvent_rpc_Echo__Ping = (super::outer::Inner, super::Inner);
}

#[sekvent_component::component(name = "echo", package = "check.echo.v1", proto = "crate::proto")]
pub trait Echo {
    #[call]
    async fn ping(
        &self,
        cx: &sekvent_component::CallContext,
        req: Inner,
    ) -> Result<Inner, sekvent_component::AppError>;
}

fn main() {}
