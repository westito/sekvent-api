#![allow(dead_code, non_upper_case_globals)]

pub struct Plain;

mod proto {
    pub const __sekvent_service_Echo: (&str, &[(&str, &str, &str, bool)]) = (
        "check.echo.v1.Echo",
        &[(
            "Ping",
            "check.echo.v1.Plain",
            "google.protobuf.StringValue",
            false,
        )],
    );
    #[allow(non_camel_case_types, dead_code)]
    pub type __sekvent_rpc_Echo__Ping = (super::Plain, String);
}

#[sekvent_component::component(name = "echo", package = "check.echo.v1", proto = "crate::proto")]
pub trait Echo {
    #[call]
    async fn ping(
        &self,
        cx: &sekvent_component::CallContext,
        req: Plain,
    ) -> Result<String, sekvent_component::AppError>;
}

fn main() {}
