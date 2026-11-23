//! gRPC-Web frames: the request frame, and the reply's message and trailers decoded into an
//! outcome.

use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD_PAD_INDIFFERENT;
use prost::Message;
use reqwest::header::HeaderMap;
use sekvent_error::{AppError, ErrorCode, FieldViolation};

/// The largest reply body read, in bytes.
pub(super) const MAX_REPLY_BYTES: usize = 4 * 1024 * 1024;

/// A frame's header: one flag byte and a four-byte big-endian length.
const HEADER_LEN: usize = 5;
/// The flag of a trailer frame.
const FLAG_TRAILERS: u8 = 0x80;
/// The flag bit of a compressed frame.
const FLAG_COMPRESSED: u8 = 0x01;

const GRPC_STATUS: &str = "grpc-status";
const GRPC_MESSAGE: &str = "grpc-message";
const GRPC_DETAILS: &str = "grpc-status-details-bin";

/// `message` in one uncompressed gRPC-Web data frame. `INVALID_ARGUMENT` for a message whose
/// length does not fit the frame's four-byte length.
pub(super) fn encode<M: Message>(message: &M) -> Result<Vec<u8>, AppError> {
    let length = message.encoded_len();
    let Ok(prefix) = u32::try_from(length) else {
        return Err(AppError::invalid_argument(
            "the admin request is too large to send",
        ));
    };
    let mut frame = Vec::with_capacity(HEADER_LEN + length);
    frame.push(0);
    frame.extend_from_slice(&prefix.to_be_bytes());
    message
        .encode(&mut frame)
        .map_err(|_| AppError::invalid_argument("the admin request could not be encoded"))?;
    Ok(frame)
}

/// Why a reply could not be read. Never shown to a caller beyond a fixed message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Malformed {
    /// A frame shorter than its header or its declared length.
    Truncated,
    /// A compressed frame.
    Compressed,
    /// A flag other than a data or a trailer frame.
    UnknownFlag,
    /// A frame after the trailer frame.
    AfterTrailers,
    /// A trailer block that is not `name: value` lines of UTF-8.
    TrailerLine,
    /// No `grpc-status` in the headers or the trailers.
    NoStatus,
    /// A `grpc-status` that is not a number.
    BadStatus,
    /// Status OK without exactly one message.
    MessageCount,
    /// The message does not decode as the reply type.
    Undecodable,
}

/// The reply of a `200 OK` answer: the decoded message on status OK, else the error the product
/// answered with; `Err` when the reply is malformed. The status of a trailers-only answer (in
/// `headers`) wins over a trailer frame.
pub(super) fn decode_reply<Rep: Message + Default>(
    headers: &HeaderMap,
    body: &[u8],
) -> Result<Result<Rep, AppError>, Malformed> {
    let frames = split(body)?;
    let status = match from_headers(headers)? {
        Some(status) => status,
        None => match frames.trailers {
            Some(block) => from_trailers(block)?,
            None => return Err(Malformed::NoStatus),
        },
    };
    if status.code == 0 {
        let [message] = frames.messages.as_slice() else {
            return Err(Malformed::MessageCount);
        };
        return Rep::decode(*message)
            .map(Ok)
            .map_err(|_| Malformed::Undecodable);
    }
    Ok(Err(status.into_error()))
}

/// A reply body cut into its frames.
#[derive(Debug, Default)]
struct Frames<'a> {
    messages: Vec<&'a [u8]>,
    trailers: Option<&'a [u8]>,
}

fn split(body: &[u8]) -> Result<Frames<'_>, Malformed> {
    let mut frames = Frames::default();
    let mut rest = body;
    while let Some((&flag, tail)) = rest.split_first() {
        if frames.trailers.is_some() {
            return Err(Malformed::AfterTrailers);
        }
        let Some((prefix, tail)) = tail.split_first_chunk::<4>() else {
            return Err(Malformed::Truncated);
        };
        let length =
            usize::try_from(u32::from_be_bytes(*prefix)).map_err(|_| Malformed::Truncated)?;
        let Some((payload, tail)) = tail.split_at_checked(length) else {
            return Err(Malformed::Truncated);
        };
        rest = tail;
        if flag & FLAG_COMPRESSED != 0 {
            return Err(Malformed::Compressed);
        }
        match flag {
            0 => frames.messages.push(payload),
            FLAG_TRAILERS => frames.trailers = Some(payload),
            _ => return Err(Malformed::UnknownFlag),
        }
    }
    Ok(frames)
}

/// The gRPC status of a reply, its texts as received.
#[derive(Debug, PartialEq, Eq)]
struct Status {
    code: i32,
    message: Option<String>,
    details: Option<String>,
}

/// The status of a trailers-only answer, when the headers carry one.
fn from_headers(headers: &HeaderMap) -> Result<Option<Status>, Malformed> {
    let Some(code) = headers.get(GRPC_STATUS) else {
        return Ok(None);
    };
    let code = code.to_str().map_err(|_| Malformed::BadStatus)?;
    let text = |name: &str| {
        headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .map(|value| value.trim().to_owned())
    };
    Ok(Some(Status {
        code: parse_code(code)?,
        message: text(GRPC_MESSAGE),
        details: text(GRPC_DETAILS),
    }))
}

/// The status of a trailer frame: `name: value` lines separated by `\r\n`, names compared
/// without case; the first of a repeated name wins.
fn from_trailers(block: &[u8]) -> Result<Status, Malformed> {
    let text = std::str::from_utf8(block).map_err(|_| Malformed::TrailerLine)?;
    let mut code = None;
    let mut message = None;
    let mut details = None;
    for line in text.split('\n') {
        let line = line.strip_suffix('\r').unwrap_or(line);
        if line.trim().is_empty() {
            continue;
        }
        let Some((name, value)) = line.split_once(':') else {
            return Err(Malformed::TrailerLine);
        };
        let name = name.trim();
        let slot = if name.eq_ignore_ascii_case(GRPC_STATUS) {
            &mut code
        } else if name.eq_ignore_ascii_case(GRPC_MESSAGE) {
            &mut message
        } else if name.eq_ignore_ascii_case(GRPC_DETAILS) {
            &mut details
        } else {
            continue;
        };
        slot.get_or_insert_with(|| value.trim().to_owned());
    }
    let Some(code) = code else {
        return Err(Malformed::NoStatus);
    };
    Ok(Status {
        code: parse_code(&code)?,
        message,
        details,
    })
}

fn parse_code(text: &str) -> Result<i32, Malformed> {
    let text = text.trim();
    if text.is_empty() || !text.bytes().all(|b| b.is_ascii_digit()) {
        return Err(Malformed::BadStatus);
    }
    text.parse::<i32>().map_err(|_| Malformed::BadStatus)
}

impl Status {
    /// The error the product answered: the code, the percent-decoded message, and what the
    /// details carry. Undecodable details are dropped, the code and message kept.
    fn into_error(self) -> AppError {
        let message = self
            .message
            .as_deref()
            .map(percent_decode)
            .unwrap_or_default();
        let error = AppError::new(ErrorCode::from_i32(self.code), message);
        match self
            .details
            .as_deref()
            .and_then(|text| STANDARD_PAD_INDIFFERENT.decode(text).ok())
        {
            Some(bytes) => with_details(error, &bytes),
            None => error,
        }
    }
}

/// `text` with every `%XX` escape decoded; a `%` without two hex digits stays as is; bytes that
/// are not UTF-8 become the replacement character.
fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while let Some(&byte) = bytes.get(index) {
        if byte == b'%'
            && let (Some(high), Some(low)) = (
                bytes.get(index + 1).copied().and_then(hex),
                bytes.get(index + 2).copied().and_then(hex),
            )
        {
            out.push((high << 4) | low);
            index += 3;
            continue;
        }
        out.push(byte);
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// The messages of `grpc-status-details-bin`, as sekvent and the standard error model write
/// them.
mod wire {
    // prost generates `pub` items, unreachable from this private module.
    #![allow(unreachable_pub)]

    use std::collections::BTreeMap;

    /// `google.rpc.Status`.
    #[derive(Clone, PartialEq, prost::Message)]
    pub struct RpcStatus {
        #[prost(int32, tag = "1")]
        pub code: i32,
        #[prost(string, tag = "2")]
        pub message: String,
        #[prost(message, repeated, tag = "3")]
        pub details: Vec<Any>,
    }

    /// `google.protobuf.Any`.
    #[derive(Clone, PartialEq, prost::Message)]
    pub struct Any {
        #[prost(string, tag = "1")]
        pub type_url: String,
        #[prost(bytes = "vec", tag = "2")]
        pub value: Vec<u8>,
    }

    /// `google.rpc.ErrorInfo`.
    #[derive(Clone, PartialEq, prost::Message)]
    pub struct ErrorInfo {
        #[prost(string, tag = "1")]
        pub reason: String,
        #[prost(string, tag = "2")]
        pub domain: String,
        #[prost(btree_map = "string, string", tag = "3")]
        pub metadata: BTreeMap<String, String>,
    }

    /// `google.rpc.RetryInfo`.
    #[derive(Clone, PartialEq, prost::Message)]
    pub struct RetryInfo {
        #[prost(message, optional, tag = "1")]
        pub retry_delay: Option<ProtoDuration>,
    }

    /// `google.protobuf.Duration`.
    #[derive(Clone, PartialEq, prost::Message)]
    pub struct ProtoDuration {
        #[prost(int64, tag = "1")]
        pub seconds: i64,
        #[prost(int32, tag = "2")]
        pub nanos: i32,
    }

    /// `google.rpc.BadRequest`.
    #[derive(Clone, PartialEq, prost::Message)]
    pub struct BadRequest {
        #[prost(message, repeated, tag = "1")]
        pub field_violations: Vec<Violation>,
    }

    /// `google.rpc.BadRequest.FieldViolation` (its localized message is skipped).
    #[derive(Clone, PartialEq, prost::Message)]
    pub struct Violation {
        #[prost(string, tag = "1")]
        pub field: String,
        #[prost(string, tag = "2")]
        pub description: String,
        #[prost(string, tag = "3")]
        pub reason: String,
    }

    /// `sekvent.error.v1.FieldViolationDetails`.
    #[derive(Clone, PartialEq, prost::Message)]
    pub struct FieldViolationDetails {
        #[prost(message, repeated, tag = "1")]
        pub violations: Vec<FieldViolationDetail>,
    }

    /// `sekvent.error.v1.FieldViolationDetail`.
    #[derive(Clone, PartialEq, prost::Message)]
    pub struct FieldViolationDetail {
        #[prost(btree_map = "string, string", tag = "1")]
        pub metadata: BTreeMap<String, String>,
    }
}

const ERROR_INFO: &str = "google.rpc.ErrorInfo";
const RETRY_INFO: &str = "google.rpc.RetryInfo";
const BAD_REQUEST: &str = "google.rpc.BadRequest";
const VIOLATION_DETAILS: &str = "sekvent.error.v1.FieldViolationDetails";

/// `error` with the reason, domain, metadata, retry hint and field violations of the encoded
/// `google.rpc.Status` `bytes`: the first decodable detail of each type counts, unknown types
/// are skipped, and undecodable bytes leave `error` as it is.
fn with_details(mut error: AppError, bytes: &[u8]) -> AppError {
    let Ok(status) = wire::RpcStatus::decode(bytes) else {
        return error;
    };
    let mut info = None;
    let mut retry = None;
    let mut bad_request = None;
    let mut extra = None;
    for detail in &status.details {
        let name = detail
            .type_url
            .rsplit_once('/')
            .map_or(detail.type_url.as_str(), |(_, name)| name);
        let value = detail.value.as_slice();
        match name {
            ERROR_INFO if info.is_none() => info = wire::ErrorInfo::decode(value).ok(),
            RETRY_INFO if retry.is_none() => retry = wire::RetryInfo::decode(value).ok(),
            BAD_REQUEST if bad_request.is_none() => {
                bad_request = wire::BadRequest::decode(value).ok();
            }
            VIOLATION_DETAILS if extra.is_none() => {
                extra = wire::FieldViolationDetails::decode(value).ok();
            }
            _ => {}
        }
    }
    if let Some(info) = info {
        if !info.reason.is_empty() {
            error = error.with_reason(info.reason);
        }
        if !info.domain.is_empty() {
            error = error.with_domain(info.domain);
        }
        for (key, value) in info.metadata {
            error = error.with_metadata(key, value);
        }
    }
    if let Some(delay) = retry
        .and_then(|retry| retry.retry_delay)
        .and_then(|delay| duration(&delay))
    {
        error = error.with_retry_after(delay);
    }
    if let Some(bad_request) = bad_request {
        let mut metadata = extra
            .map(|extra| extra.violations)
            .unwrap_or_default()
            .into_iter();
        for violation in bad_request.field_violations {
            let mut built = FieldViolation::new(violation.field, violation.description);
            if !violation.reason.is_empty() {
                built = built.with_reason(violation.reason);
            }
            if let Some(entry) = metadata.next() {
                built.metadata = entry.metadata;
            }
            error = error.with_violation(built);
        }
    }
    error
}

/// A non-negative protobuf duration.
fn duration(delay: &wire::ProtoDuration) -> Option<Duration> {
    let seconds = u64::try_from(delay.seconds).ok()?;
    let nanos = u32::try_from(delay.nanos)
        .ok()
        .filter(|nanos| *nanos < 1_000_000_000)?;
    Some(Duration::new(seconds, nanos))
}

#[cfg(test)]
mod tests {
    use reqwest::header::HeaderValue;

    use super::*;

    /// A test message: one string and one number.
    #[derive(Clone, PartialEq, prost::Message)]
    struct Echo {
        #[prost(string, tag = "1")]
        text: String,
        #[prost(uint32, tag = "2")]
        count: u32,
    }

    fn echo() -> Echo {
        Echo {
            text: "hello".into(),
            count: 7,
        }
    }

    fn frame(flag: u8, payload: &[u8]) -> Vec<u8> {
        let mut out = vec![flag];
        out.extend_from_slice(&u32::try_from(payload.len()).unwrap().to_be_bytes());
        out.extend_from_slice(payload);
        out
    }

    fn trailers(text: &str) -> Vec<u8> {
        frame(FLAG_TRAILERS, text.as_bytes())
    }

    fn reply(body: &[u8]) -> Result<Result<Echo, AppError>, Malformed> {
        decode_reply::<Echo>(&HeaderMap::new(), body)
    }

    #[test]
    fn a_request_frame_round_trips() {
        let encoded = encode(&echo()).unwrap();
        assert_eq!(encoded[0], 0);
        let length = u32::from_be_bytes(encoded[1..5].try_into().unwrap());
        assert_eq!(usize::try_from(length).unwrap(), encoded.len() - HEADER_LEN);
        let frames = split(&encoded).unwrap();
        assert_eq!(frames.trailers, None);
        assert_eq!(frames.messages.len(), 1);
        assert_eq!(Echo::decode(frames.messages[0]).unwrap(), echo());
    }

    #[test]
    fn an_empty_message_is_an_empty_frame() {
        assert_eq!(encode(&()).unwrap(), [0, 0, 0, 0, 0]);
    }

    #[test]
    fn an_ok_reply_decodes_its_message() {
        let mut body = frame(0, &echo().encode_to_vec());
        body.extend(trailers("grpc-status: 0\r\ngrpc-message: \r\n"));
        assert_eq!(reply(&body).unwrap().unwrap(), echo());
    }

    #[test]
    fn trailers_over_lines_and_in_any_case_are_read() {
        let mut body = frame(0, &echo().encode_to_vec());
        body.extend(trailers(
            "Grpc-Status:0\r\nx-other: 1\n\r\nGRPC-MESSAGE: ignored\r\ngrpc-status: 5\r\n",
        ));
        assert_eq!(reply(&body).unwrap().unwrap(), echo());

        let body = trailers("x-first: a\r\ngrpc-status: 5\r\ngrpc-message: no%20such%20order\r\n");
        let error = reply(&body).unwrap().unwrap_err();
        assert_eq!(error.code(), ErrorCode::NotFound);
        assert_eq!(error.message(), "no such order");
    }

    #[test]
    fn a_trailers_only_answer_reads_the_headers() {
        let mut headers = HeaderMap::new();
        headers.insert("grpc-status", HeaderValue::from_static("7"));
        headers.insert("grpc-message", HeaderValue::from_static("not%20yours"));
        let error = decode_reply::<Echo>(&headers, &[]).unwrap().unwrap_err();
        assert_eq!(error.code(), ErrorCode::PermissionDenied);
        assert_eq!(error.message(), "not yours");
        assert_eq!(error.reason(), None);

        // The headers' status wins over a trailer frame.
        let error = decode_reply::<Echo>(&headers, &trailers("grpc-status: 0\r\n"))
            .unwrap()
            .unwrap_err();
        assert_eq!(error.code(), ErrorCode::PermissionDenied);

        // A trailers-only OK carries no message.
        headers.insert("grpc-status", HeaderValue::from_static("0"));
        assert_eq!(
            decode_reply::<Echo>(&headers, &[]).unwrap_err(),
            Malformed::MessageCount
        );
    }

    #[test]
    fn an_unknown_code_number_is_unknown() {
        let error = reply(&trailers("grpc-status: 99\r\n"))
            .unwrap()
            .unwrap_err();
        assert_eq!(error.code(), ErrorCode::Unknown);
        assert_eq!(error.message(), "");
    }

    #[test]
    fn percent_escapes_are_decoded() {
        assert_eq!(percent_decode("a%20b%2Fc%2fd"), "a b/c/d");
        assert_eq!(percent_decode("caf%C3%A9"), "café");
        assert_eq!(percent_decode("100%"), "100%");
        assert_eq!(percent_decode("%zz%4"), "%zz%4");
        assert_eq!(percent_decode("%FF"), "\u{fffd}");
        assert_eq!(percent_decode(""), "");
    }

    #[test]
    fn malformed_replies_are_told_apart() {
        let message = frame(0, &echo().encode_to_vec());
        let status_ok = trailers("grpc-status: 0\r\n");
        let cases: Vec<(Vec<u8>, Malformed)> = vec![
            (vec![], Malformed::NoStatus),
            (message.clone(), Malformed::NoStatus),
            (vec![0, 0, 0], Malformed::Truncated),
            (vec![0, 0, 0, 0, 9, 1], Malformed::Truncated),
            (frame(FLAG_COMPRESSED, b"x"), Malformed::Compressed),
            (
                frame(FLAG_TRAILERS | FLAG_COMPRESSED, b"x"),
                Malformed::Compressed,
            ),
            (frame(0x40, b"x"), Malformed::UnknownFlag),
            (
                [status_ok.clone(), message.clone()].concat(),
                Malformed::AfterTrailers,
            ),
            (trailers("no colon here\r\n"), Malformed::TrailerLine),
            (frame(FLAG_TRAILERS, &[0xff, 0xfe]), Malformed::TrailerLine),
            (trailers("grpc-message: x\r\n"), Malformed::NoStatus),
            (trailers("grpc-status: ok\r\n"), Malformed::BadStatus),
            (trailers("grpc-status: -1\r\n"), Malformed::BadStatus),
            (
                trailers("grpc-status: 99999999999\r\n"),
                Malformed::BadStatus,
            ),
            (trailers("grpc-status:\r\n"), Malformed::BadStatus),
            (status_ok.clone(), Malformed::MessageCount),
            (
                [message.clone(), message, status_ok.clone()].concat(),
                Malformed::MessageCount,
            ),
            (
                [frame(0, &[0xff]), status_ok].concat(),
                Malformed::Undecodable,
            ),
        ];
        for (body, expected) in cases {
            assert_eq!(reply(&body).unwrap_err(), expected, "{body:?}");
        }

        let mut headers = HeaderMap::new();
        headers.insert("grpc-status", HeaderValue::from_bytes(b"\xff").unwrap());
        assert_eq!(
            decode_reply::<Echo>(&headers, &[]).unwrap_err(),
            Malformed::BadStatus
        );
    }

    fn any(type_url: &str, message: &impl Message) -> wire::Any {
        wire::Any {
            type_url: type_url.to_owned(),
            value: message.encode_to_vec(),
        }
    }

    #[test]
    fn details_from_sekvent_errors_decode_exactly() {
        let sent = AppError::failed_precondition("the order is closed")
            .with_reason("ORDER_CLOSED")
            .with_domain("orders.v1")
            .with_metadata("order", "o-1")
            .with_metadata("state", "closed")
            .with_retry_after(Duration::from_millis(1500))
            .with_violation(
                FieldViolation::new("items[0].quantity", "must be positive")
                    .with_reason("NOT_POSITIVE")
                    .with_metadata("min", "1"),
            )
            .with_violation(FieldViolation::new("note", "too long"));
        let status = sekvent_error::grpc::to_status(&sent);
        let text = STANDARD_PAD_INDIFFERENT.encode(status.details());
        for text in [text.as_str(), text.trim_end_matches('=')] {
            let body = trailers(&format!(
                "grpc-status: 9\r\ngrpc-message: the%20order%20is%20closed\r\n\
                 grpc-status-details-bin: {text}\r\n"
            ));
            let got = reply(&body).unwrap().unwrap_err();
            assert_eq!(got.code(), ErrorCode::FailedPrecondition);
            assert_eq!(got.message(), "the order is closed");
            assert_eq!(got.reason(), Some("ORDER_CLOSED"));
            assert_eq!(got.domain(), Some("orders.v1"));
            assert_eq!(got.metadata(), sent.metadata());
            assert_eq!(got.retry_after(), Some(Duration::from_millis(1500)));
            assert_eq!(got.field_violations(), sent.field_violations());
        }
    }

    #[test]
    fn odd_details_are_skipped() {
        let status = wire::RpcStatus {
            code: 14,
            message: String::new(),
            details: vec![
                any("type.googleapis.com/example.Unknown", &echo()),
                wire::Any {
                    type_url: "type.googleapis.com/google.rpc.ErrorInfo".into(),
                    value: vec![0xff],
                },
                wire::Any {
                    type_url: "type.googleapis.com/google.rpc.RetryInfo".into(),
                    value: vec![0xff],
                },
                any(
                    "google.rpc.RetryInfo",
                    &wire::RetryInfo {
                        retry_delay: Some(wire::ProtoDuration {
                            seconds: -1,
                            nanos: 0,
                        }),
                    },
                ),
                any(
                    "type.googleapis.com/google.rpc.BadRequest",
                    &wire::BadRequest {
                        field_violations: vec![wire::Violation {
                            field: "name".into(),
                            description: "empty".into(),
                            reason: String::new(),
                        }],
                    },
                ),
                any(
                    "type.googleapis.com/google.rpc.ErrorInfo",
                    &wire::ErrorInfo {
                        reason: String::new(),
                        domain: "orders.v1".into(),
                        metadata: [("k".to_owned(), "v".to_owned())].into(),
                    },
                ),
            ],
        };
        let body = trailers(&format!(
            "grpc-status: 14\r\ngrpc-status-details-bin: {}\r\n",
            STANDARD_PAD_INDIFFERENT.encode(status.encode_to_vec())
        ));
        let got = reply(&body).unwrap().unwrap_err();
        assert_eq!(got.code(), ErrorCode::Unavailable);
        assert_eq!(got.reason(), None);
        assert_eq!(got.domain(), Some("orders.v1"));
        assert_eq!(got.metadata().get("k").map(String::as_str), Some("v"));
        assert_eq!(got.retry_after(), None);
        assert_eq!(
            got.field_violations(),
            [FieldViolation::new("name", "empty")]
        );
    }

    #[test]
    fn undecodable_details_keep_the_code_and_message() {
        for details in ["!!not base64!!", "AAEC"] {
            let body = trailers(&format!(
                "grpc-status: 3\r\ngrpc-message: bad\r\ngrpc-status-details-bin: {details}\r\n"
            ));
            let got = reply(&body).unwrap().unwrap_err();
            assert_eq!(got.code(), ErrorCode::InvalidArgument);
            assert_eq!(got.message(), "bad");
            assert_eq!(got.reason(), None);
            assert_eq!(got.field_violations(), [] as [FieldViolation; 0]);
        }
    }

    #[test]
    fn durations_are_checked() {
        let at = |seconds, nanos| duration(&wire::ProtoDuration { seconds, nanos });
        assert_eq!(at(2, 500), Some(Duration::new(2, 500)));
        assert_eq!(at(-1, 0), None);
        assert_eq!(at(0, -1), None);
        assert_eq!(at(0, 1_000_000_000), None);
    }
}
