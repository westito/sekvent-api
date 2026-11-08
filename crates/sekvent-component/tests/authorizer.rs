//! The App's call authorizer under both in-process bindings and on the
//! caller side of a component bound `grpc`: what it sees, how its answer
//! reaches the caller, nested calls, stream opens and panics.

mod support;

use std::sync::{Arc, Mutex, PoisonError};

use sekvent_component::{App, AppError, Binding, CallContext, CallTarget, ErrorCode, LOCAL_CALLER};
use sekvent_context::EndUser;
use support::fakes::{
    Audit, AuditHandle, Behaviour, FakeInventory, LOCAL_BINDINGS, ScriptedTicker, Seen, build_with,
    source,
};
use support::grpc::{caller_keys, guarded, serve, service_keys};
use support::inventory::{InventoryError, InventoryHandle, ReserveRequest};
use support::ticker::{TickerError, TickerHandle, TicksRequest};

/// What the authorizer saw of one call.
#[derive(Debug, Clone, PartialEq)]
struct Asked {
    component: &'static str,
    method: &'static str,
    idempotent: bool,
    caller: Option<(String, bool)>,
    subject: Option<String>,
    end_user: bool,
}

/// Every call the authorizer was asked about, in order.
#[derive(Debug, Clone, Default)]
struct Asks(Arc<Mutex<Vec<Asked>>>);

impl Asks {
    fn record(&self, target: CallTarget, cx: &CallContext) {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(Asked {
                component: target.component().name(),
                method: target.method().name(),
                idempotent: target.method().is_idempotent(),
                caller: cx
                    .caller()
                    .map(|caller| (caller.name.clone(), caller.trusted)),
                subject: cx.subject().map(str::to_owned),
                end_user: cx.end_user().is_some(),
            });
    }

    fn all(&self) -> Vec<Asked> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

/// Records every call; refuses the subject `intruder` with
/// `PERMISSION_DENIED` and panics for the subject `panic`.
fn policy(asks: &Asks) -> impl Fn(CallTarget, &CallContext) -> Result<(), AppError> + use<> {
    let asks = asks.clone();
    move |target, cx| {
        asks.record(target, cx);
        match cx.subject() {
            Some("panic") => panic!("the authorizer panicked on purpose"),
            Some("intruder") => Err(denied()),
            _ => Ok(()),
        }
    }
}

fn denied() -> AppError {
    AppError::permission_denied("machine principals may not write here")
        .with_reason("SCOPE_MISSING")
        .with_domain("shop.access")
        .with_metadata("scope", "inventory:w")
}

fn reserve(order: &str) -> ReserveRequest {
    ReserveRequest {
        order_id: order.to_owned(),
        sku: "sku-1".to_owned(),
        quantity: 1,
    }
}

fn as_subject(subject: &str) -> CallContext {
    CallContext::new().with_subject(subject)
}

/// A started App with `inventory` installed and `policy` registered.
async fn guarded_inventory(binding: Binding, inventory: FakeInventory, asks: &Asks) -> App {
    let asks = asks.clone();
    let app = build_with(&source(binding, &[]), move |builder| {
        builder.call_authorizer(policy(&asks))?;
        InventoryHandle::install(builder, move |_| Ok(inventory))
    })
    .unwrap();
    app.start().await.unwrap();
    app
}

fn other(error: InventoryError) -> AppError {
    match error {
        InventoryError::Other(error) => error,
        unexpected => panic!("expected Other, got {unexpected:?}"),
    }
}

#[tokio::test]
async fn an_allowed_call_reaches_the_handler_with_what_the_authorizer_saw() {
    for binding in LOCAL_BINDINGS {
        let asks = Asks::default();
        let fake = FakeInventory::stock(10);
        let probe = fake.probe.clone();
        let app = guarded_inventory(binding, fake, &asks).await;
        let inventory = app.handle::<InventoryHandle>().unwrap();
        let cx = as_subject("user-1").with_end_user(EndUser::new("user-1"));
        inventory.reserve(&cx, reserve("o1")).await.unwrap();

        let calls: Vec<Seen> = probe.calls();
        assert_eq!(calls.len(), 1, "{binding}");
        assert_eq!(
            asks.all(),
            [Asked {
                component: "inventory",
                method: "reserve",
                idempotent: true,
                caller: Some((LOCAL_CALLER.to_owned(), true)),
                subject: Some("user-1".to_owned()),
                end_user: false,
            }],
            "{binding}"
        );
        assert_eq!(calls[0].caller, asks.all()[0].caller, "{binding}");
        assert_eq!(calls[0].subject, asks.all()[0].subject, "{binding}");
    }
}

#[tokio::test]
async fn a_denial_is_the_answer_unchanged_and_the_handler_never_runs() {
    for binding in LOCAL_BINDINGS {
        let asks = Asks::default();
        let fake = FakeInventory::stock(10);
        let probe = fake.probe.clone();
        let app = guarded_inventory(binding, fake, &asks).await;
        let inventory = app.handle::<InventoryHandle>().unwrap();
        let error = other(
            inventory
                .reserve(&as_subject("intruder"), reserve("o1"))
                .await
                .unwrap_err(),
        );
        let expected = denied();
        assert_eq!(error.code(), ErrorCode::PermissionDenied, "{binding}");
        assert_eq!(error.message(), expected.message(), "{binding}");
        assert_eq!(error.reason(), Some("SCOPE_MISSING"), "{binding}");
        assert_eq!(error.domain(), Some("shop.access"), "{binding}");
        assert_eq!(
            error.metadata(),
            expected.metadata(),
            "not tagged: {binding}"
        );
        assert!(probe.calls().is_empty(), "{binding}");
        assert_eq!(asks.all().len(), 1, "{binding}");
    }
}

/// Serves `audit` by reserving one item through the inventory.
struct Reserving {
    inventory: InventoryHandle,
}

impl Audit for Reserving {
    async fn add(&self, cx: &CallContext, req: String) -> Result<usize, AppError> {
        self.inventory
            .reserve(cx, reserve(&req))
            .await
            .map(|reply| reply.remaining as usize)
            .map_err(|error| match error {
                InventoryError::Other(error) => error,
                typed => AppError::internal(format!("{typed:?}")),
            })
    }
}

#[tokio::test]
async fn a_nested_call_is_authorized_at_the_nested_component() {
    for binding in LOCAL_BINDINGS {
        let asks = Asks::default();
        let registered = asks.clone();
        let app = build_with(&source(binding, &[]), move |builder| {
            builder.call_authorizer(policy(&registered))?;
            InventoryHandle::install(builder, |_| Ok(FakeInventory::stock(10)))?;
            AuditHandle::install(builder, |deps| {
                Ok(Reserving {
                    inventory: deps.handle::<InventoryHandle>()?,
                })
            })
        })
        .unwrap();
        app.start().await.unwrap();
        let audit = app.handle::<AuditHandle>().unwrap();
        assert_eq!(
            audit.add(&as_subject("user-1"), "o1".into()).await.unwrap(),
            9
        );

        let asked: Vec<_> = asks
            .all()
            .into_iter()
            .map(|asked| (asked.component, asked.method, asked.subject))
            .collect();
        assert_eq!(
            asked,
            [
                ("audit", "add", Some("user-1".to_owned())),
                ("inventory", "reserve", Some("user-1".to_owned())),
            ],
            "{binding}"
        );
    }
}

#[tokio::test]
async fn a_stream_open_can_be_denied() {
    for binding in LOCAL_BINDINGS {
        let asks = Asks::default();
        let registered = asks.clone();
        let (scripted, mut events) = ScriptedTicker::new();
        let app = build_with(&source(binding, &[]), move |builder| {
            builder.call_authorizer(policy(&registered))?;
            TickerHandle::install(builder, move |_| Ok(scripted))
        })
        .unwrap();
        app.start().await.unwrap();
        let ticker = app.handle::<TickerHandle>().unwrap();
        let request = TicksRequest {
            series: "feed".to_owned(),
            from: 0,
        };
        let error = match ticker.ticks(&as_subject("intruder"), request).await {
            Err(TickerError::Other(error)) => error,
            Err(unexpected) => panic!("expected Other, got {unexpected:?}"),
            Ok(_) => panic!("the open was allowed"),
        };
        assert_eq!(error.code(), ErrorCode::PermissionDenied, "{binding}");
        assert_eq!(error.metadata(), denied().metadata(), "{binding}");
        assert!(events.opened.try_recv().is_err(), "never opened: {binding}");
        assert_eq!(
            asks.all()
                .iter()
                .map(|asked| asked.method)
                .collect::<Vec<_>>(),
            ["ticks"]
        );
    }
}

#[tokio::test]
async fn a_panicking_authorizer_is_internal_when_serialized() {
    let asks = Asks::default();
    let fake = FakeInventory::new(Behaviour::Panic);
    let app = guarded_inventory(Binding::LocalSerialized, fake, &asks).await;
    let inventory = app.handle::<InventoryHandle>().unwrap();
    let error = other(
        inventory
            .reserve(&as_subject("panic"), reserve("o1"))
            .await
            .unwrap_err(),
    );
    assert_eq!(error.code(), ErrorCode::Internal);
    assert_eq!(
        error.message(),
        "component inventory method reserve panicked"
    );
    assert_eq!(error.metadata()["method"], "reserve");
    assert_eq!(asks.all().len(), 1);
    app.stop(std::time::Duration::ZERO).await.unwrap();
}

#[tokio::test]
async fn a_panicking_authorizer_propagates_when_local() {
    let asks = Asks::default();
    let app = guarded_inventory(Binding::Local, FakeInventory::stock(1), &asks).await;
    let inventory = app.handle::<InventoryHandle>().unwrap();
    let call =
        tokio::spawn(async move { inventory.reserve(&as_subject("panic"), reserve("o1")).await });
    assert!(call.await.unwrap_err().is_panic());
}

#[tokio::test]
async fn a_grpc_bound_component_is_authorized_by_the_process_serving_it() {
    guarded(async {
        let served_asks = Asks::default();
        let registered = served_asks.clone();
        let service = serve(&service_keys(), move |builder| {
            builder.call_authorizer(policy(&registered))?;
            InventoryHandle::install(builder, |_| Ok(FakeInventory::stock(10)))
        })
        .await;

        let caller_asks = Asks::default();
        let registered = caller_asks.clone();
        let config: sekvent_config::MapSource =
            caller_keys(&service.endpoint(), &[]).into_iter().collect();
        let validated = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&validated);
        let app = build_with(&config, move |builder| {
            builder.call_authorizer(Validating {
                policy: policy(&registered),
                validated: seen,
            })?;
            InventoryHandle::install(builder, |_| -> Result<FakeInventory, AppError> {
                panic!("a grpc-bound component runs no factory")
            })
        })
        .unwrap();
        app.start().await.unwrap();
        let inventory = app.handle::<InventoryHandle>().unwrap();

        inventory
            .reserve(&CallContext::new(), reserve("o1"))
            .await
            .unwrap();
        assert!(caller_asks.all().is_empty(), "the caller side never asks");
        assert_eq!(
            *validated.lock().unwrap(),
            ["inventory"],
            "but it validates every component with an implementation"
        );
        let served = served_asks.all();
        assert_eq!(served.len(), 1);
        assert_eq!(served[0].caller, Some(("shop".to_owned(), false)));
        assert!(!served[0].end_user);

        app.stop(std::time::Duration::ZERO).await.unwrap();
        service.stop().await;
    })
    .await;
}

/// An authorizer that records the components it validates.
struct Validating<P> {
    policy: P,
    validated: Arc<Mutex<Vec<&'static str>>>,
}

impl<P> sekvent_component::CallAuthorizer for Validating<P>
where
    P: Fn(CallTarget, &CallContext) -> Result<(), AppError> + Send + Sync + 'static,
{
    fn authorize(&self, target: CallTarget, cx: &CallContext) -> Result<(), AppError> {
        (self.policy)(target, cx)
    }

    fn validate(
        &self,
        component: &'static sekvent_component::ComponentDescriptor,
    ) -> Result<(), AppError> {
        self.validated.lock().unwrap().push(component.name());
        Ok(())
    }
}
