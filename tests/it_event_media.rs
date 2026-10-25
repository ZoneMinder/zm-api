//! A recorded event played back from its files (#239).
//!
//! Before this, the playback endpoints were only tested for unknown events
//! (404). Here a closed event on shallow storage has a real MP4 as ZoneMinder
//! writes it (`{event_id}-video.mp4`), and each way of playing it is checked
//! against the file.
//!
//!   APP_PROFILE=test-db cargo test --test it_event_media -- --include-ignored

mod common;

use axum::http::{Method, StatusCode};
use common::fixtures::{insert_monitor, insert_storage, unique_name, RowGuard};
use common::harness::{superuser_token, TestApp};
use rust_decimal::Decimal;
use sea_orm::{ActiveModelTrait, ColumnTrait, EntityTrait, QueryFilter, Set};
use serde_json::Value;
use zm_api::entity::sea_orm_active_enums::Scheme;

struct Recorded {
    app: TestApp,
    token: String,
    event_id: u64,
    video: Vec<u8>,
    _tmp: tempfile::TempDir,
    _guards: Vec<RowGuard>,
}

/// A closed 2-second event at 160x120 with its MP4 on disk, or `None` when
/// there's no ffmpeg (outside CI).
async fn recorded(label: &str) -> Option<Recorded> {
    let app = TestApp::spawn().await;
    let tmp = tempfile::tempdir().expect("tempdir");
    let monitor = insert_monitor(&app.db, label).await.expect("monitor");
    let storage = insert_storage(
        &app.db,
        label,
        tmp.path().to_str().unwrap(),
        Scheme::Shallow,
    )
    .await
    .expect("storage");
    let event = zm_api::entity::events::ActiveModel {
        monitor_id: Set(monitor.id),
        storage_id: Set(Some(storage.id)),
        scheme: Set(Scheme::Shallow),
        state_id: Set(1),
        name: Set(unique_name(label)),
        frames: Set(Some(20)),
        length: Set(Decimal::new(200, 2)),
        width: Set(160),
        height: Set(120),
        default_video: Set(String::new()),
        start_date_time: Set(Some(
            chrono::Utc::now().naive_utc() - chrono::Duration::seconds(60),
        )),
        end_date_time: Set(Some(
            chrono::Utc::now().naive_utc() - chrono::Duration::seconds(58),
        )),
        ..Default::default()
    }
    .insert(&app.db)
    .await
    .expect("event");
    let id = event.id;
    let guards = vec![
        RowGuard::new(format!("Events#{id}"), move |db| async move {
            let _ = zm_api::entity::frames::Entity::delete_many()
                .filter(zm_api::entity::frames::Column::EventId.eq(id))
                .exec(&db)
                .await;
            let _ = zm_api::entity::events::Entity::delete_by_id(id)
                .exec(&db)
                .await;
        }),
        RowGuard::storage(storage.id),
        RowGuard::monitor(monitor.id),
    ];
    let dir = tmp.path().join(monitor.id.to_string()).join(id.to_string());
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(format!("{id}-video.mp4"));
    if !common::media::black_then_white_mp4(&path, "160x120", &["-movflags", "+faststart"]) {
        return None;
    }
    Some(Recorded {
        app,
        token: superuser_token(),
        event_id: id,
        video: std::fs::read(&path).unwrap(),
        _tmp: tmp,
        _guards: guards,
    })
}

#[tokio::test]
#[ignore = "requires the test database (APP_PROFILE=test-db)"]
async fn the_video_is_served_whole_and_in_ranges() {
    let Some(r) = recorded("MediaVideo").await else {
        return;
    };
    for path in [
        format!("/api/v3/events/{}/video", r.event_id),
        format!("/api/v3/events/{}/stream/video.mp4", r.event_id),
    ] {
        let whole = r.app.get(&path, &r.token).await;
        assert_eq!(whole.status(), StatusCode::OK, "{path}: {}", whole.text());
        assert_eq!(whole.body, r.video, "{path}: the file as written");

        let part = r
            .app
            .request(Method::GET, &path)
            .bearer(&r.token)
            .header("Range", "bytes=100-199")
            .send()
            .await;
        assert_eq!(part.status(), StatusCode::PARTIAL_CONTENT, "{path}");
        assert_eq!(part.body, r.video[100..200], "{path}: bytes 100-199");
        assert_eq!(
            part.headers
                .get("content-range")
                .and_then(|v| v.to_str().ok()),
            Some(format!("bytes 100-199/{}", r.video.len()).as_str()),
            "{path}"
        );

        // A last byte past the end means "to the end".
        let rest = r
            .app
            .request(Method::GET, &path)
            .bearer(&r.token)
            .header("Range", "bytes=0-99999999")
            .send()
            .await;
        assert_eq!(rest.status(), StatusCode::PARTIAL_CONTENT, "{path}");
        assert_eq!(rest.body, r.video, "{path}: all of it");

        let beyond = r
            .app
            .request(Method::GET, &path)
            .bearer(&r.token)
            .header("Range", &format!("bytes={}-", r.video.len() + 10))
            .send()
            .await;
        assert_eq!(beyond.status(), StatusCode::RANGE_NOT_SATISFIABLE, "{path}");
        assert_eq!(
            beyond
                .headers
                .get("content-range")
                .and_then(|v| v.to_str().ok()),
            Some(format!("bytes */{}", r.video.len()).as_str()),
            "{path}"
        );
    }
}

#[tokio::test]
#[ignore = "requires the test database (APP_PROFILE=test-db)"]
async fn the_event_plays_as_hls() {
    let Some(r) = recorded("MediaHls").await else {
        return;
    };
    let base = format!("/api/v3/events/{}/stream", r.event_id);

    let playlist = r.app.get(&format!("{base}/playlist.m3u8"), &r.token).await;
    assert_eq!(playlist.status(), StatusCode::OK, "{}", playlist.text());
    let text = playlist.text();
    assert!(text.starts_with("#EXTM3U"), "{text}");
    assert!(
        text.contains("#EXT-X-ENDLIST"),
        "a closed event is complete: {text}"
    );

    let init = r.app.get(&format!("{base}/init.mp4"), &r.token).await;
    assert_eq!(init.status(), StatusCode::OK, "{}", init.text());
    assert!(init.body.windows(4).any(|w| w == b"ftyp"), "init has ftyp");
    let segment = r.app.get(&format!("{base}/segment/0"), &r.token).await;
    assert_eq!(segment.status(), StatusCode::OK, "{}", segment.text());
    assert!(
        segment.body.windows(4).any(|w| w == b"moof"),
        "segment has moof"
    );
}

#[tokio::test]
#[ignore = "requires the test database (APP_PROFILE=test-db)"]
async fn the_thumbnail_and_info_describe_the_video() {
    let Some(r) = recorded("MediaThumb").await else {
        return;
    };

    let thumb = r
        .app
        .get(
            &format!("/api/v3/events/{}/thumbnail", r.event_id),
            &r.token,
        )
        .await;
    assert_eq!(thumb.status(), StatusCode::OK, "{}", thumb.text());
    assert_eq!(&thumb.body[..2], &[0xFF, 0xD8], "a JPEG");

    let info = r
        .app
        .get(&format!("/api/v3/events/{}/info", r.event_id), &r.token)
        .await;
    assert_eq!(info.status(), StatusCode::OK, "{}", info.text());
    let info: Value = info.json();
    assert_eq!(info["width"], 160, "{info}");
    assert_eq!(info["height"], 120, "{info}");
}
