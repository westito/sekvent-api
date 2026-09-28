#![allow(dead_code)]

pub struct Plain;

#[sekvent_component::component(name = "echo", package = "check.echo.v1")]
pub trait Echo {
    #[call]
    async fn ping(
        &self,
        cx: &sekvent_component::CallContext,
        req: Plain,
    ) -> Result<String, sekvent_component::AppError>;
}

fn main() {}
