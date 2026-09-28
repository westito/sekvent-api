//! `()` is `google.protobuf.Empty`, as a reply and as a request, and
//! crosses the serialized binding as an empty message.

use std::sync::Mutex;
use std::time::Duration;

use sekvent_component::{App, AppError, Binding, CallContext, component};
use sekvent_config::MapSource;

#[derive(Clone, PartialEq, prost::Message)]
pub struct Chore {
    #[prost(string, tag = "1")]
    pub name: String,
}

impl prost::Name for Chore {
    const NAME: &'static str = "Chore";
    const PACKAGE: &'static str = "check.chores.v1";
}

/// The contract sekvent-proto-build would generate for `check.chores.v1`.
mod proto {
    #[allow(non_upper_case_globals)]
    pub const __sekvent_service_Chores: (&str, &[(&str, &str, &str, bool)]) = (
        "check.chores.v1.Chores",
        &[
            (
                "Add",
                "check.chores.v1.Chore",
                "google.protobuf.Empty",
                false,
            ),
            (
                "Next",
                "google.protobuf.Empty",
                "check.chores.v1.Chore",
                false,
            ),
            (
                "Clear",
                "google.protobuf.Empty",
                "google.protobuf.Empty",
                false,
            ),
        ],
    );
    #[allow(non_camel_case_types, dead_code)]
    pub type __sekvent_rpc_Chores__Add = (super::Chore, ());
    #[allow(non_camel_case_types, dead_code)]
    pub type __sekvent_rpc_Chores__Next = ((), super::Chore);
    #[allow(non_camel_case_types, dead_code)]
    pub type __sekvent_rpc_Chores__Clear = ((), ());
}

/// A list of chores.
#[component(name = "chores", package = "check.chores.v1", proto = "crate::proto")]
pub trait Chores {
    /// Add a chore.
    #[call]
    async fn add(&self, cx: &CallContext, req: Chore) -> Result<(), AppError>;

    /// The oldest chore.
    #[call]
    async fn next(&self, cx: &CallContext, req: ()) -> Result<Chore, AppError>;

    /// Forget every chore.
    #[call]
    async fn clear(&self, cx: &CallContext, req: ()) -> Result<(), AppError>;
}

#[derive(Default)]
struct ChoreList(Mutex<Vec<Chore>>);

impl Chores for ChoreList {
    async fn add(&self, _cx: &CallContext, req: Chore) -> Result<(), AppError> {
        self.0.lock().expect("lock").push(req);
        Ok(())
    }

    async fn next(&self, _cx: &CallContext, (): ()) -> Result<Chore, AppError> {
        self.0
            .lock()
            .expect("lock")
            .first()
            .cloned()
            .ok_or_else(|| AppError::not_found("no chores"))
    }

    async fn clear(&self, _cx: &CallContext, (): ()) -> Result<(), AppError> {
        self.0.lock().expect("lock").clear();
        Ok(())
    }
}

async fn run(source: &MapSource, binding: Binding) {
    let mut builder = App::builder(source);
    ChoresHandle::install(&mut builder, |_deps| Ok(ChoreList::default())).expect("install");
    let app = builder.build().expect("build");
    app.start().await.expect("start");
    let chores = app.handle::<ChoresHandle>().expect("handle");
    assert_eq!(chores.binding(), binding);

    let cx = CallContext::new();
    chores
        .add(
            &cx,
            Chore {
                name: "dishes".into(),
            },
        )
        .await
        .expect("add");
    assert_eq!(chores.next(&cx, ()).await.expect("next").name, "dishes");
    chores.clear(&cx, ()).await.expect("clear");
    chores.next(&cx, ()).await.expect_err("no chores left");

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
