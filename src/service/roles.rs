//! ZoneMinder 1.39 roles (`User_Roles`) (#143).

use sea_orm::{ActiveModelTrait, EntityTrait, PaginatorTrait, QueryOrder, Set};

use crate::dto::request::roles::{CreateRoleRequest, UpdateRoleRequest};
use crate::dto::response::roles::RoleResponse;
use crate::dto::{PaginatedResponse, PaginationParams};
use crate::entity::sea_orm_active_enums as E;
use crate::entity::user_roles::{ActiveModel, Column, Entity as Roles};
use crate::error::{AppError, AppResult, Resource, ResourceType};
use crate::server::state::AppState;

fn not_found(id: u32) -> AppError {
    AppError::NotFoundError(Resource {
        details: vec![("role_id".into(), id.to_string())],
        resource_type: ResourceType::User,
    })
}

pub async fn list(
    state: &AppState,
    params: &PaginationParams,
) -> AppResult<PaginatedResponse<RoleResponse>> {
    let paginator = Roles::find()
        .order_by_asc(Column::Name)
        .paginate(state.db(), params.page_size());
    let total = paginator.num_items().await?;
    let items = paginator
        .fetch_page(params.page().saturating_sub(1))
        .await?;
    Ok(PaginatedResponse::from_params(
        items.iter().map(RoleResponse::from).collect(),
        total,
        params,
    ))
}

pub async fn get(state: &AppState, id: u32) -> AppResult<RoleResponse> {
    Roles::find_by_id(id)
        .one(state.db())
        .await?
        .map(|r| RoleResponse::from(&r))
        .ok_or_else(|| not_found(id))
}

pub async fn create(state: &AppState, req: CreateRoleRequest) -> AppResult<RoleResponse> {
    let p = &req.permissions;
    let am = ActiveModel {
        id: Default::default(),
        name: Set(req.name.clone()),
        description: Set(req.description.clone()),
        stream: Set(p.stream_level()?.unwrap_or(E::Stream::None)),
        events: Set(p.events_level()?.unwrap_or(E::Events::None)),
        control: Set(p.control_level()?.unwrap_or(E::Control::None)),
        monitors: Set(p.monitors_level()?.unwrap_or(E::Monitors::None)),
        groups: Set(p.groups_level()?.unwrap_or(E::Groups::None)),
        devices: Set(p.devices_level()?.unwrap_or(E::Devices::None)),
        snapshots: Set(p.snapshots_level()?.unwrap_or(E::Snapshots::None)),
        system: Set(p.system_level()?.unwrap_or(E::System::None)),
    };
    Ok(RoleResponse::from(&am.insert(state.db()).await?))
}

pub async fn update(state: &AppState, id: u32, req: UpdateRoleRequest) -> AppResult<RoleResponse> {
    let model = Roles::find_by_id(id)
        .one(state.db())
        .await?
        .ok_or_else(|| not_found(id))?;
    let mut am: ActiveModel = model.into();
    if let Some(name) = req.name {
        am.name = Set(name);
    }
    if let Some(description) = req.description {
        am.description = Set(description);
    }
    let p = &req.permissions;
    if let Some(v) = p.stream_level()? {
        am.stream = Set(v);
    }
    if let Some(v) = p.events_level()? {
        am.events = Set(v);
    }
    if let Some(v) = p.control_level()? {
        am.control = Set(v);
    }
    if let Some(v) = p.monitors_level()? {
        am.monitors = Set(v);
    }
    if let Some(v) = p.groups_level()? {
        am.groups = Set(v);
    }
    if let Some(v) = p.devices_level()? {
        am.devices = Set(v);
    }
    if let Some(v) = p.snapshots_level()? {
        am.snapshots = Set(v);
    }
    if let Some(v) = p.system_level()? {
        am.system = Set(v);
    }
    let changes_levels = p.changes_any();
    let saved = am.update(state.db()).await?;
    if changes_levels {
        let holders = holders(state, id).await?;
        revoke_tokens(state, &holders).await?;
    }
    Ok(RoleResponse::from(&saved))
}

/// Delete a role. Its users keep their own levels: `Users.RoleId` is
/// `ON DELETE SET NULL`, and the role's monitor and group rows cascade.
pub async fn delete(state: &AppState, id: u32) -> AppResult<()> {
    // Read who holds it first: the delete clears their `RoleId`.
    let holders = holders(state, id).await?;
    let res = Roles::delete_by_id(id).exec(state.db()).await?;
    if res.rows_affected == 0 {
        return Err(not_found(id));
    }
    revoke_tokens(state, &holders).await
}

/// The ids of the users holding role `id`.
async fn holders(state: &AppState, id: u32) -> AppResult<Vec<u32>> {
    use crate::entity::users;
    use sea_orm::{ColumnTrait, QueryFilter};
    Ok(users::Entity::find()
        .filter(users::Column::RoleId.eq(id))
        .all(state.db())
        .await?
        .into_iter()
        .map(|u| u.id)
        .collect())
}

/// Revoke the tokens of `user_ids`. A token carries the levels its user had
/// at login, the role's included, so when a role's levels change or it goes,
/// its users' tokens must go with it. A user edit does the same
/// (`service::users::update`).
async fn revoke_tokens(state: &AppState, user_ids: &[u32]) -> AppResult<()> {
    use crate::entity::users;
    use sea_orm::{sea_query::Expr, ColumnTrait, QueryFilter};
    if user_ids.is_empty() {
        return Ok(());
    }
    // now+1, as logout does: it covers tokens minted in the current second.
    let floor = crate::util::time::unix_now() + 1;
    // In memory first, so the tokens are dead even if the write below fails.
    for uid in user_ids {
        state.revocations.revoke(*uid, floor);
    }
    users::Entity::update_many()
        .col_expr(users::Column::TokenMinExpiry, Expr::value(floor as u64))
        .filter(users::Column::Id.is_in(user_ids.iter().copied()))
        // Only ever raise it: a floor an administrator set further ahead, to
        // keep a user out, stays.
        .filter(users::Column::TokenMinExpiry.lt(floor as u64))
        .exec(state.db())
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dto::request::users::UserPermissionsInput;
    use crate::entity::{user_roles, users};
    use sea_orm::{DatabaseBackend, MockDatabase, MockExecResult};

    fn role() -> user_roles::Model {
        user_roles::Model {
            id: 7,
            name: "operators".into(),
            description: None,
            stream: E::Stream::View,
            events: E::Events::Edit,
            control: E::Control::None,
            monitors: E::Monitors::None,
            groups: E::Groups::None,
            devices: E::Devices::None,
            snapshots: E::Snapshots::None,
            system: E::System::Edit,
        }
    }

    fn holder(id: u32) -> users::Model {
        users::Model {
            id,
            username: format!("user{id}"),
            password: "hash".into(),
            name: String::new(),
            email: String::new(),
            phone: String::new(),
            language: None,
            enabled: 1,
            stream: E::Stream::None,
            events: E::Events::None,
            control: E::Control::None,
            monitors: E::Monitors::None,
            groups: E::Groups::None,
            devices: E::Devices::None,
            snapshots: E::Snapshots::None,
            system: E::System::None,
            max_bandwidth: None,
            token_min_expiry: 0,
            api_enabled: 1,
            home_view: "console".into(),
            role_id: Some(7),
        }
    }

    fn done() -> MockExecResult {
        MockExecResult {
            last_insert_id: 0,
            rows_affected: 1,
        }
    }

    /// Taking System away from a role must not leave its users' tokens
    /// carrying it until they expire.
    #[tokio::test]
    async fn changing_a_roles_levels_revokes_its_users_tokens() {
        let db = MockDatabase::new(DatabaseBackend::MySql)
            .append_query_results(vec![vec![role()]])
            .append_exec_results(vec![done()])
            .append_query_results(vec![vec![role()]])
            .append_query_results(vec![vec![holder(5), holder(6)]])
            .append_exec_results(vec![done()])
            .into_connection();
        let state = AppState::for_test_with_db(db);
        let issued = crate::util::time::unix_now();

        update(
            &state,
            7,
            UpdateRoleRequest {
                name: None,
                description: None,
                permissions: UserPermissionsInput {
                    system: Some("None".into()),
                    ..Default::default()
                },
            },
        )
        .await
        .unwrap();

        assert!(state.revocations.is_revoked(5, issued));
        assert!(state.revocations.is_revoked(6, issued));
        assert!(!state.revocations.is_revoked(1, issued));
    }

    #[tokio::test]
    async fn renaming_a_role_revokes_nothing() {
        let db = MockDatabase::new(DatabaseBackend::MySql)
            .append_query_results(vec![vec![role()]])
            .append_exec_results(vec![done()])
            .append_query_results(vec![vec![role()]])
            .into_connection();
        let state = AppState::for_test_with_db(db);

        update(
            &state,
            7,
            UpdateRoleRequest {
                name: Some("night shift".into()),
                description: None,
                permissions: UserPermissionsInput::default(),
            },
        )
        .await
        .unwrap();

        assert!(!state
            .revocations
            .is_revoked(5, crate::util::time::unix_now()));
    }

    #[tokio::test]
    async fn deleting_a_role_revokes_its_users_tokens() {
        let db = MockDatabase::new(DatabaseBackend::MySql)
            .append_query_results(vec![vec![holder(5)]])
            .append_exec_results(vec![done(), done()])
            .into_connection();
        let state = AppState::for_test_with_db(db);
        let issued = crate::util::time::unix_now();

        delete(&state, 7).await.unwrap();

        assert!(state.revocations.is_revoked(5, issued));
    }
}
