use crate::dto::response::GroupResponse;
use crate::dto::{PaginatedResponse, PaginationParams};
use crate::error::{AppError, AppResult, Resource, ResourceType};
use crate::repo;
use crate::server::state::AppState;
use crate::service::group_acl::GroupScope;
use crate::util::authz::Level;

/// The 404 returned both for a genuinely missing group and for a group the
/// caller's [`GroupScope`] hides — identical, so scope never leaks existence.
fn not_found(id: u32) -> AppError {
    AppError::NotFoundError(Resource {
        details: vec![("id".into(), id.to_string())],
        resource_type: ResourceType::User,
    })
}

pub async fn list_all(state: &AppState) -> AppResult<Vec<GroupResponse>> {
    let items = repo::groups::find_all(state.db()).await?;
    Ok(items.iter().map(GroupResponse::from).collect())
}

pub async fn list_paginated(
    state: &AppState,
    params: &PaginationParams,
    scope: &GroupScope,
) -> AppResult<PaginatedResponse<GroupResponse>> {
    let filter = scope.visible_ids(Level::View);
    let (items, total) =
        repo::groups::find_paginated(state.db(), params, filter.as_deref()).await?;
    let responses: Vec<GroupResponse> = items.iter().map(GroupResponse::from).collect();
    Ok(PaginatedResponse::from_params(responses, total, params))
}

pub async fn get_by_id(state: &AppState, id: u32, scope: &GroupScope) -> AppResult<GroupResponse> {
    if !scope.allows(id, Level::View) {
        return Err(not_found(id));
    }
    let item = repo::groups::find_by_id(state.db(), id).await?;
    let item = item.ok_or_else(|| not_found(id))?;
    Ok(GroupResponse::from(&item))
}

pub async fn update(
    state: &AppState,
    id: u32,
    name: Option<String>,
    parent_id: Option<Option<u32>>,
    scope: &GroupScope,
) -> AppResult<GroupResponse> {
    if !scope.allows(id, Level::Edit) {
        return Err(not_found(id));
    }
    if let Some(new_parent) = parent_id {
        let current = repo::groups::find_by_id(state.db(), id)
            .await?
            .ok_or_else(|| not_found(id))?;
        if current.parent_id != new_parent {
            // Where a group sits decides whose grants reach its monitors, so
            // moving it needs Edit where it comes from and where it goes.
            if let Some(old_parent) = current.parent_id {
                ensure_may_place_under(scope, old_parent)?;
            }
            if let Some(new_parent) = new_parent {
                ensure_may_place_under(scope, new_parent)?;
                // Must not form a cycle or self-parent.
                validate_new_parent(state, id, new_parent).await?;
            }
        }
    }
    let item = repo::groups::update(state.db(), id, name, parent_id).await?;
    let item = item.ok_or_else(|| not_found(id))?;
    Ok(GroupResponse::from(&item))
}

/// A group may go under (or come out from under) `parent` only with Edit on
/// it. A parent the caller can't see answers as if it didn't exist.
fn ensure_may_place_under(scope: &GroupScope, parent: u32) -> AppResult<()> {
    if !scope.allows(parent, Level::View) {
        return Err(AppError::BadRequestError(format!(
            "parent group {parent} does not exist"
        )));
    }
    if !scope.allows(parent, Level::Edit) {
        return Err(AppError::PermissionDeniedError(format!(
            "Edit on group {parent} required to move groups in or out of it"
        )));
    }
    Ok(())
}

/// Reject re-parenting `id` under `new_parent` when it would self-parent, name
/// a non-existent parent, or create a cycle (i.e. `id` is already an ancestor
/// of `new_parent`).
async fn validate_new_parent(state: &AppState, id: u32, new_parent: u32) -> AppResult<()> {
    if new_parent == id {
        return Err(AppError::BadRequestError(
            "a group cannot be its own parent".to_string(),
        ));
    }
    // Walk up from the proposed parent; if we reach `id`, this would cycle.
    let mut cursor = Some(new_parent);
    let mut hops = 0u32;
    while let Some(pid) = cursor {
        if pid == id {
            return Err(AppError::BadRequestError(
                "re-parenting would create a cycle in the group hierarchy".to_string(),
            ));
        }
        match repo::groups::find_by_id(state.db(), pid).await? {
            Some(model) => cursor = model.parent_id,
            None => {
                if pid == new_parent {
                    return Err(AppError::BadRequestError(format!(
                        "parent group {new_parent} does not exist"
                    )));
                }
                break; // a dangling ancestor link; not our concern here
            }
        }
        hops += 1;
        if hops > 10_000 {
            break; // defensive: never loop forever on corrupt data
        }
    }
    Ok(())
}

pub async fn create(
    state: &AppState,
    req: crate::dto::request::CreateGroupRequest,
    scope: &GroupScope,
) -> AppResult<GroupResponse> {
    if let Some(parent) = req.parent_id {
        ensure_may_place_under(scope, parent)?;
    }
    let model = repo::groups::create(state.db(), &req).await?;
    Ok(GroupResponse::from(&model))
}

pub async fn delete(state: &AppState, id: u32, scope: &GroupScope) -> AppResult<()> {
    if !scope.allows(id, Level::Edit) {
        return Err(not_found(id));
    }
    let ok = repo::groups::delete_by_id(state.db(), id).await?;
    if ok {
        Ok(())
    } else {
        Err(not_found(id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entity::groups::Model as GroupModel;
    use sea_orm::{DatabaseBackend, MockDatabase, MockExecResult};

    fn mk_group(id: u32, name: &str) -> GroupModel {
        GroupModel {
            id,
            name: name.into(),
            parent_id: None,
        }
    }

    #[tokio::test]
    async fn update_rejects_self_parenting() {
        // Only the group itself is read: self-parent is caught before the walk.
        let db = MockDatabase::new(DatabaseBackend::MySql)
            .append_query_results::<GroupModel, _, _>(vec![vec![mk_group(5, "g5")]])
            .into_connection();
        let state = AppState::for_test_with_db(db);
        let err = update(&state, 5, None, Some(Some(5)), &GroupScope::All)
            .await
            .expect_err("a group cannot be its own parent");
        assert!(matches!(err, AppError::BadRequestError(_)), "got {err:?}");
    }

    #[tokio::test]
    async fn update_rejects_reparent_cycle() {
        // Re-parent group 5 under group 10, whose parent is already 5 → cycle.
        let mut g10 = mk_group(10, "g10");
        g10.parent_id = Some(5);
        let db = MockDatabase::new(DatabaseBackend::MySql)
            .append_query_results::<GroupModel, _, _>(vec![vec![mk_group(5, "g5")], vec![g10]])
            .into_connection();
        let state = AppState::for_test_with_db(db);
        let err = update(&state, 5, None, Some(Some(10)), &GroupScope::All)
            .await
            .expect_err("re-parenting into a cycle must be rejected");
        assert!(matches!(err, AppError::BadRequestError(_)), "got {err:?}");
    }

    #[tokio::test]
    async fn update_rejects_nonexistent_parent() {
        let empty: Vec<GroupModel> = vec![];
        let db = MockDatabase::new(DatabaseBackend::MySql)
            .append_query_results::<GroupModel, _, _>(vec![vec![mk_group(5, "g5")], empty])
            .into_connection();
        let state = AppState::for_test_with_db(db);
        let err = update(&state, 5, None, Some(Some(999)), &GroupScope::All)
            .await
            .expect_err("re-parenting under a missing group must be rejected");
        assert!(matches!(err, AppError::BadRequestError(_)), "got {err:?}");
    }

    #[tokio::test]
    async fn test_list_all_ok() {
        let db = MockDatabase::new(DatabaseBackend::MySql)
            .append_query_results::<GroupModel, _, _>(vec![vec![
                mk_group(1, "g1"),
                mk_group(2, "g2"),
            ]])
            .into_connection();
        let state = AppState::for_test_with_db(db);
        let out = list_all(&state).await.unwrap();
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].name, "g1");
    }

    #[tokio::test]
    async fn test_get_by_id_ok_and_not_found() {
        let db_ok = MockDatabase::new(DatabaseBackend::MySql)
            .append_query_results::<GroupModel, _, _>(vec![vec![mk_group(9, "ok")]])
            .into_connection();
        let state_ok = AppState::for_test_with_db(db_ok);
        assert_eq!(
            get_by_id(&state_ok, 9, &GroupScope::All).await.unwrap().id,
            9
        );

        let empty: Vec<GroupModel> = vec![];
        let db_none = MockDatabase::new(DatabaseBackend::MySql)
            .append_query_results::<GroupModel, _, _>(vec![empty])
            .into_connection();
        let state_none = AppState::for_test_with_db(db_none);
        assert!(matches!(
            get_by_id(&state_none, 1, &GroupScope::All)
                .await
                .err()
                .unwrap(),
            AppError::NotFoundError(_)
        ));
    }

    #[tokio::test]
    async fn test_update_ok_and_not_found() {
        let initial = mk_group(3, "old");
        let after = mk_group(3, "new");
        let db_ok = MockDatabase::new(DatabaseBackend::MySql)
            .append_query_results::<GroupModel, _, _>(vec![vec![initial]])
            .append_exec_results(vec![MockExecResult {
                last_insert_id: 0,
                rows_affected: 1,
            }])
            .append_query_results::<GroupModel, _, _>(vec![vec![after.clone()]])
            .into_connection();
        let state_ok = AppState::for_test_with_db(db_ok);
        assert_eq!(
            update(&state_ok, 3, Some("new".into()), None, &GroupScope::All)
                .await
                .unwrap()
                .name,
            "new"
        );

        let empty: Vec<GroupModel> = vec![];
        let db_none = MockDatabase::new(DatabaseBackend::MySql)
            .append_query_results::<GroupModel, _, _>(vec![empty])
            .into_connection();
        let state_none = AppState::for_test_with_db(db_none);
        assert!(matches!(
            update(&state_none, 3, Some("x".into()), None, &GroupScope::All)
                .await
                .err()
                .unwrap(),
            AppError::NotFoundError(_)
        ));
    }

    #[tokio::test]
    async fn test_create_and_delete() {
        use crate::dto::request::groups::CreateGroupRequest;
        let db_create = MockDatabase::new(DatabaseBackend::MySql)
            .append_exec_results(vec![MockExecResult {
                last_insert_id: 42,
                rows_affected: 1,
            }])
            .append_query_results::<GroupModel, _, _>(vec![vec![mk_group(42, "new")]])
            .into_connection();
        let state_create = AppState::for_test_with_db(db_create);
        let req = CreateGroupRequest {
            name: "new".into(),
            parent_id: None,
        };
        assert_eq!(
            create(&state_create, req, &GroupScope::All)
                .await
                .unwrap()
                .name,
            "new"
        );

        let db_del_ok = MockDatabase::new(DatabaseBackend::MySql)
            .append_exec_results(vec![MockExecResult {
                last_insert_id: 0,
                rows_affected: 1,
            }])
            .into_connection();
        let state_del_ok = AppState::for_test_with_db(db_del_ok);
        assert!(delete(&state_del_ok, 1, &GroupScope::All).await.is_ok());

        let db_del_none = MockDatabase::new(DatabaseBackend::MySql)
            .append_exec_results(vec![MockExecResult {
                last_insert_id: 0,
                rows_affected: 0,
            }])
            .into_connection();
        let state_del_none = AppState::for_test_with_db(db_del_none);
        assert!(matches!(
            delete(&state_del_none, 1, &GroupScope::All)
                .await
                .err()
                .unwrap(),
            AppError::NotFoundError(_)
        ));
    }
}
