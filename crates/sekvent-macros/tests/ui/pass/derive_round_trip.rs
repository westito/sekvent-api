//! `#[derive(ComponentError)]` round trips, with an `Option` field, a unit
//! variant and decoding that falls back to the `#[other]` variant.

use std::collections::BTreeMap;

use sekvent_component::{AppError, ComponentError, ErrorCode};

#[derive(Debug, ComponentError)]
#[component_error(domain = "check.orders.v1")]
pub enum OrdersError {
    #[reason("ORDER_NOT_FOUND", code = NotFound, message = "order {order_id} not found")]
    OrderNotFound {
        order_id: String,
        hint: Option<String>,
    },
    #[reason("QUANTITY_TOO_LARGE", code = InvalidArgument)]
    QuantityTooLarge { max: u32 },
    #[reason("ORDERS_CLOSED", code = Unavailable)]
    Closed,
    #[other]
    Other(AppError),
}

/// Everything caller-visible about an error (`AppError` is neither `Clone`
/// nor `PartialEq`).
type Parts = (
    ErrorCode,
    String,
    Option<String>,
    Option<String>,
    BTreeMap<String, String>,
);

fn parts(error: &AppError) -> Parts {
    (
        error.code(),
        error.message().to_owned(),
        error.reason().map(str::to_owned),
        error.domain().map(str::to_owned),
        error.metadata().clone(),
    )
}

fn rebuild(parts: &Parts) -> AppError {
    let (code, message, reason, domain, metadata) = parts;
    let mut error = AppError::new(*code, message.clone());
    if let Some(reason) = reason {
        error = error.with_reason(reason.clone());
    }
    if let Some(domain) = domain {
        error = error.with_domain(domain.clone());
    }
    for (key, value) in metadata {
        error = error.with_metadata(key.clone(), value.clone());
    }
    error
}

fn not_found(order_id: &str, hint: Option<&str>) -> OrdersError {
    OrdersError::OrderNotFound {
        order_id: order_id.to_owned(),
        hint: hint.map(str::to_owned),
    }
}

fn main() {
    let wire = not_found("ord-1", Some("check the id")).into_app_error();
    assert_eq!(wire.code(), ErrorCode::NotFound);
    assert_eq!(wire.message(), "order ord-1 not found");
    assert_eq!(wire.reason(), Some("ORDER_NOT_FOUND"));
    assert_eq!(wire.domain(), Some("check.orders.v1"));
    let metadata = wire.metadata();
    assert_eq!(metadata.get("order_id").map(String::as_str), Some("ord-1"));
    assert_eq!(
        metadata.get("hint").map(String::as_str),
        Some("check the id")
    );
    match OrdersError::from_app_error(wire) {
        OrdersError::OrderNotFound { order_id, hint } => {
            assert_eq!(order_id, "ord-1");
            assert_eq!(hint.as_deref(), Some("check the id"));
        }
        other => panic!("expected OrderNotFound, got {other:?}"),
    }

    // An absent `Option` field is not written and decodes as `None`.
    let wire = AppError::from(not_found("ord-2", None));
    assert!(!wire.metadata().contains_key("hint"));
    match OrdersError::from(wire) {
        OrdersError::OrderNotFound { order_id, hint } => {
            assert_eq!(order_id, "ord-2");
            assert_eq!(hint, None);
        }
        other => panic!("expected OrderNotFound, got {other:?}"),
    }

    let wire = OrdersError::QuantityTooLarge { max: 5 }.into_app_error();
    assert_eq!(wire.message(), "quantity too large");
    assert!(matches!(
        OrdersError::from_app_error(wire),
        OrdersError::QuantityTooLarge { max: 5 }
    ));

    let wire = OrdersError::Closed.into_app_error();
    assert_eq!(wire.code(), ErrorCode::Unavailable);
    assert_eq!(wire.message(), "orders closed");
    assert!(wire.metadata().is_empty());
    assert!(matches!(
        OrdersError::from_app_error(wire),
        OrdersError::Closed
    ));

    // Everything the enum does not know decodes into `Other`, unchanged.
    let fallbacks = [
        AppError::failed_precondition("later").with_reason("FROM_THE_FUTURE"),
        AppError::unavailable("closed")
            .with_reason("ORDERS_CLOSED")
            .with_domain("check.billing.v1"),
        AppError::unavailable("closed").with_reason("ORDERS_CLOSED"),
        AppError::not_found("gone")
            .with_reason("ORDER_NOT_FOUND")
            .with_domain("check.orders.v1"),
        AppError::invalid_argument("too many")
            .with_reason("QUANTITY_TOO_LARGE")
            .with_domain("check.orders.v1")
            .with_metadata("max", "lots"),
        AppError::new(ErrorCode::Internal, "no reason"),
    ];
    for error in fallbacks {
        let expected = parts(&error);
        match OrdersError::from_app_error(error) {
            OrdersError::Other(error) => assert_eq!(parts(&error), expected),
            other => panic!("expected Other, got {other:?}"),
        }
    }

    let plain = parts(&AppError::unavailable("down"));
    let wire = OrdersError::Other(rebuild(&plain)).into_app_error();
    assert_eq!(parts(&wire), plain);
}
