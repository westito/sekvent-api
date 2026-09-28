#[sekvent_component::component(name = "notes", local_only, proto = "crate::proto")]
pub trait Notes {
    #[call]
    async fn add(
        &self,
        cx: &sekvent_component::CallContext,
        req: String,
    ) -> Result<usize, sekvent_component::AppError>;
}

fn main() {}
