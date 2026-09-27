use std::sync::Arc;

use http::HeaderMap;
use http::header::AUTHORIZATION;
use http::request::Parts;
use sekvent_context::ServiceIdentity;

use crate::TokenMap;

/// The caller-visible message when service authentication fails. It is the
/// same for a missing, malformed or unknown token.
pub const REJECTED_MESSAGE: &str = "service authentication required";

/// A pluggable request authenticator, the shape a server's context layer
/// accepts to learn the calling service.
pub type Authenticator = Arc<dyn Fn(&Parts) -> Option<ServiceIdentity> + Send + Sync>;

/// The service identified by the `Authorization: Bearer` header in
/// `headers`, if any.
pub fn authenticate_headers(map: &TokenMap, headers: &HeaderMap) -> Option<ServiceIdentity> {
    let value = headers.get(AUTHORIZATION)?.to_str().ok()?;
    map.authenticate(bearer_token(value)?)
}

/// An [`Authenticator`] backed by `map`.
pub fn authenticator(map: Arc<TokenMap>) -> Authenticator {
    Arc::new(move |parts: &Parts| authenticate_headers(&map, &parts.headers))
}

/// The token of an `Authorization: Bearer <token>` value. The scheme is
/// case-insensitive; the token is taken verbatim, never trimmed.
fn bearer_token(value: &str) -> Option<&str> {
    let (scheme, token) = value.split_once(' ')?;
    (scheme.eq_ignore_ascii_case("bearer") && !token.is_empty()).then_some(token)
}

/// axum middleware: authenticate the calling service, insert its
/// [`ServiceIdentity`] into the request extensions, or answer
/// `UNAUTHENTICATED` with [`REJECTED_MESSAGE`].
///
/// ```ignore
/// let app = Router::new()
///     .route("/internal/orders", post(create_order))
///     .layer(axum::middleware::from_fn_with_state(map, sekvent_link::require_service));
///
/// async fn create_order(Extension(caller): Extension<ServiceIdentity>) { /* … */ }
/// ```
#[cfg(feature = "axum")]
pub async fn require_service(
    axum::extract::State(map): axum::extract::State<Arc<TokenMap>>,
    mut request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Result<axum::response::Response, sekvent_error::AppError> {
    let Some(identity) = authenticate_headers(&map, request.headers()) else {
        tracing::debug!("service token rejected");
        return Err(sekvent_error::AppError::unauthenticated(REJECTED_MESSAGE));
    };
    request.extensions_mut().insert(identity);
    Ok(next.run(request).await)
}

/// tonic interceptor: authenticate the calling service, insert its
/// [`ServiceIdentity`] into the request extensions, or fail with
/// `UNAUTHENTICATED` and [`REJECTED_MESSAGE`].
#[cfg(feature = "tonic")]
#[derive(Debug, Clone)]
pub struct ServiceTokenInterceptor {
    map: Arc<TokenMap>,
}

#[cfg(feature = "tonic")]
impl ServiceTokenInterceptor {
    /// An interceptor accepting the tokens in `map`.
    pub fn new(map: Arc<TokenMap>) -> Self {
        Self { map }
    }
}

#[cfg(feature = "tonic")]
impl tonic::service::Interceptor for ServiceTokenInterceptor {
    fn call(
        &mut self,
        mut request: tonic::Request<()>,
    ) -> Result<tonic::Request<()>, tonic::Status> {
        let identity = request
            .metadata()
            .get("authorization")
            .and_then(|value| value.to_str().ok())
            .and_then(bearer_token)
            .and_then(|token| self.map.authenticate(token));
        let Some(identity) = identity else {
            tracing::debug!("service token rejected");
            return Err(sekvent_error::AppError::unauthenticated(REJECTED_MESSAGE).into());
        };
        request.extensions_mut().insert(identity);
        Ok(request)
    }
}

#[cfg(test)]
mod tests {
    use http::HeaderValue;
    use sekvent_config::Secret;

    use super::*;
    use crate::{BearerInjector, InboundLink};

    const BILLING: &str = "billing-token-0123456789abcdefABCDEF";
    const ORDERS: &str = "orders_token_0123456789abcdefABCDEF";

    fn map() -> Arc<TokenMap> {
        Arc::new(
            TokenMap::new([
                InboundLink::trusted("billing", Secret::new(BILLING)),
                InboundLink::untrusted("orders", Secret::new(ORDERS)),
            ])
            .unwrap(),
        )
    }

    fn parts(authorization: Option<&str>) -> Parts {
        let mut builder = http::Request::builder();
        if let Some(value) = authorization {
            builder = builder.header(AUTHORIZATION, value);
        }
        builder.body(()).unwrap().into_parts().0
    }

    #[test]
    fn bearer_token_parsing() {
        assert_eq!(bearer_token("Bearer abc"), Some("abc"));
        assert_eq!(bearer_token("bEaReR abc"), Some("abc"));
        assert_eq!(bearer_token("Bearer  abc"), Some(" abc"));
        assert_eq!(bearer_token("Bearer "), None);
        assert_eq!(bearer_token("Bearer"), None);
        assert_eq!(bearer_token("Token abc"), None);
    }

    #[test]
    fn authenticator_reads_parts() {
        let authenticate = authenticator(map());
        assert_eq!(
            authenticate(&parts(Some(&format!("Bearer {BILLING}")))),
            Some(ServiceIdentity::trusted("billing"))
        );
        assert_eq!(
            authenticate(&parts(Some(&format!("bearer {ORDERS}")))),
            Some(ServiceIdentity::untrusted("orders"))
        );
        for authorization in [
            None,
            Some(format!("Bearer  {BILLING}")),
            Some(format!("Basic {BILLING}")),
            Some(BILLING.to_owned()),
            Some("Bearer unknown-token-0123456789abcdefABCDEF".to_owned()),
        ] {
            assert_eq!(
                authenticate(&parts(authorization.as_deref())),
                None,
                "{authorization:?}"
            );
        }
    }

    #[test]
    fn non_ascii_header_is_refused() {
        let mut headers = HeaderMap::new();
        headers.insert(
            AUTHORIZATION,
            HeaderValue::from_bytes(b"Bearer \xfftoken").unwrap(),
        );
        assert_eq!(authenticate_headers(&map(), &headers), None);
    }

    #[test]
    fn injector_output_is_accepted() {
        let mut headers = HeaderMap::new();
        BearerInjector::new("billing", &Secret::new(BILLING))
            .unwrap()
            .apply(&mut headers);
        assert_eq!(
            authenticate_headers(&map(), &headers),
            Some(ServiceIdentity::trusted("billing"))
        );
    }

    #[cfg(feature = "axum")]
    mod axum_middleware {
        use axum::body::{Body, to_bytes};
        use axum::routing::get;
        use axum::{Extension, Router};
        use http::StatusCode;
        use tower::{Layer, ServiceExt};

        use super::*;

        async fn whoami(Extension(caller): Extension<ServiceIdentity>) -> String {
            format!("{}:{}", caller.name, caller.trusted)
        }

        fn app() -> Router {
            Router::new()
                .route("/whoami", get(whoami))
                .layer(axum::middleware::from_fn_with_state(map(), require_service))
        }

        async fn body(response: axum::response::Response) -> String {
            let bytes = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
            String::from_utf8(bytes.to_vec()).unwrap()
        }

        #[tokio::test]
        async fn injector_and_middleware_round_trip() {
            let injector = BearerInjector::new("orders", &Secret::new(ORDERS)).unwrap();
            let client = injector.layer(app());
            let request = http::Request::builder()
                .uri("/whoami")
                .body(Body::empty())
                .unwrap();
            let response = client.oneshot(request).await.unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            assert_eq!(body(response).await, "orders:false");
        }

        #[tokio::test]
        async fn unknown_or_missing_token_is_unauthenticated() {
            for authorization in [None, Some("Bearer not-a-known-token-0123456789abcdef")] {
                let mut request = http::Request::builder().uri("/whoami");
                if let Some(value) = authorization {
                    request = request.header(AUTHORIZATION, value);
                }
                let response = app()
                    .oneshot(request.body(Body::empty()).unwrap())
                    .await
                    .unwrap();
                assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
                let text = body(response).await;
                assert!(!text.contains("not-a-known-token"), "{text}");
            }
        }
    }

    #[cfg(feature = "tonic")]
    mod tonic_interceptor {
        use tonic::Code;
        use tonic::service::Interceptor;

        use super::*;

        #[test]
        fn injector_and_interceptor_round_trip() {
            let mut outbound = BearerInjector::new("billing", &Secret::new(BILLING)).unwrap();
            let mut inbound = ServiceTokenInterceptor::new(map());
            let request = outbound.call(tonic::Request::new(())).unwrap();
            let request = inbound.call(request).unwrap();
            assert_eq!(
                request.extensions().get::<ServiceIdentity>(),
                Some(&ServiceIdentity::trusted("billing"))
            );
            assert!(format!("{inbound:?}").contains("billing"));
            assert!(!format!("{inbound:?}").contains(BILLING));
        }

        #[test]
        fn missing_or_unknown_token_is_unauthenticated() {
            let inbound = ServiceTokenInterceptor::new(map());
            for authorization in [
                None,
                Some("Bearer not-a-known-token-0123456789abcdef"),
                Some("Bearer "),
            ] {
                let mut request = tonic::Request::new(());
                if let Some(value) = authorization {
                    request
                        .metadata_mut()
                        .insert("authorization", value.parse().unwrap());
                }
                let status = inbound.clone().call(request).unwrap_err();
                assert_eq!(status.code(), Code::Unauthenticated);
                assert_eq!(status.message(), REJECTED_MESSAGE);
            }
        }
    }
}
