//! End-to-end coverage for `DELETE /api/v3/events/{id}`.
//!
//! The delete must remove the event's on-disk media directory **and** its child
//! DB rows (frames, per-period stats — which have no FK cascade), not just the
//! `Events` row. This drives the full HTTP stack against a real event whose
//! media lives in a tempdir-backed `Storage`.
//!
//! Requires the test database — run with:
//!   APP_PROFILE=test-db cargo test --test it_event_delete -- --include-ignored

mod common;

use common::fixtures::{insert_monitor, insert_storage, unique_name, RowGuard};
use common::harness::{superuser_token, TestApp};
use sea_orm::{
    ActiveModelTrait, ColumnTrait, DatabaseConnection, EntityTrait, PaginatorTrait, QueryFilter,
    Set,
};
use zm_api::entity::sea_orm_active_enums::Scheme;

fn guard_event(id: u64) -> RowGuard {
    RowGuard::new(format!("Events#{id}"), move |db| async move {
        let _ = zm_api::entity::events::Entity::delete_by_id(id)
            .exec(&db)
            .await;
    })
}

/// Insert a finished event pinned to a specific storage + scheme.
async fn insert_event_on_storage(
    db: &DatabaseConnection,
    monitor_id: u32,
    storage_id: u16,
    scheme: Scheme,
    label: &str,
) -> u64 {
    let now = zm_api::util::time::db_utc_now();
    zm_api::entity::events::ActiveModel {
        monitor_id: Set(monitor_id),
        storage_id: Set(Some(storage_id)),
        state_id: Set(1),
        name: Set(unique_name(label)),
        scheme: Set(scheme),
        start_date_time: Set(Some(now)),
        end_date_time: Set(Some(now)),
        ..Default::default()
    }
    .insert(db)
    .await
    .expect("insert event")
    .id
}

async fn insert_frame(db: &DatabaseConnection, event_id: u64, frame_id: u32) {
    zm_api::entity::frames::ActiveModel {
        event_id: Set(event_id),
        frame_id: Set(frame_id),
        ..Default::default()
    }
    .insert(db)
    .await
    .expect("insert frame");
}

async fn frame_count(db: &DatabaseConnection, event_id: u64) -> u64 {
    zm_api::entity::frames::Entity::find()
        .filter(zm_api::entity::frames::Column::EventId.eq(event_id))
        .count(db)
        .await
        .expect("count frames")
}

#[tokio::test]
#[ignore = "requires the test database (APP_PROFILE=test-db)"]
async fn delete_event_removes_media_directory_and_child_rows() {
    let app = TestApp::spawn().await;
    let token = superuser_token();
    let tmp = tempfile::tempdir().expect("tempdir");

    let monitor = insert_monitor(&app.db, "EvtDelMon")
        .await
        .expect("insert monitor");
    let _mg = RowGuard::monitor(monitor.id);
    let storage = insert_storage(
        &app.db,
        "EvtDelStore",
        tmp.path().to_str().unwrap(),
        Scheme::Shallow,
    )
    .await
    .expect("insert storage");
    let _sg = RowGuard::storage(storage.id);

    let event_id =
        insert_event_on_storage(&app.db, monitor.id, storage.id, Scheme::Shallow, "EvtDel").await;
    let _eg = guard_event(event_id);
    insert_frame(&app.db, event_id, 1).await;
    insert_frame(&app.db, event_id, 2).await;

    // Shallow layout on disk: {root}/{monitor}/{event_id}/{event_id}-video.mp4
    let dir = tmp
        .path()
        .join(monitor.id.to_string())
        .join(event_id.to_string());
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join(format!("{event_id}-video.mp4")), b"video-bytes").unwrap();

    // Preconditions: media on disk and child rows present.
    assert!(dir.exists(), "precondition: media directory exists");
    assert_eq!(
        frame_count(&app.db, event_id).await,
        2,
        "precondition: frames exist"
    );

    let resp = app
        .delete(&format!("/api/v3/events/{event_id}"), &token)
        .await;
    assert!(
        resp.status().is_success(),
        "delete should succeed; status {} body {}",
        resp.status(),
        resp.text()
    );

    // Media directory removed.
    assert!(
        !dir.exists(),
        "the event's media directory must be gone after delete"
    );
    // Event row removed.
    assert!(
        zm_api::entity::events::Entity::find_by_id(event_id)
            .one(&app.db)
            .await
            .unwrap()
            .is_none(),
        "the event row must be gone"
    );
    // Child frame rows removed (delete_with_children), not orphaned.
    assert_eq!(
        frame_count(&app.db, event_id).await,
        0,
        "frames must be deleted together with the event"
    );
}
