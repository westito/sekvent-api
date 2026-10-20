# Object storage (`sekvent-storage`)

> Status: implemented. The user guide is
> [modules/storage.md](../modules/storage.md); the test container is in
> [modules/testing.md](../modules/testing.md).

## 1. Problem

Applications need to store files that users upload (attachments, images,
exports) and to hand them back. Passing file bytes through the API server
costs memory, bandwidth and request time, and makes the server's body
limits the upload limit. S3-compatible object storage solves the transport
side: the server signs a short-lived URL, the browser talks to the store
directly, and the server keeps only the object key and the metadata it
trusts.

What is missing is a small, auditable way to sign those URLs and to do the
few server-side calls an upload flow needs (check what arrived, delete,
copy), without pulling a second TLS stack into the graph.

## 2. Scope

### In

- `StorageConfig`: endpoint, optional public endpoint for presigned URLs,
  region, bucket, credentials (`Secret`), path-style addressing, timeout.
- `ObjectKey`: a validated key (length, no control characters, no empty,
  `.` or `..` segments, no leading `/`).
- Presigned `PUT` (content type always signed, exact length optionally
  signed) and presigned `GET` (optional `response-content-disposition` with
  a safely encoded file name, and `response-content-type`).
- Server-side `HEAD` (size, content type, ETag, last modified; a missing
  object is `None`), `DELETE`, `COPY` and a bucket check.
- A readiness probe for `sekvent-runtime` (feature `runtime`).
- `MinioHarness` in `sekvent-testing` (feature `minio`): one MinIO
  container per test binary, a fresh bucket per test.

### Out

- Streaming uploads or downloads through the server, multipart uploads,
  listing, bucket management, lifecycle rules, server-side encryption
  settings, presigned `POST` policies. They can be added when an
  application needs them.
- Credential providers (instance metadata, web identity, profiles).
  Credentials come from configuration; an optional session token covers
  short-lived keys injected by the platform.

## 3. Decisions

### 3.1 Own SigV4 signer instead of the AWS SDK

The workspace runs one TLS stack: rustls with the ring provider. The
current `aws-sdk-s3` default HTTP client is built on rustls with
`aws-lc-rs`; the only ring-based client it still offers is the legacy
hyper 0.14 / rustls 0.21 stack, kept for compatibility and slated to be
retired. Using the SDK would therefore mean either a second crypto
provider (and a C build) or a deprecated second HTTP stack, plus a large
dependency tree for four operations.

AWS Signature Version 4 is small and fully specified. `sekvent-storage`
implements it with `hmac` and `sha2` (already pinned for `sekvent-sso`)
and sends requests with `reqwest` from `sekvent_client::reqwest_builder`,
so TLS is the workspace's rustls/ring setup. The signer is checked against
the published S3 examples (the presigned `GET` of `test.txt` and the
header-signed `GET`, `PUT`, lifecycle and list examples, all with the
documented example credentials).

### 3.2 Time is injected

Presigning reads `X-Amz-Date` from an injected `sekvent_context::Clock`
(`SystemClock` by default). Tests presign at a fixed instant to compare
against the published vectors, and presign in the past to prove that an
expired URL is rejected, without sleeping.

### 3.3 What the browser must send

A presigned `PUT` signs `host` and `content-type`, and `content-length`
when the application passes the exact size. The browser must send the
content type it was given; browsers set `Content-Length` from the body, so
a body of another size fails the signature. S3 cannot bound a presigned
`PUT` by a maximum size, so the upload flow is: presign with the exact
size the client announced (or none), let the browser upload, then `HEAD`
the object on the server, check size and type, and `DELETE` it when it
does not match.

### 3.4 Two endpoints

The server often reaches the store at a different address than browsers
do (MinIO in a Docker network as `http://minio:9000`, published to
browsers as `https://files.example.com`). SigV4 signs the `Host` header,
so presigned URLs are signed for `PUBLIC_ENDPOINT` and server-side calls
for `ENDPOINT`.

### 3.5 Raw reqwest, not `HttpClient`

`HttpClient` decompresses responses, and the decompression layer drops
`Content-Length` from a response whose `Content-Encoding` is `gzip`. An
object uploaded with `Content-Encoding: gzip` would then report no size
on `HEAD`, which breaks the size check above. The storage client builds
its own `reqwest` client without decompression or redirects and wraps
each call in a resilience `Policy` (per-attempt timeout plus the default
retries; every storage call is idempotent). Status mapping reuses
`sekvent_client::code_for_status`, with `401`/`403` mapped to `INTERNAL`:
the store refusing the service's credentials is a configuration problem.

### 3.6 Errors and logging

Messages are fixed (`the object store responded with HTTP 403`) and carry
no URL, key or bucket. The S3 error code from an XML error body
(`SignatureDoesNotMatch`, `NoSuchBucket`) is kept in the internal source
chain only, and only when it is a short ASCII identifier; the body is
never kept. Presigned URLs are bearer credentials for their lifetime:
`PresignedRequest`'s `Debug` prints method, host and path, never the
query.

### 3.7 MinIO image

MinIO stopped publishing community images in October 2025 (the
`minio/minio` and `quay.io/minio/minio` repositories no longer serve new
tags) and the upstream repository is no longer maintained. The harness
defaults to `pgsty/minio`, a maintained community build of the same
server (it ships `mc`), pinned to a release tag. `SEKVENT_TEST_MINIO_IMAGE`
overrides it with any image that runs `server <dir>` and carries `mc`.
Buckets are created with `mc mb` executed inside the container, so the
harness needs no S3 client of its own.
