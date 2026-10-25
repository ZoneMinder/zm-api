use axum::extract::{Path, State};
use axum::Json;
use tracing::{info, warn};

use crate::dto::response::{LocaleResponse, MessageResponse, VersionResponse};
use crate::error::AppResult;
use crate::server::state::AppState;
use crate::service;

// Health check.
#[utoipa::path(
    get,
    path = "/api/v3/server/health_check",
    responses(
        (status = 200, description = "check service is up", body = MessageResponse)
    ),
    tag = "Server"
)]
pub async fn health_check() -> AppResult<Json<MessageResponse>> {
    Ok(Json(MessageResponse::new("Ok")))
}

/// Server locale: effective timezone, current UTC offset, and ZoneMinder's
/// date/time format patterns, for rendering server-local time (GH #33).
#[utoipa::path(
    get,
    path = "/api/v3/system/locale",
    responses(
        (status = 200, description = "Server timezone and date/time formats", body = LocaleResponse),
        (status = 401, description = "Unauthorized", body = crate::error::AppResponseError),
        (status = 500, description = "Internal server error", body = crate::error::AppResponseError)
    ),
    tag = "Server",
    security(("jwt" = []))
)]
pub async fn get_locale(State(state): State<AppState>) -> AppResult<Json<LocaleResponse>> {
    Ok(Json(service::server::get_locale(&state).await?))
}

#[utoipa::path(
    get,
    path = "/api/v3/host/getVersion",
    responses(
        (status = 200, description = "Get ZoneMinder and API version information", body = VersionResponse),
        (status = 500, description = "Internal server error", body = crate::error::AppResponseError)
    ),
    tag = "Server"
)]
pub async fn get_version(State(state): State<AppState>) -> AppResult<Json<VersionResponse>> {
    tracing::debug!("Handling request to get ZoneMinder version information");
    match service::server::get_version(&state).await {
        Ok(version_info) => {
            tracing::debug!("Successfully retrieved version information");
            Ok(Json(version_info))
        }
        Err(e) => {
            warn!("Failed to get version information: {:?}", e);
            Err(e)
        }
    }
}

/// Control the ZoneMinder system: restart / stop / start.
///
/// This is whole-system power control (`systemctl restart/stop/start
/// zoneminder`, with a `zmcontrol.pl` fallback) — it does NOT apply a named run
/// state from the `States` table. Applying a run state is
/// `POST /api/v3/system/state`. The canonical path is
/// `/api/v3/server/control/{action}`; `/api/v3/states/change/{action}` remains
/// as a deprecated alias (it wrongly implied run-state control).
#[utoipa::path(
    post,
    path = "/api/v3/server/control/{action}",
    params(
        ("action" = String, Path, description = "Action to perform: restart, stop, or start")
    ),
    responses(
        (status = 200, description = "System control action performed", body = MessageResponse),
        (status = 400, description = "Invalid action", body = crate::error::AppResponseError),
        (status = 500, description = "Failed to perform action", body = crate::error::AppResponseError)
    ),
    tag = "Server",
    summary = "Control the ZoneMinder system (restart/stop/start)",
    description = "- Restarts/stops/starts the ZoneMinder system via systemctl (zmcontrol.pl fallback).\n- This is NOT run-state application; use POST /api/v3/system/state for that.\n- Requires a valid JWT with admin (System) permissions.",
    security(("jwt" = []))
)]
pub async fn change_state(
    State(state): State<AppState>,
    Path(action): Path<String>,
) -> AppResult<Json<MessageResponse>> {
    info!("Handling request to change ZoneMinder state: {}", action);

    let message = match action.to_lowercase().as_str() {
        "restart" => {
            service::server::restart_zoneminder(&state).await?;
            "ZoneMinder restarted successfully"
        }
        "stop" => {
            service::server::stop_zoneminder(&state).await?;
            "ZoneMinder stopped successfully"
        }
        "start" => {
            service::server::start_zoneminder(&state).await?;
            "ZoneMinder started successfully"
        }
        _ => {
            return Err(crate::error::AppError::BadRequestError(format!(
                "Invalid action '{}'. Valid actions are: restart, stop, start",
                action
            )));
        }
    };

    Ok(Json(MessageResponse::new(message)))
}

#[cfg(test)]
pub mod tests {
    use super::*;

    #[tokio::test]
    async fn test_health_check_handler() {
        assert_eq!(health_check().await.unwrap().0.message(), "Ok");
    }
}
