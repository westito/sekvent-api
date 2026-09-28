//! A standard component (`echo`) and a `local_only` one (`notes`), declared
//! through the facade alone.

use sekvent::component::BuildError;
use sekvent::config::ConfigSource;
use sekvent::prelude::*;

/// Text to echo.
#[derive(Clone, PartialEq, prost::Message)]
pub struct PingRequest {
    /// The text.
    #[prost(string, tag = "1")]
    pub text: String,
}

/// The echoed text.
#[derive(Clone, PartialEq, prost::Message)]
pub struct PingReply {
    /// The text of the request.
    #[prost(string, tag = "1")]
    pub text: String,
}

/// What [`Echo::ping`] can fail with.
#[derive(Debug, sekvent::ComponentError)]
pub enum EchoError {
    /// The request had no text.
    #[reason("EMPTY_TEXT", code = InvalidArgument)]
    EmptyText,
    /// Any other error.
    #[other]
    Other(AppError),
}

/// Echoes text back.
#[sekvent::component(name = "echo", package = "check.echo.v1")]
pub trait Echo: Send + Sync + 'static {
    /// The request's text, unchanged.
    #[call(timeout = "1s")]
    async fn ping(&self, cx: &CallContext, req: PingRequest) -> Result<PingReply, EchoError>;
}

/// Counts the characters of a note; takes plain Rust values.
#[sekvent::component(name = "notes", local_only)]
pub trait Notes: Send + Sync + 'static {
    /// The number of characters in the note.
    #[call]
    async fn add(&self, cx: &CallContext, req: String) -> Result<usize, AppError>;
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
}

/// The [`Notes`] implementation.
#[derive(Debug, Default)]
pub struct NotesService;

impl Notes for NotesService {
    async fn add(&self, _cx: &CallContext, req: String) -> Result<usize, AppError> {
        Ok(req.chars().count())
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
    use sekvent::config::MapSource;

    use super::*;

    fn bound(binding: Binding) -> MapSource {
        MapSource::new().with("SEKVENT_COMPONENT_BINDING", binding.as_str())
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
    fn an_unavailable_binding_fails_the_build() {
        let error = app(&bound(Binding::Grpc)).unwrap_err();
        assert!(
            matches!(error, BuildError::BindingUnavailable { .. }),
            "{error}"
        );
    }
}
