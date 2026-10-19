# Serving a built frontend (`sekvent-runtime` `vfs`)

> Status: implemented. Builds on
> [`p8-service-essentials.md`](p8-service-essentials.md) (the server, its
> prefix, CORS, layers and `Download`). The user guide is
> [modules/server.md: Serve a bundled frontend](../modules/server.md#serve-a-bundled-frontend).

## 1. Problem

A service with a browser frontend (React, Vue or Svelte built with Vite)
wants one deployable: the API and the built frontend from the same binary
and the same listener. Without framework support every project hand-rolls
`tower-http`'s `ServeDir` plus a fallback to `index.html`, and the usual
mistakes follow: the SPA fallback swallows unknown API paths (a typo in an
API URL answers `200` with HTML), a missing `/assets/app.js` answers the
index (the browser then reports a MIME error instead of a `404`), hashed
assets are not cached or `index.html` is cached forever, the authenticator
or an auth layer rejects the anonymous page load, and paths are joined onto
a directory without normalization.

## 2. Scope

### In

- `Vfs`: an object-safe async lookup from a normalized path to a file
  (bytes, strong `ETag`, content type, last modified).
- `VfsPath`: the one path normalizer every implementation and the site use.
- `Directory`: a folder on disk, reads on the blocking pool, confined to
  its canonical root.
- `Embedded<E: RustEmbed>` (feature `vfs-embed`): files compiled into the
  binary; `rust-embed` re-exported so consumers need no dependency.
- `StaticSite`: a tower service with index files, directory redirects,
  SPA fallback, cache policy, conditional requests, precompressed variants
  and configurable extra response headers.
- `ServerBuilder::site`: routing next to the API, health and gRPC.

### Out

- Runtime compression, range requests, directory listings, streaming large
  files (a built frontend is small; `Download` covers files from handlers).
- Serving the development build. Vite's dev server stays the development
  front door (see 5).
- Per-file cache rules beyond one immutable prefix.

## 3. Decisions

### 3.1 Features and placement

The code lives in `sekvent-runtime`, module `vfs`, behind `vfs`
(`mime_guess`, `sha2`, `percent-encoding`, `httpdate`) and `vfs-embed`
(adds `rust-embed`). The facade exposes them as `runtime-vfs` and
`runtime-vfs-embed`. A service without a frontend compiles none of it.

`rust-embed` is re-exported as `sekvent::runtime::vfs::rust_embed` together
with its derive. The derive's `#[crate_path = "…"]` attribute (supported
by rust-embed 8) points the generated code at the re-export, so a consumer
writes:

```rust
#[derive(sekvent::runtime::vfs::RustEmbed)]
#[crate_path = "sekvent::runtime::vfs::rust_embed"]
#[folder = "../../web/dist"]
struct Ui;
```

`$CARGO_MANIFEST_DIR` interpolation needs rust-embed's
`interpolate-folder-path` feature, which sekvent does not turn on;
`#[folder]` is relative to the crate's manifest anyway.

rust-embed reads the folder from disk in debug builds and returns
compiled-in bytes in release builds, behind the same `get`. `Embedded`
moves the lookup to the blocking pool when `cfg!(debug_assertions)` holds
and answers inline otherwise. Both branches are one function compiled in
every profile, and the tests call it both ways, so the release branch is
type-checked and covered by a debug gate.

### 3.2 One normalizer

`VfsPath` has no public constructor other than `from_url_path` (percent-
decodes each segment once, requires UTF-8) and `new` (for configuration
such as the index name). Both split on `/`, drop empty segments, and
reject a segment that is longer than 255 bytes (the file name limit of
common file systems, so an overlong name is a clean miss rather than an
I/O error), contains a control character, `/` (only possible after
decoding `%2F`), `\`, or `:`, or that starts with `.` (covering `.`, `..`
and hidden files such as `.env` and `.git`), with `.well-known` as the one
exception. Double encoding stays literal (`%252e` is the name
`%2e`). Because `Vfs::get` takes a `VfsPath`, an implementation cannot be
handed an unnormalized path.

Rejecting `:` blocks drive letters and NTFS alternate data streams;
rejecting `\` blocks Windows separators. Rejecting rather than cleaning
keeps the rules simple: a browser never produces these paths for a real
asset.

### 3.3 Directory confinement

`Directory::new` canonicalizes the root and fails (`INVALID_ARGUMENT`)
unless it is an existing directory. A lookup joins the normalized segments
onto the root, canonicalizes the result and requires it to start with the
canonical root; a symbolic link that leads outside is treated as missing.
The name check is repeated on the resolved path: a component below the
root that starts with `.` (other than `.well-known`) is missing too, which
covers a symbolic link inside the root pointing at `.env` and alternative
names such as NTFS short names. Directories are not files. The whole
lookup (canonicalize, metadata, read, hash) runs in `spawn_blocking`. A
missing file, a file used as a directory, or a name the file system cannot
hold (`InvalidFilename`, `InvalidInput`: too long, say) is `None`; any
other I/O error is `INTERNAL`, so an anonymous request cannot provoke a
`500` with an odd name.

`Directory` reads and hashes the file on every request. It is the simple
path for a folder in a container image; `Embedded` is the production path.
Files above `max_file_size` (default 64 MiB) are not served: the size is
checked from the metadata and again while reading (the file may grow), and
the file counts as missing. The first refusal is logged at `warn` with the
normalized path, later ones at `debug`, so repeated requests cannot flood
the log. Refusing with `500` was rejected: the request is not the server's
fault, and a missing file is what the site can explain.

### 3.4 ETags

Strong, from SHA-256 of the content: rust-embed computes it at build time
for embedded files; `Directory` hashes on read. The tag is the first 16
bytes in hex, quoted. Precompressed variants have their own bytes and so
their own tag, as RFC 9110 requires for a different representation.

### 3.5 `StaticSite` behaviour

- `GET` and `HEAD` only, otherwise `405` with `Allow: GET, HEAD`.
- `/` and paths ending in `/` serve `<dir>/index.html`. A path that is not
  a file but has `<path>/index.html` answers `308` to `<path>/`, query
  kept, so relative URLs in that page resolve against the directory (the
  root never redirects). The `Location` is built from the normalized path,
  percent-encoded, so `//evil.example` cannot become a protocol-relative
  redirect.
- SPA fallback, on by default: a navigation, meaning `Accept` lists
  `text/html` itself with a non-zero weight (`*/*` and `text/*` do not
  count), to a missing path serves the root index with `200`. Whether the
  last segment has a dot does not matter: client routes of an operations
  UI carry IP addresses, OCIDs, host names and versions. Paths the
  normalizer rejects fall back as well, since the index is safe to serve
  for any URL. The exception is the immutable prefix: a missing path below
  it is always `404` (for rejected paths the prefix is compared against the
  leniently decoded raw segments), so a stale tab after a deploy gets a
  real error for its old chunk instead of HTML. Requiring `text/html` keeps
  `fetch` and API-style clients on a real `404`.
- `Cache-Control`: `no-cache` for HTML (so a deploy is picked up at once,
  revalidated by `ETag`), `public, max-age=31536000, immutable` under the
  immutable prefix (default `assets`, Vite's `assetsDir`, whose names carry
  a content hash), `no-cache` for everything else (`favicon.ico`,
  `robots.txt`, files from Vite's `public/` keep their names across
  deploys).
- `If-None-Match` with a list, `W/` tags (weak comparison per RFC 9110) or
  `*` answers `304` with the validators and cache headers, no body. The
  empty body announces no size, because axum turns an exact size hint into
  `Content-Length` and a `304` may only repeat the `200`'s length (RFC 9110
  8.6). `HEAD` carries the `GET` length.
  `If-Modified-Since` is ignored: the `ETag` is always present and
  stronger.
- Precompressed variants: `Accept-Encoding` is parsed with weights; `q=0`
  refuses, `*` covers unlisted codings, a coding listed twice takes its
  lowest weight, `br` wins a tie with `gzip`. The original is looked up
  first: a variant is only a representation of an existing file, and its
  content type is the original's (a `with_content_type` override
  included), never a guess from the `.br`/`.gz` name. A variant whose
  lookup fails is skipped (logged at `debug`) and the next one or the
  original is sent; only a failing original is a `500`. `Vary:
  Accept-Encoding` is on every response (also `304`, `308`, `404`, `405`
  and `500`) because whether a variant exists is not known to caches;
  `Accept` joins it whenever the SPA fallback decided the answer, and on a
  `500` while the fallback is on.
- `X-Content-Type-Options: nosniff` everywhere; `Content-Length` on files;
  `Last-Modified` when known (informational).
- Extra headers: `StaticSiteBuilder::header(HeaderName, HeaderValue)` adds
  a header to every site response, replacing one of the same name the site
  would set; `without_header` drops one. Typed arguments keep the builder
  infallible. The default set is `X-Frame-Options: DENY` (an admin UI has
  no reason to be framed). A default `Content-Security-Policy` was
  rejected: a generic policy breaks real apps, and a second CSP header
  from the service would combine with ours rather than replace it.
- A lookup error is `500` through `AppError`'s HTTP mapping, logged
  server-side, never echoed.

### 3.6 Server routing

`ServerBuilder::site(site)` stores the site; `bind` and `from_listener`
fail with `INVALID_ARGUMENT` when a site is set without a prefix, and a
second `site(..)` is recorded in the builder's `problems`. A prefix is
required because the SPA fallback answers any browser navigation: at the
root it would shadow every API route that does not match exactly.

In `Server::router` the prefix is nested as before; the root fallback is
the same content-type `Dispatch` used under the prefix, with gRPC (when
`grpc_at_root`) on one side and the site on the other. Requests under the
prefix never reach the site, so an unknown API path stays the REST `404`.
Health endpoints are matched before the dispatch, so `/livez`, `/readyz`
and `/healthz` stay health.

The site sits where the REST routes sit: inside the request id, access
log, CORS, call context and application layers. The server's
`Authenticator` never rejects (it only feeds the call context), so the
site is public by construction. Component end-user authentication is per
component and does not touch the site. `ServerBuilder::layer` wraps the
site too, so a layer that rejects anonymous callers would reject the page
load (and the login page); the `layer` docs and the module guide say to
put such a layer on the API router (`api_routes().layer(..)` before
`.rest(..)`).

## 4. Rejected alternatives

- **Runtime compression** (`tower-http` `CompressionLayer`): spends CPU on
  every response for content that never changes, and needs care with
  `ETag` and `Content-Length`. Precompressing at build time
  (`vite-plugin-compression`) is free at runtime.
- **Serving development through the backend** (the backend proxying to
  Vite, or serving `web/` with a file watcher): duplicates Vite's dev
  server, breaks hot module replacement over websockets, and couples the
  backend to Node tooling. Vite's `server.proxy` sending `/api` to the
  backend gives one origin with no CORS and keeps both tools doing what
  they do best.
- **`tower-http` `ServeDir`**: no embedded source, falls back on any
  missing path (or none), no immutable-prefix policy, its own path
  handling; wrapping it would still need everything above.
- **SPA fallback at the root without a prefix**: shadows API routes;
  rejected by `bind`/`from_listener` with `INVALID_ARGUMENT` instead.
- **Deciding the fallback by extension** (no fallback when the last
  segment has a dot): breaks reloads of client routes such as
  `/nodes/10.0.0.1` or `/releases/v1.2.3`. Missing chunks are protected by
  the immutable prefix instead, which is where Vite puts them.
- **Serving a directory index at the slash-less URL**: relative links in
  that page would resolve against the parent directory.
- **rust-embed's `#[allow_missing = true]` in the recommended setup**: a
  release built before the frontend would embed nothing and fail only at
  runtime. The guide says the `embed-ui` feature needs `web/dist` instead.
- **`include_dir`**: no build-time hashes and no debug-mode disk reads;
  rust-embed has both.

## 5. Development and production workflow

- Development: `cargo run` without the consumer's `embed-ui` feature, and
  `npm run dev` in `web/` with
  `server.proxy: { "^/api([/?]|$)": "http://127.0.0.1:8080" }` (a key
  starting with `^` is a regular expression; a plain `"/api"` would also
  proxy `/api-keys`). The browser talks to Vite only.
- Production: `vite build` writes `web/dist`; `cargo build --release
  --features embed-ui` embeds it; the binary serves API and frontend on one
  port. Keeping the derive behind the consumer's feature keeps
  `cargo check` and the backend tests independent of a frontend build; the
  derive fails to compile without `web/dist`, so `--all-features` runs
  need a built frontend or an explicit feature list.
- Alternative: `Directory::new("/srv/web/dist")` serves a folder copied
  into the image next to the binary.
