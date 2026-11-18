use crate::dto::response::ServerResponse;
use crate::dto::{PaginatedResponse, PaginationParams};
use crate::error::{AppError, AppResult, Resource, ResourceType};
use crate::repo;
use crate::server::state::AppState;

/// Responses for `items`, each with its monitor count.
async fn with_counts(
    state: &AppState,
    items: &[crate::entity::servers::Model],
) -> AppResult<Vec<ServerResponse>> {
    let counts = repo::servers::monitor_counts(state.db()).await?;
    Ok(items
        .iter()
        .map(|m| {
            let mut r = ServerResponse::from(m);
            r.monitor_count = counts.get(&m.id).copied().unwrap_or(0);
            r.api_url = state.cluster.base_url(m.id, m.hostname.as_deref());
            r.is_this_server = state.server_id == Some(m.id);
            r
        })
        .collect())
}

pub async fn list_all(state: &AppState) -> AppResult<Vec<ServerResponse>> {
    let items = repo::servers::find_all(state.db()).await?;
    with_counts(state, &items).await
}

pub async fn list_paginated(
    state: &AppState,
    params: &PaginationParams,
) -> AppResult<PaginatedResponse<ServerResponse>> {
    let (items, total) = repo::servers::find_paginated(state.db(), params).await?;
    let responses = with_counts(state, &items).await?;
    Ok(PaginatedResponse::from_params(responses, total, params))
}

pub async fn get_by_id(state: &AppState, id: u32) -> AppResult<ServerResponse> {
    let item = repo::servers::find_by_id(state.db(), id).await?;
    let item = item.ok_or_else(|| {
        AppError::NotFoundError(Resource {
            details: vec![("id".into(), id.to_string())],
            resource_type: ResourceType::Message,
        })
    })?;
    Ok(with_counts(state, std::slice::from_ref(&item))
        .await?
        .remove(0))
}

pub async fn create(
    state: &AppState,
    req: crate::dto::request::CreateServerRequest,
) -> AppResult<ServerResponse> {
    let model = repo::servers::create(state.db(), &req).await?;
    Ok(ServerResponse::from(&model))
}

pub async fn update(
    state: &AppState,
    id: u32,
    req: crate::dto::request::servers::UpdateServerRequest,
) -> AppResult<ServerResponse> {
    let updated = repo::servers::update(state.db(), id, &req).await?;
    let updated = updated.ok_or_else(|| {
        crate::error::AppError::NotFoundError(crate::error::Resource {
            details: vec![("id".into(), id.to_string())],
            resource_type: crate::error::ResourceType::Message,
        })
    })?;
    Ok(with_counts(state, std::slice::from_ref(&updated))
        .await?
        .remove(0))
}

pub async fn delete(state: &AppState, id: u32) -> AppResult<()> {
    let ok = repo::servers::delete_by_id(state.db(), id).await?;
    if ok {
        Ok(())
    } else {
        Err(crate::error::AppError::NotFoundError(
            crate::error::Resource {
                details: vec![("id".into(), id.to_string())],
                resource_type: crate::error::ResourceType::Message,
            },
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entity::sea_orm_active_enums::Status;
    use crate::entity::servers::Model as ServerModel;
    use sea_orm::{DatabaseBackend, MockDatabase, MockExecResult};

    /// An empty monitor-count result, for the query every read now makes.
    fn no_counts() -> Vec<std::collections::BTreeMap<String, sea_orm::Value>> {
        vec![]
    }

    fn mk(id: u32, name: &str) -> ServerModel {
        ServerModel {
            id,
            protocol: None,
            hostname: None,
            port: None,
            path_to_index: None,
            path_to_zms: None,
            path_to_api: None,
            name: name.into(),
            state_id: None,
            status: Status::Unknown,
            cpu_load: None,
            cpu_user_percent: None,
            cpu_nice_percent: None,
            cpu_system_percent: None,
            cpu_idle_percent: None,
            cpu_usage_percent: None,
            total_mem: None,
            free_mem: None,
            total_swap: None,
            free_swap: None,
            zmstats: 0,
            zmaudit: 0,
            zmtrigger: 0,
            zmeventnotification: 0,
            latitude: None,
            longitude: None,
        }
    }

    #[tokio::test]
    async fn test_list_get_ok() {
        let db = MockDatabase::new(DatabaseBackend::MySql)
            .append_query_results::<ServerModel, _, _>(vec![vec![mk(1, "s1"), mk(2, "s2")]])
            .append_query_results(vec![no_counts()])
            .into_connection();
        let state = AppState::for_test_with_db(db);
        assert_eq!(list_all(&state).await.unwrap().len(), 2);

        let db2 = MockDatabase::new(DatabaseBackend::MySql)
            .append_query_results::<ServerModel, _, _>(vec![vec![mk(9, "x")]])
            .append_query_results(vec![no_counts()])
            .into_connection();
        let state2 = AppState::for_test_with_db(db2);
        assert_eq!(get_by_id(&state2, 9).await.unwrap().id, 9);
    }

    #[tokio::test]
    async fn test_not_found_paths() {
        let empty: Vec<ServerModel> = vec![];
        let db_none_get = MockDatabase::new(DatabaseBackend::MySql)
            .append_query_results::<ServerModel, _, _>(vec![empty.clone()])
            .into_connection();
        let state_none_get = AppState::for_test_with_db(db_none_get);
        assert!(matches!(
            get_by_id(&state_none_get, 1).await.err().unwrap(),
            AppError::NotFoundError(_)
        ));

        let db_none_upd = MockDatabase::new(DatabaseBackend::MySql)
            .append_query_results::<ServerModel, _, _>(vec![empty])
            .into_connection();
        let state_none_upd = AppState::for_test_with_db(db_none_upd);
        assert!(matches!(
            update(
                &state_none_upd,
                1,
                crate::dto::request::servers::UpdateServerRequest {
                    name: Some("n".into()),
                    ..Default::default()
                }
            )
            .await
            .err()
            .unwrap(),
            AppError::NotFoundError(_)
        ));

        let db_del_none = MockDatabase::new(DatabaseBackend::MySql)
            .append_exec_results(vec![MockExecResult {
                last_insert_id: 0,
                rows_affected: 0,
            }])
            .into_connection();
        let state_del_none = AppState::for_test_with_db(db_del_none);
        assert!(matches!(
            delete(&state_del_none, 1).await.err().unwrap(),
            AppError::NotFoundError(_)
        ));
    }

    #[tokio::test]
    async fn test_update_create_delete_ok() {
        use crate::dto::request::servers::CreateServerRequest;
        let initial = mk(4, "old");
        let mut after = initial.clone();
        after.name = "new".into();
        after.hostname = Some("host".into());
        after.status = Status::Running;
        let db = MockDatabase::new(DatabaseBackend::MySql)
            .append_query_results::<ServerModel, _, _>(vec![vec![initial]])
            .append_exec_results(vec![MockExecResult {
                last_insert_id: 0,
                rows_affected: 1,
            }])
            .append_query_results::<ServerModel, _, _>(vec![vec![after.clone()]])
            .append_query_results(vec![no_counts()])
            .into_connection();
        let state = AppState::for_test_with_db(db);
        let out = update(
            &state,
            4,
            crate::dto::request::servers::UpdateServerRequest {
                name: Some("new".into()),
                hostname: Some(Some("host".into())),
                port: Some(Some(8080)),
                status: Some("running".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(out.name, "new");
        assert_eq!(out.hostname.as_deref(), Some("host"));
        assert_eq!(out.status, "Running");

        let db_create = MockDatabase::new(DatabaseBackend::MySql)
            .append_exec_results(vec![MockExecResult {
                last_insert_id: 12,
                rows_affected: 1,
            }])
            .append_query_results::<ServerModel, _, _>(vec![vec![mk(12, "srv")]])
            .into_connection();
        let state_create = AppState::for_test_with_db(db_create);
        let req = CreateServerRequest {
            name: "srv".into(),
            hostname: Some("h".into()),
            port: Some(80),
            status: Some("running".into()),
            ..Default::default()
        };
        assert_eq!(create(&state_create, req).await.unwrap().name, "srv");

        let db_del_ok = MockDatabase::new(DatabaseBackend::MySql)
            .append_exec_results(vec![MockExecResult {
                last_insert_id: 0,
                rows_affected: 1,
            }])
            .into_connection();
        let state_del_ok = AppState::for_test_with_db(db_del_ok);
        assert!(delete(&state_del_ok, 1).await.is_ok());
    }
}
