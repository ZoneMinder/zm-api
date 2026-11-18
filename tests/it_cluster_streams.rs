//! Multi-server installs: the long-lived and aggregate requests. The WebRTC
//! signalling WebSocket is relayed to the server that runs the monitor, the
//! live event stream carries every server's alarm states, and the status and
//! live lists reach other servers.
//!
//! The app under test is server 1; server 2 is a stub. Tests that touch the
//! `Servers` table use their own pair of ids.
//!
//! Requires the test database. Run with:
//!   APP_PROFILE=test-db cargo test --test it_cluster_streams -- --include-ignored

mod common;

use std::time::Duration;

use axum::http::{header, StatusCode};
use common::assertions::assert_status;
use common::cluster::{
    down_url, monitor_on, peer, peer_json, serve_app, server_as, server_one, server_row, sse_peer,
    ws_peer, HERE, THERE,
};
use common::fixtures::{grant_monitor_permission, insert_user_with_id, RowGuard};
use common::harness::{superuser_token, token_for, TestApp};
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::{Error as WsError, Message};
use tower::ServiceExt;
use zm_api::entity::sea_orm_active_enums::Permission;
use zm_api::util::authz::UserPermissions;

// ---- The signalling WebSocket ----------------------------------------------

type Client =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

fn ws_path(monitor_id: u32) -> String {
    format!("/api/v3/live/{monitor_id}/webrtc/ws?room=a")
}

/// Connect as a client of the app served at `addr`, offering `zm.v1`.
async fn connect(
    addr: &str,
    path: &str,
    token: &str,
    extra: &[(&str, &str)],
) -> Result<
    (
        Client,
        tokio_tungstenite::tungstenite::handshake::client::Response,
    ),
    WsError,
> {
    let mut request = format!("ws://{addr}{path}").into_client_request().unwrap();
    let headers = request.headers_mut();
    headers.insert(
        header::AUTHORIZATION,
        format!("Bearer {token}").parse().unwrap(),
    );
    headers.insert(header::SEC_WEBSOCKET_PROTOCOL, "zm.v1".parse().unwrap());
    for (name, value) in extra {
        headers.insert(
            header::HeaderName::from_bytes(name.as_bytes()).unwrap(),
            value.parse().unwrap(),
        );
    }
    tokio::time::timeout(
        Duration::from_secs(10),
        tokio_tungstenite::connect_async(request),
    )
    .await
    .expect("connect within 10 s")
}

fn refused_with(result: Result<(Client, impl Sized), WsError>) -> StatusCode {
    match result {
        Err(WsError::Http(response)) => response.status(),
        Err(other) => panic!("expected an HTTP refusal, got {other}"),
        Ok(_) => panic!("expected an HTTP refusal, the socket opened"),
    }
}

async fn next_message(client: &mut Client) -> Message {
    tokio::time::timeout(Duration::from_secs(10), client.next())
        .await
        .expect("a message within 10 s")
        .expect("the socket is open")
        .expect("no error")
}

async fn wait_for(what: &str, mut ready: impl FnMut() -> bool) {
    for _ in 0..100 {
        if ready() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("{what} did not happen within 5 s");
}

#[tokio::test]
#[ignore = "requires the test database (APP_PROFILE=test-db)"]
async fn a_websocket_is_relayed_both_ways() {
    let stub = ws_peer().await;
    let app = server_one(Some(&stub.url)).await;
    let monitor = monitor_on(&app.db, "StreamsWs", THERE).await;
    let _m = RowGuard::monitor(monitor.id);
    let addr = serve_app(&app).await;
    let token = superuser_token();

    let (mut client, answer) = connect(&addr, &ws_path(monitor.id), &token, &[])
        .await
        .expect("the relay opens");

    // The owner's choice of subprotocol reaches the client.
    assert_eq!(answer.headers()[header::SEC_WEBSOCKET_PROTOCOL], "zm.v1");

    client.send(Message::text("hello")).await.unwrap();
    assert_eq!(next_message(&mut client).await, Message::text("peer:hello"));
    client
        .send(Message::Binary(vec![1u8, 2, 3].into()))
        .await
        .unwrap();
    assert_eq!(
        next_message(&mut client).await,
        Message::Binary(vec![3u8, 2, 1].into())
    );

    // The owner saw the same path and query, the caller's token and who
    // relayed it.
    let seen = stub.seen();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].path_and_query, ws_path(monitor.id));
    assert_eq!(
        seen[0].headers[header::AUTHORIZATION],
        format!("Bearer {token}")
    );
    assert_eq!(seen[0].headers["x-zm-forwarded-by"], "1");
    assert_eq!(seen[0].headers[header::SEC_WEBSOCKET_PROTOCOL], "zm.v1");
}

#[tokio::test]
#[ignore = "requires the test database (APP_PROFILE=test-db)"]
async fn a_close_from_the_client_closes_the_owners_socket() {
    let stub = ws_peer().await;
    let app = server_one(Some(&stub.url)).await;
    let monitor = monitor_on(&app.db, "StreamsWsClientClose", THERE).await;
    let _m = RowGuard::monitor(monitor.id);
    let addr = serve_app(&app).await;

    let (mut client, _) = connect(&addr, &ws_path(monitor.id), &superuser_token(), &[])
        .await
        .unwrap();
    client
        .send(Message::Close(Some(CloseFrame {
            code: CloseCode::Library(4001),
            reason: "bye".into(),
        })))
        .await
        .unwrap();

    wait_for("the owner seeing the close", || {
        stub.log() == ["close:4001:bye"]
    })
    .await;
}

#[tokio::test]
#[ignore = "requires the test database (APP_PROFILE=test-db)"]
async fn a_close_from_the_owner_closes_the_clients_socket() {
    let stub = ws_peer().await;
    let app = server_one(Some(&stub.url)).await;
    let monitor = monitor_on(&app.db, "StreamsWsPeerClose", THERE).await;
    let _m = RowGuard::monitor(monitor.id);
    let addr = serve_app(&app).await;

    let (mut client, _) = connect(&addr, &ws_path(monitor.id), &superuser_token(), &[])
        .await
        .unwrap();
    client.send(Message::text("close-me")).await.unwrap();

    match next_message(&mut client).await {
        Message::Close(Some(frame)) => {
            assert_eq!(u16::from(frame.code), 4002);
            assert_eq!(frame.reason.as_str(), "from peer");
        }
        other => panic!("expected the owner's close frame, got {other:?}"),
    }
}

#[tokio::test]
#[ignore = "requires the test database (APP_PROFILE=test-db)"]
async fn a_dropped_client_closes_the_owners_socket() {
    let stub = ws_peer().await;
    let app = server_one(Some(&stub.url)).await;
    let monitor = monitor_on(&app.db, "StreamsWsDrop", THERE).await;
    let _m = RowGuard::monitor(monitor.id);
    let addr = serve_app(&app).await;

    let (client, _) = connect(&addr, &ws_path(monitor.id), &superuser_token(), &[])
        .await
        .unwrap();
    drop(client);

    wait_for("the owner losing its socket", || !stub.log().is_empty()).await;
}

/// The owner is connected to before the client's upgrade is accepted, so a
/// server that is down is an HTTP error the client can read.
#[tokio::test]
#[ignore = "requires the test database (APP_PROFILE=test-db)"]
async fn a_server_that_is_down_is_a_bad_gateway() {
    let app = server_one(Some(&down_url().await)).await;
    let monitor = monitor_on(&app.db, "StreamsWsDown", THERE).await;
    let _m = RowGuard::monitor(monitor.id);
    let addr = serve_app(&app).await;

    let result = connect(&addr, &ws_path(monitor.id), &superuser_token(), &[]).await;

    assert_eq!(refused_with(result), StatusCode::BAD_GATEWAY);
}

#[tokio::test]
#[ignore = "requires the test database (APP_PROFILE=test-db)"]
async fn a_refusal_from_the_owner_is_passed_on() {
    // The stub answers the upgrade with a plain 403.
    let forbidden = peer(StatusCode::FORBIDDEN).await;
    let app = server_one(Some(&forbidden.url)).await;
    let monitor = monitor_on(&app.db, "StreamsWsRefused", THERE).await;
    let _m = RowGuard::monitor(monitor.id);
    let addr = serve_app(&app).await;
    let result = connect(&addr, &ws_path(monitor.id), &superuser_token(), &[]).await;
    assert_eq!(refused_with(result), StatusCode::FORBIDDEN);

    // A 401 means the servers sign tokens with different keys.
    let unauthorized = peer(StatusCode::UNAUTHORIZED).await;
    let app = server_one(Some(&unauthorized.url)).await;
    let addr = serve_app(&app).await;
    let result = connect(&addr, &ws_path(monitor.id), &superuser_token(), &[]).await;
    assert_eq!(refused_with(result), StatusCode::BAD_GATEWAY);
}

#[tokio::test]
#[ignore = "requires the test database (APP_PROFILE=test-db)"]
async fn a_caller_who_cannot_see_the_monitor_never_reaches_the_owner() {
    let stub = ws_peer().await;
    let app = server_one(Some(&stub.url)).await;
    let hidden = monitor_on(&app.db, "StreamsWsHidden", THERE).await;
    let _h = RowGuard::monitor(hidden.id);

    const UID: u32 = 990_231;
    insert_user_with_id(&app.db, UID, "StreamsWsAcl")
        .await
        .expect("user");
    let _u = RowGuard::user(UID);
    grant_monitor_permission(&app.db, hidden.id, UID, Permission::None)
        .await
        .expect("grant");
    let token = token_for(UID, UserPermissions::superuser());
    let addr = serve_app(&app).await;

    let result = connect(&addr, &ws_path(hidden.id), &token, &[]).await;

    assert_eq!(refused_with(result), StatusCode::NOT_FOUND);
    assert!(stub.seen().is_empty());
}

#[tokio::test]
#[ignore = "requires the test database (APP_PROFILE=test-db)"]
async fn a_request_another_server_relayed_is_not_relayed_again() {
    let stub = ws_peer().await;
    let app = server_one(Some(&stub.url)).await;
    let monitor = monitor_on(&app.db, "StreamsWsLoop", THERE).await;
    let _m = RowGuard::monitor(monitor.id);
    let addr = serve_app(&app).await;

    let result = connect(
        &addr,
        &ws_path(monitor.id),
        &superuser_token(),
        &[("x-zm-forwarded-by", "2")],
    )
    .await;

    assert_eq!(refused_with(result), StatusCode::MISDIRECTED_REQUEST);
    assert!(stub.seen().is_empty());
}

#[tokio::test]
#[ignore = "requires the test database (APP_PROFILE=test-db)"]
async fn this_servers_own_monitor_is_not_relayed() {
    let stub = ws_peer().await;
    let app = server_one(Some(&stub.url)).await;
    let monitor = monitor_on(&app.db, "StreamsWsOwn", HERE).await;
    let _m = RowGuard::monitor(monitor.id);
    let addr = serve_app(&app).await;

    let result = connect(&addr, &ws_path(monitor.id), &superuser_token(), &[]).await;

    // The local handler answers: the test app has no live streaming.
    assert_eq!(refused_with(result), StatusCode::SERVICE_UNAVAILABLE);
    assert!(stub.seen().is_empty());
}

/// An `Upgrade` header without a WebSocket handshake is not something the
/// relay can carry.
#[tokio::test]
#[ignore = "requires the test database (APP_PROFILE=test-db)"]
async fn an_upgrade_that_is_not_a_websocket_is_refused() {
    let stub = ws_peer().await;
    let app = server_one(Some(&stub.url)).await;
    let monitor = monitor_on(&app.db, "StreamsWsNot", THERE).await;
    let _m = RowGuard::monitor(monitor.id);

    let r = app
        .request(axum::http::Method::GET, &ws_path(monitor.id))
        .bearer(&superuser_token())
        .header("connection", "upgrade")
        .header("upgrade", "websocket")
        .send()
        .await;

    assert_status(&r, StatusCode::MISDIRECTED_REQUEST);
    assert!(stub.seen().is_empty());
}

// ---- The live event stream -------------------------------------------------

/// Open `/events/stream` as `token`, with these extra headers. The response
/// head only: the body is read with [`read_until`].
async fn open_stream(
    app: &TestApp,
    token: &str,
    extra: &[(&str, &str)],
) -> axum::response::Response {
    let mut request = axum::http::Request::builder()
        .uri("/api/v3/events/stream")
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .extension(axum::extract::ConnectInfo(std::net::SocketAddr::from((
            [127, 0, 0, 1],
            50000,
        ))));
    for (name, value) in extra {
        request = request.header(*name, *value);
    }
    tokio::time::timeout(
        Duration::from_secs(10),
        app.router()
            .oneshot(request.body(axum::body::Body::empty()).unwrap()),
    )
    .await
    .expect("the head within 10 s")
    .unwrap()
}

/// Read the body until `done` accepts what has arrived, or `within` passes.
/// Returns everything read.
async fn read_until(
    response: axum::response::Response,
    within: Duration,
    done: impl Fn(&str) -> bool,
) -> String {
    let mut body = response.into_body().into_data_stream();
    let mut text = String::new();
    let _ = tokio::time::timeout(within, async {
        while let Some(Ok(chunk)) = body.next().await {
            text.push_str(&String::from_utf8_lossy(&chunk));
            if done(&text) {
                return;
            }
        }
    })
    .await;
    text
}

fn alarm(monitor_id: u32, state: &str) -> Value {
    json!({ "type": "alarm_state", "monitor_id": monitor_id, "state": state, "score": 5 })
}

fn has_alarm(text: &str, monitor_id: u32, state: &str) -> bool {
    text.contains(&format!(r#""monitor_id":{monitor_id},"state":"{state}""#))
}

/// Server ids for the stream tests, one pair each: the `Servers` rows are
/// shared database state.
fn ids(n: u32) -> (u32, u32) {
    (7600 + 2 * n, 7601 + 2 * n)
}

const SECONDS: Duration = Duration::from_secs(1);

#[tokio::test]
#[ignore = "requires the test database (APP_PROFILE=test-db)"]
async fn a_peers_alarm_state_reaches_the_client() {
    let (here, there) = ids(0);
    let app_db = common::test_db::get_test_db().await.unwrap();
    let theirs = monitor_on(&app_db, "StreamsSseTheirs", there).await;
    let _t = RowGuard::monitor(theirs.id);
    let ours = monitor_on(&app_db, "StreamsSseOurs", here).await;
    let _o = RowGuard::monitor(ours.id);
    let stub = sse_peer(vec![
        // A monitor the peer does not run: dropped.
        ("alarm_state", alarm(ours.id, "alarm")),
        // A table-backed event: this server reads those itself.
        (
            "event_start",
            json!({ "type": "event_start", "event_id": 987654321u64, "monitor_id": theirs.id }),
        ),
        ("alarm_state", alarm(theirs.id, "alert")),
    ])
    .await;
    let app = server_as(here, there, Some(&stub.url)).await;
    let _a = server_row(&app.db, here, "streams-a").await;
    let _b = server_row(&app.db, there, "streams-b").await;
    let token = superuser_token();

    let response = open_stream(&app, &token, &[]).await;
    assert_eq!(response.status(), StatusCode::OK);
    let text = read_until(response, 10 * SECONDS, |t| has_alarm(t, theirs.id, "alert")).await;

    assert!(has_alarm(&text, theirs.id, "alert"), "{text}");
    assert!(
        !has_alarm(&text, ours.id, "alarm"),
        "a peer can't speak for our monitor: {text}"
    );
    assert!(!text.contains("987654321"), "no duplicated event: {text}");

    let seen = stub.seen();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].path_and_query, "/api/v3/events/stream");
    assert_eq!(
        seen[0].headers[header::AUTHORIZATION],
        format!("Bearer {token}")
    );
    assert_eq!(seen[0].headers["x-zm-forwarded-by"], here.to_string());
}

#[tokio::test]
#[ignore = "requires the test database (APP_PROFILE=test-db)"]
async fn a_client_does_not_get_alarm_states_for_monitors_it_cannot_see() {
    let (here, there) = ids(1);
    let db = common::test_db::get_test_db().await.unwrap();
    let hidden = monitor_on(&db, "StreamsSseHidden", there).await;
    let _h = RowGuard::monitor(hidden.id);
    let visible = monitor_on(&db, "StreamsSseVisible", there).await;
    let _v = RowGuard::monitor(visible.id);
    let stub = sse_peer(vec![
        ("alarm_state", alarm(hidden.id, "alarm")),
        ("alarm_state", alarm(visible.id, "alarm")),
    ])
    .await;
    let app = server_as(here, there, Some(&stub.url)).await;
    let _a = server_row(&app.db, here, "streams-a").await;
    let _b = server_row(&app.db, there, "streams-b").await;

    const UID: u32 = 990_232;
    insert_user_with_id(&app.db, UID, "StreamsSseAcl")
        .await
        .expect("user");
    let _u = RowGuard::user(UID);
    grant_monitor_permission(&app.db, hidden.id, UID, Permission::None)
        .await
        .expect("grant");
    let token = token_for(UID, UserPermissions::superuser());

    let response = open_stream(&app, &token, &[]).await;
    let text = read_until(response, 10 * SECONDS, |t| {
        has_alarm(t, visible.id, "alarm")
    })
    .await;

    assert!(has_alarm(&text, visible.id, "alarm"), "{text}");
    assert!(!has_alarm(&text, hidden.id, "alarm"), "{text}");
}

#[tokio::test]
#[ignore = "requires the test database (APP_PROFILE=test-db)"]
async fn a_relayed_stream_request_follows_nobody() {
    let (here, there) = ids(2);
    let stub = sse_peer(vec![]).await;
    let app = server_as(here, there, Some(&stub.url)).await;
    let _a = server_row(&app.db, here, "streams-a").await;
    let _b = server_row(&app.db, there, "streams-b").await;

    let response = open_stream(&app, &superuser_token(), &[("x-zm-forwarded-by", "9")]).await;
    assert_eq!(response.status(), StatusCode::OK);
    let _ = read_until(response, 2 * SECONDS, |_| false).await;

    assert!(stub.seen().is_empty());
}

#[tokio::test]
#[ignore = "requires the test database (APP_PROFILE=test-db)"]
async fn the_stream_works_while_a_peer_is_down() {
    let (here, there) = ids(3);
    let db = common::test_db::get_test_db().await.unwrap();
    let ours = monitor_on(&db, "StreamsSseDown", here).await;
    let _o = RowGuard::monitor(ours.id);
    let app = server_as(here, there, Some(&down_url().await)).await;
    let _a = server_row(&app.db, here, "streams-a").await;
    let _b = server_row(&app.db, there, "streams-b").await;

    let response = open_stream(&app, &superuser_token(), &[]).await;
    assert_eq!(response.status(), StatusCode::OK);
    // This server's own monitors still report.
    let needle = format!(r#""monitor_id":{}"#, ours.id);
    let text = read_until(response, 10 * SECONDS, |t| t.contains(&needle)).await;

    assert!(text.contains(&needle), "{text}");
}

#[tokio::test]
#[ignore = "requires the test database (APP_PROFILE=test-db)"]
async fn a_single_server_install_follows_nobody() {
    let stub = sse_peer(vec![("alarm_state", alarm(1, "alarm"))]).await;
    // An address for a peer, but no server id here: nothing relays.
    let mut config = zm_api::configure::cluster::ClusterConfig::default();
    config.peers.insert(THERE.to_string(), stub.url.clone());
    let app = TestApp::spawn_with(move |state| {
        state.cluster = std::sync::Arc::new(zm_api::cluster::Cluster::new(None, &config, 0, false));
    })
    .await;
    let _b = server_row(&app.db, 7699, "streams-lone").await;

    let response = open_stream(&app, &superuser_token(), &[]).await;
    assert_eq!(response.status(), StatusCode::OK);
    let _ = read_until(response, 2 * SECONDS, |_| false).await;

    assert!(stub.seen().is_empty());
}

// ---- Monitor status and the live lists ---------------------------------------

#[tokio::test]
#[ignore = "requires the test database (APP_PROFILE=test-db)"]
async fn a_peers_monitor_status_is_relayed() {
    let stub = peer(StatusCode::OK).await;
    let app = server_one(Some(&stub.url)).await;
    let monitor = monitor_on(&app.db, "StreamsStatus", THERE).await;
    let _m = RowGuard::monitor(monitor.id);

    let path = format!("/api/v3/monitor-status/{}", monitor.id);
    let r = app.get(&path, &superuser_token()).await;

    assert_status(&r, StatusCode::OK);
    assert_eq!(r.text(), "from server 2");
    let seen = stub.seen();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].path_and_query, path);
}

#[tokio::test]
#[ignore = "requires the test database (APP_PROFILE=test-db)"]
async fn the_live_lists_answer_for_every_server() {
    let sources = json!({ "active_sources": 0, "sources": [] });
    for (n, path, answer) in [
        (4, "/api/v3/live/sessions", json!([7])),
        (5, "/api/v3/live/sources", sources),
    ] {
        let (here, there) = ids(n);
        let stub = peer_json(StatusCode::OK, answer.clone()).await;
        let app = server_as(here, there, Some(&stub.url)).await;
        let _a = server_row(&app.db, here, "streams-a").await;
        let _b = server_row(&app.db, there, "streams-b").await;

        let r = app
            .get(&format!("{path}?server_id=all"), &superuser_token())
            .await;

        assert_status(&r, StatusCode::MULTI_STATUS);
        let body = r.json::<Value>();
        let servers = body["servers"].as_array().unwrap();
        // Other tests' `Servers` rows may be there too: each of ours once.
        let entries = |id: u32| servers.iter().filter(|e| e["server_id"] == id).count();
        assert_eq!((entries(here), entries(there)), (1, 1), "{body}");
        assert_eq!(servers[0]["server_id"], here, "this server first");
        let remote = servers.iter().find(|e| e["server_id"] == there).unwrap();
        assert_eq!(remote["ok"], true);
        assert_eq!(remote["result"], answer);
        let seen = stub.seen();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].path_and_query, format!("{path}?server_id={there}"));

        // One server by number is relayed as that server's own answer.
        let r = app
            .get(&format!("{path}?server_id={there}"), &superuser_token())
            .await;
        assert_status(&r, StatusCode::OK);
        assert_eq!(r.json::<Value>(), answer);
    }
}
