//! A stub second server for the multi-server tests.
//!
//! The app under test is server 1. Server 2 is a stub on a real socket that
//! records what reaches it and answers with a fixed status and body.
#![allow(dead_code)]

use std::sync::{Arc, Mutex};

use axum::body::Bytes;
use axum::extract::{Request, State};
use axum::http::{header, HeaderMap, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Router;
use sea_orm::{ActiveModelTrait, DatabaseConnection, EntityTrait, Set};
use zm_api::cluster::Cluster;
use zm_api::configure::cluster::ClusterConfig;
use zm_api::entity::monitors::{ActiveModel as MonitorActiveModel, Model as MonitorModel};

use super::fixtures::{insert_monitor, RowGuard};
use super::harness::TestApp;

pub const HERE: u32 = 1;
pub const THERE: u32 = 2;

/// One request the stub received.
#[derive(Clone)]
pub struct Seen {
    pub method: Method,
    pub path_and_query: String,
    pub headers: HeaderMap,
    pub body: Bytes,
}

#[derive(Clone)]
struct Stub {
    status: StatusCode,
    content_type: &'static str,
    body: String,
    seen: Arc<Mutex<Vec<Seen>>>,
}

pub struct Peer {
    pub url: String,
    seen: Arc<Mutex<Vec<Seen>>>,
}

impl Peer {
    pub fn seen(&self) -> Vec<Seen> {
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
            (header::CONTENT_TYPE, stub.content_type),
            (header::HeaderName::from_static("x-answered-by"), "2"),
        ],
        stub.body,
    )
        .into_response()
}

/// Server 2: answers every request with `status` and a plain-text body.
pub async fn peer(status: StatusCode) -> Peer {
    peer_with(status, "text/plain", "from server 2".to_string()).await
}

/// Server 2: answers every request with `status` and this JSON.
pub async fn peer_json(status: StatusCode, json: serde_json::Value) -> Peer {
    peer_with(status, "application/json", json.to_string()).await
}

async fn peer_with(status: StatusCode, content_type: &'static str, body: String) -> Peer {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let router = Router::new().fallback(record).with_state(Stub {
        status,
        content_type,
        body,
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
pub async fn server_one(peer_url: Option<&str>) -> TestApp {
    server_as(HERE, THERE, peer_url).await
}

/// The app as server `here`, with server `there` at `peer_url` (if any).
pub async fn server_as(here: u32, there: u32, peer_url: Option<&str>) -> TestApp {
    let mut config = ClusterConfig {
        connect_timeout_secs: 1,
        ..Default::default()
    };
    if let Some(url) = peer_url {
        config.peers.insert(there.to_string(), url.to_string());
    }
    TestApp::spawn_with(move |state| {
        state.server_id = Some(here);
        state.cluster = Arc::new(Cluster::new(Some(here), &config, 0, false));
    })
    .await
}

/// A monitor that `server` runs.
pub async fn monitor_on(db: &DatabaseConnection, label: &str, server: u32) -> MonitorModel {
    let monitor = insert_monitor(db, label).await.expect("monitor");
    let mut row: MonitorActiveModel = monitor.into();
    row.server_id = Set(Some(server));
    row.update(db).await.expect("set server")
}

/// A `Servers` row, so the server is known to the cluster. The guard removes it.
pub async fn server_row(db: &DatabaseConnection, id: u32, name: &str) -> RowGuard {
    use zm_api::entity::sea_orm_active_enums::Status;
    let _ = zm_api::entity::servers::Entity::delete_by_id(id)
        .exec(db)
        .await;
    zm_api::entity::servers::ActiveModel {
        id: Set(id),
        name: Set(name.to_string()),
        status: Set(Status::Unknown),
        zmstats: Set(0),
        zmaudit: Set(0),
        zmtrigger: Set(0),
        zmeventnotification: Set(0),
        ..Default::default()
    }
    .insert(db)
    .await
    .expect("insert server row");
    RowGuard::server(id)
}

// ---- Streams: a WebSocket peer, an event-stream peer, a real listener ------

use axum::extract::ws::{CloseFrame, Message, WebSocketUpgrade};
use axum::extract::FromRequestParts;
use axum::response::sse::{Event, Sse};
use futures_util::{stream, StreamExt};

#[derive(Clone)]
struct WsStub {
    seen: Arc<Mutex<Vec<Seen>>>,
    log: Arc<Mutex<Vec<String>>>,
}

/// Server 2 as a WebSocket peer. It selects the subprotocol `zm.v1` when the
/// client offers it, and answers text `t` with `peer:t`, a binary message
/// with its bytes reversed, and the text `close-me` with a close frame
/// (4002, "from peer").
pub struct WsPeer {
    pub url: String,
    seen: Arc<Mutex<Vec<Seen>>>,
    log: Arc<Mutex<Vec<String>>>,
}

impl WsPeer {
    pub fn seen(&self) -> Vec<Seen> {
        self.seen.lock().unwrap().clone()
    }

    /// What the peer observed on its socket, in order: `close:<code>:<reason>`
    /// for a close frame, `gone` for a socket that ended without one.
    pub fn log(&self) -> Vec<String> {
        self.log.lock().unwrap().clone()
    }
}

async fn ws_record(State(stub): State<WsStub>, request: Request) -> Response {
    let (mut parts, _) = request.into_parts();
    stub.seen.lock().unwrap().push(Seen {
        method: parts.method.clone(),
        path_and_query: parts
            .uri
            .path_and_query()
            .map(|pq| pq.as_str().to_string())
            .unwrap_or_default(),
        headers: parts.headers.clone(),
        body: Bytes::new(),
    });
    let Ok(ws) = WebSocketUpgrade::from_request_parts(&mut parts, &()).await else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let log = stub.log.clone();
    ws.protocols(["zm.v1"])
        .on_upgrade(move |mut socket| async move {
            while let Some(message) = socket.next().await {
                let reply = match message {
                    Ok(Message::Text(text)) if text.as_str() == "close-me" => {
                        let _ = socket
                            .send(Message::Close(Some(CloseFrame {
                                code: 4002,
                                reason: "from peer".into(),
                            })))
                            .await;
                        return;
                    }
                    Ok(Message::Text(text)) => Message::text(format!("peer:{}", text.as_str())),
                    Ok(Message::Binary(bytes)) => {
                        Message::Binary(bytes.iter().rev().copied().collect::<Vec<u8>>().into())
                    }
                    Ok(Message::Close(frame)) => {
                        let (code, reason) = frame
                            .map(|f| (f.code, f.reason.as_str().to_string()))
                            .unwrap_or((1005, String::new()));
                        log.lock().unwrap().push(format!("close:{code}:{reason}"));
                        return;
                    }
                    Ok(_) => continue,
                    Err(_) => break,
                };
                if socket.send(reply).await.is_err() {
                    break;
                }
            }
            log.lock().unwrap().push("gone".to_string());
        })
}

pub async fn ws_peer() -> WsPeer {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = Arc::new(Mutex::new(Vec::new()));
    let router = Router::new().fallback(ws_record).with_state(WsStub {
        seen: seen.clone(),
        log: log.clone(),
    });
    WsPeer {
        url: serve_router(router).await,
        seen,
        log,
    }
}

#[derive(Clone)]
struct SseStub {
    events: Arc<Vec<(String, serde_json::Value)>>,
    seen: Arc<Mutex<Vec<Seen>>>,
}

/// Server 2 as an event-stream peer: sends `events` to each subscriber, then
/// holds the stream open.
pub struct SsePeer {
    pub url: String,
    seen: Arc<Mutex<Vec<Seen>>>,
}

impl SsePeer {
    pub fn seen(&self) -> Vec<Seen> {
        self.seen.lock().unwrap().clone()
    }
}

async fn sse_record(State(stub): State<SseStub>, request: Request) -> Response {
    let (parts, _) = request.into_parts();
    stub.seen.lock().unwrap().push(Seen {
        method: parts.method,
        path_and_query: parts
            .uri
            .path_and_query()
            .map(|pq| pq.as_str().to_string())
            .unwrap_or_default(),
        headers: parts.headers,
        body: Bytes::new(),
    });
    let sent: Vec<Result<Event, std::convert::Infallible>> = stub
        .events
        .iter()
        .map(|(name, data)| Ok(Event::default().event(name).data(data.to_string())))
        .collect();
    Sse::new(stream::iter(sent).chain(stream::pending())).into_response()
}

pub async fn sse_peer(events: Vec<(&str, serde_json::Value)>) -> SsePeer {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let events = events
        .into_iter()
        .map(|(name, data)| (name.to_string(), data))
        .collect();
    let router = Router::new().fallback(sse_record).with_state(SseStub {
        events: Arc::new(events),
        seen: seen.clone(),
    });
    SsePeer {
        url: serve_router(router).await,
        seen,
    }
}

/// Serve `router` on a free port; its base URL.
pub async fn serve_router(router: Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    url
}

/// Serve the app on a real socket, for clients that need one (WebSockets);
/// its `host:port`.
pub async fn serve_app(app: &TestApp) -> String {
    serve_router(app.router())
        .await
        .trim_start_matches("http://")
        .to_string()
}

/// A base URL where nothing listens, for a server that is down.
pub async fn down_url() -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    format!("http://{}", listener.local_addr().unwrap())
}
