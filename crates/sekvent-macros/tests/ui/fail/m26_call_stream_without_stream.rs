//! A method that returns a `CallStream` without `#[call(stream)]`.

#[sekvent_component::component(name = "echo", package = "check.echo.v1", proto = "crate::proto")]
pub trait Echo {
    #[call(idempotent)]
    async fn ping(
        &self,
        cx: &sekvent_component::CallContext,
        req: String,
    ) -> Result<
        sekvent_component::CallStream<String, sekvent_component::AppError>,
        sekvent_component::AppError,
    >;
}

fn main() {}
