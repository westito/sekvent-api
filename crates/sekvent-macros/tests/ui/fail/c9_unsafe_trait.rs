#[sekvent_component::component(name = "echo", package = "check.echo.v1", proto = "crate::proto")]
pub unsafe trait Echo {
    #[call]
    async fn ping(
        &self,
        cx: &sekvent_component::CallContext,
        req: String,
    ) -> Result<String, sekvent_component::AppError>;
}

fn main() {}
