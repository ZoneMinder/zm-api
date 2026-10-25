use crate::dto::request::tags::{CreateTagRequest, UpdateTagRequest};
use crate::dto::response::events_tags::{EventSummary, TagDetailResponse};
use crate::dto::response::tags::PaginatedTagsResponse;
use crate::dto::response::TagResponse;
use crate::dto::{PaginatedResponse, PaginationParams};
use crate::error::{AppError, AppResult, Resource, ResourceType};
use crate::repo;
use crate::server::state::AppState;
use crate::service::monitor_acl::MonitorScope;
use crate::util::authz::Level;

/// Default page size for paginated event listing
const DEFAULT_PAGE_SIZE: u64 = 20;

pub async fn list_all(state: &AppState, scope: &MonitorScope) -> AppResult<Vec<TagResponse>> {
    let items = repo::tags::find_all(state.db()).await?;
    let visible = scope.visible_ids(Level::View);
    let counts = repo::tags::get_event_counts(state.db(), visible.as_deref()).await?;

    Ok(items
        .iter()
        .map(|t| {
            let count = counts.get(&t.id).copied().unwrap_or(0);
            TagResponse::with_event_count(t, count)
        })
        .collect())
}

pub async fn list_paginated(
    state: &AppState,
    params: &PaginationParams,
    scope: &MonitorScope,
) -> AppResult<PaginatedTagsResponse> {
    let (items, total) = repo::tags::find_paginated(state.db(), params).await?;
    let visible = scope.visible_ids(Level::View);
    let counts = repo::tags::get_event_counts(state.db(), visible.as_deref()).await?;

    let responses: Vec<TagResponse> = items
        .iter()
        .map(|t| {
            let count = counts.get(&t.id).copied().unwrap_or(0);
            TagResponse::with_event_count(t, count)
        })
        .collect();

    Ok(PaginatedTagsResponse::from(PaginatedResponse::from_params(
        responses, total, params,
    )))
}

pub async fn get_by_id(state: &AppState, id: u64) -> AppResult<TagResponse> {
    let item = repo::tags::find_by_id(state.db(), id).await?;
    let item = item.ok_or_else(|| {
        AppError::NotFoundError(Resource {
            details: vec![("id".into(), id.to_string())],
            resource_type: ResourceType::Message,
        })
    })?;
    Ok(TagResponse::from(&item))
}

/// Get tag detail with paginated events
pub async fn get_by_id_with_events(
    state: &AppState,
    id: u64,
    page: Option<u64>,
    page_size: Option<u64>,
    scope: &MonitorScope,
) -> AppResult<TagDetailResponse> {
    let page = page.unwrap_or(1).max(1);
    let page_size = page_size.unwrap_or(DEFAULT_PAGE_SIZE).clamp(1, 1000);

    let tag = repo::tags::find_by_id(state.db(), id).await?;
    let tag = tag.ok_or_else(|| {
        AppError::NotFoundError(Resource {
            details: vec![("id".into(), id.to_string())],
            resource_type: ResourceType::Message,
        })
    })?;

    // Only events on monitors the caller can see, as `/events` lists them.
    let visible = scope.visible_ids(Level::View);
    let (events, total) =
        repo::tags::find_events_for_tag(state.db(), id, visible.as_deref(), page - 1, page_size)
            .await?;

    let total_pages = if total == 0 {
        0
    } else {
        total.div_ceil(page_size)
    };

    let event_summaries: Vec<EventSummary> = events.iter().map(EventSummary::from).collect();

    Ok(TagDetailResponse {
        id: tag.id,
        name: tag.name,
        create_date: tag.create_date.map(crate::util::timestamp_to_utc),
        events: event_summaries,
        total_events: total,
        per_page: page_size,
        current_page: page,
        last_page: total_pages,
    })
}

pub async fn create(state: &AppState, req: CreateTagRequest) -> AppResult<TagResponse> {
    let model = repo::tags::create(state.db(), &req).await?;
    Ok(TagResponse::from(&model))
}

pub async fn update(state: &AppState, id: u64, req: UpdateTagRequest) -> AppResult<TagResponse> {
    let updated = repo::tags::update(state.db(), id, &req).await?;
    let updated = updated.ok_or_else(|| {
        AppError::NotFoundError(Resource {
            details: vec![("id".into(), id.to_string())],
            resource_type: ResourceType::Message,
        })
    })?;
    Ok(TagResponse::from(&updated))
}

pub async fn delete(state: &AppState, id: u64) -> AppResult<()> {
    let ok = repo::tags::delete_by_id(state.db(), id).await?;
    if ok {
        Ok(())
    } else {
        Err(AppError::NotFoundError(Resource {
            details: vec![("id".into(), id.to_string())],
            resource_type: ResourceType::Message,
        }))
    }
}
