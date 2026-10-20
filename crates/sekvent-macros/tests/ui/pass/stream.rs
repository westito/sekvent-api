//! A standard component with a server-streaming method, opened and read
//! under both local bindings.

use std::time::Duration;

use sekvent_component::{
    App, AppError, Binding, CallContext, CallStream, ComponentError, ComponentHandle, ErrorCode,
    component,
};
use sekvent_config::MapSource;

#[derive(Clone, PartialEq, prost::Message)]
pub struct CountRequest {
    #[prost(uint32, tag = "1")]
    pub from: u32,
}

impl prost::Name for CountRequest {
    const NAME: &'static str = "CountRequest";
    const PACKAGE: &'static str = "check.countdown.v1";
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct Tick {
    #[prost(uint32, tag = "1")]
    pub value: u32,
}

impl prost::Name for Tick {
    const NAME: &'static str = "Tick";
    const PACKAGE: &'static str = "check.countdown.v1";
}

/// The contract sekvent-proto-build would generate for `check.countdown.v1`.
mod proto {
    #[allow(non_upper_case_globals)]
    pub const __sekvent_service_Countdown: (&str, &[(&str, &str, &str, bool, bool)]) = (
        "check.countdown.v1.Countdown",
        &[
            (
                "Count",
                "check.countdown.v1.CountRequest",
                "check.countdown.v1.Tick",
                false,
                true,
            ),
            (
                "Last",
                "check.countdown.v1.CountRequest",
                "check.countdown.v1.Tick",
                false,
                false,
            ),
        ],
    );
    #[allow(non_camel_case_types, dead_code)]
    pub type __sekvent_rpc_Countdown__Count = (super::CountRequest, super::Tick);
    #[allow(non_camel_case_types, dead_code)]
    pub type __sekvent_rpc_Countdown__Last = (super::CountRequest, super::Tick);
}

/// Why a countdown cannot start.
#[derive(Debug, ComponentError)]
#[component_error(domain = "check.countdown.v1")]
pub enum CountError {
    #[reason("NOTHING_TO_COUNT", code = InvalidArgument)]
    Zero,
    #[other]
    Other(AppError),
}

/// Why a running countdown stopped.
#[derive(Debug, ComponentError)]
#[component_error(domain = "check.countdown.v1")]
pub enum TickError {
    #[reason("COUNT_TOO_HIGH", code = OutOfRange, message = "counts at most {limit}")]
    TooHigh { limit: u32 },
    #[other]
    Other(AppError),
}

/// Counts down.
#[component(
    name = "countdown",
    package = "check.countdown.v1",
    proto = "crate::proto"
)]
pub trait Countdown {
    /// `from`, `from - 1`, ..., `1`; above 3 the stream ends after the first
    /// tick with `TooHigh`.
    #[call(stream, idempotent, timeout = "1s", bulkhead = 2, max_lifetime = "10m")]
    async fn count(
        &self,
        cx: &CallContext,
        req: CountRequest,
    ) -> Result<CallStream<Tick, TickError>, CountError>;

    /// The last tick of a countdown.
    #[call]
    async fn last(&self, cx: &CallContext, req: CountRequest) -> Result<Tick, CountError>;
}

struct CountdownService;

impl Countdown for CountdownService {
    async fn count(
        &self,
        _cx: &CallContext,
        req: CountRequest,
    ) -> Result<CallStream<Tick, TickError>, CountError> {
        if req.from == 0 {
            return Err(CountError::Zero);
        }
        let (sender, receiver) = tokio::sync::mpsc::channel(4);
        tokio::spawn(async move {
            for value in (1..=req.from).rev() {
                let item = if value > 3 && value < req.from {
                    Err(TickError::TooHigh { limit: 3 })
                } else {
                    Ok(Tick { value })
                };
                if sender.send(item).await.is_err() {
                    return;
                }
            }
        });
        Ok(CallStream::from_receiver(receiver))
    }

    async fn last(&self, _cx: &CallContext, req: CountRequest) -> Result<Tick, CountError> {
        if req.from == 0 {
            return Err(CountError::Zero);
        }
        Ok(Tick { value: 1 })
    }
}

async fn run(source: &MapSource, binding: Binding) {
    let mut builder = App::builder(source);
    CountdownHandle::install(&mut builder, |_deps| Ok(CountdownService)).expect("install");
    let app = builder.build().expect("build");
    app.start().await.expect("start");
    let countdown = app.handle::<CountdownHandle>().expect("handle");
    assert_eq!(countdown.binding(), binding);
    let cx = CallContext::new();

    let mut ticks = countdown
        .count(&cx, CountRequest { from: 3 })
        .await
        .expect("open");
    let mut values = Vec::new();
    while let Some(tick) = ticks.next().await {
        values.push(tick.expect("tick").value);
    }
    assert_eq!(values, [3, 2, 1]);

    let mut ticks = countdown
        .count(&cx, CountRequest { from: 5 })
        .await
        .expect("open");
    assert_eq!(
        ticks.next().await.map(|tick| tick.expect("tick").value),
        Some(5)
    );
    let error = ticks.next().await.expect("an end").expect_err("too high");
    assert!(
        matches!(error, TickError::TooHigh { limit: 3 }),
        "{error:?}"
    );
    assert!(ticks.next().await.is_none(), "a stream ends at its error");
    let error = AppError::from(error);
    assert_eq!(error.code(), ErrorCode::OutOfRange);
    assert_eq!(error.message(), "counts at most 3");

    let error = countdown
        .count(&cx, CountRequest { from: 0 })
        .await
        .expect_err("nothing to count");
    assert!(matches!(error, CountError::Zero), "{error:?}");

    let last = countdown
        .last(&cx, CountRequest { from: 2 })
        .await
        .expect("last");
    assert_eq!(last.value, 1);

    app.stop(Duration::from_secs(1)).await.expect("stop");
}

#[tokio::main]
async fn main() {
    let descriptor = <CountdownHandle as ComponentHandle>::DESCRIPTOR;
    let [count, last] = descriptor.methods() else {
        panic!("two methods expected");
    };
    assert_eq!((count.name(), count.rpc()), ("count", "Count"));
    assert!(count.is_stream());
    assert!(count.is_idempotent());
    assert_eq!(count.timeout(), Some(Duration::from_secs(1)));
    assert_eq!(count.bulkhead(), Some(2));
    assert_eq!(count.max_lifetime(), Some(Duration::from_secs(600)));
    assert_eq!((last.name(), last.rpc()), ("last", "Last"));
    assert!(!last.is_stream());
    assert_eq!(last.max_lifetime(), None);

    run(&MapSource::new(), Binding::Local).await;
    run(
        &MapSource::new().with("SEKVENT_COMPONENT_BINDING", "local-serialized"),
        Binding::LocalSerialized,
    )
    .await;
}
