//! A trait whose messages are named `T` and `F`, the generic parameter
//! names a hand-written blanket impl or `install` would pick.

use std::time::Duration;

use sekvent_component::{App, AppError, Binding, CallContext, component};
use sekvent_config::MapSource;

#[derive(Clone, PartialEq, prost::Message)]
pub struct T {
    #[prost(string, tag = "1")]
    pub text: String,
}

impl prost::Name for T {
    const NAME: &'static str = "T";
    const PACKAGE: &'static str = "check.letters.v1";
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct F {
    #[prost(uint64, tag = "1")]
    pub length: u64,
}

impl prost::Name for F {
    const NAME: &'static str = "F";
    const PACKAGE: &'static str = "check.letters.v1";
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct Verdict {
    #[prost(bool, tag = "1")]
    pub ok: bool,
}

impl prost::Name for Verdict {
    const NAME: &'static str = "Verdict";
    const PACKAGE: &'static str = "check.letters.v1";
}

/// The contract sekvent-proto-build would generate for `check.letters.v1`.
mod proto {
    #[allow(non_upper_case_globals)]
    pub const __sekvent_service_Letters: (&str, &[(&str, &str, &str, bool)]) = (
        "check.letters.v1.Letters",
        &[
            ("Measure", "check.letters.v1.T", "check.letters.v1.F", false),
            ("Check", "check.letters.v1.F", "check.letters.v1.Verdict", false),
        ],
    );
    #[allow(non_camel_case_types, dead_code)]
    pub type __sekvent_rpc_Letters__Measure = (super::T, super::F);
    #[allow(non_camel_case_types, dead_code)]
    pub type __sekvent_rpc_Letters__Check = (super::F, super::Verdict);
}

/// Measures letters.
#[component(name = "letters", package = "check.letters.v1", proto = "crate::proto")]
pub trait Letters {
    /// The length of a text.
    #[call]
    async fn measure(&self, cx: &CallContext, req: T) -> Result<F, AppError>;

    /// Whether a length is positive.
    #[call]
    async fn check(&self, cx: &CallContext, req: F) -> Result<Verdict, AppError>;
}

struct Counter;

impl Letters for Counter {
    async fn measure(&self, _cx: &CallContext, req: T) -> Result<F, AppError> {
        Ok(F {
            length: u64::try_from(req.text.len()).unwrap_or_default(),
        })
    }

    async fn check(&self, _cx: &CallContext, req: F) -> Result<Verdict, AppError> {
        Ok(Verdict { ok: req.length > 0 })
    }
}

async fn run(source: &MapSource, binding: Binding) {
    let mut builder = App::builder(source);
    LettersHandle::install(&mut builder, |_deps| Ok(Counter)).expect("install");
    let app = builder.build().expect("build");
    app.start().await.expect("start");
    let letters = app.handle::<LettersHandle>().expect("handle");
    assert_eq!(letters.binding(), binding);

    let cx = CallContext::new();
    let length = letters
        .measure(&cx, T { text: "abc".into() })
        .await
        .expect("measure");
    assert_eq!(length.length, 3);
    assert!(letters.check(&cx, length).await.expect("check").ok);

    app.stop(Duration::from_secs(1)).await.expect("stop");
}

#[tokio::main]
async fn main() {
    run(&MapSource::new(), Binding::Local).await;
    run(
        &MapSource::new().with("SEKVENT_COMPONENT_BINDING", "local-serialized"),
        Binding::LocalSerialized,
    )
    .await;
}
