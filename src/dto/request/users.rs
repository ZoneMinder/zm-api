use garde::Validate;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::entity::sea_orm_active_enums::{
    Control, Devices, Events, Groups, Monitors, Snapshots, Stream, System,
};
use crate::error::{AppError, AppResult};

/// The eight ZoneMinder per-feature permission levels. Each is optional so a
/// request can set only the ones it cares about.
///
/// Values are the same names `UserResponse` reports: `None` / `View` / `Edit`
/// (`Stream` has no `Edit`; `Monitors` also accepts `Create`). They are taken
/// as strings and parsed, because the generated entity enums do not derive
/// `ToSchema` and `src/entity/` is a generated artifact we don't hand-edit.
#[derive(Debug, Default, Serialize, Deserialize, ToSchema)]
pub struct UserPermissionsInput {
    #[schema(example = "View")]
    pub stream: Option<String>,
    #[schema(example = "Edit")]
    pub events: Option<String>,
    pub control: Option<String>,
    pub monitors: Option<String>,
    pub groups: Option<String>,
    pub devices: Option<String>,
    pub snapshots: Option<String>,
    pub system: Option<String>,
}

fn bad(field: &str, value: &str, allowed: &str) -> AppError {
    AppError::BadRequestError(format!(
        "invalid {field} permission {value:?}; expected one of: {allowed}"
    ))
}

impl UserPermissionsInput {
    /// Whether any level is set.
    pub fn changes_any(&self) -> bool {
        [
            &self.stream,
            &self.events,
            &self.control,
            &self.monitors,
            &self.groups,
            &self.devices,
            &self.snapshots,
            &self.system,
        ]
        .iter()
        .any(|level| level.is_some())
    }

    pub fn stream_level(&self) -> AppResult<Option<Stream>> {
        self.stream
            .as_deref()
            .map(|v| match v {
                "None" => Ok(Stream::None),
                "View" => Ok(Stream::View),
                _ => Err(bad("stream", v, "None, View")),
            })
            .transpose()
    }

    pub fn events_level(&self) -> AppResult<Option<Events>> {
        self.events
            .as_deref()
            .map(|v| match v {
                "None" => Ok(Events::None),
                "View" => Ok(Events::View),
                "Edit" => Ok(Events::Edit),
                _ => Err(bad("events", v, "None, View, Edit")),
            })
            .transpose()
    }

    pub fn control_level(&self) -> AppResult<Option<Control>> {
        self.control
            .as_deref()
            .map(|v| match v {
                "None" => Ok(Control::None),
                "View" => Ok(Control::View),
                "Edit" => Ok(Control::Edit),
                _ => Err(bad("control", v, "None, View, Edit")),
            })
            .transpose()
    }

    pub fn monitors_level(&self) -> AppResult<Option<Monitors>> {
        self.monitors
            .as_deref()
            .map(|v| match v {
                "None" => Ok(Monitors::None),
                "View" => Ok(Monitors::View),
                "Edit" => Ok(Monitors::Edit),
                "Create" => Ok(Monitors::Create),
                _ => Err(bad("monitors", v, "None, View, Edit, Create")),
            })
            .transpose()
    }

    pub fn groups_level(&self) -> AppResult<Option<Groups>> {
        self.groups
            .as_deref()
            .map(|v| match v {
                "None" => Ok(Groups::None),
                "View" => Ok(Groups::View),
                "Edit" => Ok(Groups::Edit),
                _ => Err(bad("groups", v, "None, View, Edit")),
            })
            .transpose()
    }

    pub fn devices_level(&self) -> AppResult<Option<Devices>> {
        self.devices
            .as_deref()
            .map(|v| match v {
                "None" => Ok(Devices::None),
                "View" => Ok(Devices::View),
                "Edit" => Ok(Devices::Edit),
                _ => Err(bad("devices", v, "None, View, Edit")),
            })
            .transpose()
    }

    pub fn snapshots_level(&self) -> AppResult<Option<Snapshots>> {
        self.snapshots
            .as_deref()
            .map(|v| match v {
                "None" => Ok(Snapshots::None),
                "View" => Ok(Snapshots::View),
                "Edit" => Ok(Snapshots::Edit),
                _ => Err(bad("snapshots", v, "None, View, Edit")),
            })
            .transpose()
    }

    pub fn system_level(&self) -> AppResult<Option<System>> {
        self.system
            .as_deref()
            .map(|v| match v {
                "None" => Ok(System::None),
                "View" => Ok(System::View),
                "Edit" => Ok(System::Edit),
                _ => Err(bad("system", v, "None, View, Edit")),
            })
            .transpose()
    }
}

#[derive(Debug, Default, Serialize, Deserialize, ToSchema, Validate)]
pub struct CreateUserRequest {
    #[garde(length(chars, max = 64))]
    pub username: String,
    #[garde(skip)]
    pub password: String,
    #[garde(length(chars, max = 64))]
    pub email: String,
    #[garde(inner(length(chars, max = 64)))]
    pub name: Option<String>,
    #[garde(inner(length(chars, max = 64)))]
    pub phone: Option<String>,
    #[garde(skip)]
    pub enabled: Option<u8>,
    #[garde(inner(length(chars, max = 8)))]
    pub language: Option<String>,
    #[garde(inner(length(chars, max = 64)))]
    pub home_view: Option<String>,
    /// Whether the user may authenticate to the API (`APIEnabled`).
    #[garde(skip)]
    pub api_enabled: Option<u8>,
    #[garde(inner(length(chars, max = 16)))]
    pub max_bandwidth: Option<String>,
    /// The user's role (`GET /api/v3/roles`); its permissions add to the
    /// user's own.
    #[garde(skip)]
    pub role_id: Option<u32>,
    /// Per-feature permission levels. Omitted features default to `View`.
    #[serde(flatten)]
    #[garde(skip)]
    pub permissions: UserPermissionsInput,
}

/// Partial update: every field is optional and only provided fields change.
#[derive(Debug, Default, Serialize, Deserialize, ToSchema, Validate)]
pub struct UpdateUserRequest {
    #[garde(inner(length(chars, max = 64)))]
    pub email: Option<String>,
    #[garde(skip)]
    pub enabled: Option<u8>,
    /// New password (re-hashed with bcrypt; never stored in plaintext).
    #[garde(skip)]
    pub password: Option<String>,
    #[garde(inner(length(chars, max = 64)))]
    pub name: Option<String>,
    #[garde(inner(length(chars, max = 64)))]
    pub phone: Option<String>,
    #[garde(inner(length(chars, max = 8)))]
    pub language: Option<String>,
    #[garde(inner(length(chars, max = 64)))]
    pub home_view: Option<String>,
    #[garde(skip)]
    pub api_enabled: Option<u8>,
    #[garde(inner(length(chars, max = 16)))]
    pub max_bandwidth: Option<String>,
    /// Set the token-revocation floor (unix seconds). Setting it to "now"
    /// revokes all of the user's outstanding tokens (admin revoke-all).
    #[garde(skip)]
    pub token_min_expiry: Option<u64>,
    /// The user's role; `null` removes it. Omit to leave it unchanged.
    #[serde(default, deserialize_with = "crate::dto::serde_helpers::double_option")]
    #[schema(value_type = Option<u32>)]
    #[garde(skip)]
    pub role_id: Option<Option<u32>>,
    #[serde(flatten)]
    #[garde(skip)]
    pub permissions: UserPermissionsInput,
}

impl UpdateUserRequest {
    /// Whether this update changes what the user may do or whether they may
    /// sign in at all. Their existing tokens carry the old permissions, so
    /// such a change must revoke them.
    pub fn changes_access(&self) -> bool {
        let p = &self.permissions;
        self.password.as_deref().is_some_and(|pw| !pw.is_empty())
            || self.enabled.is_some()
            || self.api_enabled.is_some()
            // A role's levels are in the token too (#143).
            || self.role_id.is_some()
            || p.changes_any()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn permission_levels_parse_from_names() {
        let p = UserPermissionsInput {
            stream: Some("View".into()),
            events: Some("Edit".into()),
            monitors: Some("Create".into()),
            system: Some("None".into()),
            ..Default::default()
        };
        assert_eq!(p.stream_level().unwrap(), Some(Stream::View));
        assert_eq!(p.events_level().unwrap(), Some(Events::Edit));
        assert_eq!(p.monitors_level().unwrap(), Some(Monitors::Create));
        assert_eq!(p.system_level().unwrap(), Some(System::None));
        // Unset fields stay None (no change).
        assert_eq!(p.groups_level().unwrap(), None);
    }

    #[test]
    fn invalid_permission_value_is_a_bad_request() {
        let p = UserPermissionsInput {
            events: Some("Superuser".into()),
            ..Default::default()
        };
        let err = p
            .events_level()
            .expect_err("invalid level must be rejected");
        assert!(matches!(err, AppError::BadRequestError(_)), "got {err:?}");
    }

    /// `Stream` has no Edit level in ZoneMinder; rejecting it here keeps the
    /// API from silently storing a level the schema can't express.
    #[test]
    fn stream_rejects_edit() {
        let p = UserPermissionsInput {
            stream: Some("Edit".into()),
            ..Default::default()
        };
        assert!(p.stream_level().is_err());
    }

    /// The permission block is flattened, so it deserializes from top-level
    /// keys alongside the profile fields.
    #[test]
    fn update_request_deserializes_flattened_permissions() {
        let req: UpdateUserRequest = serde_json::from_value(serde_json::json!({
            "email": "a@b.com",
            "monitors": "Edit",
            "system": "None"
        }))
        .unwrap();
        assert_eq!(req.email.as_deref(), Some("a@b.com"));
        assert_eq!(
            req.permissions.monitors_level().unwrap(),
            Some(Monitors::Edit)
        );
        assert_eq!(req.permissions.system_level().unwrap(), Some(System::None));
    }
}
