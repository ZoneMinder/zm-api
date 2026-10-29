//! `/api/v3/encoder-templates` and `/api/v3/menu-items`: ZoneMinder 1.39's
//! EncoderTemplates and Menu_Items (#144). System-gated; the navbar's view of
//! the menu, `GET /api/v3/system/menu`, needs only a login.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::Json;
use garde::Validate;

use crate::dto::request::settings_tables::*;
use crate::dto::response::settings_tables::*;
use crate::error::{AppError, AppResponseError, AppResult};
use crate::server::state::AppState;
use crate::service::settings_tables as svc;

/// List encoder templates
#[utoipa::path(
    get,
    path = "/api/v3/encoder-templates",
    responses((status = 200, description = "Encoder templates", body = Vec<EncoderTemplateResponse>)),
    tag = "System",
    security(("jwt" = []))
)]
pub async fn list_encoder_templates(
    State(state): State<AppState>,
) -> AppResult<Json<Vec<EncoderTemplateResponse>>> {
    Ok(Json(svc::list_encoder_templates(&state).await?))
}

/// Get one encoder template
#[utoipa::path(
    get,
    path = "/api/v3/encoder-templates/{id}",
    params(("id" = u32, Path, description = "Encoder template ID")),
    responses(
        (status = 200, description = "Encoder template", body = EncoderTemplateResponse),
        (status = 404, description = "Not found", body = AppResponseError)
    ),
    tag = "System",
    security(("jwt" = []))
)]
pub async fn get_encoder_template(
    State(state): State<AppState>,
    Path(id): Path<u32>,
) -> AppResult<Json<EncoderTemplateResponse>> {
    Ok(Json(svc::get_encoder_template(&state, id).await?))
}

/// Create a encoder template
#[utoipa::path(
    post,
    path = "/api/v3/encoder-templates",
    request_body = CreateEncoderTemplateRequest,
    responses((status = 201, description = "Created", body = EncoderTemplateResponse)),
    tag = "System",
    security(("jwt" = []))
)]
pub async fn create_encoder_template(
    State(state): State<AppState>,
    Json(req): Json<CreateEncoderTemplateRequest>,
) -> AppResult<(StatusCode, Json<EncoderTemplateResponse>)> {
    req.validate().map_err(AppError::InvalidInputError)?;
    Ok((
        StatusCode::CREATED,
        Json(svc::create_encoder_template(&state, req).await?),
    ))
}

/// Update a encoder template
#[utoipa::path(
    put,
    path = "/api/v3/encoder-templates/{id}",
    params(("id" = u32, Path, description = "Encoder template ID")),
    request_body = UpdateEncoderTemplateRequest,
    responses(
        (status = 200, description = "Updated", body = EncoderTemplateResponse),
        (status = 404, description = "Not found", body = AppResponseError)
    ),
    tag = "System",
    security(("jwt" = []))
)]
pub async fn update_encoder_template(
    State(state): State<AppState>,
    Path(id): Path<u32>,
    Json(req): Json<UpdateEncoderTemplateRequest>,
) -> AppResult<Json<EncoderTemplateResponse>> {
    req.validate().map_err(AppError::InvalidInputError)?;
    Ok(Json(svc::update_encoder_template(&state, id, req).await?))
}

/// Delete a encoder template
#[utoipa::path(
    delete,
    path = "/api/v3/encoder-templates/{id}",
    params(("id" = u32, Path, description = "Encoder template ID")),
    responses(
        (status = 204, description = "Deleted"),
        (status = 404, description = "Not found", body = AppResponseError)
    ),
    tag = "System",
    security(("jwt" = []))
)]
pub async fn delete_encoder_template(
    State(state): State<AppState>,
    Path(id): Path<u32>,
) -> AppResult<StatusCode> {
    svc::delete_encoder_template(&state, id).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// List menu items
#[utoipa::path(
    get,
    path = "/api/v3/menu-items",
    responses((status = 200, description = "Menu items", body = Vec<MenuItemResponse>)),
    tag = "System",
    security(("jwt" = []))
)]
pub async fn list_menu_items(
    State(state): State<AppState>,
) -> AppResult<Json<Vec<MenuItemResponse>>> {
    Ok(Json(svc::list_menu_items(&state).await?))
}

/// Get one menu item
#[utoipa::path(
    get,
    path = "/api/v3/menu-items/{id}",
    params(("id" = u32, Path, description = "Menu item ID")),
    responses(
        (status = 200, description = "Menu item", body = MenuItemResponse),
        (status = 404, description = "Not found", body = AppResponseError)
    ),
    tag = "System",
    security(("jwt" = []))
)]
pub async fn get_menu_item(
    State(state): State<AppState>,
    Path(id): Path<u32>,
) -> AppResult<Json<MenuItemResponse>> {
    Ok(Json(svc::get_menu_item(&state, id).await?))
}

/// Reset the menu to a fresh install's
///
/// Deletes every menu item and inserts ZoneMinder's default rows (Console
/// through Map, Watch included, as `zm_create.sql` seeds them), in one
/// transaction. Returns the new list.
#[utoipa::path(
    post,
    path = "/api/v3/menu-items/reset",
    responses(
        (status = 200, description = "The default menu items", body = Vec<MenuItemResponse>),
        (status = 403, description = "Needs System Edit", body = crate::error::AppResponseError)
    ),
    tag = "System",
    security(("jwt" = []))
)]
pub async fn reset_menu_items(
    State(state): State<AppState>,
) -> AppResult<Json<Vec<MenuItemResponse>>> {
    Ok(Json(svc::reset_menu_items(&state).await?))
}

/// Create a menu item
#[utoipa::path(
    post,
    path = "/api/v3/menu-items",
    request_body = CreateMenuItemRequest,
    responses((status = 201, description = "Created", body = MenuItemResponse)),
    tag = "System",
    security(("jwt" = []))
)]
pub async fn create_menu_item(
    State(state): State<AppState>,
    Json(req): Json<CreateMenuItemRequest>,
) -> AppResult<(StatusCode, Json<MenuItemResponse>)> {
    req.validate().map_err(AppError::InvalidInputError)?;
    Ok((
        StatusCode::CREATED,
        Json(svc::create_menu_item(&state, req).await?),
    ))
}

/// Update a menu item
#[utoipa::path(
    put,
    path = "/api/v3/menu-items/{id}",
    params(("id" = u32, Path, description = "Menu item ID")),
    request_body = UpdateMenuItemRequest,
    responses(
        (status = 200, description = "Updated", body = MenuItemResponse),
        (status = 404, description = "Not found", body = AppResponseError)
    ),
    tag = "System",
    security(("jwt" = []))
)]
pub async fn update_menu_item(
    State(state): State<AppState>,
    Path(id): Path<u32>,
    Json(req): Json<UpdateMenuItemRequest>,
) -> AppResult<Json<MenuItemResponse>> {
    req.validate().map_err(AppError::InvalidInputError)?;
    Ok(Json(svc::update_menu_item(&state, id, req).await?))
}

/// Delete a menu item
#[utoipa::path(
    delete,
    path = "/api/v3/menu-items/{id}",
    params(("id" = u32, Path, description = "Menu item ID")),
    responses(
        (status = 204, description = "Deleted"),
        (status = 404, description = "Not found", body = AppResponseError)
    ),
    tag = "System",
    security(("jwt" = []))
)]
pub async fn delete_menu_item(
    State(state): State<AppState>,
    Path(id): Path<u32>,
) -> AppResult<StatusCode> {
    svc::delete_menu_item(&state, id).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// The navbar: enabled menu items in order
///
/// - Needs a login but no feature permission: every user's navbar renders it.
#[utoipa::path(
    get,
    path = "/api/v3/system/menu",
    responses((status = 200, description = "Enabled menu items, in order", body = Vec<MenuItemResponse>)),
    tag = "System",
    security(("jwt" = []))
)]
pub async fn get_navbar(State(state): State<AppState>) -> AppResult<Json<Vec<MenuItemResponse>>> {
    Ok(Json(svc::navbar(&state).await?))
}

/// Record the answer to the privacy statement
///
/// - What ZoneMinder's privacy page does: the statement stops being shown,
///   and telemetry (`ZM_TELEMETRY_DATA`) follows the answer.
/// - Requires System edit.
#[utoipa::path(
    post,
    path = "/api/v3/system/privacy",
    request_body = PrivacyRequest,
    responses((status = 200, description = "Privacy settings now", body = PrivacyResponse)),
    tag = "System",
    security(("jwt" = []))
)]
pub async fn record_privacy(
    State(state): State<AppState>,
    Json(req): Json<PrivacyRequest>,
) -> AppResult<Json<PrivacyResponse>> {
    Ok(Json(svc::record_privacy(&state, req.accept).await?))
}
