//! HTTP handlers for daemon controller API.

use axum::extract::{ConnectInfo, Path, State};
use axum::http::request::Parts;
use axum::http::Method;
use axum::Json;
use tracing::{info, warn};

use crate::cluster::forward::FORWARDED_BY;
use crate::cluster::system::{fan_out, parse_target, Target};

use crate::dto::request::daemon::{ApplyStateRequest, StartDaemonRequest};
use crate::dto::response::daemon::{
    AllServersResponse, DaemonActionResponse, DaemonListResponse, DaemonStatusResponse,
    ServerTargetQuery, SystemStatusResponse,
};
use crate::error::{AppError, AppResult};
use crate::server::state::AppState;
use crate::service;
use garde::Validate;

/// List all daemons.
#[utoipa::path(
    security(("jwt" = [])),
    get,
    path = "/api/v3/daemons",
    params(ServerTargetQuery),
    responses(
        (status = 200, description = "List of all daemons", body = DaemonListResponse),
        (status = 207, description = "With `server_id=all`: one entry per server; the status is 207 even when every server failed", body = AllServersResponse),
        (status = 400, description = "`server_id` is neither a server number nor `all`", body = crate::error::AppResponseError),
        (status = 404, description = "No server has the number given in `server_id`", body = crate::error::AppResponseError),
        (status = 421, description = "The server named by `server_id` has no address", body = crate::error::AppResponseError),
        (status = 502, description = "The server named by `server_id` did not answer", body = crate::error::AppResponseError),
        (status = 503, description = "Daemon manager not available", body = crate::error::AppResponseError)
    ),
    tag = "Daemons"
)]
pub async fn list_daemons(State(state): State<AppState>) -> AppResult<Json<DaemonListResponse>> {
    tracing::debug!("Listing all daemons");
    let response = service::daemon::get_all_daemons(&state).await?;
    Ok(Json(response))
}

/// Get a specific daemon's status.
#[utoipa::path(
    security(("jwt" = [])),
    get,
    path = "/api/v3/daemons/{id}",
    params(
        ServerTargetQuery,
        ("id" = String, Path, description = "Daemon identifier")
    ),
    responses(
        (status = 200, description = "Daemon status", body = DaemonStatusResponse),
        (status = 207, description = "With `server_id=all`: one entry per server; the status is 207 even when every server failed", body = AllServersResponse),
        (status = 400, description = "`server_id` is neither a server number nor `all`", body = crate::error::AppResponseError),
        (status = 421, description = "The server named by `server_id` has no address", body = crate::error::AppResponseError),
        (status = 502, description = "The server named by `server_id` did not answer", body = crate::error::AppResponseError),
        (status = 404, description = "Daemon not found", body = crate::error::AppResponseError),
        (status = 503, description = "Daemon manager not available", body = crate::error::AppResponseError)
    ),
    tag = "Daemons"
)]
pub async fn get_daemon(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> AppResult<Json<DaemonStatusResponse>> {
    tracing::debug!("Getting daemon status: {}", id);
    let response = service::daemon::get_daemon(&state, &id).await?;
    Ok(Json(response))
}

/// Start a daemon.
#[utoipa::path(
    post,
    path = "/api/v3/daemons/{id}/start",
    params(
        ServerTargetQuery,
        ("id" = String, Path, description = "Daemon identifier")
    ),
    request_body(content = Option<StartDaemonRequest>, description = "Optional arguments"),
    responses(
        (status = 200, description = "Daemon started", body = DaemonActionResponse),
        (status = 207, description = "With `server_id=all`: one entry per server; the status is 207 even when every server failed", body = AllServersResponse),
        (status = 400, description = "`server_id` is neither a server number nor `all`", body = crate::error::AppResponseError),
        (status = 404, description = "No server has the number given in `server_id`", body = crate::error::AppResponseError),
        (status = 421, description = "The server named by `server_id` has no address", body = crate::error::AppResponseError),
        (status = 502, description = "The server named by `server_id` did not answer", body = crate::error::AppResponseError),
        (status = 503, description = "Daemon manager not available", body = crate::error::AppResponseError)
    ),
    tag = "Daemons",
    security(("jwt" = []))
)]
pub async fn start_daemon(
    State(state): State<AppState>,
    Path(id): Path<String>,
    request: Option<Json<StartDaemonRequest>>,
) -> AppResult<Json<DaemonActionResponse>> {
    let args = request.map(|r| r.0.args).unwrap_or_default();
    info!("Starting daemon: {} with args: {:?}", id, args);
    let response = service::daemon::start_daemon(&state, &id, &args).await?;
    Ok(Json(response))
}

/// Stop a daemon.
#[utoipa::path(
    post,
    path = "/api/v3/daemons/{id}/stop",
    params(
        ServerTargetQuery,
        ("id" = String, Path, description = "Daemon identifier")
    ),
    responses(
        (status = 200, description = "Daemon stopped", body = DaemonActionResponse),
        (status = 207, description = "With `server_id=all`: one entry per server; the status is 207 even when every server failed", body = AllServersResponse),
        (status = 400, description = "`server_id` is neither a server number nor `all`", body = crate::error::AppResponseError),
        (status = 404, description = "No server has the number given in `server_id`", body = crate::error::AppResponseError),
        (status = 421, description = "The server named by `server_id` has no address", body = crate::error::AppResponseError),
        (status = 502, description = "The server named by `server_id` did not answer", body = crate::error::AppResponseError),
        (status = 503, description = "Daemon manager not available", body = crate::error::AppResponseError)
    ),
    tag = "Daemons",
    security(("jwt" = []))
)]
pub async fn stop_daemon(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> AppResult<Json<DaemonActionResponse>> {
    info!("Stopping daemon: {}", id);
    let response = service::daemon::stop_daemon(&state, &id).await?;
    Ok(Json(response))
}

/// Restart a daemon.
#[utoipa::path(
    post,
    path = "/api/v3/daemons/{id}/restart",
    params(
        ServerTargetQuery,
        ("id" = String, Path, description = "Daemon identifier")
    ),
    responses(
        (status = 200, description = "Daemon restarted", body = DaemonActionResponse),
        (status = 207, description = "With `server_id=all`: one entry per server; the status is 207 even when every server failed", body = AllServersResponse),
        (status = 400, description = "`server_id` is neither a server number nor `all`", body = crate::error::AppResponseError),
        (status = 404, description = "No server has the number given in `server_id`", body = crate::error::AppResponseError),
        (status = 421, description = "The server named by `server_id` has no address", body = crate::error::AppResponseError),
        (status = 502, description = "The server named by `server_id` did not answer", body = crate::error::AppResponseError),
        (status = 503, description = "Daemon manager not available", body = crate::error::AppResponseError)
    ),
    tag = "Daemons",
    security(("jwt" = []))
)]
pub async fn restart_daemon(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> AppResult<Json<DaemonActionResponse>> {
    info!("Restarting daemon: {}", id);
    let response = service::daemon::restart_daemon(&state, &id).await?;
    Ok(Json(response))
}

/// Reload a daemon's configuration (SIGHUP).
#[utoipa::path(
    post,
    path = "/api/v3/daemons/{id}/reload",
    params(
        ServerTargetQuery,
        ("id" = String, Path, description = "Daemon identifier")
    ),
    responses(
        (status = 200, description = "Reload signal sent", body = DaemonActionResponse),
        (status = 207, description = "With `server_id=all`: one entry per server; the status is 207 even when every server failed", body = AllServersResponse),
        (status = 400, description = "`server_id` is neither a server number nor `all`", body = crate::error::AppResponseError),
        (status = 404, description = "No server has the number given in `server_id`", body = crate::error::AppResponseError),
        (status = 421, description = "The server named by `server_id` has no address", body = crate::error::AppResponseError),
        (status = 502, description = "The server named by `server_id` did not answer", body = crate::error::AppResponseError),
        (status = 503, description = "Daemon manager not available", body = crate::error::AppResponseError)
    ),
    tag = "Daemons",
    security(("jwt" = []))
)]
pub async fn reload_daemon(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> AppResult<Json<DaemonActionResponse>> {
    info!("Reloading daemon: {}", id);
    let response = service::daemon::reload_daemon(&state, &id).await?;
    Ok(Json(response))
}

/// Get system status.
#[utoipa::path(
    security(("jwt" = [])),
    get,
    path = "/api/v3/system/status",
    params(ServerTargetQuery),
    responses(
        (status = 200, description = "System status", body = SystemStatusResponse),
        (status = 207, description = "With `server_id=all`: one entry per server; the status is 207 even when every server failed", body = AllServersResponse),
        (status = 400, description = "`server_id` is neither a server number nor `all`", body = crate::error::AppResponseError),
        (status = 404, description = "No server has the number given in `server_id`", body = crate::error::AppResponseError),
        (status = 421, description = "The server named by `server_id` has no address", body = crate::error::AppResponseError),
        (status = 502, description = "The server named by `server_id` did not answer", body = crate::error::AppResponseError),
        (status = 503, description = "Daemon manager not available", body = crate::error::AppResponseError)
    ),
    tag = "System"
)]
pub async fn get_system_status(
    State(state): State<AppState>,
) -> AppResult<Json<SystemStatusResponse>> {
    tracing::debug!("Getting system status");
    let response = service::daemon::get_system_status(&state).await?;
    Ok(Json(response))
}

/// Perform full system startup.
#[utoipa::path(
    post,
    path = "/api/v3/system/startup",
    params(ServerTargetQuery),
    responses(
        (status = 200, description = "System started", body = DaemonActionResponse),
        (status = 207, description = "With `server_id=all`: one entry per server; the status is 207 even when every server failed", body = AllServersResponse),
        (status = 400, description = "`server_id` is neither a server number nor `all`", body = crate::error::AppResponseError),
        (status = 404, description = "No server has the number given in `server_id`", body = crate::error::AppResponseError),
        (status = 421, description = "The server named by `server_id` has no address", body = crate::error::AppResponseError),
        (status = 502, description = "The server named by `server_id` did not answer", body = crate::error::AppResponseError),
        (status = 503, description = "Daemon manager not available", body = crate::error::AppResponseError)
    ),
    tag = "System",
    security(("jwt" = []))
)]
pub async fn system_startup(
    State(state): State<AppState>,
) -> AppResult<Json<DaemonActionResponse>> {
    info!("Starting system");
    let response = service::daemon::system_startup(&state).await?;
    Ok(Json(response))
}

/// Perform full system shutdown.
#[utoipa::path(
    post,
    path = "/api/v3/system/shutdown",
    params(ServerTargetQuery),
    responses(
        (status = 200, description = "System shutdown initiated", body = DaemonActionResponse),
        (status = 207, description = "With `server_id=all`: one entry per server; the status is 207 even when every server failed", body = AllServersResponse),
        (status = 400, description = "`server_id` is neither a server number nor `all`", body = crate::error::AppResponseError),
        (status = 404, description = "No server has the number given in `server_id`", body = crate::error::AppResponseError),
        (status = 421, description = "The server named by `server_id` has no address", body = crate::error::AppResponseError),
        (status = 502, description = "The server named by `server_id` did not answer", body = crate::error::AppResponseError),
        (status = 503, description = "Daemon manager not available", body = crate::error::AppResponseError)
    ),
    tag = "System",
    security(("jwt" = []))
)]
pub async fn system_shutdown(
    State(state): State<AppState>,
) -> AppResult<Json<DaemonActionResponse>> {
    info!("Shutting down system");
    let response = service::daemon::system_shutdown(&state).await?;
    Ok(Json(response))
}

/// Perform full system restart.
#[utoipa::path(
    post,
    path = "/api/v3/system/restart",
    params(ServerTargetQuery),
    responses(
        (status = 200, description = "System restarted", body = DaemonActionResponse),
        (status = 207, description = "With `server_id=all`: one entry per server; the status is 207 even when every server failed", body = AllServersResponse),
        (status = 400, description = "`server_id` is neither a server number nor `all`", body = crate::error::AppResponseError),
        (status = 404, description = "No server has the number given in `server_id`", body = crate::error::AppResponseError),
        (status = 421, description = "The server named by `server_id` has no address", body = crate::error::AppResponseError),
        (status = 502, description = "The server named by `server_id` did not answer", body = crate::error::AppResponseError),
        (status = 503, description = "Daemon manager not available", body = crate::error::AppResponseError)
    ),
    tag = "System",
    security(("jwt" = []))
)]
pub async fn system_restart(
    State(state): State<AppState>,
) -> AppResult<Json<DaemonActionResponse>> {
    info!("Restarting system");
    let response = service::daemon::system_restart(&state).await?;
    Ok(Json(response))
}

/// Trigger log rotation for all daemons.
#[utoipa::path(
    post,
    path = "/api/v3/system/logrot",
    params(ServerTargetQuery),
    responses(
        (status = 200, description = "Log rotation triggered", body = DaemonActionResponse),
        (status = 207, description = "With `server_id=all`: one entry per server; the status is 207 even when every server failed", body = AllServersResponse),
        (status = 400, description = "`server_id` is neither a server number nor `all`", body = crate::error::AppResponseError),
        (status = 404, description = "No server has the number given in `server_id`", body = crate::error::AppResponseError),
        (status = 421, description = "The server named by `server_id` has no address", body = crate::error::AppResponseError),
        (status = 502, description = "The server named by `server_id` did not answer", body = crate::error::AppResponseError),
        (status = 503, description = "Daemon manager not available", body = crate::error::AppResponseError)
    ),
    tag = "System",
    security(("jwt" = []))
)]
pub async fn system_logrot(State(state): State<AppState>) -> AppResult<Json<DaemonActionResponse>> {
    info!("Triggering log rotation");
    let response = service::daemon::system_logrot(&state).await?;
    Ok(Json(response))
}

/// Apply a system state.
///
/// States are predefined monitor configurations stored in the database.
/// Applying a state updates monitor Function/Enabled settings and restarts affected daemons.
///
/// On a multi-server install the database change is made once, here, and then
/// every other server is told to restart its daemons for the monitors the
/// state names. What each of them answered is in `servers`; a server that
/// could not be reached does not undo the change. `server_id` may be absent or
/// `all` (the same); a server's number is refused with 400, because the
/// monitors of every server change together. A request that another server
/// relayed is handled here only.
#[utoipa::path(
    post,
    path = "/api/v3/system/state",
    request_body = ApplyStateRequest,
    params(ServerTargetQuery),
    responses(
        (status = 200, description = "State applied", body = DaemonActionResponse),
        (status = 400, description = "`server_id` is neither absent nor `all`", body = crate::error::AppResponseError),
        (status = 404, description = "State not found"),
        (status = 503, description = "Daemon manager not available", body = crate::error::AppResponseError)
    ),
    tag = "System",
    security(("jwt" = []))
)]
pub async fn apply_state(
    State(state): State<AppState>,
    parts: Parts,
    Json(request): Json<ApplyStateRequest>,
) -> AppResult<Json<DaemonActionResponse>> {
    request.validate().map_err(AppError::InvalidInputError)?;
    let forwarded = parts.headers.contains_key(&FORWARDED_BY);
    if !forwarded && matches!(parse_target(parts.uri.query())?, Target::Server(_)) {
        return Err(AppError::BadRequestError(
            "a run state applies to every server's monitors at once: leave out server_id, or \
             use server_id=all"
                .to_string(),
        ));
    }
    if request.daemons_only {
        info!("Restarting daemons for state: {}", request.state_name);
        let response =
            service::daemon::restart_daemons_for_state(&state, &request.state_name).await?;
        return Ok(Json(response));
    }
    info!("Applying state: {}", request.state_name);
    let mut response = service::daemon::apply_state(&state, &request.state_name).await?;

    // The rows are written; the other servers' daemons don't watch them.
    if !forwarded && state.cluster.relays() {
        let body = serde_json::json!({ "state_name": request.state_name, "daemons_only": true });
        let connect = parts.extensions.get::<ConnectInfo<std::net::SocketAddr>>();
        match fan_out(
            &state,
            &Method::POST,
            parts.uri.path(),
            &parts.headers,
            connect.copied(),
            &body.to_string().into(),
        )
        .await
        {
            Ok(others) if !others.is_empty() => response.servers = Some(others),
            Ok(_) => {}
            // The state is committed either way, as with a failed restart.
            Err(e) => warn!(
                "state '{}' applied here, but listing the other servers failed: {e}",
                request.state_name
            ),
        }
    }
    Ok(Json(response))
}

#[cfg(test)]
mod tests {
    // Handler tests would require mocking AppState with daemon_manager
    // which is added in Phase 7
}
