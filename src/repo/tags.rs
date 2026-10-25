use std::collections::HashMap;

use sea_orm::*;

use crate::dto::request::tags::{CreateTagRequest, UpdateTagRequest};
use crate::dto::PaginationParams;
use crate::entity::events;
use crate::entity::events_tags;
use crate::entity::tags::{ActiveModel, Column, Entity as Tags, Model as TagModel};
use crate::error::AppResult;

pub async fn find_all(db: &DatabaseConnection) -> AppResult<Vec<TagModel>> {
    Ok(Tags::find().all(db).await?)
}

pub async fn find_paginated(
    db: &DatabaseConnection,
    params: &PaginationParams,
) -> AppResult<(Vec<TagModel>, u64)> {
    let paginator = Tags::find().paginate(db, params.page_size());
    let total = paginator.num_items().await?;
    let items = paginator
        .fetch_page(params.page().saturating_sub(1))
        .await?;
    Ok((items, total))
}

pub async fn find_by_id(db: &DatabaseConnection, id: u64) -> AppResult<Option<TagModel>> {
    Ok(Tags::find_by_id(id).one(db).await?)
}

pub async fn find_by_name(db: &DatabaseConnection, name: &str) -> AppResult<Option<TagModel>> {
    Ok(Tags::find().filter(Column::Name.eq(name)).one(db).await?)
}

pub async fn create(db: &DatabaseConnection, req: &CreateTagRequest) -> AppResult<TagModel> {
    let am = ActiveModel {
        id: Default::default(),
        name: Set(req.name.clone()),
        create_date: Set(req.create_date.map(crate::util::utc_to_timestamp)),
        ..Default::default()
    };
    Ok(am.insert(db).await?)
}

pub async fn update(
    db: &DatabaseConnection,
    id: u64,
    req: &UpdateTagRequest,
) -> AppResult<Option<TagModel>> {
    let Some(model) = find_by_id(db, id).await? else {
        return Ok(None);
    };
    let mut am: ActiveModel = model.into();

    if let Some(v) = &req.name {
        am.name = Set(v.clone());
    }
    if let Some(v) = req.create_date {
        am.create_date = Set(Some(crate::util::utc_to_timestamp(v)));
    }

    let updated = am.update(db).await?;
    Ok(Some(updated))
}

pub async fn delete_by_id(db: &DatabaseConnection, id: u64) -> AppResult<bool> {
    let res = Tags::delete_by_id(id).exec(db).await?;
    Ok(res.rows_affected > 0)
}

/// Each tag's event count, over `monitors` only when given (a restricted
/// caller's visible monitors).
pub async fn get_event_counts(
    db: &DatabaseConnection,
    monitors: Option<&[u32]>,
) -> AppResult<HashMap<u64, u64>> {
    let mut q = events_tags::Entity::find()
        .select_only()
        .column(events_tags::Column::TagId)
        .column_as(events_tags::Column::EventId.count(), "n")
        .group_by(events_tags::Column::TagId);
    if let Some(ids) = monitors {
        q = q
            .join(JoinType::InnerJoin, events_tags::Relation::Events.def())
            .filter(events::Column::MonitorId.is_in(ids.iter().copied()));
    }
    let rows: Vec<(u64, i64)> = q.into_tuple().all(db).await?;
    Ok(rows
        .into_iter()
        .map(|(tag, n)| (tag, u64::try_from(n).unwrap_or(0)))
        .collect())
}

/// A page of a tag's events, newest first, over `monitors` only when given.
/// `page` is 0-based.
pub async fn find_events_for_tag(
    db: &DatabaseConnection,
    tag_id: u64,
    monitors: Option<&[u32]>,
    page: u64,
    page_size: u64,
) -> AppResult<(Vec<events::Model>, u64)> {
    let tagged = sea_query::Query::select()
        .column(events_tags::Column::EventId)
        .from(events_tags::Entity)
        .and_where(events_tags::Column::TagId.eq(tag_id))
        .to_owned();
    let mut q = events::Entity::find().filter(events::Column::Id.in_subquery(tagged));
    if let Some(ids) = monitors {
        q = q.filter(events::Column::MonitorId.is_in(ids.iter().copied()));
    }
    let paginator = q
        .order_by_desc(events::Column::StartDateTime)
        .paginate(db, page_size);
    let total = paginator.num_items().await?;
    let events_list = paginator.fetch_page(page).await?;
    Ok((events_list, total))
}
