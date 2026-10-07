use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use sekvent_error::AppError;
use tokio::task::JoinHandle;
use tokio::time::{Instant, sleep_until};
use tokio_util::sync::CancellationToken;

use super::{FencingToken, valid_until};

pub(super) type BoxFut<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Shortest pause before retrying a failed renewal.
const MIN_RETRY: Duration = Duration::from_millis(100);

/// What the heartbeat needs from a lease row; a seam for tests.
pub(super) trait Renew: Send + Sync + 'static {
    /// Extend the lease by its TTL. `Ok(false)`: the row is no longer ours.
    fn renew(&self) -> BoxFut<'_, Result<bool, AppError>>;
    /// Give the lease up; a no-op when it is no longer ours.
    fn release(&self) -> BoxFut<'_, Result<(), AppError>>;
}

/// Labels for logs: never the owner.
#[derive(Debug, Clone)]
pub(super) struct Labels {
    pub(super) name: String,
    pub(super) holder: String,
    pub(super) fence: FencingToken,
}

/// Releases a row in the background when dropped while armed.
///
/// An acquisition arms it right before sending its COMMIT: from then on
/// the row may belong to an owner only this process knows, even when no
/// reply arrives (an error, or the caller dropping the future). Once the
/// acquisition hands out its lease it [`claim`](Self::claim)s the row.
pub(super) struct Unclaimed {
    row: Arc<dyn Renew>,
    name: String,
    armed: bool,
}

impl Unclaimed {
    pub(super) fn new(row: Arc<dyn Renew>, name: &str) -> Self {
        Self {
            row,
            name: name.to_owned(),
            armed: false,
        }
    }

    /// The acquiring transaction may commit from now on.
    pub(super) fn arm(&mut self) {
        self.armed = true;
    }

    /// The caller holds the lease now.
    pub(super) fn claim(mut self) {
        self.armed = false;
    }
}

impl Drop for Unclaimed {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let row = Arc::clone(&self.row);
        let name = self.name.clone();
        runtime.spawn(async move {
            if let Err(error) = row.release().await {
                tracing::debug!(
                    lease = %name,
                    code = %error.code(),
                    "releasing an unfinished acquisition failed; it expires on its own"
                );
            }
        });
    }
}

/// A lease renewed by a background task.
///
/// The task renews every `ttl / 3`, retries a failed renewal after
/// `ttl / 10`, and fires [`lost`](Self::lost) when a renewal finds the row
/// taken or when the lease's validity passes without a successful renewal;
/// a lease whose validity already passed is lost from the start.
/// Dropping it, or cancelling [`release`](Self::release) before the
/// release statement completed, stops the heartbeat and, inside a tokio
/// runtime, releases the lease in the background; otherwise the lease
/// simply expires.
pub struct HeldLease {
    renew: Arc<dyn Renew>,
    labels: Labels,
    lost: CancellationToken,
    stop: CancellationToken,
    task: Option<JoinHandle<()>>,
    released: bool,
}

impl fmt::Debug for HeldLease {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HeldLease")
            .field("name", &self.labels.name)
            .field("holder", &self.labels.holder)
            .field("fence", &self.labels.fence)
            .field("lost", &self.lost.is_cancelled())
            .finish_non_exhaustive()
    }
}

impl HeldLease {
    /// Start the heartbeat of a lease last renewed (or acquired) at
    /// `renewed_at`; one whose validity already passed starts out lost,
    /// without a heartbeat.
    ///
    /// # Panics
    ///
    /// Outside a tokio runtime.
    pub(super) fn start(
        renew: Arc<dyn Renew>,
        labels: Labels,
        ttl: Duration,
        renewed_at: Instant,
    ) -> Self {
        let lost = CancellationToken::new();
        let stop = CancellationToken::new();
        let task = if Instant::now() >= valid_until(renewed_at, ttl) {
            lose(
                &labels,
                &lost,
                "the lease's validity passed before keep_alive",
            );
            None
        } else {
            Some(tokio::spawn(heartbeat(
                Arc::clone(&renew),
                labels.clone(),
                ttl,
                renewed_at,
                stop.clone(),
                lost.clone(),
            )))
        };
        Self {
            renew,
            labels,
            lost,
            stop,
            task,
            released: false,
        }
    }

    /// The lease name.
    pub fn name(&self) -> &str {
        &self.labels.name
    }

    /// The fencing token of this acquisition.
    pub fn fence(&self) -> FencingToken {
        self.labels.fence
    }

    /// Fires when the lease is lost.
    pub fn lost(&self) -> CancellationToken {
        self.lost.clone()
    }

    /// Whether the lease is lost.
    pub fn is_lost(&self) -> bool {
        self.lost.is_cancelled()
    }

    /// Stop renewing, then release. When the release fails or this future
    /// is dropped before it completed, the lease is released in the
    /// background as if it had been dropped.
    pub async fn release(mut self) -> Result<(), AppError> {
        self.stop.cancel();
        if let Some(task) = self.task.take() {
            // The task only ends or is cancelled; neither needs handling.
            let _ = task.await;
        }
        self.renew.release().await?;
        self.released = true;
        Ok(())
    }
}

impl Drop for HeldLease {
    fn drop(&mut self) {
        self.stop.cancel();
        if self.released {
            return;
        }
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let renew = Arc::clone(&self.renew);
        let labels = self.labels.clone();
        runtime.spawn(async move {
            if let Err(error) = renew.release().await {
                tracing::debug!(
                    lease = %labels.name,
                    fence = %labels.fence,
                    holder = %labels.holder,
                    code = %error.code(),
                    "releasing a dropped lease failed; it expires on its own"
                );
            }
        });
    }
}

/// Renew until `stop` fires or the lease is lost.
async fn heartbeat(
    renew: Arc<dyn Renew>,
    labels: Labels,
    ttl: Duration,
    renewed_at: Instant,
    stop: CancellationToken,
    lost: CancellationToken,
) {
    let period = ttl / 3;
    let retry = (ttl / 10).max(MIN_RETRY);
    let mut valid = valid_until(renewed_at, ttl);
    let mut next = renewed_at + period;
    loop {
        tokio::select! {
            biased;
            () = stop.cancelled() => return,
            () = sleep_until(next.min(valid)) => {}
        }
        if Instant::now() >= valid {
            lose(
                &labels,
                &lost,
                "no renewal succeeded within the lease's validity",
            );
            return;
        }
        let sent = Instant::now();
        let outcome = tokio::select! {
            biased;
            () = stop.cancelled() => return,
            outcome = renew.renew() => outcome,
            () = sleep_until(valid) => {
                lose(&labels, &lost, "no renewal succeeded within the lease's validity");
                return;
            }
        };
        match outcome {
            Ok(true) => {
                valid = valid_until(sent, ttl);
                next = sent + period;
            }
            Ok(false) => {
                lose(&labels, &lost, "another holder took the lease");
                return;
            }
            Err(error) => {
                tracing::debug!(
                    lease = %labels.name,
                    fence = %labels.fence,
                    holder = %labels.holder,
                    code = %error.code(),
                    "lease renewal failed; retrying"
                );
                next = Instant::now() + retry;
            }
        }
    }
}

fn lose(labels: &Labels, lost: &CancellationToken, why: &'static str) {
    tracing::warn!(
        lease = %labels.name,
        fence = %labels.fence,
        holder = %labels.holder,
        why,
        "lease lost"
    );
    lost.cancel();
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::Mutex;

    use sekvent_error::ErrorCode;
    use tokio::sync::mpsc;

    use super::*;

    const TTL: Duration = Duration::from_secs(30);

    #[derive(Debug, Clone, Copy)]
    enum Step {
        Renewed,
        Gone,
        Fails,
        Hangs,
    }

    const OK: &[Step] = &[Step::Renewed];
    const FAILS: &[Step] = &[Step::Fails];

    /// Answers renewals and releases from scripts (the last step repeats;
    /// a release treats `Renewed` and `Gone` as success) and reports every
    /// call through channels.
    struct Scripted {
        steps: Mutex<VecDeque<Step>>,
        release_steps: Mutex<VecDeque<Step>>,
        renewals: mpsc::UnboundedSender<Instant>,
        releases: mpsc::UnboundedSender<Instant>,
    }

    struct Probe {
        renewals: mpsc::UnboundedReceiver<Instant>,
        releases: mpsc::UnboundedReceiver<Instant>,
    }

    fn scripted(steps: &[Step], release_steps: &[Step]) -> (Arc<Scripted>, Probe) {
        let (renew_tx, renewals) = mpsc::unbounded_channel();
        let (release_tx, releases) = mpsc::unbounded_channel();
        let script = Arc::new(Scripted {
            steps: Mutex::new(steps.iter().copied().collect()),
            release_steps: Mutex::new(release_steps.iter().copied().collect()),
            renewals: renew_tx,
            releases: release_tx,
        });
        (script, Probe { renewals, releases })
    }

    fn next(steps: &Mutex<VecDeque<Step>>) -> Step {
        let mut steps = steps.lock().unwrap();
        if steps.len() > 1 {
            steps.pop_front().unwrap()
        } else {
            *steps.front().unwrap()
        }
    }

    impl Renew for Scripted {
        fn renew(&self) -> BoxFut<'_, Result<bool, AppError>> {
            let step = next(&self.steps);
            let _ = self.renewals.send(Instant::now());
            Box::pin(async move {
                match step {
                    Step::Renewed => Ok(true),
                    Step::Gone => Ok(false),
                    Step::Fails => Err(AppError::unavailable("database unavailable")),
                    Step::Hangs => std::future::pending().await,
                }
            })
        }

        fn release(&self) -> BoxFut<'_, Result<(), AppError>> {
            let step = next(&self.release_steps);
            let _ = self.releases.send(Instant::now());
            Box::pin(async move {
                match step {
                    Step::Renewed | Step::Gone => Ok(()),
                    Step::Fails => Err(AppError::unavailable("database unavailable")),
                    Step::Hangs => std::future::pending().await,
                }
            })
        }
    }

    fn labels() -> Labels {
        Labels {
            name: "orders-sync".to_owned(),
            holder: "pid-1".to_owned(),
            fence: FencingToken::new(7),
        }
    }

    fn start(script: &Arc<Scripted>) -> (HeldLease, Instant) {
        let now = Instant::now();
        let renew: Arc<dyn Renew> = Arc::clone(script) as Arc<dyn Renew>;
        (HeldLease::start(renew, labels(), TTL, now), now)
    }

    fn secs(start: Instant, at: Instant) -> u64 {
        (at - start).as_secs()
    }

    #[tokio::test(start_paused = true)]
    async fn renews_every_third_of_the_ttl() {
        let (script, mut probe) = scripted(OK, OK);
        let (held, start) = start(&script);
        let mut at = Vec::new();
        for _ in 0..3 {
            at.push(secs(start, probe.renewals.recv().await.unwrap()));
        }
        assert_eq!(at, [10, 20, 30]);
        assert!(!held.is_lost());
        assert_eq!(held.name(), "orders-sync");
        assert_eq!(held.fence(), FencingToken::new(7));
        let debug = format!("{held:?}");
        assert!(debug.contains("orders-sync") && debug.contains("lost: false"));

        held.release().await.unwrap();
        assert_eq!(secs(start, probe.releases.recv().await.unwrap()), 30);
        // The heartbeat stopped with the release.
        tokio::time::sleep(TTL * 2).await;
        assert!(probe.renewals.try_recv().is_err());
        assert!(probe.releases.try_recv().is_err());
    }

    #[tokio::test(start_paused = true)]
    async fn failed_renewals_are_retried_after_a_tenth_of_the_ttl() {
        let (script, mut probe) = scripted(&[Step::Fails, Step::Fails, Step::Renewed], OK);
        let (held, start) = start(&script);
        let mut at = Vec::new();
        for _ in 0..4 {
            at.push(secs(start, probe.renewals.recv().await.unwrap()));
        }
        assert_eq!(at, [10, 13, 16, 26]);
        assert!(!held.is_lost());
        drop(held);
    }

    #[tokio::test(start_paused = true)]
    async fn validity_passing_without_a_renewal_fires_lost() {
        let (script, mut probe) = scripted(FAILS, OK);
        let (held, start) = start(&script);
        held.lost().cancelled().await;
        // Valid until 30 s - 3 s; the retries came at 10, 13, …, 25 s.
        assert_eq!(start.elapsed(), Duration::from_secs(27));
        let mut at = Vec::new();
        while let Ok(sent) = probe.renewals.try_recv() {
            at.push(secs(start, sent));
        }
        assert_eq!(at, [10, 13, 16, 19, 22, 25]);
        assert!(held.is_lost());
    }

    #[tokio::test(start_paused = true)]
    async fn a_hanging_renewal_loses_the_lease_at_the_end_of_validity() {
        let (script, _probe) = scripted(&[Step::Hangs], OK);
        let (held, start) = start(&script);
        held.lost().cancelled().await;
        assert_eq!(start.elapsed(), Duration::from_secs(27));
    }

    #[tokio::test(start_paused = true)]
    async fn a_renewal_that_finds_no_row_fires_lost_at_once() {
        let (script, mut probe) = scripted(&[Step::Gone], OK);
        let (held, start) = start(&script);
        held.lost().cancelled().await;
        assert_eq!(start.elapsed(), Duration::from_secs(10));
        assert_eq!(secs(start, probe.renewals.recv().await.unwrap()), 10);
        assert!(probe.renewals.try_recv().is_err());
        // A lost lease still releases (a no-op on the row) when asked.
        held.release().await.unwrap();
        assert!(probe.releases.recv().await.is_some());
    }

    #[tokio::test(start_paused = true)]
    async fn dropping_stops_the_heartbeat_and_releases_once() {
        let (script, mut probe) = scripted(OK, OK);
        let (held, start) = start(&script);
        assert_eq!(secs(start, probe.renewals.recv().await.unwrap()), 10);
        drop(held);
        assert_eq!(secs(start, probe.releases.recv().await.unwrap()), 10);
        tokio::time::sleep(TTL * 2).await;
        assert!(probe.renewals.try_recv().is_err());
        assert!(probe.releases.try_recv().is_err());
    }

    #[tokio::test(start_paused = true)]
    async fn a_failing_release_on_drop_is_only_logged() {
        let (script, mut probe) = scripted(OK, FAILS);
        let (held, _start) = start(&script);
        drop(held);
        assert!(probe.releases.recv().await.is_some());
        tokio::time::sleep(TTL).await;
        assert!(probe.releases.try_recv().is_err());
    }

    #[tokio::test(start_paused = true)]
    async fn a_failing_release_is_returned_and_retried_in_the_background() {
        let (script, mut probe) = scripted(OK, &[Step::Fails, Step::Renewed]);
        let (held, _start) = start(&script);
        let error = held.release().await.unwrap_err();
        assert_eq!(error.code(), ErrorCode::Unavailable);
        assert!(probe.releases.recv().await.is_some());
        assert!(probe.releases.recv().await.is_some());
        tokio::time::sleep(TTL).await;
        assert!(probe.releases.try_recv().is_err());
    }

    #[tokio::test(start_paused = true)]
    async fn a_release_cancelled_midway_still_releases_in_the_background() {
        let (script, mut probe) = scripted(OK, &[Step::Hangs, Step::Renewed]);
        let (held, start) = start(&script);
        let cancelled = tokio::time::timeout(Duration::from_secs(5), held.release()).await;
        assert!(cancelled.is_err());
        assert_eq!(secs(start, probe.releases.recv().await.unwrap()), 0);
        // The hanging attempt was dropped with the lease, which released again.
        assert_eq!(secs(start, probe.releases.recv().await.unwrap()), 5);
        tokio::time::sleep(TTL * 2).await;
        assert!(probe.releases.try_recv().is_err());
        assert!(probe.renewals.try_recv().is_err());
    }

    #[tokio::test(start_paused = true)]
    async fn a_lease_kept_alive_after_its_validity_is_lost_at_once() {
        let (script, mut probe) = scripted(OK, OK);
        let acquired = Instant::now();
        tokio::time::advance(Duration::from_secs(27)).await;
        let renew: Arc<dyn Renew> = Arc::clone(&script) as Arc<dyn Renew>;
        let held = HeldLease::start(renew, labels(), TTL, acquired);
        assert!(held.is_lost());
        assert!(held.lost().is_cancelled());
        // No heartbeat runs for it, but it still releases.
        tokio::time::sleep(TTL * 2).await;
        assert!(probe.renewals.try_recv().is_err());
        held.release().await.unwrap();
        assert!(probe.releases.recv().await.is_some());
    }

    #[tokio::test(start_paused = true)]
    async fn a_lease_kept_alive_just_inside_its_validity_is_held() {
        let (script, mut probe) = scripted(OK, OK);
        let acquired = Instant::now();
        tokio::time::advance(Duration::from_secs(26)).await;
        let renew: Arc<dyn Renew> = Arc::clone(&script) as Arc<dyn Renew>;
        let held = HeldLease::start(renew, labels(), TTL, acquired);
        assert!(!held.is_lost());
        // The first renewal was due at 10 s: it runs at once.
        assert_eq!(secs(acquired, probe.renewals.recv().await.unwrap()), 26);
        assert!(!held.is_lost());
        drop(held);
    }

    #[tokio::test(start_paused = true)]
    async fn an_armed_acquisition_is_released_when_dropped() {
        let (script, mut probe) = scripted(OK, OK);
        let row: Arc<dyn Renew> = Arc::clone(&script) as Arc<dyn Renew>;
        drop(Unclaimed::new(Arc::clone(&row), "orders-sync"));
        let mut claimed = Unclaimed::new(Arc::clone(&row), "orders-sync");
        claimed.arm();
        claimed.claim();
        tokio::time::sleep(TTL).await;
        assert!(probe.releases.try_recv().is_err());

        let mut armed = Unclaimed::new(row, "orders-sync");
        armed.arm();
        drop(armed);
        assert!(probe.releases.recv().await.is_some());
        tokio::time::sleep(TTL).await;
        assert!(probe.releases.try_recv().is_err());
    }

    #[tokio::test(start_paused = true)]
    async fn a_failing_release_of_an_unfinished_acquisition_is_only_logged() {
        let (script, mut probe) = scripted(OK, FAILS);
        let mut armed = Unclaimed::new(Arc::clone(&script) as Arc<dyn Renew>, "orders-sync");
        armed.arm();
        drop(armed);
        assert!(probe.releases.recv().await.is_some());
        tokio::time::sleep(TTL).await;
        assert!(probe.releases.try_recv().is_err());
    }

    #[test]
    fn an_armed_acquisition_dropped_outside_a_runtime_expires() {
        let (script, mut probe) = scripted(OK, OK);
        let mut armed = Unclaimed::new(Arc::clone(&script) as Arc<dyn Renew>, "orders-sync");
        armed.arm();
        drop(armed);
        assert!(probe.releases.try_recv().is_err());
    }

    #[test]
    fn dropping_outside_a_runtime_lets_the_lease_expire() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        let (script, mut probe) = scripted(OK, OK);
        let held = runtime.block_on(async { start(&script).0 });
        drop(held);
        assert!(probe.releases.try_recv().is_err());
        drop(runtime);
    }
}
