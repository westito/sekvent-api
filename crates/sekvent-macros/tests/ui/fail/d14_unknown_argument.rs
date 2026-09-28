#![allow(dead_code)]

#[derive(Debug, sekvent_component::ComponentError)]
pub enum OrdersError {
    #[reason("ORDER_GONE", code = NotFound, colour = "red")]
    Gone,
    #[other]
    Other(sekvent_component::AppError),
}

#[derive(Debug, sekvent_component::ComponentError)]
#[component_error(colour = "red")]
pub enum BillingError {
    #[other]
    Other(sekvent_component::AppError),
}

fn main() {}
