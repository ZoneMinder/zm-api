use crate::dto::request::CreateStorageRequest;
use crate::dto::response::storage::PaginatedStorageResponse;
use crate::dto::response::StorageResponse;
use crate::dto::PaginationParams;
use crate::error::{AppError, AppResult};
use crate::server::state::AppState;
use axum::{
    extract::{Path, Query, State},
    Json,
};
use garde::Validate;
use serde::Deserialize;

/// Whether the caller may edit storage, and so already knows its secrets.
fn may_edit(claims: &crate::util::claim::UserClaims) -> bool {
    use crate::util::authz::{Feature, Level};
    claims.perms.level(Feature::System) >= Level::Edit
}

/// List storage definitions used by ZoneMinder for event/video storage.
///
/// - Requires a valid JWT.
#[utoipa::path(
    get,
    path = "/api/v3/storage",
    params(
        ("page" = Option<u64>, Query, description = "Page number (1-indexed)", example = 1),
        ("page_size" = Option<u64>, Query, description = "Items per page (max 1000)", example = 25)
    ),
    responses((status = 200, description = "Paginated list of storage", body = PaginatedStorageResponse)),
    tag = "Storage",
    security(("jwt" = []))
)]
pub async fn list_storage(
    State(state): State<AppState>,
    axum::Extension(claims): axum::Extension<crate::util::claim::UserClaims>,
    Query(params): Query<PaginationParams>,
) -> AppResult<Json<PaginatedStorageResponse>> {
    let mut result = crate::service::storage::list_paginated(&state, &params).await?;
    if !may_edit(&claims) {
        result.items = result
            .items
            .into_iter()
            .map(StorageResponse::without_secrets)
            .collect();
    }
    Ok(Json(PaginatedStorageResponse::from(result)))
}

/// Get a storage entry by id.
///
/// - Requires a valid JWT; responds 404 if not found.
#[utoipa::path(
    get,
    path = "/api/v3/storage/{id}",
    params(("id" = u16, Path, description = "Storage ID")),
    responses((status = 200, description = "Storage detail", body = serde_json::Value)),
    tag = "Storage",
    security(("jwt" = []))
)]
pub async fn get_storage(
    Path(id): Path<u16>,
    State(state): State<AppState>,
    axum::Extension(claims): axum::Extension<crate::util::claim::UserClaims>,
) -> AppResult<Json<StorageResponse>> {
    let item = crate::service::storage::get_by_id(&state, id).await?;
    Ok(Json(if may_edit(&claims) {
        item
    } else {
        item.without_secrets()
    }))
}

/// Create a new storage entry.
///
/// - Accepts `type` and `scheme` as strings; converted to DB enums.
/// - Requires a valid JWT.
#[utoipa::path(
    post,
    path = "/api/v3/storage",
    request_body = CreateStorageRequest,
    responses((status = 201, description = "Created storage", body = StorageResponse)),
    tag = "Storage",
    security(("jwt" = []))
)]
pub async fn create_storage(
    State(state): State<AppState>,
    Json(req): Json<CreateStorageRequest>,
) -> AppResult<(axum::http::StatusCode, Json<StorageResponse>)> {
    req.validate().map_err(AppError::InvalidInputError)?;
    let item = crate::service::storage::create(&state, req).await?;
    Ok((axum::http::StatusCode::CREATED, Json(item)))
}

#[derive(Debug, Deserialize, utoipa::ToSchema, Validate)]
pub struct UpdateStorageRequest {
    #[garde(inner(length(chars, max = 64)))]
    pub name: Option<String>,
    #[garde(inner(length(chars, max = 64)))]
    #[garde(inner(custom(crate::dto::request::monitor::is_safe_storage_path)))]
    pub path: Option<String>,
    #[garde(skip)]
    pub r#type: Option<String>,
    #[garde(skip)]
    pub enabled: Option<i8>,
    #[garde(skip)]
    pub scheme: Option<String>,
    #[garde(skip)]
    pub server_id: Option<u32>,
    #[garde(inner(length(chars, max = 255)))]
    pub url: Option<String>,
    /// Whether ZoneMinder may delete media from this area (`DoDelete`, 0/1);
    /// the reaper and event deletes honour it (#185).
    #[garde(inner(range(min = 0, max = 1)))]
    pub do_delete: Option<i8>,
}

/// Update storage fields (partial update).
///
/// - Applies provided fields; type/scheme strings are mapped to enums.
/// - Requires a valid JWT.
#[utoipa::path(
    patch,
    path = "/api/v3/storage/{id}",
    params(("id" = u16, Path, description = "Storage ID")),
    request_body = UpdateStorageRequest,
    responses((status = 200, description = "Updated storage", body = StorageResponse)),
    tag = "Storage",
    security(("jwt" = []))
)]
pub async fn update_storage(
    Path(id): Path<u16>,
    State(state): State<AppState>,
    Json(req): Json<UpdateStorageRequest>,
) -> AppResult<Json<StorageResponse>> {
    req.validate().map_err(AppError::InvalidInputError)?;
    let item = crate::service::storage::update(
        &state,
        id,
        req.name,
        req.path,
        req.r#type,
        req.enabled,
        req.scheme,
        req.server_id,
        req.url,
        req.do_delete,
    )
    .await?;
    Ok(Json(item))
}

/// Delete a storage entry by id.
///
/// - Responds 204 on success, 404 if not found.
/// - Requires a valid JWT.
#[utoipa::path(
    delete,
    path = "/api/v3/storage/{id}",
    params(("id" = u16, Path, description = "Storage ID")),
    responses((status = 204, description = "Deleted storage")),
    tag = "Storage",
    security(("jwt" = []))
)]
pub async fn delete_storage(
    Path(id): Path<u16>,
    State(state): State<AppState>,
) -> AppResult<axum::http::StatusCode> {
    crate::service::storage::delete(&state, id).await?;
    Ok(axum::http::StatusCode::NO_CONTENT)
}
