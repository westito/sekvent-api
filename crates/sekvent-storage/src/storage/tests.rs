use std::time::UNIX_EPOCH;

use sekvent_config::MapSource;
use sekvent_context::ManualClock;

use super::*;
use crate::test_support::{config, fake, response};

/// 2013-05-24T00:00:00Z.
fn example_time() -> SystemTime {
    UNIX_EPOCH + Duration::from_hours(380_376)
}

fn fixed_clock() -> Arc<ManualClock> {
    Arc::new(ManualClock::new(example_time()))
}

type Change = Box<dyn Fn(&mut StorageConfig)>;

fn storage(endpoint: &str) -> Storage {
    Storage::builder(config(endpoint))
        .clock(fixed_clock())
        .policy(Policy::new("test"))
        .build()
        .unwrap()
}

fn key(value: &str) -> ObjectKey {
    ObjectKey::new(value).unwrap()
}

fn query_value<'a>(url: &'a str, name: &str) -> Option<&'a str> {
    url.split_once('?')?
        .1
        .split('&')
        .find_map(|pair| pair.strip_prefix(name)?.strip_prefix('='))
}

#[test]
fn build_rejects_each_bad_setting_by_name() {
    let cases: Vec<(&str, Change)> = vec![
        ("ENDPOINT", Box::new(|c| c.endpoint = "ftp://x".into())),
        (
            "PUBLIC_ENDPOINT",
            Box::new(|c| c.public_endpoint = Some("http://h/p".into())),
        ),
        ("REGION", Box::new(|c| c.region = " ".into())),
        ("BUCKET", Box::new(|c| c.bucket = "No".into())),
        (
            "BUCKET",
            Box::new(|c| {
                c.bucket = "a.b.c".into();
                c.path_style = false;
            }),
        ),
        (
            "ACCESS_KEY_ID",
            Box::new(|c| c.access_key_id = String::new()),
        ),
        (
            "SECRET_ACCESS_KEY",
            Box::new(|c| c.secret_access_key = " ".into()),
        ),
        (
            "SESSION_TOKEN",
            Box::new(|c| c.session_token = Some("".into())),
        ),
        ("TIMEOUT", Box::new(|c| c.timeout = Duration::ZERO)),
    ];
    for (name, change) in cases {
        let mut bad = config("http://127.0.0.1:1");
        change(&mut bad);
        let error = Storage::new(bad).unwrap_err();
        assert_eq!(error.code(), ErrorCode::InvalidArgument);
        assert!(
            error
                .message()
                .starts_with(&format!("the storage setting {name} ")),
            "{}",
            error.message()
        );
        assert!(!error.message().contains("EXAMPLEKEY"));
    }
}

#[test]
fn from_config_reads_the_prefix_and_reports_keys() {
    let source = MapSource::new()
        .with("FILES_ENDPOINT", "http://127.0.0.1:9000")
        .with("FILES_REGION", "us-east-1")
        .with("FILES_BUCKET", "files")
        .with("FILES_ACCESS_KEY_ID", "id")
        .with("FILES_SECRET_ACCESS_KEY", "secret")
        .with("FILES_PATH_STYLE", "true");
    let storage = Storage::from_config(&source, "FILES_").unwrap();
    assert_eq!(storage.bucket(), "files");
    let shown = format!("{storage:?}");
    assert!(shown.contains("127.0.0.1:9000"));
    assert!(!shown.contains("secret"));
    let error = Storage::from_config(&MapSource::new(), "FILES_").unwrap_err();
    assert_eq!(error.code(), ErrorCode::InvalidArgument);
    assert!(error.message().contains("FILES_BUCKET"));
    let builder = Storage::builder(config("http://h"));
    assert!(!format!("{builder:?}").contains("EXAMPLEKEY"));
}

#[test]
fn presigned_puts_sign_type_and_length_for_the_public_endpoint() {
    let mut settings = config("http://minio:9000");
    settings.public_endpoint = Some("https://files.example.com".into());
    let storage = Storage::builder(settings)
        .clock(fixed_clock())
        .build()
        .unwrap();
    let put = storage
        .presign_put(
            &key("a b/ü.png"),
            PresignPut::new("image/png")
                .content_length(42)
                .expires_in(Duration::from_mins(1)),
        )
        .unwrap();
    assert_eq!(put.method, "PUT");
    assert!(
        put.url
            .starts_with("https://files.example.com/files/a%20b/%C3%BC.png?"),
        "{}",
        put.url
    );
    assert_eq!(
        query_value(&put.url, "X-Amz-SignedHeaders"),
        Some("content-length%3Bcontent-type%3Bhost")
    );
    assert_eq!(query_value(&put.url, "X-Amz-Expires"), Some("60"));
    assert_eq!(
        query_value(&put.url, "X-Amz-Date"),
        Some("20130524T000000Z")
    );
    assert_eq!(query_value(&put.url, "X-Amz-Signature").unwrap().len(), 64);
    assert_eq!(
        put.headers,
        [("content-type".to_owned(), "image/png".to_owned())]
    );
    assert_eq!(put.expires_at, example_time() + Duration::from_mins(1));
    let again = storage
        .presign_put(
            &key("a b/ü.png"),
            PresignPut::new("image/png")
                .content_length(42)
                .expires_in(Duration::from_mins(1)),
        )
        .unwrap();
    assert_eq!(again.url, put.url, "a fixed clock signs deterministically");
    let without_length = storage
        .presign_put(&key("k"), PresignPut::new("image/png"))
        .unwrap();
    assert_eq!(
        query_value(&without_length.url, "X-Amz-SignedHeaders"),
        Some("content-type%3Bhost")
    );
    assert_eq!(
        query_value(&without_length.url, "X-Amz-Expires"),
        Some("900")
    );
}

#[test]
fn presigned_gets_carry_the_response_overrides() {
    let storage = storage("http://127.0.0.1:9000");
    let get = storage
        .presign_get(
            &key("k"),
            PresignGet::new()
                .attachment("ä.txt")
                .response_content_type("application/octet-stream"),
        )
        .unwrap();
    assert_eq!(get.method, "GET");
    assert_eq!(get.headers, [] as [(String, String); 0]);
    assert!(get.url.starts_with("http://127.0.0.1:9000/files/k?"));
    assert_eq!(
        query_value(&get.url, "response-content-disposition"),
        Some("attachment%3B%20filename%3D%22_.txt%22%3B%20filename%2A%3DUTF-8%27%27%25C3%25A4.txt")
    );
    assert_eq!(
        query_value(&get.url, "response-content-type"),
        Some("application%2Foctet-stream")
    );
    assert_eq!(query_value(&get.url, "X-Amz-SignedHeaders"), Some("host"));
}

#[test]
fn virtual_hosted_urls_put_the_bucket_in_the_host() {
    let mut settings = config("https://s3.eu-central-1.amazonaws.com");
    settings.path_style = false;
    let storage = Storage::new(settings).unwrap();
    let get = storage.presign_get(&key("a/b"), PresignGet::new()).unwrap();
    assert!(
        get.url
            .starts_with("https://files.s3.eu-central-1.amazonaws.com/a/b?"),
        "{}",
        get.url
    );
    let (host, path) = storage.locate(&storage.inner.endpoint, None);
    assert_eq!(
        (host.as_str(), path.as_str()),
        ("files.s3.eu-central-1.amazonaws.com", "/")
    );
}

#[test]
fn bad_presign_requests_are_invalid_arguments() {
    let storage = storage("http://127.0.0.1:9000");
    for error in [
        storage
            .presign_put(&key("k"), PresignPut::new("not a type"))
            .unwrap_err(),
        storage
            .presign_put(
                &key("k"),
                PresignPut::new("image/png").expires_in(Duration::from_hours(8 * 24)),
            )
            .unwrap_err(),
        storage
            .presign_get(&key("k"), PresignGet::new().response_content_type("x"))
            .unwrap_err(),
        storage
            .presign_get(&key("k"), PresignGet::new().expires_in(Duration::ZERO))
            .unwrap_err(),
    ] {
        assert_eq!(error.code(), ErrorCode::InvalidArgument);
        assert_eq!(error.reason(), Some(reasons::INVALID_PRESIGN_REQUEST));
    }
}

#[tokio::test]
async fn head_reads_the_object_metadata_from_a_signed_request() {
    let mut fake = fake(Some(
        "HTTP/1.1 200 OK\r\ncontent-length: 1234\r\ncontent-type: image/png\r\n\
         etag: \"abc123\"\r\nlast-modified: Fri, 24 May 2013 00:00:00 GMT\r\n\
         connection: close\r\n\r\n",
    ))
    .await;
    let storage = storage(&fake.endpoint);
    let info = storage
        .head(&CallContext::new(), &key("dir/a b.png"))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        info,
        ObjectInfo {
            size: 1234,
            content_type: Some("image/png".into()),
            etag: Some("abc123".into()),
            last_modified: Some(example_time()),
        }
    );
    let request = fake.request().await.to_ascii_lowercase();
    assert!(
        request.starts_with("head /files/dir/a%20b.png http/1.1"),
        "{request}"
    );
    assert!(request.contains("authorization: aws4-hmac-sha256 credential=akiaiosfodnn7example/20130524/us-east-1/s3/aws4_request"));
    assert!(request.contains("x-amz-date: 20130524t000000z"));
    assert!(request.contains(&format!("x-amz-content-sha256: {EMPTY_SHA256}")));
}

#[tokio::test]
async fn head_of_a_missing_object_is_none() {
    let fake = fake(Some(response("404 Not Found", "", ""))).await;
    let found = storage(&fake.endpoint)
        .head(&CallContext::new(), &key("k"))
        .await
        .unwrap();
    assert_eq!(found, None);
}

#[tokio::test]
async fn head_without_a_size_is_an_error() {
    let fake = fake(Some("HTTP/1.1 200 OK\r\nconnection: close\r\n\r\n")).await;
    let error = storage(&fake.endpoint)
        .head(&CallContext::new(), &key("k"))
        .await
        .unwrap_err();
    assert_eq!(error.code(), ErrorCode::Internal);
    assert_eq!(error.message(), "the object store sent no object size");
}

#[tokio::test]
async fn a_refused_request_is_internal_and_keeps_only_the_s3_code() {
    let fake = fake(Some(response(
        "403 Forbidden",
        "content-type: application/xml\r\n",
        "<Error><Code>SignatureDoesNotMatch</Code><Message>sig detail here</Message></Error>",
    )))
    .await;
    let error = storage(&fake.endpoint)
        .delete(&CallContext::new(), &key("k"))
        .await
        .unwrap_err();
    assert_eq!(error.code(), ErrorCode::Internal);
    assert_eq!(error.reason(), Some(reasons::STORAGE_REJECTED));
    assert_eq!(error.message(), "the object store responded with HTTP 403");
    let chain = format!("{error:?}");
    assert!(!chain.contains("sig detail"));
}

#[tokio::test]
async fn delete_and_check_succeed_on_success_statuses() {
    let mut fake = fake(Some("HTTP/1.1 204 No Content\r\nconnection: close\r\n\r\n")).await;
    let storage = storage(&fake.endpoint);
    storage
        .delete(&CallContext::new(), &key("k"))
        .await
        .unwrap();
    assert!(fake.request().await.starts_with("DELETE /files/k "));
    storage.check(&CallContext::new()).await.unwrap();
    assert!(fake.request().await.starts_with("HEAD /files "));
}

#[tokio::test]
async fn a_missing_bucket_fails_the_check() {
    let fake = fake(Some(response("404 Not Found", "", ""))).await;
    let error = storage(&fake.endpoint)
        .check(&CallContext::new())
        .await
        .unwrap_err();
    assert_eq!(error.code(), ErrorCode::Internal);
    assert_eq!(error.reason(), Some(reasons::STORAGE_REJECTED));
}

#[tokio::test]
async fn a_missing_bucket_named_in_the_body_is_rejected() {
    let fake = fake(Some(response(
        "404 Not Found",
        "",
        "<Error><Code>NoSuchBucket</Code></Error>",
    )))
    .await;
    let error = storage(&fake.endpoint)
        .delete(&CallContext::new(), &key("k"))
        .await
        .unwrap_err();
    assert_eq!(error.reason(), Some(reasons::STORAGE_REJECTED));
}

#[tokio::test]
async fn copy_sends_the_encoded_source() {
    let mut fake = fake(Some(response("200 OK", "", "<CopyObjectResult/>"))).await;
    storage(&fake.endpoint)
        .copy(&CallContext::new(), &key("from/ä"), &key("to/b"))
        .await
        .unwrap();
    let request = fake.request().await;
    assert!(request.starts_with("PUT /files/to/b "));
    assert!(request.contains("x-amz-copy-source: /files/from/%C3%A4"));
    assert!(request.contains("x-amz-copy-source;x-amz-date"));
}

#[tokio::test]
async fn copy_of_a_missing_source_is_not_found() {
    let fake = fake(Some(response(
        "404 Not Found",
        "",
        "<Error><Code>NoSuchKey</Code></Error>",
    )))
    .await;
    let error = storage(&fake.endpoint)
        .copy(&CallContext::new(), &key("a"), &key("b"))
        .await
        .unwrap_err();
    assert_eq!(error.code(), ErrorCode::NotFound);
    assert_eq!(error.reason(), Some(reasons::OBJECT_NOT_FOUND));
}

#[tokio::test]
async fn a_copy_reported_as_failed_in_a_200_is_unavailable() {
    let fake = fake(Some(response(
        "200 OK",
        "",
        "<Error><Code>InternalError</Code></Error>",
    )))
    .await;
    let error = storage(&fake.endpoint)
        .copy(&CallContext::new(), &key("a"), &key("b"))
        .await
        .unwrap_err();
    assert_eq!(error.code(), ErrorCode::Unavailable);
    assert_eq!(error.reason(), Some(reasons::STORAGE_HTTP_ERROR));
}

#[tokio::test]
async fn server_errors_follow_the_status_table() {
    let fake = fake(Some(response(
        "503 Service Unavailable",
        "retry-after: 2\r\n",
        "",
    )))
    .await;
    let error = storage(&fake.endpoint)
        .delete(&CallContext::new(), &key("k"))
        .await
        .unwrap_err();
    assert_eq!(error.code(), ErrorCode::Unavailable);
    assert_eq!(error.retry_after(), Some(Duration::from_secs(2)));
}

#[tokio::test]
async fn an_unreachable_store_is_unavailable() {
    let error = storage("http://127.0.0.1:1")
        .delete(&CallContext::new(), &key("k"))
        .await
        .unwrap_err();
    assert_eq!(error.code(), ErrorCode::Unavailable);
    assert_eq!(error.message(), "could not connect to the object store");
}

#[tokio::test]
async fn a_silent_store_runs_into_the_deadline() {
    let fake = fake(None).await;
    let ctx = CallContext::new().with_timeout(Duration::from_millis(200));
    let started = std::time::Instant::now();
    let error = tokio::time::timeout(
        Duration::from_secs(30),
        storage(&fake.endpoint).delete(&ctx, &key("k")),
    )
    .await
    .unwrap()
    .unwrap_err();
    assert_eq!(error.code(), ErrorCode::DeadlineExceeded);
    assert!(started.elapsed() >= Duration::from_millis(200));
}

#[tokio::test]
async fn a_short_request_timeout_fires_without_a_deadline() {
    let fake = fake(None).await;
    let mut settings = config(&fake.endpoint);
    settings.timeout = Duration::from_millis(100);
    let storage = Storage::builder(settings)
        .policy(Policy::new("test"))
        .build()
        .unwrap();
    let error = tokio::time::timeout(
        Duration::from_secs(30),
        storage.delete(&CallContext::new(), &key("k")),
    )
    .await
    .unwrap()
    .unwrap_err();
    assert_eq!(error.code(), ErrorCode::DeadlineExceeded);
    assert_eq!(error.message(), "the object store request timed out");
}

#[tokio::test]
async fn the_default_policy_retries_transient_failures() {
    let mut fake = fake(Some(response("503 Service Unavailable", "", ""))).await;
    let storage = Storage::new(config(&fake.endpoint)).unwrap();
    let error = storage
        .delete(&CallContext::new(), &key("k"))
        .await
        .unwrap_err();
    assert_eq!(error.code(), ErrorCode::Unavailable);
    for _ in 0..3 {
        fake.request().await;
    }
}
