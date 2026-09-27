use axum::Json;
use axum::Router;
use axum::extract::State;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use http::StatusCode;
use serde_json::{Map, Value, json};

use crate::health::{HealthRegistry, Readiness};
use crate::probe::ProbeStatus;

/// How much the HTTP health endpoints reveal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum HealthVisibility {
    /// Only the verdict. Safe for anonymous or public exposure.
    #[default]
    Minimal,
    /// The verdict plus probe names and states, lifecycle flags and the
    /// build version. For internal networks only.
    Full,
}

#[derive(Clone)]
struct HealthState {
    registry: HealthRegistry,
    visibility: HealthVisibility,
}

impl HealthRegistry {
    /// `GET /livez`, `GET /readyz` and `GET /healthz` (an alias of
    /// `/readyz`), answering 200 when healthy and 503 otherwise.
    ///
    /// `/livez` has a tiny text body; `/readyz` a JSON object whose detail
    /// depends on `visibility`.
    pub fn http_routes(&self, visibility: HealthVisibility) -> Router {
        Router::new()
            .route("/livez", get(livez))
            .route("/readyz", get(readyz))
            .route("/healthz", get(readyz))
            .with_state(HealthState {
                registry: self.clone(),
                visibility,
            })
    }
}

async fn livez(State(state): State<HealthState>) -> Response {
    if state.registry.is_live() {
        (StatusCode::OK, "ok\n").into_response()
    } else {
        (StatusCode::SERVICE_UNAVAILABLE, "not live\n").into_response()
    }
}

async fn readyz(State(state): State<HealthState>) -> Response {
    let readiness = state.registry.readiness();
    let code = if readiness.ready {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    (code, Json(readiness_body(&readiness, state.visibility))).into_response()
}

fn readiness_body(readiness: &Readiness, visibility: HealthVisibility) -> Value {
    let verdict = if readiness.ready {
        "ready"
    } else {
        "not_ready"
    };
    if visibility == HealthVisibility::Minimal {
        return json!({ "status": verdict });
    }

    let probes: Vec<Value> = readiness
        .probes
        .iter()
        .map(|probe| {
            let mut entry = Map::new();
            entry.insert("name".into(), probe.name.clone().into());
            entry.insert("required".into(), probe.required.into());
            let status = probe.status.map_or("unknown", ProbeStatus::as_str);
            entry.insert("status".into(), status.into());
            if let Some(ProbeStatus::Down(failure)) = probe.status {
                entry.insert("failure".into(), failure.kind().into());
                entry.insert("detail".into(), failure.detail().into());
            }
            Value::Object(entry)
        })
        .collect();

    let mut body = json!({
        "status": verdict,
        "live": readiness.live,
        "started": readiness.started,
        "draining": readiness.draining,
        "probes": probes,
    });
    if let (Some(version), Value::Object(fields)) = (&readiness.version, &mut body) {
        fields.insert("version".into(), version.clone().into());
    }
    body
}

#[cfg(test)]
mod tests {
    use axum::body::{Body, to_bytes};
    use http::Request;
    use tower::ServiceExt;

    use super::*;
    use crate::probe::ProbeFailure;

    async fn get_path(router: &Router, path: &str) -> (StatusCode, String) {
        let response = router
            .clone()
            .oneshot(Request::get(path).body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        (status, String::from_utf8(body.to_vec()).unwrap())
    }

    #[tokio::test]
    async fn minimal_output_reveals_only_the_verdict() {
        let registry = HealthRegistry::new();
        registry.set_version("9.9.9");
        registry.register_probe("secret-db", true);
        let router = registry.http_routes(HealthVisibility::default());

        assert_eq!(
            get_path(&router, "/livez").await,
            (StatusCode::OK, "ok\n".into())
        );
        let (status, body) = get_path(&router, "/readyz").await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body, r#"{"status":"not_ready"}"#);

        registry.mark_started().await;
        registry.record_probe("secret-db", ProbeStatus::Up);
        let (status, body) = get_path(&router, "/healthz").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, r#"{"status":"ready"}"#);
        assert!(!body.contains("secret-db") && !body.contains("9.9.9"));
    }

    #[tokio::test]
    async fn full_output_lists_probes_flags_and_version() {
        let registry = HealthRegistry::new();
        registry.set_version("1.0.0");
        registry.register_probe("db", true);
        registry.register_probe("cache", false);
        registry.register_probe("search", false);
        registry.mark_started().await;
        registry.record_probe(
            "db",
            ProbeStatus::Down(ProbeFailure::Rejected("bad credentials")),
        );
        registry.record_probe("cache", ProbeStatus::Up);
        let router = registry.http_routes(HealthVisibility::Full);

        let (status, body) = get_path(&router, "/readyz").await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        let json: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(json["status"], "not_ready");
        assert_eq!(json["live"], true);
        assert_eq!(json["started"], true);
        assert_eq!(json["draining"], false);
        assert_eq!(json["version"], "1.0.0");
        assert_eq!(
            json["probes"],
            json!([
                {"name": "cache", "required": false, "status": "up"},
                {"name": "db", "required": true, "status": "down",
                 "failure": "rejected", "detail": "bad credentials"},
                {"name": "search", "required": false, "status": "unknown"},
            ])
        );
    }

    #[tokio::test]
    async fn full_output_without_a_version_omits_it() {
        let registry = HealthRegistry::new();
        registry.mark_started().await;
        let router = registry.http_routes(HealthVisibility::Full);
        let (status, body) = get_path(&router, "/readyz").await;
        assert_eq!(status, StatusCode::OK);
        let json: Value = serde_json::from_str(&body).unwrap();
        assert!(json.get("version").is_none());
        assert_eq!(json["probes"], json!([]));
    }

    #[tokio::test]
    async fn a_fatal_process_fails_liveness() {
        let registry = HealthRegistry::new();
        registry.mark_fatal("wedged").await;
        let router = registry.http_routes(HealthVisibility::Full);
        let (status, body) = get_path(&router, "/livez").await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body, "not live\n");
        assert!(!body.contains("wedged"));
    }
}
