use crate::dto::request::frames::{CreateFrameRequest, UpdateFrameRequest};
use crate::dto::PaginationParams;
use crate::entity::frames::{ActiveModel, Column, Entity as FrameEntity, Model as FrameModel};
use crate::error::AppResult;
use chrono::DateTime;
use sea_orm::{
    ActiveModelTrait, ColumnTrait, DatabaseConnection, EntityTrait, PaginatorTrait, QueryFilter,
    QueryOrder, Set,
};

/// Subquery selecting event ids that belong to an allowlist of monitors.
/// Used to row-level-ACL-filter frames, which link to monitors only via
/// their parent event.
fn events_for_monitors(ids: &[u32]) -> sea_orm::sea_query::SelectStatement {
    use crate::entity::events;
    use sea_orm::sea_query::Query;
    Query::select()
        .column(events::Column::Id)
        .from(events::Entity)
        .and_where(events::Column::MonitorId.is_in(ids.iter().copied()))
        .to_owned()
}

/// Find all frames for a single event.
///
/// `event_id` is REQUIRED (not optional): `Frames` is one of the largest tables
/// in a real ZoneMinder install (tens of millions of rows), so an unscoped
/// `SELECT *` would be a reliability hazard. Callers that need a broad listing
/// must page via [`find_paginated`]. See REVIEW_FIXES_PLAN §4.4.
pub async fn find_all(
    db: &DatabaseConnection,
    event_id: u64,
    monitor_filter: Option<&[u32]>,
) -> AppResult<Vec<FrameModel>> {
    let mut query = FrameEntity::find().filter(Column::EventId.eq(event_id));

    if let Some(ids) = monitor_filter {
        query = query.filter(Column::EventId.in_subquery(events_for_monitors(ids)));
    }

    let frames = query.order_by_asc(Column::FrameId).all(db).await?;
    Ok(frames)
}

/// Find frames with pagination, optionally filtered by event_id
pub async fn find_paginated(
    db: &DatabaseConnection,
    event_id: Option<u64>,
    params: &PaginationParams,
    monitor_filter: Option<&[u32]>,
) -> AppResult<(Vec<FrameModel>, u64)> {
    let mut query = FrameEntity::find();

    if let Some(eid) = event_id {
        query = query.filter(Column::EventId.eq(eid));
    }
    if let Some(ids) = monitor_filter {
        query = query.filter(Column::EventId.in_subquery(events_for_monitors(ids)));
    }

    let paginator = query
        .order_by_asc(Column::FrameId)
        .paginate(db, params.page_size());
    let total = paginator.num_items().await?;
    let items = paginator
        .fetch_page(params.page().saturating_sub(1))
        .await?;
    Ok((items, total))
}

/// Find a frame by its key: the event and the frame number within it.
pub async fn find_by_key(
    db: &DatabaseConnection,
    event_id: u64,
    frame_id: u32,
) -> AppResult<Option<FrameModel>> {
    let frame = FrameEntity::find_by_id((event_id, frame_id))
        .one(db)
        .await?;
    Ok(frame)
}

/// The event's `Frames` row with the highest `FrameId` at or below `frame_id`.
///
/// ZoneMinder writes a row for every frame only while alarmed; in between it
/// writes one "Bulk" row every few frames, so an arbitrary frame number is
/// located from the nearest row before it.
pub async fn find_at_or_before(
    db: &DatabaseConnection,
    event_id: u64,
    frame_id: u32,
) -> AppResult<Option<FrameModel>> {
    let frame = FrameEntity::find()
        .filter(Column::EventId.eq(event_id))
        .filter(Column::FrameId.lte(frame_id))
        .order_by_desc(Column::FrameId)
        .one(db)
        .await?;
    Ok(frame)
}

/// Create a new frame
pub async fn create(db: &DatabaseConnection, req: &CreateFrameRequest) -> AppResult<FrameModel> {
    // Parse timestamp
    let time_stamp = DateTime::parse_from_rfc3339(&req.time_stamp)
        .map_err(|e| crate::error::AppError::BadRequestError(format!("Invalid timestamp: {}", e)))?
        .naive_utc();

    let frame = ActiveModel {
        event_id: Set(req.event_id),
        frame_id: Set(req.frame_id),
        r#type: Set(req.r#type.clone()),
        time_stamp: Set(crate::util::utc_to_timestamp(time_stamp.and_utc())),
        delta: Set(req.delta),
        score: Set(req.score),
        ..Default::default()
    };

    let result = frame.insert(db).await?;
    Ok(result)
}

/// Update a frame by key
pub async fn update(
    db: &DatabaseConnection,
    event_id: u64,
    frame_id: u32,
    req: &UpdateFrameRequest,
) -> AppResult<FrameModel> {
    let frame = find_by_key(db, event_id, frame_id).await?.ok_or_else(|| {
        crate::error::AppError::NotFoundError(crate::error::Resource {
            resource_type: crate::error::ResourceType::File,
            details: vec![
                ("event_id".to_string(), event_id.to_string()),
                ("frame_id".to_string(), frame_id.to_string()),
            ],
        })
    })?;

    let mut frame: ActiveModel = frame.into();

    if let Some(ref t) = req.r#type {
        frame.r#type = Set(t.clone());
    }
    if let Some(s) = req.score {
        frame.score = Set(s);
    }

    let updated = frame.update(db).await?;
    Ok(updated)
}

/// Delete a frame by key
pub async fn delete_by_key(db: &DatabaseConnection, event_id: u64, frame_id: u32) -> AppResult<()> {
    FrameEntity::delete_by_id((event_id, frame_id))
        .exec(db)
        .await?;
    Ok(())
}
