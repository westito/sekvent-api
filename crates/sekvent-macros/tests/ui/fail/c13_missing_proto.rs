#[sekvent_component::component(name = "echo", package = "check.echo.v1")]
pub trait Echo {
    #[call]
    async fn ping(
        &self,
        cx: &sekvent_component::CallContext,
        req: String,
    ) -> Result<String, sekvent_component::AppError>;
}

#[sekvent_component::component(name = "ledger", package = "check.ledger.v1", remote_only)]
pub trait Ledger {
    #[call]
    async fn record(
        &self,
        cx: &sekvent_component::CallContext,
        req: String,
    ) -> Result<String, sekvent_component::AppError>;
}

fn main() {}
