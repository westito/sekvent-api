#[sekvent_component::component(name = "echo", package = "check.echo.v1", proto = "check.echo.v1")]
pub trait Echo {
    #[call]
    async fn ping(
        &self,
        cx: &sekvent_component::CallContext,
        req: String,
    ) -> Result<String, sekvent_component::AppError>;
}

#[sekvent_component::component(
    name = "again",
    package = "check.again.v1",
    proto = "crate::proto::Again<u8>"
)]
pub trait Again {
    #[call]
    async fn ping(
        &self,
        cx: &sekvent_component::CallContext,
        req: String,
    ) -> Result<String, sekvent_component::AppError>;
}

fn main() {}
