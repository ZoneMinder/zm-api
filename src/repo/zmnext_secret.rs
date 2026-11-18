//! DB query layer for the zm-api-owned `zmnext_secret` table.

use sea_orm::*;

use crate::entity::zmnext_secret;

/// All stored secrets for a monitor.
pub async fn find_by_monitor<C: ConnectionTrait>(
    db: &C,
    monitor_id: u32,
) -> Result<Vec<zmnext_secret::Model>, DbErr> {
    zmnext_secret::Entity::find()
        .filter(zmnext_secret::Column::MonitorId.eq(monitor_id))
        .all(db)
        .await
}

/// Insert or replace one secret.
pub async fn upsert<C: ConnectionTrait>(
    db: &C,
    monitor_id: u32,
    name: &str,
    ciphertext: String,
    now: jiff::civil::DateTime,
) -> Result<(), DbErr> {
    let now = crate::util::time::db_datetime(now);
    let existing = zmnext_secret::Entity::find_by_id((monitor_id, name.to_string()))
        .one(db)
        .await?;
    match existing {
        Some(row) => {
            let mut active: zmnext_secret::ActiveModel = row.into();
            active.ciphertext = Set(ciphertext);
            active.updated_at = Set(now);
            active.update(db).await?;
        }
        None => {
            zmnext_secret::ActiveModel {
                monitor_id: Set(monitor_id),
                name: Set(name.to_string()),
                ciphertext: Set(ciphertext),
                updated_at: Set(now),
            }
            .insert(db)
            .await?;
        }
    }
    Ok(())
}

/// Delete a monitor's secrets whose names are not in `keep`.
pub async fn delete_unreferenced<C: ConnectionTrait>(
    db: &C,
    monitor_id: u32,
    keep: &[String],
) -> Result<u64, DbErr> {
    let mut q = zmnext_secret::Entity::delete_many()
        .filter(zmnext_secret::Column::MonitorId.eq(monitor_id));
    if !keep.is_empty() {
        q = q.filter(zmnext_secret::Column::Name.is_not_in(keep.iter().cloned()));
    }
    Ok(q.exec(db).await?.rows_affected)
}
