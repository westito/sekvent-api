#[sekvent_component::component(name = "echo", package = "check.echo.v1")]
pub trait Echo {
    #[call]
    async fn Ping(
        &self,
        cx: &sekvent_component::CallContext,
        req: String,
    ) -> Result<String, sekvent_component::AppError>;
}

fn main() {}
