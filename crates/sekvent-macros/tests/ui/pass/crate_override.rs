//! `crate = "..."` names the runtime when it is reachable under another name.

extern crate sekvent_component as fw;

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use fw::{App, AppError, CallContext, ComponentError, component};

#[derive(Debug, ComponentError)]
#[component_error(crate = "::fw", domain = "check.counter.v1")]
pub enum CounterError {
    #[reason("COUNTER_OVERFLOW", code = OutOfRange, message = "the counter stops at {limit}")]
    Overflow { limit: u64 },
    #[other]
    Other(AppError),
}

/// Counts upwards.
#[component(name = "counter", local_only, crate = "::fw")]
pub trait Counter {
    /// Add to the counter and return the new value.
    #[call(timeout = "1s")]
    async fn add(&self, cx: &CallContext, req: u64) -> Result<u64, CounterError>;
}

struct CounterService {
    value: AtomicU64,
}

impl Counter for CounterService {
    async fn add(&self, _cx: &CallContext, req: u64) -> Result<u64, CounterError> {
        let next = self
            .value
            .load(Ordering::SeqCst)
            .checked_add(req)
            .filter(|next| *next <= 10)
            .ok_or(CounterError::Overflow { limit: 10 })?;
        self.value.store(next, Ordering::SeqCst);
        Ok(next)
    }
}

#[tokio::main]
async fn main() {
    let source = sekvent_config::MapSource::new();
    let mut builder = App::builder(&source);
    CounterHandle::install(&mut builder, |_deps| {
        Ok(CounterService {
            value: AtomicU64::new(0),
        })
    })
    .expect("install");
    let app = builder.build().expect("build");
    app.start().await.expect("start");

    let counter = app.handle::<CounterHandle>().expect("handle");
    let cx = CallContext::new();
    assert_eq!(counter.add(&cx, 4).await.expect("add"), 4);
    let error = counter.add(&cx, 7).await.expect_err("overflow");
    assert!(
        matches!(error, CounterError::Overflow { limit: 10 }),
        "{error:?}"
    );
    assert_eq!(AppError::from(error).message(), "the counter stops at 10");

    app.stop(Duration::from_secs(1)).await.expect("stop");
}
