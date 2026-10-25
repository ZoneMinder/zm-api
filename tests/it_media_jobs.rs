//! Generated event videos through the media job queue (#187).
//!
//! Requires the test database:
//!   APP_PROFILE=test-db cargo nextest run --test it_media_jobs --run-ignored all

mod common;

use axum::http::{Method, StatusCode};
use common::fixtures::{insert_monitor, insert_storage, unique_name, RowGuard};
use common::harness::{superuser_token, TestApp};
use rust_decimal::Decimal;
use sea_orm::{ActiveModelTrait, ColumnTrait, EntityTrait, QueryFilter, Set};
use serde_json::json;
use zm_api::entity::sea_orm_active_enums::Scheme;

/// `run_pending` runs every queued job, so tests that queue and run jobs
/// take turns: otherwise one runs the other's job.
static QUEUE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[tokio::test]
#[ignore = "requires the test database (APP_PROFILE=test-db)"]
async fn a_jpeg_event_gets_a_generated_video() {
    let _queue = QUEUE.lock().await;
    let app = TestApp::spawn().await;
    // media_jobs is zm-api's own table, created by its migrations.
    common::test_db::migrate_test_db(&app.db).await;
    let token = superuser_token();
    let tmp = tempfile::tempdir().unwrap();
    let monitor = insert_monitor(&app.db, "Video").await.unwrap();
    let storage = insert_storage(
        &app.db,
        "Video",
        tmp.path().to_str().unwrap(),
        Scheme::Shallow,
    )
    .await
    .unwrap();
    let name = format!("Event {}", unique_name("v"));
    let event = zm_api::entity::events::ActiveModel {
        monitor_id: Set(monitor.id),
        storage_id: Set(Some(storage.id)),
        scheme: Set(Scheme::Shallow),
        state_id: Set(1),
        name: Set(name.clone()),
        frames: Set(Some(10)),
        length: Set(Decimal::new(100, 2)),
        width: Set(64),
        height: Set(48),
        default_video: Set(String::new()),
        ..Default::default()
    }
    .insert(&app.db)
    .await
    .unwrap();
    let id = event.id;
    let _g = (
        RowGuard::new(format!("Events#{id}"), move |db| async move {
            use zm_api::entity::{events, media_jobs};
            let _ = media_jobs::Entity::delete_many()
                .filter(media_jobs::Column::EventId.eq(id))
                .exec(&db)
                .await;
            let _ = events::Entity::delete_by_id(id).exec(&db).await;
        }),
        RowGuard::storage(storage.id),
        RowGuard::monitor(monitor.id),
    );
    let dir = tmp.path().join(monitor.id.to_string()).join(id.to_string());
    std::fs::create_dir_all(&dir).unwrap();
    let base = format!("/api/v3/events/{id}/videos");

    // Options that contradict each other, or a format ZoneMinder doesn't offer.
    for bad in [
        json!({ "rate": 2, "fps": 5 }),
        json!({ "scale": 0.5, "size": "32x24" }),
        json!({ "format": "exe" }),
    ] {
        let r = app.post_json(&base, &token, &bad).await;
        assert_eq!(r.status(), StatusCode::BAD_REQUEST, "{bad}: {}", r.text());
    }

    let r = app
        .post_json(
            &base,
            &token,
            &json!({ "scale": 0.5, "transform": "greyscale" }),
        )
        .await;
    assert_eq!(r.status(), StatusCode::ACCEPTED, "{}", r.text());
    let job: serde_json::Value = r.json();
    assert_eq!(job["status"], "queued");
    let job_id = job["id"].as_u64().unwrap();

    // Ten capture JPEGs, as zmc names them.
    for i in 1..=10u8 {
        image::RgbImage::from_pixel(64, 48, image::Rgb([i * 20, 100, 50]))
            .save(dir.join(format!("{i:05}-capture.jpg")))
            .unwrap();
    }
    let state = zm_api::server::state::AppState::for_test_with_db(app.db.clone());
    assert!(zm_api::service::media_jobs::run_pending(&state).await >= 1);

    let job: serde_json::Value = app
        .get(&format!("/api/v3/jobs/{job_id}"), &token)
        .await
        .json();
    assert_eq!(job["status"], "done", "{job}");
    let file = format!("{}-s0_5-hue=s=0.mp4", name.replace(' ', "_"));
    assert_eq!(job["output"], file.as_str());

    let videos: serde_json::Value = app.get(&base, &token).await.json();
    let v = &videos.as_array().unwrap()[0];
    assert_eq!(v["name"], file.as_str(), "{videos}");
    assert_eq!(v["scale"], 0.5);
    assert_eq!(v["recorded"], false);

    let r = app.get(&format!("{base}/{file}"), &token).await;
    assert_eq!(r.status(), StatusCode::OK);
    assert!(r.body.len() > 100, "an mp4, {} bytes", r.body.len());
    // A path in the name is not a file in the event.
    let r = app
        .get(&format!("{base}/..%2F..%2Fsecret.mp4"), &token)
        .await;
    assert_eq!(r.status(), StatusCode::NOT_FOUND);

    let r = app
        .request(Method::DELETE, &format!("{base}/{file}"))
        .bearer(&token)
        .send()
        .await;
    assert_eq!(r.status(), StatusCode::NO_CONTENT);
    assert!(!dir.join(&file).exists());
    let r = app
        .request(Method::DELETE, &format!("/api/v3/jobs/{job_id}"))
        .bearer(&token)
        .send()
        .await;
    assert_eq!(r.status(), StatusCode::NO_CONTENT);
}

/// A generated video can't take the recording's file name, whether the
/// event is named for it before the job is queued or after; and one user
/// can only have a few videos waiting.
#[tokio::test]
#[ignore = "requires the test database (APP_PROFILE=test-db)"]
async fn a_generated_video_never_replaces_the_recording() {
    let _queue = QUEUE.lock().await;
    let app = TestApp::spawn().await;
    common::test_db::migrate_test_db(&app.db).await;
    let token = superuser_token();
    let tmp = tempfile::tempdir().unwrap();
    let monitor = insert_monitor(&app.db, "VideoOver").await.unwrap();
    let storage = insert_storage(
        &app.db,
        "VideoOver",
        tmp.path().to_str().unwrap(),
        Scheme::Shallow,
    )
    .await
    .unwrap();
    let event = zm_api::entity::events::ActiveModel {
        monitor_id: Set(monitor.id),
        storage_id: Set(Some(storage.id)),
        scheme: Set(Scheme::Shallow),
        state_id: Set(1),
        name: Set(unique_name("Over")),
        frames: Set(Some(10)),
        length: Set(Decimal::new(100, 2)),
        width: Set(64),
        height: Set(48),
        default_video: Set(String::new()),
        ..Default::default()
    }
    .insert(&app.db)
    .await
    .unwrap();
    let id = event.id;
    let _g = (
        RowGuard::new(format!("Events#{id}"), move |db| async move {
            use zm_api::entity::{events, media_jobs};
            let _ = media_jobs::Entity::delete_many()
                .filter(media_jobs::Column::EventId.eq(id))
                .exec(&db)
                .await;
            let _ = events::Entity::delete_by_id(id).exec(&db).await;
        }),
        RowGuard::storage(storage.id),
        RowGuard::monitor(monitor.id),
    );
    let dir = tmp.path().join(monitor.id.to_string()).join(id.to_string());
    std::fs::create_dir_all(&dir).unwrap();
    let recording = format!("{id}-video.mp4");
    std::fs::write(dir.join(&recording), b"the recording").unwrap();
    let base = format!("/api/v3/events/{id}/videos");
    let rename = |name: String, default_video: String| {
        let db = &app.db;
        async move {
            zm_api::entity::events::ActiveModel {
                id: Set(id),
                name: Set(name),
                default_video: Set(default_video),
                ..Default::default()
            }
            .update(db)
            .await
            .unwrap();
        }
    };

    // Named so the plain video's file is the recording's: refused up front.
    rename(format!("{id}-video"), recording.clone()).await;
    let r = app
        .post_json(&base, &token, &json!({ "overwrite": true }))
        .await;
    assert_eq!(r.status(), StatusCode::CONFLICT, "{}", r.text());

    // Queued under another name, then renamed: the job fails instead.
    rename("Over".into(), String::new()).await;
    let r = app
        .post_json(&base, &token, &json!({ "overwrite": true, "rate": 2 }))
        .await;
    assert_eq!(r.status(), StatusCode::ACCEPTED, "{}", r.text());
    let job_id = r.json::<serde_json::Value>()["id"].as_u64().unwrap();
    std::fs::write(dir.join("Over-r2.mp4"), b"the recording").unwrap();
    rename("Over".into(), "Over-r2.mp4".into()).await;
    let state = zm_api::server::state::AppState::for_test_with_db(app.db.clone());
    zm_api::service::media_jobs::run_pending(&state).await;
    let job: serde_json::Value = app
        .get(&format!("/api/v3/jobs/{job_id}"), &token)
        .await
        .json();
    assert_eq!(job["status"], "failed", "{job}");
    assert_eq!(
        std::fs::read(dir.join("Over-r2.mp4")).unwrap(),
        b"the recording"
    );

    // Four waiting videos per user; the fifth is refused.
    for rate in [3, 4, 5, 6] {
        let r = app.post_json(&base, &token, &json!({ "rate": rate })).await;
        assert_eq!(r.status(), StatusCode::ACCEPTED, "{rate}: {}", r.text());
    }
    let r = app.post_json(&base, &token, &json!({ "rate": 7 })).await;
    assert_eq!(r.status(), StatusCode::CONFLICT, "{}", r.text());

    // A size at most four times the event's 64x48.
    let r = app
        .post_json(&base, &token, &json!({ "size": "257x48" }))
        .await;
    assert_eq!(r.status(), StatusCode::BAD_REQUEST, "{}", r.text());
}
