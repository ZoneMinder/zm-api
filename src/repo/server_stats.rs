use crate::dto::request::server_stats::CreateServerStatRequest;
use crate::dto::PaginationParams;
use crate::entity::server_stats::{ActiveModel, Entity as ServerStats, Model as ServerStatModel};
use crate::error::{AppError, AppResult};
use chrono::Utc;
use rust_decimal::Decimal;
use sea_orm::*;
use std::str::FromStr;

pub async fn find_all(db: &DatabaseConnection) -> AppResult<Vec<ServerStatModel>> {
    Ok(ServerStats::find().all(db).await?)
}

pub async fn find_paginated(
    db: &DatabaseConnection,
    params: &PaginationParams,
) -> AppResult<(Vec<ServerStatModel>, u64)> {
    let paginator = ServerStats::find().paginate(db, params.page_size());
    let total = paginator.num_items().await?;
    let items = paginator
        .fetch_page(params.page().saturating_sub(1))
        .await?;
    Ok((items, total))
}

pub async fn find_by_id(db: &DatabaseConnection, id: u32) -> AppResult<Option<ServerStatModel>> {
    Ok(ServerStats::find_by_id(id).one(db).await?)
}

pub async fn create(
    db: &DatabaseConnection,
    req: &CreateServerStatRequest,
) -> AppResult<ServerStatModel> {
    let cpu_load = req
        .cpu_load
        .as_ref()
        .map(|s| Decimal::from_str(s))
        .transpose()
        .map_err(|e| AppError::BadRequestError(format!("Invalid cpu_load: {}", e)))?;

    let cpu_user_percent = req
        .cpu_user_percent
        .as_ref()
        .map(|s| Decimal::from_str(s))
        .transpose()
        .map_err(|e| AppError::BadRequestError(format!("Invalid cpu_user_percent: {}", e)))?;

    let cpu_nice_percent = req
        .cpu_nice_percent
        .as_ref()
        .map(|s| Decimal::from_str(s))
        .transpose()
        .map_err(|e| AppError::BadRequestError(format!("Invalid cpu_nice_percent: {}", e)))?;

    let cpu_system_percent = req
        .cpu_system_percent
        .as_ref()
        .map(|s| Decimal::from_str(s))
        .transpose()
        .map_err(|e| AppError::BadRequestError(format!("Invalid cpu_system_percent: {}", e)))?;

    let cpu_idle_percent = req
        .cpu_idle_percent
        .as_ref()
        .map(|s| Decimal::from_str(s))
        .transpose()
        .map_err(|e| AppError::BadRequestError(format!("Invalid cpu_idle_percent: {}", e)))?;

    let cpu_usage_percent = req
        .cpu_usage_percent
        .as_ref()
        .map(|s| Decimal::from_str(s))
        .transpose()
        .map_err(|e| AppError::BadRequestError(format!("Invalid cpu_usage_percent: {}", e)))?;

    let am = ActiveModel {
        id: Default::default(),
        server_id: Set(req.server_id),
        time_stamp: Set(crate::util::utc_to_timestamp(Utc::now())),
        cpu_load: Set(cpu_load),
        cpu_user_percent: Set(cpu_user_percent),
        cpu_nice_percent: Set(cpu_nice_percent),
        cpu_system_percent: Set(cpu_system_percent),
        cpu_idle_percent: Set(cpu_idle_percent),
        cpu_usage_percent: Set(cpu_usage_percent),
        total_mem: Set(req.total_mem),
        free_mem: Set(req.free_mem),
        total_swap: Set(req.total_swap),
        free_swap: Set(req.free_swap),
    };
    Ok(am.insert(db).await?)
}

pub async fn delete_by_id(db: &DatabaseConnection, id: u32) -> AppResult<bool> {
    let res = ServerStats::delete_by_id(id).exec(db).await?;
    Ok(res.rows_affected > 0)
}
