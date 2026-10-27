//! Role-based access control (RBAC) enforcement.
//!
//! ZoneMinder's `Users` table carries per-feature permission columns
//! (`Stream`, `Events`, `Control`, `Monitors`, `Groups`, `Devices`,
//! `Snapshots`, `System`), each `None` / `View` / `Edit`. Authentication
//! alone only proves *who* the caller is — this module enforces *what* they
//! are allowed to do.
//!
//! Permissions are embedded in the access token at login time (see
//! [`crate::service::token`]) and carried in [`UserClaims::perms`]. The
//! [`protect`] middleware is self-contained: it decodes the bearer token
//! itself, so it does not depend on any other middleware running first and
//! can be applied to a router from a single place.

use axum::{
    extract::Request,
    http::Method,
    middleware::{from_fn, Next},
    response::Response,
    Router,
};
use fake::Dummy;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::entity::sea_orm_active_enums as enums;
use crate::entity::users::Model as UserModel;
use crate::error::AppError;
use crate::server::state::AppState;
use crate::util::claim::UserClaims;
use crate::util::middleware::{extract_token_from_header, extract_token_from_query};

/// A permission level, ordered `None < View < Edit < Create`.
///
/// `Create` only exists for `Monitors`: ZoneMinder lets `Edit` change
/// existing monitors but only `Create` add new ones.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Default,
    Serialize,
    Deserialize,
    Dummy,
    ToSchema,
)]
pub enum Level {
    /// No access.
    #[default]
    None,
    /// Read-only access.
    View,
    /// Read-write access.
    Edit,
    /// Read-write access plus adding new items (`Monitors` only).
    Create,
}

/// A snapshot of a user's per-feature permission levels, embedded in the JWT.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Default,
    Serialize,
    Deserialize,
    Dummy,
    ToSchema,
)]
pub struct UserPermissions {
    pub stream: Level,
    pub events: Level,
    pub control: Level,
    pub monitors: Level,
    pub groups: Level,
    pub devices: Level,
    pub snapshots: Level,
    pub system: Level,
}

impl UserPermissions {
    /// A permission set granting full `Edit` access to every feature.
    /// Intended for tests and tooling, never for real token issuance.
    pub fn superuser() -> Self {
        Self {
            stream: Level::View, // `Stream` has no `Edit` level in ZoneMinder
            events: Level::Edit,
            control: Level::Edit,
            monitors: Level::Create,
            groups: Level::Edit,
            devices: Level::Edit,
            snapshots: Level::Edit,
            system: Level::Edit,
        }
    }

    /// The caller's level for a given feature.
    pub fn level(&self, feature: Feature) -> Level {
        match feature {
            Feature::Stream => self.stream,
            Feature::Events => self.events,
            Feature::Control => self.control,
            Feature::Monitors => self.monitors,
            Feature::Groups => self.groups,
            Feature::Devices => self.devices,
            Feature::Snapshots => self.snapshots,
            Feature::System => self.system,
        }
    }
}

/// The levels of one permission row: a user's own, or a role's (the two
/// tables share their permission columns).
#[allow(clippy::too_many_arguments)]
fn levels(
    stream: &enums::Stream,
    events: &enums::Events,
    control: &enums::Control,
    monitors: &enums::Monitors,
    groups: &enums::Groups,
    devices: &enums::Devices,
    snapshots: &enums::Snapshots,
    system: &enums::System,
) -> UserPermissions {
    UserPermissions {
        stream: match stream {
            enums::Stream::None => Level::None,
            enums::Stream::View => Level::View,
        },
        events: from_nve(events),
        control: match control {
            enums::Control::None => Level::None,
            enums::Control::View => Level::View,
            enums::Control::Edit => Level::Edit,
        },
        monitors: match monitors {
            enums::Monitors::None => Level::None,
            enums::Monitors::View => Level::View,
            enums::Monitors::Edit => Level::Edit,
            enums::Monitors::Create => Level::Create,
        },
        groups: match groups {
            enums::Groups::None => Level::None,
            enums::Groups::View => Level::View,
            enums::Groups::Edit => Level::Edit,
        },
        devices: match devices {
            enums::Devices::None => Level::None,
            enums::Devices::View => Level::View,
            enums::Devices::Edit => Level::Edit,
        },
        snapshots: match snapshots {
            enums::Snapshots::None => Level::None,
            enums::Snapshots::View => Level::View,
            enums::Snapshots::Edit => Level::Edit,
        },
        system: match system {
            enums::System::None => Level::None,
            enums::System::View => Level::View,
            enums::System::Edit => Level::Edit,
        },
    }
}

/// The user's own permission columns, ignoring any role. For what a user
/// may actually do, see [`effective_permissions`].
impl From<&UserModel> for UserPermissions {
    fn from(u: &UserModel) -> Self {
        levels(
            &u.stream,
            &u.events,
            &u.control,
            &u.monitors,
            &u.groups,
            &u.devices,
            &u.snapshots,
            &u.system,
        )
    }
}

impl From<&crate::entity::user_roles::Model> for UserPermissions {
    fn from(r: &crate::entity::user_roles::Model) -> Self {
        levels(
            &r.stream,
            &r.events,
            &r.control,
            &r.monitors,
            &r.groups,
            &r.devices,
            &r.snapshots,
            &r.system,
        )
    }
}

impl UserPermissions {
    /// Per feature, the higher of the two: how ZoneMinder 1.39 combines a
    /// user's own levels with their role's (`User::permissionLevels`).
    pub fn max(self, other: Self) -> Self {
        Self {
            stream: self.stream.max(other.stream),
            events: self.events.max(other.events),
            control: self.control.max(other.control),
            monitors: self.monitors.max(other.monitors),
            groups: self.groups.max(other.groups),
            devices: self.devices.max(other.devices),
            snapshots: self.snapshots.max(other.snapshots),
            system: self.system.max(other.system),
        }
    }

    /// Whether these permissions may add new monitors. ZoneMinder's API
    /// allows `Monitors: Create` or `System: Edit`.
    pub fn can_create_monitors(&self) -> bool {
        self.monitors >= Level::Create || self.system >= Level::Edit
    }
}

/// What `user` may do: their own levels raised by their role's, if they
/// have one (#143). A role that no longer exists adds nothing.
pub async fn effective_permissions(
    db: &sea_orm::DatabaseConnection,
    user: &UserModel,
) -> crate::error::AppResult<UserPermissions> {
    use sea_orm::EntityTrait;
    let own = UserPermissions::from(user);
    let Some(role_id) = user.role_id else {
        return Ok(own);
    };
    let role = crate::entity::user_roles::Entity::find_by_id(role_id)
        .one(db)
        .await?;
    Ok(match role {
        Some(r) => own.max(UserPermissions::from(&r)),
        None => own,
    })
}

fn from_nve(e: &enums::Events) -> Level {
    match e {
        enums::Events::None => Level::None,
        enums::Events::View => Level::View,
        enums::Events::Edit => Level::Edit,
    }
}

/// A ZoneMinder permission feature/category.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Feature {
    Stream,
    Events,
    Control,
    Monitors,
    Groups,
    Devices,
    Snapshots,
    System,
}

/// The permission level a request needs, derived from the HTTP method:
/// safe (read) methods need `View`, mutating methods need `Edit`.
///
/// `Stream` has no `Edit` level in ZoneMinder, so it always requires `View`.
pub(crate) fn required_level(feature: Feature, method: &Method) -> Level {
    if feature == Feature::Stream {
        return Level::View;
    }
    match *method {
        Method::GET | Method::HEAD | Method::OPTIONS => Level::View,
        _ => Level::Edit,
    }
}

/// The caller's claims from the bearer token (header, or `?token=` for media
/// elements), refused if the token is missing, invalid or revoked.
/// Self-contained, so it doesn't depend on auth middleware ordering.
pub(crate) fn authenticate(state: &AppState, request: &Request) -> Result<UserClaims, AppError> {
    let token = extract_token_from_header(request)
        .or_else(|| extract_token_from_query(request))
        .ok_or_else(|| AppError::UnauthorizedError("Authentication required".to_string()))?;

    let claims = UserClaims::decode_access(&token)
        .map_err(|_| AppError::UnauthorizedError("Invalid token".to_string()))?;

    // Server-side revocation floor (logout, password change, user disable):
    // an in-memory check, so the media hot path stays free of DB round-trips.
    if state.revocations.is_revoked(claims.uid, claims.iat) {
        return Err(AppError::UnauthorizedError(
            "Token has been revoked".to_string(),
        ));
    }
    Ok(claims)
}

async fn enforce(
    state: AppState,
    feature: Feature,
    level: Option<Level>,
    request: Request,
    next: Next,
) -> Result<Response, AppError> {
    // A signed media URL was checked by `media_grant_layer`, including the
    // feature this resource's media belongs to and the user's current access.
    if request
        .extensions()
        .get::<crate::service::media_grant::MediaGrant>()
        .is_some()
    {
        return Ok(next.run(request).await);
    }

    let claims = authenticate(&state, &request)?;
    let required = level.unwrap_or_else(|| required_level(feature, request.method()));
    let granted = claims.perms.level(feature);

    if granted >= required {
        // Handlers behind `protect` alone (the live WebSocket) read who the
        // caller is from here.
        let mut request = request;
        request.extensions_mut().insert(claims);
        Ok(next.run(request).await)
    } else {
        Err(AppError::PermissionDeniedError(format!(
            "{:?} access to {:?} required",
            required, feature
        )))
    }
}

/// Wrap a router so every request is checked against the given [`Feature`].
///
/// The required level is method-derived (read → `View`, write → `Edit`).
/// Apply this to routers whose endpoints all belong to one feature. The
/// state is needed for the token-revocation check.
pub fn protect(router: Router<AppState>, feature: Feature, state: AppState) -> Router<AppState> {
    router.layer(from_fn(move |req: Request, next: Next| {
        enforce(state.clone(), feature, None, req, next)
    }))
}

/// Like [`protect`], but requiring `level` whatever the method: for a POST
/// that only reads, such as minting a read-only media link.
pub fn protect_at(
    router: Router<AppState>,
    feature: Feature,
    level: Level,
    state: AppState,
) -> Router<AppState> {
    router.layer(from_fn(move |req: Request, next: Next| {
        enforce(state.clone(), feature, Some(level), req, next)
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn level_ordering() {
        assert!(Level::None < Level::View);
        assert!(Level::View < Level::Edit);
    }

    #[test]
    fn required_level_is_method_aware() {
        assert_eq!(required_level(Feature::Monitors, &Method::GET), Level::View);
        assert_eq!(
            required_level(Feature::Monitors, &Method::POST),
            Level::Edit
        );
        assert_eq!(
            required_level(Feature::Monitors, &Method::DELETE),
            Level::Edit
        );
        // Stream never requires Edit.
        assert_eq!(required_level(Feature::Stream, &Method::POST), Level::View);
    }

    #[test]
    fn create_outranks_edit() {
        assert!(Level::Edit < Level::Create);
    }

    #[test]
    fn role_levels_raise_but_never_lower_the_users() {
        let user = UserPermissions {
            events: Level::Edit,
            monitors: Level::None,
            ..Default::default()
        };
        let role = UserPermissions {
            events: Level::View,
            monitors: Level::View,
            ..Default::default()
        };
        let merged = user.max(role);
        assert_eq!(merged.events, Level::Edit);
        assert_eq!(merged.monitors, Level::View);
        assert_eq!(merged.system, Level::None);
    }

    #[test]
    fn only_create_or_system_edit_may_add_monitors() {
        let edit = UserPermissions {
            monitors: Level::Edit,
            ..Default::default()
        };
        assert!(!edit.can_create_monitors());
        let create = UserPermissions {
            monitors: Level::Create,
            ..Default::default()
        };
        assert!(create.can_create_monitors());
        let admin = UserPermissions {
            system: Level::Edit,
            ..Default::default()
        };
        assert!(admin.can_create_monitors());
    }

    #[test]
    fn superuser_grants_every_feature() {
        let p = UserPermissions::superuser();
        assert_eq!(p.level(Feature::Monitors), Level::Create);
        for f in [
            Feature::Events,
            Feature::Control,
            Feature::Groups,
            Feature::Devices,
            Feature::Snapshots,
            Feature::System,
        ] {
            assert_eq!(p.level(f), Level::Edit);
        }
        assert_eq!(p.level(Feature::Stream), Level::View);
    }

    #[test]
    fn a_role_raises_levels_and_never_lowers_them() {
        let own = UserPermissions {
            events: Level::Edit,
            ..Default::default()
        };
        let role = UserPermissions {
            stream: Level::View,
            events: Level::View,
            system: Level::Edit,
            ..Default::default()
        };
        let eff = own.max(role);
        assert_eq!(eff.stream, Level::View);
        assert_eq!(eff.events, Level::Edit);
        assert_eq!(eff.system, Level::Edit);
        assert_eq!(eff.control, Level::None);
    }

    #[test]
    fn default_permissions_grant_nothing() {
        let p = UserPermissions::default();
        assert_eq!(p.level(Feature::Monitors), Level::None);
        assert_eq!(p.level(Feature::System), Level::None);
    }
}
