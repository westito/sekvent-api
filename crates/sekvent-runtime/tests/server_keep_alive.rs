//! HTTP/2 keep-alive on a real listener: the server pings idle connections
//! and closes one whose client stops answering. A raw client speaks the
//! frames itself, so it decides whether pings are acknowledged. Real clock,
//! short intervals, every wait bounded.

use std::io;
use std::net::SocketAddr;
use std::time::Duration;

use sekvent_runtime::{Http2KeepAlive, Runtime, RuntimeHandle, Server, Stage, UnitPolicy};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// Upper bound for every test, so a regression fails instead of hanging.
const BOUND: Duration = Duration::from_secs(30);

const PREFACE: &[u8] = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n";
const SETTINGS: u8 = 0x4;
const PING: u8 = 0x6;
const GOAWAY: u8 = 0x7;
const ACK: u8 = 0x1;

/// One HTTP/2 frame as read off the wire.
struct Frame {
    kind: u8,
    flags: u8,
    payload: Vec<u8>,
}

/// A client that sends the preface and an empty SETTINGS frame, then only
/// what each test writes.
struct RawClient {
    stream: TcpStream,
}

impl RawClient {
    async fn connect(addr: SocketAddr) -> Self {
        let mut stream = TcpStream::connect(addr).await.expect("connects");
        stream.write_all(PREFACE).await.expect("preface");
        stream
            .write_all(&header(0, SETTINGS, 0))
            .await
            .expect("settings");
        Self { stream }
    }

    /// The next frame, or `None` once the server closed the connection.
    async fn next_frame(&mut self) -> Option<Frame> {
        let mut head = [0_u8; 9];
        match self.stream.read_exact(&mut head).await {
            Ok(_) => {}
            Err(error) if closed(&error) => return None,
            Err(error) => panic!("reading a frame header failed: {error}"),
        }
        let len = u32::from_be_bytes([0, head[0], head[1], head[2]]);
        let mut payload = vec![0_u8; usize::try_from(len).expect("fits")];
        match self.stream.read_exact(&mut payload).await {
            Ok(_) => {}
            Err(error) if closed(&error) => return None,
            Err(error) => panic!("reading a frame payload failed: {error}"),
        }
        Some(Frame {
            kind: head[3],
            flags: head[4],
            payload,
        })
    }

    async fn send(&mut self, kind: u8, flags: u8, payload: &[u8]) {
        let len = u32::try_from(payload.len()).expect("small frame");
        let mut frame = header(len, kind, flags).to_vec();
        frame.extend_from_slice(payload);
        self.stream.write_all(&frame).await.expect("frame written");
    }
}

/// A frame header on stream 0.
fn header(len: u32, kind: u8, flags: u8) -> [u8; 9] {
    let [_, a, b, c] = len.to_be_bytes();
    [a, b, c, kind, flags, 0, 0, 0, 0]
}

fn closed(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::UnexpectedEof | io::ErrorKind::ConnectionReset | io::ErrorKind::BrokenPipe
    )
}

fn is_ping(frame: &Frame) -> bool {
    frame.kind == PING && frame.flags & ACK == 0
}

async fn serve(keep_alive: Http2KeepAlive) -> (SocketAddr, RuntimeHandle) {
    let server = Server::builder()
        .http2_keep_alive(Some(keep_alive))
        .bind(SocketAddr::from(([127, 0, 0, 1], 0)))
        .await
        .expect("binds");
    let addr = server.local_addr();
    let handle = Runtime::builder()
        .without_signals()
        .unit(
            "api",
            Stage::Ingress,
            UnitPolicy::Critical,
            server.into_unit(),
        )
        .build()
        .expect("builds")
        .start()
        .await
        .expect("starts");
    (addr, handle)
}

const FAST: Http2KeepAlive = Http2KeepAlive {
    interval: Duration::from_millis(100),
    timeout: Duration::from_millis(100),
};

#[tokio::test]
async fn a_client_that_stops_answering_pings_is_disconnected() {
    let (addr, handle) = serve(FAST).await;
    tokio::time::timeout(BOUND, async {
        let mut client = RawClient::connect(addr).await;
        let mut pinged = false;
        let mut went_away = false;
        while let Some(frame) = client.next_frame().await {
            pinged |= is_ping(&frame);
            went_away |= frame.kind == GOAWAY;
        }
        assert!(pinged, "the server pinged the idle connection");
        assert!(went_away, "the server said goodbye before closing");
    })
    .await
    .expect("the connection closed within the bound");
    handle.shutdown();
    handle.wait().await.expect("the runtime stops");
}

#[tokio::test]
async fn a_client_that_answers_pings_stays_connected() {
    let (addr, handle) = serve(FAST).await;
    tokio::time::timeout(BOUND, async {
        let mut client = RawClient::connect(addr).await;
        let mut pings = 0;
        while pings < 3 {
            let frame = client.next_frame().await.expect("still connected");
            assert_ne!(frame.kind, GOAWAY, "the server kept the connection");
            if frame.kind == SETTINGS && frame.flags & ACK == 0 {
                client.send(SETTINGS, ACK, &[]).await;
            } else if is_ping(&frame) {
                pings += 1;
                client.send(PING, ACK, &frame.payload).await;
            }
        }
    })
    .await
    .expect("three pings answered within the bound");
    handle.shutdown();
    handle.wait().await.expect("the runtime stops");
}
