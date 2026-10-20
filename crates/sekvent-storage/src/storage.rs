use std::fmt;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use reqwest::header::{CONTENT_LENGTH, CONTENT_TYPE, ETAG, HeaderMap, HeaderName, LAST_MODIFIED};
use reqwest::{Method, StatusCode};
use sekvent_config::ConfigSource;
use sekvent_context::{CallContext, Clock, SystemClock};
use sekvent_error::{AppError, ErrorCode};
use sekvent_resilience::{Policy, RetryPolicy, Timeout};
use tracing::Instrument as _;

use crate::ObjectKey;
use crate::config::{Endpoint, StorageConfig, not_blank, parse_endpoint, positive, valid_bucket};
use crate::error::{reasons, s3_error_code, status_error, transport_error};
use crate::presign::{PresignGet, PresignPut, PresignedRequest, check_content_type, expiry_secs};
use crate::sigv4::{EMPTY_SHA256, Request, Signer, encode_path};

/// Most of a response body read for error mapping; the rest is dropped.
const BODY_LIMIT: usize = 64 * 1024;

/// What the store knows about one object.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ObjectInfo {
    /// Size in bytes.
    pub size: u64,
    /// The stored `Content-Type`.
    pub content_type: Option<String>,
    /// The entity tag, without quotes.
    pub etag: Option<String>,
    /// When the object was last written.
    pub last_modified: Option<SystemTime>,
}

/// One bucket on an S3-compatible store: presigned URLs for browsers and
/// the server-side calls an upload flow needs.
///
/// Build it once at startup and share it; clones share the HTTP connection
/// pool and the resilience policy.
#[derive(Clone)]
pub struct Storage {
    inner: Arc<Inner>,
}

struct Inner {
    signer: Signer,
    bucket: String,
    endpoint: Endpoint,
    public: Endpoint,
    path_style: bool,
    timeout: Duration,
    http: reqwest::Client,
    policy: Policy,
    clock: Arc<dyn Clock>,
}

impl fmt::Debug for Storage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Storage")
            .field("bucket", &self.inner.bucket)
            .field("endpoint", &self.inner.endpoint.authority)
            .field("public_endpoint", &self.inner.public.authority)
            .field("path_style", &self.inner.path_style)
            .finish_non_exhaustive()
    }
}

/// Builds a [`Storage`] from a [`StorageConfig`].
pub struct StorageBuilder {
    config: StorageConfig,
    clock: Arc<dyn Clock>,
    policy: Option<Policy>,
}

impl fmt::Debug for StorageBuilder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StorageBuilder")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

impl StorageBuilder {
    /// The clock that dates signatures (default: [`SystemClock`]).
    #[must_use]
    pub fn clock(mut self, clock: Arc<dyn Clock>) -> Self {
        self.clock = clock;
        self
    }

    /// The resilience policy around each server-side call. The default is
    /// named `storage`: a per-attempt timeout of `TIMEOUT` plus
    /// [`RetryPolicy::default`]. Every call of this crate is idempotent.
    #[must_use]
    pub fn policy(mut self, policy: Policy) -> Self {
        self.policy = Some(policy);
        self
    }

    /// Validate the settings and build the client. Errors are
    /// `INVALID_ARGUMENT` and name the setting (`ENDPOINT`, …), never its
    /// value.
    pub fn build(self) -> Result<Storage, AppError> {
        let config = self.config;
        let invalid = |key: &str, reason: String| {
            AppError::invalid_argument(format!("the storage setting {key} {reason}"))
        };
        let endpoint = parse_endpoint(&config.endpoint).map_err(|r| invalid("ENDPOINT", r))?;
        let public = match &config.public_endpoint {
            Some(value) => parse_endpoint(value).map_err(|r| invalid("PUBLIC_ENDPOINT", r))?,
            None => endpoint.clone(),
        };
        not_blank(&config.region).map_err(|r| invalid("REGION", r))?;
        valid_bucket(&config.bucket).map_err(|r| invalid("BUCKET", r))?;
        if !config.path_style && config.bucket.contains('.') {
            return Err(invalid(
                "BUCKET",
                "must not contain '.' unless PATH_STYLE is on".to_owned(),
            ));
        }
        not_blank(&config.access_key_id).map_err(|r| invalid("ACCESS_KEY_ID", r))?;
        if config.secret_access_key.is_blank() {
            return Err(invalid("SECRET_ACCESS_KEY", "must not be blank".to_owned()));
        }
        if config
            .session_token
            .as_ref()
            .is_some_and(sekvent_config::Secret::is_blank)
        {
            return Err(invalid("SESSION_TOKEN", "must not be blank".to_owned()));
        }
        positive(&config.timeout).map_err(|r| invalid("TIMEOUT", r))?;
        let http = sekvent_client::reqwest_builder()
            .no_gzip()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(config.timeout)
            .build()
            .map_err(|error| {
                AppError::new(
                    ErrorCode::Internal,
                    "the storage HTTP client could not be initialised",
                )
                .with_source(error)
            })?;
        let policy = self.policy.unwrap_or_else(|| {
            Policy::new("storage")
                .with_timeout(Timeout::new(config.timeout))
                .with_retry(RetryPolicy::default())
        });
        Ok(Storage {
            inner: Arc::new(Inner {
                signer: Signer::new(
                    config.access_key_id,
                    config.secret_access_key,
                    config.session_token,
                    config.region.trim().to_owned(),
                ),
                bucket: config.bucket,
                endpoint,
                public,
                path_style: config.path_style,
                timeout: config.timeout,
                http,
                policy,
                clock: self.clock,
            }),
        })
    }
}

/// A response the operation decides about.
struct Reply {
    status: StatusCode,
    headers: HeaderMap,
    body: Vec<u8>,
}

impl Storage {
    /// A builder over `config`.
    pub fn builder(config: StorageConfig) -> StorageBuilder {
        StorageBuilder {
            config,
            clock: Arc::new(SystemClock),
            policy: None,
        }
    }

    /// Build with the defaults; see [`StorageBuilder::build`].
    pub fn new(config: StorageConfig) -> Result<Self, AppError> {
        Self::builder(config).build()
    }

    /// Read [`StorageConfig`] under `prefix` and build with the defaults.
    /// A config error becomes `INVALID_ARGUMENT` naming the keys.
    pub fn from_config(source: &dyn ConfigSource, prefix: &str) -> Result<Self, AppError> {
        let config = StorageConfig::load(source, prefix)
            .map_err(|error| AppError::invalid_argument(error.to_string()))?;
        Self::new(config)
    }

    /// The bucket name.
    pub fn bucket(&self) -> &str {
        &self.inner.bucket
    }

    /// Host and encoded path of `key` (or of the bucket) on `endpoint`.
    fn locate(&self, endpoint: &Endpoint, key: Option<&ObjectKey>) -> (String, String) {
        let object = key.map(|key| encode_path(key.as_str()));
        if self.inner.path_style {
            let path = match object {
                Some(object) => format!("/{}/{object}", self.inner.bucket),
                None => format!("/{}", self.inner.bucket),
            };
            (endpoint.authority.clone(), path)
        } else {
            let host = format!("{}.{}", self.inner.bucket, endpoint.authority);
            (host, format!("/{}", object.unwrap_or_default()))
        }
    }

    fn presign(
        &self,
        method: &'static str,
        key: &ObjectKey,
        query: Vec<(String, String)>,
        headers: Vec<(String, String)>,
        lifetime: Duration,
    ) -> Result<(String, SystemTime), AppError> {
        let expires = expiry_secs(lifetime)?;
        let endpoint = &self.inner.public;
        let (host, path) = self.locate(endpoint, Some(key));
        let now = self.inner.clock.now();
        let query = self.inner.signer.presign(
            Request {
                method,
                host: &host,
                path: &path,
                query,
                headers,
            },
            now,
            expires,
        );
        let url = format!("{}://{host}{path}?{query}", endpoint.scheme);
        Ok((url, now + Duration::from_secs(expires)))
    }

    /// A presigned `PUT` that uploads `key`.
    ///
    /// The uploader sends the body with the returned `headers`. S3 cannot
    /// bound a presigned `PUT` by a maximum size; sign the exact size with
    /// [`PresignPut::content_length`], or [`head`](Self::head) the object
    /// after the upload and delete it when it is too large.
    pub fn presign_put(
        &self,
        key: &ObjectKey,
        request: PresignPut,
    ) -> Result<PresignedRequest, AppError> {
        check_content_type(&request.content_type)?;
        let mut signed = vec![("content-type".to_owned(), request.content_type.clone())];
        if let Some(length) = request.content_length {
            signed.push(("content-length".to_owned(), length.to_string()));
        }
        let (url, expires_at) = self.presign("PUT", key, Vec::new(), signed, request.expires_in)?;
        Ok(PresignedRequest {
            method: "PUT",
            url,
            headers: vec![("content-type".to_owned(), request.content_type)],
            expires_at,
        })
    }

    /// A presigned `GET` that downloads `key`.
    pub fn presign_get(
        &self,
        key: &ObjectKey,
        request: PresignGet,
    ) -> Result<PresignedRequest, AppError> {
        let mut query = Vec::new();
        if let Some(disposition) = request.disposition {
            query.push(("response-content-disposition".to_owned(), disposition));
        }
        if let Some(content_type) = request.content_type {
            check_content_type(&content_type)?;
            query.push(("response-content-type".to_owned(), content_type));
        }
        let (url, expires_at) = self.presign("GET", key, query, Vec::new(), request.expires_in)?;
        Ok(PresignedRequest {
            method: "GET",
            url,
            headers: Vec::new(),
            expires_at,
        })
    }

    /// Size, content type, entity tag and modification time of `key`, or
    /// `None` when it does not exist.
    ///
    /// A store that denies listing the bucket answers `403` for a missing
    /// object, which is an error (`INTERNAL`, `STORAGE_REJECTED`). A `HEAD`
    /// response has no body, so a missing bucket looks like a missing
    /// object here; [`check`](Self::check) and the readiness probe tell
    /// them apart.
    pub async fn head(
        &self,
        ctx: &CallContext,
        key: &ObjectKey,
    ) -> Result<Option<ObjectInfo>, AppError> {
        let reply = self.send(ctx, Method::HEAD, Some(key), Vec::new()).await?;
        if reply.status == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        let size = header(&reply.headers, &CONTENT_LENGTH)
            .and_then(|value| value.parse::<u64>().ok())
            .ok_or_else(|| {
                AppError::new(ErrorCode::Internal, "the object store sent no object size")
                    .with_reason(reasons::STORAGE_HTTP_ERROR)
            })?;
        Ok(Some(ObjectInfo {
            size,
            content_type: header(&reply.headers, &CONTENT_TYPE),
            etag: header(&reply.headers, &ETAG).map(|tag| tag.trim_matches('"').to_owned()),
            last_modified: header(&reply.headers, &LAST_MODIFIED)
                .and_then(|value| httpdate::parse_http_date(&value).ok()),
        }))
    }

    /// Delete `key`. Deleting a missing object succeeds.
    pub async fn delete(&self, ctx: &CallContext, key: &ObjectKey) -> Result<(), AppError> {
        self.send(ctx, Method::DELETE, Some(key), Vec::new())
            .await
            .map(drop)
    }

    /// Copy `from` to `to` in this bucket, with its metadata and content
    /// type. A missing source is `NOT_FOUND`, reason `OBJECT_NOT_FOUND`.
    pub async fn copy(
        &self,
        ctx: &CallContext,
        from: &ObjectKey,
        to: &ObjectKey,
    ) -> Result<(), AppError> {
        let source = format!("/{}/{}", self.inner.bucket, encode_path(from.as_str()));
        let reply = self
            .send(
                ctx,
                Method::PUT,
                Some(to),
                vec![("x-amz-copy-source".to_owned(), source)],
            )
            .await?;
        if reply.status == StatusCode::NOT_FOUND {
            return Err(AppError::not_found("the source object does not exist")
                .with_reason(reasons::OBJECT_NOT_FOUND));
        }
        // S3 may report a failed copy with `200` and an error document.
        if let Some(code) = s3_error_code(&reply.body) {
            return Err(
                AppError::unavailable("the object store could not copy the object")
                    .with_reason(reasons::STORAGE_HTTP_ERROR)
                    .with_source(format!("object store copy failed ({code})")),
            );
        }
        Ok(())
    }

    /// Check that the bucket exists and the credentials may use it
    /// (`HEAD` on the bucket). A missing bucket is `INTERNAL`, reason
    /// `STORAGE_REJECTED`.
    pub async fn check(&self, ctx: &CallContext) -> Result<(), AppError> {
        let reply = self.send(ctx, Method::HEAD, None, Vec::new()).await?;
        if reply.status == StatusCode::NOT_FOUND {
            return Err(AppError::new(
                ErrorCode::Internal,
                "the object store bucket does not exist",
            )
            .with_reason(reasons::STORAGE_REJECTED));
        }
        Ok(())
    }

    /// Sign and send one request under the policy. Success statuses, and
    /// `404` when the body names no missing bucket, come back as a
    /// [`Reply`]; every other status is an error.
    async fn send(
        &self,
        ctx: &CallContext,
        method: Method,
        key: Option<&ObjectKey>,
        headers: Vec<(String, String)>,
    ) -> Result<Reply, AppError> {
        let (host, path) = self.locate(&self.inner.endpoint, key);
        let url = format!("{}://{host}{path}", self.inner.endpoint.scheme);
        let span = tracing::debug_span!(
            "storage_request",
            method = %method,
            bucket = %self.inner.bucket,
            status = tracing::field::Empty,
        );
        let result = self
            .inner
            .policy
            .call(ctx, true, || {
                self.attempt(ctx, &method, &host, &path, &url, &headers)
            })
            .instrument(span.clone())
            .await;
        match &result {
            Ok(reply) => {
                span.record("status", reply.status.as_u16());
            }
            Err(error) => {
                tracing::debug!(parent: &span, code = %error.code(), "storage request failed");
            }
        }
        result
    }

    async fn attempt(
        &self,
        ctx: &CallContext,
        method: &Method,
        host: &str,
        path: &str,
        url: &str,
        headers: &[(String, String)],
    ) -> Result<Reply, AppError> {
        let now = self.inner.clock.now();
        let signed = self.inner.signer.sign(
            Request {
                method: method.as_str(),
                host,
                path,
                query: Vec::new(),
                headers: headers.to_vec(),
            },
            EMPTY_SHA256,
            now,
        );
        let timeout = ctx
            .remaining()
            .map_or(self.inner.timeout, |left| left.min(self.inner.timeout));
        let mut request = self
            .inner
            .http
            .request(method.clone(), url)
            .timeout(timeout);
        for (name, value) in signed {
            request = request.header(name, value);
        }
        let mut response = request.send().await.map_err(transport_error)?;
        let status = response.status();
        let response_headers = response.headers().clone();
        let mut body = Vec::new();
        while body.len() < BODY_LIMIT {
            match response.chunk().await.map_err(transport_error)? {
                Some(chunk) => body.extend_from_slice(&chunk),
                None => break,
            }
        }
        let missing_object = status == StatusCode::NOT_FOUND
            && s3_error_code(&body).as_deref() != Some("NoSuchBucket");
        if status.is_success() || missing_object {
            Ok(Reply {
                status,
                headers: response_headers,
                body,
            })
        } else {
            Err(status_error(status, &response_headers, &body, now))
        }
    }
}

fn header(headers: &HeaderMap, name: &HeaderName) -> Option<String> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
}

#[cfg(test)]
mod tests;
