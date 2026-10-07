//! `AppError` <-> `tonic::Status`, lossless for the caller-visible part.
//!
//! Details travel as a `google.rpc.Status` in the standard
//! `grpc-status-details-bin` trailer (via `tonic-types`): `ErrorInfo`
//! (reason, domain, metadata), `RetryInfo`, `BadRequest` (field violations).
//! An optional project-specific binary trailer can be added for clients that
//! predate the standard one.
//!
//! `ErrorInfo` is emitted whenever a reason, a domain or metadata is present,
//! so none of the three is lost when the others are absent. An absent reason
//! or domain travels as the empty string; on the wire an empty string and an
//! absent value are therefore the same thing.

use std::collections::HashMap;

use tonic::metadata::{BinaryMetadataKey, MetadataValue};
use tonic::{Code, Status};
use tonic_types::{ErrorDetails, StatusExt};

use crate::{AppError, ErrorCode};

/// Encode an error as a status. The internal source is never sent.
///
/// This only encodes; a serving boundary that answers with it directly
/// calls `sekvent_error::log_server_side` itself. `From<AppError> for Status`
/// does both.
pub fn to_status(error: &AppError) -> Status {
    let code = Code::from(error.code().as_i32());
    let mut details = ErrorDetails::new();
    let mut has_details = false;

    if error.reason().is_some() || error.domain().is_some() || !error.metadata().is_empty() {
        let metadata: HashMap<String, String> = error
            .metadata()
            .iter()
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();
        details.set_error_info(
            error.reason().unwrap_or_default(),
            error.domain().unwrap_or_default(),
            metadata,
        );
        has_details = true;
    }
    if let Some(after) = error.retry_after() {
        details.set_retry_info(Some(after));
        has_details = true;
    }
    if !error.field_violations().is_empty() {
        let violations: Vec<tonic_types::FieldViolation> = error
            .field_violations()
            .iter()
            .map(|violation| {
                tonic_types::FieldViolation::new(
                    violation.field.clone(),
                    violation.description.clone(),
                )
            })
            .collect();
        details.set_bad_request(violations);
        has_details = true;
    }

    if has_details {
        Status::with_error_details(code, error.message(), details)
    } else {
        Status::new(code, error.message())
    }
}

/// Decode a status. Missing or malformed details yield an error with only
/// the code and message; an unknown code number becomes `UNKNOWN`.
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
        for violation in &bad_request.field_violations {
            error =
                error.with_field_violation(violation.field.clone(), violation.description.clone());
        }
    }
    error
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
        assert!(status.details().is_empty());

        let back = from_status(&status);
        assert_eq!(back.code(), ErrorCode::NotFound);
        assert_eq!(back.message(), "no such order");
        assert!(back.reason().is_none());
        assert!(back.retry_after().is_none());
        assert!(back.field_violations().is_empty());
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
