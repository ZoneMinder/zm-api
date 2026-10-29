use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    Json,
};
// use chrono::Utc; // not needed in this module
use garde::Validate;
use tracing::{debug, instrument};

use crate::{
    dto::{
        request::events::{
            EventCountsByMonitorQuery, EventCreateRequest, EventQueryParams, EventUpdateRequest,
        },
        response::events::{
            EventCountsByMonitorResponse, EventCountsResponse, EventResponse,
            PaginatedEventsResponse,
        },
    },
    error::{AppError, AppResponseError, AppResult},
    server::state::AppState,
    service,
    service::monitor_acl::MonitorScope,
};

/// Get a paginated list of events
#[utoipa::path(
    get,
    path = "/api/v3/events",
    operation_id = "listEvents",
    tag = "Events",
    params(
        ("page" = Option<u64>, Query, description = "Page number (1-indexed)", example = 1),
        ("page_size" = Option<u64>, Query, description = "Number of items per page", example = 20),
        ("monitor_id" = Option<u32>, Query, description = "Filter by monitor ID", example = 1),
        ("start_time" = Option<String>, Query, description = "Filter by start time (ISO8601)", example = "2025-04-28T00:00:00Z"),
        ("end_time" = Option<String>, Query, description = "Filter by end time (ISO8601)", example = "2025-04-29T23:59:59Z"),
        ("sort" = Option<String>, Query, description = "Field to sort by: start_time, end_time, alarm_frames, max_score, avg_score, tot_score, length, id, name, cause, monitor_id, notes, frames, archived, emailed, disk_space", example = "start_time"),
        ("direction" = Option<String>, Query, description = "Sort direction: asc or desc", example = "desc"),
        ("alarm_frames_min" = Option<u32>, Query, description = "Minimum number of alarm frames", example = 5),
        ("archived" = Option<bool>, Query, description = "Filter by archived status", example = false),
        ("cause" = Option<String>, Query, description = "Substring match on cause", example = "Motion"),
        ("name" = Option<String>, Query, description = "Substring match on name", example = "Front"),
        ("notes" = Option<String>, Query, description = "Substring match on notes"),
        ("q" = Option<String>, Query, description = "Free-text search over id, monitor, storage, name, cause, start/end time, length, frames, scores, archived, emailed, notes and disk space; any may match"),
        ("tag_id" = Option<String>, Query, description = "Events with any of these tag ids (comma-separated), `none` for untagged events, `any` for events with at least one tag. Legacy's `0` and `-1` mean `none` and `any`.", example = "1,4"),
        ("tag_op" = Option<String>, Query, description = "`in` (default) keeps what tag_id matches, `not_in` keeps the rest", example = "in"),
        ("totals" = Option<bool>, Query, description = "Also return total_length_seconds and total_disk_space summed over the whole result. Costs a second aggregate query."),
        ("storage_id" = Option<u16>, Query, description = "Filter by the storage holding the event media", example = 1)
    ),
    responses(
        (status = 200, description = "List of events", body = PaginatedEventsResponse),
        (status = 400, description = "Bad request", body = AppResponseError),
        (status = 401, description = "Unauthorized", body = AppResponseError),
        (status = 500, description = "Internal server error", body = AppResponseError)
    ),
    security(
        ("jwt" = [])
    )
)]
#[instrument(level = "debug", skip(state, scope))]
pub async fn list_events(
    State(state): State<AppState>,
    Query(params): Query<EventQueryParams>,
    scope: MonitorScope,
) -> AppResult<Json<PaginatedEventsResponse>> {
    params.validate().map_err(AppError::InvalidInputError)?;
    debug!("Listing events with params: {:?}", params);

    let events = service::events::list(&state, &params, &scope).await?;

    Ok(Json(events))
}

/// Get a specific event by ID
#[utoipa::path(
    get,
    path = "/api/v3/events/{id}",
    operation_id = "getEvent",
    tag = "Events",
    params(
        ("id" = u32, Path, description = "Event ID")
    ),
    responses(
        (status = 200, description = "Event details", body = EventResponse),
        (status = 404, description = "Event not found", body = AppResponseError),
        (status = 401, description = "Unauthorized", body = AppResponseError),
        (status = 500, description = "Internal server error", body = AppResponseError)
    ),
    security(
        ("jwt" = [])
    )
)]
#[instrument(level = "debug", skip(state, scope))]
pub async fn get_event(
    State(state): State<AppState>,
    Path(id): Path<u32>,
    scope: MonitorScope,
) -> AppResult<Json<EventResponse>> {
    let event = service::events::get_by_id(&state, id, &scope).await?;

    Ok(Json(event))
}

/// Create a new event
#[utoipa::path(
    post,
    path = "/api/v3/events",
    operation_id = "createEvent",
    tag = "Events",
    request_body = EventCreateRequest,
    responses(
        (status = 201, description = "Event created", body = EventResponse),
        (status = 400, description = "Invalid request", body = AppResponseError),
        (status = 401, description = "Unauthorized", body = AppResponseError),
        (status = 500, description = "Internal server error", body = AppResponseError)
    ),
    security(
        ("jwt" = [])
    )
)]
#[instrument(level = "debug", skip(state, event, scope))]
pub async fn create_event(
    State(state): State<AppState>,
    scope: MonitorScope,
    Json(event): Json<EventCreateRequest>,
) -> AppResult<(StatusCode, Json<EventResponse>)> {
    event.validate().map_err(AppError::InvalidInputError)?;

    let new_event = service::events::create(&state, event, &scope).await?;

    Ok((StatusCode::CREATED, Json(new_event)))
}

/// Update an existing event
#[utoipa::path(
    patch,
    path = "/api/v3/events/{id}",
    operation_id = "updateEvent",
    tag = "Events",
    params(
        ("id" = u32, Path, description = "Event ID")
    ),
    request_body = EventUpdateRequest,
    responses(
        (status = 200, description = "Event updated", body = EventResponse),
        (status = 404, description = "Event not found", body = AppResponseError),
        (status = 400, description = "Invalid request", body = AppResponseError),
        (status = 401, description = "Unauthorized", body = AppResponseError),
        (status = 500, description = "Internal server error", body = AppResponseError)
    ),
    security(
        ("jwt" = [])
    )
)]
#[instrument(level = "debug", skip(state, event_update, scope))]
pub async fn update_event(
    State(state): State<AppState>,
    Path(id): Path<u32>,
    scope: MonitorScope,
    Json(event_update): Json<EventUpdateRequest>,
) -> AppResult<Json<EventResponse>> {
    event_update
        .validate()
        .map_err(AppError::InvalidInputError)?;

    let updated_event = service::events::update(&state, id, event_update, &scope).await?;

    Ok(Json(updated_event))
}

/// Delete an event
#[utoipa::path(
    delete,
    path = "/api/v3/events/{id}",
    operation_id = "deleteEvent",
    tag = "Events",
    params(
        ("id" = u32, Path, description = "Event ID")
    ),
    responses(
        (status = 204, description = "Event deleted"),
        (status = 401, description = "Unauthorized", body = AppResponseError),
        (status = 500, description = "Internal server error", body = AppResponseError)
    ),
    security(
        ("jwt" = [])
    )
)]
#[instrument(level = "debug", skip(state, scope))]
pub async fn delete_event(
    State(state): State<AppState>,
    Path(id): Path<u32>,
    scope: MonitorScope,
) -> AppResult<StatusCode> {
    service::events::delete(&state, id, &scope).await?;

    Ok(StatusCode::NO_CONTENT)
}

/// `GET /events/counts/{hours}` query.
#[derive(Debug, serde::Deserialize)]
pub struct EventCountsQuery {
    pub monitor_id: Option<u32>,
}

/// Get event counts grouped by hour
#[utoipa::path(
    get,
    path = "/api/v3/events/counts/{hours}",
    operation_id = "getEventCounts",
    tag = "Events",
    params(
        ("hours" = i64, Path, description = "Number of hours to count back from now"),
        ("monitor_id" = Option<u32>, Query, description = "Count only this monitor's events (a per-monitor hourly histogram)")
    ),
    responses(
        (status = 200, description = "Event counts grouped by hour", body = EventCountsResponse),
        (status = 400, description = "Bad request", body = AppResponseError),
        (status = 401, description = "Unauthorized", body = AppResponseError),
        (status = 500, description = "Internal server error", body = AppResponseError)
    ),
    security(
        ("jwt" = [])
    )
)]
#[instrument(level = "debug", skip(state, scope))]
pub async fn get_event_counts(
    State(state): State<AppState>,
    Path(hours): Path<i64>,
    Query(query): Query<EventCountsQuery>,
    scope: MonitorScope,
) -> AppResult<Json<EventCountsResponse>> {
    let counts = service::events::get_event_counts(&state, hours, query.monitor_id, &scope).await?;

    Ok(Json(counts))
}

/// Get event counts grouped by monitor (for console view)
#[utoipa::path(
    get,
    path = "/api/v3/events/counts-by-monitor/{hours}",
    operation_id = "getEventCountsByMonitor",
    tag = "Events",
    params(
        ("hours" = i64, Path, description = "Number of hours to count back from now"),
        EventCountsByMonitorQuery
    ),
    responses(
        (status = 200, description = "Event counts grouped by monitor", body = EventCountsByMonitorResponse),
        (status = 400, description = "Bad request", body = AppResponseError),
        (status = 401, description = "Unauthorized", body = AppResponseError),
        (status = 500, description = "Internal server error", body = AppResponseError)
    ),
    security(
        ("jwt" = [])
    )
)]
#[instrument(level = "debug", skip(state, scope))]
pub async fn get_event_counts_by_monitor(
    State(state): State<AppState>,
    Path(hours): Path<i64>,
    Query(query): Query<EventCountsByMonitorQuery>,
    scope: MonitorScope,
) -> AppResult<Json<EventCountsByMonitorResponse>> {
    let counts =
        service::events::get_event_counts_by_monitor(&state, hours, query.archived, &scope).await?;

    Ok(Json(counts))
}
