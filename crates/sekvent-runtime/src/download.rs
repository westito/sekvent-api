//! File responses with safe headers.

use std::fmt;
use std::fmt::Write as _;
use std::time::Duration;

use axum::body::Body;
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use http::HeaderValue;
use http::header::{
    CACHE_CONTROL, CONTENT_DISPOSITION, CONTENT_LENGTH, CONTENT_TYPE, X_CONTENT_TYPE_OPTIONS,
};
use sekvent_error::AppError;

const OCTET_STREAM: &str = "application/octet-stream";
/// Longest name [`sanitize_filename`] returns, in UTF-8 bytes.
const MAX_FILENAME_BYTES: usize = 200;
/// Longest extension (after the dot) kept when a name is shortened.
const MAX_EXTENSION_BYTES: usize = 16;
const FALLBACK_FILENAME: &str = "download";

/// Media types a browser may render in place without running scripts.
const INLINE_SAFE: [&str; 7] = [
    "application/pdf",
    "image/png",
    "image/jpeg",
    "image/gif",
    "image/webp",
    "image/avif",
    "text/plain",
];

/// A file sent to the client with safe headers.
///
/// The response is `200 OK` with `Content-Type`, `Content-Disposition`,
/// `Content-Length` (for bytes, or a stream of known length),
/// `Cache-Control` and `X-Content-Type-Options: nosniff`.
///
/// The content type is the one set with [`content_type`](Self::content_type),
/// else sniffed from a bytes body by [`sniff_content_type`], else
/// `application/octet-stream`; text formats (CSV, JSON, plain text) are
/// never guessed and must be set. `inline` is kept only for
/// `application/pdf`, `image/png`, `image/jpeg`, `image/gif`, `image/webp`,
/// `image/avif`, `text/plain`, `audio/*` and `video/*`; anything else is
/// sent as an attachment so a stored HTML or SVG file cannot run in the
/// site's origin.
///
/// `Debug` shows the file name, the content type, the disposition, the
/// cache policy and the kind and length of the body, never its bytes.
pub struct Download {
    body: Payload,
    filename: Option<String>,
    disposition: Disposition,
    content_type: Option<(HeaderValue, String)>,
    cache: CacheControl,
}

enum Payload {
    Bytes(Bytes),
    Stream { body: Body, len: Option<u64> },
}

impl fmt::Debug for Download {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Download")
            .field("body", &self.body)
            .field("filename", &self.filename)
            .field("disposition", &self.disposition)
            .field(
                "content_type",
                &self.content_type.as_ref().map(|(_, essence)| essence),
            )
            .field("cache", &self.cache)
            .finish()
    }
}

impl fmt::Debug for Payload {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Bytes(bytes) => f.debug_struct("Bytes").field("len", &bytes.len()).finish(),
            Self::Stream { .. } => f.write_str("Stream"),
        }
    }
}

impl Download {
    /// A download of `body`, held in memory.
    pub fn bytes(body: impl Into<Bytes>) -> Self {
        Self::with_payload(Payload::Bytes(body.into()))
    }

    /// A streamed body; `len` sets `Content-Length` when known.
    pub fn stream(body: Body, len: Option<u64>) -> Self {
        Self::with_payload(Payload::Stream { body, len })
    }

    fn with_payload(body: Payload) -> Self {
        Self {
            body,
            filename: None,
            disposition: Disposition::Attachment,
            content_type: None,
            cache: CacheControl::default(),
        }
    }

    /// The name offered to the client, passed through [`sanitize_filename`].
    #[must_use]
    pub fn filename(mut self, name: &str) -> Self {
        self.filename = Some(sanitize_filename(name));
        self
    }

    /// `attachment` (the default) or `inline`.
    #[must_use]
    pub fn disposition(mut self, disposition: Disposition) -> Self {
        self.disposition = disposition;
        self
    }

    /// The media type, e.g. `text/csv; charset=utf-8`.
    ///
    /// `INVALID_ARGUMENT` unless `type/subtype[; params]` with token
    /// characters (parameter values may be quoted strings).
    pub fn content_type(mut self, value: &str) -> Result<Self, AppError> {
        let value = value.trim();
        let parsed = media_type_essence(value).zip(HeaderValue::from_str(value).ok());
        let Some((essence, header)) = parsed else {
            return Err(AppError::invalid_argument(
                "content type must be type/subtype[; name=value]",
            ));
        };
        self.content_type = Some((header, essence));
        Ok(self)
    }

    /// The caching policy; `private, no-cache` by default.
    #[must_use]
    pub fn cache(mut self, cache: CacheControl) -> Self {
        self.cache = cache;
        self
    }
}

impl IntoResponse for Download {
    fn into_response(self) -> Response {
        let (content_type, essence) = if let Some(explicit) = self.content_type {
            explicit
        } else {
            let sniffed = match &self.body {
                Payload::Bytes(bytes) => sniff_content_type(bytes, self.filename.as_deref()),
                Payload::Stream { .. } => OCTET_STREAM,
            };
            (HeaderValue::from_static(sniffed), sniffed.to_owned())
        };
        let disposition = match self.disposition {
            Disposition::Inline if !inline_safe(&essence) => {
                tracing::debug!(
                    content_type = %essence,
                    "inline download of an unsafe type sent as attachment"
                );
                Disposition::Attachment
            }
            other => other,
        };
        let (body, len) = match self.body {
            Payload::Bytes(bytes) => {
                let len = HeaderValue::from(bytes.len());
                (Body::from(bytes), Some(len))
            }
            Payload::Stream { body, len } => (body, len.map(HeaderValue::from)),
        };
        let mut response = Response::new(body);
        let headers = response.headers_mut();
        headers.insert(CONTENT_TYPE, content_type);
        headers.insert(
            CONTENT_DISPOSITION,
            content_disposition(disposition, self.filename.as_deref()),
        );
        if let Some(len) = len {
            headers.insert(CONTENT_LENGTH, len);
        }
        headers.insert(CACHE_CONTROL, self.cache.header_value());
        headers.insert(X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
        response
    }
}

/// How the client presents a [`Download`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Disposition {
    /// Save as a file.
    #[default]
    Attachment,
    /// Show in the browser, for types that are safe to render.
    Inline,
}

impl Disposition {
    fn as_str(self) -> &'static str {
        match self {
            Self::Attachment => "attachment",
            Self::Inline => "inline",
        }
    }
}

/// The `Cache-Control` of a [`Download`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum CacheControl {
    /// `no-store`.
    NoStore,
    /// `private, no-cache` for zero (the default), else `private, max-age=<s>`.
    Private(Duration),
    /// `public, max-age=<s>`.
    Public(Duration),
}

impl Default for CacheControl {
    /// `Private(Duration::ZERO)`: `private, no-cache`.
    fn default() -> Self {
        Self::Private(Duration::ZERO)
    }
}

impl CacheControl {
    fn header_value(self) -> HeaderValue {
        match self {
            Self::NoStore => HeaderValue::from_static("no-store"),
            Self::Private(max_age) if max_age.as_secs() == 0 => {
                HeaderValue::from_static("private, no-cache")
            }
            Self::Private(max_age) => max_age_header("private", max_age),
            Self::Public(max_age) => max_age_header("public", max_age),
        }
    }
}

fn max_age_header(scope: &str, max_age: Duration) -> HeaderValue {
    let value = format!("{scope}, max-age={}", max_age.as_secs());
    HeaderValue::try_from(value).unwrap_or(HeaderValue::from_static("no-store"))
}

/// The media type of `head` by magic bytes; `filename` only refines ZIP and
/// OLE containers (office documents).
///
/// Text formats are never guessed: anything without a known signature is
/// `application/octet-stream`.
pub fn sniff_content_type(head: &[u8], filename: Option<&str>) -> &'static str {
    const PNG: &[u8] = &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    const OLE: &[u8] = &[0xD0, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0x1A, 0xE1];
    let extension = filename.and_then(extension_of);
    if head.starts_with(b"%PDF-") {
        "application/pdf"
    } else if head.starts_with(PNG) {
        "image/png"
    } else if head.starts_with(&[0xFF, 0xD8, 0xFF]) {
        "image/jpeg"
    } else if head.starts_with(b"GIF87a") || head.starts_with(b"GIF89a") {
        "image/gif"
    } else if head.len() >= 12 && head.starts_with(b"RIFF") && &head[8..12] == b"WEBP" {
        "image/webp"
    } else if head.len() >= 12 && &head[4..8] == b"ftyp" {
        if &head[8..12] == b"avif" {
            "image/avif"
        } else {
            "video/mp4"
        }
    } else if head.starts_with(b"OggS") {
        "audio/ogg"
    } else if head.starts_with(b"ID3") {
        "audio/mpeg"
    } else if head.starts_with(&[0x1F, 0x8B]) {
        "application/gzip"
    } else if head.starts_with(b"PK\x03\x04") {
        match extension.as_deref() {
            Some("docx") => {
                "application/vnd.openxmlformats-officedocument.wordprocessingml.document"
            }
            Some("xlsx") => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
            Some("pptx") => {
                "application/vnd.openxmlformats-officedocument.presentationml.presentation"
            }
            Some("odt") => "application/vnd.oasis.opendocument.text",
            Some("ods") => "application/vnd.oasis.opendocument.spreadsheet",
            Some("odp") => "application/vnd.oasis.opendocument.presentation",
            _ => "application/zip",
        }
    } else if head.starts_with(OLE) {
        match extension.as_deref() {
            Some("doc") => "application/msword",
            Some("xls") => "application/vnd.ms-excel",
            Some("ppt") => "application/vnd.ms-powerpoint",
            _ => OCTET_STREAM,
        }
    } else {
        OCTET_STREAM
    }
}

/// The lowercase extension of the last path segment of `filename`.
fn extension_of(filename: &str) -> Option<String> {
    let name = filename.rsplit(['/', '\\']).next().unwrap_or(filename);
    let (stem, extension) = name.rsplit_once('.')?;
    (!stem.is_empty() && !extension.is_empty()).then(|| extension.to_ascii_lowercase())
}

/// A name safe to store and to send back.
///
/// Keeps the part after the last `/` or `\`; drops control characters
/// (U+0000–U+001F, U+007F–U+009F) and bidirectional overrides
/// (U+202A–U+202E, U+2066–U+2069); collapses runs of whitespace into one
/// space; trims spaces and dots at both ends; caps the result at 200 UTF-8
/// bytes on a character boundary, keeping an extension of up to 16 bytes.
/// An empty result, `.` or `..` becomes `download`.
pub fn sanitize_filename(name: &str) -> String {
    let name = name.rsplit(['/', '\\']).next().unwrap_or(name);
    let mut cleaned = String::with_capacity(name.len());
    let mut pending_space = false;
    for ch in name.chars().filter(|&ch| !is_dropped(ch)) {
        if ch.is_whitespace() {
            pending_space = true;
            continue;
        }
        if pending_space && !cleaned.is_empty() {
            cleaned.push(' ');
        }
        pending_space = false;
        cleaned.push(ch);
    }
    let trimmed = cap_length(trim_name(&cleaned));
    let trimmed = trim_name(&trimmed);
    if trimmed.is_empty() {
        FALLBACK_FILENAME.to_owned()
    } else {
        trimmed.to_owned()
    }
}

fn is_dropped(ch: char) -> bool {
    matches!(
        ch,
        '\u{0000}'..='\u{001F}'
            | '\u{007F}'..='\u{009F}'
            | '\u{202A}'..='\u{202E}'
            | '\u{2066}'..='\u{2069}'
    )
}

fn trim_name(name: &str) -> &str {
    name.trim_matches([' ', '.'])
}

/// `name` within [`MAX_FILENAME_BYTES`], keeping a short extension.
fn cap_length(name: &str) -> String {
    if name.len() <= MAX_FILENAME_BYTES {
        return name.to_owned();
    }
    let extension = name
        .rfind('.')
        .filter(|&dot| dot > 0 && name.len() - dot - 1 <= MAX_EXTENSION_BYTES)
        .map_or("", |dot| &name[dot..]);
    let stem = &name[..name.len() - extension.len()];
    let mut cut = MAX_FILENAME_BYTES - extension.len();
    while !stem.is_char_boundary(cut) {
        cut -= 1;
    }
    let stem = stem[..cut].trim_end_matches([' ', '.']);
    format!("{stem}{extension}")
}

/// `attachment; filename="…"; filename*=UTF-8''…` (RFC 6266 and 8187).
///
/// The name goes through [`sanitize_filename`]. The quoted `filename` is an
/// ASCII fallback with non-ASCII characters, `"`, `\` and `%` replaced by
/// `_`; `filename*` carries the UTF-8 name, percent-encoded, and is added
/// only when it differs from the fallback. Without a name the value is the
/// bare disposition.
pub fn content_disposition(disposition: Disposition, filename: Option<&str>) -> HeaderValue {
    let Some(filename) = filename else {
        return HeaderValue::from_static(disposition.as_str());
    };
    let name = sanitize_filename(filename);
    let fallback: String = name
        .chars()
        .map(|ch| {
            if ch.is_ascii() && !matches!(ch, '"' | '\\' | '%') {
                ch
            } else {
                '_'
            }
        })
        .collect();
    let mut value = format!("{}; filename=\"{fallback}\"", disposition.as_str());
    if fallback != name {
        value.push_str("; filename*=UTF-8''");
        for byte in name.bytes() {
            if is_attr_char(byte) {
                value.push(char::from(byte));
            } else {
                let _ = write!(value, "%{byte:02X}");
            }
        }
    }
    HeaderValue::try_from(value).unwrap_or(HeaderValue::from_static("attachment"))
}

/// RFC 8187 `attr-char`.
fn is_attr_char(byte: u8) -> bool {
    byte.is_ascii_alphanumeric()
        || matches!(
            byte,
            b'!' | b'#' | b'$' | b'&' | b'+' | b'-' | b'.' | b'^' | b'_' | b'`' | b'|' | b'~'
        )
}

fn inline_safe(essence: &str) -> bool {
    INLINE_SAFE.contains(&essence) || essence.starts_with("audio/") || essence.starts_with("video/")
}

/// The lowercase `type/subtype` of a valid media type, `None` otherwise.
fn media_type_essence(value: &str) -> Option<String> {
    let bytes = value.as_bytes();
    let mut pos = 0;
    let kind = token(bytes, &mut pos)?;
    expect(bytes, &mut pos, b'/')?;
    let subtype = token(bytes, &mut pos)?;
    loop {
        skip_whitespace(bytes, &mut pos);
        if pos == bytes.len() {
            break;
        }
        expect(bytes, &mut pos, b';')?;
        skip_whitespace(bytes, &mut pos);
        token(bytes, &mut pos)?;
        expect(bytes, &mut pos, b'=')?;
        if bytes.get(pos) == Some(&b'"') {
            quoted_string(bytes, &mut pos)?;
        } else {
            token(bytes, &mut pos)?;
        }
    }
    Some(format!("{kind}/{subtype}").to_ascii_lowercase())
}

/// RFC 9110 `tchar`.
fn is_token_char(byte: u8) -> bool {
    byte.is_ascii_alphanumeric()
        || matches!(
            byte,
            b'!' | b'#'
                | b'$'
                | b'%'
                | b'&'
                | b'\''
                | b'*'
                | b'+'
                | b'-'
                | b'.'
                | b'^'
                | b'_'
                | b'`'
                | b'|'
                | b'~'
        )
}

fn token<'a>(bytes: &'a [u8], pos: &mut usize) -> Option<&'a str> {
    let start = *pos;
    while bytes.get(*pos).is_some_and(|&byte| is_token_char(byte)) {
        *pos += 1;
    }
    if *pos == start {
        return None;
    }
    std::str::from_utf8(&bytes[start..*pos]).ok()
}

fn expect(bytes: &[u8], pos: &mut usize, wanted: u8) -> Option<()> {
    if bytes.get(*pos) == Some(&wanted) {
        *pos += 1;
        Some(())
    } else {
        None
    }
}

fn skip_whitespace(bytes: &[u8], pos: &mut usize) {
    while matches!(bytes.get(*pos), Some(b' ' | b'\t')) {
        *pos += 1;
    }
}

/// A quoted string at `pos` (which holds the opening quote).
fn quoted_string(bytes: &[u8], pos: &mut usize) -> Option<()> {
    *pos += 1;
    loop {
        match *bytes.get(*pos)? {
            b'"' => {
                *pos += 1;
                return Some(());
            }
            b'\\' => {
                let escaped = *bytes.get(*pos + 1)?;
                if !is_quoted_char(escaped) {
                    return None;
                }
                *pos += 2;
            }
            byte if is_quoted_char(byte) => *pos += 1,
            _ => return None,
        }
    }
}

/// Visible ASCII, space and tab.
fn is_quoted_char(byte: u8) -> bool {
    byte == b'\t' || (b' '..=b'~').contains(&byte)
}

#[cfg(test)]
mod tests {
    use axum::body::to_bytes;
    use sekvent_error::ErrorCode;

    use super::*;

    fn header(response: &Response, name: http::header::HeaderName) -> Option<&str> {
        response
            .headers()
            .get(name)
            .map(|value| value.to_str().unwrap())
    }

    fn disposition(value: Option<&str>) -> String {
        content_disposition(Disposition::Attachment, value)
            .to_str()
            .unwrap()
            .to_owned()
    }

    #[tokio::test]
    async fn every_header_for_a_bytes_body() {
        let response = Download::bytes(&b"%PDF-1.7 rest"[..])
            .filename("report.pdf")
            .into_response();
        assert_eq!(response.status(), http::StatusCode::OK);
        assert_eq!(header(&response, CONTENT_TYPE), Some("application/pdf"));
        assert_eq!(
            header(&response, CONTENT_DISPOSITION),
            Some("attachment; filename=\"report.pdf\"")
        );
        assert_eq!(header(&response, CONTENT_LENGTH), Some("13"));
        assert_eq!(header(&response, CACHE_CONTROL), Some("private, no-cache"));
        assert_eq!(header(&response, X_CONTENT_TYPE_OPTIONS), Some("nosniff"));
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        assert_eq!(&body[..], b"%PDF-1.7 rest");
    }

    #[tokio::test]
    async fn streams_with_and_without_a_length() {
        let response = Download::stream(Body::from("abc"), Some(3))
            .content_type("text/csv; charset=utf-8")
            .unwrap()
            .into_response();
        assert_eq!(header(&response, CONTENT_LENGTH), Some("3"));
        assert_eq!(
            header(&response, CONTENT_TYPE),
            Some("text/csv; charset=utf-8")
        );
        assert_eq!(header(&response, CONTENT_DISPOSITION), Some("attachment"));
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        assert_eq!(&body[..], b"abc");

        let response = Download::stream(Body::from("%PDF-"), None).into_response();
        assert_eq!(header(&response, CONTENT_LENGTH), None);
        // Streams are never sniffed.
        assert_eq!(header(&response, CONTENT_TYPE), Some(OCTET_STREAM));
    }

    #[test]
    fn explicit_type_wins_over_sniffing() {
        let response = Download::bytes(&b"\x89PNG\r\n\x1a\nrest"[..])
            .content_type("application/x-custom")
            .unwrap()
            .into_response();
        assert_eq!(
            header(&response, CONTENT_TYPE),
            Some("application/x-custom")
        );
    }

    #[test]
    fn text_is_never_sniffed() {
        for body in [&b"a,b,c\n1,2,3"[..], b"{\"a\":1}", b"hello", b"<html>", b""] {
            let response = Download::bytes(body.to_vec()).into_response();
            assert_eq!(header(&response, CONTENT_TYPE), Some(OCTET_STREAM));
        }
    }

    #[test]
    fn cache_control_values() {
        for (cache, expected) in [
            (CacheControl::NoStore, "no-store"),
            (CacheControl::Private(Duration::ZERO), "private, no-cache"),
            (
                CacheControl::Private(Duration::from_secs(60)),
                "private, max-age=60",
            ),
            (
                CacheControl::Public(Duration::from_secs(3600)),
                "public, max-age=3600",
            ),
            (CacheControl::Public(Duration::ZERO), "public, max-age=0"),
        ] {
            let response = Download::bytes("x").cache(cache).into_response();
            assert_eq!(
                header(&response, CACHE_CONTROL),
                Some(expected),
                "{cache:?}"
            );
        }
        assert_eq!(
            CacheControl::default(),
            CacheControl::Private(Duration::ZERO)
        );
    }

    #[test]
    fn inline_is_kept_for_safe_types() {
        for (body, content_type) in [
            (&b"%PDF-1.4"[..], None),
            (b"\xFF\xD8\xFF\xE0", None),
            (b"x", Some("text/plain; charset=utf-8")),
            (b"x", Some("audio/wav")),
            (b"x", Some("video/webm")),
            (b"x", Some("IMAGE/PNG")),
        ] {
            let mut download = Download::bytes(body.to_vec())
                .filename("file")
                .disposition(Disposition::Inline);
            if let Some(content_type) = content_type {
                download = download.content_type(content_type).unwrap();
            }
            let response = download.into_response();
            assert_eq!(
                header(&response, CONTENT_DISPOSITION),
                Some("inline; filename=\"file\""),
                "{content_type:?}"
            );
        }
        let response = Download::bytes(&b"%PDF-"[..])
            .disposition(Disposition::Inline)
            .into_response();
        assert_eq!(header(&response, CONTENT_DISPOSITION), Some("inline"));
    }

    #[test]
    fn inline_is_downgraded_for_unsafe_types() {
        for content_type in [
            Some("text/html; charset=utf-8"),
            Some("image/svg+xml"),
            Some("application/xml"),
            Some("text/xml"),
            None,
        ] {
            let mut download = Download::bytes("<svg onload=alert(1)>")
                .filename("x.svg")
                .disposition(Disposition::Inline);
            if let Some(content_type) = content_type {
                download = download.content_type(content_type).unwrap();
            }
            let response = download.into_response();
            assert_eq!(
                header(&response, CONTENT_DISPOSITION),
                Some("attachment; filename=\"x.svg\""),
                "{content_type:?}"
            );
        }
    }

    #[test]
    fn invalid_content_types_are_rejected() {
        for value in [
            "",
            "text",
            "text/",
            "/plain",
            "text/plain;",
            "text/plain; charset",
            "text/plain; charset=",
            "text/plain; =utf-8",
            "text/plain charset=utf-8",
            "text /plain",
            "text/pl ain",
            "text/plain; charset=\"utf-8",
            "text/plain; name=\"a\x01b\"",
            "text/plain; name=\"a\\",
            "text/plain; name=\"a\\\x01\"",
            "text/plain; name=\"é\"",
            "text/plain\r\nx-injected: 1",
            "tèxt/plain",
            "text/plain; a=b;; c=d",
            "text/plain; a=\"b\"c",
        ] {
            let error = Download::bytes("x").content_type(value).unwrap_err();
            assert_eq!(error.code(), ErrorCode::InvalidArgument, "{value:?}");
        }
    }

    #[test]
    fn valid_content_types_are_accepted() {
        for (value, essence) in [
            ("text/csv", "text/csv"),
            ("  Text/CSV  ", "text/csv"),
            ("application/json; charset=utf-8", "application/json"),
            ("text/plain;charset=utf-8", "text/plain"),
            ("text/plain ; charset=utf-8", "text/plain"),
            (
                "multipart/mixed; boundary=\"a; b \\\" c\"; x=y",
                "multipart/mixed",
            ),
            ("application/vnd.ms-excel", "application/vnd.ms-excel"),
        ] {
            assert_eq!(
                media_type_essence(value.trim()).as_deref(),
                Some(essence),
                "{value}"
            );
            assert!(Download::bytes("x").content_type(value).is_ok(), "{value}");
        }
    }

    #[test]
    fn sniff_table() {
        let mp4 = [
            0, 0, 0, 0x18, b'f', b't', b'y', b'p', b'i', b's', b'o', b'm',
        ];
        let avif = [
            0, 0, 0, 0x1C, b'f', b't', b'y', b'p', b'a', b'v', b'i', b'f',
        ];
        for (head, expected) in [
            (&b"%PDF-1.7"[..], "application/pdf"),
            (b"\x89PNG\r\n\x1a\n\0\0", "image/png"),
            (b"\xFF\xD8\xFF\xDB", "image/jpeg"),
            (b"GIF87a..", "image/gif"),
            (b"GIF89a..", "image/gif"),
            (b"RIFF\x10\0\0\0WEBPVP8 ", "image/webp"),
            (b"RIFF\x10\0\0\0WAVEfmt ", OCTET_STREAM),
            (&avif, "image/avif"),
            (&mp4, "video/mp4"),
            (b"OggS\0\x02", "audio/ogg"),
            (b"ID3\x04\0", "audio/mpeg"),
            (b"\x1F\x8B\x08\0", "application/gzip"),
            (b"PK\x03\x04\x14\0", "application/zip"),
            (b"\xD0\xCF\x11\xE0\xA1\xB1\x1A\xE1", OCTET_STREAM),
            (b"\x89PNG", OCTET_STREAM),
            (b"RIFF", OCTET_STREAM),
            (b"\0\0\0\0ftyp", OCTET_STREAM),
            (b"", OCTET_STREAM),
        ] {
            assert_eq!(sniff_content_type(head, None), expected, "{head:?}");
        }
    }

    #[test]
    fn containers_are_refined_by_extension() {
        let zip = b"PK\x03\x04\x14\0";
        let ole = b"\xD0\xCF\x11\xE0\xA1\xB1\x1A\xE1\0";
        for (name, expected) in [
            (
                "a.docx",
                "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
            ),
            (
                "dir/A.XLSX",
                "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
            ),
            (
                "dir\\a.pptx",
                "application/vnd.openxmlformats-officedocument.presentationml.presentation",
            ),
            ("a.odt", "application/vnd.oasis.opendocument.text"),
            ("a.ods", "application/vnd.oasis.opendocument.spreadsheet"),
            ("a.odp", "application/vnd.oasis.opendocument.presentation"),
            ("a.zip", "application/zip"),
            ("docx", "application/zip"),
            (".docx", "application/zip"),
            ("a.", "application/zip"),
        ] {
            assert_eq!(sniff_content_type(zip, Some(name)), expected, "{name}");
        }
        for (name, expected) in [
            ("a.doc", "application/msword"),
            ("a.XLS", "application/vnd.ms-excel"),
            ("a.ppt", "application/vnd.ms-powerpoint"),
            ("a.msg", OCTET_STREAM),
        ] {
            assert_eq!(sniff_content_type(ole, Some(name)), expected, "{name}");
        }
        // The extension never makes a type out of unknown bytes.
        assert_eq!(sniff_content_type(b"plain", Some("a.pdf")), OCTET_STREAM);

        let response = Download::bytes(&zip[..])
            .filename("sheet.xlsx")
            .into_response();
        assert_eq!(
            header(&response, CONTENT_TYPE),
            Some("application/vnd.openxmlformats-officedocument.spreadsheetml.sheet")
        );
    }

    #[test]
    fn sanitize_rules() {
        for (input, expected) in [
            ("report.pdf", "report.pdf"),
            ("../../etc/passwd", "passwd"),
            ("C:\\Users\\a\\file.txt", "file.txt"),
            ("dir/", "download"),
            ("", "download"),
            (".", "download"),
            ("..", "download"),
            ("...", "download"),
            ("  .hidden.  ", "hidden"),
            ("a\u{0}b\u{1F}c\u{7F}d\u{9F}e.txt", "abcde.txt"),
            ("evil\u{202E}fdp.exe", "evilfdp.exe"),
            ("a\u{2066}b\u{2069}c", "abc"),
            ("a \t\n  b\u{00A0}\u{2003}c", "a b c"),
            ("line\r\nbreak", "linebreak"),
            ("Árvíztűrő tükörfúrógép.pdf", "Árvíztűrő tükörfúrógép.pdf"),
        ] {
            assert_eq!(sanitize_filename(input), expected, "{input:?}");
        }
    }

    #[test]
    fn long_names_keep_their_extension() {
        let long = format!("{}.pdf", "a".repeat(300));
        let capped = sanitize_filename(&long);
        assert_eq!(capped.len(), MAX_FILENAME_BYTES);
        assert!(capped.ends_with("aaa.pdf"));

        // A multi-byte character is never split.
        let wide = format!("{}.xlsx", "é".repeat(150));
        let capped = sanitize_filename(&wide);
        assert!(capped.len() <= MAX_FILENAME_BYTES);
        assert!(capped.ends_with("é.xlsx"), "{capped}");

        // An extension longer than 16 bytes is not kept.
        let odd = format!("{}.{}", "b".repeat(250), "c".repeat(17));
        let capped = sanitize_filename(&odd);
        assert_eq!(capped.len(), MAX_FILENAME_BYTES);
        assert!(capped.chars().all(|ch| ch == 'b'));

        // A cut that lands on spaces or dots is trimmed.
        let spaced = format!("{}{}.txt", "d".repeat(190), " .".repeat(20));
        let capped = sanitize_filename(&spaced);
        assert_eq!(capped, format!("{}.txt", "d".repeat(190)));

        // Already short names are untouched, and sanitizing is idempotent.
        for name in [&long, &wide, &odd, &spaced] {
            let once = sanitize_filename(name);
            assert_eq!(sanitize_filename(&once), once);
        }
    }

    #[test]
    fn content_disposition_table() {
        for (input, expected) in [
            (None, "attachment".to_owned()),
            (
                Some("plain.txt"),
                "attachment; filename=\"plain.txt\"".to_owned(),
            ),
            (
                Some("say \"hi\".txt"),
                "attachment; filename=\"say _hi_.txt\"; filename*=UTF-8''say%20%22hi%22.txt"
                    .to_owned(),
            ),
            (
                Some("back\\slash.txt"),
                "attachment; filename=\"slash.txt\"".to_owned(),
            ),
            (
                Some("100%.txt"),
                "attachment; filename=\"100_.txt\"; filename*=UTF-8''100%25.txt".to_owned(),
            ),
            (
                Some("a/b/c.txt"),
                "attachment; filename=\"c.txt\"".to_owned(),
            ),
            (
                Some("x\u{0}y\u{202E}z.txt"),
                "attachment; filename=\"xyz.txt\"".to_owned(),
            ),
            (
                Some("résumé.pdf"),
                "attachment; filename=\"r_sum_.pdf\"; filename*=UTF-8''r%C3%A9sum%C3%A9.pdf"
                    .to_owned(),
            ),
            (Some(""), "attachment; filename=\"download\"".to_owned()),
            (
                Some("a!#$&+-.^_`|~b.txt"),
                "attachment; filename=\"a!#$&+-.^_`|~b.txt\"".to_owned(),
            ),
            (
                Some("semi;colon,comma.txt"),
                "attachment; filename=\"semi;colon,comma.txt\"".to_owned(),
            ),
        ] {
            assert_eq!(disposition(input), expected, "{input:?}");
        }
        let long = format!("{}.pdf", "ü".repeat(150));
        let value = disposition(Some(&long));
        assert!(value.contains(".pdf\"; filename*=UTF-8''%C3%BC"), "{value}");
        assert!(value.ends_with("%C3%BC.pdf"), "{value}");
        assert_eq!(
            content_disposition(Disposition::Inline, None),
            HeaderValue::from_static("inline")
        );
        assert_eq!(
            content_disposition(Disposition::Inline, Some("a b.png")),
            HeaderValue::from_static("inline; filename=\"a b.png\"")
        );
    }

    #[test]
    fn non_ascii_filename_on_a_response() {
        let response = Download::bytes("x")
            .filename("../отчёт 2026.csv")
            .content_type("text/csv")
            .unwrap()
            .into_response();
        assert_eq!(
            header(&response, CONTENT_DISPOSITION),
            Some(
                "attachment; filename=\"_____ 2026.csv\"; \
                 filename*=UTF-8''%D0%BE%D1%82%D1%87%D1%91%D1%82%202026.csv"
            )
        );
    }

    #[test]
    fn debug_output() {
        let download = Download::bytes("top-secret-body")
            .filename("a.txt")
            .content_type("text/plain; charset=utf-8")
            .unwrap();
        let debug = format!("{download:?}");
        assert!(!debug.contains("top-secret-body"), "{debug}");
        for part in [
            "Download",
            "Bytes { len: 15 }",
            "\"a.txt\"",
            "Attachment",
            "Some(\"text/plain\")",
            "Private(0ns)",
        ] {
            assert!(debug.contains(part), "{part}: {debug}");
        }
        let stream = format!(
            "{:?}",
            Download::stream(Body::from("streamed-secret"), Some(15))
        );
        assert!(stream.contains("body: Stream,"), "{stream}");
        assert!(stream.contains("content_type: None"), "{stream}");
        assert!(!stream.contains("streamed-secret"), "{stream}");
        assert_eq!(Disposition::default(), Disposition::Attachment);
    }
}
