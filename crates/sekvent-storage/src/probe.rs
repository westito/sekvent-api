use std::time::Duration;

use futures::future::BoxFuture;
use sekvent_context::CallContext;
use sekvent_error::{AppError, ErrorCode};
use sekvent_runtime::{DependencyProbe, ProbeFailure, ProbeStatus};

use crate::Storage;
use crate::reasons;

/// How long one check may take. Below the runtime's default
/// `probe_timeout` (2 s), so the probe records its own `Down` first.
const CHECK_TIMEOUT: Duration = Duration::from_millis(1500);

/// Readiness probe of one bucket: [`Storage::check`] (`HEAD` on the
/// bucket) within 1.5 s.
///
/// | outcome | status |
/// |---|---|
/// | the bucket answered | `Up` |
/// | credentials, permissions, region or a missing bucket refused | `Down(Rejected("bucket access rejected"))` |
/// | no answer within 1.5 s | `Down(Unreachable("no answer in time"))` |
/// | connection failed, `502`/`503` | `Down(Unreachable("unreachable"))` |
/// | anything else | `Down(Unreachable("check failed"))` |
#[derive(Debug, Clone)]
pub struct StorageProbe {
    name: String,
    storage: Storage,
    required: bool,
}

impl StorageProbe {
    /// A required probe named `name`.
    pub fn new(name: impl Into<String>, storage: Storage) -> Self {
        Self {
            name: name.into(),
            storage,
            required: true,
        }
    }

    /// Report it without gating readiness.
    #[must_use]
    pub fn optional(mut self) -> Self {
        self.required = false;
        self
    }

    async fn check(&self) -> ProbeStatus {
        let ctx = CallContext::new().with_timeout(CHECK_TIMEOUT);
        let status = match self.storage.check(&ctx).await {
            Ok(()) => ProbeStatus::Up,
            Err(error) => ProbeStatus::Down(failure(&error)),
        };
        if let ProbeStatus::Down(failure) = status {
            tracing::debug!(
                probe = %self.name,
                kind = failure.kind(),
                detail = failure.detail(),
                "storage probe failed"
            );
        }
        status
    }
}

impl DependencyProbe for StorageProbe {
    fn name(&self) -> &str {
        &self.name
    }

    fn required(&self) -> bool {
        self.required
    }

    fn probe(&self) -> BoxFuture<'_, ProbeStatus> {
        Box::pin(self.check())
    }
}

/// The probe failure for a check error; the detail never quotes the store.
fn failure(error: &AppError) -> ProbeFailure {
    if error.reason() == Some(reasons::STORAGE_REJECTED) {
        return ProbeFailure::Rejected("bucket access rejected");
    }
    match error.code() {
        ErrorCode::DeadlineExceeded => ProbeFailure::Unreachable("no answer in time"),
        ErrorCode::Unavailable => ProbeFailure::Unreachable("unreachable"),
        _ => ProbeFailure::Unreachable("check failed"),
    }
}

#[cfg(test)]
mod tests {
    use sekvent_resilience::Policy;

    use super::*;
    use crate::test_support::{config, fake, response};

    fn probe_for(endpoint: &str) -> StorageProbe {
        let storage = Storage::builder(config(endpoint))
            .policy(Policy::new("test"))
            .build()
            .unwrap();
        StorageProbe::new("files", storage)
    }

    #[tokio::test]
    async fn a_reachable_bucket_is_up() {
        let fake = fake(Some(response("200 OK", "", ""))).await;
        let probe = probe_for(&fake.endpoint);
        assert_eq!(probe.name(), "files");
        assert!(probe.required());
        assert!(!probe.clone().optional().required());
        assert_eq!(probe.probe().await, ProbeStatus::Up);
    }

    #[tokio::test]
    async fn refusals_and_outages_are_down() {
        let forbidden = fake(Some(response("403 Forbidden", "", ""))).await;
        assert_eq!(
            probe_for(&forbidden.endpoint).probe().await,
            ProbeStatus::Down(ProbeFailure::Rejected("bucket access rejected"))
        );
        let missing = fake(Some(response("404 Not Found", "", ""))).await;
        assert_eq!(
            probe_for(&missing.endpoint).probe().await,
            ProbeStatus::Down(ProbeFailure::Rejected("bucket access rejected"))
        );
        assert_eq!(
            probe_for("http://127.0.0.1:1").probe().await,
            ProbeStatus::Down(ProbeFailure::Unreachable("unreachable"))
        );
        let broken = fake(Some(response("500 Internal Server Error", "", ""))).await;
        assert_eq!(
            probe_for(&broken.endpoint).probe().await,
            ProbeStatus::Down(ProbeFailure::Unreachable("check failed"))
        );
    }

    #[tokio::test]
    async fn a_silent_store_is_down_after_the_check_timeout() {
        let silent = fake(None).await;
        let status =
            tokio::time::timeout(Duration::from_secs(30), probe_for(&silent.endpoint).probe())
                .await
                .unwrap();
        assert_eq!(
            status,
            ProbeStatus::Down(ProbeFailure::Unreachable("no answer in time"))
        );
    }
}
