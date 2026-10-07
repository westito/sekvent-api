//! The request id reaches access events under a `warn` filter.
//!
//! A level-filtered subscriber lowers tracing's process-wide max level while
//! it is installed, which would hide info events from tests running next to
//! it, so this test has its own binary.

use std::convert::Infallible;

use bytes::Bytes;
use http::{HeaderValue, Request, Response, StatusCode};
use http_body_util::{BodyExt, Full};
use sekvent_telemetry::LogBuffer;
use sekvent_telemetry::access_log::{ACCESS_LOG_TARGET, AccessLogLayer};
use sekvent_telemetry::request_id::REQUEST_ID_HEADER;
use tower::{Layer, ServiceExt};
use tracing::Level;
use tracing_subscriber::Registry;
use tracing_subscriber::filter::LevelFilter;
use tracing_subscriber::layer::SubscriberExt;

async fn answer(request: Request<()>, status: StatusCode) {
    let service = tower::service_fn(move |_request: Request<()>| async move {
        let mut response = Response::new(Full::new(Bytes::from_static(b"ok")));
        *response.status_mut() = status;
        Ok::<_, Infallible>(response)
    });
    let response = AccessLogLayer::new()
        .layer(service)
        .oneshot(request)
        .await
        .expect("infallible");
    let _ = response.into_body().collect().await;
}

fn get(path: &str) -> Request<()> {
    Request::get(path).body(()).expect("valid request")
}

#[tokio::test]
async fn the_request_id_survives_a_warn_filter() {
    let buffer = LogBuffer::new(16);
    let _guard = tracing::subscriber::set_default(
        Registry::default()
            .with(LevelFilter::WARN)
            .with(buffer.layer()),
    );
    let mut request = get("/orders");
    request
        .headers_mut()
        .insert(REQUEST_ID_HEADER, HeaderValue::from_static("warn-me"));
    answer(request, StatusCode::BAD_GATEWAY).await;
    answer(get("/orders"), StatusCode::OK).await;

    let records: Vec<_> = buffer
        .snapshot()
        .into_iter()
        .filter(|record| record.target == ACCESS_LOG_TARGET)
        .collect();
    assert_eq!(records.len(), 1, "{records:?}");
    assert_eq!(records[0].level, Level::WARN);
    assert_eq!(records[0].fields["request_id"], "warn-me");
    assert_eq!(records[0].fields["status"], "502");
}
