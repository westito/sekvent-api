#[sekvent_component::component(name = "notes", local_only)]
pub trait Notes {
    #[call(anonymous, idempotent)]
    async fn list(
        &self,
        cx: &sekvent_component::CallContext,
        req: (),
    ) -> Result<Vec<String>, sekvent_component::AppError>;
}

fn main() {}
