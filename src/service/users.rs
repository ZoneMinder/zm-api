use crate::dto::response::UserResponse;
use crate::dto::{PaginatedResponse, PaginationParams};
use crate::error::{AppError, AppResult, Resource, ResourceType};
use crate::repo;
use crate::server::state::AppState;

pub async fn list_all(state: &AppState) -> AppResult<Vec<UserResponse>> {
    let items = repo::users::find_all(state.db()).await?;
    Ok(items.iter().map(UserResponse::from).collect())
}

pub async fn list_paginated(
    state: &AppState,
    params: &PaginationParams,
) -> AppResult<PaginatedResponse<UserResponse>> {
    let (items, total) = repo::users::find_paginated(state.db(), params).await?;
    let responses: Vec<UserResponse> = items.iter().map(UserResponse::from).collect();
    Ok(PaginatedResponse::from_params(responses, total, params))
}

pub async fn get_by_id(state: &AppState, id: u32) -> AppResult<UserResponse> {
    let item = repo::users::find_by_id(state.db(), id).await?;
    let item = item.ok_or_else(|| {
        AppError::NotFoundError(Resource {
            details: vec![("id".into(), id.to_string())],
            resource_type: ResourceType::User,
        })
    })?;
    Ok(UserResponse::from(&item))
}

/// A user's `role_id` must name a role, not fail on the foreign key.
async fn ensure_role_exists(state: &AppState, role_id: u32) -> AppResult<()> {
    use sea_orm::EntityTrait;
    crate::entity::user_roles::Entity::find_by_id(role_id)
        .one(state.db())
        .await?
        .map(|_| ())
        .ok_or_else(|| AppError::BadRequestError(format!("role {role_id} does not exist")))
}

/// A revocation floor that kills every token issued up to now, including
/// those minted earlier in the current second (see `service::auth::logout`).
fn revoke_now_floor() -> u64 {
    (crate::util::time::unix_now() + 1) as u64
}

pub async fn update(
    state: &AppState,
    id: u32,
    mut req: crate::dto::request::UpdateUserRequest,
) -> AppResult<UserResponse> {
    // Tokens carry the permissions they were issued with. When an admin
    // changes those, disables the user or resets their password, revoke the
    // outstanding tokens so the user can't keep acting on the old grants.
    if req.changes_access() {
        let floor = revoke_now_floor();
        req.token_min_expiry = Some(req.token_min_expiry.map_or(floor, |f| f.max(floor)));
    }
    // Hash a new password before it reaches the DB (never store plaintext).
    let hashed_password = match &req.password {
        Some(pw) if !pw.is_empty() => Some(crate::util::password::hash(pw.clone()).await?),
        _ => None,
    };
    // Mirror an explicit token-revocation floor into the in-memory registry so
    // an admin revoke-all takes effect immediately, not just on restart.
    if let Some(floor) = req.token_min_expiry {
        state.revocations.revoke(id, floor as i64);
    }
    if let Some(Some(role_id)) = req.role_id {
        ensure_role_exists(state, role_id).await?;
    }
    let item = repo::users::update(state.db(), id, &req, hashed_password).await?;
    let item = item.ok_or_else(|| {
        AppError::NotFoundError(Resource {
            details: vec![("id".into(), id.to_string())],
            resource_type: ResourceType::User,
        })
    })?;
    Ok(UserResponse::from(&item))
}

pub async fn create(
    state: &AppState,
    mut req: crate::dto::request::CreateUserRequest,
) -> AppResult<UserResponse> {
    // Hash the password before it ever reaches the DB. Without this the
    // plaintext is stored verbatim, which both leaks credentials and makes the
    // account unusable (the bcrypt login verifier can never match a plaintext
    // value). See docs/REVIEW_FIXES_PLAN.md §1.1.
    req.password = crate::util::password::hash(req.password).await?;
    if let Some(role_id) = req.role_id {
        ensure_role_exists(state, role_id).await?;
    }
    let model = repo::users::create(state.db(), &req).await?;
    Ok(UserResponse::from(&model))
}

pub async fn delete(state: &AppState, id: u32) -> AppResult<()> {
    let ok = repo::users::delete_by_id(state.db(), id).await?;
    if ok {
        // The row (and its TokenMinExpiry) is gone, so only the in-memory
        // floor can stop the deleted user's live tokens.
        state.revocations.revoke(id, revoke_now_floor() as i64);
        Ok(())
    } else {
        Err(crate::error::AppError::NotFoundError(
            crate::error::Resource {
                details: vec![("id".into(), id.to_string())],
                resource_type: crate::error::ResourceType::User,
            },
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entity::users::Model as UserModel;
    use crate::error::AppError;
    use crate::server::state::AppState;
    use sea_orm::{DatabaseBackend, MockDatabase, MockExecResult};

    #[tokio::test]
    async fn test_get_by_id_not_found() {
        let empty: Vec<UserModel> = vec![];
        let db = MockDatabase::new(DatabaseBackend::MySql)
            .append_query_results::<UserModel, _, _>(vec![empty])
            .into_connection();
        let state = AppState::for_test_with_db(db);
        let err = get_by_id(&state, 123).await.expect_err("should err");
        matches!(err, AppError::NotFoundError(_));
    }

    #[tokio::test]
    async fn test_update_not_found() {
        let empty: Vec<UserModel> = vec![];
        let db = MockDatabase::new(DatabaseBackend::MySql)
            .append_query_results::<UserModel, _, _>(vec![empty])
            .into_connection();
        let state = AppState::for_test_with_db(db);
        let err = update(
            &state,
            1,
            crate::dto::request::UpdateUserRequest {
                email: Some("x@y.com".into()),
                enabled: Some(1),
                ..Default::default()
            },
        )
        .await
        .expect_err("should err");
        matches!(err, AppError::NotFoundError(_));
    }

    #[tokio::test]
    async fn test_delete_not_found() {
        let db = MockDatabase::new(DatabaseBackend::MySql)
            .append_exec_results(vec![MockExecResult {
                last_insert_id: 0,
                rows_affected: 0,
            }])
            .into_connection();
        let state = AppState::for_test_with_db(db);
        let err = delete(&state, 77).await.expect_err("should err");
        matches!(err, AppError::NotFoundError(_));
    }

    fn mk_user(id: u32, username: &str) -> UserModel {
        use crate::entity::sea_orm_active_enums as E;
        UserModel {
            id,
            username: username.into(),
            password: "hash".into(),
            name: "Name".into(),
            email: format!("{username}@example.com"),
            phone: "".into(),
            language: None,
            enabled: 1,
            stream: E::Stream::View,
            events: E::Events::View,
            control: E::Control::View,
            monitors: E::Monitors::View,
            groups: E::Groups::View,
            devices: E::Devices::View,
            snapshots: E::Snapshots::View,
            system: E::System::View,
            max_bandwidth: None,
            token_min_expiry: 0,
            api_enabled: 1,
            home_view: "console".into(),
            role_id: None,
        }
    }

    #[tokio::test]
    async fn test_list_all_ok() {
        let db = MockDatabase::new(DatabaseBackend::MySql)
            .append_query_results::<UserModel, _, _>(vec![vec![
                mk_user(1, "alice"),
                mk_user(2, "bob"),
            ]])
            .into_connection();
        let state = AppState::for_test_with_db(db);
        let out = list_all(&state).await.unwrap();
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].username, "alice");
    }

    #[tokio::test]
    async fn test_get_by_id_ok() {
        let db = MockDatabase::new(DatabaseBackend::MySql)
            .append_query_results::<UserModel, _, _>(vec![vec![mk_user(10, "carol")]])
            .into_connection();
        let state = AppState::for_test_with_db(db);
        let out = get_by_id(&state, 10).await.unwrap();
        assert_eq!(out.id, 10);
        assert_eq!(out.username, "carol");
    }

    #[tokio::test]
    async fn test_update_ok() {
        let initial = mk_user(5, "dave");
        let mut after = initial.clone();
        after.email = "new@example.com".into();
        after.enabled = 0;
        let db = MockDatabase::new(DatabaseBackend::MySql)
            .append_query_results::<UserModel, _, _>(vec![vec![initial]])
            .append_exec_results(vec![MockExecResult {
                last_insert_id: 0,
                rows_affected: 1,
            }])
            .append_query_results::<UserModel, _, _>(vec![vec![after.clone()]])
            .into_connection();
        let state = AppState::for_test_with_db(db);
        let out = update(
            &state,
            5,
            crate::dto::request::UpdateUserRequest {
                email: Some("new@example.com".into()),
                enabled: Some(0),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(out.email, "new@example.com");
        assert_eq!(out.enabled, 0);
    }

    #[tokio::test]
    async fn changing_a_users_access_revokes_their_tokens() {
        let user = mk_user(6, "erin");
        let db = MockDatabase::new(DatabaseBackend::MySql)
            .append_query_results::<UserModel, _, _>(vec![vec![user.clone()]])
            .append_exec_results(vec![MockExecResult {
                last_insert_id: 0,
                rows_affected: 1,
            }])
            .append_query_results::<UserModel, _, _>(vec![vec![user]])
            .into_connection();
        let state = AppState::for_test_with_db(db);
        let issued = crate::util::time::unix_now();
        assert!(!state.revocations.is_revoked(6, issued));
        update(
            &state,
            6,
            crate::dto::request::UpdateUserRequest {
                permissions: crate::dto::request::users::UserPermissionsInput {
                    events: Some("None".into()),
                    ..Default::default()
                },
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert!(state.revocations.is_revoked(6, issued));
    }

    #[test]
    fn profile_edits_do_not_count_as_access_changes() {
        let req = crate::dto::request::UpdateUserRequest {
            email: Some("a@b.c".into()),
            name: Some("A".into()),
            home_view: Some("console".into()),
            ..Default::default()
        };
        assert!(!req.changes_access());
    }

    #[tokio::test]
    async fn deleting_a_user_revokes_their_tokens() {
        let db = MockDatabase::new(DatabaseBackend::MySql)
            .append_exec_results(vec![MockExecResult {
                last_insert_id: 0,
                rows_affected: 1,
            }])
            .into_connection();
        let state = AppState::for_test_with_db(db);
        let issued = crate::util::time::unix_now();
        delete(&state, 9).await.unwrap();
        assert!(state.revocations.is_revoked(9, issued));
    }

    #[tokio::test]
    async fn test_create_ok() {
        use crate::dto::request::users::CreateUserRequest;
        let db = MockDatabase::new(DatabaseBackend::MySql)
            .append_exec_results(vec![MockExecResult {
                last_insert_id: 100,
                rows_affected: 1,
            }])
            .append_query_results::<UserModel, _, _>(vec![vec![mk_user(100, "eve")]])
            .into_connection();
        let state = AppState::for_test_with_db(db);
        let req = CreateUserRequest {
            username: "eve".into(),
            password: "pass".into(),
            name: Some("Eve".into()),
            email: "eve@example.com".into(),
            phone: None,
            enabled: Some(1),
            ..Default::default()
        };
        // NOTE: this exercises the hashing code path but cannot assert on the
        // stored value — `MockDatabase` returns a canned row and the connection
        // is moved into `AppState` (the `mock` feature disables
        // `DatabaseConnection: Clone`, and the transaction log is only readable
        // by consuming the connection). The authoritative regression that the
        // password is bcrypt-hashed and never stored in plaintext lives in the
        // DB integration test `test_api_users_create_get_delete`
        // (REVIEW_FIXES_PLAN §1.1).
        let out = create(&state, req).await.unwrap();
        assert_eq!(out.username, "eve");
    }

    #[tokio::test]
    async fn test_delete_ok() {
        let db = MockDatabase::new(DatabaseBackend::MySql)
            .append_exec_results(vec![MockExecResult {
                last_insert_id: 0,
                rows_affected: 1,
            }])
            .into_connection();
        let state = AppState::for_test_with_db(db);
        assert!(delete(&state, 1).await.is_ok());
    }
}
