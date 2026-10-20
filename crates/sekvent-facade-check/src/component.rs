//! A standard component (`echo`) and a `local_only` one (`notes`), declared
//! through the facade alone, each with a unary and a server-streaming
//! method. `echo`'s contract is the hand-written [`proto`] module, shaped
//! like sekvent-proto-build's output.

use sekvent::component::BuildError;
use sekvent::config::ConfigSource;
use sekvent::prelude::*;

/// What sekvent-proto-build generates for the `check.echo.v1` package,
/// reduced to the contract of its `Echo` service.
pub mod proto {
    /// Contract of `check.echo.v1.Echo`, checked by `#[component(proto = …)]`.
    #[doc(hidden)]
    #[allow(non_upper_case_globals, dead_code, clippy::type_complexity)]
    pub const __sekvent_service_Echo: (&str, &[(&str, &str, &str, bool, bool)]) = (
        "check.echo.v1.Echo",
        &[
            (
                "Ping",
                "check.echo.v1.PingRequest",
                "check.echo.v1.PingReply",
                false,
                false,
            ),
            (
                "Pings",
                "check.echo.v1.PingRequest",
                "check.echo.v1.PingReply",
                false,
                true,
            ),
        ],
    );
    /// Request and reply of `check.echo.v1.Echo.Ping`, checked by `#[component(proto = …)]`.
    #[doc(hidden)]
    #[allow(non_camel_case_types, dead_code)]
    pub type __sekvent_rpc_Echo__Ping = (super::PingRequest, super::PingReply);
    /// Request and reply of `check.echo.v1.Echo.Pings`, checked by `#[component(proto = …)]`.
    #[doc(hidden)]
    #[allow(non_camel_case_types, dead_code)]
    pub type __sekvent_rpc_Echo__Pings = (super::PingRequest, super::PingReply);
}

/// Text to echo.
#[derive(Clone, PartialEq, prost::Message)]
pub struct PingRequest {
    /// The text.
    #[prost(string, tag = "1")]
    pub text: String,
}

impl prost::Name for PingRequest {
    const NAME: &'static str = "PingRequest";
    const PACKAGE: &'static str = "check.echo.v1";
}

/// The echoed text.
#[derive(Clone, PartialEq, prost::Message)]
pub struct PingReply {
    /// The text of the request.
    #[prost(string, tag = "1")]
    pub text: String,
}

impl prost::Name for PingReply {
    const NAME: &'static str = "PingReply";
    const PACKAGE: &'static str = "check.echo.v1";
}

/// What [`Echo::ping`] and [`Echo::pings`] can fail with.
#[derive(Debug, sekvent::ComponentError)]
pub enum EchoError {
    /// The request had no text, or a word of it was empty.
    #[reason("EMPTY_TEXT", code = InvalidArgument)]
    EmptyText,
    /// Any other error.
    #[other]
    Other(AppError),
}

/// Echoes text back.
#[sekvent::component(
    name = "echo",
    package = "check.echo.v1",
    proto = "crate::component::proto"
)]
pub trait Echo: Send + Sync + 'static {
    /// The request's text, unchanged; open to end users with or without
    /// credentials when served to them.
    #[call(timeout = "1s", optional_end_user)]
    async fn ping(&self, cx: &CallContext, req: PingRequest) -> Result<PingReply, EchoError>;

    /// Each space-separated word of the request's text; an empty word ends
    /// the stream with [`EchoError::EmptyText`].
    #[call(stream, timeout = "1s", max_lifetime = "5m")]
    async fn pings(
        &self,
        cx: &CallContext,
        req: PingRequest,
    ) -> Result<CallStream<PingReply, EchoError>, EchoError>;
}

/// Counts and spells the characters of a note; takes plain Rust values.
#[sekvent::component(name = "notes", local_only)]
pub trait Notes: Send + Sync + 'static {
    /// The number of characters in the note.
    #[call]
    async fn add(&self, cx: &CallContext, req: String) -> Result<usize, AppError>;

    /// The characters of the note, one by one; an empty note is refused.
    #[call(stream)]
    async fn letters(
        &self,
        cx: &CallContext,
        req: String,
    ) -> Result<CallStream<char, AppError>, AppError>;
}

/// The [`Echo`] implementation.
#[derive(Debug, Default)]
pub struct EchoService;

impl Echo for EchoService {
    async fn ping(&self, _cx: &CallContext, req: PingRequest) -> Result<PingReply, EchoError> {
        if req.text.is_empty() {
            return Err(EchoError::EmptyText);
        }
        Ok(PingReply { text: req.text })
    }

    async fn pings(
        &self,
        _cx: &CallContext,
        req: PingRequest,
    ) -> Result<CallStream<PingReply, EchoError>, EchoError> {
        if req.text.is_empty() {
            return Err(EchoError::EmptyText);
        }
        let words: Vec<Result<PingReply, EchoError>> = req
            .text
            .split(' ')
            .map(|word| {
                if word.is_empty() {
                    Err(EchoError::EmptyText)
                } else {
                    Ok(PingReply {
                        text: word.to_owned(),
                    })
                }
            })
            .collect();
        Ok(CallStream::iter(words))
    }
}

/// The [`Notes`] implementation.
#[derive(Debug, Default)]
pub struct NotesService;

impl Notes for NotesService {
    async fn add(&self, _cx: &CallContext, req: String) -> Result<usize, AppError> {
        Ok(req.chars().count())
    }

    async fn letters(
        &self,
        _cx: &CallContext,
        req: String,
    ) -> Result<CallStream<char, AppError>, AppError> {
        if req.is_empty() {
            return Err(AppError::invalid_argument("the note is empty"));
        }
        let letters: Vec<Result<char, AppError>> = req.chars().map(Ok).collect();
        Ok(CallStream::iter(letters))
    }
}

/// An App with `echo` and `notes` installed, configured from `source`.
pub fn app(source: &dyn ConfigSource) -> Result<App, BuildError> {
    let mut builder = App::builder(source);
    EchoHandle::install(&mut builder, |_deps| Ok(EchoService))?;
    NotesHandle::install(&mut builder, |_deps| Ok(NotesService))?;
    builder.build()
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use sekvent::component::Binding;
    use sekvent::component::reasons;
    use sekvent::config::MapSource;

    use super::*;

    const INBOUND_TOKEN: &str = "orders-inbound-token-0123456789abcdef";
    const OUTBOUND_TOKEN: &str = "echo-outbound-token-0123456789abcdefg";

    fn bound(binding: Binding) -> MapSource {
        MapSource::new().with("SEKVENT_COMPONENT_BINDING", binding.as_str())
    }

    /// An `http://` endpoint on which nothing listens.
    fn closed_endpoint() -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        format!("http://127.0.0.1:{port}")
    }

    #[tokio::test]
    async fn both_components_answer_under_both_bindings() {
        for binding in [Binding::Local, Binding::LocalSerialized] {
            let built = app(&bound(binding)).unwrap();
            built.start().await.unwrap();
            let cx = CallContext::new();

            let echo = built.handle::<EchoHandle>().unwrap();
            assert_eq!(echo.binding(), binding);
            let request = PingRequest {
                text: "hello".to_owned(),
            };
            let reply = echo.ping(&cx, request).await.unwrap();
            assert_eq!(reply.text, "hello");
            let error = echo.ping(&cx, PingRequest::default()).await.unwrap_err();
            assert!(
                matches!(error, EchoError::EmptyText),
                "{binding}: {error:?}"
            );

            let notes = built.handle::<NotesHandle>().unwrap();
            assert_eq!(notes.binding(), Binding::Local);
            assert_eq!(notes.add(&cx, "héllo".to_owned()).await.unwrap(), 5);

            built.stop(Duration::from_secs(1)).await.unwrap();
        }
    }

    /// Every item of `stream` up to its end, and the error that ended it.
    async fn drain<T, E>(mut stream: CallStream<T, E>) -> (Vec<T>, Option<E>) {
        let mut items = Vec::new();
        while let Some(item) = stream.next().await {
            match item {
                Ok(item) => items.push(item),
                Err(error) => {
                    assert!(stream.next().await.is_none(), "a stream ends at its error");
                    return (items, Some(error));
                }
            }
        }
        (items, None)
    }

    #[tokio::test]
    async fn both_components_stream_under_both_bindings() {
        for binding in [Binding::Local, Binding::LocalSerialized] {
            let built = app(&bound(binding)).unwrap();
            built.start().await.unwrap();
            let cx = CallContext::new();
            let echo = built.handle::<EchoHandle>().unwrap();
            let words = |text: &str| PingRequest {
                text: text.to_owned(),
            };

            let stream = echo.pings(&cx, words("to be")).await.unwrap();
            let (items, end) = drain(stream).await;
            let texts: Vec<String> = items.into_iter().map(|reply| reply.text).collect();
            assert_eq!(texts, ["to", "be"], "{binding}");
            assert!(end.is_none(), "{binding}: {end:?}");

            let stream = echo.pings(&cx, words("to  be")).await.unwrap();
            let (items, end) = drain(stream).await;
            assert_eq!(items.len(), 1, "{binding}");
            assert!(
                matches!(end, Some(EchoError::EmptyText)),
                "{binding}: {end:?}"
            );

            let error = echo.pings(&cx, words("")).await.unwrap_err();
            assert!(
                matches!(error, EchoError::EmptyText),
                "{binding}: {error:?}"
            );

            let notes = built.handle::<NotesHandle>().unwrap();
            let stream = notes.letters(&cx, "hé".to_owned()).await.unwrap();
            assert_eq!(drain(stream).await.0, ['h', 'é'], "{binding}");
            let error = notes.letters(&cx, String::new()).await.unwrap_err();
            assert_eq!(error.code(), ErrorCode::InvalidArgument, "{binding}");

            built.stop(Duration::from_secs(1)).await.unwrap();
        }
    }

    #[test]
    fn the_call_options_reach_the_descriptor() {
        use sekvent::component::ComponentHandle;

        let ping = EchoHandle::DESCRIPTOR.methods()[0];
        assert_eq!(ping.timeout(), Some(Duration::from_secs(1)));
        assert!(ping.is_optional_end_user());
        assert!(!ping.is_anonymous());
        assert!(!ping.is_stream());
        assert_eq!(ping.max_lifetime(), None);

        let pings = EchoHandle::DESCRIPTOR.methods()[1];
        assert_eq!(pings.rpc(), "Pings");
        assert!(pings.is_stream());
        assert_eq!(pings.timeout(), Some(Duration::from_secs(1)));
        assert_eq!(pings.max_lifetime(), Some(Duration::from_secs(300)));

        let letters = NotesHandle::DESCRIPTOR.methods()[1];
        assert!(letters.is_stream());
        assert_eq!(letters.max_lifetime(), None);
    }

    #[test]
    fn the_error_travels_as_an_app_error() {
        let error = AppError::from(EchoError::EmptyText);
        assert_eq!(error.code(), ErrorCode::InvalidArgument);
        assert_eq!(error.reason(), Some("EMPTY_TEXT"));
        assert!(matches!(EchoError::from(error), EchoError::EmptyText));

        let other = EchoError::from(AppError::unavailable("down"));
        let EchoError::Other(other) = other else {
            panic!("expected Other, got {other:?}");
        };
        assert_eq!(other.code(), ErrorCode::Unavailable);
    }

    #[test]
    fn a_grpc_binding_needs_an_endpoint_and_a_token() {
        let source = MapSource::new().with("SEKVENT_COMPONENT_ECHO_BINDING", "grpc");
        let text = app(&source).unwrap_err().to_string();
        assert!(text.contains("SEKVENT_COMPONENT_ECHO_ENDPOINT"), "{text}");

        let source = source.with("SEKVENT_COMPONENT_ECHO_ENDPOINT", "http://127.0.0.1:50051");
        let text = app(&source).unwrap_err().to_string();
        assert!(text.contains("SEKVENT_LINK_OUTBOUND_ECHO"), "{text}");
        assert!(!text.contains("50051"), "{text}");
    }

    #[tokio::test]
    async fn a_grpc_bound_component_reports_an_unreachable_endpoint() {
        let source = MapSource::new()
            .with("SEKVENT_COMPONENT_ECHO_BINDING", "grpc")
            .with("SEKVENT_COMPONENT_ECHO_ENDPOINT", closed_endpoint())
            .with("SEKVENT_LINK_OUTBOUND_ECHO", OUTBOUND_TOKEN);
        let built = app(&source).unwrap();
        built.start().await.unwrap();
        let echo = built.handle::<EchoHandle>().unwrap();
        assert_eq!(echo.binding(), Binding::Grpc);

        let request = PingRequest {
            text: "hello".to_owned(),
        };
        let error = tokio::time::timeout(
            Duration::from_secs(30),
            echo.ping(&CallContext::new(), request),
        )
        .await
        .expect("the call finishes")
        .unwrap_err();
        let EchoError::Other(error) = error else {
            panic!("expected Other, got {error:?}");
        };
        assert_eq!(error.code(), ErrorCode::Unavailable);
        assert_eq!(error.reason(), Some(reasons::UNREACHABLE));

        built.stop(Duration::from_secs(1)).await.unwrap();
    }

    #[tokio::test]
    async fn an_exposed_component_starts_once_its_routes_are_mounted() {
        let source = MapSource::new()
            .with("SEKVENT_COMPONENT_ECHO_SERVE", "grpc")
            .with("SEKVENT_LINK_INBOUND_ORDERS", INBOUND_TOKEN);

        let unmounted = app(&source).unwrap();
        assert_eq!(unmounted.grpc_services(), ["check.echo.v1.Echo"]);
        let error = unmounted.start().await.unwrap_err();
        assert_eq!(error.reason(), Some(reasons::GRPC_NOT_MOUNTED));

        let mounted = app(&source).unwrap();
        let _routes = mounted.grpc_routes();
        mounted.start().await.unwrap();
        mounted.stop(Duration::from_secs(1)).await.unwrap();
    }

    #[test]
    fn a_local_only_component_cannot_be_served() {
        let source = MapSource::new()
            .with("SEKVENT_COMPONENT_NOTES_SERVE", "grpc")
            .with("SEKVENT_LINK_INBOUND_ORDERS", INBOUND_TOKEN);
        let error = app(&source).unwrap_err();
        let text = error.to_string();
        assert!(
            matches!(
                error,
                BuildError::NotServable { .. } | BuildError::Multiple(_)
            ),
            "{text}"
        );
        assert!(text.contains("SEKVENT_COMPONENT_NOTES_SERVE"), "{text}");
    }
}
