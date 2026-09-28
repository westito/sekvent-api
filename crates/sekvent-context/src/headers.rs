//! Wire encoding of [`CallContext`](crate::CallContext) as HTTP/gRPC headers.
//!
//! Inbound values are validated before they are trusted with anything: a
//! request id or key must be visible ASCII without spaces and of bounded
//! length, a `traceparent` must have the W3C version-00 shape, and
//! `subject`/`tenant` survive only when the authenticated caller is a trusted
//! link. The component call depth ([`HOPS`]) is read from any caller: it is
//! a safety net against call cycles, not identity. Anything that fails validation is dropped (or, for the request id,
//! replaced) rather than rejected, so a sloppy client still gets served.

use std::time::{Duration, Instant};

use http::{HeaderMap, HeaderValue};

use crate::{CallContext, ServiceIdentity};

/// Request id header.
pub const REQUEST_ID: &str = "x-request-id";
/// gRPC relative deadline, e.g. `250m` (see the gRPC over HTTP/2 spec).
pub const GRPC_TIMEOUT: &str = "grpc-timeout";
/// W3C trace context.
pub const TRACEPARENT: &str = "traceparent";
/// End-user subject, honoured only from trusted links.
pub const SUBJECT: &str = "x-sekvent-subject";
/// Tenant, honoured only from trusted links.
pub const TENANT: &str = "x-sekvent-tenant";
/// Idempotency key.
pub const IDEMPOTENCY_KEY: &str = "idempotency-key";
/// Component call depth.
pub const HOPS: &str = "x-sekvent-hops";

/// Longest accepted request id.
const MAX_REQUEST_ID_LEN: usize = 128;
/// Longest accepted subject, tenant or idempotency key.
const MAX_VALUE_LEN: usize = 255;
/// Most digits a `grpc-timeout` value may carry.
const MAX_TIMEOUT_DIGITS: usize = 8;
/// Largest `grpc-timeout` value.
const MAX_TIMEOUT_VALUE: u128 = 99_999_999;
/// Most digits a hop count may carry.
const MAX_HOPS_DIGITS: usize = 4;

/// `grpc-timeout` units from finest to coarsest, with their size in nanoseconds.
const TIMEOUT_UNITS: [(char, u128); 6] = [
    ('n', 1),
    ('u', 1_000),
    ('m', 1_000_000),
    ('S', 1_000_000_000),
    ('M', 60 * 1_000_000_000),
    ('H', 3_600 * 1_000_000_000),
];

/// Build a server-side context from inbound headers.
///
/// `caller` is the identity established by authentication (or `None`).
/// Untrusted callers' `subject`/`tenant` headers are ignored. A missing or
/// malformed request id is replaced by a fresh one; a malformed
/// `grpc-timeout` is ignored. The hop count is read from any caller as one
/// to four ASCII digits; anything else leaves it at 0.
///
/// The idempotency key is received, not set for this call (see
/// [`CallContext::into_inbound`]): handlers read it, but neither
/// [`inject`] nor [`CallContext::child`] forwards it to another operation.
pub fn from_headers(headers: &HeaderMap, caller: Option<ServiceIdentity>) -> CallContext {
    let mut ctx = CallContext::new();
    if let Some(id) = token(headers, REQUEST_ID, MAX_REQUEST_ID_LEN) {
        ctx = ctx.with_request_id(id);
    }
    if let Some(deadline) = header_str(headers, GRPC_TIMEOUT)
        .and_then(parse_grpc_timeout)
        .and_then(|timeout| Instant::now().checked_add(timeout))
    {
        ctx = ctx.with_deadline(deadline);
    }
    if let Some(traceparent) = header_str(headers, TRACEPARENT).filter(|v| is_traceparent(v)) {
        ctx = ctx.with_traceparent(traceparent);
    }
    if let Some(subject) = token(headers, SUBJECT, MAX_VALUE_LEN) {
        ctx = ctx.with_subject(subject);
    }
    if let Some(tenant) = token(headers, TENANT, MAX_VALUE_LEN) {
        ctx = ctx.with_tenant(tenant);
    }
    if let Some(key) = token(headers, IDEMPOTENCY_KEY, MAX_VALUE_LEN) {
        ctx = ctx.with_idempotency_key(key);
    }
    if let Some(hops) = header_str(headers, HOPS).and_then(parse_hops) {
        ctx = ctx.with_hops(hops);
    }
    if let Some(caller) = caller {
        ctx = ctx.with_caller(caller);
    }
    ctx.sanitize_for_caller().into_inbound()
}

/// Write the whole context onto a header map (the remaining deadline as
/// `grpc-timeout`). Existing values for these names are replaced.
///
/// A field the context does not carry removes any stale header of that name,
/// so a reused header map never leaks a previous call's values; a hop count
/// of 0 counts as not carried. An expired deadline is sent as `1n`, the
/// shortest positive timeout.
///
/// This mirrors the context, which suits re-encoding it for the callee of a
/// component call. The idempotency key is written only when it was set for
/// this call ([`CallContext::outbound_idempotency_key`]); a key received
/// from upstream names the caller's operation and is not forwarded. For a
/// request to another sekvent service use [`propagate`], which leaves the
/// caller's own headers alone and never forwards the idempotency key; for a
/// third-party API use [`propagate_external`].
pub fn inject(ctx: &CallContext, headers: &mut HeaderMap) {
    set(headers, REQUEST_ID, Some(ctx.request_id()));
    let timeout = ctx.remaining().map(encode_grpc_timeout);
    set(headers, GRPC_TIMEOUT, timeout.as_deref());
    set(headers, TRACEPARENT, ctx.traceparent());
    set(headers, SUBJECT, ctx.subject());
    set(headers, TENANT, ctx.tenant());
    set(headers, IDEMPOTENCY_KEY, ctx.outbound_idempotency_key());
    let hops = encode_hops(ctx.hops());
    set(headers, HOPS, hops.as_deref());
}

/// Add the context to the headers of an outbound request without touching
/// what the caller set.
///
/// Request id, `traceparent`, subject, tenant and a hop count above 0 are
/// written only when the caller has not set that header; nothing is ever removed. The one
/// exception is `grpc-timeout`: a caller value longer than the time this
/// context has left (or one that does not parse) is replaced by the
/// remaining time, because a callee must never get more time than its
/// caller has.
///
/// The idempotency key is never propagated: it identifies one request to
/// one service, and reusing an inbound key for several different upstream
/// calls would make the upstream treat them as duplicates. Set it on each
/// outbound request that needs one.
pub fn propagate(ctx: &CallContext, headers: &mut HeaderMap) {
    propagate_external(ctx, headers);
    set_if_absent(headers, SUBJECT, ctx.subject());
    set_if_absent(headers, TENANT, ctx.tenant());
    let hops = encode_hops(ctx.hops());
    set_if_absent(headers, HOPS, hops.as_deref());
}

/// Add the part of the context a third-party service may see to the
/// headers of an outbound request: the request id, `traceparent` and the
/// remaining deadline as `grpc-timeout`, under the same rules as
/// [`propagate`] (the caller's headers win, `grpc-timeout` only narrows).
///
/// Nothing sekvent-internal is written: no subject or tenant, no hop count
/// and no idempotency key. Those mean something only to a sekvent peer that
/// authenticates this service as a link.
pub fn propagate_external(ctx: &CallContext, headers: &mut HeaderMap) {
    set_if_absent(headers, REQUEST_ID, Some(ctx.request_id()));
    if let Some(left) = ctx.remaining() {
        let theirs = header_str(headers, GRPC_TIMEOUT).and_then(parse_grpc_timeout);
        if theirs.is_none_or(|theirs| theirs > left) {
            set(headers, GRPC_TIMEOUT, Some(&encode_grpc_timeout(left)));
        }
    }
    set_if_absent(headers, TRACEPARENT, ctx.traceparent());
}

/// Encode a duration as a `grpc-timeout` value (at most 8 digits, choosing
/// the finest unit that fits).
///
/// The value is truncated to the chosen unit, never rounded up, so the callee
/// never gets more time than the caller has. Zero becomes `1n` because the
/// wire format only allows positive values; anything beyond 99 999 999 hours
/// is capped there.
pub fn encode_grpc_timeout(timeout: std::time::Duration) -> String {
    let nanos = timeout.as_nanos().max(1);
    for (unit, size) in TIMEOUT_UNITS {
        let value = nanos / size;
        if value <= MAX_TIMEOUT_VALUE {
            return format!("{value}{unit}");
        }
    }
    format!("{MAX_TIMEOUT_VALUE}H")
}

/// Parse a `grpc-timeout` value.
///
/// Accepts one to eight ASCII digits followed by one of `H`, `M`, `S`, `m`,
/// `u`, `n`; anything else is `None`.
pub fn parse_grpc_timeout(value: &str) -> Option<std::time::Duration> {
    let unit = value.chars().next_back()?;
    let digits = &value[..value.len() - unit.len_utf8()];
    if digits.is_empty()
        || digits.len() > MAX_TIMEOUT_DIGITS
        || !digits.bytes().all(|b| b.is_ascii_digit())
    {
        return None;
    }
    let amount: u64 = digits.parse().ok()?;
    Some(match unit {
        'H' => Duration::from_secs(amount * 3_600),
        'M' => Duration::from_secs(amount * 60),
        'S' => Duration::from_secs(amount),
        'm' => Duration::from_millis(amount),
        'u' => Duration::from_micros(amount),
        'n' => Duration::from_nanos(amount),
        _ => return None,
    })
}

/// One to four ASCII digits.
fn parse_hops(value: &str) -> Option<u32> {
    if value.is_empty()
        || value.len() > MAX_HOPS_DIGITS
        || !value.bytes().all(|b| b.is_ascii_digit())
    {
        return None;
    }
    value.parse().ok()
}

/// The header value for a hop count; `None` for 0, which is not sent.
fn encode_hops(hops: u32) -> Option<String> {
    (hops > 0).then(|| hops.to_string())
}

fn header_str<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name).and_then(|value| value.to_str().ok())
}

/// A header value that is non-empty visible ASCII (no spaces) of bounded length.
fn token<'a>(headers: &'a HeaderMap, name: &str, max_len: usize) -> Option<&'a str> {
    header_str(headers, name).filter(|value| is_token(value, max_len))
}

fn is_token(value: &str, max_len: usize) -> bool {
    !value.is_empty() && value.len() <= max_len && value.bytes().all(|b| b.is_ascii_graphic())
}

/// `00-<32 hex>-<16 hex>-<2 hex>`, lowercase, with non-zero trace and parent ids.
fn is_traceparent(value: &str) -> bool {
    fn lower_hex(part: &str, len: usize) -> bool {
        part.len() == len && part.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
    }
    fn not_zero(part: &str) -> bool {
        part.bytes().any(|b| b != b'0')
    }

    let mut parts = value.split('-');
    let (Some(version), Some(trace_id), Some(parent_id), Some(flags), None) = (
        parts.next(),
        parts.next(),
        parts.next(),
        parts.next(),
        parts.next(),
    ) else {
        return false;
    };
    version == "00"
        && lower_hex(trace_id, 32)
        && lower_hex(parent_id, 16)
        && lower_hex(flags, 2)
        && not_zero(trace_id)
        && not_zero(parent_id)
}

/// Replace `name` with `value`, or remove it when there is no valid value.
fn set(headers: &mut HeaderMap, name: &'static str, value: Option<&str>) {
    match value.and_then(|value| HeaderValue::from_str(value).ok()) {
        Some(value) => {
            headers.insert(name, value);
        }
        None => {
            headers.remove(name);
        }
    }
}

/// Insert `name` with `value` unless the header is already present.
fn set_if_absent(headers: &mut HeaderMap, name: &'static str, value: Option<&str>) {
    if headers.contains_key(name) {
        return;
    }
    if let Some(value) = value.and_then(|value| HeaderValue::from_str(value).ok()) {
        headers.insert(name, value);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TRACE: &str = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";

    fn headers(pairs: &[(&'static str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.insert(*name, HeaderValue::from_str(value).unwrap());
        }
        map
    }

    #[test]
    fn encoding_picks_the_finest_unit_that_fits() {
        assert_eq!(encode_grpc_timeout(Duration::ZERO), "1n");
        assert_eq!(encode_grpc_timeout(Duration::from_nanos(1)), "1n");
        assert_eq!(
            encode_grpc_timeout(Duration::from_nanos(99_999_999)),
            "99999999n"
        );
        assert_eq!(encode_grpc_timeout(Duration::from_millis(100)), "100000u");
        assert_eq!(encode_grpc_timeout(Duration::from_millis(250)), "250000u");
        assert_eq!(encode_grpc_timeout(Duration::from_secs(5)), "5000000u");
        assert_eq!(encode_grpc_timeout(Duration::from_secs(100)), "100000m");
        assert_eq!(encode_grpc_timeout(Duration::from_secs(100_000)), "100000S");
        assert_eq!(
            encode_grpc_timeout(Duration::from_secs(100_000_000)),
            "1666666M"
        );
        assert_eq!(
            encode_grpc_timeout(Duration::from_mins(100_000_000)),
            "1666666H"
        );
        assert_eq!(encode_grpc_timeout(Duration::MAX), "99999999H");
    }

    #[test]
    fn every_unit_parses() {
        assert_eq!(parse_grpc_timeout("2H"), Some(Duration::from_secs(7_200)));
        assert_eq!(parse_grpc_timeout("3M"), Some(Duration::from_secs(180)));
        assert_eq!(parse_grpc_timeout("4S"), Some(Duration::from_secs(4)));
        assert_eq!(parse_grpc_timeout("250m"), Some(Duration::from_millis(250)));
        assert_eq!(parse_grpc_timeout("7u"), Some(Duration::from_micros(7)));
        assert_eq!(
            parse_grpc_timeout("99999999n"),
            Some(Duration::from_nanos(99_999_999))
        );
        assert_eq!(parse_grpc_timeout("0n"), Some(Duration::ZERO));
    }

    #[test]
    fn malformed_timeouts_are_rejected() {
        for bad in [
            "",
            "m",
            "123",
            "123456789n",
            "-1S",
            "+1S",
            "1.5S",
            "1s",
            "1 S",
            "1é",
            "S1",
        ] {
            assert_eq!(parse_grpc_timeout(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn encoded_values_parse_back_without_gaining_time() {
        for nanos in [1, 999, 1_000_001, 123_456_789_012, 3_600_000_000_000_123] {
            let original = Duration::from_nanos(nanos);
            let parsed = parse_grpc_timeout(&encode_grpc_timeout(original)).unwrap();
            assert!(parsed <= original, "{original:?} became {parsed:?}");
        }
    }

    #[test]
    fn a_trusted_caller_propagates_every_field() {
        let map = headers(&[
            (REQUEST_ID, "req-1"),
            (GRPC_TIMEOUT, "10S"),
            (TRACEPARENT, TRACE),
            (SUBJECT, "user-7"),
            (TENANT, "acme"),
            (IDEMPOTENCY_KEY, "key-1"),
        ]);
        let before = Instant::now();
        let ctx = from_headers(&map, Some(ServiceIdentity::trusted("billing")));
        assert_eq!(ctx.request_id(), "req-1");
        let deadline = ctx.deadline().expect("deadline");
        assert!(deadline >= before + Duration::from_secs(10));
        assert!(deadline <= Instant::now() + Duration::from_secs(10));
        assert_eq!(ctx.traceparent(), Some(TRACE));
        assert_eq!(ctx.subject(), Some("user-7"));
        assert_eq!(ctx.tenant(), Some("acme"));
        assert_eq!(ctx.idempotency_key(), Some("key-1"));
        assert_eq!(ctx.caller(), Some(&ServiceIdentity::trusted("billing")));
    }

    #[test]
    fn untrusted_and_anonymous_callers_cannot_assert_identity() {
        let map = headers(&[
            (SUBJECT, "user-7"),
            (TENANT, "acme"),
            (IDEMPOTENCY_KEY, "k"),
        ]);
        for caller in [None, Some(ServiceIdentity::untrusted("web"))] {
            let ctx = from_headers(&map, caller.clone());
            assert_eq!(ctx.subject(), None);
            assert_eq!(ctx.tenant(), None);
            assert_eq!(ctx.idempotency_key(), Some("k"));
            assert_eq!(ctx.caller(), caller.as_ref());
        }
    }

    #[test]
    fn missing_or_invalid_request_ids_are_replaced() {
        let long = "a".repeat(MAX_REQUEST_ID_LEN + 1);
        let exact = "b".repeat(MAX_REQUEST_ID_LEN);
        assert_eq!(
            from_headers(&headers(&[(REQUEST_ID, &exact)]), None).request_id(),
            exact
        );
        for bad in ["", "has space", long.as_str()] {
            let ctx = from_headers(&headers(&[(REQUEST_ID, bad)]), None);
            assert_ne!(ctx.request_id(), bad);
            assert!(!ctx.request_id().is_empty());
        }
        let mut map = HeaderMap::new();
        map.insert(REQUEST_ID, HeaderValue::from_bytes(b"caf\xc3\xa9").unwrap());
        assert!(from_headers(&map, None).request_id().is_ascii());
        assert!(
            !from_headers(&HeaderMap::new(), None)
                .request_id()
                .is_empty()
        );
    }

    #[test]
    fn malformed_optional_headers_are_dropped() {
        let map = headers(&[
            (GRPC_TIMEOUT, "soon"),
            (
                TRACEPARENT,
                "01-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01",
            ),
            (IDEMPOTENCY_KEY, "two words"),
        ]);
        let ctx = from_headers(&map, None);
        assert_eq!(ctx.deadline(), None);
        assert_eq!(ctx.traceparent(), None);
        assert_eq!(ctx.idempotency_key(), None);
    }

    #[test]
    fn traceparent_shape_is_checked() {
        assert!(is_traceparent(TRACE));
        for bad in [
            "00-4BF92F3577B34DA6A3CE929D0E0E4736-00f067aa0ba902b7-01",
            "00-00000000000000000000000000000000-00f067aa0ba902b7-01",
            "00-4bf92f3577b34da6a3ce929d0e0e4736-0000000000000000-01",
            "00-4bf92f3577b34da6a3ce929d0e0e473-00f067aa0ba902b7-01",
            "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-1",
            "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7",
            "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01-xx",
        ] {
            assert!(!is_traceparent(bad), "{bad}");
        }
    }

    #[test]
    fn inject_writes_everything_and_replaces_stale_values() {
        let ctx = CallContext::new()
            .with_request_id("req-9")
            .with_timeout(Duration::from_secs(30))
            .with_traceparent(TRACE)
            .with_subject("user-1")
            .with_tenant("acme")
            .with_idempotency_key("idem");
        let mut map = headers(&[(REQUEST_ID, "stale"), (GRPC_TIMEOUT, "1H")]);
        inject(&ctx, &mut map);

        assert_eq!(map[REQUEST_ID], "req-9");
        assert_eq!(map.get_all(REQUEST_ID).iter().count(), 1);
        let sent = parse_grpc_timeout(map[GRPC_TIMEOUT].to_str().unwrap()).unwrap();
        assert!(sent <= Duration::from_secs(30) && sent > Duration::from_secs(29));
        assert_eq!(map[TRACEPARENT], TRACE);
        assert_eq!(map[SUBJECT], "user-1");
        assert_eq!(map[TENANT], "acme");
        assert_eq!(map[IDEMPOTENCY_KEY], "idem");

        let back = from_headers(&map, Some(ServiceIdentity::trusted("orders")));
        assert_eq!(back.request_id(), "req-9");
        assert_eq!(back.subject(), Some("user-1"));
    }

    #[test]
    fn inject_removes_fields_the_context_does_not_carry() {
        let mut map = headers(&[
            (GRPC_TIMEOUT, "1H"),
            (TRACEPARENT, TRACE),
            (SUBJECT, "old"),
            (TENANT, "old"),
            (IDEMPOTENCY_KEY, "old"),
        ]);
        inject(&CallContext::new().with_request_id("fresh"), &mut map);
        assert_eq!(map[REQUEST_ID], "fresh");
        for name in [GRPC_TIMEOUT, TRACEPARENT, SUBJECT, TENANT, IDEMPOTENCY_KEY] {
            assert!(map.get(name).is_none(), "{name}");
        }
    }

    #[test]
    fn an_expired_deadline_is_sent_as_one_nanosecond() {
        let ctx = CallContext::new().with_deadline(Instant::now());
        let mut map = HeaderMap::new();
        inject(&ctx, &mut map);
        assert_eq!(map[GRPC_TIMEOUT], "1n");
    }

    #[test]
    fn propagate_fills_only_what_the_caller_left_unset() {
        let ctx = CallContext::new()
            .with_request_id("req-9")
            .with_traceparent(TRACE)
            .with_subject("user-1")
            .with_tenant("acme")
            .with_idempotency_key("inbound-key");
        let mut map = headers(&[(REQUEST_ID, "caller-id"), (SUBJECT, "caller-subject")]);
        propagate(&ctx, &mut map);
        assert_eq!(map[REQUEST_ID], "caller-id");
        assert_eq!(map[SUBJECT], "caller-subject");
        assert_eq!(map[TRACEPARENT], TRACE);
        assert_eq!(map[TENANT], "acme");
        assert!(
            map.get(IDEMPOTENCY_KEY).is_none(),
            "the inbound idempotency key is per hop"
        );
        assert!(map.get(GRPC_TIMEOUT).is_none(), "no deadline, no timeout");

        let mut own_key = headers(&[(IDEMPOTENCY_KEY, "outbound-key")]);
        propagate(&ctx, &mut own_key);
        assert_eq!(own_key[IDEMPOTENCY_KEY], "outbound-key");
        assert_eq!(own_key[REQUEST_ID], "req-9");
    }

    #[test]
    fn propagate_never_removes_headers() {
        let mut map = headers(&[
            (TRACEPARENT, TRACE),
            (SUBJECT, "caller"),
            (TENANT, "caller"),
            (GRPC_TIMEOUT, "1H"),
        ]);
        propagate(&CallContext::new().with_request_id("bad\nid"), &mut map);
        assert!(map.get(REQUEST_ID).is_none(), "an invalid value is skipped");
        assert_eq!(map[TRACEPARENT], TRACE);
        assert_eq!(map[SUBJECT], "caller");
        assert_eq!(map[TENANT], "caller");
        assert_eq!(
            map[GRPC_TIMEOUT], "1H",
            "no deadline keeps the caller's value"
        );
    }

    #[test]
    fn propagate_only_ever_narrows_grpc_timeout() {
        let ctx = CallContext::new().with_timeout(Duration::from_secs(30));
        let mut longer = headers(&[(GRPC_TIMEOUT, "1H")]);
        propagate(&ctx, &mut longer);
        let sent = parse_grpc_timeout(longer[GRPC_TIMEOUT].to_str().unwrap()).unwrap();
        assert!(sent <= Duration::from_secs(30), "narrowed to the time left");

        let mut shorter = headers(&[(GRPC_TIMEOUT, "5S")]);
        propagate(&ctx, &mut shorter);
        assert_eq!(shorter[GRPC_TIMEOUT], "5S", "a tighter caller value stays");

        let mut malformed = headers(&[(GRPC_TIMEOUT, "soon")]);
        propagate(&ctx, &mut malformed);
        assert!(parse_grpc_timeout(malformed[GRPC_TIMEOUT].to_str().unwrap()).is_some());

        let mut absent = HeaderMap::new();
        propagate(&ctx, &mut absent);
        assert_eq!(absent.get_all(GRPC_TIMEOUT).iter().count(), 1);
    }

    #[test]
    fn hops_are_read_as_one_to_four_digits_from_any_caller() {
        for (value, expected) in [("1", 1), ("16", 16), ("0042", 42), ("9999", 9999), ("0", 0)] {
            for caller in [None, Some(ServiceIdentity::untrusted("web"))] {
                let ctx = from_headers(&headers(&[(HOPS, value)]), caller);
                assert_eq!(ctx.hops(), expected, "{value:?}");
            }
        }
        for bad in ["", "10000", "-1", "+1", "1a", "a", " 1", "1.0", "٣"] {
            assert_eq!(parse_hops(bad), None, "{bad:?}");
        }
        for bad in ["", "10000", "-1", "+1", "1a", "abc", "1.0"] {
            let ctx = from_headers(&headers(&[(HOPS, bad)]), None);
            assert_eq!(ctx.hops(), 0, "{bad:?}");
        }
        let mut non_ascii = HeaderMap::new();
        non_ascii.insert(HOPS, HeaderValue::from_bytes(b"\xc3\xa9").unwrap());
        assert_eq!(from_headers(&non_ascii, None).hops(), 0);
        assert_eq!(from_headers(&HeaderMap::new(), None).hops(), 0);
    }

    #[test]
    fn inject_writes_hops_above_zero_and_removes_a_stale_value() {
        let mut map = HeaderMap::new();
        inject(&CallContext::new().with_hops(2), &mut map);
        assert_eq!(map[HOPS], "2");
        assert_eq!(from_headers(&map, None).hops(), 2);

        inject(&CallContext::new(), &mut map);
        assert!(map.get(HOPS).is_none(), "a zero count removes the header");
    }

    #[test]
    fn propagate_keeps_a_callers_hops_and_skips_zero() {
        let mut theirs = headers(&[(HOPS, "7")]);
        propagate(&CallContext::new().with_hops(2), &mut theirs);
        assert_eq!(theirs[HOPS], "7");

        let mut absent = HeaderMap::new();
        propagate(&CallContext::new().with_hops(2), &mut absent);
        assert_eq!(absent[HOPS], "2");

        let mut zero = HeaderMap::new();
        propagate(&CallContext::new(), &mut zero);
        assert!(zero.get(HOPS).is_none());
    }

    #[test]
    fn a_received_idempotency_key_is_not_injected_onward() {
        let inbound = from_headers(&headers(&[(IDEMPOTENCY_KEY, "upstream-key")]), None);
        assert_eq!(inbound.idempotency_key(), Some("upstream-key"));
        let mut onward = headers(&[(IDEMPOTENCY_KEY, "stale")]);
        inject(&inbound, &mut onward);
        assert!(onward.get(IDEMPOTENCY_KEY).is_none());
        inject(&inbound.child(), &mut onward);
        assert!(onward.get(IDEMPOTENCY_KEY).is_none());

        let own = inbound.child().with_idempotency_key("own-key");
        inject(&own, &mut onward);
        assert_eq!(onward[IDEMPOTENCY_KEY], "own-key");
        let back = from_headers(&onward, None);
        assert_eq!(back.idempotency_key(), Some("own-key"));
        assert_eq!(back.outbound_idempotency_key(), None);
    }

    #[test]
    fn external_propagation_leaves_sekvent_headers_out() {
        let ctx = CallContext::new()
            .with_request_id("req-9")
            .with_timeout(Duration::from_secs(30))
            .with_traceparent(TRACE)
            .with_subject("user-1")
            .with_tenant("acme")
            .with_idempotency_key("own-key")
            .with_hops(3);
        let mut map = HeaderMap::new();
        propagate_external(&ctx, &mut map);
        assert_eq!(map[REQUEST_ID], "req-9");
        assert_eq!(map[TRACEPARENT], TRACE);
        let sent = parse_grpc_timeout(map[GRPC_TIMEOUT].to_str().unwrap()).unwrap();
        assert!(sent <= Duration::from_secs(30));
        for name in [SUBJECT, TENANT, IDEMPOTENCY_KEY, HOPS] {
            assert!(map.get(name).is_none(), "{name}");
        }

        let mut theirs = headers(&[(REQUEST_ID, "caller-id"), (GRPC_TIMEOUT, "5S")]);
        propagate_external(&ctx, &mut theirs);
        assert_eq!(theirs[REQUEST_ID], "caller-id");
        assert_eq!(theirs[GRPC_TIMEOUT], "5S");

        let mut unbounded = HeaderMap::new();
        propagate_external(&CallContext::new(), &mut unbounded);
        assert!(unbounded.get(GRPC_TIMEOUT).is_none());
        assert!(unbounded.get(TRACEPARENT).is_none());
    }

    #[test]
    fn a_value_that_is_not_a_valid_header_is_skipped() {
        let ctx = CallContext::new().with_request_id("bad\nid");
        let mut map = headers(&[(REQUEST_ID, "stale")]);
        inject(&ctx, &mut map);
        assert!(map.get(REQUEST_ID).is_none());
    }
}
