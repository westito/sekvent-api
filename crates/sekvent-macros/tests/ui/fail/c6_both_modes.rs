#[sekvent_component::component(name = "echo", package = "check.echo.v1", local_only, remote_only)]
pub trait Echo {
    #[call]
    async fn ping(
        &self,
        cx: &sekvent_component::CallContext,
        req: String,
    ) -> Result<String, sekvent_component::AppError>;
}

fn main() {}
