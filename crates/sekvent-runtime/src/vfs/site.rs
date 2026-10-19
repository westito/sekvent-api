//! [`StaticSite`]: a built frontend served from a [`Vfs`].

use std::cmp::Reverse;
use std::convert::Infallible;
use std::fmt;
use std::sync::Arc;
use std::task::{Context, Poll};

use axum::body::Body;
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use futures::future::BoxFuture;
use http::header::HeaderName;
use http::header::{
    ACCEPT, ACCEPT_ENCODING, ALLOW, CACHE_CONTROL, CONTENT_ENCODING, CONTENT_LENGTH, CONTENT_TYPE,
    ETAG, IF_NONE_MATCH, LAST_MODIFIED, LOCATION, VARY, X_CONTENT_TYPE_OPTIONS, X_FRAME_OPTIONS,
};
use http::{HeaderMap, HeaderValue, Method, Request, StatusCode, Uri};
use percent_encoding::{AsciiSet, CONTROLS, percent_decode_str, utf8_percent_encode};
use sekvent_error::AppError;

use super::{Vfs, VfsFile, VfsPath};

const DEFAULT_INDEX: &str = "index.html";
/// Where Vite emits content-hashed files.
const DEFAULT_IMMUTABLE_PREFIX: &str = "assets";

const NO_CACHE: &str = "no-cache";
const IMMUTABLE: &str = "public, max-age=31536000, immutable";
const VARY_ENCODING: &str = "Accept-Encoding";
const VARY_ENCODING_AND_ACCEPT: &str = "Accept-Encoding, Accept";

/// Escaped in a path segment of a `Location`.
const SEGMENT: &AsciiSet = &CONTROLS
    .add(b' ')
    .add(b'"')
    .add(b'#')
    .add(b'%')
    .add(b'/')
    .add(b'<')
    .add(b'>')
    .add(b'?')
    .add(b'\\')
    .add(b'^')
    .add(b'`')
    .add(b'{')
    .add(b'|')
    .add(b'}');
/// Escaped in the query of a `Location`; existing escapes are kept.
const QUERY: &AsciiSet = &CONTROLS.add(b' ').add(b'"').add(b'#').add(b'<').add(b'>');

/// A built frontend (typically a single-page app) served from a [`Vfs`],
/// as a tower service; mount it with
/// [`ServerBuilder::site`](crate::ServerBuilder::site).
///
/// - Only `GET` and `HEAD`; anything else is `405` with
///   `Allow: GET, HEAD`. `HEAD` gets the headers of `GET`, no body.
/// - The request path goes through [`VfsPath::from_url_path`].
/// - `/` and any path ending in `/` serve the index (`index.html` by
///   default) of that directory. A path that is not a file but a
///   directory with an index (`/docs` with `docs/index.html`) answers
///   `308` to the same path with a trailing `/`, query kept.
/// - SPA fallback (on by default): a navigation (an `Accept` that lists
///   `text/html` itself; `*/*` does not count) for a missing path serves
///   the root index with `200`, so client-side routes survive a reload,
///   dots in the path or not (`/nodes/10.0.0.1`). Paths the normalizer
///   rejects fall back too. A path below the immutable prefix
///   (`/assets/gone.js`) is always `404`, never the index, and so is any
///   missing path requested without `text/html`.
/// - `Cache-Control`: HTML and the index are `no-cache`; files below the
///   immutable prefix (`assets/` by default, where Vite puts
///   content-hashed files) are `public, max-age=31536000, immutable`;
///   everything else is `no-cache`.
/// - A strong `ETag` from the content hash; a matching `If-None-Match`
///   (a list, weak tags compared weakly, or `*`) answers `304` without a
///   body or `Content-Length`.
/// - Precompressed variants: when a file exists and `<file>.br` or
///   `<file>.gz` next to it too, and `Accept-Encoding` allows it (`q=0`
///   refuses), the variant is sent with `Content-Encoding`, the file's
///   content type and its own `ETag`; `br` wins a tie. A variant that
///   cannot be read is skipped. Nothing is compressed at runtime.
/// - Every response carries `X-Content-Type-Options: nosniff`,
///   `Vary: Accept-Encoding` (plus `Accept` when the SPA fallback decided
///   the answer) and the configured extra headers (by default
///   `X-Frame-Options: DENY`; see [`StaticSiteBuilder::header`]); files
///   carry `Content-Length` and, when known, `Last-Modified`.
///
/// `Debug` shows the configuration, never file contents.
#[derive(Clone)]
pub struct StaticSite {
    inner: Arc<SiteInner>,
}

struct SiteInner {
    vfs: Arc<dyn Vfs>,
    index: VfsPath,
    spa_fallback: bool,
    immutable_prefix: Option<VfsPath>,
    headers: HeaderMap,
}

impl fmt::Debug for StaticSite {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StaticSite")
            .field("index", &self.inner.index.as_str())
            .field("spa_fallback", &self.inner.spa_fallback)
            .field(
                "immutable_prefix",
                &self.inner.immutable_prefix.as_ref().map(VfsPath::as_str),
            )
            .field("headers", &header_names(&self.inner.headers))
            .finish_non_exhaustive()
    }
}

/// Configures a [`StaticSite`].
pub struct StaticSiteBuilder {
    vfs: Arc<dyn Vfs>,
    index: String,
    spa_fallback: bool,
    immutable_prefix: Option<String>,
    headers: HeaderMap,
}

impl fmt::Debug for StaticSiteBuilder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StaticSiteBuilder")
            .field("index", &self.index)
            .field("spa_fallback", &self.spa_fallback)
            .field("immutable_prefix", &self.immutable_prefix)
            .field("headers", &header_names(&self.headers))
            .finish_non_exhaustive()
    }
}

impl StaticSiteBuilder {
    /// The file served for `/` and directory paths, and by the SPA
    /// fallback (default `index.html`). One path segment.
    #[must_use]
    pub fn index(mut self, name: impl Into<String>) -> Self {
        self.index = name.into();
        self
    }

    /// Whether a navigation to a missing path serves the root index
    /// (default on).
    #[must_use]
    pub fn spa_fallback(mut self, enabled: bool) -> Self {
        self.spa_fallback = enabled;
        self
    }

    /// The directory whose files never change under the same name and are
    /// cached for a year (default `assets`). A missing path below it is
    /// always `404`.
    #[must_use]
    pub fn immutable_prefix(mut self, prefix: impl Into<String>) -> Self {
        self.immutable_prefix = Some(prefix.into());
        self
    }

    /// Cache nothing as immutable.
    #[must_use]
    pub fn no_immutable_prefix(mut self) -> Self {
        self.immutable_prefix = None;
        self
    }

    /// Send `name: value` on every response of the site (files, `304`,
    /// `308`, `404`, `405` and `500`), replacing a header of that name the
    /// site would set itself and an earlier value given here. The default
    /// set is `X-Frame-Options: DENY`; give `X_FRAME_OPTIONS` to change it.
    #[must_use]
    pub fn header(mut self, name: HeaderName, value: HeaderValue) -> Self {
        self.headers.insert(name, value);
        self
    }

    /// Stop sending a header given with [`header`](Self::header) or the
    /// default `X-Frame-Options`. Headers the site sets itself stay.
    #[must_use]
    pub fn without_header(mut self, name: &HeaderName) -> Self {
        self.headers.remove(name);
        self
    }

    /// Validate and build. `INVALID_ARGUMENT` when the index is not a
    /// single valid path segment or the immutable prefix is not a valid,
    /// non-empty path (see [`VfsPath`]).
    pub fn build(self) -> Result<StaticSite, AppError> {
        let index = VfsPath::new(&self.index)
            .filter(|index| index.segments().count() == 1)
            .ok_or_else(|| {
                AppError::invalid_argument(format!(
                    "the static site index {:?} must be one plain file name",
                    self.index
                ))
            })?;
        let immutable_prefix = self
            .immutable_prefix
            .map(|prefix| {
                VfsPath::new(&prefix)
                    .filter(|path| !path.is_root())
                    .ok_or_else(|| {
                        AppError::invalid_argument(format!(
                            "the static site immutable prefix {prefix:?} must be a relative \
                             directory such as assets"
                        ))
                    })
            })
            .transpose()?;
        Ok(StaticSite {
            inner: Arc::new(SiteInner {
                vfs: self.vfs,
                index,
                spa_fallback: self.spa_fallback,
                immutable_prefix,
                headers: self.headers,
            }),
        })
    }
}

/// A file found for a request.
struct Hit {
    /// The path the file stands for (without a `.br` or `.gz` suffix).
    path: VfsPath,
    /// The bytes sent: the file itself or a precompressed variant.
    file: VfsFile,
    /// The content type of the file itself, also for a variant.
    content_type: HeaderValue,
    encoding: Option<Encoding>,
    /// Served by the SPA fallback.
    fallback: bool,
}

enum Outcome {
    File(Box<Hit>),
    /// A directory without its trailing `/`.
    Redirect(VfsPath),
    Missing,
}

/// What a request resolved to.
struct Resolution {
    outcome: Outcome,
    /// Whether `Accept` decided the answer.
    varies_on_accept: bool,
}

impl Resolution {
    fn found(hit: Option<Hit>, varies_on_accept: bool) -> Self {
        Self {
            outcome: hit.map_or(Outcome::Missing, |hit| Outcome::File(Box::new(hit))),
            varies_on_accept,
        }
    }
}

impl StaticSite {
    /// A site over `vfs` with the defaults: `index.html`, SPA fallback on,
    /// `assets/` immutable, `X-Frame-Options: DENY`.
    pub fn new(vfs: Arc<dyn Vfs>) -> Self {
        Self::builder(vfs)
            .build()
            .expect("the default index and immutable prefix are valid paths")
    }

    /// Start configuring a site over `vfs`.
    pub fn builder(vfs: Arc<dyn Vfs>) -> StaticSiteBuilder {
        let mut headers = HeaderMap::new();
        headers.insert(X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
        StaticSiteBuilder {
            vfs,
            index: DEFAULT_INDEX.to_owned(),
            spa_fallback: true,
            immutable_prefix: Some(DEFAULT_IMMUTABLE_PREFIX.to_owned()),
            headers,
        }
    }

    /// The response to a request with this method, URI and headers.
    pub async fn respond(&self, method: &Method, uri: &Uri, headers: &HeaderMap) -> Response {
        let mut response = self.answer(method, uri, headers).await;
        for (name, value) in &self.inner.headers {
            response.headers_mut().insert(name.clone(), value.clone());
        }
        response
    }

    async fn answer(&self, method: &Method, uri: &Uri, headers: &HeaderMap) -> Response {
        let head = *method == Method::HEAD;
        if !head && *method != Method::GET {
            return method_not_allowed();
        }
        match self.resolve(uri.path(), headers).await {
            Ok(Resolution {
                outcome: Outcome::File(hit),
                varies_on_accept,
            }) => self.file_response(*hit, head, headers, varies_on_accept),
            Ok(Resolution {
                outcome: Outcome::Redirect(path),
                ..
            }) => redirect(&path, uri.query()),
            Ok(Resolution {
                outcome: Outcome::Missing,
                varies_on_accept,
            }) => not_found(varies_on_accept),
            Err(error) => {
                let mut response = error.into_response();
                response
                    .headers_mut()
                    .extend(common_headers(self.inner.spa_fallback));
                response
            }
        }
    }

    async fn resolve(&self, raw: &str, headers: &HeaderMap) -> Result<Resolution, AppError> {
        let inner = &self.inner;
        let encodings = accepted_encodings(headers);
        let Some(path) = VfsPath::from_url_path(raw) else {
            let immutable = inner
                .immutable_prefix
                .as_ref()
                .is_some_and(|prefix| raw_is_below(raw, prefix));
            return self.fallback(None, immutable, headers, &encodings).await;
        };
        let directory = raw.ends_with('/') || path.is_root();
        let target = if directory {
            path.join(&inner.index)
        } else {
            path.clone()
        };
        if let Some(hit) = self.lookup(&target, &encodings, false).await? {
            return Ok(Resolution::found(Some(hit), false));
        }
        if !directory && inner.vfs.get(&path.join(&inner.index)).await?.is_some() {
            return Ok(Resolution {
                outcome: Outcome::Redirect(path),
                varies_on_accept: false,
            });
        }
        let immutable = self.is_immutable(&path);
        self.fallback(Some(&target), immutable, headers, &encodings)
            .await
    }

    /// The SPA fallback for a missing path (`target`, `None` when the
    /// normalizer rejected it).
    async fn fallback(
        &self,
        target: Option<&VfsPath>,
        immutable: bool,
        headers: &HeaderMap,
        encodings: &[Encoding],
    ) -> Result<Resolution, AppError> {
        let inner = &self.inner;
        if !inner.spa_fallback || immutable {
            return Ok(Resolution::found(None, false));
        }
        let hit = if accepts_html(headers) && target != Some(&inner.index) {
            self.lookup(&inner.index, encodings, true).await?
        } else {
            None
        };
        Ok(Resolution::found(hit, true))
    }

    /// Whether `path` lies below the immutable prefix.
    fn is_immutable(&self, path: &VfsPath) -> bool {
        self.inner
            .immutable_prefix
            .as_ref()
            .is_some_and(|prefix| path.starts_with(prefix) && path != prefix)
    }

    /// The file at `path`, sent as its most preferred precompressed variant
    /// when one exists. A variant that cannot be read is skipped.
    async fn lookup(
        &self,
        path: &VfsPath,
        encodings: &[Encoding],
        fallback: bool,
    ) -> Result<Option<Hit>, AppError> {
        let vfs = &self.inner.vfs;
        let Some(original) = vfs.get(path).await? else {
            return Ok(None);
        };
        let content_type = original.content_type().clone();
        for &encoding in encodings {
            match vfs.get(&path.with_suffix(encoding.suffix())).await {
                Ok(Some(variant)) => {
                    return Ok(Some(Hit {
                        path: path.clone(),
                        file: variant,
                        content_type,
                        encoding: Some(encoding),
                        fallback,
                    }));
                }
                Ok(None) => {}
                Err(error) => tracing::debug!(
                    path = %path,
                    coding = encoding.token(),
                    %error,
                    "a precompressed variant cannot be read; trying the next"
                ),
            }
        }
        Ok(Some(Hit {
            path: path.clone(),
            file: original,
            content_type,
            encoding: None,
            fallback,
        }))
    }

    fn file_response(
        &self,
        hit: Hit,
        head: bool,
        request: &HeaderMap,
        varies_on_accept: bool,
    ) -> Response {
        let Hit {
            path,
            file,
            content_type,
            encoding,
            fallback,
        } = hit;
        let html = content_type
            .to_str()
            .is_ok_and(|value| value.starts_with("text/html"));
        let immutable = !fallback && !html && self.is_immutable(&path);
        let mut headers = common_headers(varies_on_accept);
        headers.insert(ETAG, file.etag().clone());
        headers.insert(
            CACHE_CONTROL,
            HeaderValue::from_static(if immutable { IMMUTABLE } else { NO_CACHE }),
        );
        if let Some(modified) = file.last_modified()
            && let Ok(value) = HeaderValue::try_from(httpdate::fmt_http_date(modified))
        {
            headers.insert(LAST_MODIFIED, value);
        }
        if none_match(request, file.etag()) {
            return with_headers(StatusCode::NOT_MODIFIED, headers, unsized_empty_body());
        }
        headers.insert(CONTENT_TYPE, content_type);
        headers.insert(CONTENT_LENGTH, HeaderValue::from(file.contents().len()));
        if let Some(encoding) = encoding {
            headers.insert(CONTENT_ENCODING, HeaderValue::from_static(encoding.token()));
        }
        let body = if head {
            Body::empty()
        } else {
            Body::from(file.contents().clone())
        };
        with_headers(StatusCode::OK, headers, body)
    }
}

impl<B> tower::Service<Request<B>> for StaticSite {
    type Response = Response;
    type Error = Infallible;
    type Future = BoxFuture<'static, Result<Response, Infallible>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, request: Request<B>) -> Self::Future {
        let site = self.clone();
        let (parts, _body) = request.into_parts();
        Box::pin(async move {
            Ok(site
                .respond(&parts.method, &parts.uri, &parts.headers)
                .await)
        })
    }
}

/// A precompressed variant's coding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Encoding {
    Brotli,
    Gzip,
}

impl Encoding {
    fn suffix(self) -> &'static str {
        match self {
            Self::Brotli => ".br",
            Self::Gzip => ".gz",
        }
    }

    fn token(self) -> &'static str {
        match self {
            Self::Brotli => "br",
            Self::Gzip => "gzip",
        }
    }
}

/// The codings `Accept-Encoding` allows, most preferred first (`br` on a
/// tie). A coding listed more than once takes its lowest weight; `*`
/// covers codings not listed; an entry with a malformed weight is
/// ignored. Without the header, none.
fn accepted_encodings(headers: &HeaderMap) -> Vec<Encoding> {
    let mut brotli: Option<u16> = None;
    let mut gzip: Option<u16> = None;
    let mut any: Option<u16> = None;
    for item in list_items(headers, &ACCEPT_ENCODING) {
        let mut parts = item.split(';');
        let coding = parts.next().unwrap_or_default().trim();
        let Some(weight) = quality(parts) else {
            continue;
        };
        let slot = if coding.eq_ignore_ascii_case("br") {
            &mut brotli
        } else if coding.eq_ignore_ascii_case("gzip") || coding.eq_ignore_ascii_case("x-gzip") {
            &mut gzip
        } else if coding == "*" {
            &mut any
        } else {
            continue;
        };
        *slot = Some(slot.map_or(weight, |seen| seen.min(weight)));
    }
    let mut ranked: Vec<(Encoding, u16)> = [
        (Encoding::Brotli, brotli.or(any).unwrap_or(0)),
        (Encoding::Gzip, gzip.or(any).unwrap_or(0)),
    ]
    .into_iter()
    .filter(|&(_, weight)| weight > 0)
    .collect();
    ranked.sort_by_key(|&(_, weight)| Reverse(weight));
    ranked.into_iter().map(|(encoding, _)| encoding).collect()
}

/// Whether `Accept` lists `text/html` itself with a non-zero weight: a
/// browser navigation. `*/*` and `text/*` do not count.
fn accepts_html(headers: &HeaderMap) -> bool {
    list_items(headers, &ACCEPT).any(|item| {
        let mut parts = item.split(';');
        let range = parts.next().unwrap_or_default().trim();
        range.eq_ignore_ascii_case("text/html") && quality(parts).is_some_and(|weight| weight > 0)
    })
}

/// Whether the URL path `raw`, decoded leniently segment by segment, lies
/// below `dir`. For paths the normalizer rejects.
fn raw_is_below(raw: &str, dir: &VfsPath) -> bool {
    let mut segments = raw
        .split('/')
        .filter(|segment| !segment.is_empty())
        .map(|segment| percent_decode_str(segment).decode_utf8_lossy());
    dir.segments()
        .all(|expected| segments.next().is_some_and(|segment| segment == expected))
        && segments.next().is_some()
}

/// The comma-separated items of every `name` header.
fn list_items<'a>(
    headers: &'a HeaderMap,
    name: &HeaderName,
) -> impl Iterator<Item = &'a str> + use<'a> {
    headers
        .get_all(name)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
}

/// The `q` parameter among `params`, in thousandths; 1000 without one,
/// `None` when malformed.
fn quality<'a>(params: impl Iterator<Item = &'a str>) -> Option<u16> {
    for param in params {
        let (name, value) = param.split_once('=')?;
        if name.trim().eq_ignore_ascii_case("q") {
            return parse_quality(value.trim());
        }
    }
    Some(1000)
}

/// An RFC 9110 weight: `0`, `0.` and up to three digits, or `1` with up
/// to three zeros.
fn parse_quality(value: &str) -> Option<u16> {
    let (whole, fraction) = value.split_once('.').unwrap_or((value, ""));
    if fraction.len() > 3 || !fraction.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let thousandths: u16 = format!("{fraction:0<3}").parse().ok()?;
    match whole {
        "0" => Some(thousandths),
        "1" if thousandths == 0 => Some(1000),
        _ => None,
    }
}

/// Whether `If-None-Match` matches `etag` (weak comparison, `*` matches
/// any file).
fn none_match(request: &HeaderMap, etag: &HeaderValue) -> bool {
    let Ok(ours) = etag.to_str() else {
        return false;
    };
    let ours = ours.trim_start_matches("W/");
    list_items(request, &IF_NONE_MATCH).any(|tag| {
        let tag = tag.trim();
        tag == "*" || tag.trim_start_matches("W/") == ours
    })
}

fn header_names(headers: &HeaderMap) -> Vec<&str> {
    headers.keys().map(HeaderName::as_str).collect()
}

fn common_headers(varies_on_accept: bool) -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    headers.insert(
        VARY,
        HeaderValue::from_static(if varies_on_accept {
            VARY_ENCODING_AND_ACCEPT
        } else {
            VARY_ENCODING
        }),
    );
    headers
}

fn with_headers(status: StatusCode, headers: HeaderMap, body: Body) -> Response {
    let mut response = Response::new(body);
    *response.status_mut() = status;
    *response.headers_mut() = headers;
    response
}

/// An empty body that announces no size: axum turns an exact size into
/// `Content-Length`, and on a `304` that header may only repeat the length
/// of the `200` (RFC 9110, 8.6).
fn unsized_empty_body() -> Body {
    Body::from_stream(futures::stream::empty::<Result<Bytes, Infallible>>())
}

fn not_found(varies_on_accept: bool) -> Response {
    let mut headers = common_headers(varies_on_accept);
    headers.insert(CACHE_CONTROL, HeaderValue::from_static(NO_CACHE));
    with_headers(StatusCode::NOT_FOUND, headers, Body::empty())
}

/// `308` to `path` with a trailing `/` and `query`. Built from the
/// normalized path, so a request for `//evil.example` cannot produce a
/// protocol-relative `Location`.
fn redirect(path: &VfsPath, query: Option<&str>) -> Response {
    let mut location = String::new();
    for segment in path.segments() {
        location.push('/');
        location.extend(utf8_percent_encode(segment, SEGMENT));
    }
    location.push('/');
    if let Some(query) = query {
        location.push('?');
        location.extend(utf8_percent_encode(query, QUERY));
    }
    let mut headers = common_headers(false);
    headers.insert(CACHE_CONTROL, HeaderValue::from_static(NO_CACHE));
    headers.insert(
        LOCATION,
        HeaderValue::try_from(location).expect("percent-encoded ASCII is a valid header value"),
    );
    with_headers(StatusCode::PERMANENT_REDIRECT, headers, Body::empty())
}

fn method_not_allowed() -> Response {
    let mut headers = common_headers(false);
    headers.insert(ALLOW, HeaderValue::from_static("GET, HEAD"));
    with_headers(StatusCode::METHOD_NOT_ALLOWED, headers, Body::empty())
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::time::SystemTime;

    use axum::body::HttpBody as _;
    use http::header::CONTENT_SECURITY_POLICY;
    use http_body_util::BodyExt;
    use sekvent_error::ErrorCode;
    use tower::ServiceExt;

    use super::*;

    struct Memory(HashMap<String, VfsFile>);

    impl Vfs for Memory {
        fn get<'a>(
            &'a self,
            path: &'a VfsPath,
        ) -> BoxFuture<'a, Result<Option<VfsFile>, AppError>> {
            Box::pin(async move { Ok(self.0.get(path.as_str()).cloned()) })
        }
    }

    struct Broken;

    impl Vfs for Broken {
        fn get<'a>(
            &'a self,
            _path: &'a VfsPath,
        ) -> BoxFuture<'a, Result<Option<VfsFile>, AppError>> {
            Box::pin(async { Err(AppError::internal("the disk is gone")) })
        }
    }

    /// Fails for precompressed variants and serves everything else.
    struct BrokenVariants(Memory);

    impl Vfs for BrokenVariants {
        fn get<'a>(
            &'a self,
            path: &'a VfsPath,
        ) -> BoxFuture<'a, Result<Option<VfsFile>, AppError>> {
            let variant = matches!(path.extension(), Some("br" | "gz"));
            if variant {
                return Box::pin(async { Err(AppError::internal("a name too long")) });
            }
            self.0.get(path)
        }
    }

    fn file(path: &str, contents: &str) -> (String, VfsFile) {
        let vfs_path = VfsPath::new(path).unwrap();
        (
            path.to_owned(),
            VfsFile::from_contents(&vfs_path, contents.as_bytes().to_vec()),
        )
    }

    fn memory() -> Memory {
        let mut files: HashMap<String, VfsFile> = [
            file("index.html", "<!doctype html>root"),
            file("index.html.br", "br-index"),
            file("docs/index.html", "<!doctype html>docs"),
            file("my docs/index.html", "<!doctype html>mine"),
            file("assets/app.js", "js"),
            file("assets/app.js.br", "br-bytes"),
            file("assets/app.js.gz", "gz-bytes"),
            file("assets/page.html", "<p>"),
            file("assets/only.css.gz", "gz-css"),
            file("assets/custom.bin.br", "br-custom"),
            file("static/logo.png", "png"),
        ]
        .into_iter()
        .collect();
        let (path, robots) = file("robots.txt", "robots");
        files.insert(path, robots.with_last_modified(SystemTime::UNIX_EPOCH));
        let (path, custom) = file("assets/custom.bin", "custom");
        files.insert(
            path,
            custom.with_content_type(HeaderValue::from_static("application/x-custom")),
        );
        Memory(files)
    }

    fn files() -> Arc<dyn Vfs> {
        Arc::new(memory())
    }

    fn site() -> StaticSite {
        StaticSite::new(files())
    }

    struct Answer {
        status: StatusCode,
        headers: HeaderMap,
        body: Bytes,
    }

    impl Answer {
        fn header(&self, name: HeaderName) -> Option<&str> {
            self.headers.get(name).map(|value| value.to_str().unwrap())
        }

        fn text(&self) -> &str {
            std::str::from_utf8(&self.body).unwrap()
        }
    }

    async fn send(site: &StaticSite, request: Request<Body>) -> Answer {
        let response = site.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let headers = response.headers().clone();
        let body = response.into_body().collect().await.unwrap().to_bytes();
        Answer {
            status,
            headers,
            body,
        }
    }

    fn get(path: &str) -> Request<Body> {
        Request::get(path).body(Body::empty()).unwrap()
    }

    fn with(path: &str, name: HeaderName, value: &str) -> Request<Body> {
        Request::get(path)
            .header(name, value)
            .body(Body::empty())
            .unwrap()
    }

    fn page(path: &str) -> Request<Body> {
        with(
            path,
            ACCEPT,
            "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8",
        )
    }

    #[tokio::test]
    async fn files_get_every_header() {
        let answer = send(&site(), get("/robots.txt")).await;
        assert_eq!(answer.status, StatusCode::OK);
        assert_eq!(answer.text(), "robots");
        assert_eq!(
            answer.header(CONTENT_TYPE),
            Some("text/plain; charset=utf-8")
        );
        assert_eq!(answer.header(CONTENT_LENGTH), Some("6"));
        assert_eq!(answer.header(CACHE_CONTROL), Some(NO_CACHE));
        assert_eq!(answer.header(X_CONTENT_TYPE_OPTIONS), Some("nosniff"));
        assert_eq!(answer.header(X_FRAME_OPTIONS), Some("DENY"));
        assert_eq!(answer.header(VARY), Some("Accept-Encoding"));
        assert_eq!(
            answer.header(LAST_MODIFIED),
            Some("Thu, 01 Jan 1970 00:00:00 GMT")
        );
        assert_eq!(answer.header(CONTENT_ENCODING), None);
        let etag = answer.header(ETAG).unwrap();
        assert!(etag.starts_with('"') && etag.len() == 34, "{etag}");
    }

    #[tokio::test]
    async fn the_root_and_directories_serve_their_index() {
        let site = site();
        for (path, body) in [
            ("/", "<!doctype html>root"),
            ("//", "<!doctype html>root"),
            ("/docs/", "<!doctype html>docs"),
            ("/docs/index.html", "<!doctype html>docs"),
        ] {
            let answer = send(&site, get(path)).await;
            assert_eq!(answer.status, StatusCode::OK, "{path}");
            assert_eq!(answer.text(), body, "{path}");
            assert_eq!(answer.header(CACHE_CONTROL), Some(NO_CACHE), "{path}");
            assert_eq!(
                answer.header(CONTENT_TYPE),
                Some("text/html; charset=utf-8")
            );
        }
        let answer = send(&site, get("/assets/")).await;
        assert_eq!(answer.status, StatusCode::NOT_FOUND, "no assets/index.html");
    }

    #[tokio::test]
    async fn directories_without_a_slash_redirect() {
        let site = site();
        for (path, location) in [
            ("/docs", "/docs/"),
            ("/docs?tab=a&q=%20x", "/docs/?tab=a&q=%20x"),
            ("//docs", "/docs/"),
            ("/my%20docs", "/my%20docs/"),
        ] {
            for request in [page(path), get(path)] {
                let answer = send(&site, request).await;
                assert_eq!(answer.status, StatusCode::PERMANENT_REDIRECT, "{path}");
                assert_eq!(answer.header(LOCATION), Some(location), "{path}");
                assert_eq!(answer.header(CACHE_CONTROL), Some(NO_CACHE));
                assert_eq!(answer.header(VARY), Some("Accept-Encoding"));
                assert_eq!(answer.header(X_FRAME_OPTIONS), Some("DENY"));
                assert!(answer.body.is_empty());
            }
        }
        let head = Request::head("/docs").body(Body::empty()).unwrap();
        assert_eq!(
            send(&site, head).await.status,
            StatusCode::PERMANENT_REDIRECT
        );
        let answer = send(&site, get("/")).await;
        assert_eq!(answer.status, StatusCode::OK, "the root never redirects");
    }

    #[test]
    fn redirects_escape_what_a_location_cannot_hold() {
        let path = VfsPath::new("a b/c?d#e%f/é").unwrap();
        let response = redirect(&path, Some("x=é \"y\""));
        assert_eq!(
            response.headers()[LOCATION],
            "/a%20b/c%3Fd%23e%25f/%C3%A9/?x=%C3%A9%20%22y%22"
        );
    }

    #[tokio::test]
    async fn hashed_assets_are_immutable_and_html_never_is() {
        let site = site();
        let answer = send(&site, get("/assets/app.js")).await;
        assert_eq!(answer.header(CACHE_CONTROL), Some(IMMUTABLE));
        let answer = send(&site, get("/assets/page.html")).await;
        assert_eq!(answer.header(CACHE_CONTROL), Some(NO_CACHE));
        let answer = send(&site, get("/static/logo.png")).await;
        assert_eq!(answer.header(CACHE_CONTROL), Some(NO_CACHE));
        assert_eq!(answer.header(CONTENT_TYPE), Some("image/png"));

        let custom = StaticSite::builder(files())
            .immutable_prefix("static/")
            .build()
            .unwrap();
        let answer = send(&custom, get("/static/logo.png")).await;
        assert_eq!(answer.header(CACHE_CONTROL), Some(IMMUTABLE));
        let answer = send(&custom, get("/assets/app.js")).await;
        assert_eq!(answer.header(CACHE_CONTROL), Some(NO_CACHE));

        let none = StaticSite::builder(files())
            .no_immutable_prefix()
            .build()
            .unwrap();
        let answer = send(&none, get("/assets/app.js")).await;
        assert_eq!(answer.header(CACHE_CONTROL), Some(NO_CACHE));
        let answer = send(&none, page("/assets/gone.js")).await;
        assert_eq!(answer.status, StatusCode::OK, "no prefix, no hard 404");
    }

    #[tokio::test]
    async fn navigations_fall_back_to_the_index() {
        let site = site();
        for path in [
            "/settings/profile",
            "/settings/",
            "/nodes/10.0.0.1",
            "/instances/ocid1.instance.oc1.eu-frankfurt-1.abc",
            "/hosts/api.example.com",
            "/releases/v1.2.3",
            "/gone.js",
            "/assets",
        ] {
            let answer = send(&site, page(path)).await;
            assert_eq!(answer.status, StatusCode::OK, "{path}");
            assert_eq!(answer.text(), "<!doctype html>root", "{path}");
            assert_eq!(answer.header(CACHE_CONTROL), Some(NO_CACHE), "{path}");
            assert_eq!(answer.header(VARY), Some("Accept-Encoding, Accept"));
            assert_eq!(
                answer.header(CONTENT_TYPE),
                Some("text/html; charset=utf-8")
            );
        }

        for request in [
            get("/settings"),
            get("/nodes/10.0.0.1"),
            with("/settings", ACCEPT, "application/json"),
            with("/settings", ACCEPT, "*/*"),
            with("/settings", ACCEPT, "text/*"),
            with("/settings", ACCEPT, "text/html;q=0"),
            with("/settings", ACCEPT, "text/html;q=nope"),
        ] {
            let answer = send(&site, request).await;
            assert_eq!(answer.status, StatusCode::NOT_FOUND);
            assert_eq!(answer.header(VARY), Some("Accept-Encoding, Accept"));
            assert_eq!(answer.header(X_CONTENT_TYPE_OPTIONS), Some("nosniff"));
            assert!(answer.body.is_empty());
        }

        for path in ["/assets/gone.js", "/assets/deep/route", "/assets/x/"] {
            let answer = send(&site, page(path)).await;
            assert_eq!(
                answer.status,
                StatusCode::NOT_FOUND,
                "{path}: never the index"
            );
            assert_eq!(answer.header(VARY), Some("Accept-Encoding"), "{path}");
        }

        let off = StaticSite::builder(files())
            .spa_fallback(false)
            .build()
            .unwrap();
        let answer = send(&off, page("/settings")).await;
        assert_eq!(answer.status, StatusCode::NOT_FOUND);
        assert_eq!(answer.header(VARY), Some("Accept-Encoding"));

        let empty = StaticSite::new(Arc::new(Memory(HashMap::new())));
        let answer = send(&empty, page("/")).await;
        assert_eq!(answer.status, StatusCode::NOT_FOUND, "no index at all");
        let answer = send(&empty, page("/settings")).await;
        assert_eq!(answer.status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn rejected_paths_fall_back_only_for_navigations() {
        let site = site();
        let long = format!("/{}", "a".repeat(300));
        for path in [
            "/../index.html",
            "/%2e%2e/index.html",
            "/assets%2fapp.js",
            "/.env",
            "/a%00b",
            "/c:/windows",
            "/nodes/fe80::1",
            long.as_str(),
        ] {
            let answer = send(&site, page(path)).await;
            assert_eq!(answer.status, StatusCode::OK, "{path}");
            assert_eq!(answer.text(), "<!doctype html>root", "{path}");
            assert_eq!(answer.header(VARY), Some("Accept-Encoding, Accept"));

            let answer = send(&site, get(path)).await;
            assert_eq!(answer.status, StatusCode::NOT_FOUND, "{path}");
            assert_eq!(answer.header(VARY), Some("Accept-Encoding, Accept"));
        }

        for path in ["/assets/%2e%2e/x.js", "/assets/.hidden", "//assets//a:b"] {
            let answer = send(&site, page(path)).await;
            assert_eq!(answer.status, StatusCode::NOT_FOUND, "{path}");
            assert_eq!(answer.header(VARY), Some("Accept-Encoding"), "{path}");
        }

        let off = StaticSite::builder(files())
            .spa_fallback(false)
            .build()
            .unwrap();
        let answer = send(&off, page("/.env")).await;
        assert_eq!(answer.status, StatusCode::NOT_FOUND);
        assert_eq!(answer.header(VARY), Some("Accept-Encoding"));
    }

    #[test]
    fn rejected_paths_are_matched_against_the_prefix_leniently() {
        let assets = VfsPath::new("assets").unwrap();
        let nested = VfsPath::new("static/js").unwrap();
        assert!(raw_is_below("/assets/a:b", &assets));
        assert!(raw_is_below("//%61ssets/..", &assets));
        assert!(raw_is_below("/static/js/%ff", &nested));
        assert!(!raw_is_below("/assets", &assets));
        assert!(!raw_is_below("/assets/", &assets));
        assert!(!raw_is_below("/a:b/assets/x", &assets));
        assert!(!raw_is_below("/static/a:b", &nested));
        assert!(!raw_is_below("/assets%2fx", &assets));
    }

    #[tokio::test]
    async fn a_matching_etag_is_not_modified() {
        let site = site();
        let first = send(&site, get("/assets/app.js")).await;
        let etag = first.header(ETAG).unwrap().to_owned();
        for condition in [
            etag.clone(),
            format!("\"other\", {etag}"),
            format!("W/{etag}"),
            "*".to_owned(),
        ] {
            let answer = send(&site, with("/assets/app.js", IF_NONE_MATCH, &condition)).await;
            assert_eq!(answer.status, StatusCode::NOT_MODIFIED, "{condition}");
            assert!(answer.body.is_empty());
            assert_eq!(answer.header(ETAG), Some(etag.as_str()));
            assert_eq!(answer.header(CACHE_CONTROL), Some(IMMUTABLE));
            assert_eq!(answer.header(VARY), Some("Accept-Encoding"));
            assert_eq!(answer.header(X_FRAME_OPTIONS), Some("DENY"));
            assert_eq!(answer.header(CONTENT_LENGTH), None);
        }
        let answer = send(&site, with("/assets/app.js", IF_NONE_MATCH, "\"other\"")).await;
        assert_eq!(answer.status, StatusCode::OK);
        assert_eq!(answer.text(), "js");
    }

    #[tokio::test]
    async fn a_not_modified_body_announces_no_size() {
        let site = site();
        let first = send(&site, get("/assets/app.js")).await;
        let mut headers = HeaderMap::new();
        headers.insert(IF_NONE_MATCH, first.headers[ETAG].clone());
        let uri = Uri::from_static("/assets/app.js");
        let response = site.respond(&Method::GET, &uri, &headers).await;
        assert_eq!(response.status(), StatusCode::NOT_MODIFIED);
        assert_eq!(response.body().size_hint().exact(), None);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        assert!(body.is_empty());
    }

    #[tokio::test]
    async fn head_has_the_headers_without_the_body() {
        let site = site();
        let request = Request::head("/assets/app.js").body(Body::empty()).unwrap();
        let answer = send(&site, request).await;
        assert_eq!(answer.status, StatusCode::OK);
        assert_eq!(answer.header(CONTENT_LENGTH), Some("2"));
        assert!(answer.header(CONTENT_TYPE).unwrap().contains("javascript"));
        assert!(answer.header(ETAG).is_some());
        assert!(answer.body.is_empty());
    }

    #[tokio::test]
    async fn other_methods_are_not_allowed() {
        for method in [Method::POST, Method::PUT, Method::DELETE, Method::OPTIONS] {
            let request = Request::builder()
                .method(method.clone())
                .uri("/index.html")
                .body(Body::empty())
                .unwrap();
            let answer = send(&site(), request).await;
            assert_eq!(answer.status, StatusCode::METHOD_NOT_ALLOWED, "{method}");
            assert_eq!(answer.header(ALLOW), Some("GET, HEAD"));
            assert_eq!(answer.header(X_CONTENT_TYPE_OPTIONS), Some("nosniff"));
            assert_eq!(answer.header(VARY), Some("Accept-Encoding"));
            assert_eq!(answer.header(X_FRAME_OPTIONS), Some("DENY"));
        }
    }

    #[tokio::test]
    async fn precompressed_variants_follow_accept_encoding() {
        let site = site();
        let identity = send(&site, get("/assets/app.js")).await;
        let identity_etag = identity.header(ETAG).unwrap().to_owned();
        for (accept, body, encoding) in [
            ("gzip, deflate, br", "br-bytes", Some("br")),
            ("BR", "br-bytes", Some("br")),
            ("gzip", "gz-bytes", Some("gzip")),
            ("x-gzip", "gz-bytes", Some("gzip")),
            ("br;q=0, gzip", "gz-bytes", Some("gzip")),
            ("gzip;q=0.5, br;q=0.4", "gz-bytes", Some("gzip")),
            ("gzip;q=0.5, br;q=0.5", "br-bytes", Some("br")),
            ("*", "br-bytes", Some("br")),
            ("br;q=0, *", "gz-bytes", Some("gzip")),
            ("*;q=0", "js", None),
            ("br, br;q=0", "js", None),
            ("identity", "js", None),
            ("deflate", "js", None),
            ("br;q=2", "js", None),
            ("br;q", "js", None),
            ("", "js", None),
        ] {
            let answer = send(&site, with("/assets/app.js", ACCEPT_ENCODING, accept)).await;
            assert_eq!(answer.status, StatusCode::OK, "{accept:?}");
            assert_eq!(answer.text(), body, "{accept:?}");
            assert_eq!(answer.header(CONTENT_ENCODING), encoding, "{accept:?}");
            assert!(
                answer.header(CONTENT_TYPE).unwrap().contains("javascript"),
                "{accept:?}"
            );
            assert_eq!(answer.header(VARY), Some("Accept-Encoding"));
            assert_eq!(
                answer.header(CONTENT_LENGTH),
                Some(body.len().to_string().as_str())
            );
            assert_eq!(
                answer.header(ETAG) == Some(identity_etag.as_str()),
                encoding.is_none(),
                "{accept:?}"
            );
            assert_eq!(answer.header(CACHE_CONTROL), Some(IMMUTABLE));
        }

        let br = send(&site, with("/assets/app.js", ACCEPT_ENCODING, "br")).await;
        let mut request = with("/assets/app.js", ACCEPT_ENCODING, "br");
        request
            .headers_mut()
            .insert(IF_NONE_MATCH, br.headers[ETAG].clone());
        assert_eq!(send(&site, request).await.status, StatusCode::NOT_MODIFIED);

        let answer = send(&site, with("/assets/only.css", ACCEPT_ENCODING, "gzip")).await;
        assert_eq!(
            answer.status,
            StatusCode::NOT_FOUND,
            "a variant without its file"
        );

        let answer = send(&site, with("/assets/custom.bin", ACCEPT_ENCODING, "br")).await;
        assert_eq!(answer.text(), "br-custom");
        assert_eq!(
            answer.header(CONTENT_TYPE),
            Some("application/x-custom"),
            "the file's own content type, not a guess from the variant's name"
        );

        let mut request = page("/client/route");
        request
            .headers_mut()
            .insert(ACCEPT_ENCODING, HeaderValue::from_static("br"));
        let answer = send(&site, request).await;
        assert_eq!(
            answer.text(),
            "br-index",
            "the fallback index is compressed too"
        );
        assert_eq!(answer.header(CONTENT_ENCODING), Some("br"));
        assert_eq!(answer.header(VARY), Some("Accept-Encoding, Accept"));
    }

    #[tokio::test]
    async fn unreadable_variants_fall_through_to_the_file() {
        let site = StaticSite::new(Arc::new(BrokenVariants(memory())));
        let answer = send(&site, with("/assets/app.js", ACCEPT_ENCODING, "br, gzip")).await;
        assert_eq!(answer.status, StatusCode::OK);
        assert_eq!(answer.text(), "js");
        assert_eq!(answer.header(CONTENT_ENCODING), None);
    }

    #[tokio::test]
    async fn lookup_failures_are_internal_errors() {
        let site = StaticSite::new(Arc::new(Broken));
        let answer = send(&site, get("/index.html")).await;
        assert_eq!(answer.status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(answer.header(X_CONTENT_TYPE_OPTIONS), Some("nosniff"));
        assert_eq!(answer.header(VARY), Some("Accept-Encoding, Accept"));
        assert_eq!(answer.header(X_FRAME_OPTIONS), Some("DENY"));
        assert!(!answer.text().contains("disk"));

        let off = StaticSite::builder(Arc::new(Broken))
            .spa_fallback(false)
            .build()
            .unwrap();
        let answer = send(&off, get("/index.html")).await;
        assert_eq!(answer.status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(answer.header(VARY), Some("Accept-Encoding"));
    }

    #[tokio::test]
    async fn extra_headers_go_on_every_response() {
        let csp = "default-src 'self'; frame-ancestors 'none'";
        let site = StaticSite::builder(files())
            .header(X_FRAME_OPTIONS, HeaderValue::from_static("SAMEORIGIN"))
            .header(CONTENT_SECURITY_POLICY, HeaderValue::from_static(csp))
            .header(CACHE_CONTROL, HeaderValue::from_static("no-store"))
            .build()
            .unwrap();
        let post = Request::post("/").body(Body::empty()).unwrap();
        for request in [get("/robots.txt"), get("/gone"), get("/docs"), post] {
            let answer = send(&site, request).await;
            assert_eq!(answer.header(X_FRAME_OPTIONS), Some("SAMEORIGIN"));
            assert_eq!(answer.header(CONTENT_SECURITY_POLICY), Some(csp));
            assert_eq!(answer.header(CACHE_CONTROL), Some("no-store"), "replaced");
            assert_eq!(answer.headers.get_all(CACHE_CONTROL).iter().count(), 1);
        }

        let bare = StaticSite::builder(files())
            .without_header(&X_FRAME_OPTIONS)
            .without_header(&X_CONTENT_TYPE_OPTIONS)
            .build()
            .unwrap();
        let answer = send(&bare, get("/robots.txt")).await;
        assert_eq!(answer.header(X_FRAME_OPTIONS), None);
        assert_eq!(
            answer.header(X_CONTENT_TYPE_OPTIONS),
            Some("nosniff"),
            "the site's own headers stay"
        );
    }

    #[tokio::test]
    async fn the_builder_validates_and_configures() {
        for index in ["", "/", "docs/index.html", "../index.html", ".index"] {
            let error = StaticSite::builder(files())
                .index(index)
                .build()
                .unwrap_err();
            assert_eq!(error.code(), ErrorCode::InvalidArgument, "{index:?}");
        }
        for prefix in ["", "/", "..", "a\\b"] {
            let error = StaticSite::builder(files())
                .immutable_prefix(prefix)
                .build()
                .unwrap_err();
            assert_eq!(error.code(), ErrorCode::InvalidArgument, "{prefix:?}");
        }

        let builder = StaticSite::builder(files()).index("/robots.txt");
        let debug = format!("{builder:?}");
        assert!(
            debug.contains("robots.txt") && debug.contains("x-frame-options"),
            "{debug}"
        );
        let site = builder.build().unwrap();
        let debug = format!("{site:?}");
        assert!(
            debug.contains("index: \"robots.txt\"")
                && debug.contains("Some(\"assets\")")
                && debug.contains("[\"x-frame-options\"]"),
            "{debug}"
        );
        assert_eq!(send(&site, get("/")).await.text(), "robots");
    }

    #[test]
    fn weights_follow_rfc_9110() {
        for (value, expected) in [
            ("1", Some(1000)),
            ("1.", Some(1000)),
            ("1.000", Some(1000)),
            ("0", Some(0)),
            ("0.", Some(0)),
            ("0.5", Some(500)),
            ("0.05", Some(50)),
            ("0.123", Some(123)),
            ("0.1234", None),
            ("1.5", None),
            ("1.0001", None),
            ("2", None),
            ("", None),
            ("0.x", None),
            ("-0", None),
            (".5", None),
        ] {
            assert_eq!(parse_quality(value), expected, "{value:?}");
        }
        assert_eq!(quality("".split(';').skip(1)), Some(1000));
        assert_eq!(quality(" level=1; Q = 0.2 ".split(';')), Some(200));
        assert_eq!(quality("bare".split(';')), None);
    }
}
