use crate::dto::request::snapshots::{CreateSnapshotRequest, UpdateSnapshotRequest};
use crate::dto::response::SnapshotResponse;
use crate::dto::{PaginatedResponse, PaginationParams};
use crate::error::{AppError, AppResult, Resource, ResourceType};
use crate::repo;
use crate::server::state::AppState;

pub async fn list_all(state: &AppState) -> AppResult<Vec<SnapshotResponse>> {
    let items = repo::snapshots::find_all(state.db()).await?;
    Ok(items.iter().map(SnapshotResponse::from).collect())
}

pub async fn list_paginated(
    state: &AppState,
    params: &PaginationParams,
) -> AppResult<PaginatedResponse<SnapshotResponse>> {
    let (items, total) = repo::snapshots::find_paginated(state.db(), params).await?;
    let responses: Vec<SnapshotResponse> = items.iter().map(SnapshotResponse::from).collect();
    Ok(PaginatedResponse::from_params(responses, total, params))
}

pub async fn get_by_id(state: &AppState, id: u32) -> AppResult<SnapshotResponse> {
    let item = repo::snapshots::find_by_id(state.db(), id).await?;
    let item = item.ok_or_else(|| {
        AppError::NotFoundError(Resource {
            details: vec![("id".into(), id.to_string())],
            resource_type: ResourceType::Message,
        })
    })?;
    Ok(SnapshotResponse::from(&item))
}

/// Who is asking: their id, and whether they are a System editor.
#[derive(Debug, Clone, Copy)]
pub struct Caller {
    pub uid: u32,
    pub admin: bool,
}

/// A snapshot is recorded as made by whoever made it, as ZoneMinder's web UI
/// does. The request's `created_by` would let one user file a snapshot under
/// another's name.
fn authored(mut req: CreateSnapshotRequest, caller: Caller) -> CreateSnapshotRequest {
    req.created_by = i32::try_from(caller.uid).ok();
    req
}

/// Only a System editor may put a snapshot under another user's name.
fn author_kept(mut req: UpdateSnapshotRequest, caller: Caller) -> UpdateSnapshotRequest {
    if !caller.admin {
        req.created_by = None;
    }
    req
}

pub async fn create(
    state: &AppState,
    caller: Caller,
    req: CreateSnapshotRequest,
) -> AppResult<SnapshotResponse> {
    let req = authored(req, caller);
    let model = repo::snapshots::create(state.db(), &req).await?;
    Ok(SnapshotResponse::from(&model))
}

pub async fn update(
    state: &AppState,
    caller: Caller,
    id: u32,
    req: UpdateSnapshotRequest,
) -> AppResult<SnapshotResponse> {
    let req = author_kept(req, caller);
    let updated = repo::snapshots::update(state.db(), id, &req).await?;
    let updated = updated.ok_or_else(|| {
        AppError::NotFoundError(Resource {
            details: vec![("id".into(), id.to_string())],
            resource_type: ResourceType::Message,
        })
    })?;
    Ok(SnapshotResponse::from(&updated))
}

pub async fn delete(state: &AppState, id: u32) -> AppResult<()> {
    let ok = repo::snapshots::delete_by_id(state.db(), id).await?;
    if ok {
        Ok(())
    } else {
        Err(AppError::NotFoundError(Resource {
            details: vec![("id".into(), id.to_string())],
            resource_type: ResourceType::Message,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const USER: Caller = Caller {
        uid: 42,
        admin: false,
    };
    const ADMIN: Caller = Caller {
        uid: 1,
        admin: true,
    };

    #[test]
    fn a_new_snapshot_is_its_callers_whatever_the_request_says() {
        let req = CreateSnapshotRequest {
            name: None,
            description: None,
            created_by: Some(1),
            created_on: None,
        };
        assert_eq!(authored(req.clone(), USER).created_by, Some(42));
        assert_eq!(authored(req, ADMIN).created_by, Some(1));
    }

    #[test]
    fn only_a_system_editor_may_change_a_snapshots_author() {
        let req = UpdateSnapshotRequest {
            name: Some("gate".into()),
            description: None,
            created_by: Some(7),
            created_on: None,
        };
        let kept = author_kept(req.clone(), USER);
        assert_eq!(kept.created_by, None);
        assert_eq!(kept.name.as_deref(), Some("gate"));
        assert_eq!(author_kept(req, ADMIN).created_by, Some(7));
    }
}
