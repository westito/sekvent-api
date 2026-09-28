#![allow(dead_code)]

#[derive(Debug)]
pub struct PlainError;

#[sekvent_component::component(name = "echo", package = "check.echo.v1")]
pub trait Echo {
    #[call]
    async fn ping(
        &self,
        cx: &sekvent_component::CallContext,
        req: String,
    ) -> Result<String, PlainError>;
}

fn main() {}
