//! A role's per-monitor and per-group permissions (#196): ZoneMinder 1.39's
//! `Role_Monitors_Permissions` and `Role_Groups_Permissions`, which its role
//! editor shows below the global levels. The monitor ACL already applies
//! them to a role's users (`monitor_acl`); these are the rows behind it.

use sea_orm::sea_query::OnConflict;
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter, QueryOrder, Set};

use crate::dto::response::role_permissions::RolePermissionResponse;
use crate::entity::sea_orm_active_enums::Permission;
use crate::entity::user_roles;
use crate::entity::{groups, monitors, role_groups_permissions, role_monitors_permissions};
use crate::error::{AppError, AppResult, Resource, ResourceType};
use crate::server::state::AppState;

fn not_found(kind: ResourceType, field: &str, id: u32) -> AppError {
    AppError::NotFoundError(Resource {
        details: vec![(field.to_string(), id.to_string())],
        resource_type: kind,
    })
}

/// `Inherit`, `None`, `View` or `Edit`, as ZoneMinder stores them.
pub fn parse_permission(s: &str) -> AppResult<Permission> {
    match s {
        "Inherit" => Ok(Permission::Inherit),
        "None" => Ok(Permission::None),
        "View" => Ok(Permission::View),
        "Edit" => Ok(Permission::Edit),
        other => Err(AppError::BadRequestError(format!(
            "permission must be Inherit, None, View or Edit, not {other:?}"
        ))),
    }
}

async fn ensure_role(state: &AppState, role_id: u32) -> AppResult<()> {
    user_roles::Entity::find_by_id(role_id)
        .one(state.db())
        .await?
        .map(|_| ())
        .ok_or_else(|| not_found(ResourceType::User, "role_id", role_id))
}

// ---- Monitors ------------------------------------------------------------------

pub async fn list_monitors(
    state: &AppState,
    role_id: u32,
) -> AppResult<Vec<RolePermissionResponse>> {
    ensure_role(state, role_id).await?;
    Ok(role_monitors_permissions::Entity::find()
        .filter(role_monitors_permissions::Column::RoleId.eq(role_id))
        .order_by_asc(role_monitors_permissions::Column::MonitorId)
        .all(state.db())
        .await?
        .iter()
        .map(|r| RolePermissionResponse::new(r.id, r.role_id, r.monitor_id, &r.permission))
        .collect())
}

/// Set the role's permission on a monitor, adding the row if there is none.
pub async fn set_monitor(
    state: &AppState,
    role_id: u32,
    monitor_id: u32,
    permission: &str,
) -> AppResult<RolePermissionResponse> {
    let permission = parse_permission(permission)?;
    ensure_role(state, role_id).await?;
    monitors::Entity::find_by_id(monitor_id)
        .one(state.db())
        .await?
        .ok_or_else(|| not_found(ResourceType::Monitor, "monitor_id", monitor_id))?;
    // One statement, so two first-time PUTs at once can't both insert:
    // (RoleId, MonitorId) is unique.
    role_monitors_permissions::Entity::insert(role_monitors_permissions::ActiveModel {
        role_id: Set(role_id),
        monitor_id: Set(monitor_id),
        permission: Set(permission),
        ..Default::default()
    })
    .on_conflict(
        OnConflict::columns([
            role_monitors_permissions::Column::RoleId,
            role_monitors_permissions::Column::MonitorId,
        ])
        .update_column(role_monitors_permissions::Column::Permission)
        .to_owned(),
    )
    .exec_without_returning(state.db())
    .await?;
    let row = role_monitors_permissions::Entity::find()
        .filter(role_monitors_permissions::Column::RoleId.eq(role_id))
        .filter(role_monitors_permissions::Column::MonitorId.eq(monitor_id))
        .one(state.db())
        .await?
        .ok_or_else(|| AppError::InternalServerError("role permission row vanished".into()))?;
    Ok(RolePermissionResponse::new(
        row.id,
        row.role_id,
        row.monitor_id,
        &row.permission,
    ))
}

/// Remove the role's row for a monitor, so the role's Monitors level applies.
pub async fn clear_monitor(state: &AppState, role_id: u32, monitor_id: u32) -> AppResult<()> {
    let res = role_monitors_permissions::Entity::delete_many()
        .filter(role_monitors_permissions::Column::RoleId.eq(role_id))
        .filter(role_monitors_permissions::Column::MonitorId.eq(monitor_id))
        .exec(state.db())
        .await?;
    if res.rows_affected == 0 {
        return Err(not_found(ResourceType::Monitor, "monitor_id", monitor_id));
    }
    Ok(())
}

// ---- Groups --------------------------------------------------------------------

pub async fn list_groups(state: &AppState, role_id: u32) -> AppResult<Vec<RolePermissionResponse>> {
    ensure_role(state, role_id).await?;
    Ok(role_groups_permissions::Entity::find()
        .filter(role_groups_permissions::Column::RoleId.eq(role_id))
        .order_by_asc(role_groups_permissions::Column::GroupId)
        .all(state.db())
        .await?
        .iter()
        .map(|r| RolePermissionResponse::new(r.id, r.role_id, r.group_id, &r.permission))
        .collect())
}

/// Set the role's permission on a group, adding the row if there is none.
pub async fn set_group(
    state: &AppState,
    role_id: u32,
    group_id: u32,
    permission: &str,
) -> AppResult<RolePermissionResponse> {
    let permission = parse_permission(permission)?;
    ensure_role(state, role_id).await?;
    groups::Entity::find_by_id(group_id)
        .one(state.db())
        .await?
        .ok_or_else(|| not_found(ResourceType::Config, "group_id", group_id))?;
    // One statement, so two first-time PUTs at once can't both insert:
    // (RoleId, GroupId) is unique.
    role_groups_permissions::Entity::insert(role_groups_permissions::ActiveModel {
        role_id: Set(role_id),
        group_id: Set(group_id),
        permission: Set(permission),
        ..Default::default()
    })
    .on_conflict(
        OnConflict::columns([
            role_groups_permissions::Column::RoleId,
            role_groups_permissions::Column::GroupId,
        ])
        .update_column(role_groups_permissions::Column::Permission)
        .to_owned(),
    )
    .exec_without_returning(state.db())
    .await?;
    let row = role_groups_permissions::Entity::find()
        .filter(role_groups_permissions::Column::RoleId.eq(role_id))
        .filter(role_groups_permissions::Column::GroupId.eq(group_id))
        .one(state.db())
        .await?
        .ok_or_else(|| AppError::InternalServerError("role permission row vanished".into()))?;
    Ok(RolePermissionResponse::new(
        row.id,
        row.role_id,
        row.group_id,
        &row.permission,
    ))
}

/// Remove the role's row for a group.
pub async fn clear_group(state: &AppState, role_id: u32, group_id: u32) -> AppResult<()> {
    let res = role_groups_permissions::Entity::delete_many()
        .filter(role_groups_permissions::Column::RoleId.eq(role_id))
        .filter(role_groups_permissions::Column::GroupId.eq(group_id))
        .exec(state.db())
        .await?;
    if res.rows_affected == 0 {
        return Err(not_found(ResourceType::Config, "group_id", group_id));
    }
    Ok(())
}
