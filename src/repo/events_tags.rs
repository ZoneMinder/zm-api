use chrono::Utc;
use sea_orm::*;

use crate::dto::request::events_tags::CreateEventTagRequest;
use crate::dto::PaginationParams;
use crate::entity::events_tags::{
    ActiveModel, Column, Entity as EventsTags, Model as EventTagModel,
};
use crate::error::AppResult;

pub async fn find_all(
    db: &DatabaseConnection,
    event_id: Option<u64>,
    tag_id: Option<u64>,
) -> AppResult<Vec<EventTagModel>> {
    let mut query = EventsTags::find();

    if let Some(eid) = event_id {
        query = query.filter(Column::EventId.eq(eid));
    }
    if let Some(tid) = tag_id {
        query = query.filter(Column::TagId.eq(tid));
    }

    Ok(query.all(db).await?)
}

pub async fn find_paginated(
    db: &DatabaseConnection,
    event_id: Option<u64>,
    tag_id: Option<u64>,
    params: &PaginationParams,
) -> AppResult<(Vec<EventTagModel>, u64)> {
    let mut query = EventsTags::find();

    if let Some(eid) = event_id {
        query = query.filter(Column::EventId.eq(eid));
    }
    if let Some(tid) = tag_id {
        query = query.filter(Column::TagId.eq(tid));
    }

    let paginator = query.paginate(db, params.page_size());
    let total = paginator.num_items().await?;
    let items = paginator
        .fetch_page(params.page().saturating_sub(1))
        .await?;
    Ok((items, total))
}

pub async fn find_by_composite_id(
    db: &DatabaseConnection,
    tag_id: u64,
    event_id: u64,
) -> AppResult<Option<EventTagModel>> {
    Ok(EventsTags::find()
        .filter(Column::TagId.eq(tag_id))
        .filter(Column::EventId.eq(event_id))
        .one(db)
        .await?)
}

pub async fn create(
    db: &DatabaseConnection,
    req: &CreateEventTagRequest,
) -> AppResult<EventTagModel> {
    let am = ActiveModel {
        tag_id: Set(req.tag_id),
        event_id: Set(req.event_id),
        assigned_date: Set(Some(crate::util::utc_to_timestamp(Utc::now()))),
        assigned_by: Set(req.assigned_by),
    };
    Ok(am.insert(db).await?)
}

pub async fn delete_by_composite_id(
    db: &DatabaseConnection,
    tag_id: u64,
    event_id: u64,
) -> AppResult<bool> {
    let res = EventsTags::delete_many()
        .filter(Column::TagId.eq(tag_id))
        .filter(Column::EventId.eq(event_id))
        .exec(db)
        .await?;
    Ok(res.rows_affected > 0)
}
