use std::fmt;
use std::time::{Duration, SystemTime};

use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};
use sekvent_error::AppError;

use crate::error::invalid_presign;

/// Longest lifetime of a presigned URL: seven days, the Signature
/// Version 4 limit.
pub const MAX_EXPIRY: Duration = Duration::from_hours(7 * 24);

/// Lifetime of a presigned URL unless the request sets one: 15 minutes.
pub const DEFAULT_EXPIRY: Duration = Duration::from_mins(15);

/// Longest content type accepted for signing.
const CONTENT_TYPE_LIMIT: usize = 255;

/// Longest file name kept in a `Content-Disposition`, in characters.
const FILE_NAME_LIMIT: usize = 200;

/// File name used when the given one has nothing usable left.
const FALLBACK_FILE_NAME: &str = "download";

/// RFC 8187 `attr-char`: everything else in `filename*` is percent-encoded.
const ATTR_CHAR: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'!')
    .remove(b'#')
    .remove(b'$')
    .remove(b'&')
    .remove(b'+')
    .remove(b'-')
    .remove(b'.')
    .remove(b'^')
    .remove(b'_')
    .remove(b'`')
    .remove(b'|')
    .remove(b'~');

/// What a presigned upload must look like.
///
/// The content type is always signed: the uploader must send exactly this
/// `Content-Type`. With [`content_length`](Self::content_length) the size
/// is signed too, and a body of any other size is rejected by the store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PresignPut {
    pub(crate) content_type: String,
    pub(crate) content_length: Option<u64>,
    pub(crate) expires_in: Duration,
}

impl PresignPut {
    /// An upload of `content_type` (e.g. `image/png`), valid for
    /// [`DEFAULT_EXPIRY`].
    pub fn new(content_type: impl Into<String>) -> Self {
        Self {
            content_type: content_type.into(),
            content_length: None,
            expires_in: DEFAULT_EXPIRY,
        }
    }

    /// Require a body of exactly `bytes` bytes.
    #[must_use]
    pub fn content_length(mut self, bytes: u64) -> Self {
        self.content_length = Some(bytes);
        self
    }

    /// How long the URL stays valid: whole seconds, 1 s to [`MAX_EXPIRY`].
    #[must_use]
    pub fn expires_in(mut self, lifetime: Duration) -> Self {
        self.expires_in = lifetime;
        self
    }
}

/// How a presigned download is presented.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PresignGet {
    pub(crate) expires_in: Duration,
    pub(crate) disposition: Option<String>,
    pub(crate) content_type: Option<String>,
}

impl Default for PresignGet {
    fn default() -> Self {
        Self::new()
    }
}

impl PresignGet {
    /// A download with the stored headers, valid for [`DEFAULT_EXPIRY`].
    pub fn new() -> Self {
        Self {
            expires_in: DEFAULT_EXPIRY,
            disposition: None,
            content_type: None,
        }
    }

    /// How long the URL stays valid: whole seconds, 1 s to [`MAX_EXPIRY`].
    #[must_use]
    pub fn expires_in(mut self, lifetime: Duration) -> Self {
        self.expires_in = lifetime;
        self
    }

    /// Download as a file named `file_name` (`Content-Disposition:
    /// attachment`). The name is sanitised by [`attachment_disposition`].
    #[must_use]
    pub fn attachment(mut self, file_name: &str) -> Self {
        self.disposition = Some(attachment_disposition(file_name));
        self
    }

    /// Show in the browser (`Content-Disposition: inline`).
    #[must_use]
    pub fn inline(mut self) -> Self {
        self.disposition = Some("inline".to_owned());
        self
    }

    /// Serve with this `Content-Type` instead of the stored one, e.g.
    /// `application/octet-stream` to stop a browser from rendering it.
    #[must_use]
    pub fn response_content_type(mut self, content_type: impl Into<String>) -> Self {
        self.content_type = Some(content_type.into());
        self
    }
}

/// A presigned request for a browser or another client.
///
/// The URL is a bearer credential for one object until `expires_at`: hand
/// it only to the user it was made for, and never log it. `Debug` shows
/// method, host and path, never the query.
#[derive(Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct PresignedRequest {
    /// `PUT` or `GET`.
    pub method: &'static str,
    /// The full URL, signature in the query.
    pub url: String,
    /// Headers the client must set, exactly as given (for an upload,
    /// `content-type`). A signed `content-length` is not listed: clients
    /// derive it from the body, and browsers refuse to set it by hand.
    pub headers: Vec<(String, String)>,
    /// When the store starts rejecting the URL.
    pub expires_at: SystemTime,
}

impl fmt::Debug for PresignedRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let without_query = self.url.split('?').next().unwrap_or_default();
        f.debug_struct("PresignedRequest")
            .field("method", &self.method)
            .field("url", &without_query)
            .field("headers", &self.headers)
            .field("expires_at", &self.expires_at)
            .finish()
    }
}

/// The lifetime in whole seconds, if within 1 s and [`MAX_EXPIRY`].
pub(crate) fn expiry_secs(lifetime: Duration) -> Result<u64, AppError> {
    if lifetime < Duration::from_secs(1) || lifetime > MAX_EXPIRY {
        return Err(invalid_presign(
            "the presigned URL lifetime must be between 1 second and 7 days",
        ));
    }
    Ok(lifetime.as_secs())
}

/// Check a media type: `type/subtype` made of token characters,
/// optionally followed by `;` parameters of visible ASCII.
pub(crate) fn check_content_type(value: &str) -> Result<(), AppError> {
    let token = |part: &str| {
        !part.is_empty()
            && part
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"!#$&^_.+-".contains(&b))
    };
    let printable = value.bytes().all(|b| b == b' ' || b.is_ascii_graphic());
    let essence = value.split(';').next().unwrap_or_default().trim();
    let shaped = essence
        .split_once('/')
        .is_some_and(|(kind, subtype)| token(kind) && token(subtype));
    if value.len() <= CONTENT_TYPE_LIMIT && printable && shaped {
        Ok(())
    } else {
        Err(invalid_presign(
            "the content type is not a valid media type",
        ))
    }
}

/// An `attachment` `Content-Disposition` for `file_name`, safe to put in a
/// header.
///
/// Only the last path component is kept (after `/` or `\`), control
/// characters are dropped, and the name is cut to 200 characters; an empty
/// result becomes `download`. The value carries an ASCII `filename` for old
/// clients (anything else, and `"`, `%`, `;`, replaced by `_`) and the
/// exact name as RFC 8187 `filename*` in UTF-8.
pub fn attachment_disposition(file_name: &str) -> String {
    let base = file_name.rsplit(['/', '\\']).next().unwrap_or_default();
    let cleaned: String = base
        .chars()
        .filter(|c| !c.is_control())
        .take(FILE_NAME_LIMIT)
        .collect();
    let cleaned = cleaned.trim();
    let name = if cleaned.is_empty() || cleaned == "." || cleaned == ".." {
        FALLBACK_FILE_NAME
    } else {
        cleaned
    };
    let ascii: String = name
        .chars()
        .map(|c| {
            if (c.is_ascii_graphic() || c == ' ') && !matches!(c, '"' | '%' | ';' | '\\') {
                c
            } else {
                '_'
            }
        })
        .collect();
    let encoded = utf8_percent_encode(name, ATTR_CHAR);
    format!("attachment; filename=\"{ascii}\"; filename*=UTF-8''{encoded}")
}

#[cfg(test)]
mod tests {
    use sekvent_error::ErrorCode;

    use super::*;
    use crate::reasons;

    #[test]
    fn put_requests_build_with_defaults() {
        let put = PresignPut::new("image/png");
        assert_eq!(put.expires_in, DEFAULT_EXPIRY);
        assert_eq!(put.content_length, None);
        let put = put.content_length(10).expires_in(Duration::from_mins(1));
        assert_eq!(put.content_length, Some(10));
        assert_eq!(put.expires_in, Duration::from_mins(1));
    }

    #[test]
    fn get_requests_build_with_defaults() {
        assert_eq!(PresignGet::default(), PresignGet::new());
        let get = PresignGet::new()
            .inline()
            .response_content_type("text/plain")
            .expires_in(Duration::from_secs(5));
        assert_eq!(get.disposition.as_deref(), Some("inline"));
        assert_eq!(get.content_type.as_deref(), Some("text/plain"));
        assert_eq!(get.expires_in, Duration::from_secs(5));
        let get = PresignGet::new().attachment("a.txt");
        assert_eq!(
            get.disposition.as_deref(),
            Some("attachment; filename=\"a.txt\"; filename*=UTF-8''a.txt")
        );
    }

    #[test]
    fn expiries_are_bounded() {
        assert_eq!(expiry_secs(Duration::from_secs(1)).unwrap(), 1);
        assert_eq!(expiry_secs(MAX_EXPIRY).unwrap(), 604_800);
        assert_eq!(expiry_secs(Duration::from_millis(1500)).unwrap(), 1);
        for bad in [
            Duration::ZERO,
            Duration::from_millis(999),
            MAX_EXPIRY + Duration::from_secs(1),
        ] {
            let error = expiry_secs(bad).unwrap_err();
            assert_eq!(error.code(), ErrorCode::InvalidArgument);
            assert_eq!(error.reason(), Some(reasons::INVALID_PRESIGN_REQUEST));
        }
    }

    #[test]
    fn content_types_must_be_media_types() {
        for good in [
            "image/png",
            "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
            "text/plain; charset=utf-8",
            "application/x-www-form-urlencoded",
        ] {
            assert!(check_content_type(good).is_ok(), "{good}");
        }
        for bad in [
            "",
            "png",
            "/png",
            "image/",
            "image/png\r\nx-amz-acl: public-read",
            "image/pñg",
            "ima ge/png",
            &format!("text/{}", "a".repeat(255)),
        ] {
            let error = check_content_type(bad).unwrap_err();
            assert_eq!(
                error.reason(),
                Some(reasons::INVALID_PRESIGN_REQUEST),
                "{bad}"
            );
        }
    }

    #[test]
    fn dispositions_are_safe_and_keep_the_real_name() {
        assert_eq!(
            attachment_disposition("Report 2026.pdf"),
            "attachment; filename=\"Report 2026.pdf\"; filename*=UTF-8''Report%202026.pdf"
        );
        assert_eq!(
            attachment_disposition("árvíztűrő \"tükör\";%.txt"),
            "attachment; filename=\"_rv_zt_r_ _t_k_r___.txt\"; \
             filename*=UTF-8''%C3%A1rv%C3%ADzt%C5%B1r%C5%91%20%22t%C3%BCk%C3%B6r%22%3B%25.txt"
        );
        assert_eq!(
            attachment_disposition("../../etc/passwd"),
            "attachment; filename=\"passwd\"; filename*=UTF-8''passwd"
        );
        assert_eq!(
            attachment_disposition("C:\\Users\\me\\a.txt"),
            "attachment; filename=\"a.txt\"; filename*=UTF-8''a.txt"
        );
        assert_eq!(
            attachment_disposition("evil\r\nSet-Cookie: x"),
            "attachment; filename=\"evilSet-Cookie: x\"; filename*=UTF-8''evilSet-Cookie%3A%20x"
        );
        for empty in ["", "dir/", " ", "..", "\u{7}"] {
            assert_eq!(
                attachment_disposition(empty),
                "attachment; filename=\"download\"; filename*=UTF-8''download"
            );
        }
        let long = attachment_disposition(&"x".repeat(500));
        assert_eq!(long.matches('x').count(), 2 * FILE_NAME_LIMIT);
    }

    #[test]
    fn presigned_requests_debug_without_the_query() {
        let request = PresignedRequest {
            method: "GET",
            url: "https://h.example/b/k?X-Amz-Signature=abc".to_owned(),
            headers: Vec::new(),
            expires_at: SystemTime::UNIX_EPOCH,
        };
        let shown = format!("{request:?}");
        assert!(shown.contains("https://h.example/b/k"));
        assert!(!shown.contains("X-Amz-Signature"));
        assert_eq!(request.clone(), request);
    }
}
