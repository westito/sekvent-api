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
        r#"AppBuilder { components: ["notes"], resources: ["alloc::string::String", "u32"], end_user_authenticator: false, .. }"#
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
    assert!(app.grpc_services().is_empty());
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
    assert!(app.components().is_empty());
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
