//! S3-compatible object storage: presigned uploads and downloads for
//! browsers, and the server-side calls an upload flow needs.
//!
//! The browser never sends file bytes through the service. The service
//! presigns a short-lived URL ([`Storage::presign_put`]), the browser
//! uploads to the store directly, and the service then checks what arrived
//! ([`Storage::head`]) before recording it. Downloads work the same way
//! with [`Storage::presign_get`].
//!
//! ```no_run
//! # async fn demo() -> Result<(), sekvent_error::AppError> {
//! use sekvent_config::EnvSource;
//! use sekvent_context::CallContext;
//! use sekvent_storage::{ObjectKey, PresignGet, PresignPut, Storage};
//!
//! // STORAGE_ENDPOINT, STORAGE_REGION, STORAGE_BUCKET, STORAGE_ACCESS_KEY_ID, …
//! let storage = Storage::from_config(&EnvSource, "STORAGE_")?;
//! let key = ObjectKey::new("attachments/42/0190b6b2-7c1e-7d4f-9a51-1b2c3d4e5f60")?;
//!
//! let upload = storage.presign_put(&key, PresignPut::new("image/png").content_length(5120))?;
//! // Send `upload.url` and `upload.headers` to the browser; it PUTs the file.
//!
//! let ctx = CallContext::new();
//! if let Some(info) = storage.head(&ctx, &key).await? {
//!     assert_eq!(info.size, 5120);
//! }
//! let download = storage.presign_get(&key, PresignGet::new().attachment("diagram.png"))?;
//! # let _ = download;
//! # Ok(())
//! # }
//! ```
//!
//! Requests are signed with AWS Signature Version 4, implemented here on
//! `hmac` and `sha2`, and sent with `reqwest` on the workspace's rustls
//! and ring stack; the AWS SDK is not used. Signing time comes from an
//! injected [`Clock`](sekvent_context::Clock).
//!
//! | Feature | Adds |
//! |---|---|
//! | `runtime` | `StorageProbe`, a readiness probe for `sekvent-runtime` |
//!
//! Secrets never reach `Debug` output, logs or error messages. Error
//! messages carry no URL, key or bucket; a store's error body is never
//! kept (only its S3 error code, in the internal source chain).

#![forbid(unsafe_code)]

mod config;
mod error;
mod key;
mod presign;
#[cfg(feature = "runtime")]
mod probe;
mod sigv4;
mod storage;
#[cfg(test)]
mod test_support;

pub use config::StorageConfig;
pub use error::reasons;
pub use key::{MAX_KEY_LEN, ObjectKey};
pub use presign::{
    DEFAULT_EXPIRY, MAX_EXPIRY, PresignGet, PresignPut, PresignedRequest, attachment_disposition,
};
#[cfg(feature = "runtime")]
pub use probe::StorageProbe;
pub use storage::{ObjectInfo, Storage, StorageBuilder};
