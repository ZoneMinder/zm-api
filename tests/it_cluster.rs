//! Multi-server installs: a request for another server's monitor is relayed
//! to the zm-api there (#230).
//!
//! The app under test is server 1. Server 2 is a stub that records what
//! reaches it and answers with a fixed status.
//!
//! Requires the test database — run with:
//!   APP_PROFILE=test-db cargo test --test it_cluster -- --include-ignored

mod common;

use std::sync::{Arc, Mutex};

use axum::body::Bytes;
use axum::extract::{Request, State};
use axum::http::{header, HeaderMap, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Router;
use common::assertions::{assert_error, assert_status};
use common::fixtures::{insert_monitor, RowGuard};
use common::harness::{superuser_token, TestApp};
use sea_orm::{ActiveModelTrait, DatabaseConnection, Set};
use serde_json::json;
use zm_api::cluster::Cluster;
use zm_api::configure::cluster::ClusterConfig;
use zm_api::entity::monitors::{ActiveModel as MonitorActiveModel, Model as MonitorModel};

const HERE: u32 = 1;
const THERE: u32 = 2;

/// One request the stub received.
#[derive(Clone)]
struct Seen {
    method: Method,
    path_and_query: String,
    headers: HeaderMap,
    body: Bytes,
}

#[derive(Clone)]
struct Stub {
    status: StatusCode,
    seen: Arc<Mutex<Vec<Seen>>>,
}

struct Peer {
    url: String,
    seen: Arc<Mutex<Vec<Seen>>>,
}

impl Peer {
    fn seen(&self) -> Vec<Seen> {
        self.seen.lock().unwrap().clone()
    }
}

async fn record(State(stub): State<Stub>, request: Request) -> Response {
    let (parts, body) = request.into_parts();
    let body = axum::body::to_bytes(body, usize::MAX).await.unwrap();
    stub.seen.lock().unwrap().push(Seen {
        method: parts.method,
        path_and_query: parts
            .uri
            .path_and_query()
            .map(|pq| pq.as_str().to_string())
            .unwrap_or_default(),
        headers: parts.headers,
        body,
    });
    (
        stub.status,
        [
            (header::CONTENT_TYPE, "text/plain"),
            (header::HeaderName::from_static("x-answered-by"), "2"),
        ],
        "from server 2",
    )
        .into_response()
}

/// Server 2: answers every request with `status`.
async fn peer(status: StatusCode) -> Peer {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let router = Router::new().fallback(record).with_state(Stub {
        status,
        seen: seen.clone(),
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    Peer { url, seen }
}

/// The app as server 1, with server 2 at `peer_url` (if any).
async fn server_one(peer_url: Option<&str>) -> TestApp {
    let mut config = ClusterConfig {
        connect_timeout_secs: 1,
        ..Default::default()
    };
    if let Some(url) = peer_url {
        config.peers.insert(THERE.to_string(), url.to_string());
    }
    TestApp::spawn_with(move |state| {
        state.server_id = Some(HERE);
        state.cluster = Arc::new(Cluster::new(Some(HERE), &config, 0, false));
    })
    .await
}

/// A monitor that `server` runs.
async fn monitor_on(db: &DatabaseConnection, label: &str, server: u32) -> MonitorModel {
    let monitor = insert_monitor(db, label).await.expect("monitor");
    let mut row: MonitorActiveModel = monitor.into();
    row.server_id = Set(Some(server));
    row.update(db).await.expect("set server")
}

#[tokio::test]
#[ignore = "requires the test database (APP_PROFILE=test-db)"]
async fn another_servers_monitor_is_answered_by_that_server() {
    let peer = peer(StatusCode::OK).await;
    let app = server_one(Some(&peer.url)).await;
    let monitor = monitor_on(&app.db, "ClusterRelay", THERE).await;
    let _m = RowGuard::monitor(monitor.id);
    let token = superuser_token();

    let path = format!("/api/v3/monitors/{}/snapshot?scale=50", monitor.id);
    let r = app.get(&path, &token).await;

    // The owner's answer comes back as it was sent.
    assert_status(&r, StatusCode::OK);
    assert_eq!(r.text(), "from server 2");
    assert_eq!(r.headers["x-answered-by"], "2");
    assert_eq!(r.headers[header::CONTENT_TYPE], "text/plain");

    // And the owner saw the same request, from the same caller.
    let seen = peer.seen();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].method, Method::GET);
    assert_eq!(seen[0].path_and_query, path);
    assert_eq!(
        seen[0].headers[header::AUTHORIZATION],
        format!("Bearer {token}")
    );
    assert_eq!(seen[0].headers["x-zm-forwarded-by"], "1");
}

#[tokio::test]
#[ignore = "requires the test database (APP_PROFILE=test-db)"]
async fn a_write_is_relayed_with_its_body() {
    let peer = peer(StatusCode::ACCEPTED).await;
    let app = server_one(Some(&peer.url)).await;
    let monitor = monitor_on(&app.db, "ClusterWrite", THERE).await;
    let _m = RowGuard::monitor(monitor.id);

    let body = json!({ "action": "on", "cause": "relay-test" });
    let r = app
        .patch_json(
            &format!("/api/v3/monitors/{}/alarm", monitor.id),
            &superuser_token(),
            &body,
        )
        .await;

    assert_status(&r, StatusCode::ACCEPTED);
    let seen = peer.seen();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].method, Method::PATCH);
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&seen[0].body).unwrap(),
        body
    );
    assert_eq!(seen[0].headers[header::CONTENT_TYPE], "application/json");
}

/// The live routes are nested under `/api/v3/live/{monitor_id}`, which strips
/// that prefix from the URI a handler sees. The owner must get the full path.
#[tokio::test]
#[ignore = "requires the test database (APP_PROFILE=test-db)"]
async fn a_nested_live_route_is_relayed_at_its_full_path() {
    let peer = peer(StatusCode::OK).await;
    let app = server_one(Some(&peer.url)).await;
    let monitor = monitor_on(&app.db, "ClusterLive", THERE).await;
    let _m = RowGuard::monitor(monitor.id);

    let path = format!("/api/v3/live/{}/hls/live.m3u8", monitor.id);
    let r = app.get(&path, &superuser_token()).await;

    assert_status(&r, StatusCode::OK);
    assert_eq!(peer.seen()[0].path_and_query, path);
}

#[tokio::test]
#[ignore = "requires the test database (APP_PROFILE=test-db)"]
async fn this_servers_own_monitor_is_not_relayed() {
    let peer = peer(StatusCode::OK).await;
    let app = server_one(Some(&peer.url)).await;
    let monitor = monitor_on(&app.db, "ClusterLocal", HERE).await;
    let _m = RowGuard::monitor(monitor.id);

    let r = app
        .get(
            &format!("/api/v3/monitors/{}/snapshot", monitor.id),
            &superuser_token(),
        )
        .await;

    assert_ne!(r.text(), "from server 2");
    assert!(peer.seen().is_empty());
}

/// Any server can read a monitor's row, so that never leaves this one.
#[tokio::test]
#[ignore = "requires the test database (APP_PROFILE=test-db)"]
async fn reading_another_servers_monitor_row_stays_here() {
    let peer = peer(StatusCode::OK).await;
    let app = server_one(Some(&peer.url)).await;
    let monitor = monitor_on(&app.db, "ClusterRow", THERE).await;
    let _m = RowGuard::monitor(monitor.id);

    let r = app
        .get(
            &format!("/api/v3/monitors/{}", monitor.id),
            &superuser_token(),
        )
        .await;

    assert_status(&r, StatusCode::OK);
    let row: serde_json::Value = r.json();
    assert_eq!(row["id"], monitor.id);
    assert_eq!(row["server_id"], THERE);
    assert!(peer.seen().is_empty());
}

/// A caller who can't see the monitor gets the usual 404, and nothing about
/// them or the request reaches the other server.
#[tokio::test]
#[ignore = "requires the test database (APP_PROFILE=test-db)"]
async fn a_caller_who_cannot_see_the_monitor_is_not_relayed() {
    use common::fixtures::{grant_monitor_permission, insert_user_with_id};
    use common::harness::token_for;
    use zm_api::entity::sea_orm_active_enums::Permission;
    use zm_api::util::authz::UserPermissions;

    let peer = peer(StatusCode::OK).await;
    let app = server_one(Some(&peer.url)).await;
    let hidden = monitor_on(&app.db, "ClusterHidden", THERE).await;
    let _h = RowGuard::monitor(hidden.id);
    let visible = monitor_on(&app.db, "ClusterVisible", THERE).await;
    let _v = RowGuard::monitor(visible.id);

    // Denied one monitor by its own permission row. The rest follow the
    // account's base level.
    const UID: u32 = 990_230;
    insert_user_with_id(&app.db, UID, "ClusterAcl")
        .await
        .expect("user");
    let _u = RowGuard::user(UID);
    grant_monitor_permission(&app.db, hidden.id, UID, Permission::None)
        .await
        .expect("grant");
    let token = token_for(UID, UserPermissions::superuser());

    let r = app
        .get(&format!("/api/v3/monitors/{}/snapshot", hidden.id), &token)
        .await;
    assert_status(&r, StatusCode::NOT_FOUND);
    assert!(peer.seen().is_empty());

    // The one they can see is relayed.
    let r = app
        .get(&format!("/api/v3/monitors/{}/snapshot", visible.id), &token)
        .await;
    assert_status(&r, StatusCode::OK);
    assert_eq!(peer.seen().len(), 1);
}

/// Two servers that disagree about who owns a monitor answer with an error.
/// They do not pass the request back and forth.
#[tokio::test]
#[ignore = "requires the test database (APP_PROFILE=test-db)"]
async fn a_request_that_was_already_relayed_is_not_relayed_again() {
    let peer = peer(StatusCode::OK).await;
    let app = server_one(Some(&peer.url)).await;
    let monitor = monitor_on(&app.db, "ClusterLoop", THERE).await;
    let _m = RowGuard::monitor(monitor.id);

    let r = app
        .request(
            Method::GET,
            &format!("/api/v3/monitors/{}/snapshot", monitor.id),
        )
        .bearer(&superuser_token())
        .header("x-zm-forwarded-by", "2")
        .send()
        .await;

    assert_error(
        &r,
        StatusCode::MISDIRECTED_REQUEST,
        "MISDIRECTED_REQUEST_ERROR",
    );
    assert!(peer.seen().is_empty());
}

#[tokio::test]
#[ignore = "requires the test database (APP_PROFILE=test-db)"]
async fn an_owner_with_no_address_is_named_in_the_error() {
    let app = server_one(None).await;
    let monitor = monitor_on(&app.db, "ClusterNoAddr", THERE).await;
    let _m = RowGuard::monitor(monitor.id);

    let r = app
        .get(
            &format!("/api/v3/monitors/{}/snapshot", monitor.id),
            &superuser_token(),
        )
        .await;

    assert_error(
        &r,
        StatusCode::MISDIRECTED_REQUEST,
        "MISDIRECTED_REQUEST_ERROR",
    );
    assert!(r.text().contains("server 2"), "{}", r.text());
}

#[tokio::test]
#[ignore = "requires the test database (APP_PROFILE=test-db)"]
async fn an_owner_that_does_not_answer_is_a_bad_gateway() {
    // A port nothing listens on.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let dead = format!("http://{}", listener.local_addr().unwrap());
    drop(listener);

    let app = server_one(Some(&dead)).await;
    let monitor = monitor_on(&app.db, "ClusterDown", THERE).await;
    let _m = RowGuard::monitor(monitor.id);
    let token = superuser_token();

    let r = app
        .get(
            &format!("/api/v3/monitors/{}/snapshot?token={token}", monitor.id),
            &token,
        )
        .await;

    assert_error(&r, StatusCode::BAD_GATEWAY, "BAD_GATEWAY_ERROR");
    // The message names the server and leaks neither its address nor the token.
    let text = r.text();
    assert!(text.contains("server 2"), "{text}");
    assert!(!text.contains("127.0.0.1"), "{text}");
    assert!(!text.contains(&token), "{text}");
}

/// This server accepted the token, so an owner that refuses it has different
/// signing keys. That is a setup fault, reported as one, and not a 401 that
/// would send the client to log in again for nothing.
#[tokio::test]
#[ignore = "requires the test database (APP_PROFILE=test-db)"]
async fn an_owner_that_refuses_the_token_is_reported_as_a_key_mismatch() {
    let peer = peer(StatusCode::UNAUTHORIZED).await;
    let app = server_one(Some(&peer.url)).await;
    let monitor = monitor_on(&app.db, "ClusterKeys", THERE).await;
    let _m = RowGuard::monitor(monitor.id);

    let r = app
        .get(
            &format!("/api/v3/monitors/{}/snapshot", monitor.id),
            &superuser_token(),
        )
        .await;

    assert_error(&r, StatusCode::BAD_GATEWAY, "BAD_GATEWAY_ERROR");
    assert!(r.text().contains("JWT keys"), "{}", r.text());
}

/// An owner's own errors pass through: a 404 there is a 404 here.
#[tokio::test]
#[ignore = "requires the test database (APP_PROFILE=test-db)"]
async fn the_owners_error_status_passes_through() {
    let peer = peer(StatusCode::CONFLICT).await;
    let app = server_one(Some(&peer.url)).await;
    let monitor = monitor_on(&app.db, "ClusterStatus", THERE).await;
    let _m = RowGuard::monitor(monitor.id);

    let r = app
        .get(
            &format!("/api/v3/monitors/{}/snapshot", monitor.id),
            &superuser_token(),
        )
        .await;

    assert_status(&r, StatusCode::CONFLICT);
    assert_eq!(r.text(), "from server 2");
}

#[tokio::test]
#[ignore = "requires the test database (APP_PROFILE=test-db)"]
async fn a_websocket_for_another_servers_monitor_is_refused_with_its_address() {
    let peer = peer(StatusCode::OK).await;
    let app = server_one(Some(&peer.url)).await;
    let monitor = monitor_on(&app.db, "ClusterWs", THERE).await;
    let _m = RowGuard::monitor(monitor.id);
    let token = superuser_token();

    let r = app
        .request(
            Method::GET,
            &format!("/api/v3/live/{}/webrtc/ws?token={token}", monitor.id),
        )
        .bearer(&token)
        .header("connection", "upgrade")
        .header("upgrade", "websocket")
        .send()
        .await;

    assert_error(
        &r,
        StatusCode::MISDIRECTED_REQUEST,
        "MISDIRECTED_REQUEST_ERROR",
    );
    assert!(r.text().contains(&peer.url), "{}", r.text());
    assert!(peer.seen().is_empty());
}

/// Moving a monitor to another server is saved here, by the server that ran
/// it, which then asks the new owner to start its daemons.
#[tokio::test]
#[ignore = "requires the test database (APP_PROFILE=test-db)"]
async fn a_monitor_moved_to_another_server_is_started_there() {
    let peer = peer(StatusCode::OK).await;
    let app = server_one(Some(&peer.url)).await;
    let monitor = monitor_on(&app.db, "ClusterMove", HERE).await;
    let _m = RowGuard::monitor(monitor.id);
    let token = superuser_token();

    let r = app
        .patch_json(
            &format!("/api/v3/monitors/{}", monitor.id),
            &token,
            &json!({ "server_id": THERE }),
        )
        .await;

    assert_status(&r, StatusCode::OK);
    let saved: serde_json::Value = r.json();
    assert_eq!(saved["server_id"], THERE);

    let seen = peer.seen();
    assert_eq!(seen.len(), 1, "one call, to restart it on its new server");
    assert_eq!(seen[0].method, Method::PATCH);
    assert_eq!(
        seen[0].path_and_query,
        format!("/api/v3/monitors/{}/state", monitor.id)
    );
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&seen[0].body).unwrap(),
        json!({ "state": "restart" })
    );
    assert_eq!(
        seen[0].headers[header::AUTHORIZATION],
        format!("Bearer {token}")
    );
    assert_eq!(seen[0].headers["x-zm-forwarded-by"], "1");
}

/// A single-server install has no server id, and nothing is ever relayed.
#[tokio::test]
#[ignore = "requires the test database (APP_PROFILE=test-db)"]
async fn a_single_server_install_relays_nothing() {
    let peer = peer(StatusCode::OK).await;
    let mut config = ClusterConfig::default();
    config.peers.insert(THERE.to_string(), peer.url.clone());
    let app = TestApp::spawn_with(move |state| {
        state.server_id = None;
        state.cluster = Arc::new(Cluster::new(None, &config, 0, false));
    })
    .await;
    let monitor = monitor_on(&app.db, "ClusterSingle", THERE).await;
    let _m = RowGuard::monitor(monitor.id);

    let r = app
        .get(
            &format!("/api/v3/monitors/{}/snapshot", monitor.id),
            &superuser_token(),
        )
        .await;

    assert_ne!(r.text(), "from server 2");
    assert!(peer.seen().is_empty());
}

/// What each server re-reads to learn of a logout another server handled.
#[tokio::test]
#[ignore = "requires the test database (APP_PROFILE=test-db)"]
async fn token_floors_lists_only_users_who_have_one() {
    use common::fixtures::insert_user_with_id;
    use sea_orm::EntityTrait;
    use zm_api::entity::users;

    let app = TestApp::spawn().await;
    const REVOKED: u32 = 990_231;
    const CLEAN: u32 = 990_232;
    for (uid, label) in [(REVOKED, "ClusterFloor"), (CLEAN, "ClusterNoFloor")] {
        insert_user_with_id(&app.db, uid, label)
            .await
            .expect("user");
    }
    let _r = RowGuard::user(REVOKED);
    let _c = RowGuard::user(CLEAN);
    let mut row: users::ActiveModel = users::Entity::find_by_id(REVOKED)
        .one(&app.db)
        .await
        .unwrap()
        .unwrap()
        .into();
    row.token_min_expiry = Set(1_800_000_000);
    row.update(&app.db).await.expect("set floor");

    let floors = zm_api::repo::users::token_floors(&app.db).await.unwrap();

    assert!(floors.contains(&(REVOKED, 1_800_000_000)));
    assert!(!floors.iter().any(|(uid, _)| *uid == CLEAN));
}

/// Server 2 as a real zm-api: the same router, on a real socket, counting
/// what it handles.
async fn real_server_two() -> (String, Arc<std::sync::atomic::AtomicUsize>) {
    use std::sync::atomic::{AtomicUsize, Ordering};

    let app = TestApp::spawn_with(|state| {
        state.server_id = Some(THERE);
        state.cluster = Arc::new(Cluster::new(
            Some(THERE),
            &ClusterConfig::default(),
            0,
            false,
        ));
    })
    .await;
    let handled = Arc::new(AtomicUsize::new(0));
    let count = handled.clone();
    let router = app.router().layer(axum::middleware::from_fn(
        move |request, next: axum::middleware::Next| {
            let count = count.clone();
            async move {
                count.fetch_add(1, Ordering::SeqCst);
                next.run(request).await
            }
        },
    ));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    (url, handled)
}

/// Both ends real: server 1 relays an edit, and server 2's own router
/// accepts the same token, applies the caller's permissions and saves it.
#[tokio::test]
#[ignore = "requires the test database (APP_PROFILE=test-db)"]
async fn two_real_servers_share_one_token() {
    use std::sync::atomic::Ordering;

    let (two, handled) = real_server_two().await;
    let one = server_one(Some(&two)).await;
    let monitor = monitor_on(&one.db, "ClusterReal", THERE).await;
    let _m = RowGuard::monitor(monitor.id);
    let path = format!("/api/v3/monitors/{}", monitor.id);

    let r = one
        .patch_json(
            &path,
            &superuser_token(),
            &json!({ "notes": "edited via server 1" }),
        )
        .await;

    assert_status(&r, StatusCode::OK);
    let saved: serde_json::Value = r.json();
    assert_eq!(saved["notes"], "edited via server 1");
    assert_eq!(handled.load(Ordering::SeqCst), 1, "server 2 handled it");

    // A caller with view-only access is still refused an edit.
    let viewer = common::harness::token_with(zm_api::util::authz::UserPermissions {
        monitors: zm_api::util::authz::Level::View,
        ..zm_api::util::authz::UserPermissions::superuser()
    });
    let r = one
        .patch_json(&path, &viewer, &json!({ "notes": "nope" }))
        .await;
    assert_status(&r, StatusCode::FORBIDDEN);
}
