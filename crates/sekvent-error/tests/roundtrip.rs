//! Both wire mappings, both directions, for every code and every optional field.

use std::time::Duration;

use axum::response::IntoResponse;
use bytes::Bytes;
use prost::Message;
use sekvent_error::{AppError, ErrorCode, FieldViolation, grpc, http};
use tonic::metadata::{BinaryMetadataKey, MetadataValue};
use tonic::{Code, Status};
use tonic_types::StatusExt;

fn every_field() -> AppError {
    AppError::new(ErrorCode::FailedPrecondition, "order is closed")
        .with_reason("ORDER_CLOSED")
        .with_domain("orders")
        .with_metadata("order_id", "42")
        .with_metadata("state", "closed")
        .with_retry_after(Duration::new(1, 234_567_891))
        .with_field_violation("items[0].qty", "must be positive")
        .with_violation(
            FieldViolation::new("currency", "unsupported")
                .with_reason("CURRENCY_UNSUPPORTED")
                .with_metadata("currency", "XTS")
                .with_metadata("supported", "EUR,USD"),
        )
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
        AppError::unavailable("f").with_violation(FieldViolation::new("f", "g").with_reason("R")),
        AppError::unavailable("g")
            .with_violation(FieldViolation::new("f", "g").with_metadata("k", "v")),
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
    for code in ErrorCode::ALL
        .into_iter()
        .filter(|code| *code != ErrorCode::Ok)
    {
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
    assert_eq!(
        json["error"]["field_violations"][1]["reason"],
        "CURRENCY_UNSUPPORTED"
    );
    assert_eq!(
        json["error"]["field_violations"][1]["metadata"],
        serde_json::json!({"currency": "XTS", "supported": "EUR,USD"})
    );
    assert_eq!(
        json["error"]["field_violations"][0],
        serde_json::json!({"field": "items[0].qty", "description": "must be positive"}),
        "a plain violation keeps exactly the two original members"
    );
}

#[test]
fn an_old_json_violation_parses_with_defaults() {
    let body = br#"{"error":{"code":"INVALID_ARGUMENT","message":"bad","field_violations":[{"field":"qty","description":"must be positive"}]}}"#;
    let back = http::from_json_body(400, body).expect("error body");
    assert_eq!(
        back.field_violations(),
        [FieldViolation::new("qty", "must be positive")]
    );
    assert_eq!(back.field_violations()[0].reason, None);
    assert!(back.field_violations()[0].metadata.is_empty());
}

/// The detail types of a status, in order.
fn detail_types(status: &Status) -> Vec<String> {
    tonic_types::Status::decode(status.details())
        .expect("rpc status")
        .details
        .into_iter()
        .map(|detail| detail.type_url)
        .collect()
}

#[test]
fn the_reason_rides_in_the_standard_bad_request() {
    let error = AppError::invalid_argument("bad")
        .with_violation(FieldViolation::new("qty", "must be positive").with_reason("NOT_POSITIVE"))
        .with_field_violation("sku", "unknown");
    let status = grpc::to_status(&error);
    let bad_request = status.get_details_bad_request().expect("bad request");
    assert_eq!(bad_request.field_violations[0].reason, "NOT_POSITIVE");
    assert_eq!(bad_request.field_violations[1].reason, "");
    assert_eq!(
        detail_types(&status),
        ["type.googleapis.com/google.rpc.BadRequest"],
        "no extra detail without violation metadata"
    );
}

#[test]
fn violation_metadata_rides_in_the_extra_detail() {
    let error = AppError::invalid_argument("bad")
        .with_field_violation("sku", "unknown")
        .with_violation(FieldViolation::new("qty", "too large").with_metadata("max", "10"));
    let status = grpc::to_status(&error);
    assert_eq!(
        detail_types(&status),
        [
            "type.googleapis.com/google.rpc.BadRequest",
            grpc::FIELD_VIOLATION_DETAILS_TYPE_URL
        ]
    );
    assert_eq!(
        grpc::FIELD_VIOLATION_DETAILS_TYPE_URL,
        "type.googleapis.com/sekvent.error.v1.FieldViolationDetails"
    );
    assert_eq!(grpc::from_status(&status).to_wire(), error.to_wire());
}

#[test]
fn a_status_written_by_tonic_types_alone_decodes_without_reason_or_metadata() {
    let mut details = tonic_types::ErrorDetails::new();
    details.set_bad_request(vec![tonic_types::FieldViolation::new(
        "qty",
        "must be positive",
    )]);
    let status = Status::with_error_details(Code::InvalidArgument, "bad", details);
    let back = grpc::from_status(&status);
    assert_eq!(
        back.to_wire(),
        AppError::invalid_argument("bad")
            .with_field_violation("qty", "must be positive")
            .to_wire()
    );
}

/// A status with a `BadRequest` of `fields` and the given extra detail
/// payloads, typed as `FieldViolationDetails`.
fn with_extra_details(fields: &[&str], extras: Vec<Vec<u8>>) -> Status {
    let bad_request = tonic_types::pb::BadRequest {
        field_violations: fields
            .iter()
            .map(|field| tonic_types::pb::bad_request::FieldViolation {
                field: (*field).to_owned(),
                description: "d".into(),
                reason: "R".into(),
                localized_message: None,
            })
            .collect(),
    };
    let mut details = vec![prost_types::Any {
        type_url: "type.googleapis.com/google.rpc.BadRequest".into(),
        value: bad_request.encode_to_vec(),
    }];
    details.extend(extras.into_iter().map(|value| prost_types::Any {
        type_url: grpc::FIELD_VIOLATION_DETAILS_TYPE_URL.into(),
        value,
    }));
    let status = tonic_types::Status {
        code: 3,
        message: "bad".into(),
        details,
    };
    Status::with_details(Code::InvalidArgument, "bad", status.encode_to_vec().into())
}

/// The protobuf form of `FieldViolationDetails` for the given per-entry
/// metadata, written by hand so the test does not share the encoder.
fn extra_detail(entries: &[&[(&str, &str)]]) -> Vec<u8> {
    #[derive(Clone, PartialEq, prost::Message)]
    struct Entry {
        #[prost(btree_map = "string, string", tag = "1")]
        metadata: std::collections::BTreeMap<String, String>,
    }
    #[derive(Clone, PartialEq, prost::Message)]
    struct Extra {
        #[prost(message, repeated, tag = "1")]
        violations: Vec<Entry>,
    }
    Extra {
        violations: entries
            .iter()
            .map(|pairs| Entry {
                metadata: pairs
                    .iter()
                    .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
                    .collect(),
            })
            .collect(),
    }
    .encode_to_vec()
}

fn expected(fields: &[&str], metadata: &[&[(&str, &str)]]) -> AppError {
    let mut error = AppError::invalid_argument("bad");
    for (index, field) in fields.iter().enumerate() {
        let mut violation = FieldViolation::new(*field, "d").with_reason("R");
        for (key, value) in metadata.get(index).copied().unwrap_or_default() {
            violation = violation.with_metadata(*key, *value);
        }
        error = error.with_violation(violation);
    }
    error
}

#[test]
fn a_garbage_extra_detail_leaves_the_standard_part() {
    let status = with_extra_details(&["a", "b"], vec![vec![0xff, 0xff, 0xff]]);
    assert_eq!(
        grpc::from_status(&status).to_wire(),
        expected(&["a", "b"], &[]).to_wire()
    );
}

#[test]
fn a_misaligned_extra_detail_is_applied_entry_by_entry() {
    let shorter = with_extra_details(&["a", "b"], vec![extra_detail(&[&[("k", "1")]])]);
    assert_eq!(
        grpc::from_status(&shorter).to_wire(),
        expected(&["a", "b"], &[&[("k", "1")]]).to_wire()
    );

    let longer = with_extra_details(
        &["a"],
        vec![extra_detail(&[&[("k", "1")], &[("k", "2")], &[]])],
    );
    assert_eq!(
        grpc::from_status(&longer).to_wire(),
        expected(&["a"], &[&[("k", "1")]]).to_wire()
    );
}

#[test]
fn the_first_decodable_extra_detail_wins() {
    let status = with_extra_details(
        &["a"],
        vec![
            vec![0xff],
            extra_detail(&[&[("k", "1")]]),
            extra_detail(&[&[("k", "2")]]),
        ],
    );
    assert_eq!(
        grpc::from_status(&status).to_wire(),
        expected(&["a"], &[&[("k", "1")]]).to_wire()
    );
}

#[test]
fn an_extra_detail_without_a_bad_request_is_ignored() {
    let status = tonic_types::Status {
        code: 3,
        message: "bad".into(),
        details: vec![prost_types::Any {
            type_url: grpc::FIELD_VIOLATION_DETAILS_TYPE_URL.into(),
            value: extra_detail(&[&[("k", "1")]]),
        }],
    };
    let status = Status::with_details(Code::InvalidArgument, "bad", status.encode_to_vec().into());
    assert_eq!(
        grpc::from_status(&status).to_wire(),
        AppError::invalid_argument("bad").to_wire()
    );
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

#[tokio::test]
async fn an_ok_code_is_never_read_back_as_an_error() {
    let response = AppError::new(ErrorCode::Ok, "m").into_response();
    let status = response.status().as_u16();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    assert_eq!(status, 200);
    assert!(http::from_json_body(status, &body).is_none());
    assert!(http::from_json_body(500, &body).is_none());
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
