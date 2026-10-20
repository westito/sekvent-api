# Object storage (`sekvent::storage`)

`sekvent::storage` lets browsers upload files to and download files from an
S3-compatible store (AWS S3, MinIO, Cloudflare R2, …) directly, with
presigned URLs, so file bytes never pass through the service. The service
signs a short-lived URL, the browser talks to the store, and the service
checks what arrived with a server-side `HEAD` before it records anything.
It also deletes and copies objects and can gate readiness on the bucket.

Requests are signed with AWS Signature Version 4, implemented in the crate
(no AWS SDK), and sent over the workspace's rustls/ring TLS stack.

## Enable it

| | |
|---|---|
| Facade feature | `storage`; with `runtime` also `StorageProbe` |
| Module | `use sekvent::storage::…;` |
| Internal crate | `sekvent-storage` (feature `runtime` for the probe) |
| Test container | `sekvent-testing` feature `minio` ([testing](testing.md)) |

```toml
[dependencies]
sekvent-api = { git = "https://github.com/westito/sekvent-api", branch = "master", features = ["storage"] }

[dev-dependencies]
sekvent-testing = { git = "https://github.com/westito/sekvent-api", branch = "master", features = ["minio"] }
```

## Quick example

```rust
use sekvent::config::EnvSource;
use sekvent::prelude::*;
use sekvent::storage::{ObjectKey, PresignGet, PresignPut, Storage};

// At startup: STORAGE_ENDPOINT, STORAGE_REGION, STORAGE_BUCKET, …
let storage = Storage::from_config(&EnvSource, "STORAGE_")?;

// 1. The browser asks to upload a 5 KiB PNG. The server picks the key.
let key = ObjectKey::new(format!("attachments/{issue_id}/{}", uuid::Uuid::now_v7()))?;
let upload = storage.presign_put(&key, PresignPut::new("image/png").content_length(5120))?;
// Return upload.url and upload.headers; the browser does
// fetch(url, { method: "PUT", headers, body: file }).

// 2. The browser reports completion. Check what arrived, then record it.
let ctx = CallContext::new();
match storage.head(&ctx, &key).await? {
    Some(info) if info.size == 5120 && info.content_type.as_deref() == Some("image/png") => {
        // insert the attachment row: key, original file name, size, type
    }
    Some(_) => storage.delete(&ctx, &key).await?,
    None => return Err(AppError::failed_precondition("the upload did not arrive")),
}

// 3. Downloads: a fresh URL per request, with the original file name.
let download = storage.presign_get(&key, PresignGet::new().attachment("diagram.png"))?;
```

## Configuration keys

`StorageConfig` (`#[derive(EnvConfig)]`) has no prefix of its own; read it
with `StorageConfig::load(source, "STORAGE_")`, `Storage::from_config`, or
nest it with `#[config(nested)]`.

| Key | Required | Default | Meaning |
|---|---|---|---|
| `<P>ENDPOINT` | yes | | S3 API endpoint the service calls: `scheme://host[:port]`, no path (`https://s3.eu-central-1.amazonaws.com`, `http://minio:9000`) |
| `<P>PUBLIC_ENDPOINT` | no | `ENDPOINT` | Endpoint presigned URLs point at, when browsers reach the store at another address |
| `<P>REGION` | yes | | Region to sign for (`us-east-1` for MinIO's default) |
| `<P>BUCKET` | yes | | Bucket name (S3 naming rules) |
| `<P>ACCESS_KEY_ID` | yes | | Access key id |
| `<P>SECRET_ACCESS_KEY` | yes, secret | | Secret access key |
| `<P>SESSION_TOKEN` | no, secret | | Session token of temporary credentials |
| `<P>PATH_STYLE` | no | `false` | `endpoint/bucket/key` (MinIO, most self-hosted stores) instead of `bucket.endpoint/key` (AWS) |
| `<P>TIMEOUT` | no | `30s` | Timeout of each server-side request |

Fail closed: a missing key, a blank secret, an endpoint with a path or
credentials, an invalid bucket name, a bucket with `.` without path style,
or a zero timeout is a startup error naming the key, never the value.
`Debug` of the config, the builder and `Storage` never shows secrets.

Why two endpoints: SigV4 signs the `Host` header. A service in a Docker
network may call `http://minio:9000` while browsers reach the same server
as `https://files.example.com`; presigned URLs are signed for
`PUBLIC_ENDPOINT`, server-side calls for `ENDPOINT`. Both must reach the
same store with the same bucket addressing.

## API

| Item | What it does |
|---|---|
| `Storage::new(config)`, `Storage::from_config(source, prefix)` | Build with defaults; errors are `INVALID_ARGUMENT` naming the setting |
| `Storage::builder(config).clock(…).policy(…).build()` | Inject a `Clock` (signing time, default `SystemClock`) and a resilience `Policy` |
| `presign_put(&key, PresignPut)` | Presigned `PUT`: content type always signed, exact length optionally signed |
| `presign_get(&key, PresignGet)` | Presigned `GET`, optionally with `Content-Disposition` and `Content-Type` overrides |
| `head(&ctx, &key)` | `Ok(Some(ObjectInfo { size, content_type, etag, last_modified }))`, or `Ok(None)` when missing |
| `delete(&ctx, &key)` | Delete; a missing object succeeds |
| `copy(&ctx, &from, &to)` | Server-side copy in the bucket, metadata included; a missing source is `NOT_FOUND` (`OBJECT_NOT_FOUND`) |
| `check(&ctx)` | `HEAD` on the bucket: exists and the credentials may use it |
| `StorageProbe::new(name, storage)` (`runtime`) | Readiness probe running `check` within 1.5 s; `.optional()` reports without gating |
| `attachment_disposition(name)` | The sanitised `Content-Disposition` value `PresignGet::attachment` uses |
| `ObjectKey::new(s)` | A validated key (below) |

### Presigned uploads

`PresignPut::new(content_type)` with `.content_length(bytes)` and
`.expires_in(duration)` (whole seconds, 1 s to 7 days, default 15 minutes;
`MAX_EXPIRY`, `DEFAULT_EXPIRY`). The content type must be a media type
(`type/subtype`, optional parameters).

The returned `PresignedRequest` has `method` (`"PUT"`), `url`, `headers` the
client must send exactly (`content-type`) and `expires_at`. A signed length
is not listed in `headers`: browsers set `Content-Length` from the body and
refuse to set it by hand, and a body of another size fails the signature.

S3 cannot cap a presigned `PUT` at a maximum size. Either sign the exact
size the client announced (and validate it against your limit before
presigning), or `head` the object after the upload and `delete` it when it
is too large or of the wrong type. Do both for untrusted users.

The bucket needs a CORS rule that allows `PUT` and `GET` from the
application's origin with the `content-type` header; configure it on the
store, not in the service.

### Presigned downloads

`PresignGet::new()` with `.expires_in(…)`, `.attachment(file_name)` (save
as), `.inline()` (show in the browser) and `.response_content_type(…)`
(serve `application/octet-stream` to keep a browser from rendering
user-uploaded HTML or SVG). The store echoes the overrides as response
headers.

`attachment(file_name)` keeps only the last path component, drops control
characters, cuts the name to 200 characters (empty becomes `download`),
and writes an ASCII `filename` (other characters and `"`, `%`, `;`, `\` as
`_`) plus the exact UTF-8 name as RFC 8187 `filename*`.

### Object keys

`ObjectKey` is 1 to 1024 bytes of UTF-8 in `/`-separated segments; it must
not start with `/`, and no segment may be empty, `.` or `..`. Control
characters and `\` are rejected. Errors are `INVALID_ARGUMENT`, reason
`INVALID_OBJECT_KEY`, and never quote the key.

Key design: let the server choose keys from identifiers it controls, e.g.
`attachments/<issue-id>/<uuid-v7>`, and keep the user's file name, size and
type in your database. A key derived from the file name leaks it in URLs
and logs, collides, and lets a user pick another user's key. A tenant or
owner prefix makes bucket policies and cleanup by prefix possible.

## Errors

| Situation | Code | Reason |
|---|---|---|
| Bad key | `INVALID_ARGUMENT` | `INVALID_OBJECT_KEY` |
| Bad content type, expiry | `INVALID_ARGUMENT` | `INVALID_PRESIGN_REQUEST` |
| `head` of a missing object | `Ok(None)` | |
| `copy` from a missing object | `NOT_FOUND` | `OBJECT_NOT_FOUND` |
| Credentials, permissions, region, redirect (wrong endpoint) or missing bucket refused | `INTERNAL` | `STORAGE_REJECTED` |
| Other error status | per status (`503` is `UNAVAILABLE`, `429` `RESOURCE_EXHAUSTED`, …) | `STORAGE_HTTP_ERROR` |
| Connection failed | `UNAVAILABLE` | |
| Timeout or the caller's deadline | `DEADLINE_EXCEEDED` | |

Messages are fixed (`the object store responded with HTTP 403`), with the
status in metadata `upstream_status`. The internal source chain carries the
status and the S3 error code (`SignatureDoesNotMatch`) only; the response
body, URLs, keys and credentials never reach an error or a log line.
`Retry-After` is honoured.

Every server-side call is idempotent and runs under the policy: by default
named `storage`, with `TIMEOUT` per attempt and `RetryPolicy::default()`.
The call context's deadline caps each attempt.

## Security rules

- A presigned URL is a bearer credential for one object until it expires.
  Return it only to the user it is for, keep lifetimes short (minutes),
  and never log it. `PresignedRequest`'s `Debug` omits the query.
- Authorize before presigning: the service decides who may read or write
  which key; the store only checks the signature.
- Do not trust what the browser says about an upload. `head` it, compare
  size and type with what you allowed, and delete mismatches.
- Serve user content as a download (`attachment`) or with a safe
  `response_content_type`; never let user-uploaded HTML render on your
  origin.

## Testing

Unit tests presign with a `ManualClock` and compare URLs exactly. Against a
real store use `sekvent_testing::MinioHarness` (feature `minio`): one MinIO
container per test binary and a fresh bucket per test, whose
`TestBucket::config(prefix)` yields the keys above.

```rust
use sekvent::config::MapSource;
use sekvent::storage::{Storage, StorageConfig};
use sekvent_testing::{MinioHarness, docker_tests_enabled};

#[tokio::test]
#[ignore = "needs Docker; set SEKVENT_DOCKER_TESTS=1 and pass --ignored"]
async fn uploads_land_in_the_bucket() {
    if !docker_tests_enabled() {
        return;
    }
    let bucket = MinioHarness::shared().await.create_bucket().await.unwrap();
    let source: MapSource = bucket.config("FILES_").into_iter().collect();
    let storage = Storage::new(StorageConfig::load(&source, "FILES_").unwrap()).unwrap();
    // presign, upload with reqwest (sekvent::client::reqwest_builder), head, …
}
```

To prove expiry without sleeping, build a second `Storage` whose clock is
an hour in the past, presign with a one-minute lifetime and expect `403`.

## See also

- [Design note](../design/storage.md): why an own SigV4 signer, the two
  endpoints, the MinIO image
- [Configuration](config.md), [errors](error.md), [resilience](resilience.md)
- [Runtime](runtime.md) for dependency probes
- [Testing](testing.md) for `MinioHarness`
