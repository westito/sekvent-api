//! Server-streaming methods served to browsers: gRPC-Web over HTTP/1.1
//! through the runtime server under a path prefix with CORS and bearer
//! tokens, read frame by frame as the items arrive.

mod support;

use std::net::SocketAddr;
use std::time::Duration;

use bytes::{Buf, BufMut, Bytes, BytesMut};
use http::header::{AUTHORIZATION, CONTENT_TYPE, ORIGIN};
use http_body_util::combinators::UnsyncBoxBody;
use http_body_util::{BodyExt as _, Full};
use hyper_util::rt::TokioIo;
use lobby::{GreetRequest, Greeting, Lobby, LobbyHandle};
use prost::Message;
use sekvent_component::{AppError, CallContext, CallStream, END_USER_REJECTED_MESSAGE, EndUser};
use sekvent_config::MapSource;
use sekvent_runtime::{Cors, Runtime, RuntimeHandle, Server, Stage, UnitPolicy};
use support::fakes::{ScriptedTicker, TickerEvents, build_with, tick};
use support::grpc::guarded;
use support::ticker::{Tick, TickerError, TickerHandle, TicksRequest};
use tonic_types::StatusExt as _;

const ORIGIN_URL: &str = "https://shop.example";
const TICKS: &str = "/api/shop.ticker.v1.Ticker/Ticks";
const GREETINGS: &str = "/api/shop.lobby.v1.Lobby/Greetings";

/// A one-method component declared with `#[component]`, for the streaming
/// method kind the hand-expanded ticker does not carry:
/// `#[call(stream, anonymous)]`.
#[allow(dead_code, missing_docs, unreachable_pub)]
mod lobby {
    use sekvent_component::{AppError, CallContext, CallStream};

    /// The contract sekvent-proto-build would generate for
    /// `shop.lobby.v1.Lobby`.
    pub mod proto {
        #[allow(non_upper_case_globals, clippy::type_complexity)]
        pub const __sekvent_service_Lobby: (&str, &[(&str, &str, &str, bool, bool)]) = (
            "shop.lobby.v1.Lobby",
            &[(
                "Greetings",
                "shop.lobby.v1.GreetRequest",
                "shop.lobby.v1.Greeting",
                false,
                true,
            )],
        );
        #[allow(non_camel_case_types)]
        pub type __sekvent_rpc_Lobby__Greetings = (super::GreetRequest, super::Greeting);
    }

    /// Greet a visitor by name.
    #[derive(Clone, PartialEq, prost::Message)]
    pub struct GreetRequest {
        #[prost(string, tag = "1")]
        pub name: String,
    }

    impl prost::Name for GreetRequest {
        const NAME: &'static str = "GreetRequest";
        const PACKAGE: &'static str = "shop.lobby.v1";
    }

    /// One greeting, and whether the handler saw an end user.
    #[derive(Clone, PartialEq, prost::Message)]
    pub struct Greeting {
        #[prost(string, tag = "1")]
        pub text: String,
        #[prost(bool, tag = "2")]
        pub signed_in: bool,
    }

    impl prost::Name for Greeting {
        const NAME: &'static str = "Greeting";
        const PACKAGE: &'static str = "shop.lobby.v1";
    }

    /// A lobby open to everyone.
    #[sekvent_component::component(
        name = "lobby",
        package = "shop.lobby.v1",
        proto = "crate::lobby::proto"
    )]
    pub trait Lobby: Send + Sync + 'static {
        /// A few greetings for the visitor.
        #[call(stream, anonymous)]
        async fn greetings(
            &self,
            cx: &CallContext,
            req: GreetRequest,
        ) -> Result<CallStream<Greeting, AppError>, AppError>;
    }
}

/// The lobby: two greetings, then the end.
struct Doorman;

impl Lobby for Doorman {
    async fn greetings(
        &self,
        cx: &CallContext,
        req: GreetRequest,
    ) -> Result<CallStream<Greeting, AppError>, AppError> {
        let signed_in = cx.end_user().is_some();
        Ok(CallStream::iter(["hello", "welcome"].map(|word| {
            Ok(Greeting {
                text: format!("{word} {}", req.name),
                signed_in,
            })
        })))
    }
}

/// Accepts `Bearer user-<n>` session tokens.
fn sessions(request: &http::request::Parts) -> Result<EndUser, AppError> {
    request
        .headers
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .filter(|token| token.starts_with("user-"))
        .map(|token| EndUser::new(token).with_tenant("t-1"))
        .ok_or_else(|| AppError::unauthenticated("unknown session token"))
}

/// A started runtime serving the scripted ticker and the lobby to end users
/// under `/api`, with CORS and gRPC-Web.
struct Site {
    runtime: RuntimeHandle,
    addr: SocketAddr,
}

impl Site {
    async fn stop(self) {
        self.runtime.shutdown();
        self.runtime.wait().await.unwrap();
    }
}

async fn serve() -> (Site, ScriptedTicker, TickerEvents) {
    let (scripted, events) = ScriptedTicker::new();
    let control = scripted.share();
    let keys: MapSource = [
        ("SEKVENT_COMPONENT_TICKER_SERVE", "grpc"),
        ("SEKVENT_COMPONENT_TICKER_SERVE_AUTH", "bearer"),
        ("SEKVENT_COMPONENT_LOBBY_SERVE", "grpc"),
        ("SEKVENT_COMPONENT_LOBBY_SERVE_AUTH", "bearer"),
    ]
    .into_iter()
    .collect();
    let app = build_with(&keys, move |builder| {
        builder.end_user_authenticator(sessions)?;
        LobbyHandle::install(builder, |_| Ok(Doorman))?;
        TickerHandle::install_with_lifecycle(builder, move |_| Ok(scripted))
    })
    .unwrap();
    let server = Server::builder()
        .prefix("/api")
        .grpc_web(true)
        .cors(Cors::origins([ORIGIN_URL]).unwrap())
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
    (Site { runtime, addr }, control, events)
}

type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// One gRPC-Web body frame.
#[derive(Debug)]
enum WebFrame {
    /// A message (flag `0x00`).
    Data(Bytes),
    /// The trailers (flag `0x80`).
    Trailers(http::HeaderMap),
}

/// An open gRPC-Web response, read frame by frame as the body arrives, on
/// a connection of its own.
struct WebStream {
    status: http::StatusCode,
    headers: http::HeaderMap,
    body: UnsyncBoxBody<Bytes, BoxError>,
    buffer: BytesMut,
    connection: tokio::task::JoinHandle<()>,
}

impl WebStream {
    /// Open `path` with one request message, as a browser does: HTTP/1.1,
    /// `application/grpc-web+proto`, `x-grpc-web: 1`, an `Origin`.
    async fn open(
        addr: SocketAddr,
        path: &str,
        authorization: Option<&str>,
        message: &impl Message,
    ) -> Self {
        let encoded = message.encode_to_vec();
        let mut frame = BytesMut::with_capacity(5 + encoded.len());
        frame.put_u8(0);
        frame.put_u32(u32::try_from(encoded.len()).unwrap());
        frame.put_slice(&encoded);
        let mut request = http::Request::post(path)
            .version(http::Version::HTTP_11)
            .header(http::header::HOST, addr.to_string())
            .header(CONTENT_TYPE, "application/grpc-web+proto")
            .header("x-grpc-web", "1")
            .header(ORIGIN, ORIGIN_URL);
        if let Some(value) = authorization {
            request = request.header(AUTHORIZATION, value);
        }
        let request = request.body(Full::new(frame.freeze())).unwrap();

        let tcp = tokio::net::TcpStream::connect(addr).await.unwrap();
        let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(tcp))
            .await
            .unwrap();
        let connection = tokio::spawn(async move {
            drop(connection.await);
        });
        let response = sender.send_request(request).await.unwrap();
        let (parts, body) = response.into_parts();
        Self {
            status: parts.status,
            headers: parts.headers,
            body: body.map_err(BoxError::from).boxed_unsync(),
            buffer: BytesMut::new(),
            connection,
        }
    }

    /// The next whole frame, waiting for the body to deliver it; `None` at
    /// the end of the body.
    async fn next(&mut self) -> Option<WebFrame> {
        loop {
            if self.buffer.len() >= 5 {
                let length = usize::try_from(u32::from_be_bytes([
                    self.buffer[1],
                    self.buffer[2],
                    self.buffer[3],
                    self.buffer[4],
                ]))
                .unwrap();
                if self.buffer.len() >= 5 + length {
                    let mut frame = self.buffer.split_to(5 + length);
                    let flag = frame.get_u8();
                    frame.advance(4);
                    let payload = frame.freeze();
                    return Some(if flag & 0x80 == 0 {
                        WebFrame::Data(payload)
                    } else {
                        WebFrame::Trailers(parse_trailers(&payload))
                    });
                }
            }
            match self.body.frame().await {
                None => {
                    assert!(self.buffer.is_empty(), "a truncated frame");
                    return None;
                }
                Some(frame) => {
                    if let Ok(data) = frame.unwrap().into_data() {
                        self.buffer.extend_from_slice(&data);
                    }
                }
            }
        }
    }

    /// The next frame, which must be a message decoding to `M`.
    async fn message<M: Message + Default>(&mut self) -> M {
        match self.next().await {
            Some(WebFrame::Data(payload)) => M::decode(payload).unwrap(),
            other => panic!("expected a message, got {other:?}"),
        }
    }

    /// The next frame, which must be the trailers; then the body must end.
    async fn trailers(&mut self) -> tonic::Status {
        let trailers = match self.next().await {
            Some(WebFrame::Trailers(trailers)) => trailers,
            other => panic!("expected the trailers, got {other:?}"),
        };
        assert!(self.next().await.is_none(), "frames after the trailers");
        tonic::Status::from_header_map(&trailers).expect("a grpc-status trailer")
    }

    /// A trailers-only answer: the status from the response headers, and a
    /// body without messages.
    async fn trailers_only(&mut self) -> tonic::Status {
        let status = tonic::Status::from_header_map(&self.headers)
            .expect("a grpc-status in the response headers");
        while let Some(frame) = self.next().await {
            assert!(
                matches!(frame, WebFrame::Trailers(_)),
                "a message in a trailers-only answer: {frame:?}"
            );
        }
        status
    }

    /// Close the connection, as a browser does when the page goes away.
    fn disconnect(self) {
        self.connection.abort();
    }
}

/// `name: value` lines of a trailer frame.
fn parse_trailers(payload: &[u8]) -> http::HeaderMap {
    let mut trailers = http::HeaderMap::new();
    for line in std::str::from_utf8(payload).unwrap().split("\r\n") {
        if let Some((name, value)) = line.split_once(':') {
            trailers.insert(
                http::HeaderName::from_bytes(name.trim().to_ascii_lowercase().as_bytes()).unwrap(),
                value.trim().parse().unwrap(),
            );
        }
    }
    trailers
}

fn series(name: &str) -> TicksRequest {
    TicksRequest {
        series: name.to_owned(),
        from: 0,
    }
}

#[tokio::test]
async fn items_reach_the_browser_as_they_are_produced() {
    guarded(async {
        let (site, control, mut events) = serve().await;
        let feed = control.feed();

        // The response head goes out at the open, before any item exists.
        let mut stream =
            WebStream::open(site.addr, TICKS, Some("Bearer user-7"), &series("feed")).await;
        assert_eq!(stream.status, http::StatusCode::OK);
        assert_eq!(stream.headers["access-control-allow-origin"], ORIGIN_URL);
        assert!(
            stream.headers[CONTENT_TYPE]
                .to_str()
                .unwrap()
                .starts_with("application/grpc-web"),
            "{:?}",
            stream.headers
        );
        let cx = events.opened.recv().await.unwrap();
        assert_eq!(cx.end_user().map(EndUser::subject), Some("user-7"));

        // Each item is readable before the producer makes the next one.
        feed.send(Ok(tick("feed", 1))).await.unwrap();
        assert_eq!(stream.message::<Tick>().await, tick("feed", 1));
        feed.send(Ok(tick("feed", 2))).await.unwrap();
        assert_eq!(stream.message::<Tick>().await, tick("feed", 2));

        // The end is a trailer frame with grpc-status 0.
        drop(feed);
        let status = stream.trailers().await;
        assert_eq!(status.code(), tonic::Code::Ok, "{status:?}");
        events.dropped.recv().await.unwrap();

        site.stop().await;
    })
    .await;
}

#[tokio::test]
async fn a_typed_error_ends_the_stream_in_the_trailer_frame() {
    guarded(async {
        let (site, control, _events) = serve().await;
        let feed = control.feed();
        feed.send(Ok(tick("feed", 1))).await.unwrap();
        feed.send(Err(TickerError::Gap { missing: 3 }))
            .await
            .unwrap();

        let mut stream =
            WebStream::open(site.addr, TICKS, Some("Bearer user-7"), &series("feed")).await;
        assert_eq!(stream.message::<Tick>().await, tick("feed", 1));
        let status = stream.trailers().await;
        assert_eq!(status.code(), tonic::Code::FailedPrecondition, "{status:?}");
        assert_eq!(status.message(), "3 ticks were lost");
        let info = status.get_details_error_info().expect("error details");
        assert_eq!(info.reason, "SERIES_GAP");
        assert_eq!(info.domain, "shop.ticker.v1");
        assert_eq!(info.metadata["missing"], "3");

        site.stop().await;
    })
    .await;
}

#[tokio::test]
async fn open_errors_are_trailers_only() {
    guarded(async {
        let (site, _control, mut events) = serve().await;

        // A typed open error, in the response headers.
        let mut stream =
            WebStream::open(site.addr, TICKS, Some("Bearer user-7"), &series("unknown")).await;
        assert_eq!(stream.status, http::StatusCode::OK);
        assert_eq!(stream.headers["access-control-allow-origin"], ORIGIN_URL);
        let exposed = stream.headers["access-control-expose-headers"]
            .to_str()
            .unwrap()
            .to_owned();
        assert!(exposed.contains("grpc-status"), "{exposed}");
        let status = stream.trailers_only().await;
        assert_eq!(status.code(), tonic::Code::NotFound, "{status:?}");
        let info = status.get_details_error_info().expect("error details");
        assert_eq!(info.reason, "UNKNOWN_SERIES");
        assert_eq!(info.metadata["series"], "unknown");
        events.opened.recv().await.unwrap();

        // No token: refused before the handler runs.
        let mut stream = WebStream::open(site.addr, TICKS, None, &series("feed")).await;
        let status = stream.trailers_only().await;
        assert_eq!(status.code(), tonic::Code::Unauthenticated, "{status:?}");
        assert_eq!(status.message(), END_USER_REJECTED_MESSAGE);
        assert!(events.opened.try_recv().is_err(), "the handler ran");

        site.stop().await;
    })
    .await;
}

#[tokio::test]
async fn an_anonymous_stream_needs_no_token() {
    guarded(async {
        let (site, _control, _events) = serve().await;
        let request = GreetRequest {
            name: "visitor".into(),
        };

        let mut stream = WebStream::open(site.addr, GREETINGS, None, &request).await;
        assert_eq!(stream.status, http::StatusCode::OK);
        for text in ["hello visitor", "welcome visitor"] {
            let greeting = stream.message::<Greeting>().await;
            assert_eq!(greeting.text, text);
            assert!(!greeting.signed_in);
        }
        let status = stream.trailers().await;
        assert_eq!(status.code(), tonic::Code::Ok, "{status:?}");

        site.stop().await;
    })
    .await;
}

#[tokio::test]
async fn a_browser_that_goes_away_lets_the_producer_be_dropped() {
    guarded(async {
        let (site, _control, mut events) = serve().await;

        // An endless producer, read once, then the connection closes.
        let mut stream =
            WebStream::open(site.addr, TICKS, Some("Bearer user-7"), &series("count")).await;
        assert_eq!(stream.message::<Tick>().await, tick("count", 0));
        stream.disconnect();
        events.dropped.recv().await.unwrap();

        site.stop().await;
    })
    .await;
}

#[test]
fn the_lobby_contract_is_a_server_stream() {
    let descriptor = <LobbyHandle as sekvent_component::ComponentHandle>::DESCRIPTOR;
    let greetings = &descriptor.methods()[0];
    assert!(greetings.is_stream());
    assert!(greetings.is_anonymous());
}
