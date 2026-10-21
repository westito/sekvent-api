//! Server-streaming methods over native gRPC on loopback: items and their
//! end, typed open and item errors, trailers-only open errors, drops,
//! deadlines and lifetimes, open retries and the circuit breaker, the item
//! size cap, a transport that fails mid-stream, authentication at the open,
//! the hop limit, shutdown on either side, the messages a served stream
//! counts, and a plain tonic client.

mod support;

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use bytes::{BufMut, Bytes, BytesMut};
use http::header::{AUTHORIZATION, CONTENT_TYPE};
use http::uri::PathAndQuery;
use http_body_util::{BodyExt as _, Full};
use prost::Message;
use sekvent_component::{
    App, AppError, Binding, CallContext, CallStream, ComponentState, END_USER_REJECTED_MESSAGE,
    EndUser, ErrorCode, reasons,
};
use sekvent_config::MapSource;
use sekvent_runtime::UnitExit;
use sekvent_telemetry::access_log::MessageCount;
use support::fakes::{ScriptedTicker, TickerEvents, build_with, tick};
use support::grpc::{Service, TOKEN, guarded, serve};
use support::ticker::{LatestRequest, Tick, Ticker, TickerError, TickerHandle, TicksRequest};
use tokio::time::Instant;
use tonic_types::StatusExt as _;
use tower::ServiceExt as _;

const TICKS: &str = "/shop.ticker.v1.Ticker/Ticks";

/// Nothing listens on port 1 of the loopback interface.
const UNREACHABLE: &str = "http://127.0.0.1:1";

const BREAKER: [(&str, &str); 4] = [
    ("SEKVENT_COMPONENT_TICKER_BREAKER_WINDOW", "4"),
    ("SEKVENT_COMPONENT_TICKER_BREAKER_MIN_CALLS", "4"),
    ("SEKVENT_COMPONENT_TICKER_BREAKER_WAIT_IN_OPEN", "1h"),
    ("SEKVENT_COMPONENT_TICKER_RETRY_MAX_ATTEMPTS", "1"),
];

fn series(name: &str) -> TicksRequest {
    TicksRequest {
        series: name.to_owned(),
        from: 0,
    }
}

fn other(error: TickerError) -> AppError {
    match error {
        TickerError::Other(error) => error,
        unexpected => panic!("expected Other, got {unexpected:?}"),
    }
}

fn assert_tagged(error: &AppError) {
    assert_eq!(error.metadata()["component"], "ticker", "{error:?}");
    assert_eq!(error.metadata()["method"], "ticks", "{error:?}");
}

/// The `Other` error an open of series `name` fails with.
async fn open_error(ticker: &TickerHandle, cx: &CallContext, name: &str) -> AppError {
    other(ticker.ticks(cx, series(name)).await.unwrap_err())
}

/// The `Other` error that ends `stream`, which must be its next item.
async fn end_error(stream: &mut CallStream<Tick, TickerError>) -> AppError {
    other(
        stream
            .next()
            .await
            .expect("a terminal item")
            .expect_err("an error"),
    )
}

/// The keys of a service exposing the ticker to link `shop`, plus `extra`.
fn service_keys(extra: &[(&str, &str)]) -> Vec<(String, String)> {
    [
        ("SEKVENT_COMPONENT_TICKER_SERVE", "grpc"),
        ("SEKVENT_LINK_INBOUND_SHOP", TOKEN),
    ]
    .iter()
    .chain(extra)
    .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
    .collect()
}

/// A started service of the scripted ticker, a control handle on its feeds,
/// and its events.
async fn ticker_service(extra: &[(&str, &str)]) -> (Service, ScriptedTicker, TickerEvents) {
    let (scripted, events) = ScriptedTicker::new();
    let control = scripted.share();
    let service = serve(&service_keys(extra), move |builder| {
        TickerHandle::install_with_lifecycle(builder, move |_| Ok(scripted))
    })
    .await;
    (service, control, events)
}

/// A started caller App binding the ticker to `endpoint` with the shop token
/// and 1 ms backoff, plus `extra`; its handle.
async fn ticker_caller(endpoint: &str, extra: &[(&str, &str)]) -> (App, TickerHandle) {
    let config: MapSource = [
        ("SEKVENT_COMPONENT_TICKER_BINDING", "grpc"),
        ("SEKVENT_COMPONENT_TICKER_ENDPOINT", endpoint),
        ("SEKVENT_LINK_OUTBOUND_TICKER", TOKEN),
        ("SEKVENT_COMPONENT_TICKER_RETRY_INITIAL_BACKOFF", "1ms"),
        ("SEKVENT_COMPONENT_TICKER_RETRY_MAX_BACKOFF", "1ms"),
        ("SEKVENT_COMPONENT_TICKER_RETRY_JITTER", "none"),
    ]
    .iter()
    .chain(extra)
    .copied()
    .collect();
    let app = build_with(&config, |builder| {
        TickerHandle::install(builder, |_| -> Result<ScriptedTicker, AppError> {
            panic!("a grpc-bound component runs no factory")
        })
    })
    .unwrap();
    app.start().await.unwrap();
    let ticker = app.handle::<TickerHandle>().unwrap();
    assert_eq!(ticker.binding(), Binding::Grpc);
    (app, ticker)
}

/// One message as a gRPC frame.
fn grpc_frame(message: &impl Message) -> Bytes {
    let encoded = message.encode_to_vec();
    let mut frame = BytesMut::with_capacity(5 + encoded.len());
    frame.put_u8(0);
    frame.put_u32(u32::try_from(encoded.len()).unwrap());
    frame.put_slice(&encoded);
    frame.freeze()
}

/// A raw gRPC answer: the response headers, every message, the trailers.
struct Raw {
    headers: http::HeaderMap,
    messages: Vec<Bytes>,
    trailers: Option<http::HeaderMap>,
}

/// Open `request` on `addr` over a plain HTTP/2 client and read the whole
/// answer.
async fn raw_open(addr: SocketAddr, authorization: Option<&str>, request: &TicksRequest) -> Raw {
    let client = hyper_util::client::legacy::Client::builder(hyper_util::rt::TokioExecutor::new())
        .http2_only(true)
        .build_http::<Full<Bytes>>();
    let mut builder = http::Request::post(format!("http://{addr}{TICKS}"))
        .header(CONTENT_TYPE, "application/grpc")
        .header("te", "trailers");
    if let Some(value) = authorization {
        builder = builder.header(AUTHORIZATION, value);
    }
    let response = client
        .request(builder.body(Full::new(grpc_frame(request))).unwrap())
        .await
        .unwrap();
    let (parts, mut body) = response.into_parts();
    let mut data = BytesMut::new();
    let mut trailers = None;
    while let Some(frame) = body.frame().await {
        match frame.unwrap().into_data() {
            Ok(chunk) => data.extend_from_slice(&chunk),
            Err(frame) => trailers = frame.into_trailers().ok(),
        }
    }
    let mut messages = Vec::new();
    while data.len() >= 5 {
        let length =
            usize::try_from(u32::from_be_bytes([data[1], data[2], data[3], data[4]])).unwrap();
        let mut frame = data.split_to(5 + length);
        messages.push(frame.split_off(5).freeze());
    }
    Raw {
        headers: parts.headers,
        messages,
        trailers,
    }
}

#[tokio::test]
async fn items_and_typed_errors_cross_the_wire() {
    guarded(async {
        let (service, control, mut events) = ticker_service(&[]).await;
        let (app, ticker) = ticker_caller(&service.endpoint(), &[]).await;
        let cx = CallContext::new();

        let feed = control.feed();
        let mut stream = ticker.ticks(&cx, series("feed")).await.unwrap();
        let opened = events.opened.recv().await.unwrap();
        assert_eq!(
            opened.caller().map(|caller| caller.name.as_str()),
            Some("shop")
        );
        feed.send(Ok(tick("feed", 1))).await.unwrap();
        assert_eq!(stream.next().await.unwrap().unwrap(), tick("feed", 1));
        feed.send(Ok(tick("feed", 2))).await.unwrap();
        assert_eq!(stream.next().await.unwrap().unwrap(), tick("feed", 2));
        drop(feed);
        assert!(stream.next().await.is_none());

        let unknown = ticker.ticks(&cx, series("unknown")).await.unwrap_err();
        assert!(
            matches!(&unknown, TickerError::UnknownSeries { series } if series == "unknown"),
            "{unknown:?}"
        );
        let paused = open_error(&ticker, &cx, "other").await;
        assert_eq!(paused.code(), ErrorCode::FailedPrecondition);
        assert_eq!(paused.reason(), Some("PAUSED"));
        assert!(!paused.metadata().contains_key("component"));

        let feed = control.feed();
        let mut stream = ticker.ticks(&cx, series("gappy")).await.unwrap();
        feed.send(Ok(tick("gappy", 1))).await.unwrap();
        feed.send(Err(TickerError::Gap { missing: 3 }))
            .await
            .unwrap();
        assert_eq!(stream.next().await.unwrap().unwrap(), tick("gappy", 1));
        let gap = stream.next().await.unwrap().unwrap_err();
        assert!(matches!(gap, TickerError::Gap { missing: 3 }), "{gap:?}");
        assert!(stream.next().await.is_none());

        // The unary method beside it.
        let latest = ticker
            .latest(&cx, LatestRequest { series: "s".into() })
            .await
            .unwrap();
        assert_eq!(latest, tick("s", 7));
        app.stop(Duration::ZERO).await.unwrap();
        service.stop().await;
    })
    .await;
}

#[tokio::test]
async fn open_errors_are_trailers_only_and_streams_end_with_trailers() {
    guarded(async {
        let (service, control, mut events) = ticker_service(&[]).await;
        let bearer = format!("Bearer {TOKEN}");

        let refused = raw_open(service.addr, Some(&bearer), &series("unknown")).await;
        assert_eq!(refused.headers["grpc-status"], "5");
        assert_eq!(refused.messages, [] as [Bytes; 0]);
        assert!(refused.trailers.is_none());

        let feed = control.feed();
        feed.send(Ok(tick("feed", 1))).await.unwrap();
        feed.send(Ok(tick("feed", 2))).await.unwrap();
        drop(feed);
        let answer = raw_open(service.addr, Some(&bearer), &series("feed")).await;
        assert!(!answer.headers.contains_key("grpc-status"));
        let ticks: Vec<Tick> = answer
            .messages
            .into_iter()
            .map(|message| Tick::decode(message).unwrap())
            .collect();
        assert_eq!(ticks, [tick("feed", 1), tick("feed", 2)]);
        assert_eq!(answer.trailers.unwrap()["grpc-status"], "0");

        // A wrong token is refused before any handler runs.
        let _ = events.opened.recv().await.unwrap();
        let _ = events.opened.recv().await.unwrap();
        let wrong = raw_open(service.addr, Some("Bearer forged"), &series("feed")).await;
        assert_eq!(wrong.headers["grpc-status"], "16");
        assert!(events.opened.try_recv().is_err());
        service.stop().await;
    })
    .await;
}

#[tokio::test]
async fn dropping_the_stream_drops_the_producer_on_the_server() {
    guarded(async {
        let (service, control, mut events) = ticker_service(&[]).await;
        let (app, ticker) = ticker_caller(&service.endpoint(), &[]).await;
        let _feed = control.feed();
        let stream = ticker
            .ticks(&CallContext::new(), series("feed"))
            .await
            .unwrap();
        let opened = events.opened.recv().await.unwrap();
        drop(stream);
        events.dropped.recv().await.unwrap();
        assert!(opened.cancel_token().is_cancelled());
        app.stop(Duration::ZERO).await.unwrap();
        service.stop().await;
    })
    .await;
}

#[tokio::test]
async fn the_callers_deadline_bounds_the_whole_stream() {
    guarded(async {
        let (service, _control, mut events) = ticker_service(&[]).await;
        let (app, ticker) = ticker_caller(&service.endpoint(), &[]).await;
        let started = Instant::now();
        let cx =
            CallContext::new().with_deadline((started + Duration::from_millis(300)).into_std());
        let mut stream = ticker.ticks(&cx, series("feed")).await.unwrap();
        let error = end_error(&mut stream).await;
        assert!(
            started.elapsed() >= Duration::from_millis(299),
            "{:?}",
            started.elapsed()
        );
        assert_eq!(error.code(), ErrorCode::DeadlineExceeded, "{error:?}");
        assert_eq!(error.message(), "the call deadline was exceeded");
        assert_tagged(&error);
        events.dropped.recv().await.unwrap();
        app.stop(Duration::ZERO).await.unwrap();
        service.stop().await;
    })
    .await;
}

#[tokio::test]
async fn the_servers_lifetime_ends_the_stream() {
    guarded(async {
        let (service, _control, mut events) = ticker_service(&[(
            "SEKVENT_COMPONENT_TICKER_TICKS_STREAM_MAX_LIFETIME",
            "300ms",
        )])
        .await;
        let (app, ticker) = ticker_caller(&service.endpoint(), &[]).await;
        let started = Instant::now();
        let mut stream = ticker
            .ticks(&CallContext::new(), series("feed"))
            .await
            .unwrap();
        let error = end_error(&mut stream).await;
        assert!(started.elapsed() >= Duration::from_millis(270));
        assert_eq!(error.code(), ErrorCode::Unavailable, "{error:?}");
        assert_eq!(error.reason(), Some(reasons::STREAM_EXPIRED));
        assert_eq!(error.metadata()["max_lifetime_ms"], "300");
        assert_tagged(&error);
        assert!(stream.next().await.is_none());
        events.dropped.recv().await.unwrap();
        app.stop(Duration::ZERO).await.unwrap();
        service.stop().await;
    })
    .await;
}

/// Counts its opens; fails the first `failures` opens of series `flaky`
/// with `UNAVAILABLE`. A `midfail` stream yields one tick and then ends with
/// `UNAVAILABLE`; any other stream yields one tick.
#[derive(Clone, Default)]
struct Flaky(Arc<FlakyState>);

#[derive(Default)]
struct FlakyState {
    failures: AtomicU32,
    opens: AtomicU32,
}

impl Flaky {
    fn failing(failures: u32) -> Self {
        let flaky = Self::default();
        flaky.0.failures.store(failures, Ordering::SeqCst);
        flaky
    }

    fn opens(&self) -> u32 {
        self.0.opens.load(Ordering::SeqCst)
    }
}

impl Ticker for Flaky {
    async fn ticks(
        &self,
        _cx: &CallContext,
        req: TicksRequest,
    ) -> Result<CallStream<Tick, TickerError>, TickerError> {
        self.0.opens.fetch_add(1, Ordering::SeqCst);
        if req.series == "flaky"
            && self
                .0
                .failures
                .try_update(Ordering::SeqCst, Ordering::SeqCst, |left| {
                    left.checked_sub(1)
                })
                .is_ok()
        {
            return Err(TickerError::Other(AppError::unavailable("busy")));
        }
        let first = Ok(tick(&req.series, 1));
        if req.series == "midfail" {
            let gone = Err(TickerError::Other(AppError::unavailable("gone")));
            return Ok(CallStream::iter([first, gone]));
        }
        Ok(CallStream::iter([first]))
    }

    async fn latest(&self, _cx: &CallContext, req: LatestRequest) -> Result<Tick, TickerError> {
        Ok(tick(&req.series, 7))
    }
}

async fn flaky_service(flaky: Flaky) -> Service {
    serve(&service_keys(&[]), move |builder| {
        TickerHandle::install(builder, move |_| Ok(flaky))
    })
    .await
}

#[tokio::test]
async fn an_idempotent_open_is_retried_after_unavailable() {
    guarded(async {
        let flaky = Flaky::failing(1);
        let service = flaky_service(flaky.clone()).await;
        let (app, ticker) = ticker_caller(&service.endpoint(), &[]).await;
        let mut stream = ticker
            .ticks(&CallContext::new(), series("flaky"))
            .await
            .unwrap();
        assert_eq!(stream.next().await.unwrap().unwrap(), tick("flaky", 1));
        assert!(stream.next().await.is_none());
        assert_eq!(flaky.opens(), 2);
        app.stop(Duration::ZERO).await.unwrap();

        // A stream that ended mid-way is never reopened.
        let (app, ticker) = ticker_caller(&service.endpoint(), &[]).await;
        let mut stream = ticker
            .ticks(&CallContext::new(), series("midfail"))
            .await
            .unwrap();
        assert_eq!(stream.next().await.unwrap().unwrap(), tick("midfail", 1));
        assert_eq!(end_error(&mut stream).await.message(), "gone");
        assert!(stream.next().await.is_none());
        assert_eq!(flaky.opens(), 3);
        app.stop(Duration::ZERO).await.unwrap();
        service.stop().await;
    })
    .await;
}

#[tokio::test]
async fn the_breaker_counts_opens_and_never_items() {
    guarded(async {
        let flaky = Flaky::failing(100);
        let service = flaky_service(flaky.clone()).await;
        let (app, ticker) = ticker_caller(&service.endpoint(), &BREAKER).await;
        let cx = CallContext::new();
        for _ in 0..6 {
            let mut stream = ticker.ticks(&cx, series("midfail")).await.unwrap();
            assert_eq!(stream.next().await.unwrap().unwrap(), tick("midfail", 1));
            let error = end_error(&mut stream).await;
            assert_eq!(error.code(), ErrorCode::Unavailable);
            assert_eq!(error.message(), "gone");
            assert!(
                !error.metadata().contains_key("component"),
                "the server's item error, untouched: {error:?}"
            );
        }
        // Six streams that failed mid-way left the breaker closed; two failed
        // opens in a window of four reach its 50 % failure rate.
        for _ in 0..2 {
            let error = open_error(&ticker, &cx, "flaky").await;
            assert_eq!(error.code(), ErrorCode::Unavailable);
            assert_eq!(error.message(), "busy");
        }
        let error = open_error(&ticker, &cx, "flaky").await;
        assert_eq!(error.reason(), Some(reasons::CIRCUIT_OPEN), "{error:?}");
        assert_tagged(&error);
        assert_eq!(flaky.opens(), 8, "the open the breaker refused never left");
        app.stop(Duration::ZERO).await.unwrap();
        service.stop().await;
    })
    .await;
}

#[tokio::test]
async fn a_pending_open_is_a_method_timeout_and_is_abandoned_on_the_server() {
    guarded(async {
        let (service, _control, mut events) = ticker_service(&[]).await;
        let (app, ticker) = ticker_caller(
            &service.endpoint(),
            &[("SEKVENT_COMPONENT_TICKER_TICKS_TIMEOUT", "200ms")],
        )
        .await;
        let started = Instant::now();
        let error = open_error(&ticker, &CallContext::new(), "hang").await;
        assert!(started.elapsed() >= Duration::from_millis(200));
        assert_eq!(error.code(), ErrorCode::DeadlineExceeded, "{error:?}");
        assert_eq!(error.reason(), Some(reasons::METHOD_TIMEOUT));
        assert_eq!(error.metadata()["timeout_ms"], "200");
        assert_tagged(&error);
        events.dropped.recv().await.unwrap();
        app.stop(Duration::ZERO).await.unwrap();
        service.stop().await;
    })
    .await;
}

#[tokio::test]
async fn an_item_above_four_mebibytes_ends_the_stream_with_out_of_range() {
    guarded(async {
        let (service, _control, _events) = ticker_service(&[]).await;
        let (app, ticker) = ticker_caller(&service.endpoint(), &[]).await;
        let mut stream = ticker
            .ticks(&CallContext::new(), series("big"))
            .await
            .unwrap();
        let error = end_error(&mut stream).await;
        assert_eq!(error.code(), ErrorCode::OutOfRange, "{error:?}");
        assert!(stream.next().await.is_none());
        app.stop(Duration::ZERO).await.unwrap();
        service.stop().await;
    })
    .await;
}

/// A TCP proxy to `target` for one connection; aborting its task cuts the
/// connection.
async fn proxy(target: SocketAddr) -> (SocketAddr, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
        .await
        .unwrap();
    let addr = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        let (mut inbound, _) = listener.accept().await.unwrap();
        let mut outbound = tokio::net::TcpStream::connect(target).await.unwrap();
        drop(tokio::io::copy_bidirectional(&mut inbound, &mut outbound).await);
    });
    (addr, task)
}

#[tokio::test]
async fn a_transport_failure_mid_stream_is_unreachable() {
    guarded(async {
        let (service, control, mut events) = ticker_service(&[]).await;
        let (addr, cut) = proxy(service.addr).await;
        let (app, ticker) = ticker_caller(&format!("http://{addr}"), &[]).await;
        let feed = control.feed();
        let mut stream = ticker
            .ticks(&CallContext::new(), series("feed"))
            .await
            .unwrap();
        feed.send(Ok(tick("feed", 1))).await.unwrap();
        assert_eq!(stream.next().await.unwrap().unwrap(), tick("feed", 1));

        cut.abort();
        let error = end_error(&mut stream).await;
        assert_eq!(error.code(), ErrorCode::Unavailable, "{error:?}");
        assert_eq!(error.reason(), Some(reasons::UNREACHABLE));
        assert_eq!(error.message(), "component ticker is unreachable");
        assert_tagged(&error);
        assert!(stream.next().await.is_none());
        events.dropped.recv().await.unwrap();
        app.stop(Duration::ZERO).await.unwrap();
        service.stop().await;
    })
    .await;
}

#[tokio::test]
async fn nothing_listening_is_unreachable_at_the_open() {
    guarded(async {
        let (app, ticker) = ticker_caller(UNREACHABLE, &[]).await;
        let error = open_error(&ticker, &CallContext::new(), "feed").await;
        assert_eq!(error.code(), ErrorCode::Unavailable);
        assert_eq!(error.reason(), Some(reasons::UNREACHABLE));
        assert_tagged(&error);
        app.stop(Duration::ZERO).await.unwrap();
    })
    .await;
}

/// Accepts `Bearer user-<n>` session tokens.
fn sessions(request: &http::request::Parts) -> Result<EndUser, AppError> {
    request
        .headers
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .filter(|token| token.starts_with("user-"))
        .map(|token| EndUser::new(token).with_tenant("t-1"))
        .ok_or_else(|| AppError::unauthenticated("unknown session token"))
}

#[tokio::test]
async fn end_users_are_authenticated_at_the_open() {
    guarded(async {
        let (scripted, mut events) = ScriptedTicker::new();
        let control = scripted.share();
        let keys = service_keys(&[("SEKVENT_COMPONENT_TICKER_SERVE_AUTH", "bearer")]);
        let service = serve(&keys, move |builder| {
            builder.end_user_authenticator(sessions)?;
            TickerHandle::install(builder, move |_| Ok(scripted))
        })
        .await;

        let missing = raw_open(service.addr, None, &series("feed")).await;
        assert_eq!(missing.headers["grpc-status"], "16");
        let status = tonic::Status::from_header_map(&missing.headers).unwrap();
        assert_eq!(status.message(), END_USER_REJECTED_MESSAGE);
        assert_eq!(missing.messages, [] as [Bytes; 0]);
        assert!(events.opened.try_recv().is_err(), "no handler ran");

        // A link token is no session.
        let (app, ticker) = ticker_caller(&service.endpoint(), &[]).await;
        let error = open_error(&ticker, &CallContext::new(), "feed").await;
        assert_eq!(error.code(), ErrorCode::Unauthenticated);
        assert_eq!(error.message(), END_USER_REJECTED_MESSAGE);
        app.stop(Duration::ZERO).await.unwrap();
        assert!(events.opened.try_recv().is_err());

        let feed = control.feed();
        feed.send(Ok(tick("feed", 1))).await.unwrap();
        drop(feed);
        let answer = raw_open(service.addr, Some("Bearer user-7"), &series("feed")).await;
        assert_eq!(answer.messages.len(), 1);
        assert_eq!(answer.trailers.unwrap()["grpc-status"], "0");
        let opened = events.opened.recv().await.unwrap();
        assert_eq!(opened.end_user().map(EndUser::subject), Some("user-7"));
        assert_eq!(opened.tenant(), Some("t-1"));
        service.stop().await;
    })
    .await;
}

#[tokio::test]
async fn the_servers_hop_limit_applies_at_the_open() {
    guarded(async {
        let (service, _control, mut events) =
            ticker_service(&[("SEKVENT_COMPONENT_MAX_HOPS", "1")]).await;
        let (app, ticker) = ticker_caller(&service.endpoint(), &[]).await;
        let error = open_error(&ticker, &CallContext::new().with_hops(1), "feed").await;
        assert_eq!(error.code(), ErrorCode::FailedPrecondition, "{error:?}");
        assert_eq!(error.reason(), Some(reasons::CALL_DEPTH_EXCEEDED));
        assert_tagged(&error);
        assert!(events.opened.try_recv().is_err());
        app.stop(Duration::ZERO).await.unwrap();
        service.stop().await;
    })
    .await;
}

#[tokio::test]
async fn stopping_the_caller_ends_its_outbound_streams_and_resets_them() {
    guarded(async {
        let (service, control, mut events) = ticker_service(&[]).await;
        let (app, ticker) = ticker_caller(&service.endpoint(), &[]).await;
        let _feed = control.feed();
        let mut stream = ticker
            .ticks(&CallContext::new(), series("feed"))
            .await
            .unwrap();
        let opened = events.opened.recv().await.unwrap();
        let consumer = tokio::spawn(async move { stream.next().await });
        app.stop(Duration::from_secs(20)).await.unwrap();
        let error = other(consumer.await.unwrap().unwrap().unwrap_err());
        assert_eq!(error.code(), ErrorCode::Unavailable);
        assert_eq!(error.reason(), Some(reasons::DRAINING));
        assert_tagged(&error);
        assert_eq!(app.state("ticker"), Some(ComponentState::Stopped));
        events.dropped.recv().await.unwrap();
        assert!(opened.cancel_token().is_cancelled());
        service.stop().await;
    })
    .await;
}

#[tokio::test]
async fn a_shutting_down_service_ends_its_streams_and_drains_without_waiting() {
    guarded(async {
        let (service, control, _events) = ticker_service(&[]).await;
        let (app, ticker) = ticker_caller(&service.endpoint(), &[]).await;
        let _feed = control.feed();
        let mut stream = ticker
            .ticks(&CallContext::new(), series("feed"))
            .await
            .unwrap();
        let Service {
            app: served,
            runtime,
            ..
        } = service;
        runtime.shutdown();
        let error = end_error(&mut stream).await;
        assert_eq!(error.code(), ErrorCode::Unavailable, "{error:?}");
        assert_eq!(error.reason(), Some(reasons::DRAINING));
        assert_tagged(&error);
        let report = runtime.wait().await.unwrap();
        assert_ne!(report.unit("grpc").unwrap().exit, UnitExit::Aborted);
        assert_eq!(served.state("ticker"), Some(ComponentState::Stopped));
        app.stop(Duration::ZERO).await.unwrap();
    })
    .await;
}

/// A gRPC request for `message` on the ticks path.
fn ticks_request(message: &TicksRequest) -> http::Request<Full<Bytes>> {
    http::Request::post(TICKS)
        .header(CONTENT_TYPE, "application/grpc")
        .body(Full::new(grpc_frame(message)))
        .unwrap()
}

#[tokio::test]
async fn a_served_stream_counts_the_messages_it_sends() {
    let (scripted, _events) = ScriptedTicker::new();
    let control = scripted.share();
    let config: MapSource = [
        ("SEKVENT_COMPONENT_TICKER_SERVE", "grpc"),
        ("SEKVENT_COMPONENT_TICKER_SERVE_AUTH", "none"),
    ]
    .into_iter()
    .collect();
    let app = build_with(&config, move |builder| {
        TickerHandle::install(builder, move |_| Ok(scripted))
    })
    .unwrap();
    let router = app.grpc_routes().into_axum_router();
    app.start().await.unwrap();

    let feed = control.feed();
    for seq in 1..=3 {
        feed.send(Ok(tick("feed", seq))).await.unwrap();
    }
    drop(feed);
    let response = router
        .clone()
        .oneshot(ticks_request(&series("feed")))
        .await
        .unwrap();
    assert!(!response.headers().contains_key("grpc-status"));
    let count = response
        .extensions()
        .get::<MessageCount>()
        .cloned()
        .expect("a message count");
    let collected = response.into_body().collect().await.unwrap();
    assert_eq!(count.get(), 3, "counted as the items went out");
    assert_eq!(collected.trailers().unwrap()["grpc-status"], "0");

    let refused = router
        .oneshot(ticks_request(&series("unknown")))
        .await
        .unwrap();
    assert_eq!(refused.headers()["grpc-status"], "5");
    assert!(
        refused.extensions().get::<MessageCount>().is_none(),
        "a failed open has no stream to count"
    );
    app.stop(Duration::ZERO).await.unwrap();
}

#[tokio::test]
async fn a_plain_tonic_client_reads_items_and_the_typed_end() {
    guarded(async {
        let (service, control, _events) = ticker_service(&[]).await;
        let feed = control.feed();
        feed.send(Ok(tick("interop", 1))).await.unwrap();
        feed.send(Err(TickerError::Gap { missing: 2 }))
            .await
            .unwrap();

        let channel = tonic::transport::Endpoint::from_shared(service.endpoint())
            .unwrap()
            .connect()
            .await
            .unwrap();
        let mut grpc = tonic::client::Grpc::new(channel);
        grpc.ready().await.unwrap();
        let mut request = tonic::Request::new(series("interop"));
        request
            .metadata_mut()
            .insert("authorization", format!("Bearer {TOKEN}").parse().unwrap());
        let mut stream = grpc
            .server_streaming(
                request,
                PathAndQuery::from_static(TICKS),
                tonic_prost::ProstCodec::<TicksRequest, Tick>::default(),
            )
            .await
            .unwrap()
            .into_inner();
        assert_eq!(stream.message().await.unwrap(), Some(tick("interop", 1)));
        let status = stream.message().await.unwrap_err();
        assert_eq!(status.code(), tonic::Code::FailedPrecondition);
        let info = status.get_details_error_info().unwrap();
        assert_eq!(info.reason, "SERIES_GAP");
        assert_eq!(info.domain, "shop.ticker.v1");
        assert_eq!(info.metadata["missing"], "2");
        service.stop().await;
    })
    .await;
}
