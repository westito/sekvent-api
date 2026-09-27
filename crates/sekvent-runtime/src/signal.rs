//! Shutdown triggers from outside the process.
//!
//! A signal source whose handler could not be installed must never look like
//! a signal that arrived: the process would start and immediately drain
//! itself with no hint why. A failed registration is logged once and the
//! source then never resolves, leaving the other sources in charge.

use std::future::Future;
use std::io;

use futures::future::BoxFuture;

/// A future that resolves when shutdown was requested from outside.
pub(crate) type Trigger = BoxFuture<'static, ()>;

/// Resolve when `registration` reports a delivered signal; never resolve if
/// it reports that the handler could not be installed.
pub(crate) async fn signal_arrived<F>(name: &'static str, registration: F)
where
    F: Future<Output = io::Result<()>>,
{
    match registration.await {
        Ok(()) => tracing::info!(signal = name, "shutdown signal received"),
        Err(error) => {
            tracing::error!(
                signal = name,
                %error,
                "signal handler could not be installed; this source will not trigger shutdown"
            );
            std::future::pending::<()>().await;
        }
    }
}

/// `SIGTERM` and `SIGINT`, registered now so a signal sent during startup is
/// already caught.
#[cfg(unix)]
pub(crate) fn os_signals() -> Vec<Trigger> {
    use tokio::signal::unix::{SignalKind, signal};
    vec![
        unix_signal("SIGTERM", signal(SignalKind::terminate())),
        unix_signal("SIGINT", signal(SignalKind::interrupt())),
    ]
}

/// Ctrl-C, the only portable source.
#[cfg(not(unix))]
pub(crate) fn os_signals() -> Vec<Trigger> {
    let ctrl_c: Trigger = Box::pin(signal_arrived("ctrl-c", tokio::signal::ctrl_c()));
    vec![ctrl_c]
}

#[cfg(unix)]
fn unix_signal(
    name: &'static str,
    registration: io::Result<tokio::signal::unix::Signal>,
) -> Trigger {
    Box::pin(signal_arrived(name, async move {
        let mut signal = registration?;
        if signal.recv().await.is_none() {
            // A closed stream is not a delivered signal.
            std::future::pending::<()>().await;
        }
        Ok(())
    }))
}

/// Resolve when the first trigger does; never when there are none.
pub(crate) async fn any_trigger(triggers: Vec<Trigger>) {
    if triggers.is_empty() {
        std::future::pending::<()>().await;
    } else {
        futures::future::select_all(triggers).await;
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use futures::FutureExt;

    use super::*;

    #[tokio::test(start_paused = true)]
    async fn a_failed_registration_never_resolves() {
        let waited = tokio::time::timeout(
            Duration::from_secs(3_600),
            signal_arrived("TEST", async { Err(io::Error::other("refused")) }),
        )
        .await;
        assert!(
            waited.is_err(),
            "a registration failure must not end the wait"
        );
    }

    #[tokio::test]
    async fn a_delivered_signal_resolves() {
        signal_arrived("TEST", async { Ok(()) }).await;
    }

    #[tokio::test(start_paused = true)]
    async fn no_triggers_means_no_shutdown_and_any_trigger_suffices() {
        let none = tokio::time::timeout(Duration::from_secs(3_600), any_trigger(Vec::new())).await;
        assert!(none.is_err());

        let never: Trigger = Box::pin(std::future::pending());
        let now: Trigger = Box::pin(async {});
        any_trigger(vec![never, now]).await;
    }

    #[tokio::test]
    async fn os_signals_register_without_resolving() {
        let triggers = os_signals();
        assert!(!triggers.is_empty());
        assert!(any_trigger(triggers).now_or_never().is_none());
    }

    #[cfg(unix)]
    #[tokio::test(start_paused = true)]
    async fn a_refused_unix_source_never_resolves() {
        let trigger = unix_signal("SIGTEST", Err(io::Error::other("refused")));
        assert!(
            tokio::time::timeout(Duration::from_secs(3_600), trigger)
                .await
                .is_err()
        );
    }
}
