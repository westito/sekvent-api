//! Both wire mappings, both directions, for every code and every optional field.

use std::time::Duration;

use axum::response::IntoResponse;
use bytes::Bytes;
use prost::Message;
use sekvent_error::{AppError, ErrorCode, grpc, http};
use tonic::metadata::{BinaryMetadataKey, MetadataValue};
use tonic::{Code, Status};

fn every_field() -> AppError {
    AppError::new(ErrorCode::FailedPrecondition, "order is closed")
        .with_reason("ORDER_CLOSED")
        .with_domain("orders")
        .with_metadata("order_id", "42")
        .with_metadata("state", "closed")
        .with_retry_after(Duration::new(1, 234_567_891))
        .with_field_violation("items[0].qty", "must be positive")
        .with_field_violation("currency", "unsupported")
        .with_source(std::io::Error::other("secret upstream detail"))
}

/// Each optional field on its own, so none depends on another to travel.
fn one_field_each() -> Vec<AppError> {
    vec![
        AppError::unavailable("a").with_reason("BUSY"),
        AppError::unavailable("b").with_domain("billing"),
        AppError::unavailable("c").with_metadata("k", "v"),
        AppError::unavailable("d").with_retry_after(Duration::from_millis(250)),
        AppError::unavailable("e").with_field_violation("f", "g"),
    ]
}

async fn http_round_trip(error: AppError) -> (u16, axum::http::HeaderMap, AppError) {
    let response = error.into_response();
    let status = response.status().as_u16();
    let headers = response.headers().clone();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body");
    let back = http::from_json_body(status, &body).expect("error body");
    (status, headers, back)
}

#[test]
fn every_code_survives_grpc() {
    for code in ErrorCode::ALL {
        let status = grpc::to_status(&AppError::new(code, "m"));
        assert_eq!(i32::from(status.code()), code.as_i32(), "{code}");
        let back = grpc::from_status(&status);
        assert_eq!(back.code(), code);
        assert_eq!(back.message(), "m");
    }
}

#[tokio::test]
async fn every_code_survives_http() {
    for code in ErrorCode::ALL {
        let (status, _, back) = http_round_trip(AppError::new(code, "m")).await;
        assert_eq!(status, code.http_status(), "{code}");
        assert_eq!(back.code(), code);
        assert_eq!(back.message(), "m");
    }
}

#[test]
fn every_field_survives_grpc_exactly() {
    let error = every_field();
    let back = grpc::from_status(&grpc::to_status(&error));
    assert_eq!(back.to_wire(), error.to_wire());
    assert_eq!(
        back.retry_after(),
        error.retry_after(),
        "nanosecond precision"
    );
    assert!(std::error::Error::source(&back).is_none());

    for error in one_field_each() {
        let back = grpc::from_status(&grpc::to_status(&error));
        assert_eq!(back.to_wire(), error.to_wire());
    }
}

#[tokio::test]
async fn every_field_survives_http() {
    let error = every_field();
    let wire = error.to_wire();
    let (_, headers, back) = http_round_trip(error).await;
    assert_eq!(back.to_wire(), wire);
    assert_eq!(
        headers.get("retry-after").map(|v| v.to_str().unwrap()),
        Some("2")
    );
    assert_eq!(
        headers.get("content-type").map(|v| v.to_str().unwrap()),
        Some("application/json")
    );

    for error in one_field_each() {
        let wire = error.to_wire();
        let (_, _, back) = http_round_trip(error).await;
        assert_eq!(back.to_wire(), wire);
    }
}

#[tokio::test]
async fn the_http_body_has_the_documented_shape_and_no_source() {
    let response = every_field().into_response();
    assert_eq!(response.status().as_u16(), 400);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let text = std::str::from_utf8(&body).unwrap();
    assert!(!text.contains("secret upstream detail"));
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["error"]["code"], "FAILED_PRECONDITION");
    assert_eq!(json["error"]["reason"], "ORDER_CLOSED");
    assert_eq!(json["error"]["retry_after_ms"], 1234);
    assert_eq!(json["error"]["field_violations"][1]["field"], "currency");
}

#[tokio::test]
async fn absent_optional_fields_stay_absent() {
    let response = AppError::not_found("gone").into_response();
    assert!(response.headers().get("retry-after").is_none());
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(
        json,
        serde_json::json!({"error": {"code": "NOT_FOUND", "message": "gone"}})
    );

    let back = grpc::from_status(&grpc::to_status(&AppError::not_found("gone")));
    assert_eq!(back.to_wire(), AppError::not_found("gone").to_wire());
}

#[test]
fn malformed_or_absent_grpc_details_leave_code_and_message() {
    let garbage = Status::with_details(Code::Unavailable, "down", Bytes::from_static(&[0xff; 3]));
    let back = grpc::from_status(&garbage);
    assert_eq!(back.code(), ErrorCode::Unavailable);
    assert_eq!(back.message(), "down");
    assert_eq!(back.to_wire(), AppError::unavailable("down").to_wire());

    // A well-formed status whose ErrorInfo payload does not decode.
    let broken_info = tonic_types::Status {
        code: 14,
        message: "down".into(),
        details: vec![prost_types::Any {
            type_url: "type.googleapis.com/google.rpc.ErrorInfo".into(),
            value: vec![0xff, 0xff],
        }],
    };
    let status = Status::with_details(
        Code::Unavailable,
        "down",
        broken_info.encode_to_vec().into(),
    );
    assert_eq!(
        grpc::from_status(&status).to_wire(),
        AppError::unavailable("down").to_wire()
    );

    let plain = Status::new(Code::Internal, "boom");
    assert_eq!(
        grpc::from_status(&plain).to_wire(),
        AppError::new(ErrorCode::Internal, "boom").to_wire()
    );
}

#[test]
fn malformed_or_absent_http_bodies_are_rejected() {
    for body in [
        &b""[..],
        b"not json",
        br#"{"message":"no envelope"}"#,
        br#"{"error":{"message":"no code"}}"#,
        br#"{"error":"flat"}"#,
    ] {
        assert!(
            http::from_json_body(500, body).is_none(),
            "{}",
            String::from_utf8_lossy(body)
        );
    }
}

#[test]
fn legacy_trailer_rides_alongside_standard_details() {
    let detail = prost_types::Timestamp {
        seconds: 7,
        nanos: 9,
    };
    let status = grpc::with_legacy_detail(grpc::to_status(&every_field()), "x-legacy-bin", &detail);
    assert_eq!(
        grpc::legacy_detail::<prost_types::Timestamp>(&status, "x-legacy-bin"),
        Some(detail)
    );
    assert_eq!(
        grpc::from_status(&status).to_wire(),
        every_field().to_wire()
    );

    assert_eq!(
        grpc::legacy_detail::<prost_types::Timestamp>(&status, "x-missing-bin"),
        None
    );

    let mut garbage = Status::internal("x");
    garbage.metadata_mut().insert_bin(
        BinaryMetadataKey::from_static("x-legacy-bin"),
        MetadataValue::from_bytes(&[0xff]),
    );
    assert_eq!(
        grpc::legacy_detail::<prost_types::Timestamp>(&garbage, "x-legacy-bin"),
        None
    );
}

#[test]
#[should_panic(expected = "invalid metadata key")]
fn a_non_binary_legacy_key_is_a_programming_error() {
    let _ = grpc::with_legacy_detail(
        Status::internal("x"),
        "x-legacy",
        &prost_types::Timestamp::default(),
    );
}
