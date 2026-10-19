//! Static files from a virtual filesystem, and [`StaticSite`] to serve a
//! built frontend (a single-page app) from the same [`Server`](crate::Server)
//! as the API.
//!
//! A [`Vfs`] maps a [`VfsPath`] to a [`VfsFile`]. Two implementations ship:
//! [`Directory`] reads a folder on disk, and `Embedded` (feature
//! `vfs-embed`) serves files compiled into the binary with `rust-embed`.
//! Every path reaching a [`Vfs`] went through the one normalizer behind
//! [`VfsPath`], so an implementation never sees `..`, separators inside a
//! segment or a drive letter.
//!
//! With `vfs-embed` the `rust-embed` crate is re-exported here, so a
//! service needs no dependency of its own:
//!
//! ```ignore
//! #[derive(sekvent::runtime::vfs::RustEmbed)]
//! #[crate_path = "sekvent::runtime::vfs::rust_embed"]
//! #[folder = "../../web/dist"]
//! struct Ui;
//!
//! let site = StaticSite::new(Arc::new(Embedded::<Ui>::new()));
//! ```

use std::borrow::Cow;
use std::fmt;
use std::fmt::Write as _;
use std::time::SystemTime;

use bytes::Bytes;
use futures::future::BoxFuture;
use http::HeaderValue;
use percent_encoding::percent_decode_str;
use sekvent_error::AppError;
use sha2::{Digest, Sha256};

mod directory;
#[cfg(feature = "vfs-embed")]
mod embed;
mod site;

pub use directory::Directory;
#[cfg(feature = "vfs-embed")]
pub use embed::Embedded;
#[cfg(feature = "vfs-embed")]
pub use rust_embed::{self, RustEmbed};
pub use site::{StaticSite, StaticSiteBuilder};

const OCTET_STREAM: &str = "application/octet-stream";

/// The one hidden segment that may be served (RFC 8615).
const WELL_KNOWN: &str = ".well-known";

/// The longest segment, in bytes: the file name limit of common file
/// systems.
const MAX_SEGMENT_BYTES: usize = 255;

/// A source of static files.
///
/// Lookups are asynchronous so an implementation that touches the disk can
/// move the work off the async runtime ([`Directory`] uses tokio's blocking
/// pool).
pub trait Vfs: Send + Sync + 'static {
    /// The file at `path`; `None` when there is none. A directory is not a
    /// file. An error means the lookup itself failed (an unreadable file,
    /// say) and is answered with `500`.
    fn get<'a>(&'a self, path: &'a VfsPath) -> BoxFuture<'a, Result<Option<VfsFile>, AppError>>;
}

/// A normalized relative path inside a [`Vfs`], segments joined with `/`.
///
/// The only ways to build one are [`from_url_path`](Self::from_url_path)
/// and [`new`](Self::new), which share these rules: empty segments
/// collapse (so leading, trailing and doubled slashes vanish); a segment is
/// rejected when it is longer than 255 bytes, holds a control character
/// (NUL included), `/`, `\` or `:` (drive letters, alternate data
/// streams), or starts with `.` (`.`, `..` and hidden files), except
/// `.well-known`. The root is the empty path.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct VfsPath(String);

impl VfsPath {
    /// The root, the empty path.
    pub fn root() -> Self {
        Self(String::new())
    }

    /// Normalize the path of a request URL. Each segment is percent-decoded
    /// exactly once and must be UTF-8; an encoded `/` (`%2F`) or `\`
    /// (`%5C`) is rejected rather than treated as a separator, and a
    /// double-encoded `%252e` stays the literal name `%2e`. `None` when
    /// the path breaks a rule.
    pub fn from_url_path(raw: &str) -> Option<Self> {
        let mut segments = Vec::new();
        for segment in raw.split('/') {
            let decoded = percent_decode_str(segment).decode_utf8().ok()?;
            push_segment(&mut segments, decoded)?;
        }
        Some(Self(segments.join("/")))
    }

    /// Normalize a path that is not URL-encoded, such as a configured
    /// index name. `None` when it breaks a rule.
    pub fn new(path: &str) -> Option<Self> {
        let mut segments = Vec::new();
        for segment in path.split('/') {
            push_segment(&mut segments, Cow::Borrowed(segment))?;
        }
        Some(Self(segments.join("/")))
    }

    /// The path, e.g. `assets/app.js`; empty for the root.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Whether this is the root.
    pub fn is_root(&self) -> bool {
        self.0.is_empty()
    }

    /// The segments, outermost first; none for the root.
    pub fn segments(&self) -> impl Iterator<Item = &str> {
        self.0.split('/').filter(|segment| !segment.is_empty())
    }

    /// The last segment; `None` for the root.
    pub fn file_name(&self) -> Option<&str> {
        self.segments().last()
    }

    /// The extension of the last segment (after its last `.`, which must
    /// not be the first character); `None` without one.
    pub fn extension(&self) -> Option<&str> {
        let name = self.file_name()?;
        let (stem, extension) = name.rsplit_once('.')?;
        (!stem.is_empty() && !extension.is_empty()).then_some(extension)
    }

    /// `other` below this path.
    #[must_use]
    pub fn join(&self, other: &VfsPath) -> VfsPath {
        match (self.is_root(), other.is_root()) {
            (true, _) => other.clone(),
            (false, true) => self.clone(),
            (false, false) => Self(format!("{}/{}", self.0, other.0)),
        }
    }

    /// Whether this path is `dir` or lies below it.
    pub fn starts_with(&self, dir: &VfsPath) -> bool {
        dir.is_root()
            || self.0 == dir.0
            || self
                .0
                .strip_prefix(dir.as_str())
                .is_some_and(|rest| rest.starts_with('/'))
    }

    /// This path with `suffix` (such as `.br`) appended to the last segment.
    fn with_suffix(&self, suffix: &str) -> VfsPath {
        Self(format!("{}{suffix}", self.0))
    }
}

impl fmt::Display for VfsPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

fn push_segment<'a>(segments: &mut Vec<Cow<'a, str>>, segment: Cow<'a, str>) -> Option<()> {
    if segment.is_empty() {
        return Some(());
    }
    let too_long = segment.len() > MAX_SEGMENT_BYTES;
    let forbidden_char = segment
        .chars()
        .any(|ch| ch.is_control() || matches!(ch, '/' | '\\' | ':'));
    let hidden = segment.starts_with('.') && segment != WELL_KNOWN;
    if too_long || forbidden_char || hidden {
        return None;
    }
    segments.push(segment);
    Some(())
}

/// One file from a [`Vfs`]: its bytes, a strong `ETag` derived from a
/// SHA-256 of the contents, its content type and, when known, when it was
/// last modified.
///
/// `Debug` shows the length, never the bytes.
#[derive(Clone)]
pub struct VfsFile {
    contents: Bytes,
    etag: HeaderValue,
    content_type: HeaderValue,
    last_modified: Option<SystemTime>,
}

impl fmt::Debug for VfsFile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("VfsFile")
            .field("len", &self.contents.len())
            .field("etag", &self.etag)
            .field("content_type", &self.content_type)
            .field("last_modified", &self.last_modified)
            .finish()
    }
}

impl VfsFile {
    /// A file at `path` whose contents hash to `sha256`; the content type
    /// comes from the path's extension.
    pub fn new(path: &VfsPath, contents: impl Into<Bytes>, sha256: [u8; 32]) -> Self {
        Self {
            contents: contents.into(),
            etag: strong_etag(&sha256),
            content_type: content_type_of(path),
            last_modified: None,
        }
    }

    /// A file at `path`, hashing `contents` for the `ETag`.
    pub fn from_contents(path: &VfsPath, contents: impl Into<Bytes>) -> Self {
        let contents = contents.into();
        let sha256: [u8; 32] = Sha256::digest(&contents).into();
        Self::new(path, contents, sha256)
    }

    /// When the file was last modified.
    #[must_use]
    pub fn with_last_modified(mut self, at: SystemTime) -> Self {
        self.last_modified = Some(at);
        self
    }

    /// Replace the content type guessed from the extension.
    #[must_use]
    pub fn with_content_type(mut self, content_type: HeaderValue) -> Self {
        self.content_type = content_type;
        self
    }

    /// The bytes.
    pub fn contents(&self) -> &Bytes {
        &self.contents
    }

    /// The strong `ETag`, quoted.
    pub fn etag(&self) -> &HeaderValue {
        &self.etag
    }

    /// The content type.
    pub fn content_type(&self) -> &HeaderValue {
        &self.content_type
    }

    /// When the file was last modified, if known.
    pub fn last_modified(&self) -> Option<SystemTime> {
        self.last_modified
    }
}

/// `"<first 16 bytes of the hash in hex>"`.
fn strong_etag(sha256: &[u8; 32]) -> HeaderValue {
    let mut tag = String::with_capacity(34);
    tag.push('"');
    for byte in &sha256[..16] {
        let _ = write!(tag, "{byte:02x}");
    }
    tag.push('"');
    HeaderValue::try_from(tag).expect("hex digits form a valid header value")
}

/// The content type for `path` by extension, `application/octet-stream`
/// when unknown; text types get `charset=utf-8`.
pub(crate) fn content_type_of(path: &VfsPath) -> HeaderValue {
    let guess = path
        .extension()
        .and_then(|extension| mime_guess::from_ext(extension).first_raw())
        .unwrap_or(OCTET_STREAM);
    let textual = guess.starts_with("text/")
        || matches!(guess, "application/javascript" | "application/json");
    if textual {
        HeaderValue::try_from(format!("{guess}; charset=utf-8"))
            .unwrap_or(HeaderValue::from_static(OCTET_STREAM))
    } else {
        HeaderValue::from_static(guess)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(raw: &str) -> Option<String> {
        VfsPath::from_url_path(raw).map(|path| path.as_str().to_owned())
    }

    #[test]
    fn url_paths_collapse_empty_segments() {
        assert_eq!(url("/").as_deref(), Some(""));
        assert_eq!(url("").as_deref(), Some(""));
        assert_eq!(url("//assets///app.js/").as_deref(), Some("assets/app.js"));
        assert_eq!(url("/index.html").as_deref(), Some("index.html"));
        assert_eq!(url("/a%20b.txt").as_deref(), Some("a b.txt"));
        assert_eq!(url("/caf%C3%A9.html").as_deref(), Some("café.html"));
        assert_eq!(
            url("/.well-known/security.txt").as_deref(),
            Some(".well-known/security.txt")
        );
    }

    #[test]
    fn traversal_and_tricks_are_rejected() {
        for raw in [
            "/..",
            "/../etc/passwd",
            "/assets/../../secret",
            "/.",
            "/./index.html",
            "/%2e%2e/secret",
            "/%2E%2E/secret",
            "/.%2e/secret",
            "/assets/%2e%2e%2f%2e%2e%2fsecret",
            "/a%2fb",
            "/a%2Fb",
            "/a%5cb",
            "/a\\b",
            "/..\\secret",
            "/C:/Windows",
            "/c:",
            "/file.txt:stream",
            "/a%00b",
            "/a\0b",
            "/a%0ab",
            "/a%7fb",
            "/.env",
            "/.git/config",
            "/assets/.hidden",
            "/%ff",
            "/%c3",
        ] {
            assert_eq!(url(raw), None, "{raw:?} must be rejected");
        }
    }

    #[test]
    fn double_encoding_stays_literal() {
        assert_eq!(url("/%252e%252e/x").as_deref(), Some("%2e%2e/x"));
        assert_eq!(url("/a%252fb").as_deref(), Some("a%2fb"));
        assert_eq!(url("/100%").as_deref(), Some("100%"));
    }

    #[test]
    fn segments_are_at_most_255_bytes() {
        let longest = "a".repeat(MAX_SEGMENT_BYTES);
        assert_eq!(url(&format!("/{longest}")), Some(longest.clone()));
        assert_eq!(url(&format!("/{longest}a")), None);
        assert_eq!(url(&format!("/x/{longest}a/y")), None);
        let encoded = "%61".repeat(MAX_SEGMENT_BYTES);
        assert_eq!(
            url(&format!("/{encoded}")),
            Some(longest),
            "decoded bytes count"
        );
        assert!(
            VfsPath::new(&"é".repeat(128)).is_none(),
            "256 bytes, 128 chars"
        );
    }

    #[test]
    fn plain_paths_follow_the_same_rules() {
        assert_eq!(
            VfsPath::new("assets/").map(|p| p.0).as_deref(),
            Some("assets")
        );
        assert_eq!(
            VfsPath::new("a%2fb").map(|p| p.0).as_deref(),
            Some("a%2fb"),
            "not decoded"
        );
        for raw in ["../x", "a/../b", "a\\b", "C:", ".x", "a\0"] {
            assert!(VfsPath::new(raw).is_none(), "{raw:?}");
        }
    }

    #[test]
    fn path_helpers() {
        let path = VfsPath::new("assets/app.min.js").unwrap();
        assert_eq!(path.file_name(), Some("app.min.js"));
        assert_eq!(path.extension(), Some("js"));
        assert_eq!(
            path.segments().collect::<Vec<_>>(),
            ["assets", "app.min.js"]
        );
        assert_eq!(path.to_string(), "assets/app.min.js");
        assert_eq!(path.with_suffix(".br").as_str(), "assets/app.min.js.br");

        let root = VfsPath::root();
        assert!(root.is_root());
        assert_eq!(root.file_name(), None);
        assert_eq!(root.extension(), None);
        assert_eq!(root.segments().count(), 0);

        for name in ["settings", ".well-known", "trailing.", "a.b/c"] {
            assert_eq!(VfsPath::new(name).unwrap().extension(), None, "{name}");
        }

        let assets = VfsPath::new("assets").unwrap();
        let index = VfsPath::new("index.html").unwrap();
        assert_eq!(assets.join(&index).as_str(), "assets/index.html");
        assert_eq!(root.join(&index), index);
        assert_eq!(assets.join(&root), assets);

        assert!(path.starts_with(&assets));
        assert!(assets.starts_with(&assets));
        assert!(path.starts_with(&root));
        assert!(!VfsPath::new("assetsx/a.js").unwrap().starts_with(&assets));
        assert!(!assets.starts_with(&path));
    }

    #[test]
    fn content_types_come_from_the_extension() {
        let of = |raw: &str| {
            content_type_of(&VfsPath::new(raw).unwrap())
                .to_str()
                .unwrap()
                .to_owned()
        };
        assert_eq!(of("index.html"), "text/html; charset=utf-8");
        assert_eq!(of("assets/app.CSS"), "text/css; charset=utf-8");
        assert!(of("assets/app.js").ends_with("javascript; charset=utf-8"));
        assert_eq!(of("data.json"), "application/json; charset=utf-8");
        assert_eq!(of("logo.svg"), "image/svg+xml");
        assert_eq!(of("logo.png"), "image/png");
        assert_eq!(of("module.wasm"), "application/wasm");
        assert_eq!(of("blob"), OCTET_STREAM);
        assert_eq!(of("blob.unknownext"), OCTET_STREAM);
    }

    #[test]
    fn files_hash_their_contents_and_hide_them_in_debug() {
        let path = VfsPath::new("index.html").unwrap();
        let file = VfsFile::from_contents(&path, &b"<!doctype html>secret"[..]);
        let again = VfsFile::from_contents(&path, &b"<!doctype html>secret"[..]);
        let other = VfsFile::from_contents(&path, &b"<!doctype html>"[..]);
        assert_eq!(file.etag(), again.etag());
        assert_ne!(file.etag(), other.etag());
        let etag = file.etag().to_str().unwrap();
        assert_eq!(etag.len(), 34);
        assert!(etag.starts_with('"') && etag.ends_with('"'));
        assert_eq!(&file.contents()[..], b"<!doctype html>secret");
        assert_eq!(file.last_modified(), None);

        let debug = format!("{file:?}");
        assert!(debug.contains("len: 21"), "{debug}");
        assert!(!debug.contains("secret"), "{debug}");

        let at = SystemTime::UNIX_EPOCH;
        let custom = file
            .with_last_modified(at)
            .with_content_type(HeaderValue::from_static("text/plain"));
        assert_eq!(custom.last_modified(), Some(at));
        assert_eq!(custom.content_type(), "text/plain");
    }
}
