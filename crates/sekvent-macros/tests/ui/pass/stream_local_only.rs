//! A local_only component streaming plain Rust values.

use std::time::Duration;

use sekvent_component::{
    App, AppError, Binding, CallContext, CallStream, ComponentHandle, ComponentMode, ErrorCode,
    component,
};
use sekvent_config::MapSource;

/// Splits text into lines.
#[component(name = "feed", local_only)]
pub trait Feed: Send + Sync + 'static {
    /// The lines of the text, with their numbers.
    #[call(stream, max_lifetime = "30s")]
    async fn lines(
        &self,
        cx: &CallContext,
        req: String,
    ) -> Result<CallStream<(usize, String), AppError>, AppError>;
}

struct FeedService;

impl Feed for FeedService {
    async fn lines(
        &self,
        _cx: &CallContext,
        req: String,
    ) -> Result<CallStream<(usize, String), AppError>, AppError> {
        if req.is_empty() {
            return Err(AppError::invalid_argument("no text"));
        }
        let lines: Vec<Result<(usize, String), AppError>> = req
            .lines()
            .enumerate()
            .map(|(number, line)| Ok((number + 1, line.to_owned())))
            .collect();
        Ok(CallStream::iter(lines))
    }
}

#[tokio::main]
async fn main() {
    let descriptor = <FeedHandle as ComponentHandle>::DESCRIPTOR;
    assert_eq!(descriptor.mode(), ComponentMode::LocalOnly);
    let [lines] = descriptor.methods() else {
        panic!("one method expected");
    };
    assert!(lines.is_stream());
    assert_eq!(lines.max_lifetime(), Some(Duration::from_secs(30)));

    // The default binding key does not apply to local_only components.
    let source = MapSource::new().with("SEKVENT_COMPONENT_BINDING", "local-serialized");
    let mut builder = App::builder(&source);
    FeedHandle::install(&mut builder, |_deps| Ok(FeedService)).expect("install");
    let app = builder.build().expect("build");
    app.start().await.expect("start");

    let feed = app.handle::<FeedHandle>().expect("handle");
    assert_eq!(feed.binding(), Binding::Local);
    let cx = CallContext::new();
    let mut lines = feed.lines(&cx, "one\ntwo".to_owned()).await.expect("open");
    let mut seen = Vec::new();
    while let Some(line) = lines.next().await {
        seen.push(line.expect("line"));
    }
    assert_eq!(seen, [(1, "one".to_owned()), (2, "two".to_owned())]);

    let error = feed.lines(&cx, String::new()).await.expect_err("no text");
    assert_eq!(error.code(), ErrorCode::InvalidArgument);

    app.stop(Duration::from_secs(1)).await.expect("stop");
}
