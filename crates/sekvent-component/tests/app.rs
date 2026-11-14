//! Installing, building and wiring components.

mod support;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use sekvent_component::{
    App, AppError, Binding, BuildError, ComponentHandle, ComponentState, DEFAULT_BINDING_KEY,
    ErrorCode,
};
use sekvent_config::{ConfigError, MapSource};
use support::fakes::{
    AuditHandle, BareHandle, Counter, FakeInventory, JotterHandle, LedgerHandle, NotesHandle,
    build_with, source,
};
use support::inventory::InventoryHandle;

fn factory_error(error: BuildError) -> (String, AppError) {
    match error {
        BuildError::Factory { component, source } => (component, source),
        other => panic!("expected a factory error, got {other}"),
    }
}

fn all(error: BuildError) -> Vec<BuildError> {
    match error {
        BuildError::Multiple(errors) => errors,
        other => vec![other],
    }
}

#[test]
fn the_same_component_cannot_be_installed_twice() {
    let config = MapSource::new();
    let mut builder = App::builder(&config);
    InventoryHandle::install(&mut builder, |_| Ok(FakeInventory::stock(1))).unwrap();
    let by_type = InventoryHandle::install(&mut builder, |_| Ok(FakeInventory::stock(1)));
    assert!(
        matches!(&by_type, Err(BuildError::DuplicateInstall { component }) if component == "inventory"),
        "{by_type:?}"
    );

    NotesHandle::install(&mut builder, |_| Ok(Counter::plain())).unwrap();
    let by_name = JotterHandle::install(&mut builder, |_| Ok(Counter::plain()));
    assert!(
        matches!(&by_name, Err(BuildError::DuplicateInstall { component }) if component == "notes"),
        "{by_name:?}"
    );
    let remote = LedgerHandle::install_remote(&mut builder);
    assert!(remote.is_ok());
    let remote_twice = LedgerHandle::install_remote(&mut builder);
    assert!(matches!(
        remote_twice,
        Err(BuildError::DuplicateInstall { .. })
    ));
}

#[test]
fn a_resource_type_is_provided_once() {
    let config = MapSource::new();
    let mut builder = App::builder(&config);
    builder.provide(5_u32).unwrap();
    builder.provide(String::from("dsn")).unwrap();
    let error = builder.provide(6_u32).unwrap_err();
    assert!(
        matches!(error, BuildError::DuplicateResource { type_name: "u32" }),
        "{error}"
    );
    NotesHandle::install(&mut builder, |_| Ok(Counter::plain())).unwrap();
    assert_eq!(
        format!("{builder:?}"),
        r#"AppBuilder { components: ["notes"], resources: ["alloc::string::String", "u32"], end_user_authenticator: false, call_authorizer: false, scope_resolver: false, .. }"#
    );
}

#[test]
fn configuration_errors_are_reported_together_and_no_factory_runs() {
    let ran = Arc::new(AtomicBool::new(false));
    let config: MapSource = [
        ("SEKVENT_COMPONENT_INVENTORY_RESERVE_TIMEUOT", "1s"),
        ("SEKVENT_COMPONENT_INVENTORY_TIMEOUT", "whenever"),
        ("SEKVENT_COMPONENT_NOTES_BINDING", "grpc"),
    ]
    .into_iter()
    .collect();
    let (first, second) = (Arc::clone(&ran), Arc::clone(&ran));
    let error = build_with(&config, move |builder| {
        InventoryHandle::install(builder, move |_| {
            first.store(true, Ordering::SeqCst);
            Ok(FakeInventory::stock(1))
        })?;
        NotesHandle::install(builder, move |_| {
            second.store(true, Ordering::SeqCst);
            Ok(Counter::plain())
        })
    })
    .unwrap_err();
    let text = error.to_string();
    assert!(!ran.load(Ordering::SeqCst));
    assert!(text.starts_with("3 component build errors: "), "{text}");
    assert!(!text.contains("whenever"), "{text}");
    let errors = all(error);
    assert!(matches!(
        &errors[0],
        BuildError::Config(ConfigError::UnknownKeys { .. })
    ));
    assert!(matches!(
        &errors[1],
        BuildError::Config(ConfigError::Malformed { key, .. }) if key == "SEKVENT_COMPONENT_INVENTORY_TIMEOUT"
    ));
    assert!(matches!(
        &errors[2],
        BuildError::LocalOnly {
            binding: Binding::Grpc,
            ..
        }
    ));
}

#[test]
fn a_grpc_binding_needs_an_endpoint() {
    let config = source(Binding::Grpc, &[]);
    let error = build_with(&config, |builder| {
        InventoryHandle::install(builder, |_| Ok(FakeInventory::stock(1)))
    })
    .unwrap_err();
    assert_eq!(
        error.to_string(),
        "missing required configuration key SEKVENT_COMPONENT_INVENTORY_ENDPOINT"
    );
}

#[test]
fn a_grpc_bound_component_runs_no_factory() {
    let ran = Arc::new(AtomicBool::new(false));
    let config = source(
        Binding::Grpc,
        &[
            ("SEKVENT_COMPONENT_INVENTORY_ENDPOINT", "http://127.0.0.1:1"),
            ("SEKVENT_COMPONENT_INVENTORY_AUTH", "none"),
        ],
    );
    let flag = Arc::clone(&ran);
    let app = build_with(&config, move |builder| {
        InventoryHandle::install(builder, move |_| {
            flag.store(true, Ordering::SeqCst);
            Ok(FakeInventory::stock(1))
        })
    })
    .unwrap();
    assert!(!ran.load(Ordering::SeqCst));
    assert_eq!(app.binding("inventory"), Some(Binding::Grpc));
    assert_eq!(
        app.handle::<InventoryHandle>().unwrap().binding(),
        Binding::Grpc
    );
    assert_eq!(app.grpc_services(), [] as [String; 0]);
}

#[test]
fn remote_only_components_need_a_grpc_binding() {
    let error = build_with(&MapSource::new(), LedgerHandle::install_remote).unwrap_err();
    assert!(
        matches!(&error, BuildError::RemoteOnlyUnbound { component, key }
            if component == "ledger" && key == "SEKVENT_COMPONENT_LEDGER_BINDING"),
        "{error}"
    );

    let config: MapSource = [("SEKVENT_COMPONENT_LEDGER_BINDING", "grpc")]
        .into_iter()
        .collect();
    let error = build_with(&config, LedgerHandle::install_remote).unwrap_err();
    assert!(
        matches!(
            &error,
            BuildError::Config(ConfigError::Missing { key }) if key == "SEKVENT_COMPONENT_LEDGER_ENDPOINT"
        ),
        "{error}"
    );

    // A factory for a remote_only descriptor, or no factory for a standard
    // one, is the same mode error.
    let error = build_with(&MapSource::new(), |builder| {
        LedgerHandle::install(builder, |_| Ok(Counter::plain()))
    })
    .unwrap_err();
    assert!(
        matches!(error, BuildError::RemoteOnlyUnbound { .. }),
        "{error}"
    );
    let error = build_with(&MapSource::new(), BareHandle::install_remote).unwrap_err();
    assert!(
        matches!(error, BuildError::RemoteOnlyUnbound { .. }),
        "{error}"
    );
}

#[test]
fn a_factory_error_stops_the_build() {
    let later = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&later);
    let error = build_with(&MapSource::new(), move |builder| {
        InventoryHandle::install(builder, |_| {
            Err::<FakeInventory, _>(AppError::unavailable("stock database down"))
        })?;
        NotesHandle::install(builder, move |_| {
            flag.store(true, Ordering::SeqCst);
            Ok(Counter::plain())
        })
    })
    .unwrap_err();
    assert!(!later.load(Ordering::SeqCst));
    assert_eq!(
        error.to_string(),
        "component inventory failed to build: UNAVAILABLE: stock database down"
    );
    let (component, cause) = factory_error(error);
    assert_eq!(component, "inventory");
    assert_eq!(cause.code(), ErrorCode::Unavailable);
}

#[test]
fn dependencies_must_be_installed_earlier() {
    let missing = build_with(&MapSource::new(), |builder| {
        NotesHandle::install(builder, |deps| {
            deps.handle::<InventoryHandle>()?;
            Ok(Counter::plain())
        })
    })
    .unwrap_err();
    let (component, cause) = factory_error(missing);
    assert_eq!(component, "notes");
    assert_eq!(cause.code(), ErrorCode::FailedPrecondition);
    assert_eq!(
        cause.message(),
        "component notes depends on inventory, which is not installed"
    );

    let later = build_with(&MapSource::new(), |builder| {
        NotesHandle::install(builder, |deps| {
            deps.handle::<AuditHandle>()?;
            Ok(Counter::plain())
        })?;
        AuditHandle::install(builder, |_| Ok(Counter::plain()))
    })
    .unwrap_err();
    let (_, cause) = factory_error(later);
    assert_eq!(cause.code(), ErrorCode::FailedPrecondition);
    assert_eq!(
        cause.message(),
        "component notes depends on audit; install audit before notes"
    );

    let itself = build_with(&MapSource::new(), |builder| {
        NotesHandle::install(builder, |deps| {
            deps.handle::<NotesHandle>()?;
            Ok(Counter::plain())
        })
    })
    .unwrap_err();
    let (_, cause) = factory_error(itself);
    assert_eq!(cause.message(), "component notes depends on itself");
}

#[tokio::test]
async fn a_factory_sees_its_dependencies_resources_and_configuration() {
    let config = source(
        Binding::LocalSerialized,
        &[("SEKVENT_COMPONENT_AUDIT_TIMEOUT", "3s")],
    );
    let app = build_with(&config, |builder| {
        builder.provide(String::from("dsn"))?;
        InventoryHandle::install(builder, |_| Ok(FakeInventory::stock(4)))?;
        AuditHandle::install(builder, |deps| {
            assert_eq!(deps.component(), "audit");
            assert_eq!(deps.binding(), Binding::Local);
            assert_eq!(
                deps.config().get(DEFAULT_BINDING_KEY).as_deref(),
                Some("local-serialized")
            );
            assert!(format!("{deps:?}").contains("audit"));
            assert_eq!(deps.resource::<String>()?, "dsn");
            let missing = deps.resource::<u64>().unwrap_err();
            assert_eq!(missing.code(), ErrorCode::FailedPrecondition);
            assert!(missing.message().contains("u64"), "{missing}");
            let inventory = deps.handle::<InventoryHandle>()?;
            assert_eq!(inventory.binding(), Binding::LocalSerialized);
            Ok(Counter::plain())
        })
    })
    .unwrap();

    assert_eq!(app.components(), ["inventory", "audit"]);
    assert_eq!(app.binding("inventory"), Some(Binding::LocalSerialized));
    assert_eq!(app.binding("audit"), Some(Binding::Local));
    assert_eq!(app.binding("billing"), None);
    assert_eq!(app.state("inventory"), Some(ComponentState::NotStarted));
    assert_eq!(app.state("billing"), None);
    assert_eq!(
        format!("{app:?}"),
        "App { components: [(\"inventory\", LocalSerialized, NotStarted), \
         (\"audit\", Local, NotStarted)] }"
    );

    let error = app.handle::<NotesHandle>().unwrap_err();
    assert_eq!(error.code(), ErrorCode::FailedPrecondition);
    assert_eq!(error.message(), "component notes is not installed");
    let clone = app.clone();
    assert!(clone.handle::<AuditHandle>().is_ok());
    assert_eq!(
        <InventoryHandle as ComponentHandle>::DESCRIPTOR
            .full_service_name()
            .as_deref(),
        Some("shop.inventory.v1.Inventory")
    );
}

#[test]
fn an_empty_app_builds() {
    let app = App::builder(&MapSource::new()).build().unwrap();
    assert_eq!(app.components(), [] as [&str; 0]);
}

#[test]
fn a_malformed_default_binding_is_reported() {
    let config: MapSource = [(DEFAULT_BINDING_KEY, "Local")].into_iter().collect();
    let error = build_with(&config, |builder| {
        InventoryHandle::install(builder, |_| Ok(FakeInventory::stock(1)))
    })
    .unwrap_err();
    assert_eq!(
        error.to_string(),
        "configuration key SEKVENT_COMPONENT_BINDING is malformed: expected one of local, \
         local-serialized, grpc"
    );
}

/// An authorizer that records the components it validates and refuses
/// those named in `refuse`.
struct Validating {
    validated: Arc<std::sync::Mutex<Vec<&'static str>>>,
    refuse: &'static [&'static str],
}

impl sekvent_component::CallAuthorizer for Validating {
    fn authorize(
        &self,
        _target: sekvent_component::CallTarget,
        _cx: &sekvent_component::CallContext,
    ) -> Result<(), AppError> {
        Ok(())
    }

    fn validate(
        &self,
        component: &'static sekvent_component::ComponentDescriptor,
    ) -> Result<(), AppError> {
        self.validated.lock().unwrap().push(component.name());
        if self.refuse.contains(&component.name()) {
            return Err(AppError::failed_precondition("no rule for this component"));
        }
        Ok(())
    }
}

#[test]
fn a_second_call_authorizer_is_refused() {
    let config = MapSource::new();
    let mut builder = App::builder(&config);
    let allow = |_: sekvent_component::CallTarget, _: &sekvent_component::CallContext| Ok(());
    builder.call_authorizer(allow).unwrap();
    let again = builder.call_authorizer(allow);
    assert!(
        matches!(again, Err(BuildError::DuplicateCallAuthorizer)),
        "{again:?}"
    );
    assert!(format!("{builder:?}").contains("call_authorizer: true"));
}

#[test]
fn the_call_authorizer_validates_every_component_with_an_implementation() {
    let validated = Arc::new(std::sync::Mutex::new(Vec::new()));
    let authorizer = Validating {
        validated: Arc::clone(&validated),
        refuse: &[],
    };
    let config: MapSource = [
        ("SEKVENT_COMPONENT_LEDGER_BINDING", "grpc"),
        ("SEKVENT_COMPONENT_LEDGER_ENDPOINT", "http://127.0.0.1:1"),
        ("SEKVENT_COMPONENT_LEDGER_AUTH", "none"),
    ]
    .into_iter()
    .collect();
    build_with(&config, move |builder| {
        builder.call_authorizer(authorizer)?;
        InventoryHandle::install(builder, |_| Ok(FakeInventory::stock(1)))?;
        LedgerHandle::install_remote(builder)?;
        NotesHandle::install(builder, |_| Ok(Counter::plain()))
    })
    .unwrap();
    assert_eq!(
        *validated.lock().unwrap(),
        ["inventory", "notes"],
        "once each, never a remote-only component"
    );
}

#[test]
fn call_authorizer_refusals_are_reported_with_configuration_errors_and_no_factory_runs() {
    let ran = Arc::new(AtomicBool::new(false));
    let (first, second) = (Arc::clone(&ran), Arc::clone(&ran));
    let validated = Arc::new(std::sync::Mutex::new(Vec::new()));
    let authorizer = Validating {
        validated: Arc::clone(&validated),
        refuse: &["inventory", "notes"],
    };
    let config: MapSource = [("SEKVENT_COMPONENT_INVENTORY_TIMEOUT", "whenever")]
        .into_iter()
        .collect();
    let install = move |builder: &mut sekvent_component::AppBuilder<'_>| {
        builder.call_authorizer(authorizer)?;
        InventoryHandle::install(builder, move |_| {
            first.store(true, Ordering::SeqCst);
            Ok(FakeInventory::stock(1))
        })?;
        NotesHandle::install(builder, move |_| {
            second.store(true, Ordering::SeqCst);
            Ok(Counter::plain())
        })
    };
    let errors = all(build_with(&config, install).unwrap_err());
    assert!(!ran.load(Ordering::SeqCst));
    assert_eq!(errors.len(), 3, "{errors:?}");
    assert!(matches!(
        &errors[0],
        BuildError::Config(ConfigError::Malformed { .. })
    ));
    let refused: Vec<_> = errors[1..]
        .iter()
        .map(|error| match error {
            BuildError::CallAuthorizerRefused { component, source } => {
                assert_eq!(source.code(), ErrorCode::FailedPrecondition);
                component.as_str()
            }
            other => panic!("expected a refusal, got {other}"),
        })
        .collect();
    assert_eq!(refused, ["inventory", "notes"]);

    // Refusals alone fail the build too.
    let ran = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&ran);
    let error = build_with(&MapSource::new(), move |builder| {
        builder.call_authorizer(Validating {
            validated: Arc::default(),
            refuse: &["inventory"],
        })?;
        InventoryHandle::install(builder, move |_| {
            flag.store(true, Ordering::SeqCst);
            Ok(FakeInventory::stock(1))
        })
    })
    .unwrap_err();
    assert!(!ran.load(Ordering::SeqCst));
    assert!(
        matches!(&error, BuildError::CallAuthorizerRefused { component, .. } if component == "inventory"),
        "{error}"
    );
}

/// The site a resolved scope belongs to.
#[derive(Debug, Clone, PartialEq)]
struct Site(&'static str);

/// Maps the `host` header to a tenant: `a.test` is `t-1`, `open.test` a
/// scope without a tenant, `bad.test` the tenant `bad tenant` (not a valid
/// tenant), `slow.test` takes an hour, anything else is `NOT_FOUND`.
struct Hosts;

impl sekvent_component::ScopeResolver for Hosts {
    fn resolve<'a>(
        &'a self,
        request: &'a http::request::Parts,
        _cx: &'a sekvent_component::CallContext,
    ) -> std::pin::Pin<
        Box<dyn Future<Output = Result<sekvent_component::RequestScope, AppError>> + Send + 'a>,
    > {
        let host = request
            .headers
            .get(http::header::HOST)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        Box::pin(async move {
            match host.as_deref() {
                Some("a.test") => Ok(sekvent_component::RequestScope::new()
                    .with_tenant("t-1")
                    .with_extension(Site("a"))),
                Some("open.test") => Ok(sekvent_component::RequestScope::new()),
                Some("bad.test") => {
                    Ok(sekvent_component::RequestScope::new().with_tenant("bad tenant"))
                }
                Some("slow.test") => {
                    tokio::time::sleep(std::time::Duration::from_hours(1)).await;
                    Ok(sekvent_component::RequestScope::new())
                }
                _ => Err(AppError::not_found("unknown site").with_reason("SITE_UNKNOWN")),
            }
        })
    }
}

fn head(host: &str) -> http::request::Parts {
    http::Request::builder()
        .uri("/orders")
        .header(http::header::HOST, host)
        .body(())
        .unwrap()
        .into_parts()
        .0
}

fn scoped_app() -> App {
    build_with(&MapSource::new(), |builder| builder.scope_resolver(Hosts)).unwrap()
}

#[test]
fn a_second_scope_resolver_is_refused() {
    let config = MapSource::new();
    let mut builder = App::builder(&config);
    assert!(format!("{builder:?}").contains("scope_resolver: false"));
    builder.scope_resolver(Hosts).unwrap();
    let again = builder.scope_resolver(Hosts);
    assert!(
        matches!(again, Err(BuildError::DuplicateScopeResolver)),
        "{again:?}"
    );
    assert!(format!("{builder:?}").contains("scope_resolver: true"));
    assert_eq!(
        BuildError::DuplicateScopeResolver.to_string(),
        "a scope resolver is registered twice"
    );
}

#[tokio::test]
async fn resolve_scope_attaches_the_scope_to_a_rest_context() {
    use sekvent_component::CallContext;

    // Without a resolver the context is unchanged.
    let plain = App::builder(&MapSource::new()).build().unwrap();
    let cx = plain
        .resolve_scope(&head("a.test"), CallContext::new().with_tenant("t-9"))
        .await
        .unwrap();
    assert!(cx.scope().is_none());
    assert_eq!(cx.tenant(), Some("t-9"));

    // With one, through a clone of the App as a route holds it.
    let app = scoped_app();
    let route = app.clone();
    let parts = head("a.test");
    let cx = route
        .resolve_scope(&parts, CallContext::new().with_tenant("t-9"))
        .await
        .unwrap();
    assert_eq!(cx.tenant(), Some("t-1"));
    assert_eq!(cx.scoped::<Site>(), Some(&Site("a")));
    assert!(
        parts
            .extensions
            .get::<sekvent_component::RequestScope>()
            .is_none(),
        "the request's extensions are left alone"
    );

    // A service caller gets no scope.
    let linked = CallContext::new().with_caller(sekvent_context::ServiceIdentity::trusted("shop"));
    let cx = app.resolve_scope(&head("a.test"), linked).await.unwrap();
    assert!(cx.scope().is_none());
    assert_eq!(cx.tenant(), None);

    // The resolver's error, unchanged.
    let error = app
        .resolve_scope(&head("c.test"), CallContext::new())
        .await
        .unwrap_err();
    assert_eq!(error.code(), ErrorCode::NotFound);
    assert_eq!(error.message(), "unknown site");
    assert_eq!(error.reason(), Some("SITE_UNKNOWN"));
    assert!(error.metadata().is_empty());
}

#[tokio::test(start_paused = true)]
async fn resolve_scope_runs_within_the_deadline() {
    use sekvent_component::CallContext;

    let app = scoped_app();
    let cx = CallContext::new().with_timeout(std::time::Duration::from_millis(50));
    let error = app.resolve_scope(&head("slow.test"), cx).await.unwrap_err();
    assert_eq!(error.code(), ErrorCode::DeadlineExceeded);
    assert_eq!(error.message(), "the call deadline was exceeded");

    // A deadline already passed refuses even a resolver that answers at once.
    let passed = CallContext::new().with_deadline(tokio::time::Instant::now().into_std());
    let error = app
        .resolve_scope(&head("a.test"), passed)
        .await
        .unwrap_err();
    assert_eq!(error.code(), ErrorCode::DeadlineExceeded);
}

#[tokio::test]
async fn a_rest_route_applies_the_tenant_rule_with_admit_end_user() {
    use sekvent_component::{CallContext, END_USER_REJECTED_MESSAGE, EndUser, admit_end_user};

    let app = scoped_app();
    let cx = app
        .resolve_scope(&head("a.test"), CallContext::new())
        .await
        .unwrap();
    let stranger = EndUser::new("u-2").with_tenant("t-2");

    // `with_end_user` makes no check: the context keeps the scope's tenant.
    let unchecked = cx.clone().with_end_user(stranger.clone());
    assert_eq!(unchecked.tenant(), Some("t-1"));

    let error = admit_end_user(cx.clone(), stranger).unwrap_err();
    assert_eq!(error.code(), ErrorCode::Unauthenticated);
    assert_eq!(error.message(), END_USER_REJECTED_MESSAGE);
    assert_eq!(error.reason(), None);

    let member = admit_end_user(cx, EndUser::new("u-1").with_tenant("t-1")).unwrap();
    assert_eq!(member.tenant(), Some("t-1"));
    assert_eq!(member.end_user().map(EndUser::subject), Some("u-1"));
}

#[tokio::test]
async fn resolve_scope_applies_the_tenant_rule_to_an_end_user_already_in_the_context() {
    use sekvent_component::{CallContext, END_USER_REJECTED_MESSAGE, EndUser};

    let app = scoped_app();
    // A user of another tenant, or of none, on a site of `t-1`.
    for user in [
        EndUser::new("u-2").with_tenant("t-2"),
        EndUser::new("roamer"),
    ] {
        let cx = CallContext::new().with_end_user(user);
        let error = app.resolve_scope(&head("a.test"), cx).await.unwrap_err();
        assert_eq!(error.code(), ErrorCode::Unauthenticated);
        assert_eq!(error.message(), END_USER_REJECTED_MESSAGE);
        assert_eq!(error.reason(), None);
        assert!(error.metadata().is_empty());
    }

    let member = CallContext::new().with_end_user(EndUser::new("u-1").with_tenant("t-1"));
    let cx = app.resolve_scope(&head("a.test"), member).await.unwrap();
    assert_eq!(cx.tenant(), Some("t-1"));
    assert_eq!(cx.end_user().map(EndUser::subject), Some("u-1"));
    assert_eq!(cx.scoped::<Site>(), Some(&Site("a")));

    // A scope without a tenant admits any user, who keeps their own.
    let outsider = CallContext::new().with_end_user(EndUser::new("u-2").with_tenant("t-2"));
    let cx = app
        .resolve_scope(&head("open.test"), outsider)
        .await
        .unwrap();
    assert_eq!(cx.tenant(), Some("t-2"));
    assert!(cx.scope().is_some());
}

#[tokio::test]
async fn resolve_scope_refuses_a_scope_with_an_invalid_tenant() {
    use sekvent_component::{CallContext, reasons};

    let app = scoped_app();
    let error = app
        .resolve_scope(&head("bad.test"), CallContext::new())
        .await
        .unwrap_err();
    assert_eq!(error.code(), ErrorCode::Internal);
    assert_eq!(error.reason(), Some(reasons::SCOPE_TENANT_INVALID));
    assert_eq!(
        error.message(),
        "the scope resolver returned an invalid tenant"
    );
    assert!(error.metadata().is_empty());
}
