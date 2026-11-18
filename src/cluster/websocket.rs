//! Relay a WebSocket to the zm-api on the server that owns the monitor.
//!
//! The relay layer ([`super::forward`]) sees any upgrade request for a
//! monitor another server runs. This server checks the caller first, then
//! opens its own WebSocket to the same path on the owner, and only then
//! accepts the client's upgrade. A server that is down or refuses the
//! connection therefore gives the client an ordinary HTTP error instead of a
//! socket that closes at once.
//!
//! Messages are copied one at a time in both directions, so a slow side slows
//! the other and nothing queues without bound. A socket that carries
//! signalling can sit idle for minutes, so there is no read timeout; the
//! connect timeout covers the connection and the handshake.

use std::time::Duration;

use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade};
use axum::extract::{FromRequestParts, Request};
use axum::http::{header, HeaderMap, HeaderName, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::protocol::CloseFrame as PeerCloseFrame;
use tokio_tungstenite::tungstenite::{Error as PeerError, Message as PeerMessage};
use tokio_tungstenite::Connector;
use tracing::{debug, warn};

use super::forward::{relayed_headers, FORWARDED_BY};
use super::Peer;
use crate::error::AppError;
use crate::server::state::AppState;

/// How long one side may take to answer a close after the other closed.
const CLOSE_GRACE: Duration = Duration::from_secs(5);

/// The only headers the owner gets besides the ones the WebSocket client
/// makes itself (key, version, host): the caller's credentials and
/// subprotocols, and who relayed it.
const PASSED_ON: [HeaderName; 4] = [
    header::AUTHORIZATION,
    header::SEC_WEBSOCKET_PROTOCOL,
    FORWARDED_BY,
    HeaderName::from_static("x-forwarded-for"),
];

/// The WebSocket URL for a peer's HTTP base URL and a path with query.
/// `None` when the base URL is neither `http` nor `https`.
fn socket_url(base_url: &str, path_and_query: &str) -> Option<String> {
    let rest = match base_url.split_once("://")? {
        ("https", rest) => format!("wss://{rest}"),
        ("http", rest) => format!("ws://{rest}"),
        _ => return None,
    };
    Some(format!("{rest}{path_and_query}"))
}

/// What goes on the handshake to the owner.
fn handshake_headers(relayed: &HeaderMap) -> HeaderMap {
    relayed
        .iter()
        .filter(|(name, _)| PASSED_ON.contains(name))
        .map(|(name, value)| (name.clone(), value.clone()))
        .collect()
}

/// Relay an upgrade request to `peer`. `what` is the start of a refusal, as
/// for the HTTP relay.
pub(crate) async fn relay(
    state: &AppState,
    peer: &Peer,
    what: &str,
    path_and_query: &str,
    request: Request,
) -> Result<Response, AppError> {
    let (mut parts, _) = request.into_parts();
    // Something that asks to upgrade but isn't a WebSocket handshake is not
    // something this relay can carry.
    let Ok(upgrade) = WebSocketUpgrade::from_request_parts(&mut parts, &()).await else {
        return Err(AppError::MisdirectedError(format!(
            "{what} {}; this upgrade request is not a WebSocket handshake, so it can't be relayed",
            peer.label()
        )));
    };

    let client = parts
        .extensions
        .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
        .map(|info| info.0.ip());
    let headers = handshake_headers(&relayed_headers(
        &parts.headers,
        client,
        state.cluster.this_server(),
    ));
    let url = socket_url(&peer.base_url, path_and_query).ok_or_else(|| {
        AppError::MisdirectedError(format!(
            "{} has an address that is not http(s)",
            peer.label()
        ))
    })?;
    let mut handshake = url
        .as_str()
        .into_client_request()
        .map_err(|e| AppError::InternalServerError(format!("building the handshake: {e}")))?;
    handshake.headers_mut().extend(headers);

    let connector = state.cluster.ws_tls().map(Connector::Rustls);
    let connected = tokio::time::timeout(
        state.cluster.connect_timeout(),
        tokio_tungstenite::connect_async_tls_with_config(handshake, None, false, connector),
    )
    .await;
    let (socket, answer) = match connected {
        Ok(Ok(connected)) => connected,
        Ok(Err(PeerError::Http(answer))) => return refusal(peer, *answer),
        // The URL can carry a token in its query, and the error text of some
        // failures names it, so only the kind is logged.
        Ok(Err(e)) => {
            warn!("{} did not accept a WebSocket: {}", peer.label(), kind(&e));
            return Err(unreachable(peer));
        }
        Err(_) => {
            warn!("{} did not answer a WebSocket in time", peer.label());
            return Err(unreachable(peer));
        }
    };

    // Echo the subprotocol the owner chose. The client offered it, or the
    // owner would not have picked it.
    let chosen = answer
        .headers()
        .get(header::SEC_WEBSOCKET_PROTOCOL)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let upgrade = match chosen {
        Some(protocol) => upgrade.protocols([protocol]),
        None => upgrade,
    };
    let label = peer.label();
    debug!("relaying a WebSocket to {label}");
    Ok(upgrade.on_upgrade(move |client| pump(client, socket, label)))
}

fn unreachable(peer: &Peer) -> AppError {
    AppError::BadGatewayError(format!("{} did not answer", peer.label()))
}

/// The owner answered the handshake with something other than an upgrade.
fn refusal(
    peer: &Peer,
    answer: tokio_tungstenite::tungstenite::http::Response<Option<Vec<u8>>>,
) -> Result<Response, AppError> {
    let status = answer.status();
    // This server accepted the credentials a moment ago, so only different
    // signing keys explain a refusal.
    if status == StatusCode::UNAUTHORIZED {
        warn!(
            "{} refused credentials this server accepted: every server must use the same JWT \
             key files",
            peer.label()
        );
        return Err(AppError::BadGatewayError(format!(
            "{} refused credentials this server accepted; the servers' JWT keys differ",
            peer.label()
        )));
    }
    Ok(RelayedAnswer::new(status, answer).into_response())
}

/// The variant name of a connection error, without its text.
fn kind(e: &PeerError) -> &'static str {
    match e {
        PeerError::Io(_) => "connection failed",
        PeerError::Tls(_) => "TLS failed",
        PeerError::Url(_) => "bad address",
        PeerError::Protocol(_) => "handshake failed",
        _ => "connection failed",
    }
}

fn to_peer(message: Message) -> PeerMessage {
    match message {
        Message::Text(text) => PeerMessage::text(text.as_str()),
        Message::Binary(bytes) => PeerMessage::Binary(bytes),
        Message::Ping(bytes) => PeerMessage::Ping(bytes),
        Message::Pong(bytes) => PeerMessage::Pong(bytes),
        Message::Close(frame) => PeerMessage::Close(frame.map(|f| PeerCloseFrame {
            code: CloseCode::from(f.code),
            reason: f.reason.as_str().into(),
        })),
    }
}

/// `None` for a raw frame, which a reader never yields.
fn to_client(message: PeerMessage) -> Option<Message> {
    Some(match message {
        PeerMessage::Text(text) => Message::text(text.as_str()),
        PeerMessage::Binary(bytes) => Message::Binary(bytes),
        PeerMessage::Ping(bytes) => Message::Ping(bytes),
        PeerMessage::Pong(bytes) => Message::Pong(bytes),
        PeerMessage::Close(frame) => Message::Close(frame.map(|f| CloseFrame {
            code: u16::from(f.code),
            reason: f.reason.as_str().into(),
        })),
        PeerMessage::Frame(_) => return None,
    })
}

/// 1011: the relay itself failed, so the client can tell it from a close
/// the owner chose.
fn relay_failed(reason: &'static str) -> CloseFrame {
    CloseFrame {
        code: 1011,
        reason: reason.into(),
    }
}

/// Copy messages both ways until one side closes or fails, then close the
/// other.
async fn pump<S>(client: WebSocket, peer: tokio_tungstenite::WebSocketStream<S>, label: String)
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send,
{
    let (mut to_client_sink, mut from_client) = client.split();
    let (mut to_peer_sink, mut from_peer) = peer.split();

    let upstream = async {
        while let Some(message) = from_client.next().await {
            let Ok(message) = message else { break };
            let closing = matches!(message, Message::Close(_));
            if to_peer_sink.send(to_peer(message)).await.is_err() || closing {
                break;
            }
        }
        // The client left or failed: tell the owner, if it has not heard.
        let _ = to_peer_sink.send(PeerMessage::Close(None)).await;
    };
    let downstream = async {
        loop {
            match from_peer.next().await {
                Some(Ok(message)) => {
                    let Some(message) = to_client(message) else {
                        continue;
                    };
                    let closing = matches!(message, Message::Close(_));
                    if to_client_sink.send(message).await.is_err() || closing {
                        break;
                    }
                }
                Some(Err(e)) => {
                    debug!("{label}: WebSocket failed: {}", kind(&e));
                    let _ = to_client_sink
                        .send(Message::Close(Some(relay_failed(
                            "the owning server's connection failed",
                        ))))
                        .await;
                    break;
                }
                None => {
                    // The owner went without a close frame.
                    let _ = to_client_sink
                        .send(Message::Close(Some(relay_failed(
                            "the owning server closed the connection",
                        ))))
                        .await;
                    break;
                }
            }
        }
    };
    tokio::pin!(upstream, downstream);
    // Once one direction ends the other gets a short time to finish its
    // close handshake.
    tokio::select! {
        () = &mut upstream => { let _ = tokio::time::timeout(CLOSE_GRACE, &mut downstream).await; }
        () = &mut downstream => { let _ = tokio::time::timeout(CLOSE_GRACE, &mut upstream).await; }
    }
    debug!("{label}: WebSocket relay ended");
}

/// A refusal from the owner, passed on as it came: its status, its body and
/// its content type.
#[derive(Debug)]
pub struct RelayedAnswer {
    status: StatusCode,
    content_type: Option<HeaderValue>,
    body: Vec<u8>,
}

impl RelayedAnswer {
    fn new(
        status: StatusCode,
        answer: tokio_tungstenite::tungstenite::http::Response<Option<Vec<u8>>>,
    ) -> Self {
        Self {
            status,
            content_type: answer.headers().get(header::CONTENT_TYPE).cloned(),
            body: answer.into_body().unwrap_or_default(),
        }
    }
}

impl IntoResponse for RelayedAnswer {
    fn into_response(self) -> Response {
        let mut response = (self.status, self.body).into_response();
        if let Some(content_type) = self.content_type {
            response
                .headers_mut()
                .insert(header::CONTENT_TYPE, content_type);
        }
        response
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_peers_http_address_becomes_a_websocket_address() {
        assert_eq!(
            socket_url("http://zm2:8080", "/api/v3/live/7/webrtc/ws?token=t").unwrap(),
            "ws://zm2:8080/api/v3/live/7/webrtc/ws?token=t"
        );
        assert_eq!(
            socket_url("https://zm2:8443", "/x").unwrap(),
            "wss://zm2:8443/x"
        );
        assert!(socket_url("ftp://zm2", "/x").is_none());
    }

    #[test]
    fn the_handshake_carries_credentials_and_nothing_of_the_clients_own_handshake() {
        let mut from_caller = HeaderMap::new();
        from_caller.insert(header::AUTHORIZATION, HeaderValue::from_static("Bearer t"));
        from_caller.insert(
            header::SEC_WEBSOCKET_PROTOCOL,
            HeaderValue::from_static("zm"),
        );
        from_caller.insert(
            header::SEC_WEBSOCKET_KEY,
            HeaderValue::from_static("client-key"),
        );
        from_caller.insert(header::COOKIE, HeaderValue::from_static("a=b"));
        from_caller.insert(FORWARDED_BY, HeaderValue::from_static("9"));
        let out = handshake_headers(&relayed_headers(&from_caller, None, Some(1)));
        assert_eq!(out.get(header::AUTHORIZATION).unwrap(), "Bearer t");
        assert_eq!(out.get(header::SEC_WEBSOCKET_PROTOCOL).unwrap(), "zm");
        assert_eq!(out.get(FORWARDED_BY).unwrap(), "1");
        assert!(!out.contains_key(header::SEC_WEBSOCKET_KEY));
        assert!(!out.contains_key(header::COOKIE));
    }

    #[test]
    fn close_frames_keep_their_code_and_reason_both_ways() {
        let closed = Message::Close(Some(CloseFrame {
            code: 4001,
            reason: "bye".into(),
        }));
        let PeerMessage::Close(Some(frame)) = to_peer(closed) else {
            panic!("a close frame")
        };
        assert_eq!(u16::from(frame.code), 4001);
        assert_eq!(frame.reason.as_str(), "bye");
        let Some(Message::Close(Some(back))) = to_client(PeerMessage::Close(Some(frame))) else {
            panic!("a close frame")
        };
        assert_eq!((back.code, back.reason.as_str()), (4001, "bye"));
    }

    #[test]
    fn text_and_binary_cross_unchanged() {
        let PeerMessage::Text(text) = to_peer(Message::text("hello")) else {
            panic!("text")
        };
        assert_eq!(text.as_str(), "hello");
        let Some(Message::Binary(bytes)) = to_client(PeerMessage::Binary(vec![1u8, 2, 3].into()))
        else {
            panic!("binary")
        };
        assert_eq!(&bytes[..], [1, 2, 3]);
    }
}
