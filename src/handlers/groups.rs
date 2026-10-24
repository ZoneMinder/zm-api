use crate::dto::request::CreateGroupRequest;
use crate::dto::response::groups::PaginatedGroupsResponse;
use crate::dto::response::GroupResponse;
use crate::dto::PaginationParams;
use crate::error::{AppError, AppResult};
use crate::server::state::AppState;
use crate::service::group_acl::GroupScope;
use axum::{
    extract::{Path, Query, State},
    Json,
};
use garde::Validate;
use serde::Deserialize;

/// List user groups with pagination.
///
/// - Requires a valid JWT.
#[utoipa::path(
    get,
    path = "/api/v3/groups",
    params(
        ("page" = Option<u64>, Query, description = "Page number (1-indexed)", example = 1),
        ("page_size" = Option<u64>, Query, description = "Items per page (max 1000)", example = 25)
    ),
    responses((status = 200, description = "Paginated list of groups", body = PaginatedGroupsResponse)),
    tag = "Groups",
    security(("jwt" = []))
)]
pub async fn list_groups(
    State(state): State<AppState>,
    Query(params): Query<PaginationParams>,
    scope: GroupScope,
) -> AppResult<Json<PaginatedGroupsResponse>> {
    let result = crate::service::groups::list_paginated(&state, &params, &scope).await?;
    Ok(Json(PaginatedGroupsResponse::from(result)))
}

/// Get a group by id.
///
/// - Requires a valid JWT.
#[utoipa::path(
    get,
    path = "/api/v3/groups/{id}",
    params(("id" = u32, Path, description = "Group ID")),
    responses((status = 200, description = "Group detail", body = serde_json::Value)),
    tag = "Groups",
    security(("jwt" = []))
)]
pub async fn get_group(
    Path(id): Path<u32>,
    State(state): State<AppState>,
    scope: GroupScope,
) -> AppResult<Json<GroupResponse>> {
    let item = crate::service::groups::get_by_id(&state, id, &scope).await?;
    Ok(Json(item))
}

#[derive(Debug, Deserialize, utoipa::ToSchema, Validate)]
pub struct UpdateGroupRequest {
    #[garde(inner(length(chars, max = 64)))]
    pub name: Option<String>,
    /// Re-parent the group. Omit to leave the parent unchanged; `null` clears
    /// it (top-level group); a group id nests under that group. Self-parenting
    /// and cycles are rejected (GH #28).
    #[serde(default, deserialize_with = "crate::dto::serde_helpers::double_option")]
    #[garde(skip)]
    #[schema(value_type = Option<u32>)]
    pub parent_id: Option<Option<u32>>,
}

/// Update a group's basic attributes.
///
/// - Partial update; only provided fields are changed.
#[utoipa::path(
    put,
    path = "/api/v3/groups/{id}",
    params(("id" = u32, Path, description = "Group ID")),
    request_body = UpdateGroupRequest,
    responses((status = 200, description = "Updated group", body = serde_json::Value)),
    tag = "Groups",
    security(("jwt" = []))
)]
pub async fn update_group(
    Path(id): Path<u32>,
    State(state): State<AppState>,
    scope: GroupScope,
    Json(req): Json<UpdateGroupRequest>,
) -> AppResult<Json<GroupResponse>> {
    req.validate().map_err(AppError::InvalidInputError)?;
    let item = crate::service::groups::update(&state, id, req.name, req.parent_id, &scope).await?;
    Ok(Json(item))
}

/// Create a new group.
///
/// - Optionally specifies `parent_id` to create nested groups.
#[utoipa::path(
    post,
    path = "/api/v3/groups",
    request_body = CreateGroupRequest,
    responses((status = 201, description = "Created group", body = GroupResponse)),
    tag = "Groups",
    security(("jwt" = []))
)]
pub async fn create_group(
    State(state): State<AppState>,
    scope: GroupScope,
    Json(req): Json<CreateGroupRequest>,
) -> AppResult<(axum::http::StatusCode, Json<GroupResponse>)> {
    req.validate().map_err(AppError::InvalidInputError)?;
    let item = crate::service::groups::create(&state, req, &scope).await?;
    Ok((axum::http::StatusCode::CREATED, Json(item)))
}

/// Delete a group by id.
///
/// - Responds 204 on success, 404 if not found.
#[utoipa::path(
    delete,
    path = "/api/v3/groups/{id}",
    params(("id" = u32, Path, description = "Group ID")),
    responses((status = 204, description = "Deleted group")),
    tag = "Groups",
    security(("jwt" = []))
)]
pub async fn delete_group(
    Path(id): Path<u32>,
    State(state): State<AppState>,
    scope: GroupScope,
) -> AppResult<axum::http::StatusCode> {
    crate::service::groups::delete(&state, id, &scope).await?;
    Ok(axum::http::StatusCode::NO_CONTENT)
}
