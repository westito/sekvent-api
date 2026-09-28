#![allow(dead_code)]

#[derive(Debug, sekvent_component::ComponentError)]
pub enum OrdersError {
    #[reason("ORDER_GONE", code = Ok)]
    Gone,
    #[other]
    Other(sekvent_component::AppError),
}

fn main() {}
