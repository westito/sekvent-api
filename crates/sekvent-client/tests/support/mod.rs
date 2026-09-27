//! Shared helpers for the integration tests.

use axum::Router;

/// Serve `router` on an ephemeral local port and return its base URL.
///
/// The listener is bound before this returns, so requests made right after
/// queue in the accept backlog instead of racing the server start.
pub(crate) async fn serve(router: Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    format!("http://{address}")
}
