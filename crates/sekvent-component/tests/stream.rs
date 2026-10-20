//! Server-streaming methods under both in-process bindings, driven through
//! the hand-expanded ticker: items and their end, typed errors, drops and
//! cancellation, the caller's deadline, the open's timeout, the stream
//! lifetime, bulkheads, the hop limit, closing and stopping, panics, how far
//! a producer runs ahead, malformed bytes, `local_only` streams, and the
//! runtime closing streams when it shuts down.

mod support;

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use futures::stream::FusedStream as _;
use rstest::rstest;
use sekvent_component::__private::{BoxFuture, Bytes, Dispatch};
use sekvent_component::{
    App, AppError, Binding, CallContext, CallStream, ComponentState, ErrorCode, reasons,
};
use sekvent_runtime::{Runtime, UnitExit};
use support::fakes::{
    LOCAL_BINDINGS, ScriptedTicker, TickerEvents, build_with, deadline_in, source, tick,
};
use support::feed::{FeedHandle, Lines};
use support::grpc::guarded;
use support::ticker::{
    LatestRequest, Tick, TickerError, TickerHandle, TicksRequest, dispatcher, install_with_dispatch,
};
use tokio::time::Instant;

/// An App with the scripted ticker installed (with its lifecycle hooks),
/// its handle, a control handle on the ticker's feeds, and its events.
struct Fixture {
    app: App,
    ticker: TickerHandle,
    control: ScriptedTicker,
    events: TickerEvents,
}

fn built(binding: Binding, pairs: &[(&str, &str)]) -> Fixture {
    let (scripted, events) = ScriptedTicker::new();
    let control = scripted.share();
    let app = build_with(&source(binding, pairs), move |builder| {
        TickerHandle::install_with_lifecycle(builder, move |_| Ok(scripted))
    })
    .unwrap();
    let ticker = app.handle::<TickerHandle>().unwrap();
    assert_eq!(ticker.binding(), binding);
    Fixture {
        app,
        ticker,
        control,
        events,
    }
}

async fn started(binding: Binding, pairs: &[(&str, &str)]) -> Fixture {
    let fixture = built(binding, pairs);
    fixture.app.start().await.unwrap();
    fixture
}

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

/// Open series `name` with a fresh context.
async fn open(fixture: &Fixture, name: &str) -> CallStream<Tick, TickerError> {
    fixture
        .ticker
        .ticks(&CallContext::new(), series(name))
        .await
        .unwrap()
}

/// The `Other` error an open of series `name` fails with.
async fn open_error(fixture: &Fixture, cx: &CallContext, name: &str) -> AppError {
    other(fixture.ticker.ticks(cx, series(name)).await.unwrap_err())
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

/// On the paused clock: return once every task is idle.
async fn settle() {
    let idle = tokio::time::timeout(Duration::from_secs(1), std::future::pending::<()>()).await;
    assert!(idle.is_err());
}

fn produced(fixture: &Fixture) -> u64 {
    fixture.events.produced.load(Ordering::SeqCst)
}

/// Stop the App with a long grace and assert that the drain did not wait
/// for it: no stream kept its admission.
async fn stop_at_once(app: &App) {
    let started = Instant::now();
    app.stop(Duration::from_secs(30)).await.unwrap();
    assert_eq!(started.elapsed(), Duration::ZERO, "the drain waited");
}

#[rstest]
#[case::local(Binding::Local)]
#[case::serialized(Binding::LocalSerialized)]
#[tokio::test(start_paused = true)]
async fn items_arrive_in_order_then_the_end(#[case] binding: Binding) {
    let mut fixture = started(binding, &[]).await;
    let feed = fixture.control.feed();
    let started = Instant::now();
    let mut stream = open(&fixture, "feed").await;
    let opened = fixture.events.opened.recv().await.unwrap();
    // The producer's context lives as long as the stream (the 10 minute
    // lifetime, jittered), not as long as the open's TIMEOUT of 1 s.
    let left = Instant::from_std(opened.deadline().unwrap()) - started;
    assert!(
        left >= Duration::from_secs(540) && left <= Duration::from_secs(600),
        "{binding}: {left:?}"
    );

    feed.send(Ok(tick("feed", 1))).await.unwrap();
    feed.send(Ok(tick("feed", 2))).await.unwrap();
    assert_eq!(stream.next().await.unwrap().unwrap(), tick("feed", 1));
    assert_eq!(stream.next().await.unwrap().unwrap(), tick("feed", 2));
    assert!(!opened.cancel_token().is_cancelled());
    assert!(!stream.is_terminated());

    drop(feed);
    assert!(stream.next().await.is_none(), "{binding}");
    assert!(stream.is_terminated());
    assert!(stream.next().await.is_none());
    // Released at the end, while the consumer still holds the stream.
    fixture.events.dropped.recv().await.unwrap();
    assert!(opened.cancel_token().is_cancelled());
    stop_at_once(&fixture.app).await;
    drop(stream);
}

#[tokio::test]
async fn typed_open_and_item_errors_are_identical_under_both_bindings() {
    let mut seen = Vec::new();
    for binding in LOCAL_BINDINGS {
        let fixture = started(binding, &[]).await;
        let cx = CallContext::new();
        let unknown = fixture
            .ticker
            .ticks(&cx, series("unknown"))
            .await
            .unwrap_err();
        assert!(
            matches!(&unknown, TickerError::UnknownSeries { series } if series == "unknown"),
            "{binding}: {unknown:?}"
        );
        let paused = open_error(&fixture, &cx, "other").await;
        assert_eq!(paused.code(), ErrorCode::FailedPrecondition, "{binding}");
        assert_eq!(paused.reason(), Some("PAUSED"));
        assert!(
            !paused.metadata().contains_key("component"),
            "the handler's error is untouched: {paused:?}"
        );

        let feed = fixture.control.feed();
        let mut stream = fixture.ticker.ticks(&cx, series("gappy")).await.unwrap();
        feed.send(Ok(tick("gappy", 1))).await.unwrap();
        feed.send(Err(TickerError::Gap { missing: 3 }))
            .await
            .unwrap();
        feed.send(Ok(tick("gappy", 5))).await.unwrap();
        assert_eq!(stream.next().await.unwrap().unwrap(), tick("gappy", 1));
        let gap = stream.next().await.unwrap().unwrap_err();
        assert!(
            matches!(gap, TickerError::Gap { missing: 3 }),
            "{binding}: {gap:?}"
        );
        assert!(stream.next().await.is_none(), "the error ends the stream");

        let feed = fixture.control.feed();
        let mut stream = fixture.ticker.ticks(&cx, series("future")).await.unwrap();
        feed.send(Err(TickerError::Other(
            AppError::failed_precondition("from a newer server")
                .with_reason("FROM_THE_FUTURE")
                .with_metadata("shelf", "7"),
        )))
        .await
        .unwrap();
        let future = end_error(&mut stream).await;
        assert_eq!(future.reason(), Some("FROM_THE_FUTURE"), "{binding}");
        assert_eq!(future.metadata()["shelf"], "7");
        assert!(
            !future.metadata().contains_key("component"),
            "item errors are untouched: {future:?}"
        );
        seen.push((
            format!("{unknown:?} {gap:?}"),
            paused.to_wire(),
            future.to_wire(),
        ));
    }
    assert_eq!(seen[0], seen[1]);
}

#[rstest]
#[case::local(Binding::Local)]
#[case::serialized(Binding::LocalSerialized)]
#[tokio::test(start_paused = true)]
async fn dropping_the_stream_drops_the_producer_and_cancels_its_token(#[case] binding: Binding) {
    let mut fixture = started(binding, &[]).await;
    let _feed = fixture.control.feed();
    let stream = open(&fixture, "feed").await;
    let opened = fixture.events.opened.recv().await.unwrap();
    assert!(!opened.cancel_token().is_cancelled());
    drop(stream);
    fixture.events.dropped.recv().await.unwrap();
    assert!(opened.cancel_token().is_cancelled(), "{binding}");
    stop_at_once(&fixture.app).await;
}

#[rstest]
#[case::local(Binding::Local)]
#[case::serialized(Binding::LocalSerialized)]
#[tokio::test(start_paused = true)]
async fn the_caller_cancelling_ends_the_stream(#[case] binding: Binding) {
    let mut fixture = started(binding, &[]).await;
    let cx = CallContext::new();
    let mut stream = fixture.ticker.ticks(&cx, series("feed")).await.unwrap();
    cx.cancel_token().cancel();
    let error = end_error(&mut stream).await;
    assert_eq!(error.code(), ErrorCode::Cancelled, "{binding}");
    assert_eq!(error.message(), "the call was cancelled");
    assert_tagged(&error);
    assert!(stream.next().await.is_none());
    fixture.events.dropped.recv().await.unwrap();
    stop_at_once(&fixture.app).await;
}

#[rstest]
#[case::local(Binding::Local)]
#[case::serialized(Binding::LocalSerialized)]
#[tokio::test(start_paused = true)]
async fn the_callers_deadline_ends_the_stream_at_exactly_the_deadline(#[case] binding: Binding) {
    let mut fixture = started(binding, &[]).await;
    let started = Instant::now();
    let deadline = deadline_in(Duration::from_secs(5));
    let cx = CallContext::new().with_deadline(deadline);
    let mut stream = fixture.ticker.ticks(&cx, series("feed")).await.unwrap();
    // The producer's context has the caller's deadline (re-anchored on
    // the std clock under local-serialized, never later), not the TIMEOUT.
    let seen = fixture
        .events
        .opened
        .recv()
        .await
        .unwrap()
        .deadline()
        .unwrap();
    assert!(
        seen <= deadline && deadline.duration_since(seen) < Duration::from_millis(1),
        "{binding}"
    );

    let error = end_error(&mut stream).await;
    assert_eq!(started.elapsed(), Duration::from_secs(5), "{binding}");
    assert_eq!(error.code(), ErrorCode::DeadlineExceeded);
    assert_eq!(error.reason(), None);
    assert_eq!(error.message(), "the call deadline was exceeded");
    assert_tagged(&error);
    assert!(stream.next().await.is_none());
    fixture.events.dropped.recv().await.unwrap();
}

#[rstest]
#[case::local(Binding::Local)]
#[case::serialized(Binding::LocalSerialized)]
#[tokio::test(start_paused = true)]
async fn an_open_that_never_completes_is_bounded_by_the_timeout(#[case] binding: Binding) {
    let mut fixture = started(binding, &[]).await;
    let started = Instant::now();
    let error = open_error(&fixture, &CallContext::new(), "hang").await;
    assert_eq!(started.elapsed(), Duration::from_secs(1), "{binding}");
    assert_eq!(error.code(), ErrorCode::DeadlineExceeded);
    assert_eq!(error.reason(), Some(reasons::METHOD_TIMEOUT));
    assert_eq!(error.metadata()["timeout_ms"], "1000");
    assert_tagged(&error);
    fixture.events.dropped.recv().await.unwrap();

    // A caller deadline before the TIMEOUT is the caller's own.
    let started = Instant::now();
    let cx = CallContext::new().with_deadline(deadline_in(Duration::from_millis(200)));
    let error = open_error(&fixture, &cx, "hang").await;
    assert_eq!(started.elapsed(), Duration::from_millis(200), "{binding}");
    assert_eq!(error.code(), ErrorCode::DeadlineExceeded);
    assert_eq!(error.reason(), None);
    assert_tagged(&error);
    fixture.events.dropped.recv().await.unwrap();
    stop_at_once(&fixture.app).await;
}

#[rstest]
#[case::local(Binding::Local)]
#[case::serialized(Binding::LocalSerialized)]
#[tokio::test(start_paused = true)]
async fn the_lifetime_ends_a_stream_between_ninety_and_a_hundred_percent(#[case] binding: Binding) {
    let mut fixture = started(
        binding,
        &[("SEKVENT_COMPONENT_TICKER_TICKS_STREAM_MAX_LIFETIME", "10s")],
    )
    .await;
    let started = Instant::now();
    let mut stream = open(&fixture, "feed").await;
    let opened = fixture.events.opened.recv().await.unwrap();
    let lifetime = Instant::from_std(opened.deadline().unwrap()) - started;
    assert!(
        lifetime >= Duration::from_secs(9) && lifetime <= Duration::from_secs(10),
        "{binding}: {lifetime:?}"
    );

    let error = end_error(&mut stream).await;
    let elapsed = started.elapsed();
    assert!(
        elapsed >= Duration::from_secs(9) && elapsed <= Duration::from_secs(10),
        "{binding}: {elapsed:?}"
    );
    assert_eq!(error.code(), ErrorCode::Unavailable);
    assert_eq!(error.reason(), Some(reasons::STREAM_EXPIRED));
    assert_eq!(error.metadata()["max_lifetime_ms"], "10000");
    assert_tagged(&error);
    assert!(stream.next().await.is_none());
    fixture.events.dropped.recv().await.unwrap();
    assert!(opened.cancel_token().is_cancelled());
}

#[rstest]
#[case::local(Binding::Local)]
#[case::serialized(Binding::LocalSerialized)]
#[tokio::test(start_paused = true)]
async fn the_bulkhead_counts_open_streams(#[case] binding: Binding) {
    let mut fixture = started(
        binding,
        &[(
            "SEKVENT_COMPONENT_TICKER_TICKS_BULKHEAD_MAX_CONCURRENT",
            "2",
        )],
    )
    .await;
    let ending = fixture.control.feed();
    let mut first = open(&fixture, "feed").await;
    let _kept = fixture.control.feed();
    let _second = open(&fixture, "feed").await;

    let full = open_error(&fixture, &CallContext::new(), "feed").await;
    assert_eq!(full.code(), ErrorCode::ResourceExhausted, "{binding}");
    assert_eq!(full.reason(), Some(reasons::BULKHEAD_FULL));
    assert_tagged(&full);
    for _ in 0..2 {
        fixture.events.opened.recv().await.unwrap();
    }
    assert!(
        fixture.events.opened.try_recv().is_err(),
        "the refused open ran no handler"
    );
    // The unary method has a bulkhead of its own.
    let latest = fixture
        .ticker
        .latest(&CallContext::new(), LatestRequest { series: "s".into() })
        .await
        .unwrap();
    assert_eq!(latest, tick("s", 7));

    // A stream that ends gives its permit back.
    drop(ending);
    assert!(first.next().await.is_none());
    let _again = fixture.control.feed();
    let _third = open(&fixture, "feed").await;
    fixture.events.opened.recv().await.unwrap();
}

#[rstest]
#[case::local(Binding::Local)]
#[case::serialized(Binding::LocalSerialized)]
#[tokio::test(start_paused = true)]
async fn a_queued_open_waits_for_a_stream_to_end(#[case] binding: Binding) {
    let mut fixture = started(
        binding,
        &[
            (
                "SEKVENT_COMPONENT_TICKER_TICKS_BULKHEAD_MAX_CONCURRENT",
                "1",
            ),
            ("SEKVENT_COMPONENT_TICKER_TICKS_BULKHEAD_MAX_QUEUE", "1"),
            (
                "SEKVENT_COMPONENT_TICKER_TICKS_BULKHEAD_QUEUE_TIMEOUT",
                "30s",
            ),
            ("SEKVENT_COMPONENT_TICKER_TICKS_TIMEOUT", "60s"),
        ],
    )
    .await;
    let feed = fixture.control.feed();
    let mut first = open(&fixture, "feed").await;
    fixture.events.opened.recv().await.unwrap();

    let ticker = fixture.ticker.clone();
    let mut queued =
        tokio::spawn(async move { ticker.ticks(&CallContext::new(), series("feed")).await });
    let started = Instant::now();
    let waited = tokio::time::timeout(Duration::from_secs(5), &mut queued).await;
    assert!(waited.is_err(), "{binding}: the second open waits");
    assert!(fixture.events.opened.try_recv().is_err());

    drop(feed);
    assert!(first.next().await.is_none());
    let second = queued.await.unwrap();
    assert!(second.is_ok(), "{binding}: {second:?}");
    fixture.events.opened.recv().await.unwrap();
    assert_eq!(started.elapsed(), Duration::from_secs(5));
}

#[rstest]
#[case::local(Binding::Local)]
#[case::serialized(Binding::LocalSerialized)]
#[tokio::test(start_paused = true)]
async fn opens_are_refused_before_the_handler_runs(#[case] binding: Binding) {
    let mut fixture = built(binding, &[("SEKVENT_COMPONENT_MAX_HOPS", "2")]);
    let error = open_error(&fixture, &CallContext::new(), "feed").await;
    assert_eq!(error.code(), ErrorCode::Unavailable, "{binding}");
    assert_eq!(error.reason(), Some(reasons::NOT_STARTED));
    assert_tagged(&error);
    fixture.app.start().await.unwrap();

    let error = open_error(&fixture, &CallContext::new().with_hops(2), "feed").await;
    assert_eq!(error.code(), ErrorCode::FailedPrecondition, "{binding}");
    assert_eq!(error.reason(), Some(reasons::CALL_DEPTH_EXCEEDED));
    assert_tagged(&error);

    let cancelled = CallContext::new();
    cancelled.cancel_token().cancel();
    let error = open_error(&fixture, &cancelled, "feed").await;
    assert_eq!(error.code(), ErrorCode::Cancelled, "{binding}");
    assert_tagged(&error);

    let late = CallContext::new().with_deadline(deadline_in(Duration::ZERO));
    let error = open_error(&fixture, &late, "feed").await;
    assert_eq!(error.code(), ErrorCode::DeadlineExceeded, "{binding}");
    assert_eq!(error.reason(), None);
    assert_tagged(&error);
    assert!(fixture.events.opened.try_recv().is_err(), "no handler ran");

    let _stream = fixture
        .ticker
        .ticks(&CallContext::new().with_hops(1), series("feed"))
        .await
        .unwrap();
    assert_eq!(fixture.events.opened.recv().await.unwrap().hops(), 2);
}

#[rstest]
#[case::local(Binding::Local)]
#[case::serialized(Binding::LocalSerialized)]
#[tokio::test(start_paused = true)]
async fn stopping_the_app_ends_open_streams_at_once(#[case] binding: Binding) {
    let mut fixture = started(binding, &[]).await;
    let mut stream = open(&fixture, "feed").await;
    let consumer = tokio::spawn(async move { stream.next().await });
    stop_at_once(&fixture.app).await;
    let error = other(consumer.await.unwrap().unwrap().unwrap_err());
    assert_eq!(error.code(), ErrorCode::Unavailable, "{binding}");
    assert_eq!(error.reason(), Some(reasons::DRAINING));
    assert_eq!(error.message(), "component ticker is shutting down");
    assert_tagged(&error);
    // `on_stop` ran, after the stream released the drain.
    fixture.events.stopped.recv().await.unwrap();
    assert_eq!(fixture.app.state("ticker"), Some(ComponentState::Stopped));
    let refused = open_error(&fixture, &CallContext::new(), "feed").await;
    assert_eq!(refused.reason(), Some(reasons::STOPPED), "{binding}");
}

#[rstest]
#[case::local(Binding::Local)]
#[case::serialized(Binding::LocalSerialized)]
#[tokio::test(start_paused = true)]
async fn closing_streams_ends_them_and_refuses_opens_while_unary_calls_go_on(
    #[case] binding: Binding,
) {
    let fixture = started(binding, &[]).await;
    let mut stream = open(&fixture, "feed").await;
    fixture.app.close_streams();
    let error = end_error(&mut stream).await;
    assert_eq!(error.code(), ErrorCode::Unavailable, "{binding}");
    assert_eq!(error.reason(), Some(reasons::DRAINING));
    assert_tagged(&error);
    assert!(stream.next().await.is_none());

    let refused = open_error(&fixture, &CallContext::new(), "feed").await;
    assert_eq!(refused.reason(), Some(reasons::DRAINING), "{binding}");
    assert_tagged(&refused);
    let latest = fixture
        .ticker
        .latest(&CallContext::new(), LatestRequest { series: "s".into() })
        .await
        .unwrap();
    assert_eq!(latest, tick("s", 7));
    assert_eq!(fixture.app.state("ticker"), Some(ComponentState::Serving));
    stop_at_once(&fixture.app).await;
}

#[rstest]
#[case::local(Binding::Local, 0)]
#[case::serialized(Binding::LocalSerialized, 16)]
#[tokio::test(start_paused = true)]
async fn a_producer_runs_ahead_of_its_consumer_only_under_local_serialized(
    #[case] binding: Binding,
    #[case] ahead: u64,
) {
    let fixture = started(binding, &[]).await;
    let mut stream = open(&fixture, "count").await;
    settle().await;
    assert_eq!(produced(&fixture), ahead, "{binding}");
    assert_eq!(stream.next().await.unwrap().unwrap(), tick("count", 0));
    settle().await;
    assert_eq!(produced(&fixture), ahead + 1, "{binding}");
}

#[tokio::test(start_paused = true)]
async fn a_serialized_stream_closed_behind_its_consumer_ends_after_its_items() {
    let fixture = started(Binding::LocalSerialized, &[]).await;
    let mut stream = open(&fixture, "count").await;
    settle().await;
    assert_eq!(produced(&fixture), 16);

    // The task ends the stream at once, releasing its admission, and
    // delivers the end after the items already sent.
    fixture.app.close_streams();
    stop_at_once(&fixture.app).await;
    for seq in 0..16 {
        assert_eq!(stream.next().await.unwrap().unwrap(), tick("count", seq));
    }
    let error = end_error(&mut stream).await;
    assert_eq!(error.reason(), Some(reasons::DRAINING));
    assert_tagged(&error);
    assert!(stream.next().await.is_none());
    assert_eq!(produced(&fixture), 16, "the producer was not polled again");
}

#[tokio::test(start_paused = true)]
async fn a_panic_propagates_into_the_consumer_under_local() {
    let fixture = started(Binding::Local, &[]).await;
    let mut stream = open(&fixture, "panic").await;
    assert_eq!(stream.next().await.unwrap().unwrap(), tick("panic", 1));
    let polled = tokio::spawn(async move { stream.next().await }).await;
    assert!(polled.unwrap_err().is_panic());

    let ticker = fixture.ticker.clone();
    let opened = tokio::spawn(async move {
        ticker
            .ticks(&CallContext::new(), series("panic-open"))
            .await
    })
    .await;
    assert!(opened.unwrap_err().is_panic());
    // Both admissions were released while unwinding.
    stop_at_once(&fixture.app).await;
}

#[tokio::test(start_paused = true)]
async fn a_panic_is_internal_under_local_serialized() {
    let fixture = started(Binding::LocalSerialized, &[]).await;
    let mut stream = open(&fixture, "panic").await;
    assert_eq!(stream.next().await.unwrap().unwrap(), tick("panic", 1));
    let error = end_error(&mut stream).await;
    assert_eq!(error.code(), ErrorCode::Internal);
    assert_eq!(error.message(), "component ticker method ticks panicked");
    assert_eq!(error.reason(), None);
    assert_tagged(&error);
    assert!(stream.next().await.is_none());

    let error = open_error(&fixture, &CallContext::new(), "panic-open").await;
    assert_eq!(error.code(), ErrorCode::Internal);
    assert_eq!(error.message(), "component ticker method ticks panicked");
    assert_tagged(&error);
    stop_at_once(&fixture.app).await;
}

/// A dispatcher whose stream items are not valid ticks.
struct GarbageItems;

impl Dispatch for GarbageItems {
    fn dispatch_stream(
        &self,
        _method: usize,
        _cx: CallContext,
        _body: Bytes,
    ) -> BoxFuture<'static, Result<CallStream<Bytes, AppError>, AppError>> {
        Box::pin(async {
            Ok(CallStream::iter([Ok(Bytes::from_static(&[
                0x0a, 0xff, 0xff,
            ]))]))
        })
    }
}

#[tokio::test]
async fn malformed_bytes_are_rejected_on_both_sides() {
    let (scripted, _events) = ScriptedTicker::new();
    let dispatch = dispatcher(scripted);
    let garbage = dispatch
        .dispatch_stream(0, CallContext::new(), Bytes::from_static(&[0x0a, 0xff]))
        .await
        .unwrap_err();
    assert_eq!(garbage.code(), ErrorCode::InvalidArgument);
    assert_eq!(garbage.reason(), Some(reasons::MALFORMED_REQUEST));
    let unary = dispatch
        .dispatch_stream(1, CallContext::new(), Bytes::new())
        .await
        .unwrap_err();
    assert_eq!(unary.code(), ErrorCode::Unimplemented);
    assert!(unary.message().contains("ticker"), "{unary}");
    let streaming = dispatch
        .dispatch(0, CallContext::new(), Bytes::new())
        .await
        .unwrap_err();
    assert_eq!(streaming.code(), ErrorCode::Unimplemented);

    let (scripted, _events) = ScriptedTicker::new();
    let app = build_with(&source(Binding::LocalSerialized, &[]), move |builder| {
        install_with_dispatch(builder, scripted, Arc::new(GarbageItems))
    })
    .unwrap();
    app.start().await.unwrap();
    let ticker = app.handle::<TickerHandle>().unwrap();
    let mut stream = ticker
        .ticks(&CallContext::new(), series("feed"))
        .await
        .unwrap();
    let error = end_error(&mut stream).await;
    assert_eq!(error.code(), ErrorCode::Internal);
    assert_eq!(error.reason(), Some(reasons::MALFORMED_REPLY));
    assert_tagged(&error);
    assert!(stream.next().await.is_none());
}

#[tokio::test]
async fn local_only_streams_carry_plain_values() {
    let app = build_with(&source(Binding::LocalSerialized, &[]), |builder| {
        FeedHandle::install(builder, |_| Ok(Lines))
    })
    .unwrap();
    assert_eq!(app.binding("feed"), Some(Binding::Local));
    app.start().await.unwrap();
    let feed = app.handle::<FeedHandle>().unwrap();
    assert_eq!(feed.binding(), Binding::Local);
    let cx = CallContext::new();

    let mut lines = feed.lines(&cx, "one\ntwo".into()).await.unwrap();
    assert_eq!(lines.next().await.unwrap().unwrap(), "one");
    assert_eq!(lines.next().await.unwrap().unwrap(), "two");
    assert!(lines.next().await.is_none());

    let empty = feed.lines(&cx, String::new()).await.unwrap_err();
    assert_eq!(empty.code(), ErrorCode::InvalidArgument);
    assert_eq!(empty.reason(), Some("EMPTY_FEED"));
    assert!(!empty.metadata().contains_key("component"));

    let mut broken = feed.lines(&cx, "x\nboom\ny".into()).await.unwrap();
    assert_eq!(broken.next().await.unwrap().unwrap(), "x");
    let error = broken.next().await.unwrap().unwrap_err();
    assert_eq!(error.code(), ErrorCode::Internal);
    assert_eq!(error.reason(), Some("FEED_BROKEN"));
    assert!(broken.next().await.is_none());

    let deep = feed
        .lines(&cx.clone().with_hops(16), "a".into())
        .await
        .unwrap_err();
    assert_eq!(deep.reason(), Some(reasons::CALL_DEPTH_EXCEEDED));
    assert_eq!(deep.metadata()["component"], "feed");
    assert_eq!(deep.metadata()["method"], "lines");
    app.stop(Duration::ZERO).await.unwrap();
}

#[rstest]
#[case::local(Binding::Local)]
#[case::serialized(Binding::LocalSerialized)]
#[tokio::test]
async fn the_runtime_closes_streams_as_soon_as_it_begins_shutting_down(#[case] binding: Binding) {
    guarded(async {
        let mut fixture = built(binding, &[]);
        let runtime = fixture
            .app
            .register(Runtime::builder().without_signals())
            .build()
            .unwrap()
            .start()
            .await
            .unwrap();
        let mut stream = open(&fixture, "feed").await;
        let consumer = tokio::spawn(async move { stream.next().await });
        runtime.shutdown();
        let error = other(consumer.await.unwrap().unwrap().unwrap_err());
        assert_eq!(error.code(), ErrorCode::Unavailable, "{binding}");
        assert_eq!(error.reason(), Some(reasons::DRAINING));
        assert_tagged(&error);

        let report = runtime.wait().await.unwrap();
        assert_eq!(report.unit("components").unwrap().exit, UnitExit::Completed);
        fixture.events.stopped.recv().await.unwrap();
        assert_eq!(fixture.app.state("ticker"), Some(ComponentState::Stopped));
    })
    .await;
}
