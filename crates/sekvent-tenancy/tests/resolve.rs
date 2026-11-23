//! Tenant selection on a real control database: the resolver's scopes, the state table, the claims
//! checked after the state, the reserved path prefix, streaming and bypass paths, the host snapshot
//! and the loopback path label.
#![cfg(all(feature = "serving", feature = "lifecycle"))]

mod common;

use std::sync::Arc;

use common::product::{OrdersFactory, OrdersScope};
use http::HeaderName;
use http::request::Parts;
use sekvent_component::ScopeResolver;
use sekvent_context::{CallContext, RequestScope};
use sekvent_error::{AppError, ErrorCode};
use sekvent_tenancy::hosts::{self, HostSnapshot};
use sekvent_tenancy::label::LabelRules;
use sekvent_tenancy::{
    ErrorDomains, Registry, SelectorRules, StreamTenant, TenantId, TenantLease, TenantResolver,
    TenantState, Tenants, reasons,
};
use sqlx::PgPool;

const HEADER: &str = "x-tenant-id";
const API: &str = "api.example.com";
const GO: &str = "go.example.com";
const LOOPBACK: &str = "localhost:8080";
const HEALTH: &str = "/billing.health.v1.Health/";
const HEALTH_CHECK: &str = "/billing.health.v1.Health/Check";
const UNARY: &str = "/billing.orders.v1.Orders/Get";
const STREAM: &str = "billing.orders.v1.Orders/Watch";
const STREAMING: &[&str] = &[STREAM];
const SLASHED_STREAMING: &[&str] = &["/billing.orders.v1.Orders/Watch"];
fn header() -> HeaderName {
    HeaderName::from_static(HEADER)
}

/// An id-header host, a path-label host under `/t/`, tenant hosts on and a bypass prefix.
fn rules() -> SelectorRules {
    SelectorRules::new()
        .id_header(header(), [API.to_owned()])
        .expect("the id-header rule is valid")
        .path_label("/t/", [GO.to_owned()])
        .expect("the path-label rule is valid")
        .tenant_hosts(true)
        .bypass(&[HEALTH])
}

/// The control database, the test product and a resolver over them.
struct Served {
    registry: Registry,
    control: PgPool,
    factory: OrdersFactory,
    tenants: Tenants<OrdersScope>,
    resolver: TenantResolver<OrdersScope>,
}

impl Served {
    async fn start(cluster: &common::Cluster, rules: SelectorRules) -> Self {
        let (registry, control) = cluster.registry(Arc::new(common::manual_clock())).await;
        let factory = OrdersFactory::new();
        let tenants = cluster.tenants(&registry, rules, &factory, 8);
        let resolver = TenantResolver::new(tenants.clone(), STREAMING);
        Self {
            registry,
            control,
            factory,
            tenants,
            resolver,
        }
    }

    async fn resolve(&self, request: &Parts) -> Result<RequestScope, AppError> {
        self.resolver.resolve(request, &CallContext::new()).await
    }
}

/// A head to `host` at `path` with these headers.
fn head(host: &str, path: &str, headers: &[(&str, &str)]) -> Parts {
    let mut builder = http::Request::builder()
        .uri(path)
        .header(http::header::HOST, host);
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    builder.body(()).unwrap().into_parts().0
}

/// A head at `path` without a host.
fn head_without_host(path: &str) -> Parts {
    http::Request::builder()
        .uri(path)
        .body(())
        .unwrap()
        .into_parts()
        .0
}

fn assert_tenant(scope: &RequestScope, id: TenantId) {
    let text = id.to_string();
    assert_eq!(scope.tenant(), Some(text.as_str()));
    let lease = scope
        .extension::<TenantLease<OrdersScope>>()
        .expect("the scope holds a lease");
    assert_eq!(lease.info().id, id);
    assert_eq!(lease.tenant, id);
    assert_eq!(scope.extension::<StreamTenant>(), None);
}

fn assert_empty(scope: &RequestScope) {
    assert_eq!(scope.tenant(), None);
    assert!(scope.extension::<TenantLease<OrdersScope>>().is_none());
    assert_eq!(scope.extension::<StreamTenant>(), None);
}

fn assert_refused(result: Result<RequestScope, AppError>, code: ErrorCode, reason: &str) {
    let error = result.expect_err("the request is refused");
    assert_eq!(error.code(), code, "{reason}");
    assert_eq!(error.reason(), Some(reason));
    assert_eq!(error.domain(), Some(ErrorDomains::DEFAULT_SERVING));
}

fn assert_not_here(result: Result<RequestScope, AppError>) {
    assert_refused(result, ErrorCode::NotFound, reasons::TENANT_NOT_HERE);
}

#[tokio::test]
#[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
async fn an_active_tenant_resolves_by_header_by_path_label_and_by_its_own_host() {
    let Some(cluster) = common::Cluster::start().await else {
        return;
    };
    common::guard(async {
        let served = Served::start(&cluster, rules()).await;
        let row = cluster
            .create_tenant(
                &served.control,
                "billing",
                &["billing.example.com", "pay.example.com"],
            )
            .await;
        let id = row.info.id;
        let text = id.to_string();

        let by_header = served
            .resolve(&head(API, UNARY, &[(HEADER, text.as_str())]))
            .await
            .expect("the header selects the tenant");
        assert_tenant(&by_header, id);
        let by_path = served
            .resolve(&head(GO, "/t/billing/billing.orders.v1.Orders/Get", &[]))
            .await
            .expect("the path label selects the tenant");
        assert_tenant(&by_path, id);
        let by_host = served
            .resolve(&head("pay.example.com", UNARY, &[]))
            .await
            .expect("the tenant's own host selects it");
        assert_tenant(&by_host, id);
        assert_eq!(served.factory.built(), 1);
    })
    .await;
}

#[tokio::test]
#[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
async fn each_state_is_refused_through_the_header_and_through_the_path() {
    let Some(cluster) = common::Cluster::start().await else {
        return;
    };
    common::guard(async {
        let served = Served::start(&cluster, rules()).await;
        for (state, code, reason) in [
            (
                TenantState::Provisioning,
                ErrorCode::Unavailable,
                reasons::TENANT_PROVISIONING,
            ),
            (
                TenantState::Suspended,
                ErrorCode::FailedPrecondition,
                reasons::TENANT_SUSPENDED,
            ),
            (
                TenantState::Purging,
                ErrorCode::NotFound,
                reasons::TENANT_NOT_HERE,
            ),
            (
                TenantState::Purged,
                ErrorCode::NotFound,
                reasons::TENANT_NOT_HERE,
            ),
        ] {
            let label = state.as_str();
            let host = format!("{label}.example.com");
            let row = cluster
                .insert_row(&served.control, label, &[host.as_str()], state)
                .await;
            let text = row.info.id.to_string();
            let by_header = served
                .resolve(&head(API, UNARY, &[(HEADER, text.as_str())]))
                .await;
            assert_refused(by_header, code, reason);
            let by_path = served
                .resolve(&head(GO, &format!("/t/{label}/x"), &[]))
                .await;
            assert_refused(by_path, code, reason);
        }
        let unknown = common::new_tenant().to_string();
        assert_not_here(
            served
                .resolve(&head(API, UNARY, &[(HEADER, unknown.as_str())]))
                .await,
        );
        assert_not_here(served.resolve(&head(GO, "/t/unknown/x", &[])).await);
        assert_not_here(
            served
                .resolve(&head("unknown.example.com", UNARY, &[]))
                .await,
        );
        assert_eq!(served.factory.built(), 0);
    })
    .await;
}

#[tokio::test]
#[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
async fn the_optional_id_header_is_compared_after_the_state() {
    let Some(cluster) = common::Cluster::start().await else {
        return;
    };
    common::guard(async {
        let served = Served::start(&cluster, rules()).await;
        let row = cluster
            .create_tenant(&served.control, "billing", &["billing.example.com"])
            .await;
        let id = row.info.id;
        let text = id.to_string();
        let other = common::new_tenant().to_string();

        let matching = served
            .resolve(&head(GO, "/t/billing/x", &[(HEADER, text.as_str())]))
            .await
            .expect("a matching id header passes");
        assert_tenant(&matching, id);
        let mismatch = (ErrorCode::InvalidArgument, reasons::TENANT_MISMATCH);
        for claimed in [other.as_str(), "billing"] {
            let result = served
                .resolve(&head(GO, "/t/billing/x", &[(HEADER, claimed)]))
                .await;
            assert_refused(result, mismatch.0, mismatch.1);
            let result = served
                .resolve(&head("billing.example.com", UNARY, &[(HEADER, claimed)]))
                .await;
            assert_refused(result, mismatch.0, mismatch.1);
        }

        cluster
            .insert_row(
                &served.control,
                "frozen",
                &["frozen.example.com"],
                TenantState::Suspended,
            )
            .await;
        let suspended = served
            .resolve(&head(GO, "/t/frozen/x", &[(HEADER, other.as_str())]))
            .await;
        assert_refused(
            suspended,
            ErrorCode::FailedPrecondition,
            reasons::TENANT_SUSPENDED,
        );
    })
    .await;
}

#[tokio::test]
#[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
async fn the_origin_of_a_header_request_must_be_a_host_of_the_tenant() {
    let Some(cluster) = common::Cluster::start().await else {
        return;
    };
    common::guard(async {
        let served = Served::start(&cluster, rules()).await;
        let row = cluster
            .create_tenant(
                &served.control,
                "billing",
                &["billing.example.com", "pay.example.com"],
            )
            .await;
        cluster
            .insert_row(
                &served.control,
                "orders",
                &["orders.example.com"],
                TenantState::Active,
            )
            .await;
        let id = row.info.id;
        let text = id.to_string();

        let without = served
            .resolve(&head(API, UNARY, &[(HEADER, text.as_str())]))
            .await
            .expect("a request without an origin passes");
        assert_tenant(&without, id);
        for origin in ["https://billing.example.com", "https://pay.example.com"] {
            let scope = served
                .resolve(&head(
                    API,
                    UNARY,
                    &[(HEADER, text.as_str()), ("origin", origin)],
                ))
                .await
                .expect("an origin of the tenant passes");
            assert_tenant(&scope, id);
        }
        for origin in ["https://orders.example.com", "http://billing.example.com"] {
            let result = served
                .resolve(&head(
                    API,
                    UNARY,
                    &[(HEADER, text.as_str()), ("origin", origin)],
                ))
                .await;
            assert_refused(
                result,
                ErrorCode::PermissionDenied,
                reasons::ORIGIN_NOT_ALLOWED,
            );
        }
        let by_path = served
            .resolve(&head(
                GO,
                "/t/billing/x",
                &[("origin", "https://orders.example.com")],
            ))
            .await
            .expect("a path request is not checked against its origin");
        assert_tenant(&by_path, id);
    })
    .await;
}

#[tokio::test]
#[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
async fn the_path_label_prefix_is_reserved_on_every_host() {
    let Some(cluster) = common::Cluster::start().await else {
        return;
    };
    common::guard(async {
        let served = Served::start(&cluster, rules()).await;
        let row = cluster
            .insert_row(
                &served.control,
                "billing",
                &["billing.example.com"],
                TenantState::Active,
            )
            .await;
        let text = row.info.id.to_string();
        assert_not_here(
            served
                .resolve(&head(API, "/t/billing/x", &[(HEADER, text.as_str())]))
                .await,
        );
        assert_not_here(
            served
                .resolve(&head("billing.example.com", "/t/billing/x", &[]))
                .await,
        );
        assert_not_here(served.resolve(&head(GO, UNARY, &[])).await);
        assert_eq!(served.factory.built(), 0);
    })
    .await;
}

#[test]
fn a_path_label_prefix_that_reserves_every_path_is_refused() {
    let error = SelectorRules::new()
        .id_header(header(), [API.to_owned()])
        .expect("the id-header rule is valid")
        .path_label("/", [GO.to_owned()])
        .expect_err("`/` names no segment");
    assert_eq!(error.code(), ErrorCode::InvalidArgument);
    assert!(
        error.message().contains("path-label prefix"),
        "{}",
        error.message()
    );
    assert!(!error.message().contains(GO));
    assert!(!error.message().contains(API));
}

#[tokio::test]
#[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
async fn a_streaming_path_gets_the_id_only_and_builds_nothing() {
    let Some(cluster) = common::Cluster::start().await else {
        return;
    };
    common::guard(async {
        let served = Served::start(&cluster, rules()).await;
        let row = cluster
            .create_tenant(&served.control, "billing", &["billing.example.com"])
            .await;
        let id = row.info.id;
        let text = id.to_string();

        let scope = served
            .resolve(&head(
                API,
                &format!("/{STREAM}"),
                &[(HEADER, text.as_str())],
            ))
            .await
            .expect("a streaming path resolves");
        assert_eq!(scope.tenant(), Some(text.as_str()));
        assert_eq!(scope.extension::<StreamTenant>(), Some(&StreamTenant(id)));
        assert!(scope.extension::<TenantLease<OrdersScope>>().is_none());
        assert_eq!(served.factory.built(), 0);
        assert_eq!(served.tenants.stats().creations, 0);

        let slashed = TenantResolver::new(served.tenants.clone(), SLASHED_STREAMING);
        let scope = slashed
            .resolve(
                &head(API, &format!("/{STREAM}"), &[(HEADER, text.as_str())]),
                &CallContext::new(),
            )
            .await
            .expect("a streaming path listed with a leading slash resolves");
        assert_eq!(scope.extension::<StreamTenant>(), Some(&StreamTenant(id)));
        assert!(scope.extension::<TenantLease<OrdersScope>>().is_none());
        assert_eq!(served.factory.built(), 0);
        assert_eq!(served.tenants.stats().creations, 0);

        let lease = served.tenants.get(id).await.expect("the stream opens it");
        assert_eq!(lease.info().id, id);
        assert_eq!(served.factory.built(), 1);
    })
    .await;
}

#[tokio::test]
#[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
async fn a_bypass_path_gets_an_empty_scope_without_a_control_read() {
    let Some(cluster) = common::Cluster::start().await else {
        return;
    };
    common::guard(async {
        let served = Served::start(&cluster, rules()).await;
        let mut held = Vec::new();
        for _ in 0..served.control.options().get_max_connections() {
            held.push(
                served
                    .control
                    .acquire()
                    .await
                    .expect("a control connection"),
            );
        }
        for request in [
            head_without_host(HEALTH_CHECK),
            head(API, HEALTH_CHECK, &[(HEADER, "nonsense")]),
            head(GO, HEALTH_CHECK, &[]),
            head("billing.example.com", HEALTH_CHECK, &[]),
        ] {
            let scope = served
                .resolve(&request)
                .await
                .expect("a bypass path resolves without the control database");
            assert_empty(&scope);
        }
        drop(held);
        assert_eq!(served.tenants.stats().creations, 0);
        assert_eq!(served.factory.built(), 0);
    })
    .await;
}

#[tokio::test]
#[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
async fn refresh_hosts_follows_the_hosts_of_the_active_tenants() {
    let Some(cluster) = common::Cluster::start().await else {
        return;
    };
    common::guard(async {
        let served = Served::start(&cluster, rules()).await;
        let billing = cluster
            .insert_row(
                &served.control,
                "billing",
                &["billing.example.com", "pay.example.com"],
                TenantState::Active,
            )
            .await;
        let orders = cluster
            .insert_row(
                &served.control,
                "orders",
                &["orders.example.com"],
                TenantState::Active,
            )
            .await;
        cluster
            .insert_row(
                &served.control,
                "frozen",
                &["frozen.example.com"],
                TenantState::Suspended,
            )
            .await;

        let snapshot = HostSnapshot::new();
        let read = hosts::refresh_hosts(&snapshot, &served.registry)
            .await
            .expect("the active tenants were read");
        let mut ids: Vec<TenantId> = read.iter().map(|info| info.id).collect();
        ids.sort();
        let mut expected = vec![billing.info.id, orders.info.id];
        expected.sort();
        assert_eq!(ids, expected);
        assert_eq!(snapshot.len(), 3);
        assert_eq!(snapshot.tenant_of("pay.example.com"), Some(billing.info.id));
        assert_eq!(snapshot.tenant_of("frozen.example.com"), None);

        let allowed = hosts::predicate(snapshot.clone(), rules());
        let ask = |origin: &str| allowed(origin, &head(API, UNARY, &[("origin", origin)]));
        assert!(ask("https://pay.example.com"));
        assert!(ask("https://orders.example.com"));
        assert!(!ask("https://frozen.example.com"));

        cluster
            .set_hosts(
                &served.control,
                billing.info.id,
                "billing",
                &["billing.example.com"],
            )
            .await;
        hosts::refresh_hosts(&snapshot, &served.registry)
            .await
            .expect("the active tenants were read again");
        assert!(!ask("https://pay.example.com"));
        assert!(ask("https://billing.example.com"));

        cluster
            .set_state(&served.control, orders.info.id, TenantState::Suspended)
            .await;
        let read = hosts::refresh_hosts(&snapshot, &served.registry)
            .await
            .expect("the active tenants were read a third time");
        assert_eq!(read.len(), 1);
        assert_eq!(snapshot.len(), 1);
        assert_eq!(snapshot.tenant_of("orders.example.com"), None);
        assert!(!ask("https://orders.example.com"));
    })
    .await;
}

#[tokio::test]
#[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
async fn a_failed_refresh_leaves_the_snapshot_as_it_was() {
    let Some(cluster) = common::Cluster::start().await else {
        return;
    };
    common::guard(async {
        let served = Served::start(&cluster, rules()).await;
        let billing = cluster
            .insert_row(
                &served.control,
                "billing",
                &["billing.example.com"],
                TenantState::Active,
            )
            .await;
        let snapshot = HostSnapshot::new();
        hosts::refresh_hosts(&snapshot, &served.registry)
            .await
            .expect("the active tenants were read");
        assert_eq!(snapshot.len(), 1);

        let pool = cluster.control_pool_of(Registry::MIN_CONNECTIONS).await;
        let second = Registry::new(pool.clone(), Arc::new(common::manual_clock()))
            .expect("a pool of 3 is enough");
        pool.close().await;
        assert!(
            hosts::refresh_hosts(&snapshot, &second).await.is_err(),
            "a closed pool cannot read"
        );
        assert_eq!(snapshot.len(), 1);
        assert_eq!(
            snapshot.tenant_of("billing.example.com"),
            Some(billing.info.id)
        );
    })
    .await;
}

#[tokio::test]
#[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
async fn a_loopback_path_label_selects_its_one_tenant_only() {
    let Some(cluster) = common::Cluster::start().await else {
        return;
    };
    common::guard(async {
        let rules = SelectorRules::new()
            .id_header(header(), [LOOPBACK.to_owned()])
            .expect("the id-header rule is valid")
            .path_label("/t/", [LOOPBACK.to_owned()])
            .expect("a loopback host may be in both lists");
        let served = Served::start(&cluster, rules).await;
        let row = cluster
            .create_tenant(&served.control, "orders", &["orders.localhost:3000"])
            .await;
        let id = row.info.id;
        let request = head(LOOPBACK, "/t/orders/billing.orders.v1.Orders/Get", &[]);
        let scope = served
            .resolve(&request)
            .await
            .expect("the loopback label selects its tenant");
        assert_tenant(&scope, id);
        let text = id.to_string();
        let by_header = served
            .resolve(&head(LOOPBACK, UNARY, &[(HEADER, text.as_str())]))
            .await
            .expect("the header selects outside the prefix");
        assert_tenant(&by_header, id);

        cluster
            .insert_row(
                &served.control,
                "orders",
                &["orders.example.com"],
                TenantState::Active,
            )
            .await;
        assert_not_here(served.resolve(&request).await);
    })
    .await;
}

#[tokio::test]
#[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
async fn a_loopback_path_label_needs_a_canonical_host_and_an_allowed_segment() {
    let Some(cluster) = common::Cluster::start().await else {
        return;
    };
    common::guard(async {
        let rules = SelectorRules::new()
            .path_label("/t/", [LOOPBACK.to_owned()])
            .expect("the path-label rule is valid")
            .labels(
                LabelRules::new()
                    .with_length(3, 20)
                    .expect("valid lengths")
                    .with_reserved(|label| label == "admin"),
            );
        let served = Served::start(&cluster, rules).await;
        let path = |segment: &str| format!("/t/{segment}/billing.orders.v1.Orders/Get");
        cluster
            .insert_row(
                &served.control,
                "upper",
                &["upper.localhost:5173"],
                TenantState::Active,
            )
            .await;
        cluster
            .admin_execute(
                cluster.control_database(),
                "UPDATE tenant_hosts SET host = 'UPPER.localhost:5173' \
                 WHERE host = 'upper.localhost:5173'",
            )
            .await;
        assert_not_here(served.resolve(&head(LOOPBACK, &path("upper"), &[])).await);

        for label in ["admin", "ab", "orders"] {
            let host = format!("{label}.localhost:3000");
            cluster
                .insert_row(
                    &served.control,
                    label,
                    &[host.as_str()],
                    TenantState::Active,
                )
                .await;
        }
        // A closed pool fails every read: the refused segments never ask for a connection.
        served.control.close().await;
        for segment in ["admin", "Admin", "ab"] {
            assert_not_here(served.resolve(&head(LOOPBACK, &path(segment), &[])).await);
        }
        let read = served
            .resolve(&head(LOOPBACK, &path("orders"), &[]))
            .await
            .expect_err("the allowed segment reads the closed pool");
        assert_ne!(read.reason(), Some(reasons::TENANT_NOT_HERE));
        assert_eq!(served.tenants.stats().creations, 0);
        assert_eq!(served.factory.built(), 0);
    })
    .await;
}
