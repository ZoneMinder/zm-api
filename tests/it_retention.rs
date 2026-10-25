//! Integration tests for the event-retention reaper (`service::retention`).
//!
//! Each test builds an isolated `Storage` rooted at a tempdir and reaps only
//! that storage via `RetentionService::reap_storage_once`, so a shared test
//! database is safe (the global `reap_once` would touch every storage).
//! Deletion is driven by the byte quota (`max_bytes`) with the free-space and
//! age checks disabled, so it is deterministic regardless of the host disk.
//!
//! Requires the test database — run with:
//!   APP_PROFILE=test-db cargo test --test it_retention -- --include-ignored

mod common;

use std::sync::Arc;

use common::fixtures::{insert_monitor, insert_storage, unique_name, RowGuard};
use sea_orm::{ActiveModelTrait, DatabaseConnection, EntityTrait, Set};
use zm_api::configure::retention::RetentionConfig;
use zm_api::entity::sea_orm_active_enums::Scheme;
use zm_api::entity::storage::Model as StorageModel;
use zm_api::service::retention::RetentionService;

const MIB: u64 = 1024 * 1024;

fn guard_event(id: u64) -> RowGuard {
    RowGuard::new(format!("Events#{id}"), move |db| async move {
        let _ = zm_api::entity::events::Entity::delete_by_id(id)
            .exec(&db)
            .await;
    })
}

/// Insert an event on a storage with an explicit start time, ended-ness,
/// archived flag, and byte size; returns its id.
#[allow(clippy::too_many_arguments)]
async fn insert_event(
    db: &DatabaseConnection,
    monitor_id: u32,
    storage_id: u16,
    start: chrono::NaiveDateTime,
    ended: bool,
    archived: u8,
    disk_space: u64,
    label: &str,
) -> u64 {
    zm_api::entity::events::ActiveModel {
        monitor_id: Set(monitor_id),
        storage_id: Set(Some(storage_id)),
        state_id: Set(1),
        name: Set(unique_name(label)),
        scheme: Set(Scheme::Shallow),
        start_date_time: Set(Some(start)),
        end_date_time: Set(ended.then_some(start)),
        archived: Set(archived),
        disk_space: Set(Some(disk_space)),
        ..Default::default()
    }
    .insert(db)
    .await
    .expect("insert event")
    .id
}

/// Create the Shallow on-disk directory `{root}/{monitor}/{event_id}` with a
/// dummy media file, so a real deletion has something to remove.
fn make_event_dir(root: &std::path::Path, monitor_id: u32, event_id: u64) -> std::path::PathBuf {
    let dir = root.join(monitor_id.to_string()).join(event_id.to_string());
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join(format!("{event_id}-video.mp4")), b"x").unwrap();
    dir
}

/// A quota-only config: free-space and age checks disabled, deletion driven
/// solely by `max_bytes`.
fn quota_config(max_bytes: u64, dry_run: bool) -> RetentionConfig {
    RetentionConfig {
        enabled: true,
        interval_seconds: 300,
        min_free_pct: 0.0,
        max_age_days: 0,
        max_bytes,
        dry_run,
        max_deletes_per_pass: 0,
    }
}

async fn service(config: RetentionConfig, iso: &common::isolated::IsolatedDb) -> RetentionService {
    // The service owns its own connection; fixtures/assertions use another.
    let db = Arc::new(iso.connect().await);
    RetentionService::new(db, config)
}

async fn event_exists(db: &DatabaseConnection, id: u64) -> bool {
    zm_api::entity::events::Entity::find_by_id(id)
        .one(db)
        .await
        .unwrap()
        .is_some()
}

/// A dedicated storage rooted at a tempdir, plus its RowGuard.
async fn dedicated_storage(
    db: &DatabaseConnection,
    tmp: &tempfile::TempDir,
    label: &str,
) -> (StorageModel, RowGuard) {
    let storage = insert_storage(db, label, tmp.path().to_str().unwrap(), Scheme::Shallow)
        .await
        .expect("insert storage");
    let guard = RowGuard::storage(storage.id);
    (storage, guard)
}

#[tokio::test]
#[ignore = "requires the test database (APP_PROFILE=test-db)"]
async fn reap_deletes_oldest_over_quota_and_protects_newest() {
    // Maintenance and retention passes act on the whole database.
    let iso = common::isolated::IsolatedDb::create().await;
    let db = iso.connect().await;
    let tmp = tempfile::tempdir().unwrap();

    let monitor = insert_monitor(&db, "ReapMon").await.expect("monitor");
    let _mg = RowGuard::monitor(monitor.id);
    let (storage, _sg) = dedicated_storage(&db, &tmp, "ReapStore").await;

    let base = chrono::Utc::now().naive_utc();
    // Oldest -> newest; 100 MiB each; total 300 MiB.
    let e1 = insert_event(
        &db,
        monitor.id,
        storage.id,
        base - chrono::Duration::hours(3),
        true,
        0,
        100 * MIB,
        "e1",
    )
    .await;
    let e2 = insert_event(
        &db,
        monitor.id,
        storage.id,
        base - chrono::Duration::hours(2),
        true,
        0,
        100 * MIB,
        "e2",
    )
    .await;
    let e3 = insert_event(
        &db,
        monitor.id,
        storage.id,
        base - chrono::Duration::hours(1),
        true,
        0,
        100 * MIB,
        "e3",
    )
    .await;
    let (_g1, _g2, _g3) = (guard_event(e1), guard_event(e2), guard_event(e3));
    let d1 = make_event_dir(tmp.path(), monitor.id, e1);
    let d2 = make_event_dir(tmp.path(), monitor.id, e2);
    let d3 = make_event_dir(tmp.path(), monitor.id, e3);

    // Quota 150 MiB: delete e1 (300->200), delete e2 (200->100 <= 150 stop);
    // e3 is the newest per monitor and is always protected.
    let deleted = service(quota_config(150 * MIB, false), &iso)
        .await
        .reap_storage_once(&storage, false)
        .await
        .expect("reap");
    assert_eq!(deleted, 2, "should delete the two oldest over-quota events");

    assert!(
        !event_exists(&db, e1).await && !d1.exists(),
        "oldest e1 gone (row + dir)"
    );
    assert!(
        !event_exists(&db, e2).await && !d2.exists(),
        "e2 gone (row + dir)"
    );
    assert!(
        event_exists(&db, e3).await && d3.exists(),
        "newest e3 protected (row + dir)"
    );
}

#[tokio::test]
#[ignore = "requires the test database (APP_PROFILE=test-db)"]
async fn reap_never_touches_archived_or_in_progress_events() {
    // Maintenance and retention passes act on the whole database.
    let iso = common::isolated::IsolatedDb::create().await;
    let db = iso.connect().await;
    let tmp = tempfile::tempdir().unwrap();

    let monitor = insert_monitor(&db, "ReapSafeMon").await.expect("monitor");
    let _mg = RowGuard::monitor(monitor.id);
    let (storage, _sg) = dedicated_storage(&db, &tmp, "ReapSafeStore").await;
    // A storage with rows and an empty directory is what an unmounted volume
    // looks like, and the reaper refuses it (#104); give it the monitor
    // directory a real storage has.
    std::fs::create_dir_all(tmp.path().join(monitor.id.to_string())).unwrap();

    let base = chrono::Utc::now().naive_utc();
    // Archived + in-progress are old and large but must be immune to the reaper.
    let archived = insert_event(
        &db,
        monitor.id,
        storage.id,
        base - chrono::Duration::hours(5),
        true,
        1,
        100 * MIB,
        "arch",
    )
    .await;
    let in_progress = insert_event(
        &db,
        monitor.id,
        storage.id,
        base - chrono::Duration::hours(4),
        false,
        0,
        100 * MIB,
        "prog",
    )
    .await;
    // A normal old event (deletable) and the newest (protected).
    let normal_old = insert_event(
        &db,
        monitor.id,
        storage.id,
        base - chrono::Duration::hours(3),
        true,
        0,
        100 * MIB,
        "old",
    )
    .await;
    let newest = insert_event(
        &db,
        monitor.id,
        storage.id,
        base - chrono::Duration::hours(1),
        true,
        0,
        100 * MIB,
        "new",
    )
    .await;
    let (_ga, _gp, _go, _gn) = (
        guard_event(archived),
        guard_event(in_progress),
        guard_event(normal_old),
        guard_event(newest),
    );

    // The storage holds 400 MiB (the quota counts everything, archived and
    // in-progress included — #110) against a 150 MiB quota: normal_old is
    // deleted; newest is protected and the other two are immune, so the pass
    // ends with the quota still exceeded rather than touching them.
    let deleted = service(quota_config(150 * MIB, false), &iso)
        .await
        .reap_storage_once(&storage, false)
        .await
        .expect("reap");
    assert_eq!(deleted, 1, "only the one deletable old event should go");

    assert!(
        event_exists(&db, archived).await,
        "archived event must survive"
    );
    assert!(
        event_exists(&db, in_progress).await,
        "in-progress event must survive"
    );
    assert!(
        !event_exists(&db, normal_old).await,
        "normal old event should be reaped"
    );
    assert!(event_exists(&db, newest).await, "newest event is protected");
}

#[tokio::test]
#[ignore = "requires the test database (APP_PROFILE=test-db)"]
async fn reap_dry_run_reports_but_deletes_nothing() {
    // Maintenance and retention passes act on the whole database.
    let iso = common::isolated::IsolatedDb::create().await;
    let db = iso.connect().await;
    let tmp = tempfile::tempdir().unwrap();

    let monitor = insert_monitor(&db, "ReapDryMon").await.expect("monitor");
    let _mg = RowGuard::monitor(monitor.id);
    let (storage, _sg) = dedicated_storage(&db, &tmp, "ReapDryStore").await;

    let base = chrono::Utc::now().naive_utc();
    let e1 = insert_event(
        &db,
        monitor.id,
        storage.id,
        base - chrono::Duration::hours(3),
        true,
        0,
        100 * MIB,
        "d1",
    )
    .await;
    let e2 = insert_event(
        &db,
        monitor.id,
        storage.id,
        base - chrono::Duration::hours(2),
        true,
        0,
        100 * MIB,
        "d2",
    )
    .await;
    let e3 = insert_event(
        &db,
        monitor.id,
        storage.id,
        base - chrono::Duration::hours(1),
        true,
        0,
        100 * MIB,
        "d3",
    )
    .await;
    let (_g1, _g2, _g3) = (guard_event(e1), guard_event(e2), guard_event(e3));
    let d1 = make_event_dir(tmp.path(), monitor.id, e1);

    // dry_run: the plan reports the same two would-be deletions, but nothing is
    // actually removed.
    let would_delete = service(quota_config(150 * MIB, true), &iso)
        .await
        .reap_storage_once(&storage, false)
        .await
        .expect("reap");
    assert_eq!(
        would_delete, 2,
        "dry-run should report the would-be deletions"
    );

    assert!(
        event_exists(&db, e1).await && d1.exists(),
        "dry-run must not delete e1 (row or dir)"
    );
    assert!(event_exists(&db, e2).await, "dry-run must not delete e2");
    assert!(event_exists(&db, e3).await, "dry-run must not delete e3");
}
