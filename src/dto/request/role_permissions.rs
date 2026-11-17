use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// The permission to give a role on one monitor or group.
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct SetRolePermissionRequest {
    /// `Inherit`, `None`, `View` or `Edit`.
    #[schema(example = "View")]
    pub permission: String,
}
