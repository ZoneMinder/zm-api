//! Row-level monitor access control.
//!
//! ZoneMinder restricts *which monitors* a user may see via two tables:
//! `Monitors_Permissions` (direct per-monitor grants) and `Groups_Permissions`
//! + `Groups_Monitors` (per-group grants the monitor inherits).
//!
//! [`resolve`] turns those rows into a [`MonitorScope`] for a user, following
//! ZoneMinder's own order (`visibleMonitor` in `web/includes/auth.php`):
//!
//! 1. the user's direct `Monitors_Permissions` row, unless it is `Inherit`;
//! 2. the user's group grants: a `None` group hides its monitors, otherwise
//!    the best `View`/`Edit` wins;
//! 3. the same two steps for the user's role (`Users.RoleId`).
//!
//! A group grant covers the monitors of its child groups too.
//!
//! * A user with **no** rows anywhere gets their base `Monitors` level on every
//!   monitor: `Edit` is [`MonitorScope::All`], `View` is
//!   [`MonitorScope::ViewOnly`], `None` sees nothing.
//! * Once a user has any row, each monitor a row decides gets that row's
//!   level, and every other monitor gets the base `Monitors` level, as in
//!   ZoneMinder ([`MonitorScope::Restricted`] lists every monitor).

use std::collections::{HashMap, HashSet};

use axum::extract::{FromRequestParts, RawPathParams, Request, State};
use axum::http::header::AUTHORIZATION;
use axum::http::request::Parts;
use axum::http::{HeaderMap, Method, Uri};
use axum::middleware::Next;
use axum::response::Response;
use sea_orm::DatabaseConnection;

use crate::entity::sea_orm_active_enums::Permission;
use crate::error::{AppError, AppResult, Resource, ResourceType};
use crate::repo;
use crate::repo::monitors_permissions::GrantKind;
use crate::server::state::AppState;
use crate::util::authz::Level;
use crate::util::claim::UserClaims;

/// The set of monitors a user may access, with each monitor's effective level.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MonitorScope {
    /// Unrestricted: no permission rows and a base `Monitors` level of `Edit`.
    All,
    /// Every monitor, read-only: no permission rows and a base `Monitors`
    /// level of `View`.
    ViewOnly,
    /// Restricted to an explicit allowlist of `monitor_id -> effective level`.
    Restricted(HashMap<u32, Level>),
}

impl MonitorScope {
    /// Whether the caller may access `monitor_id` at `required` level or above.
    pub fn allows(&self, monitor_id: u32, required: Level) -> bool {
        match self {
            MonitorScope::All => true,
            MonitorScope::ViewOnly => required <= Level::View,
            MonitorScope::Restricted(map) => {
                map.get(&monitor_id).is_some_and(|level| *level >= required)
            }
        }
    }

    /// Like [`Self::allows`] for a resource whose monitor link is optional.
    /// A `None` monitor id (a row not tied to any monitor) is visible only to
    /// callers who may see every monitor at `required`.
    pub fn allows_opt(&self, monitor_id: Option<u32>, required: Level) -> bool {
        match monitor_id {
            Some(id) => self.allows(id, required),
            None => match self {
                MonitorScope::All => true,
                MonitorScope::ViewOnly => required <= Level::View,
                MonitorScope::Restricted(_) => false,
            },
        }
    }

    /// The monitor ids visible at `required` level, or `None` when the scope
    /// is unrestricted ([`MonitorScope::All`]).
    ///
    /// `Some(vec![])` is a meaningful result: a restricted user who may see no
    /// monitors at all.
    pub fn visible_ids(&self, required: Level) -> Option<Vec<u32>> {
        match self {
            MonitorScope::All => None,
            MonitorScope::ViewOnly if required <= Level::View => None,
            MonitorScope::ViewOnly => Some(Vec::new()),
            MonitorScope::Restricted(map) => Some(
                map.iter()
                    .filter(|(_, level)| **level >= required)
                    .map(|(id, _)| *id)
                    .collect(),
            ),
        }
    }

    /// Whether this scope restricts access at all. Only [`MonitorScope::All`]
    /// is unrestricted; callers that skip per-monitor checks must test this.
    pub fn is_restricted(&self) -> bool {
        !matches!(self, MonitorScope::All)
    }

    /// The scope of a user with no permission rows: their base `Monitors`
    /// level on every monitor.
    fn from_base(base: Level) -> Self {
        match base {
            Level::Edit | Level::Create => MonitorScope::All,
            Level::View => MonitorScope::ViewOnly,
            Level::None => MonitorScope::Restricted(HashMap::new()),
        }
    }
}

/// Resolve the monitor access scope for a user from the permission tables.
///
/// `base` is the user's effective `Monitors` level (their own or their
/// role's, whichever is higher), as carried in their access token.
pub async fn resolve(
    db: &DatabaseConnection,
    user_id: u32,
    base: Level,
) -> AppResult<MonitorScope> {
    let grants = repo::monitors_permissions::find_user_grants(db, user_id).await?;

    if grants.is_empty() {
        return Ok(MonitorScope::from_base(base));
    }

    let mut user_direct = Vec::new();
    let mut user_groups = Vec::new();
    let mut role_direct = Vec::new();
    let mut role_groups = Vec::new();
    for g in grants {
        let kind = g.kind();
        let entry = (g.target_id, g.permission);
        match kind {
            GrantKind::Monitor => user_direct.push(entry),
            GrantKind::Group => user_groups.push(entry),
            GrantKind::RoleMonitor => role_direct.push(entry),
            GrantKind::RoleGroup => role_groups.push(entry),
        }
    }

    let members = if user_groups.is_empty() && role_groups.is_empty() {
        HashMap::new()
    } else {
        let granted: Vec<u32> = user_groups
            .iter()
            .chain(&role_groups)
            .map(|(gid, _)| *gid)
            .collect();
        group_members(db, &granted).await?
    };

    let user = Layer::new(&user_direct, &user_groups, &members);
    let role = Layer::new(&role_direct, &role_groups, &members);

    // Every monitor a row mentions, plus every monitor when the base level
    // reaches them all.
    let mut ids: HashSet<u32> = user.monitors().chain(role.monitors()).collect();
    if base > Level::None {
        ids.extend(repo::monitors::find_all_ids(db).await?);
    }

    let map = ids
        .into_iter()
        .filter_map(|mid| {
            if !visible(mid, &user, &role, base) {
                return None;
            }
            let level = if editable(mid, &user, &role, base) {
                Level::Edit
            } else {
                Level::View
            };
            Some((mid, level))
        })
        .collect();

    Ok(MonitorScope::Restricted(map))
}

/// One owner's rows (the user's or their role's), per monitor.
struct Layer {
    /// Direct grants, `Inherit` left out.
    direct: HashMap<u32, Level>,
    /// Group grants per monitor: whether any group denies it, and the best
    /// `View`/`Edit` any group gives it.
    groups: HashMap<u32, (bool, Option<Level>)>,
}

impl Layer {
    fn new(
        direct: &[(u32, Permission)],
        groups: &[(u32, Permission)],
        members: &HashMap<u32, HashSet<u32>>,
    ) -> Self {
        let direct = direct
            .iter()
            .filter_map(|(mid, p)| permission_level(p).map(|level| (*mid, level)))
            .collect();
        let mut by_monitor: HashMap<u32, (bool, Option<Level>)> = HashMap::new();
        for (gid, p) in groups {
            let Some(level) = permission_level(p) else {
                continue;
            };
            for mid in members.get(gid).into_iter().flatten() {
                let entry = by_monitor.entry(*mid).or_default();
                if level == Level::None {
                    entry.0 = true;
                } else {
                    entry.1 = entry.1.max(Some(level));
                }
            }
        }
        Layer {
            direct,
            groups: by_monitor,
        }
    }

    fn monitors(&self) -> impl Iterator<Item = u32> + '_ {
        self.direct.keys().chain(self.groups.keys()).copied()
    }

    /// `Some(answer)` when this layer decides whether `mid` is visible.
    fn visible(&self, mid: u32) -> Option<bool> {
        if let Some(level) = self.direct.get(&mid) {
            return Some(*level > Level::None);
        }
        match self.groups.get(&mid) {
            Some((true, _)) => Some(false),
            Some((false, Some(_))) => Some(true),
            _ => None,
        }
    }

    /// `Some(answer)` when this layer decides whether `mid` is editable. A
    /// group only ever grants: a `None` or `View` group leaves it to the
    /// next layer.
    fn editable(&self, mid: u32) -> Option<bool> {
        if let Some(level) = self.direct.get(&mid) {
            return Some(*level >= Level::Edit);
        }
        match self.groups.get(&mid) {
            Some((_, Some(Level::Edit))) => Some(true),
            _ => None,
        }
    }
}

/// ZoneMinder's `visibleMonitor`: the user's rows, then the role's, then the
/// base `Monitors` level.
fn visible(mid: u32, user: &Layer, role: &Layer, base: Level) -> bool {
    user.visible(mid)
        .or_else(|| role.visible(mid))
        .unwrap_or(base > Level::None)
}

/// ZoneMinder's `editableMonitor`, the same order with its own rules.
fn editable(mid: u32, user: &Layer, role: &Layer, base: Level) -> bool {
    user.editable(mid)
        .or_else(|| role.editable(mid))
        .unwrap_or(base >= Level::Edit)
}

/// A grant row's level, or `None` for `Inherit` (no opinion).
fn permission_level(p: &Permission) -> Option<Level> {
    match p {
        Permission::Inherit => None,
        Permission::None => Some(Level::None),
        Permission::View => Some(Level::View),
        Permission::Edit => Some(Level::Edit),
    }
}

/// The monitors of each granted group, including those of its descendant
/// groups (ZoneMinder's `Group::MonitorIds`).
async fn group_members(
    db: &DatabaseConnection,
    granted: &[u32],
) -> AppResult<HashMap<u32, HashSet<u32>>> {
    let groups = repo::groups::find_all(db).await?;
    let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
    for g in &groups {
        if let Some(parent) = g.parent_id {
            children.entry(parent).or_default().push(g.id);
        }
    }

    // Each granted group with the set of groups under it (itself included).
    let mut subtree: HashMap<u32, HashSet<u32>> = HashMap::new();
    for &root in granted {
        let mut seen = HashSet::from([root]);
        let mut stack = vec![root];
        while let Some(gid) = stack.pop() {
            for &child in children.get(&gid).into_iter().flatten() {
                // `seen` also guards against a cycle in bad data.
                if seen.insert(child) {
                    stack.push(child);
                }
            }
        }
        subtree.insert(root, seen);
    }

    let all: Vec<u32> = subtree
        .values()
        .flatten()
        .copied()
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    let mut direct: HashMap<u32, Vec<u32>> = HashMap::new();
    for link in repo::groups_monitors::find_by_group_ids(db, &all).await? {
        direct
            .entry(link.group_id)
            .or_default()
            .push(link.monitor_id);
    }

    Ok(subtree
        .into_iter()
        .map(|(root, gids)| {
            let mids = gids
                .iter()
                .flat_map(|g| direct.get(g).into_iter().flatten().copied())
                .collect();
            (root, mids)
        })
        .collect())
}

/// Extract a bearer token — `Authorization: Bearer <jwt>` header, or the
/// `?token=` query parameter used by HTML media elements.
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

/// Resolve the caller's scope from a token in the given headers/URI.
async fn resolve_from_request(
    state: &AppState,
    headers: &HeaderMap,
    uri: &Uri,
) -> AppResult<MonitorScope> {
    let token = extract_token(headers, uri)
        .ok_or_else(|| AppError::UnauthorizedError("Authentication required".to_string()))?;
    let claims = UserClaims::decode_access(&token)
        .map_err(|_| AppError::UnauthorizedError("Invalid token".to_string()))?;
    resolve(state.db(), claims.uid, claims.perms.monitors).await
}

/// Axum extractor that resolves the caller's [`MonitorScope`].
///
/// Self-contained — it decodes the access token itself (header or query), so
/// it does not depend on auth-middleware ordering. The scope is resolved live
/// from the database, so monitor-permission changes take effect immediately.
impl FromRequestParts<AppState> for MonitorScope {
    type Rejection = AppError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        // A signed media URL sees one monitor, read-only.
        if let Some(grant) = parts
            .extensions
            .get::<crate::service::media_grant::MediaGrant>()
        {
            return Ok(grant.scope());
        }
        // Already resolved by `monitor_row_gate` for this request.
        if let Some(scope) = parts.extensions.get::<MonitorScope>() {
            return Ok(scope.clone());
        }
        resolve_from_request(state, &parts.headers, &parts.uri).await
    }
}

/// Route-layer guard for endpoints whose monitor id is a *path parameter*
/// (`{monitor_id}` or `{id}`), such as PTZ, where the monitor is named in the
/// URL rather than discovered from a query result. Reads need `View` on the
/// monitor, writes `Edit`.
///
/// Apply with `Router::route_layer` so it runs after routing (path params are
/// populated). Out-of-scope monitors yield 404.
pub async fn monitor_path_guard(
    State(state): State<AppState>,
    params: RawPathParams,
    request: Request,
    next: Next,
) -> Result<Response, AppError> {
    let required = match *request.method() {
        Method::GET | Method::HEAD | Method::OPTIONS => Level::View,
        _ => Level::Edit,
    };
    guard_path_monitor(&state, &params, request, next, required).await
}

/// [`monitor_path_guard`] for live streaming. Watching a camera is a read in
/// ZoneMinder (`Stream` has no `Edit` level), so starting or stopping a
/// session with `POST`/`DELETE` needs only `View` on the monitor.
pub async fn monitor_stream_guard(
    State(state): State<AppState>,
    params: RawPathParams,
    request: Request,
    next: Next,
) -> Result<Response, AppError> {
    guard_path_monitor(&state, &params, request, next, Level::View).await
}

/// The feature gate for `Monitors` routes, in place of `authz::protect`.
///
/// ZoneMinder's `canView('Monitors', $mid)` and `canEdit('Monitors', $mid)`
/// are the monitor's own rows alone (`visibleMonitor` / `editableMonitor`);
/// the base `Monitors` level only decides where no row does (#244). So when
/// the path names a monitor (`{id}` or `{monitor_id}`), that monitor's level
/// in the caller's [`MonitorScope`] decides: 404 if they can't see it, 403 if
/// they can see it but the method needs Edit. A route that isn't about one
/// monitor (the list, adding a monitor) is checked against the base level.
///
/// Apply with `Router::route_layer` so the path params are there.
pub async fn monitor_row_gate(
    State(state): State<AppState>,
    params: RawPathParams,
    request: Request,
    next: Next,
) -> Result<Response, AppError> {
    let mid = path_monitor_id(&params);
    gate_monitor(&state, mid, request, next).await
}

/// [`monitor_row_gate`] for `/zones/{id}`: a zone belongs to its monitor, so
/// the monitor's level decides, as `canEdit('Monitors', $mid)` does for
/// zones in ZoneMinder. An unknown zone falls back to the base level.
pub async fn zone_row_gate(
    State(state): State<AppState>,
    params: RawPathParams,
    request: Request,
    next: Next,
) -> Result<Response, AppError> {
    let mut mid = None;
    if let Some(zone_id) = path_monitor_id(&params) {
        mid = repo::zones::find_by_id(state.db(), zone_id)
            .await?
            .map(|zone| zone.monitor_id);
    }
    gate_monitor(&state, mid, request, next).await
}

fn path_monitor_id(params: &RawPathParams) -> Option<u32> {
    params
        .iter()
        .find(|(key, _)| *key == "monitor_id" || *key == "id")
        .and_then(|(_, value)| value.parse::<u32>().ok())
}

async fn gate_monitor(
    state: &AppState,
    mid: Option<u32>,
    mut request: Request,
    next: Next,
) -> Result<Response, AppError> {
    use crate::util::authz::{authenticate, required_level, Feature};
    let required = required_level(Feature::Monitors, request.method());

    // A signed media URL carries its own one-monitor, read-only scope.
    if let Some(grant) = request
        .extensions()
        .get::<crate::service::media_grant::MediaGrant>()
    {
        if let Some(mid) = mid {
            check_monitor(&grant.scope(), mid, required)?;
        }
        return Ok(next.run(request).await);
    }

    let claims = authenticate(state, &request)?;
    match mid {
        Some(mid) => {
            let scope = resolve(state.db(), claims.uid, claims.perms.monitors).await?;
            check_monitor(&scope, mid, required)?;
            // The handler's `MonitorScope` extractor reuses it.
            request.extensions_mut().insert(scope);
        }
        None if claims.perms.monitors < required => {
            return Err(AppError::PermissionDeniedError(format!(
                "{required:?} access to Monitors required"
            )));
        }
        None => {}
    }
    request.extensions_mut().insert(claims);
    Ok(next.run(request).await)
}

/// 404 for a monitor the caller can't see, 403 for one they see but may not
/// change.
fn check_monitor(scope: &MonitorScope, mid: u32, required: Level) -> AppResult<()> {
    if !scope.allows(mid, Level::View) {
        return Err(AppError::NotFoundError(Resource {
            details: vec![("monitor_id".to_string(), mid.to_string())],
            resource_type: ResourceType::Monitor,
        }));
    }
    if !scope.allows(mid, required) {
        return Err(AppError::PermissionDeniedError(format!(
            "{required:?} access to monitor {mid} required"
        )));
    }
    Ok(())
}

async fn guard_path_monitor(
    state: &AppState,
    params: &RawPathParams,
    request: Request,
    next: Next,
    required: Level,
) -> Result<Response, AppError> {
    let monitor_id = params
        .iter()
        .find(|(key, _)| *key == "monitor_id" || *key == "id")
        .and_then(|(_, value)| value.parse::<u32>().ok());

    if let Some(mid) = monitor_id {
        let scope = match request
            .extensions()
            .get::<crate::service::media_grant::MediaGrant>()
        {
            Some(grant) => grant.scope(),
            None => resolve_from_request(state, request.headers(), request.uri()).await?,
        };
        if !scope.allows(mid, required) {
            return Err(AppError::NotFoundError(Resource {
                details: vec![("monitor_id".to_string(), mid.to_string())],
                resource_type: ResourceType::Monitor,
            }));
        }
    }

    Ok(next.run(request).await)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entity::{groups, groups_monitors};
    use crate::repo::monitors_permissions::{GrantKind, UserGrant};
    use sea_orm::{DatabaseBackend, MockDatabase};

    impl sea_orm::IntoMockRow for UserGrant {
        fn into_mock_row(self) -> sea_orm::MockRow {
            let mut row = std::collections::BTreeMap::new();
            row.insert("kind", sea_orm::Value::from(self.kind));
            row.insert("target_id", sea_orm::Value::from(self.target_id));
            row.insert(
                "permission",
                sea_orm::ActiveEnum::to_value(&self.permission).into(),
            );
            row.into_mock_row()
        }
    }

    fn mon_perm(_id: u32, monitor_id: u32, permission: Permission) -> UserGrant {
        UserGrant {
            kind: GrantKind::Monitor as i32,
            target_id: monitor_id,
            permission,
        }
    }

    fn grp_perm(_id: u32, group_id: u32, permission: Permission) -> UserGrant {
        UserGrant {
            kind: GrantKind::Group as i32,
            target_id: group_id,
            permission,
        }
    }

    fn grp_mon(id: u32, group_id: u32, monitor_id: u32) -> groups_monitors::Model {
        groups_monitors::Model {
            id,
            group_id,
            monitor_id,
        }
    }

    fn role_mon(monitor_id: u32, permission: Permission) -> UserGrant {
        UserGrant {
            kind: GrantKind::RoleMonitor as i32,
            target_id: monitor_id,
            permission,
        }
    }

    fn role_grp(group_id: u32, permission: Permission) -> UserGrant {
        UserGrant {
            kind: GrantKind::RoleGroup as i32,
            target_id: group_id,
            permission,
        }
    }

    fn group(id: u32, parent_id: Option<u32>) -> groups::Model {
        groups::Model {
            id,
            name: format!("G{id}"),
            parent_id,
        }
    }

    /// Every monitor id the mock database holds.
    const MONITORS: [u32; 8] = [10, 11, 20, 21, 22, 30, 31, 99];

    /// A mock DB answering the grants query, then (when there are group grants)
    /// the group hierarchy and the group-monitor links, then (when there are
    /// grants) every monitor id.
    fn mock(
        grants: Vec<UserGrant>,
        groups: Vec<groups::Model>,
        links: Vec<groups_monitors::Model>,
    ) -> sea_orm::DatabaseConnection {
        let has_grants = !grants.is_empty();
        let has_groups = grants
            .iter()
            .any(|g| matches!(g.kind(), GrantKind::Group | GrantKind::RoleGroup));
        let mut db = MockDatabase::new(DatabaseBackend::MySql).append_query_results(vec![grants]);
        if has_groups {
            db = db
                .append_query_results(vec![groups])
                .append_query_results(vec![links]);
        }
        if has_grants {
            let ids: Vec<std::collections::BTreeMap<&str, sea_orm::Value>> = MONITORS
                .iter()
                .map(|id| std::collections::BTreeMap::from([("Id", sea_orm::Value::from(*id))]))
                .collect();
            db = db.append_query_results(vec![ids]);
        }
        db.into_connection()
    }

    #[tokio::test]
    async fn no_rows_and_monitors_edit_is_unrestricted() {
        let db = mock(vec![], vec![], vec![]);
        assert_eq!(
            resolve(&db, 7, Level::Edit).await.unwrap(),
            MonitorScope::All
        );
        // Default-allow costs a single round trip.
        assert_eq!(db.into_transaction_log().len(), 1);
    }

    #[tokio::test]
    async fn no_rows_and_monitors_view_is_read_only_everywhere() {
        let db = mock(vec![], vec![], vec![]);
        let scope = resolve(&db, 7, Level::View).await.unwrap();
        assert!(scope.allows(1, Level::View));
        assert!(scope.allows(99, Level::View));
        assert!(!scope.allows(1, Level::Edit));
        assert_eq!(scope.visible_ids(Level::View), None);
        assert_eq!(scope.visible_ids(Level::Edit), Some(vec![]));
        assert!(scope.is_restricted());
    }

    #[tokio::test]
    async fn no_rows_and_monitors_none_sees_nothing() {
        // ZoneMinder: a user with Monitors=None and no grants sees no monitor,
        // so none of their events or streams either.
        let db = mock(vec![], vec![], vec![]);
        let scope = resolve(&db, 7, Level::None).await.unwrap();
        assert!(!scope.allows(1, Level::View));
        assert_eq!(scope.visible_ids(Level::View), Some(vec![]));
        assert!(!scope.allows_opt(None, Level::View));
    }

    #[tokio::test]
    async fn direct_grants_decide_only_their_monitors() {
        let db = mock(
            vec![
                mon_perm(1, 10, Permission::View),
                mon_perm(2, 11, Permission::Edit),
            ],
            vec![],
            vec![],
        );

        let scope = resolve(&db, 7, Level::Edit).await.unwrap();
        assert!(scope.is_restricted());
        // Monitor 10: read but not write.
        assert!(scope.allows(10, Level::View));
        assert!(!scope.allows(10, Level::Edit));
        // Monitor 11: read and write.
        assert!(scope.allows(11, Level::Edit));
        // Monitor 99: no row, so the base Monitors level (#203).
        assert!(scope.allows(99, Level::Edit));
    }

    /// zm-holodeck's `scoped` user (#203): Monitors Edit and one row hiding
    /// monitor B. ZoneMinder shows every other monitor, editable.
    #[tokio::test]
    async fn one_denied_monitor_hides_only_that_monitor() {
        let db = mock(vec![mon_perm(1, 11, Permission::None)], vec![], vec![]);
        let scope = resolve(&db, 7, Level::Edit).await.unwrap();
        assert!(!scope.allows(11, Level::View));
        assert!(scope.allows(10, Level::Edit));
        let mut visible = scope.visible_ids(Level::View).unwrap();
        visible.sort_unstable();
        assert_eq!(visible, [10, 20, 21, 22, 30, 31, 99]);
    }

    #[tokio::test]
    async fn unlisted_monitors_follow_a_view_base_level() {
        let db = mock(vec![mon_perm(1, 11, Permission::Edit)], vec![], vec![]);
        let scope = resolve(&db, 7, Level::View).await.unwrap();
        assert!(scope.allows(11, Level::Edit));
        assert!(scope.allows(10, Level::View));
        assert!(!scope.allows(10, Level::Edit));
    }

    #[tokio::test]
    async fn with_a_none_base_only_granted_monitors_show() {
        let db = mock(vec![mon_perm(1, 11, Permission::View)], vec![], vec![]);
        let scope = resolve(&db, 7, Level::None).await.unwrap();
        assert_eq!(scope.visible_ids(Level::View), Some(vec![11]));
    }

    #[tokio::test]
    async fn a_create_base_level_edits_unlisted_monitors() {
        let db = mock(vec![mon_perm(1, 11, Permission::None)], vec![], vec![]);
        let scope = resolve(&db, 7, Level::Create).await.unwrap();
        assert!(scope.allows(10, Level::Edit));
    }

    #[tokio::test]
    async fn group_membership_is_inherited() {
        let db = mock(
            vec![grp_perm(1, 5, Permission::View)],
            vec![group(5, None)],
            vec![grp_mon(1, 5, 20), grp_mon(2, 5, 21)],
        );

        let scope = resolve(&db, 7, Level::View).await.unwrap();
        assert!(scope.allows(20, Level::View));
        assert!(scope.allows(21, Level::View));
        // Group grant is View only.
        assert!(!scope.allows(20, Level::Edit));
    }

    /// `editableMonitor` only takes `Edit` from a group: a `View` group
    /// shows the monitor and leaves editing to the base level.
    #[tokio::test]
    async fn a_view_group_does_not_cap_an_edit_base_level() {
        let db = mock(
            vec![grp_perm(1, 5, Permission::View)],
            vec![group(5, None)],
            vec![grp_mon(1, 5, 20)],
        );
        let scope = resolve(&db, 7, Level::Edit).await.unwrap();
        assert!(scope.allows(20, Level::Edit));
    }

    /// `editableMonitor` takes `Edit` from a group over a `View` base level.
    #[tokio::test]
    async fn an_edit_group_raises_a_view_base_level() {
        let db = mock(
            vec![grp_perm(1, 5, Permission::Edit)],
            vec![group(5, None)],
            vec![grp_mon(1, 5, 20)],
        );
        let scope = resolve(&db, 7, Level::View).await.unwrap();
        assert!(scope.allows(20, Level::Edit));
        assert!(!scope.allows(21, Level::Edit));
        assert!(scope.allows(21, Level::View));
    }

    #[tokio::test]
    async fn a_group_grant_covers_its_child_groups() {
        let db = mock(
            vec![grp_perm(1, 5, Permission::View)],
            vec![group(5, None), group(8, Some(5)), group(9, Some(8))],
            vec![grp_mon(1, 9, 30)],
        );
        let scope = resolve(&db, 7, Level::Edit).await.unwrap();
        assert!(scope.allows(30, Level::View));
    }

    #[tokio::test]
    async fn a_none_group_denies_its_monitors_over_another_groups_grant() {
        // Monitor 20 is in group 5 (View) and group 6 (None): ZoneMinder hides it.
        let db = mock(
            vec![
                grp_perm(1, 5, Permission::View),
                grp_perm(2, 6, Permission::None),
            ],
            vec![group(5, None), group(6, None)],
            vec![grp_mon(1, 5, 20), grp_mon(2, 6, 20), grp_mon(3, 5, 21)],
        );
        let scope = resolve(&db, 7, Level::Edit).await.unwrap();
        assert!(!scope.allows(20, Level::View));
        assert!(scope.allows(21, Level::View));
    }

    #[tokio::test]
    async fn a_none_group_denies_monitors_in_its_child_groups() {
        let db = mock(
            vec![
                grp_perm(1, 5, Permission::View),
                grp_perm(2, 6, Permission::None),
            ],
            vec![group(5, None), group(6, None), group(7, Some(6))],
            vec![grp_mon(1, 5, 20), grp_mon(2, 7, 20)],
        );
        let scope = resolve(&db, 7, Level::Edit).await.unwrap();
        assert!(!scope.allows(20, Level::View));
    }

    #[tokio::test]
    async fn direct_none_overrides_a_group_grant() {
        let db = mock(
            vec![
                mon_perm(1, 20, Permission::None),
                grp_perm(1, 5, Permission::Edit),
            ],
            vec![group(5, None)],
            vec![grp_mon(1, 5, 20), grp_mon(2, 5, 21)],
        );

        let scope = resolve(&db, 7, Level::Edit).await.unwrap();
        // Monitor 20 is explicitly denied despite the group granting Edit.
        assert!(!scope.allows(20, Level::View));
        // Monitor 21 keeps the group-granted Edit.
        assert!(scope.allows(21, Level::Edit));
    }

    #[tokio::test]
    async fn direct_inherit_keeps_the_group_level() {
        let db = mock(
            vec![
                mon_perm(1, 20, Permission::Inherit),
                grp_perm(1, 5, Permission::Edit),
            ],
            vec![group(5, None)],
            vec![grp_mon(1, 5, 20)],
        );

        let scope = resolve(&db, 7, Level::Edit).await.unwrap();
        assert!(scope.allows(20, Level::Edit));
    }

    #[tokio::test]
    async fn role_grants_apply_to_a_user_with_no_rows_of_their_own() {
        let db = mock(vec![role_mon(10, Permission::View)], vec![], vec![]);
        let scope = resolve(&db, 7, Level::Edit).await.unwrap();
        assert!(scope.allows(10, Level::View));
        assert!(!scope.allows(10, Level::Edit));
        // No row: the base level.
        assert!(scope.allows(11, Level::Edit));
    }

    #[tokio::test]
    async fn user_grants_win_over_role_grants() {
        let db = mock(
            vec![
                mon_perm(1, 10, Permission::None),
                role_mon(10, Permission::Edit),
                mon_perm(2, 11, Permission::Inherit),
                role_mon(11, Permission::View),
            ],
            vec![],
            vec![],
        );
        let scope = resolve(&db, 7, Level::Edit).await.unwrap();
        assert!(!scope.allows(10, Level::View));
        // Inherit falls through to the role.
        assert!(scope.allows(11, Level::View));
        assert!(!scope.allows(11, Level::Edit));
    }

    #[tokio::test]
    async fn role_group_grants_apply_after_user_grants() {
        let db = mock(
            vec![
                role_grp(5, Permission::View),
                role_grp(6, Permission::None),
                mon_perm(1, 22, Permission::Edit),
            ],
            vec![group(5, None), group(6, None)],
            vec![grp_mon(1, 5, 20), grp_mon(2, 6, 21), grp_mon(3, 6, 22)],
        );
        let scope = resolve(&db, 7, Level::Edit).await.unwrap();
        assert!(scope.allows(20, Level::View));
        assert!(!scope.allows(21, Level::View));
        // The user's own grant wins over the role group's None.
        assert!(scope.allows(22, Level::Edit));
    }

    #[tokio::test]
    async fn visible_ids_filters_by_required_level() {
        let mut map = HashMap::new();
        map.insert(10, Level::View);
        map.insert(11, Level::Edit);
        let scope = MonitorScope::Restricted(map);

        let mut view = scope.visible_ids(Level::View).unwrap();
        view.sort_unstable();
        assert_eq!(view, vec![10, 11]);

        let edit = scope.visible_ids(Level::Edit).unwrap();
        assert_eq!(edit, vec![11]);

        assert_eq!(MonitorScope::All.visible_ids(Level::View), None);
    }
}
