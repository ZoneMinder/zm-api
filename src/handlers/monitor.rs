use axum::extract::{Path, Query, State};
use axum::Json;
use garde::Validate;
use tracing::{info, warn};

use crate::dto::request::monitor::MonitorListParams;
use crate::dto::request::{
    AlarmControlRequest, CreateMonitorRequest, MonitorStateRequest, UpdateMonitorRequest,
};
use crate::dto::response::monitors::PaginatedMonitorsResponse;
use crate::dto::response::MonitorResponse;
use crate::error::AppError;
use crate::error::AppResponseError;
use crate::error::AppResult;
use crate::server::state::AppState;
use crate::service;
use crate::service::monitor_acl::MonitorScope;
use crate::util::claim::UserClaims;

/// List all monitors with pagination.
///
/// - Requires a valid JWT.
#[utoipa::path(
    get,
    path = "/api/v3/monitors",
    params(
        ("page" = Option<u64>, Query, description = "Page number (1-indexed)", example = 1),
        ("page_size" = Option<u64>, Query, description = "Items per page (max 1000)", example = 25),
        ("include_deleted" = Option<bool>, Query, description = "Also list soft-deleted monitors (Deleted = 1). Default false.")
    ),
    responses(
        (status = 200, description = "Paginated list of monitors", body = PaginatedMonitorsResponse),
        (status = 401, description = "Unauthorized - Invalid or missing token", body = AppResponseError),
        (status = 500, description = "Internal server error", body = AppResponseError)
    ),
    security(
        ("jwt" = [])
    ),
    tag = "Monitors"
)]
pub async fn list_monitors(
    State(state): State<AppState>,
    Query(params): Query<MonitorListParams>,
    scope: MonitorScope,
) -> AppResult<Json<PaginatedMonitorsResponse>> {
    tracing::debug!("Listing all monitors with pagination.");
    params.validate().map_err(AppError::InvalidInputError)?;
    match service::monitor::list_paginated(
        &state,
        &params.pagination(),
        params.include_deleted,
        &scope,
    )
    .await
    {
        Ok(result) => Ok(Json(PaginatedMonitorsResponse::from(result))),
        Err(e) => {
            warn!("Failed to list monitors: {e:?}.");
            Err(e)
        }
    }
}

#[utoipa::path(
    get,
    path = "/api/v3/monitors/{id}",
    params(
        ("id" = u32, Path, description = "Monitor identifier")
    ),
    responses(
        (status = 200, description = "View monitor details", body = MonitorResponse),
        (status = 401, description = "Unauthorized - Invalid or missing token", body = AppResponseError),
        (status = 404, description = "Monitor not found", body = AppResponseError),
        (status = 500, description = "Internal server error", body = AppResponseError)
    ),
    security(
        ("jwt" = [])
    ),
    tag = "Monitors"
)]
pub async fn get_monitor(
    State(state): State<AppState>,
    Path(id): Path<u32>,
    scope: MonitorScope,
) -> AppResult<Json<MonitorResponse>> {
    tracing::debug!("Viewing monitor with ID: {id}.");
    match service::monitor::get_by_id(&state, id, &scope).await {
        Ok(monitor) => Ok(Json(monitor)),
        Err(e) => {
            warn!("Failed to view monitor: {e:?}.");
            Err(e)
        }
    }
}

#[utoipa::path(
    post,
    path = "/api/v3/monitors",
    request_body = CreateMonitorRequest,
    responses(
        (status = 200, description = "Monitor created successfully", body = MonitorResponse),
        (status = 400, description = "Invalid request data", body = AppResponseError),
        (status = 401, description = "Unauthorized - Invalid or missing token", body = AppResponseError),
        (status = 403, description = "Caller's monitor access is restricted", body = AppResponseError),
        (status = 500, description = "Internal server error", body = AppResponseError)
    ),
    security(
        ("jwt" = [])
    ),
    tag = "Monitors"
)]
pub async fn create_monitor(
    State(state): State<AppState>,
    claims: UserClaims,
    scope: MonitorScope,
    Json(req): Json<CreateMonitorRequest>,
) -> AppResult<Json<MonitorResponse>> {
    // `Monitors: Create` is checked by the route's `require_monitor_create`.
    req.validate().map_err(AppError::InvalidInputError)?;
    // A new monitor has no event commands, so any value is a change.
    service::monitor::ensure_may_set_event_commands(
        &claims.perms,
        [
            req.event_start_command.as_deref(),
            req.event_end_command.as_deref(),
        ],
        ["", ""],
    )?;
    // Row-level ACL: creating a monitor is a whole-system operation with no
    // pre-existing monitor row to scope against. A user whose access is
    // restricted to specific monitors must not be able to mint new ones — only
    // unrestricted (admin-equivalent) callers may. Feature-level `Monitors:Edit`
    // is already enforced by the route's `protect` layer (REVIEW_FIXES_PLAN §1.4).
    if scope.is_restricted() {
        return Err(AppError::PermissionDeniedError(
            "creating monitors requires unrestricted monitor access".to_string(),
        ));
    }
    // Log the name only: the request body carries camera RTSP/ONVIF credentials.
    info!("Creating new monitor: {}.", req.name);
    match service::monitor::create(&state, req).await {
        Ok(monitor) => Ok(Json(monitor)),
        Err(e) => {
            warn!("Failed to create monitor: {e:?}.");
            Err(e)
        }
    }
}

#[utoipa::path(
    patch,
    path = "/api/v3/monitors/{id}",
    params(
        ("id" = u32, Path, description = "Monitor identifier")
    ),
    request_body = UpdateMonitorRequest,
    responses(
        (status = 200, description = "Monitor updated successfully", body = MonitorResponse),
        (status = 400, description = "Invalid request data", body = AppResponseError),
        (status = 401, description = "Unauthorized - Invalid or missing token", body = AppResponseError),
        (status = 404, description = "Monitor not found", body = AppResponseError),
        (status = 500, description = "Internal server error", body = AppResponseError)
    ),
    security(
        ("jwt" = [])
    ),
    tag = "Monitors"
)]
pub async fn update_monitor(
    State(state): State<AppState>,
    Path(id): Path<u32>,
    claims: UserClaims,
    scope: MonitorScope,
    Json(req): Json<UpdateMonitorRequest>,
) -> AppResult<Json<MonitorResponse>> {
    req.validate().map_err(AppError::InvalidInputError)?;
    // Log the id only: the request body carries camera RTSP/ONVIF credentials.
    info!("Editing monitor with ID: {id}.");
    match service::monitor::update(&state, id, req, &scope, &claims.perms).await {
        Ok(monitor) => Ok(Json(monitor)),
        Err(e) => {
            warn!("Failed to edit monitor: {e:?}.");
            Err(e)
        }
    }
}

#[utoipa::path(
    delete,
    path = "/api/v3/monitors/{id}",
    params(
        ("id" = u32, Path, description = "Monitor identifier")
    ),
    responses(
        (status = 200, description = "Monitor deleted successfully"),
        (status = 401, description = "Unauthorized - Invalid or missing token", body = AppResponseError),
        (status = 403, description = "Needs Monitors Edit, and Edit on this monitor", body = AppResponseError),
        (status = 404, description = "Monitor not found", body = AppResponseError),
        (status = 500, description = "Internal server error", body = AppResponseError)
    ),
    security(
        ("jwt" = [])
    ),
    tag = "Monitors"
)]
pub async fn delete_monitor(
    State(state): State<AppState>,
    Path(id): Path<u32>,
    claims: UserClaims,
    scope: MonitorScope,
) -> AppResult<Json<()>> {
    // An Edit row on the monitor is not enough to remove it: ZoneMinder's
    // console also wants `canEdit('Monitors')`, the base level (#244).
    if claims.perms.monitors < crate::util::authz::Level::Edit {
        return Err(AppError::PermissionDeniedError(
            "Edit access to Monitors required".to_string(),
        ));
    }
    info!("Deleting monitor with ID: {id}.");
    match service::monitor::delete(&state, id, &scope).await {
        Ok(_) => Ok(Json(())),
        Err(e) => {
            warn!("Failed to delete monitor: {e:?}.");
            Err(e)
        }
    }
}

#[utoipa::path(
    operation_id = "update_monitor_state",
    patch,
    path = "/api/v3/monitors/{id}/state",
    params(
        ("id" = u32, Path, description = "Monitor identifier")
    ),
    request_body = MonitorStateRequest,
    responses(
        (status = 200, description = "Monitor state updated successfully", body = MonitorResponse),
        (status = 400, description = "Invalid request data", body = AppResponseError),
        (status = 401, description = "Unauthorized - Invalid or missing token", body = AppResponseError),
        (status = 404, description = "Monitor not found", body = AppResponseError),
        (status = 500, description = "Internal server error", body = AppResponseError)
    ),
    security(
        ("jwt" = [])
    ),
    tag = "Monitors"
)]
pub async fn update_state(
    State(state): State<AppState>,
    Path(id): Path<u32>,
    scope: MonitorScope,
    Json(req): Json<MonitorStateRequest>,
) -> AppResult<Json<MonitorResponse>> {
    info!("Updating state of monitor with ID: {id} and request: {req:?}.");
    req.validate().map_err(AppError::InvalidInputError)?;
    match service::monitor::update_state(&state, id, req, &scope).await {
        Ok(monitor) => Ok(Json(monitor)),
        Err(e) => {
            warn!("Failed to update state of monitor: {e:?}.");
            Err(e)
        }
    }
}

/// The monitor's live audio level
///
/// - What the monitor editor's level meter polls once a second. Each call asks
///   zmc to keep measuring for ten more seconds, even with audio detection
///   off. `level` and `alarm` are null while zmc isn't running.
#[utoipa::path(
    get,
    path = "/api/v3/monitors/{id}/audio-level",
    params(("id" = u32, Path, description = "Monitor identifier")),
    responses(
        (status = 200, description = "Live audio level", body = crate::dto::response::monitor_status::AudioLevelResponse),
        (status = 404, description = "Monitor not found", body = AppResponseError)
    ),
    security(("jwt" = [])),
    tag = "Monitors"
)]
pub async fn get_audio_level(
    State(state): State<AppState>,
    Path(id): Path<u32>,
    scope: MonitorScope,
) -> AppResult<Json<crate::dto::response::monitor_status::AudioLevelResponse>> {
    Ok(Json(
        service::monitor::audio_level(&state, id, &scope).await?,
    ))
}

#[utoipa::path(
    patch,
    path = "/api/v3/monitors/{id}/alarm",
    params(
        ("id" = u32, Path, description = "Monitor identifier")
    ),
    request_body = AlarmControlRequest,
    responses(
        (status = 200, description = "Monitor alarm controlled successfully", body = MonitorResponse),
        (status = 400, description = "Invalid request data", body = AppResponseError),
        (status = 401, description = "Unauthorized - Invalid or missing token", body = AppResponseError),
        (status = 404, description = "Monitor not found", body = AppResponseError),
        (status = 500, description = "Internal server error", body = AppResponseError) 
    ),
    security(
        ("jwt" = [])
    ),
    tag = "Monitors"
)]
pub async fn alarm_control(
    State(state): State<AppState>,
    Path(id): Path<u32>,
    scope: MonitorScope,
    Json(req): Json<AlarmControlRequest>,
) -> AppResult<Json<MonitorResponse>> {
    info!("Controlling alarm of monitor with ID: {id} and request: {req:?}.");
    // The cause/text bounds mirror the shared-memory buffers; enforcing them
    // here turns an over-long value into a 400 naming the field rather than an
    // error raised deep in the shm write.
    req.validate().map_err(AppError::InvalidInputError)?;
    match service::monitor::control_alarm(&state, id, req, &scope).await {
        Ok(monitor) => Ok(Json(monitor)),
        Err(e) => {
            warn!("Failed to control alarm of monitor: {e:?}.");
            Err(e)
        }
    }
}
