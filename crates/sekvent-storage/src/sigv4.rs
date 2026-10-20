//! AWS Signature Version 4 for S3: query-string presigning and
//! `Authorization` header signing.
//!
//! S3 flavour: the path is URI-encoded once (not twice, as other services
//! do), and presigned requests carry `UNSIGNED-PAYLOAD`.

use std::fmt::Write as _;
use std::time::{SystemTime, UNIX_EPOCH};

use hmac::{Hmac, KeyInit, Mac};
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};
use sekvent_config::Secret;
use sha2::{Digest, Sha256};

type HmacSha256 = Hmac<Sha256>;

pub(crate) const ALGORITHM: &str = "AWS4-HMAC-SHA256";
pub(crate) const UNSIGNED_PAYLOAD: &str = "UNSIGNED-PAYLOAD";
/// SHA-256 of the empty string: the payload hash of a request without body.
pub(crate) const EMPTY_SHA256: &str =
    "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

/// RFC 3986 unreserved characters stay as they are; everything else is
/// percent-encoded with upper-case hex digits.
const UNRESERVED: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~');

/// As [`UNRESERVED`], but `/` separates path segments and stays.
const PATH: &AsciiSet = &UNRESERVED.remove(b'/');

/// Encode a query name or value.
pub(crate) fn encode(value: &str) -> String {
    utf8_percent_encode(value, UNRESERVED).to_string()
}

/// Encode a path, keeping `/`.
pub(crate) fn encode_path(value: &str) -> String {
    utf8_percent_encode(value, PATH).to_string()
}

/// The two forms of the signing time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Timestamp {
    /// `YYYYMMDD`, the credential scope date.
    pub(crate) date: String,
    /// `YYYYMMDDTHHMMSSZ`, the `X-Amz-Date` value.
    pub(crate) datetime: String,
}

impl Timestamp {
    /// The UTC timestamp of `at`; times before the epoch count as the epoch.
    pub(crate) fn at(at: SystemTime) -> Self {
        let secs = at.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
        let days = i64::try_from(secs / 86_400).unwrap_or(i64::MAX);
        let rest = secs % 86_400;
        let (year, month, day) = civil_from_days(days);
        let date = format!("{year:04}{month:02}{day:02}");
        let datetime = format!(
            "{date}T{:02}{:02}{:02}Z",
            rest / 3600,
            rest / 60 % 60,
            rest % 60
        );
        Self { date, datetime }
    }
}

/// Proleptic Gregorian date of a day count since 1970-01-01.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = u32::try_from(doy - (153 * mp + 2) / 5 + 1).unwrap_or(1);
    let month = u32::try_from(if mp < 10 { mp + 3 } else { mp - 9 }).unwrap_or(1);
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

pub(crate) fn sha256_hex(data: &[u8]) -> String {
    hex(&Sha256::digest(data))
}

fn hmac(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC accepts keys of any length");
    mac.update(data);
    mac.finalize().into_bytes().to_vec()
}

/// The canonical query string: names and values encoded, sorted by name
/// and then value, joined with `&`.
pub(crate) fn canonical_query(pairs: &[(String, String)]) -> String {
    let mut encoded: Vec<(String, String)> = pairs
        .iter()
        .map(|(name, value)| (encode(name), encode(value)))
        .collect();
    encoded.sort();
    encoded
        .iter()
        .map(|(name, value)| format!("{name}={value}"))
        .collect::<Vec<_>>()
        .join("&")
}

/// Canonical header lines and the signed-header list. Names are lower-cased,
/// values trimmed with inner runs of spaces collapsed.
fn canonical_headers(headers: &[(String, String)]) -> (String, String) {
    let mut normalised: Vec<(String, String)> = headers
        .iter()
        .map(|(name, value)| {
            (
                name.to_ascii_lowercase(),
                value.split_whitespace().collect::<Vec<_>>().join(" "),
            )
        })
        .collect();
    normalised.sort();
    let mut lines = String::new();
    for (name, value) in &normalised {
        let _ = writeln!(lines, "{name}:{value}");
    }
    let signed = normalised
        .iter()
        .map(|(name, _)| name.as_str())
        .collect::<Vec<_>>()
        .join(";");
    (lines, signed)
}

/// One request to sign: everything but the credentials and the time.
pub(crate) struct Request<'a> {
    pub(crate) method: &'a str,
    /// `host[:port]` as sent in the `Host` header.
    pub(crate) host: &'a str,
    /// The encoded path, starting with `/`.
    pub(crate) path: &'a str,
    /// Raw (not yet encoded) query pairs.
    pub(crate) query: Vec<(String, String)>,
    /// Headers to sign besides `host` (and the `x-amz-*` the signer adds).
    pub(crate) headers: Vec<(String, String)>,
}

/// Signs with one set of credentials for one region and service.
pub(crate) struct Signer {
    access_key_id: String,
    secret_access_key: Secret,
    session_token: Option<Secret>,
    region: String,
    service: String,
}

impl std::fmt::Debug for Signer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Signer")
            .field("region", &self.region)
            .field("service", &self.service)
            .finish_non_exhaustive()
    }
}

impl Signer {
    pub(crate) fn new(
        access_key_id: String,
        secret_access_key: Secret,
        session_token: Option<Secret>,
        region: String,
    ) -> Self {
        Self {
            access_key_id,
            secret_access_key,
            session_token,
            region,
            service: "s3".to_owned(),
        }
    }

    fn scope(&self, date: &str) -> String {
        format!("{date}/{}/{}/aws4_request", self.region, self.service)
    }

    fn signing_key(&self, date: &str) -> Vec<u8> {
        let secret = Secret::new(format!("AWS4{}", self.secret_access_key.expose()));
        let key = hmac(secret.expose().as_bytes(), date.as_bytes());
        let key = hmac(&key, self.region.as_bytes());
        let key = hmac(&key, self.service.as_bytes());
        hmac(&key, b"aws4_request")
    }

    /// The signature of a canonical request made at `at`.
    fn signature(&self, at: &Timestamp, canonical_request: &str) -> String {
        let string_to_sign = format!(
            "{ALGORITHM}\n{}\n{}\n{}",
            at.datetime,
            self.scope(&at.date),
            sha256_hex(canonical_request.as_bytes())
        );
        hex(&hmac(
            &self.signing_key(&at.date),
            string_to_sign.as_bytes(),
        ))
    }

    fn canonical_request(
        method: &str,
        path: &str,
        query: &[(String, String)],
        headers: &[(String, String)],
        payload_hash: &str,
    ) -> (String, String) {
        let (lines, signed) = canonical_headers(headers);
        let canonical = format!(
            "{method}\n{path}\n{}\n{lines}\n{signed}\n{payload_hash}",
            canonical_query(query),
        );
        (canonical, signed)
    }

    /// Presign `request` at `now` for `expires_secs`: the full query
    /// string, signature included, ready to append after `?`.
    pub(crate) fn presign(
        &self,
        request: Request<'_>,
        now: SystemTime,
        expires_secs: u64,
    ) -> String {
        let at = Timestamp::at(now);
        let Request {
            method,
            host,
            path,
            mut query,
            mut headers,
        } = request;
        headers.push(("host".to_owned(), host.to_owned()));
        let (_, signed) = canonical_headers(&headers);
        query.extend([
            ("X-Amz-Algorithm".to_owned(), ALGORITHM.to_owned()),
            (
                "X-Amz-Credential".to_owned(),
                format!("{}/{}", self.access_key_id, self.scope(&at.date)),
            ),
            ("X-Amz-Date".to_owned(), at.datetime.clone()),
            ("X-Amz-Expires".to_owned(), expires_secs.to_string()),
            ("X-Amz-SignedHeaders".to_owned(), signed),
        ]);
        if let Some(token) = &self.session_token {
            query.push(("X-Amz-Security-Token".to_owned(), token.expose().to_owned()));
        }
        let (canonical, _) =
            Self::canonical_request(method, path, &query, &headers, UNSIGNED_PAYLOAD);
        let signature = self.signature(&at, &canonical);
        query.push(("X-Amz-Signature".to_owned(), signature));
        canonical_query(&query)
    }

    /// Sign `request` at `now` with an `Authorization` header. Returns the
    /// headers to send besides `host`: the request's own, `x-amz-date`,
    /// `x-amz-content-sha256`, the session token if any, and
    /// `authorization`.
    pub(crate) fn sign(
        &self,
        request: Request<'_>,
        payload_hash: &str,
        now: SystemTime,
    ) -> Vec<(String, String)> {
        let at = Timestamp::at(now);
        let Request {
            method,
            host,
            path,
            query,
            mut headers,
        } = request;
        headers.push(("x-amz-content-sha256".to_owned(), payload_hash.to_owned()));
        headers.push(("x-amz-date".to_owned(), at.datetime.clone()));
        if let Some(token) = &self.session_token {
            headers.push(("x-amz-security-token".to_owned(), token.expose().to_owned()));
        }
        let mut signing = headers.clone();
        signing.push(("host".to_owned(), host.to_owned()));
        let (canonical, signed) =
            Self::canonical_request(method, path, &query, &signing, payload_hash);
        let signature = self.signature(&at, &canonical);
        headers.push((
            "authorization".to_owned(),
            format!(
                "{ALGORITHM} Credential={}/{},SignedHeaders={signed},Signature={signature}",
                self.access_key_id,
                self.scope(&at.date)
            ),
        ));
        headers
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    const ACCESS_KEY: &str = "AKIAIOSFODNN7EXAMPLE";
    const SECRET_KEY: &str = "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY";
    const HOST: &str = "examplebucket.s3.amazonaws.com";

    /// 2013-05-24T00:00:00Z, the instant of the S3 documentation examples.
    fn example_time() -> SystemTime {
        UNIX_EPOCH + Duration::from_hours(380_376)
    }

    fn example_signer() -> Signer {
        Signer::new(
            ACCESS_KEY.to_owned(),
            Secret::new(SECRET_KEY),
            None,
            "us-east-1".to_owned(),
        )
    }

    fn pairs(list: &[(&str, &str)]) -> Vec<(String, String)> {
        list.iter()
            .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
            .collect()
    }

    fn authorization(headers: &[(String, String)]) -> &str {
        &headers
            .iter()
            .find(|(name, _)| name == "authorization")
            .unwrap()
            .1
    }

    #[test]
    fn timestamps_are_utc_in_both_forms() {
        assert_eq!(
            Timestamp::at(example_time()),
            Timestamp {
                date: "20130524".into(),
                datetime: "20130524T000000Z".into()
            }
        );
        let leap = UNIX_EPOCH + Duration::from_secs(1_709_210_096);
        assert_eq!(Timestamp::at(leap).datetime, "20240229T123456Z");
        assert_eq!(Timestamp::at(UNIX_EPOCH).datetime, "19700101T000000Z");
        let before = UNIX_EPOCH - Duration::from_secs(10);
        assert_eq!(Timestamp::at(before).datetime, "19700101T000000Z");
        let end_of_year = UNIX_EPOCH + Duration::from_secs(1_767_225_599);
        assert_eq!(Timestamp::at(end_of_year).datetime, "20251231T235959Z");
    }

    #[test]
    fn encoding_keeps_only_unreserved_characters() {
        assert_eq!(encode("a b/c~d_e.f-g"), "a%20b%2Fc~d_e.f-g");
        assert_eq!(encode_path("/test$file.text"), "/test%24file.text");
        assert_eq!(encode_path("dir/ä ü+"), "dir/%C3%A4%20%C3%BC%2B");
    }

    #[test]
    fn the_signing_key_matches_the_published_derivation_example() {
        let signer = Signer {
            service: "iam".to_owned(),
            ..Signer::new(
                ACCESS_KEY.to_owned(),
                Secret::new("wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY"),
                None,
                "us-east-1".to_owned(),
            )
        };
        assert_eq!(
            hex(&signer.signing_key("20120215")),
            "f4780e2d9f65fa895f9c67b32ce1baf0b0d8a43505a000a1a9e090d414db404d"
        );
    }

    #[test]
    fn the_presigned_get_matches_the_s3_example() {
        let query = example_signer().presign(
            Request {
                method: "GET",
                host: HOST,
                path: "/test.txt",
                query: Vec::new(),
                headers: Vec::new(),
            },
            example_time(),
            86_400,
        );
        assert_eq!(
            query,
            "X-Amz-Algorithm=AWS4-HMAC-SHA256\
             &X-Amz-Credential=AKIAIOSFODNN7EXAMPLE%2F20130524%2Fus-east-1%2Fs3%2Faws4_request\
             &X-Amz-Date=20130524T000000Z\
             &X-Amz-Expires=86400\
             &X-Amz-Signature=aeeed9bbccd4d02ee5c0109b86d86835f995330da4c265957d157751f604d404\
             &X-Amz-SignedHeaders=host"
        );
    }

    #[test]
    fn the_header_signed_get_with_range_matches_the_s3_example() {
        let headers = example_signer().sign(
            Request {
                method: "GET",
                host: HOST,
                path: "/test.txt",
                query: Vec::new(),
                headers: pairs(&[("Range", "bytes=0-9")]),
            },
            EMPTY_SHA256,
            example_time(),
        );
        assert_eq!(
            authorization(&headers),
            "AWS4-HMAC-SHA256 Credential=AKIAIOSFODNN7EXAMPLE/20130524/us-east-1/s3/aws4_request,\
             SignedHeaders=host;range;x-amz-content-sha256;x-amz-date,\
             Signature=f0e8bdb87c964420e857bd35b5d6ed310bd44f0170aba48dd91039c6036bdb41"
        );
        assert!(
            headers
                .iter()
                .any(|(name, value)| name == "x-amz-date" && value == "20130524T000000Z")
        );
    }

    #[test]
    fn the_header_signed_put_matches_the_s3_example() {
        let body = b"Welcome to Amazon S3.";
        let hash = sha256_hex(body);
        assert_eq!(
            hash,
            "44ce7dd67c959e0d3524ffac1771dfbba87d2b6b4b4e99e42034a8b803f8b072"
        );
        let headers = example_signer().sign(
            Request {
                method: "PUT",
                host: HOST,
                path: &encode_path("/test$file.text"),
                query: Vec::new(),
                headers: pairs(&[
                    ("Date", "Fri, 24 May 2013 00:00:00 GMT"),
                    ("x-amz-storage-class", "REDUCED_REDUNDANCY"),
                ]),
            },
            &hash,
            example_time(),
        );
        assert!(authorization(&headers).ends_with(
            "SignedHeaders=date;host;x-amz-content-sha256;x-amz-date;x-amz-storage-class,\
             Signature=98ad721746da40c64f1a55b78f14c238d841ea1380cd77a1b5971af0ece108bd"
        ));
    }

    #[test]
    fn the_header_signed_lifecycle_and_list_requests_match_the_s3_examples() {
        let signer = example_signer();
        let lifecycle = signer.sign(
            Request {
                method: "GET",
                host: HOST,
                path: "/",
                query: pairs(&[("lifecycle", "")]),
                headers: Vec::new(),
            },
            EMPTY_SHA256,
            example_time(),
        );
        assert!(authorization(&lifecycle).ends_with(
            "Signature=fea454ca298b7da1c68078a5d1bdbfbbe0d65c699e0f91ac7a200a0136783543"
        ));
        let list = signer.sign(
            Request {
                method: "GET",
                host: HOST,
                path: "/",
                query: pairs(&[("max-keys", "2"), ("prefix", "J")]),
                headers: Vec::new(),
            },
            EMPTY_SHA256,
            example_time(),
        );
        assert!(authorization(&list).ends_with(
            "Signature=34b48302e7b5fa45bde8084f4b7868a86f0a534bc59db6670ed5711ef69dc6f7"
        ));
    }

    #[test]
    fn a_session_token_is_signed_in_both_forms() {
        let signer = Signer::new(
            ACCESS_KEY.to_owned(),
            Secret::new(SECRET_KEY),
            Some(Secret::new("token/with+chars")),
            "us-east-1".to_owned(),
        );
        let request = || Request {
            method: "GET",
            host: HOST,
            path: "/test.txt",
            query: Vec::new(),
            headers: Vec::new(),
        };
        let query = signer.presign(request(), example_time(), 60);
        assert!(query.contains("X-Amz-Security-Token=token%2Fwith%2Bchars"));
        assert!(
            !query.contains("aeeed9bb"),
            "the token changes the signature"
        );
        let headers = signer.sign(request(), EMPTY_SHA256, example_time());
        assert!(
            headers
                .iter()
                .any(|(name, value)| name == "x-amz-security-token" && value == "token/with+chars")
        );
        assert!(
            authorization(&headers).contains(
                "SignedHeaders=host;x-amz-content-sha256;x-amz-date;x-amz-security-token"
            )
        );
    }

    #[test]
    fn canonical_parts_sort_encode_and_normalise() {
        assert_eq!(
            canonical_query(&pairs(&[("b", "2"), ("a", "x y"), ("a", "1")])),
            "a=1&a=x%20y&b=2"
        );
        let (lines, signed) = canonical_headers(&pairs(&[
            ("X-Amz-Meta-B", "  two   words "),
            ("content-type", "text/plain"),
        ]));
        assert_eq!(lines, "content-type:text/plain\nx-amz-meta-b:two words\n");
        assert_eq!(signed, "content-type;x-amz-meta-b");
    }

    #[test]
    fn the_signer_debug_shows_no_credentials() {
        let shown = format!("{:?}", example_signer());
        assert!(!shown.contains(SECRET_KEY));
        assert!(!shown.contains(ACCESS_KEY));
        assert!(shown.contains("us-east-1"));
    }
}
