//! A loopback HTTP server that answers every request with fixed bytes and
//! reports each request head, for tests without an object store.

use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpListener;
use tokio::sync::mpsc;

use crate::StorageConfig;

pub(crate) struct Fake {
    pub(crate) endpoint: String,
    requests: mpsc::UnboundedReceiver<String>,
}

impl Fake {
    /// The head of the next request (request line and headers).
    pub(crate) async fn request(&mut self) -> String {
        self.requests.recv().await.expect("a request arrived")
    }
}

/// Serve `response` (a complete HTTP/1.1 response) to every request; with
/// `None`, accept and never answer.
pub(crate) async fn fake(response: Option<&'static str>) -> Fake {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let (tx, requests) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let tx = tx.clone();
            tokio::spawn(async move {
                let mut head = Vec::new();
                let mut buffer = [0_u8; 4096];
                while !head.windows(4).any(|w| w == b"\r\n\r\n") {
                    match socket.read(&mut buffer).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => head.extend_from_slice(&buffer[..n]),
                    }
                }
                let _ = tx.send(String::from_utf8_lossy(&head).into_owned());
                match response {
                    Some(response) => {
                        let _ = socket.write_all(response.as_bytes()).await;
                        let _ = socket.shutdown().await;
                    }
                    None => std::future::pending::<()>().await,
                }
            });
        }
    });
    Fake { endpoint, requests }
}

/// A complete response with `status`, extra header lines (each ending in
/// `\r\n`) and `body`, its length computed.
pub(crate) fn response(status: &str, headers: &str, body: &str) -> &'static str {
    Box::leak(
        format!(
            "HTTP/1.1 {status}\r\n{headers}content-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        )
        .into_boxed_str(),
    )
}

/// A path-style config for `endpoint` with the documented example keys.
pub(crate) fn config(endpoint: &str) -> StorageConfig {
    StorageConfig {
        endpoint: endpoint.to_owned(),
        public_endpoint: None,
        region: "us-east-1".to_owned(),
        bucket: "files".to_owned(),
        access_key_id: "AKIAIOSFODNN7EXAMPLE".to_owned(),
        secret_access_key: "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY".into(),
        session_token: None,
        path_style: true,
        timeout: std::time::Duration::from_secs(10),
    }
}
