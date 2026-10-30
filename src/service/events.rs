use chrono::{DateTime, Utc};
use sea_orm::ActiveValue::Set;
use tracing::instrument;

use crate::{
    dto::{
        request::events::{
            parse_tag_filter, EventCreateRequest, EventQueryParams, EventUpdateRequest, TagOp,
        },
        response::events::{
            EventCountResponse, EventCountsResponse, EventResponse, PaginatedEventsResponse,
        },
        response::events_tags::TagSummary,
    },
    entity::events,
    error::{AppError, AppResult, Resource, ResourceType},
    repo::events as events_repo,
    server::state::AppState,
    service::monitor_acl::MonitorScope,
    util::authz::Level,
};

/// Default page size for paginated event listing
const DEFAULT_PAGE_SIZE: u64 = 20;

/// Not-found error for an event, used both when an event genuinely does not
/// exist and when it belongs to a monitor outside the caller's ACL scope (so
/// hidden monitors are not revealed).
fn event_not_found(id: u32) -> AppError {
    AppError::NotFoundError(Resource {
        details: vec![("id".to_string(), id.to_string())],
        resource_type: ResourceType::Message,
    })
}

/// List events with full query parameters support
#[instrument(level = "debug", skip(state))]
pub async fn list(
    state: &AppState,
    params: &EventQueryParams,
    scope: &MonitorScope,
) -> AppResult<PaginatedEventsResponse> {
    let page = params.page.unwrap_or(1);
    let page_size = params.page_size.unwrap_or(DEFAULT_PAGE_SIZE);

    let options = events_repo::EventQueryOptions {
        monitor_id: params.monitor_id,
        // The columns hold server-local wall-clock time; the filter is a UTC
        // instant (#173).
        start_time: params
            .start_time
            .as_ref()
            .map(|dt| crate::util::utc_to_naive_local(dt.0)),
        end_time: params
            .end_time
            .as_ref()
            .map(|dt| crate::util::utc_to_naive_local(dt.0)),
        sort_field: params.sort.unwrap_or_default(),
        sort_direction: params.direction.unwrap_or_default(),
        alarm_frames_min: params.alarm_frames_min,
        archived: params.archived,
        cause: params.cause.clone().filter(|s| !s.is_empty()),
        name: params.name.clone().filter(|s| !s.is_empty()),
        notes: params.notes.clone().filter(|s| !s.is_empty()),
        q: params.q.clone().filter(|s| !s.trim().is_empty()),
        storage_id: params.storage_id,
        tags: params
            .tag_id
            .as_deref()
            .map(parse_tag_filter)
            .transpose()
            .map_err(|e| AppError::BadRequestError(format!("tag_id: {e}")))?,
        tags_negated: matches!(params.tag_op, Some(TagOp::NotIn)),
        monitor_filter: scope.visible_ids(Level::View),
    };

    let totals = if params.totals {
        Some(events_repo::totals_with_options(state, &options).await?)
    } else {
        None
    };
    let (events, total) =
        events_repo::find_with_options(state, options, page - 1, page_size).await?;

    let total_pages = total.div_ceil(page_size);

    // Batch load tags for all events
    let event_ids: Vec<u64> = events.iter().map(|e| e.id).collect();
    let tags_map = events_repo::find_tags_for_events(state, &event_ids).await?;

    let event_responses = events
        .into_iter()
        .map(|e| {
            let event_id = e.id;
            let tags: Vec<TagSummary> = tags_map
                .get(&event_id)
                .map(|t| t.iter().map(TagSummary::from).collect())
                .unwrap_or_default();
            EventResponse::with_tags(e, tags)
        })
        .collect();

    Ok(PaginatedEventsResponse {
        items: event_responses,
        total,
        per_page: page_size,
        current_page: page,
        last_page: total_pages,
        total_length_seconds: totals.map(|t| t.0),
        total_disk_space: totals.map(|t| t.1),
    })
}

/// List all events with pagination (legacy helper)
#[instrument(level = "debug", skip(state))]
pub async fn list_all(
    state: &AppState,
    page: Option<u64>,
    page_size: Option<u64>,
    scope: &MonitorScope,
) -> AppResult<PaginatedEventsResponse> {
    list(
        state,
        &EventQueryParams {
            page,
            page_size,
            monitor_id: None,
            start_time: None,
            end_time: None,
            sort: None,
            direction: None,
            alarm_frames_min: None,
            archived: None,
            cause: None,
            name: None,
            notes: None,
            q: None,
            tag_id: None,
            tag_op: None,
            totals: false,
            storage_id: None,
        },
        scope,
    )
    .await
}

/// List events by monitor ID with pagination (legacy helper)
#[instrument(level = "debug", skip(state))]
pub async fn list_by_monitor(
    state: &AppState,
    monitor_id: u32,
    page: Option<u64>,
    page_size: Option<u64>,
    start_time: Option<DateTime<Utc>>,
    end_time: Option<DateTime<Utc>>,
    scope: &MonitorScope,
) -> AppResult<PaginatedEventsResponse> {
    use crate::dto::wrappers::DateTimeWrapper;

    list(
        state,
        &EventQueryParams {
            page,
            page_size,
            monitor_id: Some(monitor_id),
            start_time: start_time.map(DateTimeWrapper),
            end_time: end_time.map(DateTimeWrapper),
            sort: None,
            direction: None,
            alarm_frames_min: None,
            archived: None,
            cause: None,
            name: None,
            notes: None,
            q: None,
            tag_id: None,
            tag_op: None,
            totals: false,
            storage_id: None,
        },
        scope,
    )
    .await
}

/// Get event by ID
#[instrument(level = "debug", skip(state))]
pub async fn get_by_id(
    state: &AppState,
    id: u32,
    scope: &MonitorScope,
) -> AppResult<EventResponse> {
    let (event, tags) = events_repo::find_by_id_with_tags(state, id as u64)
        .await?
        .ok_or_else(|| event_not_found(id))?;
    // Row-level ACL: hide events of monitors outside the caller's scope.
    if !scope.allows(event.monitor_id, Level::View) {
        return Err(event_not_found(id));
    }
    let tag_summaries: Vec<TagSummary> = tags.iter().map(TagSummary::from).collect();
    let path = crate::service::event_storage::event_directory(state, &event)
        .await
        .ok()
        .map(|p| p.display().to_string());
    let mut response = EventResponse::with_tags(event, tag_summaries);
    response.path = path;
    Ok(response)
}

/// Create a new event
#[instrument(level = "debug", skip(state))]
pub async fn create(
    state: &AppState,
    event: EventCreateRequest,
    scope: &MonitorScope,
) -> AppResult<EventResponse> {
    // Row-level ACL: an event may only be created for a monitor the caller
    // has write access to.
    if !scope.allows(event.monitor_id, Level::Edit) {
        return Err(AppError::NotFoundError(Resource {
            details: vec![("monitor_id".to_string(), event.monitor_id.to_string())],
            resource_type: ResourceType::Monitor,
        }));
    }
    // `Events.StateId` is NOT NULL with no default. zmc records the run
    // state in effect when the event opened, or 0 with none active
    // (zm_event.cpp); without it every create failed.
    let state_id = {
        use crate::entity::states;
        use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};
        states::Entity::find()
            .filter(states::Column::IsActive.eq(1))
            .one(state.db())
            .await?
            .map_or(0, |s| s.id)
    };
    // Convert EventCreateRequest to ActiveModel for database insertion
    let active_event = events::ActiveModel {
        monitor_id: Set(event.monitor_id),
        state_id: Set(state_id),
        storage_id: Set(Some(event.storage_id)),
        secondary_storage_id: Set(event.secondary_storage_id),
        name: Set(event.name),
        cause: Set(event.cause),
        start_date_time: Set(event
            .start_date_time
            .map(|dt| crate::util::utc_to_naive_local(dt.0))),
        end_date_time: Set(event
            .end_date_time
            .map(|dt| crate::util::utc_to_naive_local(dt.0))),
        width: Set(event.width),
        height: Set(event.height),
        length: Set(event.length.0),
        notes: Set(event.notes),
        orientation: Set(event.orientation),
        ..Default::default()
    };

    let saved_event = events_repo::create(state, active_event).await?;
    Ok(EventResponse::from(saved_event))
}

/// Update an existing event
#[instrument(level = "debug", skip(state))]
pub async fn update(
    state: &AppState,
    id: u32,
    event_update: EventUpdateRequest,
    scope: &MonitorScope,
) -> AppResult<EventResponse> {
    let event = events_repo::find_by_id(state, id as u64)
        .await?
        .ok_or_else(|| event_not_found(id))?;
    // Row-level ACL: writing an event requires write access to its monitor.
    if !scope.allows(event.monitor_id, Level::Edit) {
        return Err(event_not_found(id));
    }
    let mut active_event = events::ActiveModel::from(event);

    // Only update fields that are present in the request
    if let Some(name) = event_update.name {
        active_event.name = Set(name);
    }

    if let Some(cause) = event_update.cause {
        active_event.cause = Set(Some(cause));
    }

    if let Some(notes) = event_update.notes {
        active_event.notes = Set(Some(notes));
    }

    if let Some(orientation) = event_update.orientation {
        active_event.orientation = Set(orientation);
    }

    if let Some(archived) = event_update.archived {
        active_event.archived = Set(if archived { 1 } else { 0 });
    }

    if let Some(locked) = event_update.locked {
        active_event.locked = Set(if locked { 1 } else { 0 });
    }

    if let Some(emailed) = event_update.emailed {
        active_event.emailed = Set(if emailed { 1 } else { 0 });
    }

    if let Some(messaged) = event_update.messaged {
        active_event.messaged = Set(if messaged { 1 } else { 0 });
    }

    if let Some(uploaded) = event_update.uploaded {
        active_event.uploaded = Set(if uploaded { 1 } else { 0 });
    }

    if let Some(executed) = event_update.executed {
        active_event.executed = Set(if executed { 1 } else { 0 });
    }

    let updated_event = events_repo::update(state, active_event).await?;
    Ok(EventResponse::from(updated_event))
}

/// Delete an event, including its on-disk media.
///
/// The database row is removed first (it is the authoritative record of the
/// event's existence), then the media directory is cleaned up best-effort. A
/// filesystem failure is logged but does not fail the request — leaving a rare
/// orphaned directory is preferable to reporting a delete failure after the row
/// is already gone. Previously the media was never removed, so every deletion
/// leaked the recorded video and frames on disk.
#[instrument(level = "debug", skip(state))]
pub async fn delete(state: &AppState, id: u32, scope: &MonitorScope) -> AppResult<()> {
    // Fetch first so the event's monitor can be checked against the ACL and its
    // on-disk location resolved from its own fields.
    let event = events_repo::find_by_id(state, id as u64)
        .await?
        .ok_or_else(|| event_not_found(id))?;
    if !scope.allows(event.monitor_id, Level::Edit) {
        return Err(event_not_found(id));
    }
    // Remove the event and its child rows (frames, per-period stats) atomically,
    // then the on-disk media — the same delete the retention reaper performs.
    events_repo::delete_with_children(state.db(), id as u64).await?;
    crate::service::event_storage::delete_event_media(state, &event).await?;
    Ok(())
}

/// The longest window the counts routes take: ten years.
pub const MAX_COUNT_HOURS: i64 = 87_600;

/// `hours` for a counts route, or 400. Out of range used to overflow the time
/// arithmetic and panic the request.
fn check_count_hours(hours: i64) -> AppResult<()> {
    if (1..=MAX_COUNT_HOURS).contains(&hours) {
        Ok(())
    } else {
        Err(AppError::BadRequestError(format!(
            "hours must be between 1 and {MAX_COUNT_HOURS}"
        )))
    }
}

/// Get event counts for the last n hours (grouped by hour)
#[instrument(level = "debug", skip(state))]
pub async fn get_event_counts(
    state: &AppState,
    hours: i64,
    monitor_id: Option<u32>,
    scope: &MonitorScope,
) -> AppResult<EventCountsResponse> {
    check_count_hours(hours)?;
    // One monitor's histogram (#36), or every monitor the caller can see. A
    // monitor outside the caller's scope counts nothing.
    let filter = match monitor_id {
        Some(id) if scope.allows(id, Level::View) => Some(vec![id]),
        Some(_) => Some(Vec::new()),
        None => scope.visible_ids(Level::View),
    };
    let counts = events_repo::get_counts_by_hour(state, hours, filter.as_deref()).await?;

    let event_counts = counts
        .into_iter()
        .map(|(date, count)| EventCountResponse {
            count,
            date: crate::dto::wrappers::NaiveDateTimeWrapper(date),
        })
        .collect();

    Ok(EventCountsResponse {
        counts: event_counts,
        hours,
    })
}

/// Get event counts for the last n hours (grouped by monitor)
#[instrument(level = "debug", skip(state))]
pub async fn get_event_counts_by_monitor(
    state: &AppState,
    hours: i64,
    archived: Option<bool>,
    scope: &MonitorScope,
) -> AppResult<crate::dto::response::events::EventCountsByMonitorResponse> {
    use crate::dto::response::events::{EventCountsByMonitorResponse, MonitorEventCount};

    check_count_hours(hours)?;
    let filter = scope.visible_ids(Level::View);
    let counts =
        events_repo::get_counts_by_monitor(state, hours, archived, filter.as_deref()).await?;

    let monitor_counts = counts
        .into_iter()
        .map(|(monitor_id, count)| MonitorEventCount { monitor_id, count })
        .collect();

    Ok(EventCountsByMonitorResponse {
        counts: monitor_counts,
        hours,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entity::events::Model as EventModel;
    use chrono::DateTime;
    use rust_decimal::Decimal;
    use sea_orm::{DatabaseBackend, MockDatabase, MockExecResult};

    fn mk_event(id: u64, name: &str) -> EventModel {
        use crate::entity::sea_orm_active_enums::{Orientation, Scheme};
        EventModel {
            id,
            monitor_id: 1,
            storage_id: Some(1),
            secondary_storage_id: None,
            name: name.into(),
            cause: None,
            start_date_time: DateTime::from_timestamp(1_700_000_000, 0).map(|dt| dt.naive_utc()),
            end_date_time: None,
            width: 1920,
            height: 1080,
            length: Decimal::new(0, 0),
            frames: Some(0),
            alarm_frames: Some(0),
            default_video: "v.mp4".into(),
            save_jpe_gs: None,
            tot_score: 0,
            avg_score: None,
            max_score: None,
            max_score_frame_id: None,
            archived: 0,
            videoed: 0,
            uploaded: 0,
            emailed: 0,
            messaged: 0,
            executed: 0,
            notes: None,
            state_id: 1,
            orientation: Orientation::Rotate0,
            disk_space: None,
            scheme: Scheme::Deep,
            locked: 0,
            latitude: None,
            longitude: None,
        }
    }

    #[tokio::test]
    async fn test_get_by_id_ok_and_not_found() {
        use crate::entity::events_tags::Model as EventTagModel;

        // Mock needs results for: 1) find event, 2) find event_tags associations
        let empty_event_tags: Vec<EventTagModel> = vec![];
        let db_ok = MockDatabase::new(DatabaseBackend::MySql)
            .append_query_results::<EventModel, _, _>(vec![vec![mk_event(5, "ev")]])
            .append_query_results::<EventTagModel, _, _>(vec![empty_event_tags])
            .into_connection();
        let state_ok = AppState::for_test_with_db(db_ok);
        assert_eq!(
            get_by_id(&state_ok, 5, &MonitorScope::All)
                .await
                .unwrap()
                .id,
            5
        );

        let empty: Vec<EventModel> = vec![];
        let db_none = MockDatabase::new(DatabaseBackend::MySql)
            .append_query_results::<EventModel, _, _>(vec![empty])
            .into_connection();
        let state_none = AppState::for_test_with_db(db_none);
        assert!(matches!(
            get_by_id(&state_none, 1, &MonitorScope::All)
                .await
                .err()
                .unwrap(),
            AppError::NotFoundError(_)
        ));
    }

    #[tokio::test]
    async fn test_create_update_delete_ok() {
        use crate::dto::request::events::{EventCreateRequest, EventUpdateRequest};
        use crate::dto::wrappers::DecimalWrapper;
        use crate::entity::sea_orm_active_enums::Orientation;
        // Create: no active run state (StateId 0), then the insert.
        let no_state: Vec<crate::entity::states::Model> = vec![];
        let db_create = MockDatabase::new(DatabaseBackend::MySql)
            .append_query_results(vec![no_state])
            .append_exec_results(vec![MockExecResult {
                last_insert_id: 100,
                rows_affected: 1,
            }])
            .append_query_results::<EventModel, _, _>(vec![vec![mk_event(100, "name")]])
            .into_connection();
        let state_create = AppState::for_test_with_db(db_create);
        let req = EventCreateRequest {
            monitor_id: 1,
            storage_id: 1,
            secondary_storage_id: None,
            name: "name".into(),
            cause: None,
            start_date_time: None,
            end_date_time: None,
            width: 1280,
            height: 720,
            length: DecimalWrapper(Decimal::new(0, 0)),
            notes: None,
            orientation: Orientation::Rotate0,
        };
        let out = create(&state_create, req, &MonitorScope::All)
            .await
            .unwrap();
        assert_eq!(out.name, "name");

        // Update: first find_by_id returns a model; then exec + query returns updated
        let initial = mk_event(7, "old");
        let mut after = initial.clone();
        after.name = "new".into();
        let db_update = MockDatabase::new(DatabaseBackend::MySql)
            .append_query_results::<EventModel, _, _>(vec![vec![initial]])
            .append_exec_results(vec![MockExecResult {
                last_insert_id: 0,
                rows_affected: 1,
            }])
            .append_query_results::<EventModel, _, _>(vec![vec![after.clone()]])
            .into_connection();
        let state_update = AppState::for_test_with_db(db_update);
        let out_upd = update(
            &state_update,
            7,
            EventUpdateRequest {
                name: Some("new".into()),
                cause: None,
                notes: None,
                orientation: None,
                archived: None,
                locked: None,
                emailed: None,
                messaged: None,
                uploaded: None,
                executed: None,
            },
            &MonitorScope::All,
        )
        .await
        .unwrap();
        assert_eq!(out_upd.name, "new");

        // Delete — the service fetches the event first to ACL-check its
        // monitor, deletes the row, then resolves storage to remove media. The
        // storage row points at an empty tempdir, so the media cleanup finds no
        // directory (a no-op) and nothing on disk is touched.
        let tmp = tempfile::tempdir().unwrap();
        let storage = crate::entity::storage::Model {
            id: 1,
            path: tmp.path().to_string_lossy().into_owned(),
            name: "store".into(),
            r#type: crate::entity::sea_orm_active_enums::StorageType::Local,
            url: None,
            disk_space: None,
            scheme: crate::entity::sea_orm_active_enums::Scheme::Deep,
            server_id: None,
            do_delete: 1,
            enabled: 1,
        };
        // delete_with_children runs a transaction: 7 child-table deletes
        // (Frames + Stats + Events_Hour/Day/Week/Month + Events_Archived) then
        // the event delete — 8 exec results.
        let db_del = MockDatabase::new(DatabaseBackend::MySql)
            .append_query_results::<EventModel, _, _>(vec![vec![mk_event(7, "old")]])
            .append_exec_results(vec![
                MockExecResult {
                    last_insert_id: 0,
                    rows_affected: 1,
                };
                8
            ])
            .append_query_results::<crate::entity::storage::Model, _, _>(vec![vec![storage]])
            .into_connection();
        let state_del = AppState::for_test_with_db(db_del);
        assert!(delete(&state_del, 7, &MonitorScope::All).await.is_ok());
    }
}
