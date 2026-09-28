#[sekvent_component::component(name = "echo", package = "check.echo.v1")]
pub trait Echo {
    #[async_call]
    async fn ping(
        &self,
        cx: &sekvent_component::CallContext,
        req: String,
    ) -> Result<String, sekvent_component::AppError>;
}

#[sekvent_component::component(name = "later", package = "check.later.v1")]
pub trait Later {
    #[deferred]
    async fn ping(
        &self,
        cx: &sekvent_component::CallContext,
        req: String,
    ) -> Result<String, sekvent_component::AppError>;
}

fn main() {}
