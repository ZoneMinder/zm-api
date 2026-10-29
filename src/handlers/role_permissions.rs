//! A role's per-monitor and per-group permissions (#196).

use axum::{
    extract::{Path, State},
    http::StatusCode,
    Json,
};

use crate::dto::request::role_permissions::SetRolePermissionRequest;
use crate::dto::response::role_permissions::RolePermissionResponse;
use crate::error::{AppResponseError, AppResult};
use crate::server::state::AppState;
use crate::service::role_permissions as svc;

/// List a role's per-monitor permissions
#[utoipa::path(
    operation_id = "listRoleMonitorPermissions",
    get,
    path = "/api/v3/roles/{id}/monitors-permissions",
    params(("id" = u32, Path, description = "Role ID")),
    responses(
        (status = 200, description = "The role's monitor rows; target_id is the monitor", body = Vec<RolePermissionResponse>),
        (status = 404, description = "No such role", body = AppResponseError)
    ),
    tag = "Roles",
    security(("jwt" = []))
)]
pub async fn list_monitors(
    State(state): State<AppState>,
    Path(role_id): Path<u32>,
) -> AppResult<Json<Vec<RolePermissionResponse>>> {
    Ok(Json(svc::list_monitors(&state, role_id).await?))
}

/// Set a role's permission on a monitor
#[utoipa::path(
    operation_id = "setRoleMonitorPermission",
    put,
    path = "/api/v3/roles/{id}/monitors-permissions/{monitor_id}",
    params(
        ("id" = u32, Path, description = "Role ID"),
        ("monitor_id" = u32, Path, description = "Monitor ID")
    ),
    request_body = SetRolePermissionRequest,
    responses(
        (status = 200, description = "The row as saved", body = RolePermissionResponse),
        (status = 400, description = "Not Inherit, None, View or Edit", body = AppResponseError),
        (status = 404, description = "No such role or monitor", body = AppResponseError)
    ),
    tag = "Roles",
    security(("jwt" = []))
)]
pub async fn set_monitor(
    State(state): State<AppState>,
    Path((role_id, monitor_id)): Path<(u32, u32)>,
    Json(req): Json<SetRolePermissionRequest>,
) -> AppResult<Json<RolePermissionResponse>> {
    Ok(Json(
        svc::set_monitor(&state, role_id, monitor_id, &req.permission).await?,
    ))
}

/// Clear a role's permission on a monitor
#[utoipa::path(
    operation_id = "clearRoleMonitorPermission",
    delete,
    path = "/api/v3/roles/{id}/monitors-permissions/{monitor_id}",
    params(
        ("id" = u32, Path, description = "Role ID"),
        ("monitor_id" = u32, Path, description = "Monitor ID")
    ),
    responses(
        (status = 204, description = "Removed; the role's Monitors level applies"),
        (status = 404, description = "The role has no row for that monitor", body = AppResponseError)
    ),
    tag = "Roles",
    security(("jwt" = []))
)]
pub async fn clear_monitor(
    State(state): State<AppState>,
    Path((role_id, monitor_id)): Path<(u32, u32)>,
) -> AppResult<StatusCode> {
    svc::clear_monitor(&state, role_id, monitor_id).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// List a role's per-group permissions
#[utoipa::path(
    operation_id = "listRoleGroupPermissions",
    get,
    path = "/api/v3/roles/{id}/groups-permissions",
    params(("id" = u32, Path, description = "Role ID")),
    responses(
        (status = 200, description = "The role's group rows; target_id is the group", body = Vec<RolePermissionResponse>),
        (status = 404, description = "No such role", body = AppResponseError)
    ),
    tag = "Roles",
    security(("jwt" = []))
)]
pub async fn list_groups(
    State(state): State<AppState>,
    Path(role_id): Path<u32>,
) -> AppResult<Json<Vec<RolePermissionResponse>>> {
    Ok(Json(svc::list_groups(&state, role_id).await?))
}

/// Set a role's permission on a group
#[utoipa::path(
    operation_id = "setRoleGroupPermission",
    put,
    path = "/api/v3/roles/{id}/groups-permissions/{group_id}",
    params(
        ("id" = u32, Path, description = "Role ID"),
        ("group_id" = u32, Path, description = "Group ID")
    ),
    request_body = SetRolePermissionRequest,
    responses(
        (status = 200, description = "The row as saved", body = RolePermissionResponse),
        (status = 400, description = "Not Inherit, None, View or Edit", body = AppResponseError),
        (status = 404, description = "No such role or group", body = AppResponseError)
    ),
    tag = "Roles",
    security(("jwt" = []))
)]
pub async fn set_group(
    State(state): State<AppState>,
    Path((role_id, group_id)): Path<(u32, u32)>,
    Json(req): Json<SetRolePermissionRequest>,
) -> AppResult<Json<RolePermissionResponse>> {
    Ok(Json(
        svc::set_group(&state, role_id, group_id, &req.permission).await?,
    ))
}

/// Clear a role's permission on a group
#[utoipa::path(
    operation_id = "clearRoleGroupPermission",
    delete,
    path = "/api/v3/roles/{id}/groups-permissions/{group_id}",
    params(
        ("id" = u32, Path, description = "Role ID"),
        ("group_id" = u32, Path, description = "Group ID")
    ),
    responses(
        (status = 204, description = "Removed"),
        (status = 404, description = "The role has no row for that group", body = AppResponseError)
    ),
    tag = "Roles",
    security(("jwt" = []))
)]
pub async fn clear_group(
    State(state): State<AppState>,
    Path((role_id, group_id)): Path<(u32, u32)>,
) -> AppResult<StatusCode> {
    svc::clear_group(&state, role_id, group_id).await?;
    Ok(StatusCode::NO_CONTENT)
}
