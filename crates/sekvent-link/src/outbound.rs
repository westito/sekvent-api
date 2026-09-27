use std::fmt;
use std::task::{Context, Poll};

use http::header::AUTHORIZATION;
use http::{HeaderMap, HeaderValue};
use sekvent_config::Secret;
use tower::{Layer, Service};

use crate::LinkError;
use crate::token::validate_token;

/// Attaches `Authorization: Bearer <token>` for one outbound link.
///
/// The token is validated as canonical at construction and used verbatim;
/// the header value is marked sensitive, and `Debug` shows the link only.
///
/// Use it directly on a [`HeaderMap`] ([`apply`](Self::apply)), as a tower
/// [`Layer`] around an HTTP client service, or — with the `tonic` feature —
/// as a `tonic::service::Interceptor`.
#[derive(Clone)]
pub struct BearerInjector {
    link: String,
    value: HeaderValue,
    #[cfg(feature = "tonic")]
    metadata: tonic::metadata::MetadataValue<tonic::metadata::Ascii>,
}

impl BearerInjector {
    /// An injector presenting `token` on calls to `link`.
    pub fn new(link: impl Into<String>, token: &Secret) -> Result<Self, LinkError> {
        let link = link.into();
        validate_token(&link, token.expose())?;
        let header = Secret::new(format!("Bearer {}", token.expose()));
        let non_canonical = || LinkError::NonCanonical { link: link.clone() };
        let mut value = HeaderValue::from_str(header.expose()).map_err(|_| non_canonical())?;
        value.set_sensitive(true);
        #[cfg(feature = "tonic")]
        let metadata = {
            let mut metadata: tonic::metadata::MetadataValue<tonic::metadata::Ascii> =
                header.expose().parse().map_err(|_| non_canonical())?;
            metadata.set_sensitive(true);
            metadata
        };
        Ok(Self {
            link,
            value,
            #[cfg(feature = "tonic")]
            metadata,
        })
    }

    /// The link this injector authenticates to.
    pub fn link(&self) -> &str {
        &self.link
    }

    /// Set the `Authorization` header, replacing any existing one.
    pub fn apply(&self, headers: &mut HeaderMap) {
        headers.insert(AUTHORIZATION, self.value.clone());
    }
}

impl fmt::Debug for BearerInjector {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BearerInjector")
            .field("link", &self.link)
            .finish_non_exhaustive()
    }
}

impl<S> Layer<S> for BearerInjector {
    type Service = InjectBearer<S>;

    fn layer(&self, inner: S) -> Self::Service {
        InjectBearer {
            inner,
            injector: self.clone(),
        }
    }
}

/// The service produced by [`BearerInjector`] as a tower [`Layer`].
#[derive(Debug, Clone)]
pub struct InjectBearer<S> {
    inner: S,
    injector: BearerInjector,
}

impl<S, B> Service<http::Request<B>> for InjectBearer<S>
where
    S: Service<http::Request<B>>,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = S::Future;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, mut request: http::Request<B>) -> Self::Future {
        self.injector.apply(request.headers_mut());
        self.inner.call(request)
    }
}

#[cfg(feature = "tonic")]
impl tonic::service::Interceptor for BearerInjector {
    fn call(
        &mut self,
        mut request: tonic::Request<()>,
    ) -> Result<tonic::Request<()>, tonic::Status> {
        request
            .metadata_mut()
            .insert("authorization", self.metadata.clone());
        Ok(request)
    }
}

#[cfg(test)]
mod tests {
    use std::convert::Infallible;

    use tower::ServiceExt;

    use super::*;

    const TOKEN: &str = "billing-token-0123456789abcdefABCDEF";

    fn injector() -> BearerInjector {
        BearerInjector::new("billing", &Secret::new(TOKEN)).unwrap()
    }

    #[test]
    fn apply_sets_a_sensitive_header() {
        let mut headers = HeaderMap::new();
        headers.insert(AUTHORIZATION, HeaderValue::from_static("Basic old"));
        injector().apply(&mut headers);
        let value = headers.get(AUTHORIZATION).unwrap();
        assert_eq!(value.to_str().unwrap(), format!("Bearer {TOKEN}"));
        assert!(value.is_sensitive());
        assert_eq!(headers.get_all(AUTHORIZATION).iter().count(), 1);
    }

    #[test]
    fn construction_validates_without_trimming() {
        for token in [
            String::new(),
            "short".to_owned(),
            format!(" {TOKEN}"),
            format!("{TOKEN}\n"),
        ] {
            let error = BearerInjector::new("billing", &Secret::new(token)).unwrap_err();
            assert!(error.to_string().contains("billing"));
            assert!(!error.to_string().contains(TOKEN));
        }
    }

    #[test]
    fn debug_shows_the_link_only() {
        let injector = injector();
        assert_eq!(injector.link(), "billing");
        let debug = format!("{injector:?}");
        assert!(debug.contains("billing"));
        assert!(!debug.contains(TOKEN));
        let service = injector.layer(());
        assert!(!format!("{service:?}").contains(TOKEN));
    }

    #[tokio::test]
    async fn layer_injects_on_every_request() {
        let echo = tower::service_fn(|request: http::Request<()>| async move {
            Ok::<_, Infallible>(
                request
                    .headers()
                    .get(AUTHORIZATION)
                    .map(|value| value.to_str().unwrap().to_owned()),
            )
        });
        let service = injector().layer(echo);
        let seen = service.oneshot(http::Request::new(())).await.unwrap();
        assert_eq!(seen, Some(format!("Bearer {TOKEN}")));
    }

    #[cfg(feature = "tonic")]
    #[test]
    fn tonic_interceptor_sets_metadata() {
        use tonic::service::Interceptor;

        let request = injector().call(tonic::Request::new(())).unwrap();
        let value = request.metadata().get("authorization").unwrap();
        assert_eq!(value.to_str().unwrap(), format!("Bearer {TOKEN}"));
    }
}
