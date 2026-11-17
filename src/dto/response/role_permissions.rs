use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::entity::sea_orm_active_enums::Permission;

/// One row of a role's per-monitor or per-group permissions. `target_id`
/// is the monitor's or the group's id, as the route says.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct RolePermissionResponse {
    pub id: u32,
    pub role_id: u32,
    pub target_id: u32,
    /// `Inherit`, `None`, `View` or `Edit`.
    #[schema(example = "View")]
    pub permission: String,
}

impl RolePermissionResponse {
    pub fn new(id: u32, role_id: u32, target_id: u32, permission: &Permission) -> Self {
        let permission = match permission {
            Permission::Inherit => "Inherit",
            Permission::None => "None",
            Permission::View => "View",
            Permission::Edit => "Edit",
        };
        Self {
            id,
            role_id,
            target_id,
            permission: permission.to_string(),
        }
    }
}
