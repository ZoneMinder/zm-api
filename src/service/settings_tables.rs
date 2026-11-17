//! ZoneMinder 1.39's EncoderTemplates and Menu_Items (#144).

use sea_orm::{ActiveModelTrait, EntityTrait, NotSet, QueryOrder, Set, TransactionTrait};

use crate::dto::request::settings_tables::{
    CreateEncoderTemplateRequest, CreateMenuItemRequest, UpdateEncoderTemplateRequest,
    UpdateMenuItemRequest,
};
use crate::dto::response::settings_tables::{
    EncoderTemplateResponse, MenuItemResponse, PrivacyResponse,
};
use crate::entity::{encoder_templates, menu_items};
use crate::error::{AppError, AppResult, Resource, ResourceType};
use crate::server::state::AppState;

fn not_found(what: &str, id: u32) -> AppError {
    AppError::NotFoundError(Resource {
        details: vec![(format!("{what}_id"), id.to_string())],
        resource_type: ResourceType::Config,
    })
}

// ---- Encoder templates ------------------------------------------------------

pub async fn list_encoder_templates(state: &AppState) -> AppResult<Vec<EncoderTemplateResponse>> {
    Ok(encoder_templates::Entity::find()
        .order_by_asc(encoder_templates::Column::Encoder)
        .order_by_asc(encoder_templates::Column::Name)
        .all(state.db())
        .await?
        .iter()
        .map(EncoderTemplateResponse::from)
        .collect())
}

async fn find_template(state: &AppState, id: u32) -> AppResult<encoder_templates::Model> {
    encoder_templates::Entity::find_by_id(id)
        .one(state.db())
        .await?
        .ok_or_else(|| not_found("encoder_template", id))
}

pub async fn get_encoder_template(state: &AppState, id: u32) -> AppResult<EncoderTemplateResponse> {
    Ok(EncoderTemplateResponse::from(
        &find_template(state, id).await?,
    ))
}

pub async fn create_encoder_template(
    state: &AppState,
    req: CreateEncoderTemplateRequest,
) -> AppResult<EncoderTemplateResponse> {
    let am = encoder_templates::ActiveModel {
        id: NotSet,
        encoder: Set(req.encoder),
        name: Set(req.name),
        description: Set(req.description),
        params: Set(req.params),
    };
    Ok(EncoderTemplateResponse::from(&am.insert(state.db()).await?))
}

pub async fn update_encoder_template(
    state: &AppState,
    id: u32,
    req: UpdateEncoderTemplateRequest,
) -> AppResult<EncoderTemplateResponse> {
    let mut am: encoder_templates::ActiveModel = find_template(state, id).await?.into();
    if let Some(v) = req.encoder {
        am.encoder = Set(v);
    }
    if let Some(v) = req.name {
        am.name = Set(v);
    }
    if let Some(v) = req.description {
        am.description = Set(v);
    }
    if let Some(v) = req.params {
        am.params = Set(v);
    }
    Ok(EncoderTemplateResponse::from(&am.update(state.db()).await?))
}

pub async fn delete_encoder_template(state: &AppState, id: u32) -> AppResult<()> {
    let res = encoder_templates::Entity::delete_by_id(id)
        .exec(state.db())
        .await?;
    if res.rows_affected == 0 {
        return Err(not_found("encoder_template", id));
    }
    Ok(())
}

// ---- Menu items ---------------------------------------------------------------

/// Every menu item, in navbar order.
pub async fn list_menu_items(state: &AppState) -> AppResult<Vec<MenuItemResponse>> {
    Ok(menu_items::Entity::find()
        .order_by_asc(menu_items::Column::SortOrder)
        .order_by_asc(menu_items::Column::Id)
        .all(state.db())
        .await?
        .iter()
        .map(MenuItemResponse::from)
        .collect())
}

/// The enabled menu items, in order: what a navbar renders.
pub async fn navbar(state: &AppState) -> AppResult<Vec<MenuItemResponse>> {
    Ok(list_menu_items(state)
        .await?
        .into_iter()
        .filter(|m| m.enabled)
        .collect())
}

async fn find_item(state: &AppState, id: u32) -> AppResult<menu_items::Model> {
    menu_items::Entity::find_by_id(id)
        .one(state.db())
        .await?
        .ok_or_else(|| not_found("menu_item", id))
}

pub async fn get_menu_item(state: &AppState, id: u32) -> AppResult<MenuItemResponse> {
    Ok(MenuItemResponse::from(&find_item(state, id).await?))
}

pub async fn create_menu_item(
    state: &AppState,
    req: CreateMenuItemRequest,
) -> AppResult<MenuItemResponse> {
    let am = menu_items::ActiveModel {
        id: NotSet,
        menu_key: Set(req.menu_key),
        enabled: req.enabled.map_or(NotSet, |v| Set(v as i8)),
        label: Set(req.label),
        sort_order: req.sort_order.map_or(NotSet, Set),
        icon: Set(req.icon),
        icon_type: req.icon_type.map_or(NotSet, Set),
        link: Set(req.link),
    };
    Ok(MenuItemResponse::from(&am.insert(state.db()).await?))
}

pub async fn update_menu_item(
    state: &AppState,
    id: u32,
    req: UpdateMenuItemRequest,
) -> AppResult<MenuItemResponse> {
    let mut am: menu_items::ActiveModel = find_item(state, id).await?.into();
    if let Some(v) = req.enabled {
        am.enabled = Set(v as i8);
    }
    if let Some(v) = req.label {
        am.label = Set(v);
    }
    if let Some(v) = req.sort_order {
        am.sort_order = Set(v);
    }
    if let Some(v) = req.icon {
        am.icon = Set(v);
    }
    if let Some(v) = req.icon_type {
        am.icon_type = Set(v);
    }
    if let Some(v) = req.link {
        am.link = Set(v);
    }
    Ok(MenuItemResponse::from(&am.update(state.db()).await?))
}

pub async fn delete_menu_item(state: &AppState, id: u32) -> AppResult<()> {
    let res = menu_items::Entity::delete_by_id(id)
        .exec(state.db())
        .await?;
    if res.rows_affected == 0 {
        return Err(not_found("menu_item", id));
    }
    Ok(())
}

/// The menu a fresh install has: ZoneMinder's `zm_create.sql` seed. Its
/// Options > Menu Reset (`actions/options.php`, `resetmenu`) inserts the
/// same rows less Watch; a reset here gives back what a new install shows.
pub const DEFAULT_MENU: &[(&str, i16)] = &[
    ("Console", 10),
    ("Watch", 15),
    ("Montage", 20),
    ("MontageReview", 30),
    ("Events", 40),
    ("Options", 50),
    ("Log", 60),
    ("Devices", 70),
    ("IntelGpu", 80),
    ("Groups", 90),
    ("Filters", 100),
    ("Snapshots", 110),
    ("Reports", 120),
    ("ReportEventAudit", 130),
    ("Map", 140),
];

/// Replace every menu item with [`DEFAULT_MENU`], in one transaction, so a
/// failure leaves the old menu as it was (#197).
pub async fn reset_menu_items(state: &AppState) -> AppResult<Vec<MenuItemResponse>> {
    let txn = state.db().begin().await?;
    menu_items::Entity::delete_many().exec(&txn).await?;
    menu_items::Entity::insert_many(DEFAULT_MENU.iter().map(|(key, order)| {
        menu_items::ActiveModel {
            id: NotSet,
            menu_key: Set((*key).to_string()),
            enabled: Set(1),
            sort_order: Set(*order),
            ..Default::default()
        }
    }))
    .exec(&txn)
    .await?;
    txn.commit().await?;
    list_menu_items(state).await
}

// ---- Privacy statement ---------------------------------------------------------

/// What legacy's `actions/privacy.php` does with an answer: the statement is
/// no longer shown, and telemetry follows the answer. `ZM_SHOW_PRIVACY` is
/// read-only on the Options page, so this is the only way to write it (#189).
pub async fn record_privacy(state: &AppState, accept: bool) -> AppResult<PrivacyResponse> {
    let db = state.db();
    crate::repo::config::update_value(db, "ZM_SHOW_PRIVACY", "0").await?;
    crate::repo::config::update_value(db, "ZM_TELEMETRY_DATA", if accept { "1" } else { "0" })
        .await?;
    let read = |name: &'static str| async move {
        crate::repo::config::get_config_value(db, name)
            .await
            .map(|v| v.map(|v| v.trim() == "1"))
    };
    Ok(PrivacyResponse {
        show_privacy: read("ZM_SHOW_PRIVACY").await?,
        telemetry: read("ZM_TELEMETRY_DATA").await?,
    })
}
