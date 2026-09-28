#[sekvent_component::component(name = "echo", package = "Check.Echo")]
pub trait Echo {
    #[call]
    async fn ping(
        &self,
        cx: &sekvent_component::CallContext,
        req: String,
    ) -> Result<String, sekvent_component::AppError>;
}

fn main() {}
