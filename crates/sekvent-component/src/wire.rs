//! What crosses the serialization boundary besides requests and replies:
//! errors as a private protobuf message, and the guard that ties a spawned
//! call to the caller's future.

use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

use bytes::Bytes;
use prost::Message;
use sekvent_error::{AppError, FieldViolation, WireError};
use tokio::task::{JoinError, JoinHandle};

mod pb {
    // prost generates `pub` accessors, unreachable from this private module.
    #![allow(unreachable_pub)]

    use std::collections::BTreeMap;

    /// Protobuf form of [`WireError`](sekvent_error::WireError).
    #[derive(Clone, PartialEq, prost::Message)]
    pub struct WireErrorPb {
        #[prost(string, tag = "1")]
        pub code: String,
        #[prost(string, tag = "2")]
        pub message: String,
        #[prost(string, optional, tag = "3")]
        pub reason: Option<String>,
        #[prost(string, optional, tag = "4")]
        pub domain: Option<String>,
        #[prost(btree_map = "string, string", tag = "5")]
        pub metadata: BTreeMap<String, String>,
        #[prost(uint64, optional, tag = "6")]
        pub retry_after_ms: Option<u64>,
        #[prost(message, repeated, tag = "7")]
        pub field_violations: Vec<FieldViolationPb>,
    }

    /// Protobuf form of [`FieldViolation`](sekvent_error::FieldViolation).
    #[derive(Clone, PartialEq, prost::Message)]
    pub struct FieldViolationPb {
        #[prost(string, tag = "1")]
        pub field: String,
        #[prost(string, tag = "2")]
        pub description: String,
    }
}

use pb::{FieldViolationPb, WireErrorPb};

impl From<WireError> for WireErrorPb {
    fn from(wire: WireError) -> Self {
        Self {
            code: wire.code,
            message: wire.message,
            reason: wire.reason,
            domain: wire.domain,
            metadata: wire.metadata,
            retry_after_ms: wire.retry_after_ms,
            field_violations: wire
                .field_violations
                .into_iter()
                .map(|violation| FieldViolationPb {
                    field: violation.field,
                    description: violation.description,
                })
                .collect(),
        }
    }
}

impl From<WireErrorPb> for WireError {
    fn from(pb: WireErrorPb) -> Self {
        Self {
            code: pb.code,
            message: pb.message,
            reason: pb.reason,
            domain: pb.domain,
            metadata: pb.metadata,
            retry_after_ms: pb.retry_after_ms,
            field_violations: pb
                .field_violations
                .into_iter()
                .map(|violation| FieldViolation {
                    field: violation.field,
                    description: violation.description,
                })
                .collect(),
        }
    }
}

/// Encode the caller-visible part of `error`; the source chain stays behind.
pub(crate) fn encode_error(error: &AppError) -> Bytes {
    Bytes::from(WireErrorPb::from(error.to_wire()).encode_to_vec())
}

/// Decode an error encoded by [`encode_error`]; `None` when the bytes are
/// not a valid error message.
pub(crate) fn decode_error(bytes: Bytes) -> Option<AppError> {
    WireErrorPb::decode(bytes)
        .ok()
        .map(|pb| AppError::from_wire(pb.into()))
}

/// A spawned task that is aborted when this guard is dropped, so a caller
/// that stops waiting also stops the work.
pub(crate) struct AbortOnDrop<T>(pub(crate) JoinHandle<T>);

impl<T> Future for AbortOnDrop<T> {
    type Output = Result<T, JoinError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        Pin::new(&mut self.get_mut().0).poll(cx)
    }
}

impl<T> Drop for AbortOnDrop<T> {
    fn drop(&mut self) {
        self.0.abort();
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use sekvent_error::ErrorCode;
    use tokio::sync::oneshot;

    use super::*;

    #[test]
    fn every_wire_field_round_trips() {
        let error = AppError::new(ErrorCode::FailedPrecondition, "only 2 of sku-1 left")
            .with_reason("OUT_OF_STOCK")
            .with_domain("shop.inventory.v1")
            .with_metadata("sku", "sku-1")
            .with_metadata("available", "2")
            .with_retry_after(Duration::from_millis(1500))
            .with_field_violation("quantity", "must be positive")
            .with_field_violation("sku", "unknown")
            .with_source(std::io::Error::other("hidden cause"));
        let back = decode_error(encode_error(&error)).unwrap();
        assert_eq!(back.to_wire(), error.to_wire());
        assert!(std::error::Error::source(&back).is_none());
    }

    #[test]
    fn a_bare_error_round_trips() {
        let error = AppError::unavailable("down");
        let back = decode_error(encode_error(&error)).unwrap();
        assert_eq!(back.code(), ErrorCode::Unavailable);
        assert_eq!(back.reason(), None);
        assert_eq!(back.retry_after(), None);
        assert!(back.metadata().is_empty());
    }

    #[test]
    fn garbage_is_not_an_error() {
        assert!(decode_error(Bytes::from_static(&[0xff, 0xff, 0xff])).is_none());
    }

    #[test]
    fn an_unknown_code_decodes_as_unknown() {
        let pb = WireErrorPb {
            code: "FROM_THE_FUTURE".into(),
            message: "m".into(),
            ..WireErrorPb::default()
        };
        let back = decode_error(Bytes::from(pb.encode_to_vec())).unwrap();
        assert_eq!(back.code(), ErrorCode::Unknown);
    }

    #[tokio::test]
    async fn the_guard_yields_the_outcome() {
        let guard = AbortOnDrop(tokio::spawn(async { 7 }));
        assert_eq!(guard.await.unwrap(), 7);
    }

    #[tokio::test]
    async fn dropping_the_guard_aborts_the_task() {
        struct Signal(Option<oneshot::Sender<()>>);
        impl Drop for Signal {
            fn drop(&mut self) {
                if let Some(sender) = self.0.take() {
                    let _ = sender.send(());
                }
            }
        }

        let (started_tx, started_rx) = oneshot::channel();
        let (dropped_tx, dropped_rx) = oneshot::channel();
        let guard = AbortOnDrop(tokio::spawn(async move {
            let _signal = Signal(Some(dropped_tx));
            started_tx.send(()).unwrap();
            std::future::pending::<()>().await;
        }));
        started_rx.await.unwrap();
        drop(guard);
        dropped_rx.await.unwrap();
    }
}
