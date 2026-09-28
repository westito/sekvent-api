#![allow(dead_code)]

#[derive(Debug, sekvent_component::ComponentError)]
pub enum OrdersError {
    #[reason("order_gone", code = NotFound)]
    Gone,
    #[other]
    Other(sekvent_component::AppError),
}

fn main() {}
