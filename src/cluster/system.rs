//! System-wide control across servers: `?server_id=` on the daemon and
//! system routes.
//!
//! Daemons and the run state belong to one server each, but a client talks to
//! whichever zm-api it knows. `server_id` says which server a request is for:
//! absent means this one, a number means that server (relayed to its zm-api
//! with the caller's own credentials, as for a monitor), and `all` means every
//! server, with one entry per server in the answer.
//!
//! [`target_server`] sits behind authentication and the route's permission
//! check, so nothing leaves this server for a caller who could not do the
//! same here.

use std::net::SocketAddr;

use axum::body::{Body, Bytes};
use axum::extract::{ConnectInfo, Request, State};
use axum::http::{header, HeaderMap, Method, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::Json;
use futures_util::future::join_all;

use super::forward::{original_path_and_query, relay, relay_to_server, FORWARDED_BY};
use super::Member;
use crate::dto::response::daemon::{AllServersResponse, ServerResult};
use crate::error::{AppError, AppResult, Resource, ResourceType};
use crate::server::state::AppState;

const QUERY_KEY: &str = "server_id";

/// The largest body read back from a server in a fan-out. A daemon list or a
/// system status is a few kilobytes.
const MAX_ANSWER: usize = 4 * 1024 * 1024;

/// The largest request body held while it is sent to several servers.
const MAX_REQUEST: usize = 2 * 1024 * 1024;

/// Which server a request is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    /// This server.
    Here,
    /// The server with this `Servers.Id`, which may be this one.
    Server(u32),
    /// Every server.
    All,
}

/// Read `server_id` from a raw query string. A value that is neither a
/// number nor `all` is an error, so a typo never acts on this server by
/// accident.
pub fn parse_target(query: Option<&str>) -> AppResult<Target> {
    let value = query
        .unwrap_or_default()
        .split('&')
        .filter_map(|pair| pair.split_once('=').or(Some((pair, ""))))
        .find(|(key, _)| *key == QUERY_KEY)
        .map(|(_, value)| value);
    match value {
        None => Ok(Target::Here),
        Some(v) if v.eq_ignore_ascii_case("all") => Ok(Target::All),
        Some(v) => v.parse().map(Target::Server).map_err(|_| {
            AppError::BadRequestError("server_id must be a server's number or \"all\"".to_string())
        }),
    }
}

/// `path_and_query` with `server_id` set to `id`, so the server that gets it
/// handles it as its own.
pub fn with_server_id(path_and_query: &str, id: u32) -> String {
    let (path, query) = path_and_query
        .split_once('?')
        .unwrap_or((path_and_query, ""));
    let mut pairs: Vec<&str> = query
        .split('&')
        .filter(|pair| !pair.is_empty() && pair.split('=').next() != Some(QUERY_KEY))
        .collect();
    let id = format!("{QUERY_KEY}={id}");
    pairs.push(&id);
    format!("{path}?{}", pairs.join("&"))
}

fn unknown_server(id: u32) -> AppError {
    AppError::NotFoundError(Resource {
        details: vec![("server_id".to_string(), id.to_string())],
        resource_type: ResourceType::Config,
    })
}

/// The status and text a failure is reported with in a per-server entry.
fn describe(error: AppError) -> (u16, String) {
    let text = match &error {
        AppError::BadGatewayError(m) | AppError::MisdirectedError(m) => m.clone(),
        other => other.to_string(),
    };
    (error.into_response().status().as_u16(), text)
}

/// The middleware. Apply with `route_layer`, inside the guards.
///
/// A request that already carries [`FORWARDED_BY`] is handled here whatever
/// `server_id` says: that is what stops two servers passing a request back
/// and forth.
pub async fn target_server(
    State(state): State<AppState>,
    request: Request,
    next: Next,
) -> Result<Response, AppError> {
    if request.headers().contains_key(&FORWARDED_BY) {
        return Ok(next.run(request).await);
    }
    match parse_target(request.uri().query())? {
        Target::Here => Ok(next.run(request).await),
        Target::Server(id) if Some(id) == state.cluster.this_server() => {
            Ok(next.run(request).await)
        }
        Target::Server(id) => {
            if !state.cluster.relays() {
                return Err(unknown_server(id));
            }
            let others = state.cluster.other_servers(state.db()).await?;
            if !others.iter().any(|m| m.id == id) {
                return Err(unknown_server(id));
            }
            let path_and_query = with_server_id(&original_path_and_query(&request), id);
            relay_to_server(&state, id, "this request is for", &path_and_query, request).await
        }
        Target::All => all_servers(&state, request, next).await,
    }
}

async fn all_servers(state: &AppState, request: Request, next: Next) -> Result<Response, AppError> {
    let path_and_query = original_path_and_query(&request);
    let (parts, body) = request.into_parts();
    let body = axum::body::to_bytes(body, MAX_REQUEST)
        .await
        .map_err(|_| AppError::BadRequestError("the request body is too large".to_string()))?;
    let connect = parts.extensions.get::<ConnectInfo<SocketAddr>>().copied();
    let (method, headers) = (parts.method.clone(), parts.headers.clone());
    let local = Request::from_parts(parts, Body::from(body.clone()));

    let here = async {
        let name = state.cluster.this_name(state.db()).await.ok().flatten();
        let response = next.run(local).await;
        let status = response.status().as_u16();
        let answer = axum::body::to_bytes(response.into_body(), MAX_ANSWER).await;
        let id = state.cluster.this_server();
        let name = name.unwrap_or_else(|| "local".to_string());
        match answer {
            Ok(bytes) => ServerResult::answered(id, name, status, &bytes),
            Err(e) => {
                ServerResult::failed(id, name, 500, format!("reading the answer failed: {e}"))
            }
        }
    };
    let others = fan_out(state, &method, &path_and_query, &headers, connect, &body);
    let (here, others) = tokio::join!(here, others);

    let mut servers = vec![here];
    servers.extend(others?);
    Ok((
        StatusCode::MULTI_STATUS,
        Json(AllServersResponse { servers }),
    )
        .into_response())
}

/// Send the same request to every other server at once and collect what each
/// answers. One server failing never fails the others: it becomes an entry
/// with `ok: false`. Empty when this install does not relay.
///
/// Each server gets `server_id` set to its own id, and the caller's
/// credentials, so it authenticates and authorises the caller itself.
pub async fn fan_out(
    state: &AppState,
    method: &Method,
    path_and_query: &str,
    caller: &HeaderMap,
    connect: Option<ConnectInfo<SocketAddr>>,
    body: &Bytes,
) -> AppResult<Vec<ServerResult>> {
    if !state.cluster.relays() {
        return Ok(Vec::new());
    }
    let members = state.cluster.other_servers(state.db()).await?;
    Ok(join_all(
        members
            .iter()
            .map(|m| ask(state, m, method, path_and_query, caller, connect, body)),
    )
    .await)
}

async fn ask(
    state: &AppState,
    member: &Member,
    method: &Method,
    path_and_query: &str,
    caller: &HeaderMap,
    connect: Option<ConnectInfo<SocketAddr>>,
    body: &Bytes,
) -> ServerResult {
    let id = Some(member.id);
    let name = member.name.clone();
    let Some(peer) = &member.peer else {
        return ServerResult::failed(
            id,
            name,
            421,
            format!(
                "server {} ({}) has no address: give its Servers row a hostname, or set \
                 cluster.peers.{}",
                member.id, member.name, member.id
            ),
        );
    };
    let mut request = Request::new(Body::from(body.clone()));
    *request.method_mut() = method.clone();
    *request.headers_mut() = caller.clone();
    request.headers_mut().remove(header::CONTENT_LENGTH);
    // The answer is read here as JSON, so it must not come back compressed.
    request.headers_mut().remove(header::ACCEPT_ENCODING);
    if let Some(connect) = connect {
        request.extensions_mut().insert(connect);
    }
    let path_and_query = with_server_id(path_and_query, member.id);
    match relay(state, peer, &path_and_query, request).await {
        Ok(response) => {
            let status = response.status().as_u16();
            match axum::body::to_bytes(response.into_body(), MAX_ANSWER).await {
                Ok(bytes) => ServerResult::answered(id, name, status, &bytes),
                Err(_) => ServerResult::failed(
                    id,
                    name,
                    502,
                    format!("{} did not finish its answer", peer.label()),
                ),
            }
        }
        Err(e) => {
            let (status, error) = describe(e);
            ServerResult::failed(id, name, status, error)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_server_id_means_this_server() {
        assert_eq!(parse_target(None).unwrap(), Target::Here);
        assert_eq!(parse_target(Some("")).unwrap(), Target::Here);
        assert_eq!(parse_target(Some("other=1")).unwrap(), Target::Here);
    }

    #[test]
    fn a_number_names_a_server_and_all_names_every_one() {
        assert_eq!(
            parse_target(Some("server_id=2")).unwrap(),
            Target::Server(2)
        );
        assert_eq!(
            parse_target(Some("a=b&server_id=7&c=d")).unwrap(),
            Target::Server(7)
        );
        assert_eq!(parse_target(Some("server_id=all")).unwrap(), Target::All);
        assert_eq!(parse_target(Some("server_id=ALL")).unwrap(), Target::All);
    }

    #[test]
    fn anything_else_is_refused_rather_than_run_here() {
        for bad in [
            "server_id=",
            "server_id",
            "server_id=two",
            "server_id=-1",
            "server_id=2,3",
        ] {
            assert!(
                matches!(parse_target(Some(bad)), Err(AppError::BadRequestError(_))),
                "{bad}"
            );
        }
    }

    #[test]
    fn a_relayed_request_names_the_server_it_goes_to() {
        assert_eq!(
            with_server_id("/api/v3/daemons", 2),
            "/api/v3/daemons?server_id=2"
        );
        assert_eq!(
            with_server_id("/api/v3/daemons?server_id=all", 2),
            "/api/v3/daemons?server_id=2"
        );
        assert_eq!(
            with_server_id("/api/v3/x?a=1&server_id=all&b=2", 3),
            "/api/v3/x?a=1&b=2&server_id=3"
        );
    }

    #[test]
    fn a_success_keeps_the_servers_json() {
        let r = ServerResult::answered(Some(2), "zm2".into(), 200, br#"{"daemons":[]}"#);
        assert!(r.ok);
        assert_eq!(r.status, 200);
        assert_eq!(r.result.unwrap()["daemons"], serde_json::json!([]));
        assert!(r.error.is_none());
    }

    #[test]
    fn a_refusal_carries_the_servers_message() {
        let body = br#"{"kind":"X","error_message":"no such daemon","details":[]}"#;
        let r = ServerResult::answered(Some(2), "zm2".into(), 404, body);
        assert!(!r.ok);
        assert_eq!(r.error.as_deref(), Some("no such daemon"));
        assert!(r.result.is_none());
        let bare = ServerResult::answered(Some(2), "zm2".into(), 500, b"oops");
        assert_eq!(bare.error.as_deref(), Some("answered 500"));
    }

    #[test]
    fn a_server_that_did_not_answer_is_named_and_marked_failed() {
        let (status, text) = describe(AppError::BadGatewayError(
            "server 2 (zm2) did not answer".into(),
        ));
        assert_eq!(status, 502);
        assert_eq!(text, "server 2 (zm2) did not answer");
        let (status, _) = describe(AppError::MisdirectedError("no address".into()));
        assert_eq!(status, 421);
    }
}
