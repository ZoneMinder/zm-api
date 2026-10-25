//! Row-level group access control.
//!
//! ZoneMinder's `Groups_Permissions` table records, per user, which *groups*
//! they may see and at what level (`None` / `View` / `Edit`). Feature-level
//! RBAC ([`crate::util::authz`]) decides whether a user may touch the groups
//! API at all; this module decides *which groups* within it.
//!
//! [`resolve_groups`] turns those rows into a [`GroupScope`]. Policy mirrors
//! [`crate::service::monitor_acl`]:
//!
//! * A user with **no** `Groups_Permissions` rows, and whose role has no
//!   `Role_Groups_Permissions` rows, is unrestricted ([`GroupScope::All`]) —
//!   default-allow, so deployments that never populate the tables are
//!   unaffected.
//! * Once there is any row, the rows become an allowlist
//!   ([`GroupScope::Restricted`]): only groups with an effective level of
//!   `View` or higher are visible. `Inherit`/`None` rows grant nothing.

use std::collections::HashMap;

use axum::extract::FromRequestParts;
use axum::http::header::AUTHORIZATION;
use axum::http::request::Parts;
use axum::http::{HeaderMap, Uri};
use sea_orm::DatabaseConnection;

use crate::entity::sea_orm_active_enums::Permission;
use crate::error::{AppError, AppResult};
use crate::repo;
use crate::server::state::AppState;
use crate::util::authz::Level;
use crate::util::claim::UserClaims;

/// The set of groups a user may access, with each group's effective level.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GroupScope {
    /// Unrestricted — the user has no `Groups_Permissions` rows (default-allow).
    All,
    /// Restricted to an explicit allowlist of `group_id -> effective level`.
    Restricted(HashMap<u32, Level>),
}

impl GroupScope {
    /// Whether the caller may access `group_id` at `required` level or above.
    pub fn allows(&self, group_id: u32, required: Level) -> bool {
        match self {
            GroupScope::All => true,
            GroupScope::Restricted(map) => {
                map.get(&group_id).is_some_and(|level| *level >= required)
            }
        }
    }

    /// The group ids visible at `required` level, or `None` when the scope is
    /// unrestricted ([`GroupScope::All`]).
    ///
    /// `Some(vec![])` is a meaningful result: a restricted user who may see no
    /// groups at all.
    pub fn visible_ids(&self, required: Level) -> Option<Vec<u32>> {
        match self {
            GroupScope::All => None,
            GroupScope::Restricted(map) => Some(
                map.iter()
                    .filter(|(_, level)| **level >= required)
                    .map(|(id, _)| *id)
                    .collect(),
            ),
        }
    }

    /// Whether this scope restricts access at all.
    pub fn is_restricted(&self) -> bool {
        matches!(self, GroupScope::Restricted(_))
    }
}

/// Resolve the group access scope for a user from their `Groups_Permissions`
/// rows and their role's `Role_Groups_Permissions` rows. As for monitors, the
/// user's own row decides a group first and the role's second; `Inherit`
/// leaves it to the role.
pub async fn resolve_groups(db: &DatabaseConnection, user_id: u32) -> AppResult<GroupScope> {
    use crate::repo::monitors_permissions::GrantKind;
    let grants: Vec<_> = repo::monitors_permissions::find_user_grants(db, user_id)
        .await?
        .into_iter()
        .filter(|g| matches!(g.kind(), GrantKind::Group | GrantKind::RoleGroup))
        .collect();

    // Default-allow: no rows from the user or their role means unrestricted.
    if grants.is_empty() {
        return Ok(GroupScope::All);
    }

    let level = |p: &Permission| match p {
        Permission::Inherit => None,
        Permission::None => Some(Level::None),
        Permission::View => Some(Level::View),
        Permission::Edit => Some(Level::Edit),
    };
    let mut own: HashMap<u32, Level> = HashMap::new();
    let mut role: HashMap<u32, Level> = HashMap::new();
    for g in &grants {
        let Some(l) = level(&g.permission) else {
            continue;
        };
        let layer = if g.kind() == GrantKind::Group {
            &mut own
        } else {
            &mut role
        };
        let entry = layer.entry(g.target_id).or_insert(l);
        *entry = (*entry).max(l);
    }

    // `None` grants nothing: the group stays out of the map, which `allows`
    // treats as denied.
    let mut map: HashMap<u32, Level> = HashMap::new();
    for id in own.keys().chain(role.keys()) {
        if let Some(&l) = own.get(id).or_else(|| role.get(id)) {
            if l > Level::None {
                map.insert(*id, l);
            }
        }
    }
    Ok(GroupScope::Restricted(map))
}

/// Extract a bearer token — `Authorization: Bearer <jwt>` header, or the
/// `?token=` query parameter.
fn extract_token(headers: &HeaderMap, uri: &Uri) -> Option<String> {
    let from_header = headers
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .map(|token| token.trim().to_string());

    from_header.or_else(|| {
        uri.query().and_then(|query| {
            query.split('&').find_map(|pair| {
                let (key, value) = pair.split_once('=')?;
                (key == "token").then(|| value.to_string())
            })
        })
    })
}

/// Resolve the caller's group scope from a token in the given headers/URI.
async fn resolve_from_request(
    state: &AppState,
    headers: &HeaderMap,
    uri: &Uri,
) -> AppResult<GroupScope> {
    let token = extract_token(headers, uri)
        .ok_or_else(|| AppError::UnauthorizedError("Authentication required".to_string()))?;
    let claims = UserClaims::decode_access(&token)
        .map_err(|_| AppError::UnauthorizedError("Invalid token".to_string()))?;
    resolve_groups(state.db(), claims.uid).await
}

/// Axum extractor that resolves the caller's [`GroupScope`].
///
/// Self-contained — it decodes the access token itself, so it does not depend
/// on auth-middleware ordering. The scope is resolved live from the database,
/// so group-permission changes take effect immediately.
impl FromRequestParts<AppState> for GroupScope {
    type Rejection = AppError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        resolve_from_request(state, &parts.headers, &parts.uri).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repo::monitors_permissions::GrantKind;
    use sea_orm::{DatabaseBackend, MockDatabase, Value};
    use std::collections::BTreeMap;

    /// A row of `find_user_grants`, as the mock database returns it.
    fn grant(kind: GrantKind, group_id: u32, permission: &str) -> BTreeMap<String, Value> {
        BTreeMap::from([
            ("kind".to_string(), Value::Int(Some(kind as i32))),
            ("target_id".to_string(), Value::Unsigned(Some(group_id))),
            (
                "permission".to_string(),
                Value::String(Some(permission.to_string())),
            ),
        ])
    }

    async fn scope_of(rows: Vec<BTreeMap<String, Value>>) -> GroupScope {
        let db = MockDatabase::new(DatabaseBackend::MySql)
            .append_query_results(vec![rows])
            .into_connection();
        resolve_groups(&db, 7).await.unwrap()
    }

    #[tokio::test]
    async fn no_rows_is_unrestricted() {
        assert_eq!(scope_of(vec![]).await, GroupScope::All);
    }

    #[tokio::test]
    async fn grants_form_an_allowlist() {
        let scope = scope_of(vec![
            grant(GrantKind::Group, 10, "View"),
            grant(GrantKind::Group, 11, "Edit"),
        ])
        .await;
        assert!(scope.is_restricted());
        // Group 10: read but not write.
        assert!(scope.allows(10, Level::View));
        assert!(!scope.allows(10, Level::Edit));
        // Group 11: read and write.
        assert!(scope.allows(11, Level::Edit));
        // Group 99: no row at all.
        assert!(!scope.allows(99, Level::View));
    }

    #[tokio::test]
    async fn inherit_and_none_grant_nothing() {
        let scope = scope_of(vec![
            grant(GrantKind::Group, 10, "Inherit"),
            grant(GrantKind::Group, 11, "None"),
        ])
        .await;
        // The user has rows, so the scope is restricted...
        assert!(scope.is_restricted());
        // ...but neither group is visible.
        assert!(!scope.allows(10, Level::View));
        assert!(!scope.allows(11, Level::View));
    }

    #[tokio::test]
    async fn highest_level_wins_for_duplicate_rows() {
        let scope = scope_of(vec![
            grant(GrantKind::Group, 10, "View"),
            grant(GrantKind::Group, 10, "Edit"),
        ])
        .await;
        assert!(scope.allows(10, Level::Edit));
    }

    #[tokio::test]
    async fn role_rows_restrict_and_the_users_own_row_wins() {
        let scope = scope_of(vec![
            grant(GrantKind::RoleGroup, 10, "Edit"),
            grant(GrantKind::RoleGroup, 11, "Edit"),
            grant(GrantKind::RoleGroup, 12, "View"),
            grant(GrantKind::Group, 11, "None"),
            grant(GrantKind::Group, 12, "Inherit"),
        ])
        .await;
        // Role rows alone make the scope restricted.
        assert!(scope.is_restricted());
        assert!(scope.allows(10, Level::Edit));
        // The user's own None overrides the role's Edit.
        assert!(!scope.allows(11, Level::View));
        // Inherit leaves it to the role.
        assert!(scope.allows(12, Level::View));
        assert!(!scope.allows(12, Level::Edit));
        assert!(!scope.allows(99, Level::View));
    }

    #[test]
    fn visible_ids_filters_by_required_level() {
        let mut map = HashMap::new();
        map.insert(10, Level::View);
        map.insert(11, Level::Edit);
        let scope = GroupScope::Restricted(map);

        let mut view = scope.visible_ids(Level::View).unwrap();
        view.sort_unstable();
        assert_eq!(view, vec![10, 11]);

        let edit = scope.visible_ids(Level::Edit).unwrap();
        assert_eq!(edit, vec![11]);

        assert_eq!(GroupScope::All.visible_ids(Level::View), None);
    }
}
