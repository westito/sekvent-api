#![allow(dead_code)]

#[derive(Debug, sekvent_component::ComponentError)]
pub enum OrdersError {
    #[reason("ORDER_GONE", code = NotFound)]
    Gone { order_id: String },
    #[reason("ORDER_GONE", code = NotFound)]
    AlsoGone,
    #[other]
    Other(sekvent_component::AppError),
}

fn main() {}
