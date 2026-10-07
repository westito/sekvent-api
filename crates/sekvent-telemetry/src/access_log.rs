//! Request ids, one `request` span and one completion event per request.
//!
//! [`AccessLogLayer`] is [`RequestIdLayer`](crate::request_id::RequestIdLayer)
//! plus an access log, so a stack needs only one of them:
//!
//! - **Ids** follow the request-id rules exactly: a valid incoming
//!   `x-request-id` (1–128 visible ASCII characters) is kept, anything else
//!   is replaced by a fresh UUID v7, repeated headers collapse to one. The
//!   id is stored as [`RequestId`] in the request extensions and echoed on
//!   the response unless the handler set its own.
//! - **Span**: `request` with `request_id`, `method` and `path`, entered for
//!   the inner call and instrumenting its future; `status`, `grpc_status`
//!   and `latency_ms` are recorded when the request completes.
//! - **Event**: target [`ACCESS_LOG_TARGET`], message `request completed`,
//!   emitted exactly once, when the response body ends, fails or is
//!   dropped (or when the inner service fails or its future is dropped).
//!   A response that has no body by definition (to `HEAD`, or `204`/`304`)
//!   completes when its headers are ready. Fields: `request_id` (also on
//!   the event, so it survives a filter that disables the `info` span),
//!   `method`, `path` (never the query, at most 256 characters), `route`
//!   (the [`RouteTemplate`] inner routing put on the response; for gRPC the
//!   path; empty when nothing matched), `protocol` (`http`, `grpc` or
//!   `grpc-web`, by the request's content type), `status`, `grpc_status`
//!   (gRPC only: from the trailers, the headers of a trailers-only answer,
//!   or the trailer frame of a gRPC-Web body, binary or base64 text as the
//!   response's content type says), `latency_ms` (to the end of the body)
//!   and `aborted` (the response did not reach its end).
//!
//! Headers, query strings, bodies, peer addresses and user agents are never
//! logged. Paths are, so secrets must not travel in paths. Reading a
//! gRPC-Web body for its trailer frame holds at most a frame header, three
//! base64 characters and a trailer block of 8 KiB; a longer block leaves
//! `grpc_status` unset.
//!
//! Levels: `debug` for quiet paths ([`AccessLogLayer::quiet`]), `warn` for
//! HTTP 5xx and for `grpc_status` 2, 13 or 15 (`UNKNOWN`, `INTERNAL`,
//! `DATA_LOSS`), `info` otherwise. A filter such as
//! `info,sekvent::access=warn` keeps only the failures.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll, ready};
use std::time::Instant;

use bytes::Buf;
use http::header::CONTENT_TYPE;
use http::{HeaderMap, HeaderValue, Method, Request, Response, StatusCode};
use http_body::{Body, Frame, SizeHint};
use pin_project_lite::pin_project;
use tower::{Layer, Service};
use tracing::field::Empty;
use tracing::instrument::{Instrument, Instrumented};
use tracing::{Level, Span};

use crate::request_id::{REQUEST_ID_HEADER, RequestId, ensure_request_id};
use crate::truncate_for_log;

/// Target of the completion events.
pub const ACCESS_LOG_TARGET: &str = "sekvent::access";

/// Longest path logged, in characters.
const MAX_PATH_CHARS: usize = 256;

/// gRPC codes logged at `warn`: `UNKNOWN`, `INTERNAL`, `DATA_LOSS`.
const GRPC_FAILURES: [i32; 3] = [2, 13, 15];

/// Flag of a gRPC-Web frame that carries the trailers.
const GRPC_WEB_TRAILERS: u8 = 0x80;

/// Largest gRPC-Web trailer block read for its `grpc-status`.
const MAX_TRAILER_BYTES: usize = 8 * 1024;

/// Put into a response's extensions by inner routing: the matched route
/// template (for example `/api/orders/{id}`), logged as `route`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouteTemplate(
    /// The template.
    pub String,
);

/// Request ids, the per-request span and one completion event; see the
/// [module docs](self).
#[derive(Debug, Clone)]
pub struct AccessLogLayer {
    quiet: Arc<[String]>,
    events: bool,
}

impl Default for AccessLogLayer {
    fn default() -> Self {
        Self {
            quiet: Arc::from(Vec::new()),
            events: true,
        }
    }
}

impl AccessLogLayer {
    /// Events on, no quiet paths; also `Default`.
    pub fn new() -> Self {
        Self::default()
    }

    /// Log these paths at `debug`; an entry ending in `/` is a prefix.
    /// Calls add to the earlier entries.
    #[must_use]
    pub fn quiet<I, S>(mut self, paths: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let mut all = self.quiet.to_vec();
        all.extend(paths.into_iter().map(Into::into));
        self.quiet = all.into();
        self
    }

    /// Whether to emit the completion event (default true). Ids and the
    /// span stay on either way.
    #[must_use]
    pub fn events(mut self, enabled: bool) -> Self {
        self.events = enabled;
        self
    }
}

impl<S> Layer<S> for AccessLogLayer {
    type Service = AccessLogService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        AccessLogService {
            inner,
            quiet: Arc::clone(&self.quiet),
            events: self.events,
        }
    }
}

/// Middleware applying [`AccessLogLayer`]; see the [module docs](self).
#[derive(Debug, Clone)]
pub struct AccessLogService<S> {
    inner: S,
    quiet: Arc<[String]>,
    events: bool,
}

impl<S, ReqBody, ResBody> Service<Request<ReqBody>> for AccessLogService<S>
where
    S: Service<Request<ReqBody>, Response = Response<ResBody>>,
    ResBody: Body,
{
    type Response = Response<AccessLogBody<ResBody>>;
    type Error = S::Error;
    type Future = ResponseFuture<S::Future>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, mut request: Request<ReqBody>) -> Self::Future {
        let started = Instant::now();
        let id = ensure_request_id(&mut request);
        let request_id = id.to_str().unwrap_or_default().to_owned();
        let method = request.method().clone();
        let quiet = is_quiet(&self.quiet, request.uri().path());
        let path = truncate_for_log(request.uri().path(), MAX_PATH_CHARS).into_owned();
        let protocol = Protocol::of(request.headers());
        let span = tracing::info_span!(
            "request",
            request_id = request_id.as_str(),
            method = method.as_str(),
            path = path.as_str(),
            status = Empty,
            grpc_status = Empty,
            latency_ms = Empty
        );
        request.extensions_mut().insert(RequestId::new(id.clone()));
        let inner = {
            let _entered = span.enter();
            self.inner.call(request)
        };
        ResponseFuture {
            inner: inner.instrument(span.clone()),
            pending: Some(Pending {
                id,
                completion: Completion {
                    span,
                    request_id,
                    method,
                    path,
                    route: String::new(),
                    protocol,
                    status: None,
                    grpc_status: None,
                    scanner: None,
                    started,
                    quiet,
                    events: self.events,
                },
            }),
        }
    }
}

/// Whether `path` is one of the quiet entries (exact, or a prefix when the
/// entry ends in `/`).
fn is_quiet(quiet: &[String], path: &str) -> bool {
    quiet.iter().any(|entry| {
        if entry.ends_with('/') {
            path.starts_with(entry.as_str())
        } else {
            path == entry
        }
    })
}

/// The wire protocol of a request, by its content type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Protocol {
    Http,
    Grpc,
    GrpcWeb,
    GrpcWebText,
}

impl Protocol {
    fn of(headers: &HeaderMap) -> Self {
        let Some(content_type) = headers
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .map(str::to_ascii_lowercase)
        else {
            return Self::Http;
        };
        if content_type.starts_with("application/grpc-web-text") {
            Self::GrpcWebText
        } else if content_type.starts_with("application/grpc-web") {
            Self::GrpcWeb
        } else if content_type.starts_with("application/grpc") {
            Self::Grpc
        } else {
            Self::Http
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Http => "http",
            Self::Grpc => "grpc",
            Self::GrpcWeb | Self::GrpcWebText => "grpc-web",
        }
    }

    fn is_grpc(self) -> bool {
        self != Self::Http
    }
}

/// The completion event at a level known at compile time.
macro_rules! completed {
    ($level:expr, $completion:expr, $latency_ms:expr, $aborted:expr) => {
        tracing::event!(
            target: "sekvent::access",
            $level,
            request_id = $completion.request_id.as_str(),
            method = $completion.method.as_str(),
            path = $completion.path.as_str(),
            route = $completion.route.as_str(),
            protocol = $completion.protocol.as_str(),
            status = $completion.status,
            grpc_status = $completion.grpc_status,
            latency_ms = $latency_ms,
            aborted = $aborted,
            "request completed"
        )
    };
}

/// What the completion event needs, gathered along the way.
struct Completion {
    span: Span,
    request_id: String,
    method: Method,
    path: String,
    route: String,
    protocol: Protocol,
    status: Option<u16>,
    grpc_status: Option<i32>,
    /// Reads a gRPC-Web response body for its trailer frame.
    scanner: Option<TrailerScanner>,
    started: Instant,
    quiet: bool,
    events: bool,
}

impl Completion {
    fn respond(&mut self, status: u16, headers: &HeaderMap, route: Option<&RouteTemplate>) {
        self.status = Some(status);
        if self.protocol.is_grpc() {
            self.grpc_status = grpc_status(headers);
            self.scanner = match Protocol::of(headers) {
                Protocol::GrpcWebText => Some(TrailerScanner::new(true)),
                Protocol::GrpcWeb => Some(TrailerScanner::new(false)),
                Protocol::Grpc | Protocol::Http => None,
            };
        }
        self.route = match route {
            Some(RouteTemplate(template)) => template.clone(),
            None if self.protocol.is_grpc() => self.path.clone(),
            None => String::new(),
        };
    }

    fn observe<D: Buf>(&mut self, frame: &Frame<D>) {
        let code = if let Some(trailers) = frame.trailers_ref() {
            grpc_status(trailers).filter(|_| self.protocol.is_grpc())
        } else if let Some(data) = frame.data_ref()
            && let Some(scanner) = self.scanner.as_mut()
        {
            let chunk = data.chunk();
            let code = scanner.feed(chunk);
            if chunk.len() < data.remaining() {
                // Only the first piece of a split buffer can be read in place.
                scanner.give_up();
            }
            code
        } else {
            None
        };
        if code.is_some() {
            self.grpc_status = code;
        }
    }

    /// Whether the response has no body by definition, so it is complete
    /// once its headers are ready.
    fn has_no_body(&self) -> bool {
        self.method == Method::HEAD
            || self.status.is_some_and(|status| {
                status == StatusCode::NO_CONTENT.as_u16()
                    || status == StatusCode::NOT_MODIFIED.as_u16()
            })
    }

    fn level(&self) -> Level {
        if self.quiet {
            Level::DEBUG
        } else if self.status.is_some_and(|status| status >= 500)
            || self
                .grpc_status
                .is_some_and(|code| GRPC_FAILURES.contains(&code))
        {
            Level::WARN
        } else {
            Level::INFO
        }
    }

    fn finish(self, aborted: bool) {
        let latency_ms = u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX);
        if let Some(status) = self.status {
            self.span.record("status", status);
        }
        if let Some(code) = self.grpc_status {
            self.span.record("grpc_status", code);
        }
        self.span.record("latency_ms", latency_ms);
        if !self.events {
            return;
        }
        let level = self.level();
        let _entered = self.span.enter();
        match level {
            Level::DEBUG => completed!(Level::DEBUG, self, latency_ms, aborted),
            Level::WARN => completed!(Level::WARN, self, latency_ms, aborted),
            _ => completed!(Level::INFO, self, latency_ms, aborted),
        }
    }
}

/// The `grpc-status` of a header or trailer map.
fn grpc_status(headers: &HeaderMap) -> Option<i32> {
    headers
        .get("grpc-status")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse().ok())
}

/// Follows a gRPC-Web body frame by frame (flag byte, four-byte length,
/// payload) as it streams past, decoding base64 for `grpc-web-text`, to
/// read the `grpc-status` of the trailer frame (flag `0x80`).
#[derive(Debug)]
struct TrailerScanner {
    /// The body is base64 text.
    text: bool,
    /// Base64 characters waiting for a full group of four.
    group: [u8; 4],
    group_len: usize,
    header: [u8; 5],
    header_len: usize,
    state: ScanState,
    trailer: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScanState {
    /// Reading a frame header.
    Header,
    /// Skipping the rest of a message frame.
    Skip(usize),
    /// Reading a trailer block of this length.
    Trailer(usize),
    /// Finished, or the body cannot be followed.
    Done,
}

impl TrailerScanner {
    fn new(text: bool) -> Self {
        Self {
            text,
            group: [0; 4],
            group_len: 0,
            header: [0; 5],
            header_len: 0,
            state: ScanState::Header,
            trailer: Vec::new(),
        }
    }

    /// Stop looking.
    fn give_up(&mut self) {
        self.state = ScanState::Done;
        self.trailer = Vec::new();
    }

    /// Read the next piece of the body; the status once the trailer frame
    /// is complete.
    fn feed(&mut self, chunk: &[u8]) -> Option<i32> {
        if !self.text {
            return self.frames(chunk);
        }
        for &byte in chunk {
            if self.state == ScanState::Done {
                return None;
            }
            self.group[self.group_len] = byte;
            self.group_len += 1;
            if self.group_len < self.group.len() {
                continue;
            }
            self.group_len = 0;
            let Some((decoded, len)) = decode_base64_group(self.group) else {
                self.give_up();
                return None;
            };
            if let Some(code) = self.frames(&decoded[..len]) {
                return Some(code);
            }
        }
        None
    }

    /// Read decoded frame bytes.
    fn frames(&mut self, mut bytes: &[u8]) -> Option<i32> {
        while !bytes.is_empty() {
            match self.state {
                ScanState::Done => return None,
                ScanState::Header => {
                    let take = (self.header.len() - self.header_len).min(bytes.len());
                    self.header[self.header_len..self.header_len + take]
                        .copy_from_slice(&bytes[..take]);
                    self.header_len += take;
                    bytes = &bytes[take..];
                    if self.header_len == self.header.len() {
                        self.header_len = 0;
                        self.state = self.next_frame();
                    }
                }
                ScanState::Skip(left) => {
                    let take = left.min(bytes.len());
                    bytes = &bytes[take..];
                    self.state = if take == left {
                        ScanState::Header
                    } else {
                        ScanState::Skip(left - take)
                    };
                }
                ScanState::Trailer(len) => {
                    let take = (len - self.trailer.len()).min(bytes.len());
                    self.trailer.extend_from_slice(&bytes[..take]);
                    bytes = &bytes[take..];
                    if self.trailer.len() == len {
                        let block = std::mem::take(&mut self.trailer);
                        self.state = ScanState::Done;
                        return trailer_block_status(&block);
                    }
                }
            }
        }
        None
    }

    /// What follows the frame header just read.
    fn next_frame(&self) -> ScanState {
        let [flag, length @ ..] = self.header;
        let len = usize::try_from(u32::from_be_bytes(length)).unwrap_or(usize::MAX);
        if flag & GRPC_WEB_TRAILERS == 0 {
            if len == 0 {
                ScanState::Header
            } else {
                ScanState::Skip(len)
            }
        } else if len == 0 || len > MAX_TRAILER_BYTES {
            ScanState::Done
        } else {
            ScanState::Trailer(len)
        }
    }
}

/// The bytes of one group of four base64 characters (standard alphabet,
/// `=` padding, which may also end a group in the middle of a body);
/// `None` for anything else.
fn decode_base64_group(group: [u8; 4]) -> Option<([u8; 3], usize)> {
    let len = match (group[2], group[3]) {
        (b'=', b'=') => 1,
        (_, b'=') => 2,
        _ => 3,
    };
    let mut bits = 0_u32;
    for &symbol in &group[..=len] {
        bits = (bits << 6) | u32::from(sextet(symbol)?);
    }
    bits <<= 6 * (3 - len);
    let [_, high, middle, low] = bits.to_be_bytes();
    Some(([high, middle, low], len))
}

/// The value of one base64 character.
fn sextet(byte: u8) -> Option<u8> {
    match byte {
        b'A'..=b'Z' => Some(byte - b'A'),
        b'a'..=b'z' => Some(byte - b'a' + 26),
        b'0'..=b'9' => Some(byte - b'0' + 52),
        b'+' => Some(62),
        b'/' => Some(63),
        _ => None,
    }
}

/// The `grpc-status` of a gRPC-Web trailer block (`name:value\r\n` lines).
fn trailer_block_status(block: &[u8]) -> Option<i32> {
    block.split(|&byte| byte == b'\n').find_map(|line| {
        let colon = line.iter().position(|&byte| byte == b':')?;
        let (name, value) = line.split_at(colon);
        if !name.trim_ascii().eq_ignore_ascii_case(b"grpc-status") {
            return None;
        }
        std::str::from_utf8(&value[1..]).ok()?.trim().parse().ok()
    })
}

/// The request id and the event state, until the inner service answers.
struct Pending {
    id: HeaderValue,
    completion: Completion,
}

pin_project! {
    /// Response future of [`AccessLogService`].
    pub struct ResponseFuture<F> {
        #[pin]
        inner: Instrumented<F>,
        pending: Option<Pending>,
    }

    impl<F> PinnedDrop for ResponseFuture<F> {
        fn drop(this: Pin<&mut Self>) {
            if let Some(pending) = this.project().pending.take() {
                pending.completion.finish(true);
            }
        }
    }
}

impl<F, B, E> Future for ResponseFuture<F>
where
    F: Future<Output = Result<Response<B>, E>>,
    B: Body,
{
    type Output = Result<Response<AccessLogBody<B>>, E>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.project();
        let result = ready!(this.inner.poll(cx));
        let Pending { id, mut completion } = this.pending.take().expect("polled after completion");
        match result {
            Ok(mut response) => {
                response
                    .headers_mut()
                    .entry(REQUEST_ID_HEADER)
                    .or_insert(id);
                completion.respond(
                    response.status().as_u16(),
                    response.headers(),
                    response.extensions().get::<RouteTemplate>(),
                );
                Poll::Ready(Ok(response.map(|body| AccessLogBody::new(body, completion))))
            }
            Err(error) => {
                completion.status = Some(500);
                completion.finish(true);
                Poll::Ready(Err(error))
            }
        }
    }
}

pin_project! {
    /// Response body of [`AccessLogService`]: the inner body, logging the
    /// request once it ends, fails or is dropped.
    pub struct AccessLogBody<B> {
        #[pin]
        inner: B,
        completion: Option<Completion>,
    }

    impl<B> PinnedDrop for AccessLogBody<B> {
        fn drop(this: Pin<&mut Self>) {
            if let Some(completion) = this.project().completion.take() {
                completion.finish(true);
            }
        }
    }
}

impl<B: Body> AccessLogBody<B> {
    fn new(inner: B, completion: Completion) -> Self {
        // An empty body, or one never sent, may never be polled.
        let completion = if inner.is_end_stream() || completion.has_no_body() {
            completion.finish(false);
            None
        } else {
            Some(completion)
        };
        Self { inner, completion }
    }
}

impl<B: Body> Body for AccessLogBody<B> {
    type Data = B::Data;
    type Error = B::Error;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        let mut this = self.project();
        let polled = ready!(this.inner.as_mut().poll_frame(cx));
        if let Some(completion) = this.completion.as_mut() {
            let ended = match &polled {
                None => Some(false),
                Some(Err(_)) => Some(true),
                Some(Ok(frame)) => {
                    completion.observe(frame);
                    (frame.is_trailers() || this.inner.is_end_stream()).then_some(false)
                }
            };
            if let Some(aborted) = ended
                && let Some(completion) = this.completion.take()
            {
                completion.finish(aborted);
            }
        }
        Poll::Ready(polled)
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::convert::Infallible;

    use bytes::Bytes;
    use http_body_util::{BodyExt, Empty as EmptyBody, Full};
    use tower::ServiceExt;
    use tracing_subscriber::Registry;
    use tracing_subscriber::layer::SubscriberExt;
    use uuid::Uuid;

    use super::*;
    use crate::request_id::request_id;
    use crate::{LogBuffer, LogRecord};

    /// A body that plays a script of frames and errors.
    struct Script(VecDeque<Result<Frame<Bytes>, &'static str>>);

    impl Body for Script {
        type Data = Bytes;
        type Error = &'static str;

        fn poll_frame(
            mut self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
        ) -> Poll<Option<Result<Frame<Bytes>, &'static str>>> {
            Poll::Ready(self.0.pop_front())
        }
    }

    fn script(frames: impl IntoIterator<Item = Result<Frame<Bytes>, &'static str>>) -> Script {
        Script(frames.into_iter().collect())
    }

    fn trailers(pairs: &[(&'static str, &'static str)]) -> Frame<Bytes> {
        let mut map = HeaderMap::new();
        for &(name, value) in pairs {
            map.insert(name, HeaderValue::from_static(value));
        }
        Frame::trailers(map)
    }

    fn capture() -> (LogBuffer, tracing::subscriber::DefaultGuard) {
        crate::test_support::keep_interest_open();
        let buffer = LogBuffer::new(64);
        let guard = tracing::subscriber::set_default(Registry::default().with(buffer.layer()));
        (buffer, guard)
    }

    fn access(buffer: &LogBuffer) -> Vec<LogRecord> {
        buffer
            .snapshot()
            .into_iter()
            .filter(|record| record.target == ACCESS_LOG_TARGET)
            .collect()
    }

    fn only(buffer: &LogBuffer) -> LogRecord {
        let mut records = access(buffer);
        assert_eq!(records.len(), 1, "{records:?}");
        records.remove(0)
    }

    fn get(path: &str) -> Request<()> {
        Request::get(path).body(()).expect("valid request")
    }

    fn with_type(path: &str, content_type: &'static str) -> Request<()> {
        Request::post(path)
            .header(CONTENT_TYPE, content_type)
            .body(())
            .expect("valid request")
    }

    async fn answer<B>(layer: &AccessLogLayer, request: Request<()>, response: Response<B>)
    where
        B: Body + Send + 'static,
        B::Data: Send,
        B::Error: std::fmt::Debug,
    {
        let slot = std::sync::Mutex::new(Some(response));
        let service = tower::service_fn(move |_request: Request<()>| {
            let response = slot.lock().expect("not poisoned").take().expect("one call");
            async move { Ok::<_, Infallible>(response) }
        });
        let response = layer
            .layer(service)
            .oneshot(request)
            .await
            .expect("infallible");
        let _ = response.into_body().collect().await;
    }

    fn ok_text() -> Response<Full<Bytes>> {
        Response::new(Full::new(Bytes::from_static(b"ok")))
    }

    #[test]
    fn protocols_are_told_apart_by_content_type() {
        let of = |value: Option<&'static str>| {
            let mut headers = HeaderMap::new();
            if let Some(value) = value {
                headers.insert(CONTENT_TYPE, HeaderValue::from_static(value));
            }
            Protocol::of(&headers)
        };
        assert_eq!(of(None), Protocol::Http);
        assert_eq!(of(Some("application/json")), Protocol::Http);
        assert_eq!(of(Some("application/grpc")), Protocol::Grpc);
        assert_eq!(of(Some("application/grpc+proto")), Protocol::Grpc);
        assert_eq!(of(Some("Application/GRPC-Web+proto")), Protocol::GrpcWeb);
        assert_eq!(of(Some("application/grpc-web-text")), Protocol::GrpcWebText);
        assert_eq!(Protocol::GrpcWebText.as_str(), "grpc-web");
        assert_eq!(Protocol::Grpc.as_str(), "grpc");
        assert_eq!(Protocol::Http.as_str(), "http");
        assert!(!Protocol::Http.is_grpc());
    }

    #[test]
    fn quiet_paths_match_exactly_or_by_prefix() {
        let quiet: Vec<String> = vec!["/readyz".into(), "/grpc.health.v1.Health/".into()];
        assert!(is_quiet(&quiet, "/readyz"));
        assert!(!is_quiet(&quiet, "/readyz/more"));
        assert!(!is_quiet(&quiet, "/ready"));
        assert!(is_quiet(&quiet, "/grpc.health.v1.Health/Check"));
        assert!(!is_quiet(&quiet, "/grpc.health.v1.Health"));
        assert!(!is_quiet(&[], "/"));
    }

    /// One gRPC-Web frame.
    fn frame(flag: u8, payload: &[u8]) -> Vec<u8> {
        let mut frame = vec![flag];
        frame.extend_from_slice(&u32::try_from(payload.len()).unwrap().to_be_bytes());
        frame.extend_from_slice(payload);
        frame
    }

    /// Standard base64 with padding, as `grpc-web-text` encodes each chunk.
    fn base64(bytes: &[u8]) -> Vec<u8> {
        const ALPHABET: &[u8; 64] =
            b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut out = Vec::new();
        for group in bytes.chunks(3) {
            let mut padded = [0_u8; 3];
            padded[..group.len()].copy_from_slice(group);
            let n = u32::from_be_bytes([0, padded[0], padded[1], padded[2]]);
            for (index, shift) in [18, 12, 6, 0].into_iter().enumerate() {
                if index <= group.len() {
                    out.push(ALPHABET[usize::try_from((n >> shift) & 63).unwrap()]);
                } else {
                    out.push(b'=');
                }
            }
        }
        out
    }

    /// Feed `chunks` to a fresh scanner; the first status found.
    fn scan(text: bool, chunks: &[&[u8]]) -> Option<i32> {
        let mut scanner = TrailerScanner::new(text);
        chunks.iter().find_map(|chunk| scanner.feed(chunk))
    }

    const BLOCK: &[u8] = b"grpc-message:boom\r\nGrpc-Status: 13\r\n";

    #[test]
    fn binary_trailer_frames_are_found_across_chunks() {
        let mut body = frame(0, b"message one");
        body.extend(frame(0, b""));
        body.extend(frame(0x80, BLOCK));
        assert_eq!(scan(false, &[body.as_slice()]), Some(13));
        let pieces: Vec<&[u8]> = body.chunks(1).collect();
        assert_eq!(scan(false, &pieces), Some(13), "byte by byte");
        let pieces: Vec<&[u8]> = body.chunks(7).collect();
        assert_eq!(scan(false, &pieces), Some(13));

        assert_eq!(
            scan(false, &[frame(0, BLOCK).as_slice()]),
            None,
            "a message"
        );
        assert_eq!(scan(false, &[frame(0x80, b"x:1\r\n").as_slice()]), None);
        assert_eq!(scan(false, &[frame(0x80, b"").as_slice()]), None, "empty");
        assert_eq!(scan(false, &[[0x80_u8, 0, 0].as_slice()]), None, "short");
        assert_eq!(scan(false, &[b"".as_slice()]), None);
        let mut cut = frame(0x80, BLOCK);
        cut.truncate(cut.len() - 4);
        assert_eq!(scan(false, &[cut.as_slice()]), None, "incomplete");

        let mut huge = frame(0x80, &vec![b'x'; MAX_TRAILER_BYTES + 1]);
        huge.extend(frame(0x80, BLOCK));
        let mut scanner = TrailerScanner::new(false);
        assert_eq!(scanner.feed(&huge), None, "too large to read");
        assert!(scanner.trailer.capacity() <= MAX_TRAILER_BYTES);
        assert_eq!(scanner.feed(&frame(0x80, BLOCK)), None, "done");
    }

    #[test]
    fn text_trailer_frames_are_decoded_across_chunks() {
        let message = frame(0, b"a message of some length");
        let trailer = frame(0x80, BLOCK);
        // Each chunk is encoded on its own, so padding may sit mid-body.
        let mut body = base64(&message);
        body.extend(base64(&trailer));
        assert!(body[..body.len() / 2].contains(&b'='), "padding mid-body");
        assert_eq!(scan(true, &[body.as_slice()]), Some(13));
        let pieces: Vec<&[u8]> = body.chunks(1).collect();
        assert_eq!(scan(true, &pieces), Some(13), "char by char");
        let mut whole = message.clone();
        whole.extend(&trailer);
        assert_eq!(scan(true, &[base64(&whole).as_slice()]), Some(13));
        for len in 1..=3 {
            let block = vec![b'x'; len];
            assert_eq!(scan(true, &[base64(&frame(0, &block)).as_slice()]), None);
        }

        for bad in [
            &b"!AAA"[..],
            b"A!AA",
            b"AA!A",
            b"AAA!",
            b"AA=A",
            b"====",
            b"A===",
        ] {
            let mut scanner = TrailerScanner::new(true);
            assert_eq!(scanner.feed(bad), None, "{bad:?}");
            assert_eq!(scanner.state, ScanState::Done, "{bad:?}");
            assert_eq!(scanner.feed(&base64(&trailer)), None, "gave up");
        }
        assert_eq!(decode_base64_group(*b"TWE="), Some(([b'M', b'a', 0], 2)));
        assert_eq!(decode_base64_group(*b"TQ=="), Some(([b'M', 0, 0], 1)));
        assert_eq!(decode_base64_group(*b"/+9z"), Some(([0xFF, 0xEF, 0x73], 3)));
    }

    #[tokio::test]
    async fn one_event_per_request_with_every_field() {
        let (buffer, _guard) = capture();
        let layer = AccessLogLayer::new();
        answer(&layer, get("/orders/7?token=s3cret"), ok_text()).await;
        let record = only(&buffer);
        assert_eq!(record.level, Level::INFO);
        assert_eq!(record.message, "request completed");
        assert_eq!(record.fields["method"], "GET");
        assert_eq!(record.fields["path"], "/orders/7");
        assert_eq!(record.fields["route"], "");
        assert_eq!(record.fields["protocol"], "http");
        assert_eq!(record.fields["status"], "200");
        assert_eq!(record.fields["aborted"], "false");
        assert!(record.fields["latency_ms"].parse::<u64>().is_ok());
        assert!(!record.fields.contains_key("grpc_status"));
        let id = &record.fields["request_id"];
        assert!(Uuid::parse_str(id).is_ok_and(|id| id.get_version_num() == 7));
        for record in buffer.snapshot() {
            assert!(!format!("{record:?}").contains("s3cret"));
        }
    }

    #[tokio::test]
    async fn the_route_template_comes_from_the_response() {
        let (buffer, _guard) = capture();
        let mut response = ok_text();
        response
            .extensions_mut()
            .insert(RouteTemplate("/orders/{id}".into()));
        answer(&AccessLogLayer::default(), get("/orders/7"), response).await;
        assert_eq!(only(&buffer).fields["route"], "/orders/{id}");
    }

    #[tokio::test]
    async fn long_paths_are_truncated() {
        let (buffer, _guard) = capture();
        let path = format!("/{}", "a".repeat(400));
        answer(&AccessLogLayer::new(), get(&path), ok_text()).await;
        let logged = &only(&buffer).fields["path"];
        assert!(logged.starts_with("/aaa"));
        assert!(logged.contains("…(+"), "{logged}");
    }

    #[tokio::test]
    async fn quiet_paths_log_at_debug_and_failures_at_warn() {
        let (buffer, _guard) = capture();
        let layer = AccessLogLayer::new()
            .quiet(["/readyz"])
            .quiet(["/internal/"]);
        let failing = || {
            let mut response = ok_text();
            *response.status_mut() = http::StatusCode::SERVICE_UNAVAILABLE;
            response
        };
        answer(&layer, get("/readyz"), failing()).await;
        answer(&layer, get("/internal/x"), ok_text()).await;
        answer(&layer, get("/orders"), failing()).await;
        let levels: Vec<Level> = access(&buffer).iter().map(|r| r.level).collect();
        assert_eq!(levels, [Level::DEBUG, Level::DEBUG, Level::WARN]);
    }

    #[tokio::test]
    async fn grpc_status_comes_from_trailers() {
        let (buffer, _guard) = capture();
        let layer = AccessLogLayer::new();
        let body = script([
            Ok(Frame::data(Bytes::from_static(b"\0\0\0\0\0"))),
            Ok(trailers(&[("grpc-status", "0")])),
        ]);
        answer(
            &layer,
            with_type("/pkg.Svc/Get", "application/grpc"),
            Response::new(body),
        )
        .await;
        let body = script([Ok(trailers(&[("grpc-status", "13")]))]);
        answer(
            &layer,
            with_type("/pkg.Svc/Get", "application/grpc"),
            Response::new(body),
        )
        .await;
        let records = access(&buffer);
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].fields["grpc_status"], "0");
        assert_eq!(records[0].fields["protocol"], "grpc");
        assert_eq!(records[0].fields["route"], "/pkg.Svc/Get");
        assert_eq!(records[0].level, Level::INFO);
        assert_eq!(records[1].fields["grpc_status"], "13");
        assert_eq!(records[1].level, Level::WARN);
    }

    #[tokio::test]
    async fn grpc_status_comes_from_a_trailers_only_answer() {
        let (buffer, _guard) = capture();
        let response = Response::builder()
            .header("grpc-status", "5")
            .body(EmptyBody::<Bytes>::new())
            .expect("valid response");
        answer(
            &AccessLogLayer::new(),
            with_type("/pkg.Svc/Get", "application/grpc+proto"),
            response,
        )
        .await;
        let record = only(&buffer);
        assert_eq!(record.fields["grpc_status"], "5");
        assert_eq!(record.fields["aborted"], "false");
        assert_eq!(record.level, Level::INFO);
    }

    #[tokio::test]
    async fn grpc_web_status_comes_from_the_trailer_frame() {
        let (buffer, _guard) = capture();
        let trailer = frame(0x80, b"grpc-status:15\r\n");
        let typed = |content_type: &'static str, body: Script| {
            Response::builder()
                .header(CONTENT_TYPE, content_type)
                .body(body)
                .expect("valid response")
        };
        let layer = AccessLogLayer::new();
        let body = script([
            Ok(Frame::data(Bytes::from_static(b"\0\0\0\0\0"))),
            Ok(Frame::data(Bytes::from(trailer.clone()))),
        ]);
        answer(
            &layer,
            with_type("/api/pkg.Svc/Get", "application/grpc-web+proto"),
            typed("application/grpc-web+proto", body),
        )
        .await;
        // The response's content type decides the encoding.
        let mut text = base64(b"\0\0\0\0\0");
        text.extend(base64(&trailer));
        let (head, tail) = text.split_at(5);
        let body = script([
            Ok(Frame::data(Bytes::copy_from_slice(head))),
            Ok(Frame::data(Bytes::copy_from_slice(tail))),
        ]);
        answer(
            &layer,
            with_type("/api/pkg.Svc/Get", "application/grpc-web+proto"),
            typed("application/grpc-web-text+proto", body),
        )
        .await;
        let body = script([Ok(Frame::data(Bytes::from(trailer.clone())))]);
        answer(
            &layer,
            with_type("/api/pkg.Svc/Get", "application/grpc-web-text"),
            typed("application/grpc-web+proto", body),
        )
        .await;
        let body = script([Ok(Frame::data(Bytes::from(trailer.clone())))]);
        answer(
            &layer,
            with_type("/api/pkg.Svc/Get", "application/grpc-web+proto"),
            Response::new(body),
        )
        .await;
        let body = script([Ok(Frame::data(Bytes::from(trailer)))]);
        answer(
            &layer,
            get("/download"),
            typed("application/grpc-web+proto", body),
        )
        .await;
        let records = access(&buffer);
        assert_eq!(records.len(), 5);
        assert_eq!(records[0].fields["grpc_status"], "15");
        assert_eq!(records[0].fields["protocol"], "grpc-web");
        assert_eq!(records[0].level, Level::WARN);
        assert_eq!(records[1].fields["grpc_status"], "15", "base64 text");
        assert_eq!(records[1].level, Level::WARN);
        assert_eq!(records[2].fields["grpc_status"], "15", "binary answer");
        assert!(
            !records[3].fields.contains_key("grpc_status"),
            "no gRPC-Web content type on the response"
        );
        assert!(!records[4].fields.contains_key("grpc_status"), "not gRPC");
        assert_eq!(records[4].level, Level::INFO);
    }

    /// A body whose data frames are two buffers chained together.
    struct Chained(Option<Frame<bytes::buf::Chain<Bytes, Bytes>>>);

    impl Body for Chained {
        type Data = bytes::buf::Chain<Bytes, Bytes>;
        type Error = Infallible;

        fn poll_frame(
            mut self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
        ) -> Poll<Option<Result<Frame<Self::Data>, Infallible>>> {
            Poll::Ready(self.0.take().map(Ok))
        }
    }

    #[tokio::test]
    async fn a_split_buffer_is_read_up_to_its_first_piece() {
        let (buffer, _guard) = capture();
        let trailer = frame(0x80, b"grpc-status:13\r\n");
        for (first, second) in [
            (Bytes::from(trailer.clone()), Bytes::from_static(b"\0")),
            (Bytes::from_static(b"\0\0\0"), Bytes::from(trailer)),
        ] {
            let response = Response::builder()
                .header(CONTENT_TYPE, "application/grpc-web")
                .body(Chained(Some(Frame::data(first.chain(second)))))
                .expect("valid response");
            answer(
                &AccessLogLayer::new(),
                with_type("/pkg.Svc/Get", "application/grpc-web"),
                response,
            )
            .await;
        }
        let records = access(&buffer);
        assert_eq!(records[0].fields["grpc_status"], "13");
        assert!(!records[1].fields.contains_key("grpc_status"));
    }

    #[tokio::test]
    async fn responses_without_a_body_complete_with_their_headers() {
        let (buffer, _guard) = capture();
        let service =
            tower::service_fn(|_request: Request<()>| async { Ok::<_, Infallible>(ok_text()) });
        let head = Request::head("/livez").body(()).expect("valid request");
        let response = AccessLogLayer::new()
            .layer(service)
            .oneshot(head)
            .await
            .expect("infallible");
        let record = only(&buffer);
        assert_eq!(record.fields["aborted"], "false");
        assert_eq!(record.fields["method"], "HEAD");
        drop(response);
        assert_eq!(access(&buffer).len(), 1, "logged once");

        for status in [http::StatusCode::NO_CONTENT, http::StatusCode::NOT_MODIFIED] {
            buffer.clear();
            let service = tower::service_fn(move |_request: Request<()>| async move {
                let mut response = ok_text();
                *response.status_mut() = status;
                Ok::<_, Infallible>(response)
            });
            let response = AccessLogLayer::new()
                .layer(service)
                .oneshot(get("/cached"))
                .await
                .expect("infallible");
            drop(response);
            let record = only(&buffer);
            assert_eq!(record.fields["aborted"], "false", "{status}");
            assert_eq!(record.fields["status"], status.as_str());
        }
    }

    #[tokio::test]
    async fn a_dropped_body_is_aborted() {
        let (buffer, _guard) = capture();
        let service =
            tower::service_fn(|_request: Request<()>| async { Ok::<_, Infallible>(ok_text()) });
        let response = AccessLogLayer::new()
            .layer(service)
            .oneshot(get("/download"))
            .await
            .expect("infallible");
        assert!(access(&buffer).is_empty(), "not ended yet");
        drop(response);
        let record = only(&buffer);
        assert_eq!(record.fields["aborted"], "true");
        assert_eq!(record.fields["status"], "200");
    }

    #[tokio::test]
    async fn a_failing_body_is_aborted() {
        let (buffer, _guard) = capture();
        let body = script([Ok(Frame::data(Bytes::from_static(b"x"))), Err("reset")]);
        answer(&AccessLogLayer::new(), get("/stream"), Response::new(body)).await;
        assert_eq!(only(&buffer).fields["aborted"], "true");
    }

    #[tokio::test]
    async fn a_body_ending_without_a_last_frame_completes_at_the_end() {
        let (buffer, _guard) = capture();
        let body = script([Ok(Frame::data(Bytes::from_static(b"x")))]);
        answer(&AccessLogLayer::new(), get("/stream"), Response::new(body)).await;
        assert_eq!(only(&buffer).fields["aborted"], "false");
    }

    #[tokio::test]
    async fn an_inner_failure_logs_a_warning() {
        let (buffer, _guard) = capture();
        let service = tower::service_fn(|_request: Request<()>| async {
            Err::<Response<Full<Bytes>>, _>("boom")
        });
        let result = AccessLogLayer::new()
            .layer(service)
            .oneshot(get("/orders"))
            .await;
        assert!(result.is_err());
        let record = only(&buffer);
        assert_eq!(record.level, Level::WARN);
        assert_eq!(record.fields["status"], "500");
        assert_eq!(record.fields["aborted"], "true");
    }

    #[tokio::test]
    async fn a_dropped_request_is_aborted_without_a_status() {
        let (buffer, _guard) = capture();
        let service = tower::service_fn(|_request: Request<()>| {
            std::future::pending::<Result<Response<Full<Bytes>>, Infallible>>()
        });
        let mut service = AccessLogLayer::new().layer(service);
        let mut future = Box::pin(service.call(get("/slow")));
        std::future::poll_fn(|cx| {
            assert!(future.as_mut().poll(cx).is_pending());
            Poll::Ready(())
        })
        .await;
        drop(future);
        let record = only(&buffer);
        assert_eq!(record.fields["aborted"], "true");
        assert!(!record.fields.contains_key("status"));
        assert_eq!(record.level, Level::INFO);
    }

    #[tokio::test]
    async fn ids_reach_the_handler_and_the_response() {
        let service = tower::service_fn(|request: Request<()>| async move {
            let seen = request_id(&request).unwrap_or("none").to_owned();
            Ok::<_, Infallible>(Response::new(Full::new(Bytes::from(seen))))
        });
        let mut request = get("/");
        request
            .headers_mut()
            .insert(REQUEST_ID_HEADER, HeaderValue::from_static("has space"));
        let response = AccessLogLayer::new()
            .layer(service)
            .oneshot(request)
            .await
            .expect("infallible");
        let header = response.headers()[&REQUEST_ID_HEADER]
            .to_str()
            .expect("ascii")
            .to_owned();
        assert_ne!(header, "has space");
        let body = response
            .into_body()
            .collect()
            .await
            .expect("full")
            .to_bytes();
        assert_eq!(body, header.as_bytes());
    }

    #[tokio::test]
    async fn an_id_the_handler_set_is_kept() {
        let service = tower::service_fn(|_request: Request<()>| async {
            let mut response = ok_text();
            response
                .headers_mut()
                .insert(REQUEST_ID_HEADER, HeaderValue::from_static("from-handler"));
            Ok::<_, Infallible>(response)
        });
        let response = AccessLogLayer::new()
            .layer(service)
            .oneshot(get("/"))
            .await
            .expect("infallible");
        assert_eq!(response.headers()[&REQUEST_ID_HEADER], "from-handler");
        assert_eq!(response.body().size_hint().exact(), Some(2));
        assert!(!response.body().is_end_stream());
    }

    #[tokio::test]
    async fn without_events_ids_and_the_span_stay() {
        let (buffer, _guard) = capture();
        let service = tower::service_fn(|_request: Request<()>| async {
            tracing::info!("handled");
            Ok::<_, Infallible>(ok_text())
        });
        let mut request = get("/");
        request
            .headers_mut()
            .insert(REQUEST_ID_HEADER, HeaderValue::from_static("keep-me"));
        let response = AccessLogLayer::new()
            .events(false)
            .layer(service)
            .oneshot(request)
            .await
            .expect("infallible");
        assert_eq!(response.headers()[&REQUEST_ID_HEADER], "keep-me");
        let _ = response.into_body().collect().await;
        assert!(access(&buffer).is_empty());
        let records = buffer.snapshot();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].message, "handled");
        assert_eq!(records[0].fields["request_id"], "keep-me");
        assert_eq!(records[0].fields["path"], "/");
    }

    #[test]
    fn debug_output_names_the_layer() {
        let layer = AccessLogLayer::new().quiet(["/livez"]).events(false);
        let debug = format!("{layer:?}");
        assert!(debug.contains("AccessLogLayer"), "{debug}");
        assert!(debug.contains("/livez"), "{debug}");
        assert!(format!("{:?}", layer.layer(())).contains("AccessLogService"));
    }
}
