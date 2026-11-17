//! Admission before scope resolution, over the wire: one admission shared
//! by every component an App serves, the end-user precheck refusing what
//! can never be a credential before the scope resolver runs, and every
//! request read whole, bounded in size and time, before resolution. Native
//! gRPC over HTTP/2 and gRPC-Web over HTTP/1.1 through the runtime server.

mod support;

use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Poll};
use std::time::Duration;

use bytes::{Buf, BufMut, Bytes, BytesMut};
use http::header::{AUTHORIZATION, CONTENT_LENGTH, CONTENT_TYPE};
use http_body_util::{BodyExt as _, Full};
use prost::Message;
use sekvent_component::{
    AppError, BearerShape, BuildError, CallContext, CallStream, CallTarget,
    END_USER_REJECTED_MESSAGE, EndUser, EndUserPrecheck, EndUserRejection, ErrorCode, RequestScope,
    ScopeResolver, ServeAdmission, reasons,
};
use sekvent_config::MapSource;
use sekvent_runtime::{Runtime, RuntimeHandle, Server, Stage, UnitPolicy};
use shop::{Billing, BillingHandle, Ledger, LedgerHandle, Note};
use support::fakes::build_with;
use support::grpc::{TOKEN, guarded};
use tokio::sync::Notify;

const CHARGE: &str = "/shop.billing.v1.Billing/Charge";
const QUOTE: &str = "/shop.billing.v1.Billing/Quote";
const BALANCE: &str = "/shop.billing.v1.Ledger/Balance";
const ENTRIES: &str = "/shop.billing.v1.Ledger/Entries";

/// Two components served by one App: `billing`, with a method for
/// signed-in users and an anonymous one, and `ledger`, with an
/// optional-end-user method and a stream for signed-in users.
#[allow(dead_code, missing_docs, unreachable_pub)]
mod shop {
    use sekvent_component::{AppError, CallContext, CallStream};

    /// The contracts sekvent-proto-build would generate for
    /// `shop.billing.v1.Billing` and `shop.billing.v1.Ledger`.
    pub mod proto {
        #[allow(non_upper_case_globals, clippy::type_complexity)]
        pub const __sekvent_service_Billing: (&str, &[(&str, &str, &str, bool, bool)]) = (
            "shop.billing.v1.Billing",
            &[
                (
                    "Charge",
                    "shop.billing.v1.Note",
                    "shop.billing.v1.Note",
                    false,
                    false,
                ),
                (
                    "Quote",
                    "shop.billing.v1.Note",
                    "shop.billing.v1.Note",
                    false,
                    false,
                ),
            ],
        );
        #[allow(non_camel_case_types)]
        pub type __sekvent_rpc_Billing__Charge = (super::Note, super::Note);
        #[allow(non_camel_case_types)]
        pub type __sekvent_rpc_Billing__Quote = (super::Note, super::Note);

        #[allow(non_upper_case_globals, clippy::type_complexity)]
        pub const __sekvent_service_Ledger: (&str, &[(&str, &str, &str, bool, bool)]) = (
            "shop.billing.v1.Ledger",
            &[
                (
                    "Balance",
                    "shop.billing.v1.Note",
                    "shop.billing.v1.Note",
                    false,
                    false,
                ),
                (
                    "Entries",
                    "shop.billing.v1.Note",
                    "shop.billing.v1.Note",
                    false,
                    true,
                ),
            ],
        );
        #[allow(non_camel_case_types)]
        pub type __sekvent_rpc_Ledger__Balance = (super::Note, super::Note);
        #[allow(non_camel_case_types)]
        pub type __sekvent_rpc_Ledger__Entries = (super::Note, super::Note);
    }

    /// A request or a reply: what the handler saw, or some padding.
    #[derive(Clone, PartialEq, prost::Message)]
    pub struct Note {
        #[prost(string, tag = "1")]
        pub text: String,
    }

    impl prost::Name for Note {
        const NAME: &'static str = "Note";
        const PACKAGE: &'static str = "shop.billing.v1";
    }

    /// Charges for signed-in users, quotes for everyone.
    #[sekvent_component::component(
        name = "billing",
        package = "shop.billing.v1",
        proto = "crate::shop::proto"
    )]
    pub trait Billing: Send + Sync + 'static {
        /// For signed-in users.
        #[call]
        async fn charge(&self, cx: &CallContext, req: Note) -> Result<Note, AppError>;
        /// For everyone.
        #[call(anonymous)]
        async fn quote(&self, cx: &CallContext, req: Note) -> Result<Note, AppError>;
    }

    /// Balances for everyone, more for signed-in users; entries for them.
    #[sekvent_component::component(
        name = "ledger",
        package = "shop.billing.v1",
        proto = "crate::shop::proto"
    )]
    pub trait Ledger: Send + Sync + 'static {
        /// For everyone, personal for signed-in users.
        #[call(optional_end_user)]
        async fn balance(&self, cx: &CallContext, req: Note) -> Result<Note, AppError>;
        /// For signed-in users.
        #[call(stream)]
        async fn entries(
            &self,
            cx: &CallContext,
            req: Note,
        ) -> Result<CallStream<Note, AppError>, AppError>;
    }
}

/// Who a handler saw: the end user, the link, or `anyone`.
fn who(cx: &CallContext) -> Note {
    let text = match (cx.end_user(), cx.caller()) {
        (Some(user), _) => user.subject().to_owned(),
        (None, Some(link)) => link.name.clone(),
        (None, None) => "anyone".to_owned(),
    };
    Note { text }
}

/// Both implementations.
#[derive(Clone, Default)]
struct Desk;

impl Billing for Desk {
    async fn charge(&self, cx: &CallContext, _req: Note) -> Result<Note, AppError> {
        Ok(who(cx))
    }

    async fn quote(&self, cx: &CallContext, _req: Note) -> Result<Note, AppError> {
        Ok(who(cx))
    }
}

impl Ledger for Desk {
    async fn balance(&self, cx: &CallContext, _req: Note) -> Result<Note, AppError> {
        Ok(who(cx))
    }

    async fn entries(
        &self,
        cx: &CallContext,
        _req: Note,
    ) -> Result<CallStream<Note, AppError>, AppError> {
        let first = who(cx);
        let second = Note {
            text: "second".into(),
        };
        Ok(CallStream::new(futures::stream::iter([
            Ok(first),
            Ok(second),
        ])))
    }
}

/// Scopes every request to tenant `t-1`; for a request with `x-site: slow`
/// it says so on `entered` and waits for `open` first.
#[derive(Clone, Default)]
struct Sites {
    runs: Arc<AtomicUsize>,
    entered: Arc<Notify>,
    open: Arc<Notify>,
}

impl Sites {
    fn runs(&self) -> usize {
        self.runs.load(Ordering::SeqCst)
    }
}

impl ScopeResolver for Sites {
    fn resolve<'a>(
        &'a self,
        request: &'a http::request::Parts,
        _cx: &'a CallContext,
    ) -> Pin<Box<dyn Future<Output = Result<RequestScope, AppError>> + Send + 'a>> {
        self.runs.fetch_add(1, Ordering::SeqCst);
        let slow = request
            .headers
            .get("x-site")
            .is_some_and(|site| site == "slow");
        Box::pin(async move {
            if slow {
                self.entered.notify_one();
                self.open.notified().await;
            }
            Ok(RequestScope::new().with_tenant("t-1"))
        })
    }
}

/// Accepts `Bearer user-<n>` (tenant `t-1`).
fn sessions(request: &http::request::Parts) -> Result<EndUser, AppError> {
    match BearerShape::token(&request.headers) {
        Some(token) if token.starts_with("user-") => Ok(EndUser::new(token).with_tenant("t-1")),
        _ => Err(AppError::unauthenticated("unknown session token")),
    }
}

/// Refuses calls to `billing` without an `x-api-key` header, with a reason;
/// the bearer shape decides the rest.
fn api_keys(request: &http::request::Parts, target: CallTarget) -> Result<(), AppError> {
    if target.component().name() == "billing" && !request.headers.contains_key("x-api-key") {
        return Err(EndUserRejection::new("API_KEY_REQUIRED").into());
    }
    BearerShape.precheck(request, target)
}

/// A started runtime serving both components to end users and to the link
/// `shop`: native gRPC at the root, gRPC-Web under `/api`.
struct Shop {
    runtime: RuntimeHandle,
    addr: SocketAddr,
    sites: Sites,
}

impl Shop {
    async fn stop(self) {
        self.runtime.shutdown();
        self.runtime.wait().await.unwrap();
    }
}

async fn serve(admission: ServeAdmission, precheck: bool) -> Shop {
    let keys: MapSource = [
        ("SEKVENT_COMPONENT_BILLING_SERVE", "grpc"),
        ("SEKVENT_COMPONENT_BILLING_SERVE_AUTH", "link,bearer"),
        ("SEKVENT_COMPONENT_LEDGER_SERVE", "grpc"),
        ("SEKVENT_COMPONENT_LEDGER_SERVE_AUTH", "link,bearer"),
        ("SEKVENT_LINK_INBOUND_SHOP", TOKEN),
    ]
    .into_iter()
    .collect();
    let sites = Sites::default();
    let app = {
        let sites = sites.clone();
        build_with(&keys, move |builder| {
            builder.serve_admission(admission)?;
            if precheck {
                builder.end_user_precheck(api_keys)?;
            }
            builder.end_user_authenticator(sessions)?;
            builder.scope_resolver(sites)?;
            BillingHandle::install(builder, |_| Ok(Desk))?;
            LedgerHandle::install(builder, |_| Ok(Desk))
        })
        .unwrap()
    };
    let server = Server::builder()
        .prefix("/api")
        .grpc_routes(app.grpc_routes())
        .bind(SocketAddr::from(([127, 0, 0, 1], 0)))
        .await
        .unwrap();
    let addr = server.local_addr();
    let runtime = app
        .register(
            Runtime::builder()
                .without_signals()
                .shutdown_delay(Duration::ZERO),
        )
        .unit(
            "grpc",
            Stage::Ingress,
            UnitPolicy::Critical,
            server.into_unit(),
        )
        .build()
        .unwrap()
        .start()
        .await
        .unwrap();
    Shop {
        runtime,
        addr,
        sites,
    }
}

/// One gRPC frame of `message`.
fn frame(message: &impl Message) -> Bytes {
    let encoded = message.encode_to_vec();
    let mut frame = BytesMut::with_capacity(5 + encoded.len());
    frame.put_u8(0);
    frame.put_u32(u32::try_from(encoded.len()).unwrap());
    frame.put_slice(&encoded);
    frame.freeze()
}

/// A note whose frame carries exactly `bytes` message bytes: the field's
/// tag, its varint length and the text. Panics for a size no such note has
/// (below 2, or one where the varint grows, such as 130).
fn note_of(bytes: usize) -> Note {
    let varint = |mut len: usize| {
        let mut width = 1;
        while len >= 0x80 {
            len >>= 7;
            width += 1;
        }
        width
    };
    let len = (1..=5)
        .filter_map(|width| bytes.checked_sub(1 + width))
        .find(|&len| 1 + varint(len) + len == bytes)
        .expect("a text length");
    let note = Note {
        text: "x".repeat(len),
    };
    assert_eq!(note.encoded_len(), bytes);
    note
}

/// A request body: `data`, then its end, or nothing ever when `stall`.
struct Sent {
    data: Option<Bytes>,
    stall: bool,
}

impl Sent {
    fn whole(message: &impl Message) -> Self {
        Self {
            data: Some(frame(message)),
            stall: false,
        }
    }

    /// The first bytes of a frame, then nothing.
    fn stalled() -> Self {
        Self {
            data: Some(frame(&note_of(32)).slice(..8)),
            stall: true,
        }
    }
}

impl http_body::Body for Sent {
    type Data = Bytes;
    type Error = std::convert::Infallible;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
    ) -> Poll<Option<Result<http_body::Frame<Bytes>, Self::Error>>> {
        if let Some(data) = self.data.take() {
            return Poll::Ready(Some(Ok(http_body::Frame::data(data))));
        }
        if self.stall {
            Poll::Pending
        } else {
            Poll::Ready(None)
        }
    }
}

/// What a call answered: its reply messages and its status.
struct Answer {
    messages: Vec<Note>,
    status: tonic::Status,
}

impl Answer {
    fn error(&self) -> AppError {
        AppError::from(self.status.clone())
    }

    /// The one reply of a successful unary call.
    fn reply(&self) -> &str {
        assert_eq!(self.status.code(), tonic::Code::Ok, "{:?}", self.status);
        assert_eq!(self.messages.len(), 1);
        &self.messages[0].text
    }
}

/// The messages of gRPC frames at the start of `body`, up to a trailer
/// frame (gRPC-Web) or its end, and the rest.
fn messages_of(mut body: Bytes) -> (Vec<Note>, Bytes) {
    let mut messages = Vec::new();
    while body.len() >= 5 && body[0] & 0x80 == 0 {
        let length = usize::try_from(u32::from_be_bytes(body[1..5].try_into().unwrap())).unwrap();
        body.advance(5);
        messages.push(Note::decode(body.split_to(length)).unwrap());
    }
    (messages, body)
}

/// One native gRPC call over HTTP/2 to `path`, with `x-site: site` and
/// the `Authorization` header `authorization`.
async fn h2(
    shop: &Shop,
    path: &str,
    site: &str,
    authorization: Option<&str>,
    body: Sent,
) -> Answer {
    let client = hyper_util::client::legacy::Client::builder(hyper_util::rt::TokioExecutor::new())
        .http2_only(true)
        .build_http::<tonic::body::Body>();
    let mut request = http::Request::post(format!("http://{}{path}", shop.addr))
        .header("x-site", site)
        .header(CONTENT_TYPE, "application/grpc")
        .header("te", "trailers");
    if let Some(value) = authorization {
        request = request.header(AUTHORIZATION, value);
    }
    let request = request.body(tonic::body::Body::new(body)).unwrap();
    let response = client.request(request).await.unwrap();
    if let Some(status) = tonic::Status::from_header_map(response.headers()) {
        return Answer {
            messages: Vec::new(),
            status,
        };
    }
    let collected = response.into_body().collect().await.unwrap();
    let trailers = collected.trailers().cloned().unwrap_or_default();
    let (messages, _) = messages_of(collected.to_bytes());
    Answer {
        messages,
        status: tonic::Status::from_header_map(&trailers).expect("a grpc-status"),
    }
}

/// One gRPC-Web call over HTTP/1.1 under `/api`, as a browser makes it.
async fn web(shop: &Shop, path: &str, authorization: Option<&str>, message: &Note) -> Answer {
    let client = hyper_util::client::legacy::Client::builder(hyper_util::rt::TokioExecutor::new())
        .build_http::<Full<Bytes>>();
    let mut request = http::Request::post(format!("http://{}/api{path}", shop.addr))
        .version(http::Version::HTTP_11)
        .header(CONTENT_TYPE, "application/grpc-web+proto")
        .header("x-grpc-web", "1");
    if let Some(value) = authorization {
        request = request.header(AUTHORIZATION, value);
    }
    let response = client
        .request(request.body(Full::new(frame(message))).unwrap())
        .await
        .unwrap();
    if let Some(status) = tonic::Status::from_header_map(response.headers()) {
        return Answer {
            messages: Vec::new(),
            status,
        };
    }
    web_answer(response.into_body().collect().await.unwrap().to_bytes())
}

/// One gRPC-Web call under `/api` with a `grpc-web-text` body, the frame of
/// `message` in base64 as some browsers send it, over HTTP/1.1 or, with
/// `http2`, over HTTP/2 (where a refusal before the rest is read ends only
/// the stream, never the connection the client is still writing to).
async fn web_text(shop: &Shop, path: &str, message: &Note, http2: bool) -> Answer {
    let client = hyper_util::client::legacy::Client::builder(hyper_util::rt::TokioExecutor::new())
        .http2_only(http2)
        .build_http::<Full<Bytes>>();
    let text = base64(&frame(message));
    let request = http::Request::post(format!("http://{}/api{path}", shop.addr))
        .header(CONTENT_TYPE, "application/grpc-web-text+proto")
        .header(http::header::ACCEPT, "application/grpc-web-text+proto")
        .header(CONTENT_LENGTH, text.len())
        .header("x-grpc-web", "1")
        .body(Full::new(Bytes::from(text)))
        .unwrap();
    let response = client.request(request).await.unwrap();
    if let Some(status) = tonic::Status::from_header_map(response.headers()) {
        return Answer {
            messages: Vec::new(),
            status,
        };
    }
    let body = response.into_body().collect().await.unwrap().to_bytes();
    web_answer(Bytes::from(unbase64(&body)))
}

/// The answer in a decoded gRPC-Web response body: its messages, then its
/// trailer frame.
fn web_answer(body: Bytes) -> Answer {
    let (messages, mut rest) = messages_of(body);
    rest.advance(5);
    let mut trailers = http::HeaderMap::new();
    for line in std::str::from_utf8(&rest).unwrap().split("\r\n") {
        if let Some((name, value)) = line.split_once(':') {
            trailers.insert(
                http::HeaderName::from_bytes(name.trim().to_ascii_lowercase().as_bytes()).unwrap(),
                value.trim().parse().unwrap(),
            );
        }
    }
    Answer {
        messages,
        status: tonic::Status::from_header_map(&trailers).expect("a grpc-status"),
    }
}

const BASE64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Standard base64 with padding.
fn base64(bytes: &[u8]) -> String {
    let mut text = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let group = (u32::from(chunk[0]) << 16)
            | (u32::from(chunk.get(1).copied().unwrap_or(0)) << 8)
            | u32::from(chunk.get(2).copied().unwrap_or(0));
        for index in 0..4 {
            if index <= chunk.len() {
                let sextet = (group >> (18 - 6 * index)) & 0x3f;
                text.push(char::from(BASE64[usize::try_from(sextet).unwrap()]));
            } else {
                text.push('=');
            }
        }
    }
    text
}

/// Padded standard base64, possibly several padded chunks one after the
/// other, as a server encodes each chunk of a `grpc-web-text` response.
fn unbase64(text: &[u8]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(text.len() / 4 * 3);
    for group in text.chunks(4) {
        let sextets: Vec<u32> = group
            .iter()
            .take_while(|&&symbol| symbol != b'=')
            .map(|&symbol| {
                let index = BASE64.iter().position(|&known| known == symbol);
                u32::try_from(index.expect("a base64 symbol")).unwrap()
            })
            .collect();
        let value = sextets
            .iter()
            .chain(std::iter::repeat(&0))
            .take(4)
            .fold(0_u32, |value, sextet| (value << 6) | sextet);
        let [_, first, second, third] = value.to_be_bytes();
        bytes.extend_from_slice(&[first, second, third][..sextets.len().saturating_sub(1)]);
    }
    bytes
}

const USER: Option<&str> = Some("Bearer user-1");

#[tokio::test]
async fn one_admission_is_shared_by_every_served_component() {
    guarded(async {
        let shop = Arc::new(serve(ServeAdmission::new().with_max_pending(1), false).await);

        // An anonymous billing call whose scope takes its time holds the
        // only slot.
        let held = {
            let shop = Arc::clone(&shop);
            tokio::spawn(async move {
                h2(&shop, QUOTE, "slow", None, Sent::whole(&Note::default()))
                    .await
                    .reply()
                    .to_owned()
            })
        };
        shop.sites.entered.notified().await;

        // A call to the ledger is refused before its request is read.
        let refused = h2(&shop, BALANCE, "main", None, Sent::stalled()).await;
        let error = refused.error();
        assert_eq!(error.code(), ErrorCode::ResourceExhausted);
        assert_eq!(error.reason(), Some(reasons::ADMISSION_FULL));
        assert_eq!(error.metadata()["component"], "ledger");
        let refused = h2(&shop, ENTRIES, "main", USER, Sent::stalled()).await;
        assert_eq!(refused.error().reason(), Some(reasons::ADMISSION_FULL));
        // A link caller takes no slot.
        let link = format!("Bearer {TOKEN}");
        let answer = h2(
            &shop,
            CHARGE,
            "main",
            Some(&link),
            Sent::whole(&Note::default()),
        )
        .await;
        assert_eq!(answer.reply(), "shop");
        assert_eq!(shop.sites.runs(), 1, "only the held call was resolved");

        shop.sites.open.notify_one();
        assert_eq!(held.await.unwrap(), "anyone");
        let answer = h2(&shop, BALANCE, "main", None, Sent::whole(&Note::default())).await;
        assert_eq!(answer.reply(), "anyone");
        assert_eq!(shop.sites.runs(), 2);
        Arc::into_inner(shop).unwrap().stop().await;
    })
    .await;
}

#[tokio::test]
async fn a_call_without_a_credential_never_reaches_the_resolver() {
    guarded(async {
        let shop = serve(ServeAdmission::new(), false).await;

        // Answered from the head although the request never ends.
        for (path, authorization) in [
            (CHARGE, None),
            (CHARGE, Some("Basic dXNlcg==")),
            (CHARGE, Some("Bearer user one")),
            (BALANCE, Some("Basic dXNlcg==")),
            (ENTRIES, None),
        ] {
            let answer = h2(&shop, path, "main", authorization, Sent::stalled()).await;
            assert_eq!(
                answer.status.code(),
                tonic::Code::Unauthenticated,
                "{path} {authorization:?}"
            );
            assert_eq!(answer.status.message(), END_USER_REJECTED_MESSAGE);
        }
        assert_eq!(shop.sites.runs(), 0);

        // Anonymous and optional methods without a header, and a signed-in
        // user, are resolved and served.
        let note = Note::default();
        assert_eq!(
            h2(&shop, QUOTE, "main", None, Sent::whole(&note))
                .await
                .reply(),
            "anyone"
        );
        assert_eq!(
            h2(&shop, BALANCE, "main", None, Sent::whole(&note))
                .await
                .reply(),
            "anyone"
        );
        assert_eq!(
            h2(&shop, CHARGE, "main", USER, Sent::whole(&note))
                .await
                .reply(),
            "user-1"
        );
        // A server-streaming call still streams.
        let answer = h2(&shop, ENTRIES, "main", USER, Sent::whole(&note)).await;
        assert_eq!(answer.status.code(), tonic::Code::Ok, "{:?}", answer.status);
        let texts: Vec<&str> = answer
            .messages
            .iter()
            .map(|note| note.text.as_str())
            .collect();
        assert_eq!(texts, ["user-1", "second"]);
        assert_eq!(shop.sites.runs(), 4);
        shop.stop().await;
    })
    .await;
}

#[tokio::test]
async fn a_registered_precheck_replaces_the_bearer_shape() {
    guarded(async {
        let shop = serve(ServeAdmission::new(), true).await;
        let note = Note::default();
        let answer = h2(&shop, CHARGE, "main", USER, Sent::stalled()).await;
        let error = answer.error();
        assert_eq!(error.code(), ErrorCode::Unauthenticated);
        assert_eq!(error.message(), END_USER_REJECTED_MESSAGE);
        assert_eq!(error.reason(), Some("API_KEY_REQUIRED"));
        // The ledger is left to the bearer shape.
        assert_eq!(
            h2(&shop, BALANCE, "main", USER, Sent::whole(&note))
                .await
                .reply(),
            "user-1"
        );
        let answer = h2(&shop, BALANCE, "main", Some("Basic x"), Sent::stalled()).await;
        assert_eq!(answer.status.code(), tonic::Code::Unauthenticated);
        assert_eq!(shop.sites.runs(), 1);
        shop.stop().await;
    })
    .await;
}

#[test]
fn the_builder_refuses_unusable_bounds_and_a_second_precheck() {
    let source = MapSource::new();
    let mut builder = sekvent_component::App::builder(&source);
    for (admission, needle) in [
        (ServeAdmission::new().with_max_pending(0), "max_pending"),
        (
            ServeAdmission::new().with_max_pending_per_client(0),
            "max_pending_per_client",
        ),
        (
            ServeAdmission::new().with_read_timeout(Duration::MAX),
            "read_timeout",
        ),
        (
            ServeAdmission::new().with_read_timeout(Duration::ZERO),
            "read_timeout",
        ),
        (
            ServeAdmission::new().with_max_message_bytes(ServeAdmission::MAX_MESSAGE_BYTES + 1),
            "max_message_bytes",
        ),
    ] {
        let error = builder.serve_admission(admission).unwrap_err();
        assert!(
            matches!(&error, BuildError::InvalidServeAdmission { reason } if reason.contains(needle)),
            "{error:?}"
        );
    }
    let tuned = ServeAdmission::new().with_max_pending(7);
    builder.serve_admission(tuned).unwrap();
    builder.end_user_precheck(BearerShape).unwrap();
    let error = builder.end_user_precheck(api_keys).unwrap_err();
    assert!(
        matches!(error, BuildError::DuplicateEndUserPrecheck),
        "{error:?}"
    );
    let debug = format!("{builder:?}");
    assert!(debug.contains("end_user_precheck: true"), "{debug}");
    assert!(debug.contains("max_pending: 7"), "{debug}");
    builder.build().unwrap();
}

#[tokio::test]
async fn requests_are_read_within_the_admission_bounds() {
    guarded(async {
        // The default read timeout, so no call here races a short one.
        let shop = serve(ServeAdmission::new().with_max_message_bytes(64), false).await;

        // One larger than the limit is refused, natively and over gRPC-Web.
        let large = note_of(100);
        let answer = h2(&shop, QUOTE, "main", None, Sent::whole(&large)).await;
        assert_eq!(answer.error().code(), ErrorCode::OutOfRange);
        assert_eq!(answer.error().reason(), Some(reasons::REQUEST_TOO_LARGE));
        assert_eq!(answer.error().metadata()["max_bytes"], "64");
        let answer = web(&shop, QUOTE, None, &large).await;
        assert_eq!(answer.error().reason(), Some(reasons::REQUEST_TOO_LARGE));
        let answer = web(&shop, ENTRIES, USER, &large).await;
        assert_eq!(answer.error().reason(), Some(reasons::REQUEST_TOO_LARGE));
        assert_eq!(shop.sites.runs(), 0, "none of them was resolved");

        // gRPC-Web unary calls and streams within the limit are served.
        let fits = note_of(64);
        assert_eq!(web(&shop, QUOTE, None, &fits).await.reply(), "anyone");
        assert_eq!(web(&shop, CHARGE, USER, &fits).await.reply(), "user-1");
        let answer = web(&shop, ENTRIES, USER, &fits).await;
        assert_eq!(answer.status.code(), tonic::Code::Ok, "{:?}", answer.status);
        assert_eq!(answer.messages.len(), 2);
        assert_eq!(shop.sites.runs(), 3);

        // In `grpc-web-text` the limit is on the decoded message, although
        // the base64 body (and its length) is a third larger.
        assert!(base64(&frame(&fits)).len() > 64 + 5);
        assert_eq!(web_text(&shop, QUOTE, &fits, false).await.reply(), "anyone");
        let answer = web_text(&shop, QUOTE, &note_of(65), false).await;
        let error = answer.error();
        assert_eq!(error.code(), ErrorCode::OutOfRange);
        assert_eq!(error.reason(), Some(reasons::REQUEST_TOO_LARGE));
        assert_eq!(error.metadata()["max_bytes"], "64");
        assert_eq!(shop.sites.runs(), 4);
        shop.stop().await;
    })
    .await;
}

#[tokio::test]
async fn grpc_web_text_takes_the_largest_message_although_its_body_is_larger() {
    guarded(async {
        let shop = serve(ServeAdmission::new(), false).await;
        let max = ServeAdmission::MAX_MESSAGE_BYTES;

        // A message of exactly the limit, whose base64 body is well over it.
        let fits = note_of(max);
        assert!(base64(&frame(&fits)).len() > max + 5);
        for http2 in [false, true] {
            let answer = web_text(&shop, QUOTE, &fits, http2).await;
            assert_eq!(answer.reply(), "anyone", "http2: {http2}");
        }

        // One byte more is refused.
        let answer = web_text(&shop, QUOTE, &note_of(max + 1), true).await;
        let error = answer.error();
        assert_eq!(error.code(), ErrorCode::OutOfRange);
        assert_eq!(error.reason(), Some(reasons::REQUEST_TOO_LARGE));
        assert_eq!(error.metadata()["max_bytes"], max.to_string());
        assert_eq!(shop.sites.runs(), 2);
        shop.stop().await;
    })
    .await;
}
