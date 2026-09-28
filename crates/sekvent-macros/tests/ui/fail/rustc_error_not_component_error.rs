#![allow(dead_code, non_upper_case_globals)]

#[derive(Debug)]
pub struct PlainError;

mod proto {
    pub const __sekvent_service_Echo: (&str, &[(&str, &str, &str, bool)]) = (
        "check.echo.v1.Echo",
        &[(
            "Ping",
            "google.protobuf.StringValue",
            "google.protobuf.StringValue",
            false,
        )],
    );
}

#[sekvent_component::component(name = "echo", package = "check.echo.v1", proto = "crate::proto")]
pub trait Echo {
    #[call]
    async fn ping(
        &self,
        cx: &sekvent_component::CallContext,
        req: String,
    ) -> Result<String, PlainError>;
}

fn main() {}
