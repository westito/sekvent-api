//! Which request names which tenant: the selector rules and the selection of a request head.

use std::fmt;

use http::header::{HOST, ORIGIN};
use http::request::Parts;
use http::{HeaderMap, HeaderName, HeaderValue};
use sekvent_error::AppError;

use crate::error::serving;
use crate::host::{canonical_host, host_of_origin, is_loopback_host};
use crate::label::LabelRules;
use crate::{Selector, TenantId, TenantInfo};

/// The longest well-known name.
const WELL_KNOWN_MAX: usize = 64;

/// The path segment that introduces a well-known name under the prefix.
const WELL_KNOWN: &str = ".well-known/";

/// Which request names which tenant. Built by the product from its own keys.
#[derive(Clone)]
pub struct SelectorRules {
    /// The header that names the tenant by id, and the hosts it applies to.
    id_header: Option<IdHeader>,
    /// The path prefix that names the tenant by a label, and the hosts it applies to.
    path_label: Option<PathLabel>,
    /// The well-known names under the path-label prefix.
    well_known: Vec<String>,
    /// The product's label rules, applied to every path label.
    labels: LabelRules,
    /// Whether any other host is looked up as a tenant's own host.
    tenant_hosts: bool,
    /// RPC path prefixes that belong to no tenant.
    bypass: &'static [&'static str],
}

/// The id-header rule.
#[derive(Clone)]
struct IdHeader {
    /// The header's name.
    name: HeaderName,
    /// Canonical hosts.
    hosts: Vec<String>,
}

/// The path-label rule.
#[derive(Clone)]
struct PathLabel {
    /// Starts and ends with `/` and is longer than `/`.
    prefix: String,
    /// Canonical hosts.
    hosts: Vec<String>,
}

impl SelectorRules {
    /// Rules that select nothing.
    pub fn new() -> Self {
        Self {
            id_header: None,
            path_label: None,
            well_known: Vec::new(),
            labels: LabelRules::new(),
            tenant_hosts: false,
            bypass: &[],
        }
    }

    /// Requests to these hosts name the tenant by id in `header`. `INVALID_ARGUMENT` (naming the
    /// host's position, never its text) for a host `canonical_host` refuses, for a second call, and
    /// for a host that is also a path-label host and not loopback.
    pub fn id_header(
        mut self,
        header: HeaderName,
        hosts: impl IntoIterator<Item = String>,
    ) -> Result<Self, AppError> {
        if self.id_header.is_some() {
            return Err(AppError::invalid_argument(
                "the id-header hosts are already set",
            ));
        }
        let hosts = canonical_hosts("id-header", hosts)?;
        let others = self.path_label.as_ref().map(|rule| rule.hosts.as_slice());
        check_overlap("id-header", &hosts, others)?;
        self.id_header = Some(IdHeader {
            name: header,
            hosts,
        });
        Ok(self)
    }

    /// Requests to these hosts under `prefix` name the tenant by a path label. `prefix` starts and
    /// ends with `/`, names at least one segment (it is longer than `/`), and is visible ASCII
    /// without `%`, `?` or `#`. The prefix is a reserved namespace on every host: a path under it
    /// on any other host selects nothing (`TENANT_NOT_HERE`), which is why `/` alone, reserving
    /// every path, is refused. `INVALID_ARGUMENT` for a prefix these rules refuse, as above for a
    /// host, and for a second call.
    pub fn path_label(
        mut self,
        prefix: &str,
        hosts: impl IntoIterator<Item = String>,
    ) -> Result<Self, AppError> {
        if self.path_label.is_some() {
            return Err(AppError::invalid_argument(
                "the path-label hosts are already set",
            ));
        }
        if prefix == "/" {
            return Err(AppError::invalid_argument(
                "the path-label prefix `/` names no segment: it must start with `/`, end with `/` \
                 and be longer than `/`",
            ));
        }
        if !is_prefix(prefix) {
            return Err(AppError::invalid_argument(
                "the path-label prefix must start and end with `/`, be longer than `/`, and be \
                 visible ASCII without `%`, `?` or `#`",
            ));
        }
        let hosts = canonical_hosts("path-label", hosts)?;
        let others = self.id_header.as_ref().map(|rule| rule.hosts.as_slice());
        check_overlap("path-label", &hosts, others)?;
        self.path_label = Some(PathLabel {
            prefix: prefix.to_owned(),
            hosts,
        });
        Ok(self)
    }

    /// Names so that `<prefix>.well-known/<name>/<label>` selects like `<prefix><label>`. Each is
    /// 1 to 64 of `[A-Za-z0-9._-]`. `INVALID_ARGUMENT` otherwise. A second call replaces the
    /// list.
    pub fn well_known(mut self, names: impl IntoIterator<Item = String>) -> Result<Self, AppError> {
        let mut list = Vec::new();
        for (index, name) in names.into_iter().enumerate() {
            if !is_well_known_name(&name) {
                return Err(AppError::invalid_argument(format!(
                    "well-known name #{} must be 1 to 64 of `[A-Za-z0-9._-]`",
                    index + 1
                )));
            }
            list.push(name);
        }
        self.well_known = list;
        Ok(self)
    }

    /// The product's label rules: a path segment they refuse selects nothing and causes no read.
    /// Default `LabelRules::new()`.
    #[must_use]
    pub fn labels(self, rules: LabelRules) -> Self {
        Self {
            labels: rules,
            ..self
        }
    }

    /// Any other host is looked up as a tenant's own host.
    #[must_use]
    pub fn tenant_hosts(self, enabled: bool) -> Self {
        Self {
            tenant_hosts: enabled,
            ..self
        }
    }

    /// RPC path prefixes that belong to no tenant: an empty scope, no selection, checked before
    /// the host is looked at. A prefix is compared as a plain string prefix of the request path as
    /// received, never percent-decoded: `"/"` bypasses every request, and
    /// `/billing.v1.Admin` also matches `/billing.v1.AdminOther/List`, so a prefix should start
    /// and end with `/` (for example `/billing.health.v1.Health/`). An empty prefix matches
    /// nothing. A second call replaces the list.
    #[must_use]
    pub fn bypass(self, prefixes: &'static [&'static str]) -> Self {
        Self {
            bypass: prefixes,
            ..self
        }
    }
}

impl Default for SelectorRules {
    fn default() -> Self {
        Self::new()
    }
}

// `SelectorRules { id_header_hosts: 1, path_label_hosts: 1, well_known: 0, tenant_hosts: false,
// bypass: 1, .. }`
impl fmt::Debug for SelectorRules {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SelectorRules")
            .field(
                "id_header_hosts",
                &self.id_header.as_ref().map_or(0, |rule| rule.hosts.len()),
            )
            .field(
                "path_label_hosts",
                &self.path_label.as_ref().map_or(0, |rule| rule.hosts.len()),
            )
            .field("well_known", &self.well_known.len())
            .field("tenant_hosts", &self.tenant_hosts)
            .field("bypass", &self.bypass.len())
            .finish_non_exhaustive()
    }
}

/// Canonical forms of `hosts`; `INVALID_ARGUMENT` naming the first refused host's position.
fn canonical_hosts(
    list: &str,
    hosts: impl IntoIterator<Item = String>,
) -> Result<Vec<String>, AppError> {
    hosts
        .into_iter()
        .enumerate()
        .map(|(index, host)| {
            canonical_host(&host).map_err(|_| {
                AppError::invalid_argument(format!(
                    "{list} host #{} is not a valid host",
                    index + 1
                ))
            })
        })
        .collect()
}

/// `INVALID_ARGUMENT` naming the position of the first host of `hosts` that is also in `others`
/// and is not loopback.
fn check_overlap(list: &str, hosts: &[String], others: Option<&[String]>) -> Result<(), AppError> {
    let Some(others) = others else {
        return Ok(());
    };
    match hosts
        .iter()
        .position(|host| others.contains(host) && !is_loopback_host(host))
    {
        Some(index) => Err(AppError::invalid_argument(format!(
            "{list} host #{} is in both host lists and is not loopback",
            index + 1
        ))),
        None => Ok(()),
    }
}

/// Starts and ends with `/`, longer than `/`, visible ASCII, no `%`, `?` or `#`.
fn is_prefix(prefix: &str) -> bool {
    prefix.len() > 1
        && prefix.starts_with('/')
        && prefix.ends_with('/')
        && prefix
            .bytes()
            .all(|b| b.is_ascii_graphic() && !matches!(b, b'%' | b'?' | b'#'))
}

/// 1 to 64 of `[A-Za-z0-9._-]`.
fn is_well_known_name(name: &str) -> bool {
    (1..=WELL_KNOWN_MAX).contains(&name.len())
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// Whether `path` (a request's `uri.path()`, never percent-decoded) starts with one of `prefixes`
/// as a plain string prefix (so `"/"` matches every path and a prefix without a trailing `/` also
/// matches a longer service name). An empty prefix matches nothing. The one bypass rule of both
/// forms of `Tenants`.
pub(crate) fn is_bypass(prefixes: &[&str], path: &str) -> bool {
    prefixes
        .iter()
        .any(|prefix| !prefix.is_empty() && path.starts_with(prefix))
}

/// What a request head selects, before any lookup.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Selection {
    /// A bypass path: no tenant, an empty scope.
    Bypass,
    /// Look the selector up, check the row's state, then call `Selected::admit`.
    Tenant(Selected),
}

/// The selector of a request and the claims checked once its tenant is known.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Selected {
    /// What to look up.
    pub(crate) selector: Selector,
    /// The optional id header of a path-label or tenant-host request.
    pub(crate) claimed: IdClaim,
    /// The `Origin` of an id-header request.
    pub(crate) origin: OriginClaim,
}

/// The optional id header of a path-label or tenant-host request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum IdClaim {
    /// No id header.
    Absent,
    /// An id header that is repeated or not a tenant id.
    Invalid,
    /// The id the header names.
    Id(TenantId),
}

/// The `Origin` of an id-header request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum OriginClaim {
    /// Not an id-header request.
    NotChecked,
    /// No `Origin` header.
    Absent,
    /// A repeated or malformed `Origin`.
    Invalid,
    /// The canonical host of the one `Origin`.
    Host(String),
}

impl SelectorRules {
    /// The selection order (the path-label prefix is a reserved namespace on every host, with no
    /// fall-through), pure and without I/O. Errors carry no domain: `TENANT_NOT_HERE`,
    /// `TENANT_SELECTOR_MISSING`, `TENANT_MISMATCH`.
    pub(crate) fn select(&self, request: &Parts) -> Result<Selection, AppError> {
        let path = request.uri.path();
        if is_bypass(self.bypass, path) {
            return Ok(Selection::Bypass);
        }
        let Some(host) = request_host(request) else {
            return Err(serving::not_here());
        };
        if let Some(rule) = &self.path_label
            && let Some(rest) = path.strip_prefix(rule.prefix.as_str())
        {
            if !rule.hosts.contains(&host) {
                return Err(serving::not_here());
            }
            let segment = self.label_of(rest).ok_or_else(serving::not_here)?;
            return Ok(Selection::Tenant(Selected {
                selector: Selector::PathLabel {
                    segment,
                    listener_host: host,
                },
                claimed: self.claim(request),
                origin: OriginClaim::NotChecked,
            }));
        }
        if let Some(rule) = &self.id_header
            && rule.hosts.contains(&host)
        {
            let id = match id_claim(&request.headers, &rule.name) {
                IdClaim::Absent => return Err(serving::selector_missing()),
                IdClaim::Invalid => return Err(serving::mismatch()),
                IdClaim::Id(id) => id,
            };
            return Ok(Selection::Tenant(Selected {
                selector: Selector::Id(id),
                claimed: IdClaim::Absent,
                origin: origin_claim(&request.headers),
            }));
        }
        if self
            .path_label
            .as_ref()
            .is_some_and(|rule| rule.hosts.contains(&host))
        {
            return Err(serving::not_here());
        }
        if self.tenant_hosts {
            return Ok(Selection::Tenant(Selected {
                selector: Selector::Host(host),
                claimed: self.claim(request),
                origin: OriginClaim::NotChecked,
            }));
        }
        Err(serving::not_here())
    }

    /// Whether `host` (canonical) is one of the id-header hosts.
    pub(crate) fn is_id_header_host(&self, host: &str) -> bool {
        self.id_header
            .as_ref()
            .is_some_and(|rule| rule.hosts.iter().any(|candidate| candidate == host))
    }

    /// The lowered label of the path after the prefix, or `None` when it selects nothing.
    fn label_of(&self, rest: &str) -> Option<String> {
        let rest = match rest.strip_prefix(WELL_KNOWN) {
            Some(after) => {
                let (name, after) = after.split_once('/')?;
                if !self.well_known.iter().any(|known| known == name) {
                    return None;
                }
                after
            }
            None => rest,
        };
        let segment = rest.split_once('/').map_or(rest, |(segment, _)| segment);
        if segment.is_empty() || segment.contains('%') {
            return None;
        }
        self.labels.canonical(segment).ok()
    }

    /// The optional id header: `Absent` when no id header is configured.
    fn claim(&self, request: &Parts) -> IdClaim {
        self.id_header.as_ref().map_or(IdClaim::Absent, |rule| {
            id_claim(&request.headers, &rule.name)
        })
    }
}

impl Selected {
    /// The checks that follow the state check: a claimed id that is invalid or not `info.id` is
    /// `TENANT_MISMATCH`; an origin that is invalid or not exactly one of `info.hosts` is
    /// `ORIGIN_NOT_ALLOWED`. No domain.
    pub(crate) fn admit(&self, info: &TenantInfo) -> Result<(), AppError> {
        let id_matches = match self.claimed {
            IdClaim::Absent => true,
            IdClaim::Id(id) => id == info.id,
            IdClaim::Invalid => false,
        };
        if !id_matches {
            return Err(serving::mismatch());
        }
        let origin_allowed = match &self.origin {
            OriginClaim::NotChecked | OriginClaim::Absent => true,
            OriginClaim::Host(host) => info.hosts.contains(host),
            OriginClaim::Invalid => false,
        };
        if origin_allowed {
            Ok(())
        } else {
            Err(serving::origin_not_allowed())
        }
    }
}

/// The request's canonical host: the URI authority and the one `Host` header, which must agree
/// when both are present. `None` when absent, malformed, repeated, not text, carrying user info, or
/// disagreeing. Forwarded-host headers are never read.
pub(crate) fn request_host(request: &Parts) -> Option<String> {
    let from_uri = match request.uri.authority() {
        Some(authority) if authority.as_str().contains('@') => return None,
        Some(authority) => Some(canonical_host(authority.as_str()).ok()?),
        None => None,
    };
    let from_header = match field(&request.headers, &HOST) {
        Field::Absent => None,
        Field::One(value) => Some(canonical_host(value.to_str().ok()?).ok()?),
        Field::Repeated => return None,
    };
    match (from_uri, from_header) {
        (Some(uri), Some(header)) => (uri == header).then_some(uri),
        (Some(host), None) | (None, Some(host)) => Some(host),
        (None, None) => None,
    }
}

/// How often a header field occurs in a head.
enum Field<'a> {
    /// Not at all.
    Absent,
    /// Exactly once.
    One(&'a HeaderValue),
    /// More than once.
    Repeated,
}

/// The occurrences of `name` in `headers`.
fn field<'a>(headers: &'a HeaderMap, name: &HeaderName) -> Field<'a> {
    let mut values = headers.get_all(name).iter();
    match (values.next(), values.next()) {
        (None, _) => Field::Absent,
        (Some(value), None) => Field::One(value),
        (Some(_), Some(_)) => Field::Repeated,
    }
}

/// The id header `name` of a head: exactly one value in `TenantId`'s one text, or why not.
fn id_claim(headers: &HeaderMap, name: &HeaderName) -> IdClaim {
    match field(headers, name) {
        Field::Absent => IdClaim::Absent,
        Field::One(value) => value
            .to_str()
            .ok()
            .and_then(TenantId::parse)
            .map_or(IdClaim::Invalid, IdClaim::Id),
        Field::Repeated => IdClaim::Invalid,
    }
}

/// The `Origin` of a head: exactly one value `host_of_origin` accepts, or why not.
fn origin_claim(headers: &HeaderMap) -> OriginClaim {
    match field(headers, &ORIGIN) {
        Field::Absent => OriginClaim::Absent,
        Field::One(value) => value
            .to_str()
            .ok()
            .and_then(host_of_origin)
            .map_or(OriginClaim::Invalid, OriginClaim::Host),
        Field::Repeated => OriginClaim::Invalid,
    }
}

#[cfg(test)]
mod tests {
    use sekvent_error::ErrorCode;
    use uuid::Uuid;

    use super::*;
    use crate::reasons;
    use crate::{RoleKey, TenantState};

    const HEADER: &str = "x-tenant-id";
    const API: &str = "api.example.com";
    const GO: &str = "go.example.com";
    const HEALTH: &str = "/billing.health.v1.Health/";
    const UNARY: &str = "/billing.orders.v1.Orders/Get";

    fn header() -> HeaderName {
        HeaderName::from_static(HEADER)
    }

    fn tenant(n: u128) -> TenantId {
        TenantId::from_uuid(Uuid::from_u128(n))
    }

    fn rules() -> SelectorRules {
        SelectorRules::new()
            .id_header(header(), [API.to_owned()])
            .unwrap()
            .path_label("/t/", [GO.to_owned()])
            .unwrap()
            .well_known(["app-links".to_owned()])
            .unwrap()
            .labels(LabelRules::new().with_reserved(|label| label == "admin"))
            .bypass(&[HEALTH])
    }

    /// A head with an optional `Host` header, a URI and text headers.
    fn head(host: Option<&str>, uri: &str, headers: &[(&str, &str)]) -> Parts {
        let mut builder = http::Request::builder().uri(uri);
        if let Some(host) = host {
            builder = builder.header(HOST, host);
        }
        for (name, value) in headers {
            builder = builder.header(*name, *value);
        }
        builder.body(()).unwrap().into_parts().0
    }

    /// A head with text headers and one header whose value is not text.
    fn head_with_bytes(headers: &[(&str, &str)], uri: &str, name: &str, value: &[u8]) -> Parts {
        let mut builder = http::Request::builder().uri(uri);
        for (header, text) in headers {
            builder = builder.header(*header, *text);
        }
        builder
            .header(name, HeaderValue::from_bytes(value).unwrap())
            .body(())
            .unwrap()
            .into_parts()
            .0
    }

    fn selected(rules: &SelectorRules, request: &Parts) -> Selected {
        match rules.select(request) {
            Ok(Selection::Tenant(selected)) => selected,
            other => panic!("expected a tenant, got {other:?}"),
        }
    }

    fn refusal(rules: &SelectorRules, request: &Parts) -> (ErrorCode, String) {
        match rules.select(request) {
            Ok(selection) => panic!("expected a refusal, got {selection:?}"),
            Err(error) => {
                assert_eq!(error.domain(), None);
                (error.code(), error.reason().expect("a reason").to_owned())
            }
        }
    }

    fn not_here() -> (ErrorCode, String) {
        (ErrorCode::NotFound, reasons::TENANT_NOT_HERE.to_owned())
    }

    fn mismatch() -> (ErrorCode, String) {
        (
            ErrorCode::InvalidArgument,
            reasons::TENANT_MISMATCH.to_owned(),
        )
    }

    fn path_label(segment: &str, listener_host: &str) -> Selector {
        Selector::PathLabel {
            segment: segment.to_owned(),
            listener_host: listener_host.to_owned(),
        }
    }

    fn info(id: TenantId, hosts: &[&str]) -> TenantInfo {
        TenantInfo {
            id,
            subdomain: "billing".to_owned(),
            display_name: "Billing".to_owned(),
            state: TenantState::Active,
            primary_host: hosts[0].to_owned(),
            hosts: hosts.iter().map(|host| (*host).to_owned()).collect(),
            role_key: RoleKey {
                key_id: "k1".to_owned(),
                generation: 1,
            },
        }
    }

    #[test]
    fn the_request_host_comes_from_the_authority_or_the_host_header() {
        let authority = head(None, "https://Billing.Example.com/x", &[]);
        assert_eq!(
            request_host(&authority).as_deref(),
            Some("billing.example.com")
        );
        let host = head(Some("Billing.Example.com."), "/x", &[]);
        assert_eq!(request_host(&host).as_deref(), Some("billing.example.com"));
        let both = head(
            Some("BILLING.example.com."),
            "https://billing.example.com/x",
            &[],
        );
        assert_eq!(request_host(&both).as_deref(), Some("billing.example.com"));
        let loopback = head(Some("localhost:8080"), "/x", &[]);
        assert_eq!(request_host(&loopback).as_deref(), Some("localhost:8080"));
        let public_port = head(Some("billing.example.com:8443"), "/x", &[]);
        assert_eq!(
            request_host(&public_port).as_deref(),
            Some("billing.example.com")
        );
    }

    #[test]
    fn the_request_host_is_none_when_the_sources_are_missing_or_unusable() {
        let different = head(
            Some("orders.example.com"),
            "https://billing.example.com/x",
            &[],
        );
        assert_eq!(request_host(&different), None);
        let repeated = head(
            Some("billing.example.com"),
            "/x",
            &[("host", "billing.example.com")],
        );
        assert_eq!(request_host(&repeated), None);
        let not_text = head_with_bytes(&[], "/x", "host", b"caf\xe9.example.com");
        assert_eq!(request_host(&not_text), None);
        let user_info = head(None, "https://user@billing.example.com/x", &[]);
        assert_eq!(request_host(&user_info), None);
        let bad_authority = head(None, "https://bill_ing.example.com/x", &[]);
        assert_eq!(request_host(&bad_authority), None);
        let malformed = head(Some("bill ing.example.com"), "/x", &[]);
        assert_eq!(request_host(&malformed), None);
        let neither = head(None, "/x", &[]);
        assert_eq!(request_host(&neither), None);
        let forwarded = head(
            None,
            "/x",
            &[
                ("x-forwarded-host", "billing.example.com"),
                ("forwarded", "host=billing.example.com"),
            ],
        );
        assert_eq!(request_host(&forwarded), None);
    }

    #[test]
    fn a_bypass_prefix_matches_the_path_as_given() {
        let prefixes = [HEALTH];
        assert!(is_bypass(&prefixes, "/billing.health.v1.Health/Check"));
        assert!(!is_bypass(&prefixes, UNARY));
        assert!(!is_bypass(&[], "/billing.health.v1.Health/Check"));
        assert!(!is_bypass(&["", "/other/"], UNARY));
        assert!(!is_bypass(&prefixes, "/billing.health.v1%2EHealth/Check"));
        assert!(!is_bypass(
            &["billing.health.v1.Health/"],
            "/billing.health.v1.Health/Check"
        ));
    }

    #[test]
    fn a_bypass_prefix_is_a_plain_string_prefix() {
        assert!(is_bypass(&["/"], UNARY));
        assert!(is_bypass(&["/"], "/billing.health.v1.Health/Check"));
        assert!(is_bypass(
            &["/billing.v1.Admin"],
            "/billing.v1.AdminOther/List"
        ));
        assert!(!is_bypass(
            &["/billing.v1.Admin/"],
            "/billing.v1.AdminOther/List"
        ));
    }

    #[test]
    fn a_bypass_path_is_selected_before_the_host_is_looked_at() {
        let rules = rules();
        let path = "/billing.health.v1.Health/Check";
        assert_eq!(
            rules.select(&head(None, path, &[])).unwrap(),
            Selection::Bypass
        );
        assert_eq!(
            rules
                .select(&head(Some("bill ing"), path, &[(HEADER, "nonsense")]))
                .unwrap(),
            Selection::Bypass
        );
        assert_eq!(
            rules
                .select(&head(Some(API), path, &[(HEADER, "nonsense")]))
                .unwrap(),
            Selection::Bypass
        );
    }

    #[test]
    fn a_request_without_a_usable_host_is_not_here() {
        let rules = rules().tenant_hosts(true);
        assert_eq!(refusal(&rules, &head(None, UNARY, &[])), not_here());
        assert_eq!(
            refusal(&rules, &head(Some("bill ing.example.com"), UNARY, &[])),
            not_here()
        );
    }

    #[test]
    fn an_unknown_host_is_not_here_without_tenant_hosts() {
        let rules = rules();
        assert_eq!(
            refusal(&rules, &head(Some("billing.example.com"), UNARY, &[])),
            not_here()
        );
    }

    #[test]
    fn the_prefix_on_a_path_label_host_selects_the_lowered_label() {
        let rules = rules();
        let request = head(Some(GO), "/t/Billing/billing.orders.v1.Orders/Get", &[]);
        assert_eq!(
            selected(&rules, &request),
            Selected {
                selector: path_label("billing", GO),
                claimed: IdClaim::Absent,
                origin: OriginClaim::NotChecked,
            }
        );
    }

    #[test]
    fn whatever_follows_the_label_is_ignored() {
        let rules = rules();
        for path in ["/t/billing", "/t/billing/", "/t/billing/a/b/c?x=1"] {
            let request = head(Some(GO), path, &[]);
            assert_eq!(
                selected(&rules, &request).selector,
                path_label("billing", GO),
                "{path:?}"
            );
        }
    }

    #[test]
    fn the_prefix_on_an_id_header_host_is_not_here_even_with_a_valid_header() {
        let rules = rules();
        let id = tenant(7).to_string();
        assert_eq!(
            refusal(&rules, &head(Some(API), "/t/billing/x", &[])),
            not_here()
        );
        assert_eq!(
            refusal(
                &rules,
                &head(Some(API), "/t/billing/x", &[(HEADER, id.as_str())])
            ),
            not_here()
        );
    }

    #[test]
    fn the_prefix_on_any_other_host_is_not_here_even_with_tenant_hosts() {
        let rules = rules().tenant_hosts(true);
        assert_eq!(
            refusal(
                &rules,
                &head(Some("billing.example.com"), "/t/billing/x", &[])
            ),
            not_here()
        );
    }

    #[test]
    fn a_listed_well_known_name_selects_the_label_after_it() {
        let rules = rules();
        let request = head(Some(GO), "/t/.well-known/app-links/Billing/x", &[]);
        assert_eq!(
            selected(&rules, &request).selector,
            path_label("billing", GO)
        );
    }

    #[test]
    fn an_unlisted_or_incomplete_well_known_path_is_not_here() {
        let rules = rules();
        for path in [
            "/t/.well-known/other/billing/x",
            "/t/.well-known/app-links",
            "/t/.well-known/app-links/",
            "/t/.well-known/app-links//x",
            "/t/.well-known",
        ] {
            assert_eq!(
                refusal(&rules, &head(Some(GO), path, &[])),
                not_here(),
                "{path:?}"
            );
        }
    }

    #[test]
    fn an_empty_label_is_not_here() {
        let rules = rules();
        for path in ["/t/", "/t//x"] {
            assert_eq!(
                refusal(&rules, &head(Some(GO), path, &[])),
                not_here(),
                "{path:?}"
            );
        }
    }

    #[test]
    fn a_percent_in_the_label_is_not_here() {
        let rules = rules();
        assert_eq!(
            refusal(&rules, &head(Some(GO), "/t/bil%6Cing/x", &[])),
            not_here()
        );
    }

    #[test]
    fn a_label_the_label_rules_refuse_is_not_here() {
        let rules = rules();
        for path in ["/t/Admin/x", "/t/bill_ing/x", "/t/-billing/x"] {
            assert_eq!(
                refusal(&rules, &head(Some(GO), path, &[])),
                not_here(),
                "{path:?}"
            );
        }
    }

    #[test]
    fn the_id_header_is_required_on_an_id_header_host() {
        let rules = rules();
        assert_eq!(
            refusal(&rules, &head(Some(API), UNARY, &[])),
            (
                ErrorCode::InvalidArgument,
                reasons::TENANT_SELECTOR_MISSING.to_owned()
            )
        );
    }

    #[test]
    fn a_valid_id_header_selects_by_id() {
        let rules = rules();
        let id = tenant(7);
        let request = head(Some(API), UNARY, &[(HEADER, id.to_string().as_str())]);
        assert_eq!(
            selected(&rules, &request),
            Selected {
                selector: Selector::Id(id),
                claimed: IdClaim::Absent,
                origin: OriginClaim::Absent,
            }
        );
    }

    #[test]
    fn an_upper_case_repeated_or_malformed_id_header_is_a_mismatch() {
        let rules = rules();
        let id = tenant(0xab).to_string();
        let upper = id.to_ascii_uppercase();
        for headers in [
            vec![(HEADER, upper.as_str())],
            vec![(HEADER, id.as_str()), (HEADER, id.as_str())],
            vec![(HEADER, "billing")],
            vec![(HEADER, "")],
        ] {
            assert_eq!(
                refusal(&rules, &head(Some(API), UNARY, &headers)),
                mismatch(),
                "{headers:?}"
            );
        }
        let not_text = head_with_bytes(&[("host", API)], UNARY, HEADER, b"\xff\xfe");
        assert_eq!(refusal(&rules, &not_text), mismatch());
    }

    #[test]
    fn the_origin_of_an_id_header_request_is_claimed() {
        let rules = rules();
        let id = tenant(7).to_string();
        let with = |headers: &[(&str, &str)]| {
            let mut all = vec![(HEADER, id.as_str())];
            all.extend_from_slice(headers);
            selected(&rules, &head(Some(API), UNARY, &all)).origin
        };
        assert_eq!(with(&[]), OriginClaim::Absent);
        assert_eq!(
            with(&[("origin", "https://Billing.example.com")]),
            OriginClaim::Host("billing.example.com".to_owned())
        );
        assert_eq!(
            with(&[("origin", "http://localhost:5173")]),
            OriginClaim::Host("localhost:5173".to_owned())
        );
        for origin in [
            "http://billing.example.com",
            "https://billing.example.com/path",
            "https://billing.example.com:8443",
            "null",
        ] {
            assert_eq!(
                with(&[("origin", origin)]),
                OriginClaim::Invalid,
                "{origin:?}"
            );
        }
        assert_eq!(
            with(&[
                ("origin", "https://billing.example.com"),
                ("origin", "https://billing.example.com"),
            ]),
            OriginClaim::Invalid
        );
        let not_text = head_with_bytes(
            &[("host", API), (HEADER, id.as_str())],
            UNARY,
            "origin",
            b"https://caf\xe9.example",
        );
        assert_eq!(selected(&rules, &not_text).origin, OriginClaim::Invalid);
    }

    #[test]
    fn the_optional_id_header_of_a_path_request_is_claimed() {
        let rules = rules();
        let id = tenant(0xab);
        let text = id.to_string();
        let upper = text.to_ascii_uppercase();
        let claimed = |headers: &[(&str, &str)]| {
            selected(&rules, &head(Some(GO), "/t/billing/x", headers)).claimed
        };
        assert_eq!(claimed(&[]), IdClaim::Absent);
        assert_eq!(claimed(&[(HEADER, text.as_str())]), IdClaim::Id(id));
        assert_eq!(claimed(&[(HEADER, upper.as_str())]), IdClaim::Invalid);
        assert_eq!(
            claimed(&[(HEADER, text.as_str()), (HEADER, text.as_str())]),
            IdClaim::Invalid
        );
        let not_text = head_with_bytes(&[("host", GO)], "/t/billing/x", HEADER, b"\xff");
        assert_eq!(selected(&rules, &not_text).claimed, IdClaim::Invalid);
    }

    #[test]
    fn without_an_id_header_rule_nothing_is_claimed() {
        let rules = SelectorRules::new()
            .path_label("/t/", [GO.to_owned()])
            .unwrap()
            .tenant_hosts(true);
        let id = tenant(7).to_string();
        let path = head(Some(GO), "/t/billing/x", &[(HEADER, id.as_str())]);
        assert_eq!(selected(&rules, &path).claimed, IdClaim::Absent);
        let own = head(Some("billing.example.com"), UNARY, &[(HEADER, id.as_str())]);
        assert_eq!(selected(&rules, &own).claimed, IdClaim::Absent);
        assert!(!rules.is_id_header_host(API));
    }

    #[test]
    fn a_path_label_host_outside_the_prefix_is_not_here_even_with_tenant_hosts() {
        for rules in [rules(), rules().tenant_hosts(true)] {
            assert_eq!(refusal(&rules, &head(Some(GO), UNARY, &[])), not_here());
        }
    }

    #[test]
    fn tenant_hosts_select_any_other_host() {
        let rules = rules().tenant_hosts(true);
        let id = tenant(9);
        let request = head(
            Some("Billing.example.com"),
            UNARY,
            &[(HEADER, id.to_string().as_str())],
        );
        assert_eq!(
            selected(&rules, &request),
            Selected {
                selector: Selector::Host("billing.example.com".to_owned()),
                claimed: IdClaim::Id(id),
                origin: OriginClaim::NotChecked,
            }
        );
        let off = rules.tenant_hosts(false);
        assert_eq!(
            refusal(&off, &head(Some("billing.example.com"), UNARY, &[])),
            not_here()
        );
    }

    #[test]
    fn a_loopback_host_in_both_lists_selects_by_path_and_by_header() {
        let loopback = "localhost:8080";
        let rules = SelectorRules::new()
            .id_header(header(), [loopback.to_owned()])
            .unwrap()
            .path_label("/t/", [loopback.to_owned()])
            .unwrap();
        let by_path = head(Some(loopback), "/t/orders/x", &[]);
        assert_eq!(
            selected(&rules, &by_path).selector,
            path_label("orders", loopback)
        );
        let id = tenant(3);
        let by_header = head(Some(loopback), UNARY, &[(HEADER, id.to_string().as_str())]);
        assert_eq!(selected(&rules, &by_header).selector, Selector::Id(id));
        assert!(rules.is_id_header_host(loopback));
        assert!(!rules.is_id_header_host(GO));
    }

    #[test]
    fn admit_checks_the_claimed_id() {
        let id = tenant(1);
        let tenant_info = info(id, &["billing.example.com"]);
        let with = |claimed| Selected {
            selector: Selector::Id(id),
            claimed,
            origin: OriginClaim::NotChecked,
        };
        assert!(with(IdClaim::Absent).admit(&tenant_info).is_ok());
        assert!(with(IdClaim::Id(id)).admit(&tenant_info).is_ok());
        for claimed in [IdClaim::Id(tenant(2)), IdClaim::Invalid] {
            let error = with(claimed).admit(&tenant_info).unwrap_err();
            assert_eq!(error.code(), ErrorCode::InvalidArgument);
            assert_eq!(error.reason(), Some(reasons::TENANT_MISMATCH));
            assert_eq!(error.domain(), None);
        }
    }

    #[test]
    fn admit_checks_the_origin_against_every_host_of_the_tenant() {
        let id = tenant(1);
        let tenant_info = info(id, &["billing.example.com", "pay.example.com"]);
        let with = |origin| Selected {
            selector: Selector::Id(id),
            claimed: IdClaim::Absent,
            origin,
        };
        for origin in [
            OriginClaim::NotChecked,
            OriginClaim::Absent,
            OriginClaim::Host("billing.example.com".to_owned()),
            OriginClaim::Host("pay.example.com".to_owned()),
        ] {
            assert!(
                with(origin.clone()).admit(&tenant_info).is_ok(),
                "{origin:?}"
            );
        }
        for origin in [
            OriginClaim::Invalid,
            OriginClaim::Host("orders.example.com".to_owned()),
            OriginClaim::Host("example.com".to_owned()),
        ] {
            let error = with(origin.clone()).admit(&tenant_info).unwrap_err();
            assert_eq!(error.code(), ErrorCode::PermissionDenied, "{origin:?}");
            assert_eq!(error.reason(), Some(reasons::ORIGIN_NOT_ALLOWED));
            assert_eq!(error.domain(), None);
        }
    }

    #[test]
    fn admit_checks_the_id_before_the_origin() {
        let claims = Selected {
            selector: Selector::Host("billing.example.com".to_owned()),
            claimed: IdClaim::Id(tenant(2)),
            origin: OriginClaim::Invalid,
        };
        let error = claims
            .admit(&info(tenant(1), &["billing.example.com"]))
            .unwrap_err();
        assert_eq!(error.reason(), Some(reasons::TENANT_MISMATCH));
    }

    #[test]
    fn a_refused_host_is_named_by_its_position() {
        let error = SelectorRules::new()
            .id_header(
                header(),
                [API.to_owned(), "bill ing.example.com".to_owned()],
            )
            .unwrap_err();
        assert_eq!(error.code(), ErrorCode::InvalidArgument);
        assert!(error.message().contains("#2"), "{}", error.message());
        assert!(!error.message().contains("bill ing"));
        let error = SelectorRules::new()
            .path_label("/t/", ["bad_host".to_owned()])
            .unwrap_err();
        assert!(error.message().contains("#1"), "{}", error.message());
        assert!(!error.message().contains("bad_host"));
    }

    #[test]
    fn each_host_list_is_set_once() {
        let twice = rules().id_header(header(), [API.to_owned()]);
        assert_eq!(twice.unwrap_err().code(), ErrorCode::InvalidArgument);
        let twice = rules().path_label("/p/", [GO.to_owned()]);
        assert_eq!(twice.unwrap_err().code(), ErrorCode::InvalidArgument);
    }

    #[test]
    fn a_host_in_both_lists_must_be_loopback() {
        let error = SelectorRules::new()
            .id_header(header(), [API.to_owned()])
            .unwrap()
            .path_label("/t/", [GO.to_owned(), API.to_owned()])
            .unwrap_err();
        assert_eq!(error.code(), ErrorCode::InvalidArgument);
        assert!(error.message().contains("#2"), "{}", error.message());
        let error = SelectorRules::new()
            .path_label("/t/", [API.to_owned()])
            .unwrap()
            .id_header(header(), [API.to_owned()])
            .unwrap_err();
        assert!(error.message().contains("#1"), "{}", error.message());
        assert!(!error.message().contains(API));
        let both = SelectorRules::new()
            .path_label("/t/", ["LOCALHOST:8080".to_owned()])
            .unwrap()
            .id_header(header(), ["localhost:8080".to_owned()]);
        assert!(both.is_ok());
    }

    #[test]
    fn the_prefix_is_checked() {
        for prefix in [
            "",
            "t/",
            "/t",
            "/t%2F/",
            "/t?/",
            "/t#/",
            "/t x/",
            "/t\u{e9}/",
        ] {
            let error = SelectorRules::new()
                .path_label(prefix, [GO.to_owned()])
                .unwrap_err();
            assert_eq!(error.code(), ErrorCode::InvalidArgument, "{prefix:?}");
        }
        for prefix in ["/t/", "/tenants/by-label/"] {
            let rules = SelectorRules::new().path_label(prefix, [GO.to_owned()]);
            assert!(rules.is_ok(), "{prefix:?}");
        }
    }

    #[test]
    fn a_prefix_of_only_a_slash_is_refused() {
        let error = SelectorRules::new()
            .path_label("/", [GO.to_owned()])
            .unwrap_err();
        assert_eq!(error.code(), ErrorCode::InvalidArgument);
        assert_eq!(
            error.message(),
            "the path-label prefix `/` names no segment: it must start with `/`, end with `/` and \
             be longer than `/`"
        );
        assert!(!error.message().contains(GO));
        assert!(
            SelectorRules::new()
                .id_header(header(), [API.to_owned()])
                .unwrap()
                .path_label("/", [GO.to_owned()])
                .is_err()
        );
    }

    #[test]
    fn well_known_names_are_checked() {
        let long = "a".repeat(WELL_KNOWN_MAX);
        let too_long = "a".repeat(WELL_KNOWN_MAX + 1);
        assert!(
            SelectorRules::new()
                .well_known(["app-links".to_owned(), "a.b_c-D9".to_owned(), long])
                .is_ok()
        );
        for name in [String::new(), too_long, "a/b".to_owned(), "a b".to_owned()] {
            let error = SelectorRules::new()
                .well_known(["ok".to_owned(), name.clone()])
                .unwrap_err();
            assert_eq!(error.code(), ErrorCode::InvalidArgument, "{name:?}");
            assert!(error.message().contains("#2"), "{}", error.message());
        }
    }

    #[test]
    fn a_second_bypass_call_replaces_the_list() {
        let rules = rules().bypass(&["/billing.admin.v1.Admin/"]);
        let health = head(Some(API), "/billing.health.v1.Health/Check", &[]);
        assert_eq!(refusal(&rules, &health).1, reasons::TENANT_SELECTOR_MISSING);
        let admin = head(None, "/billing.admin.v1.Admin/List", &[]);
        assert_eq!(rules.select(&admin).unwrap(), Selection::Bypass);
    }

    #[test]
    fn debug_prints_counts_and_no_host() {
        let text = format!("{:?}", rules().tenant_hosts(true));
        assert_eq!(
            text,
            "SelectorRules { id_header_hosts: 1, path_label_hosts: 1, well_known: 1, \
             tenant_hosts: true, bypass: 1, .. }"
        );
        assert!(!text.contains("example.com"));
        let empty = format!("{:?}", SelectorRules::default());
        assert_eq!(
            empty,
            "SelectorRules { id_header_hosts: 0, path_label_hosts: 0, well_known: 0, \
             tenant_hosts: false, bypass: 0, .. }"
        );
    }
}
