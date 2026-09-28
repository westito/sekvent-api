#[sekvent_component::component(name = "echo", package = "check.echo.v1", colour = "red")]
pub trait Echo {
    #[call]
    async fn ping(
        &self,
        cx: &sekvent_component::CallContext,
        req: String,
    ) -> Result<String, sekvent_component::AppError>;
}

#[sekvent_component::component(name = "again", name = "twice", package = "check.echo.v1")]
pub trait Again {
    #[call]
    async fn ping(
        &self,
        cx: &sekvent_component::CallContext,
        req: String,
    ) -> Result<String, sekvent_component::AppError>;
}

fn main() {}
