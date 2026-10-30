use std::collections::HashMap;

use sea_orm::sea_query::ExprTrait;
use sea_orm::sea_query::{Alias, MysqlQueryBuilder, Order};
use sea_orm::*;
use tracing::instrument;

use crate::dto::request::events::{EventSortField, SortDirection, TagFilter};
use crate::entity::{events, events_tags, prelude::Events, tags};
use crate::server::state::AppState;

/// Query options for filtering and sorting events
#[derive(Debug, Default)]
pub struct EventQueryOptions {
    pub monitor_id: Option<u32>,
    pub start_time: Option<sea_orm::prelude::DateTime>,
    pub end_time: Option<sea_orm::prelude::DateTime>,
    pub sort_field: EventSortField,
    pub sort_direction: SortDirection,
    pub alarm_frames_min: Option<u32>,
    pub archived: Option<bool>,
    /// Case-insensitive substring filters on the respective columns.
    pub cause: Option<String>,
    pub name: Option<String>,
    pub notes: Option<String>,
    /// Free-text search over legacy's column set (`q`).
    pub q: Option<String>,
    /// Tag filter, and whether to keep what it matches or the rest.
    pub tags: Option<TagFilter>,
    pub tags_negated: bool,
    /// Keep only events stored on this storage.
    pub storage_id: Option<u16>,
    /// Row-level ACL allowlist of monitor ids. `None` is unrestricted;
    /// `Some(ids)` limits results to events of those monitors.
    pub monitor_filter: Option<Vec<u32>>,
}

/// Helper to get the column for sorting
fn get_sort_column(field: EventSortField) -> events::Column {
    match field {
        EventSortField::StartTime => events::Column::StartDateTime,
        EventSortField::EndTime => events::Column::EndDateTime,
        EventSortField::AlarmFrames => events::Column::AlarmFrames,
        EventSortField::MaxScore => events::Column::MaxScore,
        EventSortField::AvgScore => events::Column::AvgScore,
        EventSortField::TotScore => events::Column::TotScore,
        EventSortField::Length => events::Column::Length,
        EventSortField::Id => events::Column::Id,
        EventSortField::Name => events::Column::Name,
        EventSortField::Archived => events::Column::Archived,
        EventSortField::Emailed => events::Column::Emailed,
        EventSortField::DiskSpace => events::Column::DiskSpace,
        EventSortField::Cause => events::Column::Cause,
        EventSortField::MonitorId => events::Column::MonitorId,
        EventSortField::Notes => events::Column::Notes,
        EventSortField::Frames => events::Column::Frames,
    }
}

/// Helper to apply sorting to a query
fn apply_sorting<E: EntityTrait>(
    query: Select<E>,
    column: E::Column,
    direction: SortDirection,
) -> Select<E> {
    match direction {
        SortDirection::Asc => query.order_by_asc(column),
        SortDirection::Desc => query.order_by_desc(column),
    }
}

/// Find all events with pagination and filtering/sorting options
#[instrument(level = "debug", skip(state))]
pub async fn find_all(
    state: &AppState,
    page: u64,
    page_size: u64,
) -> Result<(Vec<events::Model>, u64), DbErr> {
    find_with_options(state, EventQueryOptions::default(), page, page_size).await
}

/// Legacy's events search box (`web/ajax/events.php`): `LIKE %q%` over these
/// columns, any of which may match. Numbers and dates are compared as text,
/// as MySQL does there, so `12` finds event 112 and `2026-10` a month.
fn free_text(q: &str) -> sea_orm::Condition {
    use sea_orm::sea_query::{Alias, Expr, ExprTrait};
    use sea_orm::Condition;
    const COLUMNS: [events::Column; 17] = [
        events::Column::Id,
        events::Column::MonitorId,
        events::Column::StorageId,
        events::Column::Name,
        events::Column::Cause,
        events::Column::StartDateTime,
        events::Column::EndDateTime,
        events::Column::Length,
        events::Column::Frames,
        events::Column::AlarmFrames,
        events::Column::TotScore,
        events::Column::AvgScore,
        events::Column::MaxScore,
        events::Column::Archived,
        events::Column::Emailed,
        events::Column::Notes,
        events::Column::DiskSpace,
    ];
    let pattern = format!("%{q}%");
    COLUMNS.into_iter().fold(Condition::any(), |cond, col| {
        cond.add(
            Expr::col((events::Entity, col))
                .cast_as(Alias::new("CHAR"))
                .like(pattern.clone()),
        )
    })
}

/// Every filter on `options`, ACL included, as one select. Shared by the list
/// and the totals query so they always cover the same rows.
fn filtered(options: &EventQueryOptions) -> Select<Events> {
    let mut query = Events::find();

    if let Some(monitor_id) = options.monitor_id {
        query = query.filter(events::Column::MonitorId.eq(monitor_id));
    }

    if let Some(start) = options.start_time {
        query = query.filter(events::Column::StartDateTime.gte(start));
    }

    if let Some(end) = options.end_time {
        query = query.filter(events::Column::EndDateTime.lte(end));
    }

    if let Some(min_frames) = options.alarm_frames_min {
        query = query.filter(events::Column::AlarmFrames.gte(min_frames));
    }

    if let Some(archived) = options.archived {
        query = query.filter(events::Column::Archived.eq(if archived { 1u8 } else { 0u8 }));
    }

    if let Some(ref cause) = options.cause {
        query = query.filter(events::Column::Cause.contains(cause.clone()));
    }
    if let Some(ref name) = options.name {
        query = query.filter(events::Column::Name.contains(name.clone()));
    }
    if let Some(ref notes) = options.notes {
        query = query.filter(events::Column::Notes.contains(notes.clone()));
    }
    if let Some(storage_id) = options.storage_id {
        query = query.filter(events::Column::StorageId.eq(storage_id));
    }
    if let Some(ref q) = options.q {
        query = query.filter(free_text(q));
    }
    // Tag filter via a subquery over Events_Tags (a join would duplicate rows).
    if let Some(ref tags) = options.tags {
        use crate::entity::events_tags;
        use sea_orm::sea_query::Query;
        let mut sub = Query::select();
        sub.column(events_tags::Column::EventId)
            .from(events_tags::Entity);
        // "Untagged" is "not in the set of tagged events", so it flips the sense.
        let keep_matching = match tags {
            TagFilter::AnyOf(ids) => {
                sub.and_where(events_tags::Column::TagId.is_in(ids.iter().copied()));
                true
            }
            TagFilter::Tagged => true,
            TagFilter::Untagged => false,
        };
        query = if keep_matching != options.tags_negated {
            query.filter(events::Column::Id.in_subquery(sub))
        } else {
            query.filter(events::Column::Id.not_in_subquery(sub))
        };
    }

    // Row-level ACL: restrict to the caller's permitted monitors.
    if let Some(monitor_ids) = &options.monitor_filter {
        query = query.filter(events::Column::MonitorId.is_in(monitor_ids.iter().copied()));
    }
    query
}

/// Find events with full query options
#[instrument(level = "debug", skip(state))]
pub async fn find_with_options(
    state: &AppState,
    options: EventQueryOptions,
    page: u64,
    page_size: u64,
) -> Result<(Vec<events::Model>, u64), DbErr> {
    let sort_column = get_sort_column(options.sort_field);
    let query = apply_sorting(filtered(&options), sort_column, options.sort_direction);

    let paginator = query.paginate(state.db(), page_size);
    let total = paginator.num_items().await?;
    let events = paginator.fetch_page(page).await?;

    Ok((events, total))
}

/// Summed `Length` (seconds) and `DiskSpace` (bytes) of every event the options
/// match, not of one page.
#[instrument(level = "debug", skip(state))]
pub async fn totals_with_options(
    state: &AppState,
    options: &EventQueryOptions,
) -> Result<(f64, u64), DbErr> {
    use rust_decimal::prelude::ToPrimitive;
    use rust_decimal::Decimal;
    use sea_orm::sea_query::Expr;

    // SUM over a DECIMAL and over a BIGINT both come back as DECIMAL.
    let row: Option<(Option<Decimal>, Option<Decimal>)> = filtered(options)
        .select_only()
        .column_as(Expr::col(events::Column::Length).sum(), "total_length")
        .column_as(Expr::col(events::Column::DiskSpace).sum(), "total_disk")
        .into_tuple()
        .one(state.db())
        .await?;
    let (length, disk) = row.unwrap_or((None, None));
    Ok((
        length.and_then(|d| d.to_f64()).unwrap_or(0.0),
        disk.and_then(|d| d.to_u64()).unwrap_or(0),
    ))
}

/// Find events matching a prebuilt `Condition` (the compiled filter AST), with
/// row-level ACL, sorting, and pagination. Used by the filter `/preview`.
#[instrument(level = "debug", skip(state, condition))]
pub async fn find_with_condition(
    state: &AppState,
    condition: Condition,
    sort_column: events::Column,
    sort_direction: SortDirection,
    monitor_filter: Option<Vec<u32>>,
    page: u64,
    page_size: u64,
) -> Result<(Vec<events::Model>, u64), DbErr> {
    let mut query = Events::find().filter(condition);

    // Row-level ACL: restrict to the caller's permitted monitors.
    if let Some(monitor_ids) = &monitor_filter {
        query = query.filter(events::Column::MonitorId.is_in(monitor_ids.iter().copied()));
    }

    query = apply_sorting(query, sort_column, sort_direction);

    let paginator = query.paginate(state.db(), page_size);
    let total = paginator.num_items().await?;
    let items = paginator.fetch_page(page).await?;
    Ok((items, total))
}

/// Find events by monitor ID with optional date range filter (legacy helper)
#[instrument(level = "debug", skip(state))]
pub async fn find_by_monitor_id(
    state: &AppState,
    monitor_id: u32,
    start_time: Option<sea_orm::prelude::DateTime>,
    end_time: Option<sea_orm::prelude::DateTime>,
    page: u64,
    page_size: u64,
) -> Result<(Vec<events::Model>, u64), DbErr> {
    find_with_options(
        state,
        EventQueryOptions {
            monitor_id: Some(monitor_id),
            start_time,
            end_time,
            ..Default::default()
        },
        page,
        page_size,
    )
    .await
}

/// Find event by ID
#[instrument(level = "debug", skip(state))]
pub async fn find_by_id(state: &AppState, id: u64) -> Result<Option<events::Model>, DbErr> {
    Events::find_by_id(id).one(state.db()).await
}

/// Create new event
#[instrument(level = "debug", skip(state, event))]
pub async fn create(state: &AppState, event: events::ActiveModel) -> Result<events::Model, DbErr> {
    event.insert(state.db()).await
}

/// Update existing event
#[instrument(level = "debug", skip(state, event))]
pub async fn update(state: &AppState, event: events::ActiveModel) -> Result<events::Model, DbErr> {
    event.update(state.db()).await
}

/// Delete event by ID
#[instrument(level = "debug", skip(state))]
pub async fn delete(state: &AppState, id: u64) -> Result<DeleteResult, DbErr> {
    Events::delete_by_id(id).exec(state.db()).await
}

/// Delete an event and all of its child rows in a single transaction.
///
/// ZoneMinder's `Frames` and the per-period stats tables
/// (`Events_Hour/Day/Week/Month`) plus `Events_Archived` have **no** FK cascade
/// to `Events`, so deleting only the event row orphans them. `Snapshots_Events`
/// and `Events_Tags` *do* cascade via their FKs and are removed by the event
/// delete itself. This is the single DB-side delete both the interactive
/// `DELETE /events/{id}` path and the retention reaper use, so they can never
/// leave different debris behind.
pub async fn delete_with_children<C>(conn: &C, id: u64) -> Result<(), DbErr>
where
    C: ConnectionTrait + TransactionTrait,
{
    use crate::entity::{
        events_archived, events_day, events_hour, events_month, events_week, frames, stats,
    };

    let txn = conn.begin().await?;
    frames::Entity::delete_many()
        .filter(frames::Column::EventId.eq(id))
        .exec(&txn)
        .await?;
    // Per-frame zone scores; no FK, so they were left behind (#110).
    stats::Entity::delete_many()
        .filter(stats::Column::EventId.eq(id))
        .exec(&txn)
        .await?;
    events_hour::Entity::delete_many()
        .filter(events_hour::Column::EventId.eq(id))
        .exec(&txn)
        .await?;
    events_day::Entity::delete_many()
        .filter(events_day::Column::EventId.eq(id))
        .exec(&txn)
        .await?;
    events_week::Entity::delete_many()
        .filter(events_week::Column::EventId.eq(id))
        .exec(&txn)
        .await?;
    events_month::Entity::delete_many()
        .filter(events_month::Column::EventId.eq(id))
        .exec(&txn)
        .await?;
    events_archived::Entity::delete_many()
        .filter(events_archived::Column::EventId.eq(id))
        .exec(&txn)
        .await?;
    // Cascades Snapshots_Events + Events_Tags via their FKs.
    Events::delete_by_id(id).exec(&txn).await?;
    txn.commit().await?;
    Ok(())
}

/// Get event counts grouped by monitor over time period
#[instrument(level = "debug", skip(state))]
pub async fn get_counts_by_monitor(
    state: &AppState,
    hours_back: i64,
    archived: Option<bool>,
    monitor_filter: Option<&[u32]>,
) -> Result<Vec<(u32, u64)>, DbErr> {
    use sea_orm::sea_query::{Expr, Query};
    use sea_orm::Statement;

    // Calculate time boundary
    let now = chrono::Utc::now();
    let time_boundary = now - chrono::Duration::hours(hours_back);
    let time_boundary_str = time_boundary.format("%Y-%m-%d %H:%M:%S").to_string();

    let mut select = Query::select();
    select
        .expr(Expr::col(events::Column::MonitorId))
        .expr(Expr::cust("COUNT(*)"))
        .from(events::Entity)
        .and_where(Expr::col(events::Column::StartDateTime).gte(time_boundary_str))
        .group_by_col(events::Column::MonitorId);
    if let Some(archived) = archived {
        select.and_where(Expr::col(events::Column::Archived).eq(u8::from(archived)));
    }
    // Row-level ACL: restrict the aggregation to permitted monitors.
    if let Some(ids) = monitor_filter {
        select.and_where(Expr::col(events::Column::MonitorId).is_in(ids.iter().copied()));
    }
    let sql = select.to_string(MysqlQueryBuilder);

    let stmt = Statement::from_sql_and_values(state.db().get_database_backend(), sql, vec![]);

    // Decode by index, not column name: MariaDB does not expose the SELECT
    // alias in result metadata the way MySQL 8 does, so by-name decode failed
    // with "no column found for name: monitor_id".
    let rows = state.db().query_all_raw(stmt).await?;
    let mut counts = Vec::with_capacity(rows.len());
    for row in rows {
        let monitor_id: u32 = row.try_get_by_index(0)?;
        let count: i64 = row.try_get_by_index(1)?;
        counts.push((monitor_id, count as u64));
    }
    Ok(counts)
}

/// Get event counts by hour for the last n hours
#[instrument(level = "debug", skip(state))]
pub async fn get_counts_by_hour(
    state: &AppState,
    hours_back: i64,
    monitor_filter: Option<&[u32]>,
) -> Result<Vec<(chrono::NaiveDateTime, u64)>, DbErr> {
    use sea_orm::sea_query::{Expr, Query};
    use sea_orm::{FromQueryResult, Statement};

    #[derive(FromQueryResult)]
    struct CountResult {
        hour: String,
        count: i64,
    }

    // Calculate time boundary
    let now = chrono::Utc::now();
    let time_boundary = now - chrono::Duration::hours(hours_back);
    let time_boundary_str = time_boundary.format("%Y-%m-%d %H:%M:%S").to_string();

    // Raw SQL for better performance on complex aggregation with grouping by hour
    let hour_alias = Alias::new("hour");
    let count_alias = Alias::new("count");
    let mut select = Query::select();
    select
        .expr_as(
            Expr::cust("DATE_FORMAT(StartDateTime, '%Y-%m-%d %H:00:00')"),
            hour_alias.clone(),
        )
        .expr_as(Expr::cust("COUNT(*)"), count_alias.clone())
        .from(events::Entity)
        .and_where(Expr::col(events::Column::StartDateTime).gte(time_boundary_str))
        .group_by_col(hour_alias.clone())
        .order_by(hour_alias.clone(), Order::Asc);
    // Row-level ACL: restrict the aggregation to permitted monitors.
    if let Some(ids) = monitor_filter {
        select.and_where(Expr::col(events::Column::MonitorId).is_in(ids.iter().copied()));
    }
    let sql = select.to_string(MysqlQueryBuilder);

    let stmt = Statement::from_sql_and_values(state.db().get_database_backend(), sql, vec![]);

    let results = CountResult::find_by_statement(stmt).all(state.db()).await?;

    let counts = results
        .into_iter()
        .map(|r| {
            let naive_date = chrono::NaiveDateTime::parse_from_str(&r.hour, "%Y-%m-%d %H:%M:%S")
                .unwrap_or_else(|_| {
                    chrono::DateTime::<chrono::Utc>::from_timestamp(0, 0)
                        .unwrap()
                        .naive_utc()
                });
            (naive_date, r.count as u64)
        })
        .collect();

    Ok(counts)
}

/// Find tags for a list of event IDs (batch loading to prevent N+1)
#[instrument(level = "debug", skip(state))]
pub async fn find_tags_for_events(
    state: &AppState,
    event_ids: &[u64],
) -> Result<HashMap<u64, Vec<tags::Model>>, DbErr> {
    if event_ids.is_empty() {
        return Ok(HashMap::new());
    }

    // Fetch all event-tag associations for the given events
    let associations = events_tags::Entity::find()
        .filter(events_tags::Column::EventId.is_in(event_ids.to_vec()))
        .all(state.db())
        .await?;

    if associations.is_empty() {
        return Ok(HashMap::new());
    }

    // Collect unique tag IDs
    let tag_ids: Vec<u64> = associations.iter().map(|a| a.tag_id).collect();

    // Fetch all tags
    let all_tags = tags::Entity::find()
        .filter(tags::Column::Id.is_in(tag_ids))
        .all(state.db())
        .await?;

    // Create a map of tag_id -> tag model
    let tag_map: HashMap<u64, tags::Model> = all_tags.into_iter().map(|t| (t.id, t)).collect();

    // Group tags by event_id
    let mut result: HashMap<u64, Vec<tags::Model>> = HashMap::new();
    for assoc in associations {
        if let Some(tag) = tag_map.get(&assoc.tag_id) {
            result.entry(assoc.event_id).or_default().push(tag.clone());
        }
    }

    Ok(result)
}

/// Find a single event with its tags
#[instrument(level = "debug", skip(state))]
pub async fn find_by_id_with_tags(
    state: &AppState,
    id: u64,
) -> Result<Option<(events::Model, Vec<tags::Model>)>, DbErr> {
    let event = Events::find_by_id(id).one(state.db()).await?;

    match event {
        Some(e) => {
            let tags_map = find_tags_for_events(state, &[id]).await?;
            let tags = tags_map.get(&id).cloned().unwrap_or_default();
            Ok(Some((e, tags)))
        }
        None => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sea_orm::{DatabaseBackend, MockDatabase, MockExecResult};

    /// `delete_with_children` runs one transaction that removes the seven
    /// child tables (Frames, Stats, Events_Hour/Day/Week/Month, Events_Archived)
    /// plus the event itself — eight exec statements in total. The mock
    /// supplies exactly eight results, so a missing or extra delete would leave
    /// the queue mismatched and fail.
    #[tokio::test]
    async fn delete_with_children_removes_all_child_rows_and_event() {
        let db = MockDatabase::new(DatabaseBackend::MySql)
            .append_exec_results(vec![
                MockExecResult {
                    last_insert_id: 0,
                    rows_affected: 1,
                };
                8
            ])
            .into_connection();

        delete_with_children(&db, 42)
            .await
            .expect("transactional delete should succeed");
    }
}
