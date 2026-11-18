use crate::dto::request::triggers_x10::{CreateTriggerX10Request, UpdateTriggerX10Request};
use crate::dto::response::triggers_x10::PaginatedTriggersX10Response;
use crate::dto::response::TriggerX10Response;
use crate::dto::PaginationParams;
use crate::error::{AppError, AppResult};
use crate::server::state::AppState;
use crate::service::monitor_acl::MonitorScope;
use axum::{
    extract::{Path, Query, State},
    Json,
};
use garde::Validate;

/// List all X10 triggers with pagination.
///
/// - Requires a valid JWT.
#[utoipa::path(
    get,
    path = "/api/v3/triggers_x10",
    params(
        ("page" = Option<u64>, Query, description = "Page number (1-indexed)", example = 1),
        ("page_size" = Option<u64>, Query, description = "Items per page (max 1000)", example = 25)
    ),
    responses((status = 200, description = "Paginated list of X10 triggers", body = PaginatedTriggersX10Response)),
    tag = "TriggersX10",
    security(("jwt" = []))
)]
pub async fn list_triggers_x10(
    State(state): State<AppState>,
    Query(params): Query<PaginationParams>,
    scope: MonitorScope,
) -> AppResult<Json<PaginatedTriggersX10Response>> {
    let result = crate::service::triggers_x10::list_paginated(&state, &params, &scope).await?;
    Ok(Json(PaginatedTriggersX10Response::from(result)))
}

/// Get an X10 trigger by monitor_id.
///
/// - Requires a valid JWT; responds 404 if not found.
#[utoipa::path(
    get,
    path = "/api/v3/triggers_x10/{monitor_id}",
    params(("monitor_id" = u32, Path, description = "Monitor ID")),
    responses((status = 200, description = "X10 trigger detail", body = TriggerX10Response)),
    tag = "TriggersX10",
    security(("jwt" = []))
)]
pub async fn get_trigger_x10(
    Path(monitor_id): Path<u32>,
    State(state): State<AppState>,
    scope: MonitorScope,
) -> AppResult<Json<TriggerX10Response>> {
    let item = crate::service::triggers_x10::get_by_id(&state, monitor_id, &scope).await?;
    Ok(Json(item))
}

/// Create a new X10 trigger entry.
///
/// - Requires a valid JWT.
/// - monitor_id is the primary key.
#[utoipa::path(
    post,
    path = "/api/v3/triggers_x10",
    request_body = CreateTriggerX10Request,
    responses((status = 201, description = "Created X10 trigger", body = TriggerX10Response)),
    tag = "TriggersX10",
    security(("jwt" = []))
)]
pub async fn create_trigger_x10(
    State(state): State<AppState>,
    scope: MonitorScope,
    Json(req): Json<CreateTriggerX10Request>,
) -> AppResult<(axum::http::StatusCode, Json<TriggerX10Response>)> {
    req.validate().map_err(AppError::InvalidInputError)?;
    let item = crate::service::triggers_x10::create(&state, req, &scope).await?;
    Ok((axum::http::StatusCode::CREATED, Json(item)))
}

/// Update an X10 trigger entry.
///
/// - Partial update.
/// - Requires a valid JWT.
#[utoipa::path(
    patch,
    path = "/api/v3/triggers_x10/{monitor_id}",
    params(("monitor_id" = u32, Path, description = "Monitor ID")),
    request_body = UpdateTriggerX10Request,
    responses((status = 200, description = "Updated X10 trigger", body = TriggerX10Response)),
    tag = "TriggersX10",
    security(("jwt" = []))
)]
pub async fn update_trigger_x10(
    Path(monitor_id): Path<u32>,
    State(state): State<AppState>,
    scope: MonitorScope,
    Json(req): Json<UpdateTriggerX10Request>,
) -> AppResult<Json<TriggerX10Response>> {
    req.validate().map_err(AppError::InvalidInputError)?;
    let item = crate::service::triggers_x10::update(&state, monitor_id, req, &scope).await?;
    Ok(Json(item))
}

/// Delete an X10 trigger by monitor_id.
///
/// - Responds 204 on success, 404 if not found.
/// - Requires a valid JWT.
#[utoipa::path(
    delete,
    path = "/api/v3/triggers_x10/{monitor_id}",
    params(("monitor_id" = u32, Path, description = "Monitor ID")),
    responses((status = 204, description = "Deleted X10 trigger")),
    tag = "TriggersX10",
    security(("jwt" = []))
)]
pub async fn delete_trigger_x10(
    Path(monitor_id): Path<u32>,
    State(state): State<AppState>,
    scope: MonitorScope,
) -> AppResult<axum::http::StatusCode> {
    crate::service::triggers_x10::delete(&state, monitor_id, &scope).await?;
    Ok(axum::http::StatusCode::NO_CONTENT)
}
