use std::future::Future;

use sekvent_error::AppError;

use crate::__private::BoxFuture;

/// Start and stop hooks of a component implementation.
///
/// Opt in by implementing this trait and installing the component with the
/// generated `install_with_lifecycle`. Components start in install order and
/// stop in reverse order; `on_stop` runs after the component has drained its
/// in-flight calls (or its grace period has passed).
pub trait Lifecycle: Send + Sync + 'static {
    /// Called once when the App starts, before the component accepts calls.
    /// An error aborts the start.
    fn on_start(&self) -> impl Future<Output = Result<(), AppError>> + Send {
        async { Ok(()) }
    }

    /// Called once when the App stops, after the component stopped accepting
    /// calls. An error is reported, and the other components still stop.
    fn on_stop(&self) -> impl Future<Output = Result<(), AppError>> + Send {
        async { Ok(()) }
    }
}

/// Object-safe form of [`Lifecycle`].
pub(crate) trait LifecycleDyn: Send + Sync + 'static {
    fn start(&self) -> BoxFuture<'_, Result<(), AppError>>;
    fn stop(&self) -> BoxFuture<'_, Result<(), AppError>>;
}

impl<T: Lifecycle> LifecycleDyn for T {
    fn start(&self) -> BoxFuture<'_, Result<(), AppError>> {
        Box::pin(self.on_start())
    }

    fn stop(&self) -> BoxFuture<'_, Result<(), AppError>> {
        Box::pin(self.on_stop())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Quiet;
    impl Lifecycle for Quiet {}

    #[tokio::test]
    async fn default_hooks_succeed() {
        let quiet: &dyn LifecycleDyn = &Quiet;
        quiet.start().await.unwrap();
        quiet.stop().await.unwrap();
    }
}
