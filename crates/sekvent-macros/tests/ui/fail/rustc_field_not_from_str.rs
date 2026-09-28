#![allow(dead_code)]

#[derive(Debug, sekvent_component::ComponentError)]
pub enum OrdersError {
    #[reason("TOO_MANY_ITEMS", code = InvalidArgument)]
    TooMany { items: Vec<String> },
    #[other]
    Other(sekvent_component::AppError),
}

fn main() {}
