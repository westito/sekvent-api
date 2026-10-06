//! Outbound HTTP client with resilience, context propagation and OAuth 2.0 token caching.
//!
//! [`HttpClient`] wraps `reqwest` (rustls with the ring crypto provider and
//! the platform certificate verifier; no process-wide rustls provider needs
//! to be installed) and adds:
//!
//! - a [`Policy`](sekvent_resilience::Policy) around every request — by
//!   default a timeout plus retries, which only ever apply to idempotent
//!   requests;
//! - [`CallContext`](sekvent_context::CallContext) propagation: request id,
//!   remaining deadline as `grpc-timeout`, trace context — never
//!   overriding headers set on the request, and never forwarding the
//!   inbound idempotency key. Subject, tenant and call depth go only to
//!   upstreams declared with [`HttpClientBuilder::sekvent_upstream`];
//! - error mapping to [`AppError`](sekvent_error::AppError): transient HTTP
//!   statuses and transport failures become retryable codes, `Retry-After`
//!   is honoured up to the retry policy's cap, and upstream bodies never
//!   reach the error (only status, content type and length go to the
//!   internal source chain). Sekvent JSON error bodies are adopted only from
//!   upstreams declared with
//!   [`HttpClientBuilder::sekvent_upstream`]; otherwise a `401`/`403`
//!   means this service's credentials were refused and maps to `INTERNAL`;
//! - redirects followed only within the original origin
//!   ([`RedirectPolicy`]), so credentials and identity headers never reach
//!   another host;
//! - static Basic/bearer credentials or a [`BearerSource`] such as
//!   [`oauth2::ClientCredentials`], with one retry after a `401`.
//!
//! Each request gets a debug-level span with method, host, path (without
//! query), status and elapsed time. Headers, query strings and bodies are
//! never logged.
//!
//! ```no_run
//! # async fn demo() -> Result<(), Box<dyn std::error::Error>> {
//! use sekvent_client::HttpClient;
//! use sekvent_context::CallContext;
//!
//! let client = HttpClient::builder().base_url("https://billing.example/api").build()?;
//! let ctx = CallContext::new().with_timeout(std::time::Duration::from_secs(2));
//! let invoice: serde_json::Value = client.get("/invoices/42").send_json(&ctx).await?;
//! # let _ = invoice;
//! # Ok(())
//! # }
//! ```

#![forbid(unsafe_code)]

mod auth;
mod client;
mod mapping;
pub mod oauth2;
mod request;
mod response;
mod tls;
pub use auth::BearerSource;
pub use client::{BuildError, HttpClient, HttpClientBuilder, RedirectPolicy};
pub use mapping::{code_for_status, parse_retry_after};
pub use request::RequestBuilder;
pub use response::HttpResponse;
pub use tls::reqwest_builder;
