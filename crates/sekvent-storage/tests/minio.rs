//! Presigned uploads and downloads against a real `MinIO` server.
//!
//! ```sh
//! SEKVENT_DOCKER_TESTS=1 cargo test -p sekvent-storage --all-features -- --ignored
//! ```

use std::sync::Arc;
use std::time::{Duration, SystemTime};

use sekvent_config::MapSource;
use sekvent_context::{CallContext, ManualClock};
use sekvent_error::ErrorCode;
use sekvent_storage::{
    ObjectKey, PresignGet, PresignPut, PresignedRequest, Storage, StorageConfig, reasons,
};
use sekvent_testing::{MinioHarness, TestBucket, docker_tests_enabled};

fn skip() -> bool {
    if docker_tests_enabled() {
        return false;
    }
    eprintln!("skipped: set SEKVENT_DOCKER_TESTS=1 to run Docker-backed tests");
    true
}

fn settings(bucket: &TestBucket) -> StorageConfig {
    let source: MapSource = bucket.config("FILES_").into_iter().collect();
    StorageConfig::load(&source, "FILES_").unwrap()
}

async fn bucket() -> TestBucket {
    MinioHarness::shared().await.create_bucket().await.unwrap()
}

fn http() -> reqwest::Client {
    sekvent_client::reqwest_builder().build().unwrap()
}

fn key(value: &str) -> ObjectKey {
    ObjectKey::new(value).unwrap()
}

/// Send `body` the way a browser would: the presigned URL plus the headers
/// it came with.
async fn upload(request: &PresignedRequest, body: &'static [u8]) -> reqwest::Response {
    assert_eq!(request.method, "PUT");
    let mut put = http().put(&request.url).body(body);
    for (name, value) in &request.headers {
        put = put.header(name, value);
    }
    put.send().await.unwrap()
}

#[tokio::test]
#[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
async fn a_presigned_upload_is_checked_downloaded_and_deleted() {
    if skip() {
        return;
    }
    let storage = Storage::new(settings(&bucket().await)).unwrap();
    let ctx = CallContext::new();
    let key = key("attachments/42/ä b.txt");
    let body: &'static [u8] = b"hello, object storage";

    let put = storage
        .presign_put(
            &key,
            PresignPut::new("text/plain").content_length(body.len() as u64),
        )
        .unwrap();
    let response = upload(&put, body).await;
    assert!(response.status().is_success(), "{}", response.status());

    let info = storage.head(&ctx, &key).await.unwrap().unwrap();
    assert_eq!(info.size, body.len() as u64);
    assert_eq!(info.content_type.as_deref(), Some("text/plain"));
    assert!(info.etag.is_some_and(|tag| !tag.contains('"')));
    assert!(info.last_modified.is_some());

    let get = storage
        .presign_get(
            &key,
            PresignGet::new().attachment("Jahresbericht – 2026.txt"),
        )
        .unwrap();
    let response = http().get(&get.url).send().await.unwrap();
    assert_eq!(response.status(), 200);
    let disposition = response.headers()["content-disposition"]
        .to_str()
        .unwrap()
        .to_owned();
    assert!(
        disposition.contains("filename*=UTF-8''Jahresbericht%20%E2%80%93%202026.txt"),
        "{disposition}"
    );
    assert_eq!(response.bytes().await.unwrap().as_ref(), body);

    let as_binary = storage
        .presign_get(
            &key,
            PresignGet::new().response_content_type("application/octet-stream"),
        )
        .unwrap();
    let response = http().get(&as_binary.url).send().await.unwrap();
    assert_eq!(
        response.headers()["content-type"],
        "application/octet-stream"
    );

    storage.delete(&ctx, &key).await.unwrap();
    assert_eq!(storage.head(&ctx, &key).await.unwrap(), None);
    storage.delete(&ctx, &key).await.unwrap();
    let response = http().get(&get.url).send().await.unwrap();
    assert_eq!(response.status(), 404);
}

#[tokio::test]
#[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
async fn an_expired_url_is_rejected() {
    if skip() {
        return;
    }
    let bucket = bucket().await;
    let an_hour_ago = SystemTime::now() - Duration::from_hours(1);
    let past = Storage::builder(settings(&bucket))
        .clock(Arc::new(ManualClock::new(an_hour_ago)))
        .build()
        .unwrap();
    let put = past
        .presign_put(
            &key("late"),
            PresignPut::new("text/plain").expires_in(Duration::from_mins(1)),
        )
        .unwrap();
    assert!(put.expires_at < SystemTime::now());
    let response = upload(&put, b"too late").await;
    assert_eq!(response.status(), 403);

    let storage = Storage::new(settings(&bucket)).unwrap();
    assert_eq!(
        storage
            .head(&CallContext::new(), &key("late"))
            .await
            .unwrap(),
        None
    );
}

#[tokio::test]
#[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
async fn uploads_must_match_the_signed_type_and_length() {
    if skip() {
        return;
    }
    let storage = Storage::new(settings(&bucket().await)).unwrap();
    let put = storage
        .presign_put(
            &key("typed"),
            PresignPut::new("image/png").content_length(4),
        )
        .unwrap();

    let mut wrong_type = put.clone();
    wrong_type.headers = vec![("content-type".to_owned(), "text/html".to_owned())];
    assert_eq!(upload(&wrong_type, b"\x89PNG").await.status(), 403);

    assert_eq!(upload(&put, b"\x89PNG and more").await.status(), 403);

    assert!(
        storage
            .head(&CallContext::new(), &key("typed"))
            .await
            .unwrap()
            .is_none()
    );
    assert!(upload(&put, b"\x89PNG").await.status().is_success());
    let info = storage
        .head(&CallContext::new(), &key("typed"))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        (info.size, info.content_type.as_deref()),
        (4, Some("image/png"))
    );
}

#[tokio::test]
#[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
async fn objects_copy_within_the_bucket() {
    if skip() {
        return;
    }
    let storage = Storage::new(settings(&bucket().await)).unwrap();
    let ctx = CallContext::new();
    let put = storage
        .presign_put(&key("from/ü"), PresignPut::new("text/csv"))
        .unwrap();
    assert!(upload(&put, b"a,b\n1,2\n").await.status().is_success());

    storage
        .copy(&ctx, &key("from/ü"), &key("to/copy"))
        .await
        .unwrap();
    let info = storage.head(&ctx, &key("to/copy")).await.unwrap().unwrap();
    assert_eq!(
        (info.size, info.content_type.as_deref()),
        (8, Some("text/csv"))
    );

    let error = storage
        .copy(&ctx, &key("missing"), &key("to/other"))
        .await
        .unwrap_err();
    assert_eq!(error.code(), ErrorCode::NotFound);
    assert_eq!(error.reason(), Some(reasons::OBJECT_NOT_FOUND));
}

#[tokio::test]
#[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
async fn wrong_credentials_and_missing_buckets_are_rejected() {
    if skip() {
        return;
    }
    let bucket = bucket().await;
    let ctx = CallContext::new();
    Storage::new(settings(&bucket))
        .unwrap()
        .check(&ctx)
        .await
        .unwrap();

    let mut wrong_secret = settings(&bucket);
    wrong_secret.secret_access_key = "not-the-secret".into();
    let error = Storage::new(wrong_secret)
        .unwrap()
        .head(&ctx, &key("k"))
        .await
        .unwrap_err();
    assert_eq!(error.code(), ErrorCode::Internal);
    assert_eq!(error.reason(), Some(reasons::STORAGE_REJECTED));
    assert!(!format!("{error:?}").contains("not-the-secret"));

    let mut missing = settings(&bucket);
    missing.bucket = "no-such-bucket-here".to_owned();
    let storage = Storage::new(missing).unwrap();
    let error = storage.check(&ctx).await.unwrap_err();
    assert_eq!(error.reason(), Some(reasons::STORAGE_REJECTED));
    let error = storage.delete(&ctx, &key("k")).await.unwrap_err();
    assert_eq!(error.reason(), Some(reasons::STORAGE_REJECTED));
}

#[cfg(feature = "runtime")]
#[tokio::test]
#[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
async fn the_probe_reports_the_bucket() {
    use sekvent_runtime::{DependencyProbe, ProbeFailure, ProbeStatus};
    use sekvent_storage::StorageProbe;

    if skip() {
        return;
    }
    let bucket = bucket().await;
    let up = StorageProbe::new("files", Storage::new(settings(&bucket)).unwrap());
    assert_eq!(up.probe().await, ProbeStatus::Up);
    let mut missing = settings(&bucket);
    missing.bucket = "no-such-bucket-here".to_owned();
    let down = StorageProbe::new("files", Storage::new(missing).unwrap());
    assert_eq!(
        down.probe().await,
        ProbeStatus::Down(ProbeFailure::Rejected("bucket access rejected"))
    );
}
