//! The live WebRTC WebSocket, through a real socket (`oneshot` can't drive an
//! upgrade): a token user gets past authentication to the session code.
//!
//!   APP_PROFILE=test-db cargo test --test it_live_ws -- --include-ignored

mod common;

use std::net::SocketAddr;

use common::harness::superuser_token;
use common::test_db::get_test_db;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// The status line of a WebSocket upgrade request to `path`.
async fn upgrade_status(path: &str, token: Option<&str>) -> String {
    let state = zm_api::server::state::AppState::for_test_with_db(get_test_db().await.unwrap());
    let app = common::schema::checked(zm_api::routes::create_router_app(state));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await
        .unwrap();
    });

    let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
    let auth = token
        .map(|t| format!("Authorization: Bearer {t}\r\n"))
        .unwrap_or_default();
    let req = format!(
        "GET {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: Upgrade\r\nUpgrade: websocket\r\n\
         Sec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n{auth}\r\n"
    );
    s.write_all(req.as_bytes()).await.unwrap();
    let mut buf = vec![0u8; 512];
    let n = s.read(&mut buf).await.unwrap();
    String::from_utf8_lossy(&buf[..n])
        .lines()
        .next()
        .unwrap_or_default()
        .to_string()
}

#[tokio::test]
#[ignore = "requires the test database (APP_PROFILE=test-db)"]
async fn a_token_user_reaches_the_session_code() {
    // No live streaming in the test state: past authentication, the handler
    // answers 503. A 401 means it couldn't tell who the viewer is.
    let line = upgrade_status("/api/v3/live/1/webrtc/ws", Some(&superuser_token())).await;
    assert!(line.contains(" 503 "), "{line}");
}

#[tokio::test]
#[ignore = "requires the test database (APP_PROFILE=test-db)"]
async fn no_token_is_refused() {
    let line = upgrade_status("/api/v3/live/1/webrtc/ws", None).await;
    assert!(line.contains(" 401 "), "{line}");
}
