#[sekvent_component::component(name = "echo", package = "check.echo.v1")]
pub trait Echo<T> {
    #[call]
    async fn ping(
        &self,
        cx: &sekvent_component::CallContext,
        req: String,
    ) -> Result<String, sekvent_component::AppError>;
}

fn main() {}
