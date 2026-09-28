#![allow(dead_code)]

#[derive(Debug, sekvent_component::ComponentError)]
#[component_error(domain = "shop orders")]
pub enum OrdersError {
    #[reason("ORDER_GONE", code = NotFound)]
    Gone { order_id: String },
    #[other]
    Other(sekvent_component::AppError),
}

fn main() {}
