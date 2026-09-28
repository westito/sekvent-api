//! A local_only component with plain Rust requests and replies.

use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use sekvent_component::{
    App, AppError, Binding, CallContext, ComponentHandle, ComponentMode, Lifecycle, component,
};
use sekvent_config::MapSource;

/// Keeps notes in memory.
#[component(name = "notes", local_only)]
pub trait Notes: Send + Sync + 'static {
    /// Store a note and return how many are stored.
    #[call(bulkhead = 2)]
    async fn add(&self, cx: &CallContext, req: String) -> Result<usize, AppError>;

    /// The stored notes.
    #[call(idempotent)]
    async fn list(&self, _: &CallContext, _: ()) -> Result<Vec<String>, AppError>;
}

#[derive(Default)]
struct NotesService {
    notes: Mutex<Vec<String>>,
}

impl Notes for NotesService {
    async fn add(&self, _cx: &CallContext, req: String) -> Result<usize, AppError> {
        let mut notes = self.notes.lock().unwrap_or_else(PoisonError::into_inner);
        notes.push(req);
        Ok(notes.len())
    }

    async fn list(&self, _cx: &CallContext, _req: ()) -> Result<Vec<String>, AppError> {
        Ok(self
            .notes
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone())
    }
}

impl Lifecycle for NotesService {}

#[tokio::main]
async fn main() {
    let descriptor = <NotesHandle as ComponentHandle>::DESCRIPTOR;
    assert_eq!(descriptor.mode(), ComponentMode::LocalOnly);
    assert_eq!(descriptor.package(), None);

    // The default binding key does not apply to local_only components.
    let source = MapSource::new().with("SEKVENT_COMPONENT_BINDING", "local-serialized");
    let mut builder = App::builder(&source);
    NotesHandle::install_with_lifecycle(&mut builder, |deps| {
        assert_eq!(deps.component(), "notes");
        Ok(NotesService::default())
    })
    .expect("install");
    let app = builder.build().expect("build");
    app.start().await.expect("start");

    let notes = app.handle::<NotesHandle>().expect("handle");
    assert_eq!(notes.binding(), Binding::Local);
    let cx = CallContext::new();
    assert_eq!(notes.add(&cx, "first".to_owned()).await.expect("add"), 1);
    assert_eq!(notes.add(&cx, "second".to_owned()).await.expect("add"), 2);
    assert_eq!(
        notes.list(&cx, ()).await.expect("list"),
        ["first", "second"]
    );

    app.stop(Duration::from_secs(1)).await.expect("stop");
}
