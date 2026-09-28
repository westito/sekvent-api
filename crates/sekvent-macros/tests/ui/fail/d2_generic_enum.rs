#![allow(dead_code)]

#[derive(Debug, sekvent_component::ComponentError)]
pub enum OrdersError<T> {
    #[reason("ORDER_GONE", code = NotFound)]
    Gone { order_id: T },
    #[other]
    Other(sekvent_component::AppError),
}

fn main() {}
