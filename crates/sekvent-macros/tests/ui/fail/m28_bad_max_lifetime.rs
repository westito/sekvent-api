//! `max_lifetime` that is not a positive humantime duration.

#[sekvent_component::component(name = "echo", package = "check.echo.v1", proto = "crate::proto")]
pub trait Echo {
    #[call(stream, max_lifetime = "0s")]
    async fn ping(
        &self,
        cx: &sekvent_component::CallContext,
        req: String,
    ) -> Result<
        sekvent_component::CallStream<String, sekvent_component::AppError>,
        sekvent_component::AppError,
    >;

    #[call(stream, max_lifetime = 600)]
    async fn pong(
        &self,
        cx: &sekvent_component::CallContext,
        req: String,
    ) -> Result<
        sekvent_component::CallStream<String, sekvent_component::AppError>,
        sekvent_component::AppError,
    >;
}

fn main() {}
