//! `AppError` <-> `tonic::Status`, lossless for the caller-visible part.
//!
//! Details travel as a `google.rpc.Status` in the standard
//! `grpc-status-details-bin` trailer: `RetryInfo`, `ErrorInfo` (reason,
//! domain, metadata) and `BadRequest` (field violations with their field,
//! description and reason). When any field violation carries metadata, one
//! more detail of type [`FIELD_VIOLATION_DETAILS_TYPE_URL`]
//! (`sekvent.error.v1.FieldViolationDetails`, reference schema in
//! `proto/sekvent/error/v1/details.proto`) follows: entry `i` holds the
//! metadata of `BadRequest.field_violations[i]`. Readers that do not know
//! the type skip it. An optional project-specific binary trailer can be
//! added for clients that predate the standard one.
//!
//! `ErrorInfo` is emitted whenever a reason, a domain or metadata is present,
//! so none of the three is lost when the others are absent. An absent reason
//! or domain, of the error or of a violation, travels as the empty string; on
//! the wire an empty string and an absent value are therefore the same thing.

use std::collections::HashMap;

use prost::Message;
use tonic::metadata::{BinaryMetadataKey, MetadataValue};
use tonic::{Code, Status};
use tonic_types::{StatusExt, pb};

use crate::{AppError, ErrorCode, FieldViolation};

/// Type URL of the detail carrying the metadata of each field violation,
/// index-aligned with `google.rpc.BadRequest.field_violations`.
pub const FIELD_VIOLATION_DETAILS_TYPE_URL: &str =
    "type.googleapis.com/sekvent.error.v1.FieldViolationDetails";

mod details {
    // prost generates `pub` accessors, unreachable from this private module.
    #![allow(unreachable_pub)]

    use std::collections::BTreeMap;

    /// `sekvent.error.v1.FieldViolationDetails`.
    #[derive(Clone, PartialEq, prost::Message)]
    pub struct FieldViolationDetails {
        #[prost(message, repeated, tag = "1")]
        pub violations: Vec<FieldViolationDetail>,
    }

    /// `sekvent.error.v1.FieldViolationDetail`.
    #[derive(Clone, PartialEq, prost::Message)]
    pub struct FieldViolationDetail {
        #[prost(btree_map = "string, string", tag = "1")]
        pub metadata: BTreeMap<String, String>,
    }
}

use details::{FieldViolationDetail, FieldViolationDetails};

fn any(type_url: &str, message: &impl Message) -> prost_types::Any {
    prost_types::Any {
        type_url: type_url.to_owned(),
        value: message.encode_to_vec(),
    }
}

/// Encode an error as a status. The internal source is never sent.
///
/// This only encodes; a serving boundary that answers with it directly
/// calls `sekvent_error::log_server_side` itself. `From<AppError> for Status`
/// does both.
pub fn to_status(error: &AppError) -> Status {
    let code = Code::from(error.code().as_i32());
    let mut details = Vec::new();

    if let Some(after) = error.retry_after() {
        let retry = pb::RetryInfo::from(tonic_types::RetryInfo::new(Some(after)));
        details.push(any(tonic_types::RetryInfo::TYPE_URL, &retry));
    }
    if error.reason().is_some() || error.domain().is_some() || !error.metadata().is_empty() {
        let metadata: HashMap<String, String> = error
            .metadata()
            .iter()
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();
        let info = pb::ErrorInfo {
            reason: error.reason().unwrap_or_default().to_owned(),
            domain: error.domain().unwrap_or_default().to_owned(),
            metadata,
        };
        details.push(any(tonic_types::ErrorInfo::TYPE_URL, &info));
    }
    let violations = error.field_violations();
    if !violations.is_empty() {
        let bad_request = pb::BadRequest {
            field_violations: violations
                .iter()
                .map(|violation| pb::bad_request::FieldViolation {
                    field: violation.field.clone(),
                    description: violation.description.clone(),
                    reason: violation.reason.clone().unwrap_or_default(),
                    localized_message: None,
                })
                .collect(),
        };
        details.push(any(tonic_types::BadRequest::TYPE_URL, &bad_request));
    }
    if violations
        .iter()
        .any(|violation| !violation.metadata.is_empty())
    {
        let extra = FieldViolationDetails {
            violations: violations
                .iter()
                .map(|violation| FieldViolationDetail {
                    metadata: violation.metadata.clone(),
                })
                .collect(),
        };
        details.push(any(FIELD_VIOLATION_DETAILS_TYPE_URL, &extra));
    }

    if details.is_empty() {
        return Status::new(code, error.message());
    }
    let status = pb::Status {
        code: error.code().as_i32(),
        message: error.message().to_owned(),
        details,
    };
    Status::with_details(code, error.message(), status.encode_to_vec().into())
}

/// Decode a status. Missing or malformed details yield an error with only
/// the code and message; an unknown code number becomes `UNKNOWN`. A
/// missing, malformed or misaligned `FieldViolationDetails` detail only
/// loses the violation metadata it would have carried.
pub fn from_status(status: &Status) -> AppError {
    let code = ErrorCode::from_i32(i32::from(status.code()));
    let mut error = AppError::new(code, status.message());
    let Ok(details) = status.check_error_details() else {
        return error;
    };

    if let Some(info) = details.error_info() {
        if !info.reason.is_empty() {
            error = error.with_reason(info.reason.clone());
        }
        if !info.domain.is_empty() {
            error = error.with_domain(info.domain.clone());
        }
        for (key, value) in &info.metadata {
            error = error.with_metadata(key.clone(), value.clone());
        }
    }
    if let Some(delay) = details.retry_info().and_then(|retry| retry.retry_delay) {
        error = error.with_retry_after(delay);
    }
    if let Some(bad_request) = details.bad_request() {
        let mut metadata = violation_metadata(status).into_iter();
        for violation in &bad_request.field_violations {
            let mut built =
                FieldViolation::new(violation.field.clone(), violation.description.clone());
            if !violation.reason.is_empty() {
                built = built.with_reason(violation.reason.clone());
            }
            if let Some(entry) = metadata.next() {
                built.metadata = entry.metadata;
            }
            error = error.with_violation(built);
        }
    }
    error
}

/// The entries of the first decodable `FieldViolationDetails` detail; empty
/// when there is none.
fn violation_metadata(status: &Status) -> Vec<FieldViolationDetail> {
    let Ok(rpc_status) = pb::Status::decode(status.details()) else {
        return Vec::new();
    };
    rpc_status
        .details
        .iter()
        .filter(|detail| detail.type_url == FIELD_VIOLATION_DETAILS_TYPE_URL)
        .find_map(|detail| FieldViolationDetails::decode(detail.value.as_slice()).ok())
        .map(|extra| extra.violations)
        .unwrap_or_default()
}

/// Attach an extra binary trailer (`key` must end in `-bin`) carrying a
/// caller-defined prost message, alongside the standard details.
///
/// # Panics
///
/// Panics when `key` is not a valid binary metadata key (lowercase, ending in
/// `-bin`). The key is a `'static` constant, so this is a programming error
/// that the first test run exposes.
pub fn with_legacy_detail<M: prost::Message>(
    mut status: Status,
    key: &'static str,
    detail: &M,
) -> Status {
    let key = BinaryMetadataKey::from_static(key);
    let value = MetadataValue::from_bytes(&detail.encode_to_vec());
    status.metadata_mut().insert_bin(key, value);
    status
}

/// Read a binary trailer written by [`with_legacy_detail`]; `None` when absent
/// or undecodable.
pub fn legacy_detail<M: prost::Message + Default>(status: &Status, key: &'static str) -> Option<M> {
    let bytes = status.metadata().get_bin(key)?.to_bytes().ok()?;
    M::decode(bytes).ok()
}

/// The serving-boundary conversion: logs a server-side failure (`UNKNOWN`,
/// `INTERNAL`, `DATA_LOSS`) once with its source chain, target
/// `sekvent::error`, then encodes it with [`to_status`].
impl From<AppError> for Status {
    fn from(error: AppError) -> Self {
        crate::log_server_side(&error);
        to_status(&error)
    }
}

impl From<Status> for AppError {
    fn from(status: Status) -> Self {
        from_status(&status)
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::test_support::{capture, field};

    #[derive(Clone, PartialEq, prost::Message)]
    struct Legacy {
        #[prost(string, tag = "1")]
        reason: String,
        #[prost(uint32, tag = "2")]
        attempt: u32,
    }

    #[test]
    fn a_bare_error_carries_no_details() {
        let status = to_status(&AppError::not_found("no such order"));
        assert_eq!(status.code(), Code::NotFound);
        assert_eq!(status.message(), "no such order");
        assert_eq!(status.details(), b"");

        let back = from_status(&status);
        assert_eq!(back.code(), ErrorCode::NotFound);
        assert_eq!(back.message(), "no such order");
        assert!(back.reason().is_none());
        assert!(back.retry_after().is_none());
        assert_eq!(back.field_violations(), [] as [crate::FieldViolation; 0]);
    }

    #[test]
    fn the_source_never_reaches_the_status() {
        let error = AppError::internal(std::io::Error::other("db password is hunter2"));
        let status = to_status(&error);
        assert_eq!(status.message(), "internal error");
        assert!(!format!("{status:?}").contains("hunter2"));
    }

    #[test]
    fn domain_and_metadata_survive_without_a_reason() {
        let error = AppError::unavailable("busy")
            .with_domain("orders")
            .with_metadata("region", "eu");
        let back = from_status(&to_status(&error));
        assert_eq!(back.reason(), None);
        assert_eq!(back.domain(), Some("orders"));
        assert_eq!(
            back.metadata().get("region").map(String::as_str),
            Some("eu")
        );
    }

    #[test]
    fn a_reason_without_domain_travels_with_an_empty_domain() {
        let error = AppError::new(ErrorCode::Aborted, "conflict").with_reason("CONFLICT");
        let status = to_status(&error);
        let info = status.get_details_error_info().expect("error info");
        assert_eq!(info.reason, "CONFLICT");
        assert_eq!(info.domain, "");
        let back = from_status(&status);
        assert_eq!(back.reason(), Some("CONFLICT"));
        assert_eq!(back.domain(), None);
    }

    #[test]
    fn retry_and_violations_round_trip() {
        let error = AppError::invalid_argument("bad")
            .with_retry_after(Duration::from_millis(1_500))
            .with_field_violation("items[0].qty", "must be positive");
        let back = from_status(&to_status(&error));
        assert_eq!(back.retry_after(), Some(Duration::from_millis(1_500)));
        assert_eq!(back.field_violations(), error.field_violations());
        assert!(back.reason().is_none());
    }

    #[test]
    fn legacy_detail_round_trips_and_misses_cleanly() {
        let detail = Legacy {
            reason: "QUOTA".into(),
            attempt: 3,
        };
        let status = with_legacy_detail(Status::unavailable("later"), "x-app-error-bin", &detail);
        assert_eq!(
            legacy_detail::<Legacy>(&status, "x-app-error-bin"),
            Some(detail)
        );
        assert_eq!(legacy_detail::<Legacy>(&status, "x-other-bin"), None);
        assert_eq!(legacy_detail::<Legacy>(&status, "not-binary"), None);
    }

    #[test]
    fn the_conversion_logs_an_internal_error_once() {
        let error = AppError::internal(std::io::Error::other("deadlock detected"));
        let mut status = None;
        let events = capture(|| status = Some(Status::from(error)));
        assert_eq!(status.as_ref().map(Status::code), Some(Code::Internal));
        assert_eq!(events.len(), 1, "{events:?}");
        assert_eq!(field(&events[0], "code"), Some("INTERNAL"));
        assert_eq!(field(&events[0], "source"), Some("deadlock detected"));
    }

    #[test]
    fn to_status_alone_never_logs() {
        let error = AppError::internal(std::io::Error::other("deadlock detected"));
        let events = capture(|| drop(to_status(&error)));
        assert!(events.is_empty(), "{events:?}");
    }

    #[test]
    fn the_conversion_does_not_log_caller_errors() {
        let events = capture(|| {
            drop(Status::from(AppError::not_found("no such order")));
            drop(Status::from(AppError::invalid_argument("bad")));
        });
        assert!(events.is_empty(), "{events:?}");
    }

    #[test]
    fn conversions_delegate() {
        let status: Status = AppError::permission_denied("no").into();
        assert_eq!(status.code(), Code::PermissionDenied);
        let error: AppError = status.into();
        assert_eq!(error.code(), ErrorCode::PermissionDenied);
    }
}
