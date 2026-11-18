//! DB query layer for the zm-api-owned `monitor_pipeline` table.
//!
//! One row per monitor holds the zm-next processing plugin graph (see
//! [`crate::entity::monitor_pipeline`]). `monitor_id` is the primary key.

use sea_orm::*;

use crate::entity::monitor_pipeline;
use crate::entity::prelude::MonitorPipeline;

/// Fetch the stored processing graph for a monitor, if one exists.
pub async fn find_by_monitor(
    db: &DatabaseConnection,
    monitor_id: u32,
) -> Result<Option<monitor_pipeline::Model>, DbErr> {
    MonitorPipeline::find_by_id(monitor_id).one(db).await
}

/// Insert or replace the processing graph for a monitor. `created_at` is set on
/// first write and preserved on update; `updated_at` always advances.
pub async fn upsert(
    db: &DatabaseConnection,
    monitor_id: u32,
    graph_json: String,
    version: u32,
    now: jiff::civil::DateTime,
) -> Result<monitor_pipeline::Model, DbErr> {
    let now = crate::util::time::db_datetime(now);
    match MonitorPipeline::find_by_id(monitor_id).one(db).await? {
        Some(existing) => {
            let mut active: monitor_pipeline::ActiveModel = existing.into();
            active.graph_json = Set(graph_json);
            active.version = Set(version);
            active.updated_at = Set(now);
            active.update(db).await
        }
        None => {
            monitor_pipeline::ActiveModel {
                monitor_id: Set(monitor_id),
                graph_json: Set(graph_json),
                version: Set(version),
                created_at: Set(now),
                updated_at: Set(now),
            }
            .insert(db)
            .await
        }
    }
}

/// Remove a monitor's stored graph (e.g. when reverting it from zm-next).
pub async fn delete_by_monitor(db: &DatabaseConnection, monitor_id: u32) -> Result<(), DbErr> {
    MonitorPipeline::delete_by_id(monitor_id).exec(db).await?;
    Ok(())
}
