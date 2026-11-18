use crate::dto::request::reports::{CreateReportRequest, UpdateReportRequest};
use crate::dto::PaginationParams;
use crate::entity::reports::{ActiveModel, Entity as Reports, Model as ReportModel};
use crate::error::{AppError, AppResult};
use jiff::civil::DateTime as Civil;
use sea_orm::*;

pub async fn find_all(db: &DatabaseConnection) -> AppResult<Vec<ReportModel>> {
    Ok(Reports::find().all(db).await?)
}

pub async fn find_paginated(
    db: &DatabaseConnection,
    params: &PaginationParams,
) -> AppResult<(Vec<ReportModel>, u64)> {
    let paginator = Reports::find().paginate(db, params.page_size());
    let total = paginator.num_items().await?;
    let items = paginator
        .fetch_page(params.page().saturating_sub(1))
        .await?;
    Ok((items, total))
}

pub async fn find_by_id(db: &DatabaseConnection, id: u32) -> AppResult<Option<ReportModel>> {
    Ok(Reports::find_by_id(id).one(db).await?)
}

pub async fn create(
    db: &DatabaseConnection,
    req: &CreateReportRequest,
    created_by: Option<u32>,
) -> AppResult<ReportModel> {
    let start_date_time = req
        .start_date_time
        .as_ref()
        .map(|s| parse_report_time(s))
        .transpose()
        .map_err(|e| AppError::BadRequestError(format!("Invalid start_date_time format: {}", e)))?;

    let end_date_time = req
        .end_date_time
        .as_ref()
        .map(|s| parse_report_time(s))
        .transpose()
        .map_err(|e| AppError::BadRequestError(format!("Invalid end_date_time format: {}", e)))?;

    let am = ActiveModel {
        id: Default::default(),
        name: Set(req.name.clone()),
        filter_id: Set(req.filter_id),
        start_date_time: Set(start_date_time),
        end_date_time: Set(end_date_time),
        interval: Set(req.interval),
        // Attribution comes from the token, never the body: letting a client
        // name the creator is forging authorship.
        created_by: Set(created_by),
    };
    Ok(am.insert(db).await?)
}

pub async fn update(
    db: &DatabaseConnection,
    id: u32,
    req: &UpdateReportRequest,
) -> AppResult<Option<ReportModel>> {
    let Some(model) = find_by_id(db, id).await? else {
        return Ok(None);
    };
    let mut am: ActiveModel = model.into();

    if let Some(v) = &req.name {
        am.name = Set(Some(v.clone()));
    }
    if let Some(v) = req.filter_id {
        am.filter_id = Set(Some(v));
    }
    if let Some(v) = req.interval {
        am.interval = Set(Some(v));
    }

    if let Some(s) = &req.start_date_time {
        let dt = parse_report_time(s).map_err(|e| {
            AppError::BadRequestError(format!("Invalid start_date_time format: {}", e))
        })?;
        am.start_date_time = Set(Some(dt));
    }

    if let Some(s) = &req.end_date_time {
        let dt = parse_report_time(s).map_err(|e| {
            AppError::BadRequestError(format!("Invalid end_date_time format: {}", e))
        })?;
        am.end_date_time = Set(Some(dt));
    }

    let updated = am.update(db).await?;
    Ok(Some(updated))
}

pub async fn delete_by_id(db: &DatabaseConnection, id: u32) -> AppResult<bool> {
    let res = Reports::delete_by_id(id).exec(db).await?;
    Ok(res.rows_affected > 0)
}

/// A report bound as the client sends it: `2026-01-02T03:04:05Z` or
/// `2026-01-02 03:04:05`. Either way the digits go to the column unchanged.
fn parse_report_time(s: &str) -> Result<sea_orm::prelude::DateTime, jiff::Error> {
    Civil::strptime("%Y-%m-%dT%H:%M:%SZ", s)
        .or_else(|_| Civil::strptime("%Y-%m-%d %H:%M:%S", s))
        .map(crate::util::time::db_datetime)
}
