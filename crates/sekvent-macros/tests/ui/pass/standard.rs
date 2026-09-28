//! A standard component called under both local bindings.

use std::time::Duration;

use sekvent_component::{
    App, AppError, Binding, CallContext, ComponentError, ComponentHandle, ErrorCode, component,
};
use sekvent_config::MapSource;

#[derive(Clone, PartialEq, prost::Message)]
pub struct PingRequest {
    #[prost(string, tag = "1")]
    pub text: String,
}

impl prost::Name for PingRequest {
    const NAME: &'static str = "PingRequest";
    const PACKAGE: &'static str = "check.echo.v1";
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct PingReply {
    #[prost(string, tag = "1")]
    pub text: String,
}

impl prost::Name for PingReply {
    const NAME: &'static str = "PingReply";
    const PACKAGE: &'static str = "check.echo.v1";
}

/// The contract sekvent-proto-build would generate for `check.echo.v1`.
mod proto {
    #[allow(non_upper_case_globals)]
    pub const __sekvent_service_Echo: (&str, &[(&str, &str, &str, bool)]) = (
        "check.echo.v1.Echo",
        &[
            (
                "Ping",
                "check.echo.v1.PingRequest",
                "check.echo.v1.PingReply",
                false,
            ),
            (
                "Twice",
                "check.echo.v1.PingRequest",
                "check.echo.v1.PingReply",
                false,
            ),
        ],
    );
}

#[derive(Debug, ComponentError)]
#[component_error(domain = "check.echo.v1")]
pub enum EchoError {
    #[reason("EMPTY_TEXT", code = InvalidArgument)]
    EmptyText,
    #[reason("TEXT_TOO_LONG", code = InvalidArgument, message = "at most {limit} characters")]
    TooLong { limit: u32 },
    #[other]
    Other(AppError),
}

/// Echoes text back.
#[component(name = "echo", package = "check.echo.v1", proto = "crate::proto")]
pub trait Echo {
    /// Return the request text.
    #[call(idempotent, timeout = "1s", bulkhead = 4)]
    async fn ping(&self, cx: &CallContext, req: PingRequest) -> Result<PingReply, EchoError>;

    /// Return the request text twice.
    #[call]
    async fn twice(&self, cx: &CallContext, req: PingRequest) -> Result<PingReply, AppError>;
}

struct EchoService;

impl Echo for EchoService {
    async fn ping(&self, _cx: &CallContext, req: PingRequest) -> Result<PingReply, EchoError> {
        if req.text.is_empty() {
            return Err(EchoError::EmptyText);
        }
        if req.text.len() > 8 {
            return Err(EchoError::TooLong { limit: 8 });
        }
        Ok(PingReply { text: req.text })
    }

    async fn twice(&self, _cx: &CallContext, req: PingRequest) -> Result<PingReply, AppError> {
        Ok(PingReply {
            text: format!("{0}{0}", req.text),
        })
    }
}

async fn run(source: &MapSource, binding: Binding) {
    let mut builder = App::builder(source);
    EchoHandle::install(&mut builder, |_deps| Ok(EchoService)).expect("install");
    let app = builder.build().expect("build");
    app.start().await.expect("start");
    let echo = app.handle::<EchoHandle>().expect("handle");
    assert_eq!(echo.binding(), binding);
    assert_eq!(app.binding("echo"), Some(binding));

    let cx = CallContext::new();
    let reply = echo
        .ping(&cx, PingRequest { text: "hi".into() })
        .await
        .expect("ping");
    assert_eq!(reply.text, "hi");
    let reply = echo
        .twice(&cx, PingRequest { text: "ab".into() })
        .await
        .expect("twice");
    assert_eq!(reply.text, "abab");

    let error = echo
        .ping(
            &cx,
            PingRequest {
                text: String::new(),
            },
        )
        .await
        .expect_err("empty text");
    assert!(matches!(error, EchoError::EmptyText), "{error:?}");
    let error = echo
        .ping(
            &cx,
            PingRequest {
                text: "much too long".into(),
            },
        )
        .await
        .expect_err("long text");
    assert!(
        matches!(error, EchoError::TooLong { limit: 8 }),
        "{error:?}"
    );
    let error = AppError::from(error);
    assert_eq!(error.code(), ErrorCode::InvalidArgument);
    assert_eq!(error.message(), "at most 8 characters");
    assert_eq!(error.domain(), Some("check.echo.v1"));

    app.stop(Duration::from_secs(1)).await.expect("stop");
}

#[tokio::main]
async fn main() {
    let descriptor = <EchoHandle as ComponentHandle>::DESCRIPTOR;
    assert_eq!(descriptor.name(), "echo");
    assert_eq!(descriptor.service(), "Echo");
    assert_eq!(descriptor.package(), Some("check.echo.v1"));
    assert_eq!(
        descriptor.full_service_name().as_deref(),
        Some("check.echo.v1.Echo")
    );
    let [ping, twice] = descriptor.methods() else {
        panic!("two methods expected");
    };
    assert_eq!((ping.name(), ping.rpc()), ("ping", "Ping"));
    assert!(ping.is_idempotent());
    assert_eq!(ping.timeout(), Some(Duration::from_secs(1)));
    assert_eq!(ping.bulkhead(), Some(4));
    assert_eq!((twice.name(), twice.rpc()), ("twice", "Twice"));
    assert!(!twice.is_idempotent());
    assert_eq!(twice.timeout(), None);
    assert_eq!(twice.bulkhead(), None);

    run(&MapSource::new(), Binding::Local).await;
    run(
        &MapSource::new().with("SEKVENT_COMPONENT_BINDING", "local-serialized"),
        Binding::LocalSerialized,
    )
    .await;
}
