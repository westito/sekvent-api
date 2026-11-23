//! Calling a product's admin RPC by its path.

use std::fmt;
use std::time::Duration;

use reqwest::header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderValue};
use reqwest::{StatusCode, Url};
use sekvent_error::{AppError, ErrorCode};
use serde::Serialize;

use super::frame::{self, MAX_REPLY_BYTES};
use super::minter::Minter;
use crate::{TenantId, host};

/// The content type of a gRPC-Web call and its answer.
const GRPC_WEB: &str = "application/grpc-web+proto";
/// The largest `grpc-timeout` value: eight digits.
const MAX_TIMEOUT_MILLIS: u128 = 99_999_999;

/// What a call came to, and what the caller may assume.
#[derive(Debug)]
#[non_exhaustive]
pub enum CallOutcome<T> {
    /// The product answered OK with this reply.
    Replied(T),
    /// Encoding or minting failed, or no connection was made: nothing happened; safe to try
    /// again.
    NotSent(AppError),
    /// The product answered a status other than OK: it decided, with its code, message, reason,
    /// domain, metadata and field violations. Not proof that nothing changed.
    Refused(AppError),
    /// A failure after sending, a malformed reply, the deadline, or `UNAVAILABLE` without a
    /// reason: anything may have happened; read the tenant back, and repeat only an operation
    /// that converges.
    Unknown(AppError),
}

impl<T> CallOutcome<T> {
    /// The reply, or the error of any other outcome.
    pub fn into_result(self) -> Result<T, AppError> {
        match self {
            Self::Replied(reply) => Ok(reply),
            Self::NotSent(error) | Self::Refused(error) | Self::Unknown(error) => Err(error),
        }
    }
}

/// One product instance's admin API. `Debug` prints the base URL and the audience.
pub struct AdminClient {
    base_url: String,
    audience: String,
    minter: Minter,
    http: reqwest::Client,
}

impl fmt::Debug for AdminClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AdminClient")
            .field("base_url", &self.base_url)
            .field("audience", &self.audience)
            .finish_non_exhaustive()
    }
}

impl AdminClient {
    /// The admin API at `base_url` (the admin listener's URL, without the RPC path) of the
    /// instance whose tokens are for `audience`. `https` for any host; `http` only for a loopback
    /// host. `INVALID_ARGUMENT` for another scheme, a URL with user info, a query or a fragment.
    pub fn new(base_url: &str, audience: &str, minter: Minter) -> Result<Self, AppError> {
        Self::build(base_url, audience, minter, false)
    }

    /// [`new`](Self::new), also accepting `http` for any host: for an admin listener reached
    /// inside one cluster network only.
    pub fn in_cluster(base_url: &str, audience: &str, minter: Minter) -> Result<Self, AppError> {
        Self::build(base_url, audience, minter, true)
    }

    fn build(
        base_url: &str,
        audience: &str,
        minter: Minter,
        any_http: bool,
    ) -> Result<Self, AppError> {
        let base_url = base_of(base_url, any_http)?;
        let http = sekvent_client::reqwest_builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|error| AppError::internal(error.without_url()))?;
        Ok(Self {
            base_url,
            audience: audience.to_owned(),
            minter,
            http,
        })
    }

    /// One unary call of the RPC `op` (`<package>.<Service>/<Rpc>`) with a fresh token, within
    /// `deadline`: no automatic retry, no redirect.
    pub async fn call<Req, Rep, X>(
        &self,
        op: &str,
        tenant: Option<TenantId>,
        subject: &str,
        extra: &X,
        deadline: Duration,
        request: &Req,
    ) -> CallOutcome<Rep>
    where
        Req: prost::Message,
        Rep: prost::Message + Default,
        X: Serialize,
    {
        if !is_op(op) {
            return CallOutcome::NotSent(AppError::invalid_argument(
                "the admin operation is not <package>.<Service>/<Rpc>",
            ));
        }
        let minted = match self.minter.mint(&self.audience, tenant, op, subject, extra) {
            Ok(minted) => minted,
            Err(error) => return CallOutcome::NotSent(error),
        };
        let body = match frame::encode(request) {
            Ok(body) => body,
            Err(error) => return CallOutcome::NotSent(error),
        };
        let Ok(mut bearer) = HeaderValue::from_str(&format!("Bearer {}", minted.token.expose()))
        else {
            return CallOutcome::NotSent(AppError::internal(
                "the admin token is not a header value",
            ));
        };
        bearer.set_sensitive(true);
        let request = self
            .http
            .post(format!("{}/{op}", self.base_url))
            .header(CONTENT_TYPE, GRPC_WEB)
            .header(ACCEPT, GRPC_WEB)
            .header("x-grpc-web", "1")
            .header(AUTHORIZATION, bearer)
            .header("grpc-timeout", grpc_timeout(deadline))
            .body(body);
        let Ok(exchanged) = tokio::time::timeout(deadline, exchange(request)).await else {
            return CallOutcome::Unknown(AppError::deadline_exceeded(
                "the admin call did not finish within its deadline",
            ));
        };
        let (headers, body) = match exchanged {
            Ok(answer) => answer,
            Err(Failure::NotSent(error)) => return CallOutcome::NotSent(error),
            Err(Failure::Unknown(error)) => return CallOutcome::Unknown(error),
        };
        match frame::decode_reply::<Rep>(&headers, &body) {
            Ok(Ok(reply)) => CallOutcome::Replied(reply),
            Ok(Err(error)) => answered(error),
            Err(malformed) => CallOutcome::Unknown(
                AppError::new(
                    ErrorCode::Unknown,
                    "the admin API's reply could not be read",
                )
                .with_source(format!("malformed gRPC-Web reply: {malformed:?}")),
            ),
        }
    }
}

/// The base URL of an admin API: `https`, or `http` on a loopback host (on any host when
/// `any_http`); no user info, query or fragment; without a trailing `/`. Errors never echo the
/// URL.
fn base_of(text: &str, any_http: bool) -> Result<String, AppError> {
    let Ok(url) = Url::parse(text) else {
        return Err(AppError::invalid_argument(
            "the admin API URL is not an absolute URL",
        ));
    };
    if !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(AppError::invalid_argument(
            "the admin API URL may not carry user info, a query or a fragment",
        ));
    }
    let Some(name) = url.host_str() else {
        return Err(AppError::invalid_argument(
            "the admin API URL names no host",
        ));
    };
    let allowed = match url.scheme() {
        "https" => true,
        "http" => {
            any_http
                || host::canonical_host(name)
                    .is_ok_and(|canonical| host::is_loopback_host(&canonical))
        }
        _ => false,
    };
    if !allowed {
        return Err(AppError::invalid_argument(
            "the admin API URL must be https, or http on a loopback host",
        ));
    }
    Ok(url.as_str().trim_end_matches('/').to_owned())
}

/// Whether `op` is `<package>.<Service>/<Rpc>`: dot-separated non-empty names of
/// `[A-Za-z0-9_]`, a slash, and one such name.
fn is_op(op: &str) -> bool {
    let name = |text: &str| {
        !text.is_empty()
            && text
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
    };
    op.split_once('/')
        .is_some_and(|(service, rpc)| service.split('.').all(name) && name(rpc))
}

/// The `grpc-timeout` value of `deadline`: whole milliseconds, at least 1, at most eight digits.
fn grpc_timeout(deadline: Duration) -> String {
    let millis = deadline.as_millis().clamp(1, MAX_TIMEOUT_MILLIS);
    format!("{millis}m")
}

/// How an exchange failed short of an answer to decode.
enum Failure {
    /// No connection was made.
    NotSent(AppError),
    /// Anything may have happened.
    Unknown(AppError),
}

impl Failure {
    /// A failure of `send`: no connection made, or a request that may have left.
    fn of_send(error: &reqwest::Error) -> Self {
        if error.is_connect() || error.is_builder() {
            Self::NotSent(AppError::unavailable("the admin API could not be reached"))
        } else {
            Self::Unknown(AppError::new(
                ErrorCode::Unknown,
                "the admin call failed after the request was sent",
            ))
        }
    }

    /// With `error`, its URL removed, as the source.
    fn with_source(self, error: reqwest::Error) -> Self {
        let source = error.without_url();
        match self {
            Self::NotSent(app) => Self::NotSent(app.with_source(source)),
            Self::Unknown(app) => Self::Unknown(app.with_source(source)),
        }
    }
}

/// Sends `request` and reads the answer's headers and whole body (at most [`MAX_REPLY_BYTES`]);
/// a status other than `200 OK` is a failure.
async fn exchange(request: reqwest::RequestBuilder) -> Result<(HeaderMap, Vec<u8>), Failure> {
    let mut response = request
        .send()
        .await
        .map_err(|error| Failure::of_send(&error).with_source(error))?;
    let status = response.status();
    if status != StatusCode::OK {
        return Err(Failure::Unknown(
            AppError::new(
                ErrorCode::Unknown,
                "the admin API answered an unexpected HTTP status",
            )
            .with_metadata("http_status", status.as_str()),
        ));
    }
    let headers = std::mem::take(response.headers_mut());
    let mut body = Vec::new();
    loop {
        let chunk = match response.chunk().await {
            Ok(Some(chunk)) => chunk,
            Ok(None) => break,
            Err(error) => {
                return Err(Failure::Unknown(AppError::new(
                    ErrorCode::Unknown,
                    "the admin API's reply was cut off",
                ))
                .with_source(error));
            }
        };
        if chunk.len() > MAX_REPLY_BYTES - body.len() {
            return Err(Failure::Unknown(AppError::new(
                ErrorCode::Unknown,
                "the admin API's reply is too large",
            )));
        }
        body.extend_from_slice(&chunk);
    }
    Ok((headers, body))
}

/// The outcome of a status other than OK: `Unknown` for `UNAVAILABLE` without a reason (a proxy
/// or a server going away may answer that after the product acted), else `Refused`.
fn answered<T>(error: AppError) -> CallOutcome<T> {
    if error.code() == ErrorCode::Unavailable && error.reason().is_none() {
        CallOutcome::Unknown(error)
    } else {
        CallOutcome::Refused(error)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use sekvent_auth::{EdDsaSigningKey, NoClaims};
    use sekvent_context::SystemClock;

    use super::*;

    fn minter() -> Minter {
        let key = EdDsaSigningKey::from_seed("ops-1", &[3; 32]).unwrap();
        Minter::new("ops-console", key, Arc::new(SystemClock))
    }

    fn not_sent<T: fmt::Debug>(outcome: CallOutcome<T>) -> AppError {
        match outcome {
            CallOutcome::NotSent(error) => error,
            other => panic!("not NotSent: {other:?}"),
        }
    }

    fn base(url: &str) -> Result<String, ErrorCode> {
        AdminClient::new(url, "orders-eu", minter())
            .map(|client| client.base_url)
            .map_err(|error| error.code())
    }

    fn base_in_cluster(url: &str) -> Result<String, ErrorCode> {
        AdminClient::in_cluster(url, "orders-eu", minter())
            .map(|client| client.base_url)
            .map_err(|error| error.code())
    }

    #[test]
    fn https_is_accepted_for_any_host() {
        assert_eq!(
            base("https://admin.example.com"),
            Ok("https://admin.example.com".into())
        );
        assert_eq!(
            base("https://admin.example.com:8443/admin/"),
            Ok("https://admin.example.com:8443/admin".into())
        );
        assert_eq!(
            base_in_cluster("https://admin.example.com/"),
            Ok("https://admin.example.com".into())
        );
    }

    #[test]
    fn http_is_accepted_on_loopback_or_in_cluster() {
        for url in [
            "http://127.0.0.1:8081",
            "http://localhost:8081/",
            "http://admin.localhost",
            "http://[::1]:9000",
            "HTTP://LOCALHOST",
        ] {
            assert!(base(url).is_ok(), "{url}");
        }
        assert_eq!(
            base("http://orders-admin.internal:8081"),
            Err(ErrorCode::InvalidArgument)
        );
        assert_eq!(
            base_in_cluster("http://orders-admin.internal:8081/"),
            Ok("http://orders-admin.internal:8081".into())
        );
    }

    #[test]
    fn other_urls_are_refused_without_echoing_them() {
        for url in [
            "ftp://admin.example.com",
            "https://user:secret-pw@admin.example.com",
            "https://user@admin.example.com",
            "https://:secret-pw@admin.example.com",
            "https://admin.example.com/?secret-q=1",
            "https://admin.example.com/?",
            "https://admin.example.com/#secret-f",
            "data:text/plain,secret",
            "admin.example.com",
            "",
        ] {
            for any_http in [false, true] {
                let error = AdminClient::build(url, "orders-eu", minter(), any_http).unwrap_err();
                assert_eq!(error.code(), ErrorCode::InvalidArgument, "{url}");
                assert!(!error.message().contains("secret"), "{url}");
                assert!(!error.message().contains("example"), "{url}");
            }
        }
    }

    #[test]
    fn debug_prints_the_base_and_the_audience() {
        let client = AdminClient::new("https://admin.example.com", "orders-eu", minter()).unwrap();
        let printed = format!("{client:?}");
        assert!(printed.contains("https://admin.example.com"), "{printed}");
        assert!(printed.contains("orders-eu"), "{printed}");
    }

    #[test]
    fn operations_are_service_paths() {
        for op in [
            "billing.admin.v1.TenantAdmin/Suspend",
            "Admin/Ping",
            "a_b.c1/D_2",
        ] {
            assert!(is_op(op), "{op}");
        }
        for op in [
            "",
            "/billing.admin.v1.TenantAdmin/Suspend",
            "billing.admin.v1.TenantAdmin/",
            "billing.admin.v1.TenantAdmin",
            "billing..v1.TenantAdmin/Suspend",
            "../Suspend",
            "billing.admin.v1.TenantAdmin/Sus/pend",
            "billing.admin.v1.TenantAdmin/Sus.pend",
            "billing admin/Suspend",
            "billing.admin.v1.TenantAdmin/Suspend?x=1",
        ] {
            assert!(!is_op(op), "{op}");
        }
    }

    #[test]
    fn the_timeout_header_is_whole_milliseconds() {
        assert_eq!(grpc_timeout(Duration::from_millis(1500)), "1500m");
        assert_eq!(grpc_timeout(Duration::from_micros(10)), "1m");
        assert_eq!(grpc_timeout(Duration::ZERO), "1m");
        assert_eq!(grpc_timeout(Duration::from_secs(1_000_000)), "99999999m");
    }

    #[test]
    fn statuses_are_classified() {
        let refused = [
            AppError::failed_precondition("no"),
            AppError::not_found("no"),
            AppError::unavailable("busy").with_reason("TENANT_BUSY"),
            AppError::new(ErrorCode::Unknown, "odd"),
        ];
        for error in refused {
            let code = error.code();
            assert!(
                matches!(answered::<()>(error), CallOutcome::Refused(_)),
                "{code:?}"
            );
        }
        let unknown = answered::<()>(AppError::unavailable("gone"));
        assert!(matches!(unknown, CallOutcome::Unknown(_)));
    }

    #[test]
    fn into_result_gives_the_reply_or_the_error() {
        assert_eq!(CallOutcome::Replied(7).into_result().unwrap(), 7);
        for outcome in [
            CallOutcome::<u8>::NotSent(AppError::unavailable("a")),
            CallOutcome::Refused(AppError::not_found("b")),
            CallOutcome::Unknown(AppError::new(ErrorCode::Unknown, "c")),
        ] {
            assert!(outcome.into_result().is_err());
        }
    }

    #[tokio::test]
    async fn bad_operations_and_extra_claims_are_not_sent() {
        let client = AdminClient::new("http://127.0.0.1:1", "orders-eu", minter()).unwrap();
        let outcome = client
            .call::<(), (), _>(
                "/billing.admin.v1.TenantAdmin/Suspend",
                None,
                "operator-1",
                &NoClaims {},
                Duration::from_secs(5),
                &(),
            )
            .await;
        let error = not_sent(outcome);
        assert_eq!(error.code(), ErrorCode::InvalidArgument);

        let outcome = client
            .call::<(), (), _>(
                "billing.admin.v1.TenantAdmin/Suspend",
                None,
                "operator-1",
                &"not an object",
                Duration::from_secs(5),
                &(),
            )
            .await;
        let error = not_sent(outcome);
        assert_eq!(error.code(), ErrorCode::InvalidArgument);
    }

    #[tokio::test]
    async fn no_listener_is_not_sent() {
        let client = AdminClient::new("http://127.0.0.1:1", "orders-eu", minter()).unwrap();
        let outcome = client
            .call::<(), (), _>(
                "billing.admin.v1.TenantAdmin/Ping",
                None,
                "operator-1",
                &NoClaims {},
                Duration::from_secs(30),
                &(),
            )
            .await;
        let error = not_sent(outcome);
        assert_eq!(error.code(), ErrorCode::Unavailable);
        assert_eq!(error.message(), "the admin API could not be reached");
    }
}
