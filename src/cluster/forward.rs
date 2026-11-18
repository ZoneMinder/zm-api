//! Relay a request for another server's monitor to the zm-api there.
//!
//! [`forward_to_owner`] sits behind the authentication and monitor guards of
//! the routes that touch a monitor's node-local resources. For a monitor
//! this server owns it does nothing. For another server's monitor it sends
//! the same request there, with the caller's own credentials, and streams
//! the answer back, so a client only ever needs one address.

use axum::body::Body;
use axum::extract::{OriginalUri, RawPathParams, Request, State};
use axum::http::{header, HeaderMap, HeaderName, HeaderValue, Method};
use axum::middleware::Next;
use axum::response::Response;
use tracing::{debug, warn};

use super::{Owner, Peer};
use crate::error::{AppError, AppResult, Resource, ResourceType};
use crate::server::state::AppState;
use crate::service::monitor_acl::{self, MonitorScope};
use crate::util::authz::Level;
use crate::util::claim::UserClaims;

/// Set on a relayed request to the id of the server that relayed it. A
/// server that receives it never relays again, so two servers that disagree
/// about who owns a monitor answer with an error, not a loop.
pub const FORWARDED_BY: HeaderName = HeaderName::from_static("x-zm-forwarded-by");

const X_FORWARDED_FOR: HeaderName = HeaderName::from_static("x-forwarded-for");

/// The largest request body relayed. Every relayed route takes a small JSON
/// document or an SDP offer.
const MAX_RELAYED_BODY: usize = 2 * 1024 * 1024;

/// Headers that describe one connection, not the request, and so are not
/// passed on (RFC 9110 §7.6.1), plus the two the HTTP client sets itself.
const HOP_BY_HOP: [HeaderName; 9] = [
    header::CONNECTION,
    header::PROXY_AUTHENTICATE,
    header::PROXY_AUTHORIZATION,
    header::TE,
    header::TRAILER,
    header::TRANSFER_ENCODING,
    header::UPGRADE,
    header::HOST,
    header::CONTENT_LENGTH,
];

/// Whether a route stays on this server even for another server's monitor.
///
/// - Reading a monitor's row is a database read any server can do.
/// - A signed playlist link is minted here, so the client keeps the one
///   address it already has. The link is valid on every server, and the
///   playlist requests that follow it are relayed.
fn stays_local(method: &Method, path: &str) -> bool {
    let path = path.trim_end_matches('/');
    if path.ends_with("/hls/link") {
        return true;
    }
    let is_read = matches!(*method, Method::GET | Method::HEAD | Method::OPTIONS);
    let last_is_the_id = path
        .rsplit_once('/')
        .is_some_and(|(parent, last)| parent.ends_with("/monitors") && last.parse::<u32>().is_ok());
    is_read && last_is_the_id
}

/// The middleware. Apply with `route_layer`, inside the guards.
pub async fn forward_to_owner(
    State(state): State<AppState>,
    params: RawPathParams,
    request: Request,
    next: Next,
) -> Result<Response, AppError> {
    if !state.cluster.relays() {
        return Ok(next.run(request).await);
    }
    let Some(monitor_id) = monitor_acl::path_monitor_id(&params) else {
        return Ok(next.run(request).await);
    };
    let path_and_query = original_path_and_query(&request);
    let path = path_and_query.split('?').next().unwrap_or_default();
    if stays_local(request.method(), path) {
        return Ok(next.run(request).await);
    }
    let Owner::Peer(server_id) = state.cluster.monitor_owner(state.db(), monitor_id).await? else {
        return Ok(next.run(request).await);
    };

    // The caller must be able to see the monitor before anything leaves this
    // server. The owning server checks again, with the level the route needs.
    let sight = sight(&request, monitor_id);
    if !caller_sees(&state, sight, monitor_id).await? {
        return Err(AppError::NotFoundError(Resource {
            details: vec![("monitor_id".to_string(), monitor_id.to_string())],
            resource_type: ResourceType::Monitor,
        }));
    }

    let peer = state.cluster.peer(state.db(), server_id).await?;
    if let Some(by) = request.headers().get(&FORWARDED_BY) {
        return Err(AppError::MisdirectedError(format!(
            "monitor {monitor_id} belongs to server {server_id}, and server {} already relayed \
             this request here; the servers disagree about which one this is",
            by.to_str().unwrap_or("?")
        )));
    }
    let Some(peer) = peer else {
        return Err(AppError::MisdirectedError(format!(
            "monitor {monitor_id} belongs to server {server_id}, which has no address: give its \
             Servers row a hostname, or set cluster.peers.{server_id}"
        )));
    };
    if request.headers().contains_key(header::UPGRADE) {
        return Err(AppError::MisdirectedError(format!(
            "monitor {monitor_id} belongs to {}; a WebSocket is not relayed, so connect to {} \
             or use WHEP",
            peer.label(),
            peer.base_url
        )));
    }
    debug!("relaying monitor {monitor_id} request to {}", peer.label());
    relay(&state, &peer, &path_and_query, request).await
}

/// The path and query the client sent. A nested router strips its prefix
/// from the request's own URI, so the original is read from the extension
/// axum keeps.
fn original_path_and_query(request: &Request) -> String {
    let uri = request
        .extensions()
        .get::<OriginalUri>()
        .map_or_else(|| request.uri(), |original| &original.0);
    uri.path_and_query()
        .map_or_else(|| uri.path().to_string(), |pq| pq.as_str().to_string())
}

/// What the guards left on the request about the caller's view of a monitor.
enum Sight {
    Known(bool),
    /// Only the claims: the scope still has to be resolved.
    Resolve(u32, Level),
}

/// Read synchronously, because a borrowed request can't be held across the
/// database call that resolving a scope needs.
fn sight(request: &Request, monitor_id: u32) -> Sight {
    let ext = request.extensions();
    if let Some(scope) = ext.get::<MonitorScope>() {
        Sight::Known(scope.allows(monitor_id, Level::View))
    } else if let Some(grant) = ext.get::<crate::service::media_grant::MediaGrant>() {
        Sight::Known(grant.scope().allows(monitor_id, Level::View))
    } else if let Some(claims) = ext.get::<UserClaims>() {
        Sight::Resolve(claims.uid, claims.perms.monitors)
    } else {
        // Nothing authenticated this request here. The owning server will.
        Sight::Known(true)
    }
}

/// Whether the caller may view the monitor.
async fn caller_sees(state: &AppState, sight: Sight, monitor_id: u32) -> AppResult<bool> {
    match sight {
        Sight::Known(sees) => Ok(sees),
        Sight::Resolve(uid, level) => {
            let scope = monitor_acl::resolve(state.db(), uid, level).await?;
            Ok(scope.allows(monitor_id, Level::View))
        }
    }
}

/// The headers the owning server gets: the caller's own, minus the ones
/// that belong to this connection, plus who relayed it and for whom.
fn relayed_headers(
    from_caller: &HeaderMap,
    client: Option<std::net::IpAddr>,
    this_server: Option<u32>,
) -> HeaderMap {
    let mut headers = end_to_end(from_caller);
    if let Some(id) = this_server {
        headers.insert(FORWARDED_BY, HeaderValue::from(id));
    }
    // The owner's rate limiter can key on the real client, if it trusts this
    // server as a proxy. Appended, as a proxy does, to what the client sent.
    if let Some(client) = client.and_then(|ip| HeaderValue::from_str(&ip.to_string()).ok()) {
        headers.append(X_FORWARDED_FOR, client);
    }
    headers
}

fn end_to_end(headers: &HeaderMap) -> HeaderMap {
    headers
        .iter()
        .filter(|(name, _)| !HOP_BY_HOP.contains(name))
        .map(|(name, value)| (name.clone(), value.clone()))
        .collect()
}

/// Send `request` to `peer` and stream its answer back.
async fn relay(
    state: &AppState,
    peer: &Peer,
    path_and_query: &str,
    request: Request,
) -> Result<Response, AppError> {
    let (parts, body) = request.into_parts();
    let body = axum::body::to_bytes(body, MAX_RELAYED_BODY)
        .await
        .map_err(|_| AppError::BadRequestError("the request body is too large".to_string()))?;

    let client = parts
        .extensions
        .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
        .map(|info| info.0.ip());
    let headers = relayed_headers(&parts.headers, client, state.cluster.this_server());
    let url = format!("{}{}", peer.base_url, path_and_query);
    let sent = state
        .cluster
        .client()
        .request(parts.method, url)
        .headers(headers)
        .body(body)
        .send()
        .await;
    let answer = match sent {
        Ok(answer) => answer,
        Err(e) => {
            // The URL can carry a token in its query, so it stays out.
            warn!("{} did not answer: {}", peer.label(), e.without_url());
            return Err(AppError::BadGatewayError(format!(
                "{} did not answer",
                peer.label()
            )));
        }
    };

    // This server accepted the caller's credentials a moment ago. The only
    // way the owner can refuse them is with different signing keys.
    if answer.status() == reqwest::StatusCode::UNAUTHORIZED {
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

    let mut response = Response::builder().status(answer.status().as_u16());
    if let Some(out) = response.headers_mut() {
        for (name, value) in answer.headers() {
            // Content-Length is kept: the body is passed on untouched.
            if !HOP_BY_HOP.contains(name) || name == header::CONTENT_LENGTH {
                out.append(name.clone(), value.clone());
            }
        }
    }
    response
        .body(Body::from_stream(answer.bytes_stream()))
        .map_err(|e| AppError::InternalServerError(e.to_string()))
}

/// After a monitor row was saved here: if another server owns it now, ask
/// that server to restart its daemons.
///
/// Edits and deletes reach the owner through [`forward_to_owner`], so its own
/// daemon control runs in the right place. Two cases end up on a server that
/// doesn't own the saved row: a monitor created for another server, and a
/// monitor just moved to one. Failures are logged, not returned, as for any
/// daemon control after a row change: the row is saved either way.
pub async fn restart_on_owner(
    state: &AppState,
    caller: &HeaderMap,
    monitor_id: u32,
    monitor_server: Option<u32>,
) {
    state.cluster.forget_monitor(monitor_id);
    if !state.cluster.relays() {
        return;
    }
    let Owner::Peer(server_id) = super::owner(state.cluster.this_server(), monitor_server) else {
        return;
    };
    let peer = match state.cluster.peer(state.db(), server_id).await {
        Ok(Some(peer)) => peer,
        Ok(None) => {
            warn!(
                "monitor {monitor_id}: server {server_id} runs its daemons but has no address;                  they start when that server next reconciles"
            );
            return;
        }
        Err(e) => {
            warn!("monitor {monitor_id}: looking up server {server_id} failed: {e}");
            return;
        }
    };
    let mut request = state
        .cluster
        .client()
        .patch(format!(
            "{}/api/v3/monitors/{monitor_id}/state",
            peer.base_url
        ))
        .json(&serde_json::json!({ "state": "restart" }));
    if let Some(auth) = caller.get(header::AUTHORIZATION) {
        request = request.header(header::AUTHORIZATION, auth);
    }
    if let Some(id) = state.cluster.this_server() {
        request = request.header(FORWARDED_BY, HeaderValue::from(id));
    }
    match request.send().await {
        Ok(answer) if answer.status().is_success() => {
            debug!(
                "monitor {monitor_id}: {} restarted its daemons",
                peer.label()
            )
        }
        Ok(answer) => warn!(
            "monitor {monitor_id}: {} answered {} to a daemon restart",
            peer.label(),
            answer.status()
        ),
        Err(e) => warn!(
            "monitor {monitor_id}: {} did not answer a daemon restart: {}",
            peer.label(),
            e.without_url()
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reading_a_monitors_row_stays_local() {
        assert!(stays_local(&Method::GET, "/api/v3/monitors/7"));
        assert!(stays_local(&Method::HEAD, "/api/v3/monitors/7/"));
        // Changing it does not: the owner must restart its daemons.
        assert!(!stays_local(&Method::PATCH, "/api/v3/monitors/7"));
        assert!(!stays_local(&Method::DELETE, "/api/v3/monitors/7"));
    }

    #[test]
    fn node_local_reads_are_relayed() {
        for path in [
            "/api/v3/monitors/7/snapshot",
            "/api/v3/monitors/7/audio-level",
            "/api/v3/monitors/7/pipeline",
            "/api/v3/live/7/hls/live.m3u8",
            "/api/v3/live/7/stats",
        ] {
            assert!(!stays_local(&Method::GET, path), "{path}");
        }
    }

    #[test]
    fn a_signed_playlist_link_is_minted_here() {
        assert!(stays_local(&Method::POST, "/api/v3/live/7/hls/link"));
        // The RTSP link names the owner's RTSP server, so it is relayed.
        assert!(!stays_local(&Method::POST, "/api/v3/live/7/rtsp/link"));
    }

    #[test]
    fn the_owner_learns_who_relayed_and_for_which_client() {
        let mut from_caller = HeaderMap::new();
        from_caller.insert(X_FORWARDED_FOR, HeaderValue::from_static("203.0.113.9"));
        // A caller can't pose as a relaying server: the value is replaced.
        from_caller.insert(FORWARDED_BY, HeaderValue::from_static("7"));
        let out = relayed_headers(&from_caller, Some("192.0.2.4".parse().unwrap()), Some(1));
        assert_eq!(out.get(FORWARDED_BY).unwrap(), "1");
        let chain: Vec<_> = out.get_all(X_FORWARDED_FOR).iter().collect();
        assert_eq!(chain, ["203.0.113.9", "192.0.2.4"]);
        // With no connection address (a test client), nothing is invented.
        let bare = relayed_headers(&HeaderMap::new(), None, Some(1));
        assert!(!bare.contains_key(X_FORWARDED_FOR));
    }

    #[test]
    fn connection_headers_stay_behind_and_credentials_go_through() {
        let mut headers = HeaderMap::new();
        headers.insert(header::AUTHORIZATION, HeaderValue::from_static("Bearer t"));
        headers.insert(header::HOST, HeaderValue::from_static("zm1"));
        headers.insert(header::CONNECTION, HeaderValue::from_static("keep-alive"));
        headers.insert(header::CONTENT_LENGTH, HeaderValue::from_static("2"));
        headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );
        let out = end_to_end(&headers);
        assert_eq!(out.get(header::AUTHORIZATION).unwrap(), "Bearer t");
        assert_eq!(out.get(header::CONTENT_TYPE).unwrap(), "application/json");
        assert!(!out.contains_key(header::HOST));
        assert!(!out.contains_key(header::CONNECTION));
        assert!(!out.contains_key(header::CONTENT_LENGTH));
    }
}
