//! Integration tests for the motion-synopsis feature (P1).
//!
//! Covers, against the real test database:
//!   - the `event_synopsis` schema/entity/enum round-trip (repo CRUD);
//!   - the `review_assets` (0x0306) ingest path (`EventIngestor` end-to-end),
//!     including event-id reconciliation;
//!   - the `SynopsisService` still render (compositor over a black plate);
//!   - the `GET /api/v3/events/{id}/synopsis/review` endpoint: auth, ACL
//!     not-found, and the disabled-by-default gate.
//!
//! The `event_synopsis` table is zm-api-owned, so each test first applies the
//! crate migrations (idempotent) — it is not part of ZoneMinder's schema.
//!
//! Requires the test database — run with:
//!   APP_PROFILE=test-db cargo test --test it_synopsis -- --include-ignored

mod common;

use std::sync::Arc;

use axum::http::{Method, StatusCode};
use jiff::SignedDuration;
use sea_orm::{ActiveModelTrait, EntityTrait, Set};
use tokio::sync::mpsc;
use zm_api::util::time::{db_datetime, utc_now};

use common::fixtures::{insert_monitor, unique_name, RowGuard};
use common::harness::{superuser_token, TestApp};
use common::test_db::{get_test_db, migrate_test_db};

use zm_api::configure::synopsis::SynopsisConfig;
use zm_api::configure::zmnext::IngestConfig;
use zm_api::entity::event_synopsis;
use zm_api::entity::sea_orm_active_enums::SynopsisStatus;
use zm_api::repo;
use zm_api::service::synopsis::SynopsisService;
use zm_api::service::zmnext::EventIngestor;
use zm_api::streaming::source::protocol::{MonitorEvent, EVENT_REVIEW_ASSETS};
use zm_api::streaming::source::router::{ControlReply, MonitorEventEnvelope};

const MISSING_EVENT_ID: u64 = 999_000_222;

/// Apply the crate migrations to the test DB (creates `event_synopsis`).
///
/// [`migrate_test_db`] serializes migrations across test processes; the
/// `OnceCell` skips the repeat when tests share a process under `cargo test`.
async fn ensure_schema(db: &sea_orm::DatabaseConnection) {
    static SCHEMA: tokio::sync::OnceCell<()> = tokio::sync::OnceCell::const_new();
    SCHEMA
        .get_or_init(|| async {
            migrate_test_db(db).await;
        })
        .await;
}

/// Delete every `event_synopsis` row for a monitor on drop.
fn guard_synopsis_for_monitor(monitor_id: u32) -> RowGuard {
    RowGuard::new(
        format!("event_synopsis(monitor={monitor_id})"),
        move |db| async move {
            use sea_orm::{ColumnTrait, QueryFilter};
            let _ = event_synopsis::Entity::delete_many()
                .filter(event_synopsis::Column::MonitorId.eq(monitor_id))
                .exec(&db)
                .await;
        },
    )
}

fn guard_event(id: u64) -> RowGuard {
    RowGuard::new(format!("Events#{id}"), move |db| async move {
        let _ = zm_api::entity::events::Entity::delete_by_id(id)
            .exec(&db)
            .await;
    })
}

/// A minimal but well-formed `review_assets` manifest, with source dimensions so
/// the still renders, and a tube whose cutout file does not exist (it is skipped
/// — the still renders over the black plate either way).
fn manifest_json(monitor_id: u32, event_id: u64, clip_token: &str) -> String {
    serde_json::json!({
        "type": "review_assets",
        "schema": 1,
        "monitor_id": monitor_id,
        "event_id": event_id,
        "clip_token": clip_token,
        "clip_path": "/nonexistent/clip/video.mkv",
        "path_base": "synopsis",
        "t_start_us": 1_782_129_185_000_000i64,
        "t_end_us": 1_782_129_260_000_000i64,
        "source_w": 32,
        "source_h": 24,
        "sample_fps": 4,
        "plates": [],
        "tubes": [
            {"track_id": 1, "label": "person", "class_id": 0,
             "samples": [{"bbox": [2, 2, 8, 8], "cutout": "t1/0001.jpg",
                          "cutout_w": 8, "cutout_h": 8}]}
        ]
    })
    .to_string()
}

async fn insert_synopsis_row(
    db: &sea_orm::DatabaseConnection,
    monitor_id: u32,
    event_id: Option<u64>,
    clip_token: &str,
    status: SynopsisStatus,
) -> event_synopsis::Model {
    let manifest = manifest_json(monitor_id, event_id.unwrap_or(0), clip_token);
    event_synopsis::ActiveModel {
        event_id: Set(event_id),
        monitor_id: Set(monitor_id),
        clip_token: Set(clip_token.to_string()),
        manifest_json: Set(manifest),
        asset_dir: Set("/nonexistent/clip/synopsis".to_string()),
        status: Set(status),
        rendered_path: Set(None),
        tube_count: Set(1),
        source_w: Set(32),
        source_h: Set(24),
        created_at: Set(db_datetime(utc_now())),
        expires_at: Set(None),
        ..Default::default()
    }
    .insert(db)
    .await
    .expect("insert event_synopsis row")
}

// ---------------------------------------------------------------------------
// repo round-trip — validates migration ↔ entity ↔ enum mapping
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires the test database (APP_PROFILE=test-db)"]
async fn synopsis_repo_round_trip() {
    let db = get_test_db().await.expect("test db");
    ensure_schema(&db).await;

    let monitor_id = 990_701;
    let _guard = guard_synopsis_for_monitor(monitor_id);
    let clip_token = unique_name("clip");
    let event_id = 990_801_u64;

    let row = insert_synopsis_row(
        &db,
        monitor_id,
        Some(event_id),
        &clip_token,
        SynopsisStatus::Pending,
    )
    .await;

    // by event id
    let by_event = repo::event_synopsis::find_by_event_id(&db, event_id)
        .await
        .unwrap()
        .expect("row by event id");
    assert_eq!(by_event.id, row.id);
    assert_eq!(by_event.status, SynopsisStatus::Pending);
    assert_eq!(by_event.tube_count, 1);
    assert_eq!(by_event.source_w, 32);

    // by (monitor, clip_token)
    let by_clip = repo::event_synopsis::find_by_monitor_clip(&db, monitor_id, &clip_token)
        .await
        .unwrap()
        .expect("row by clip token");
    assert_eq!(by_clip.id, row.id);

    // transition to ready, with a rendered path
    repo::event_synopsis::update_status(
        &db,
        row.id,
        SynopsisStatus::Ready,
        Some("/tmp/out.mp4".to_string()),
    )
    .await
    .unwrap();
    let reloaded = repo::event_synopsis::find_by_id(&db, row.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(reloaded.status, SynopsisStatus::Ready);
    assert_eq!(reloaded.rendered_path.as_deref(), Some("/tmp/out.mp4"));

    // not expired (expires_at is NULL), so find_expired in the far future is empty for it
    let future = utc_now() + SignedDuration::from_hours(3650 * 24);
    let expired = repo::event_synopsis::find_expired(&db, future)
        .await
        .unwrap();
    assert!(
        !expired.iter().any(|r| r.id == row.id),
        "a NULL-expiry row must never be reported expired"
    );
}

// ---------------------------------------------------------------------------
// ingest — drive EventIngestor end-to-end over the channel
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires the test database (APP_PROFILE=test-db)"]
async fn ingest_review_assets_persists_and_reconciles() {
    let fixture_db = get_test_db().await.expect("fixture db");
    ensure_schema(&fixture_db).await;
    let ingest_db = get_test_db().await.expect("ingest db");

    let monitor_id = 990_702;
    let _guard = guard_synopsis_for_monitor(monitor_id);
    let clip_token = unique_name("clip");

    let ingestor = EventIngestor::new(
        Arc::new(ingest_db),
        IngestConfig::default(),
        SynopsisConfig::default(),
        None,
    );
    let (tx, rx) = mpsc::channel(8);
    let handle = tokio::spawn(ingestor.run(rx));

    // First manifest arrives unreconciled (event_id == 0).
    let ev1 = MonitorEvent {
        code: EVENT_REVIEW_ASSETS,
        wall_clock_us: Some(1_782_129_185_000_000),
        json_detail: Some(manifest_json(monitor_id, 0, &clip_token)),
        ..Default::default()
    };
    tx.send(MonitorEventEnvelope {
        monitor_id,
        event: ev1,
        reply: ControlReply::detached(),
    })
    .await
    .unwrap();

    // Second manifest for the SAME clip token now carries the assigned event id.
    let assigned = 990_802_u64;
    let ev2 = MonitorEvent {
        code: EVENT_REVIEW_ASSETS,
        wall_clock_us: Some(1_782_129_186_000_000),
        json_detail: Some(manifest_json(monitor_id, assigned, &clip_token)),
        ..Default::default()
    };
    tx.send(MonitorEventEnvelope {
        monitor_id,
        event: ev2,
        reply: ControlReply::detached(),
    })
    .await
    .unwrap();

    drop(tx); // closes the channel → run() drains and returns
    handle.await.expect("ingest task joins");

    // Exactly one row for the (monitor, clip_token) key, with the event id
    // reconciled in by the second manifest.
    let row = repo::event_synopsis::find_by_monitor_clip(&fixture_db, monitor_id, &clip_token)
        .await
        .unwrap()
        .expect("ingested synopsis row");
    assert_eq!(
        row.event_id,
        Some(assigned),
        "second manifest reconciles event id"
    );
    assert_eq!(row.status, SynopsisStatus::Pending);
    assert_eq!(row.tube_count, 1);
    assert_eq!(row.source_w, 32);
    assert_eq!(row.asset_dir, "/nonexistent/clip/synopsis");
}

// ---------------------------------------------------------------------------
// service render — SynopsisService composites a still over a black plate
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires the test database (APP_PROFILE=test-db)"]
async fn synopsis_service_renders_still_over_black() {
    let fixture_db = get_test_db().await.expect("fixture db");
    ensure_schema(&fixture_db).await;
    let svc_db = get_test_db().await.expect("svc db");

    let monitor_id = 990_703;
    let _guard = guard_synopsis_for_monitor(monitor_id);
    let clip_token = unique_name("clip");
    let event_id = 990_803_u64;
    insert_synopsis_row(
        &fixture_db,
        monitor_id,
        Some(event_id),
        &clip_token,
        SynopsisStatus::Pending,
    )
    .await;

    let config = SynopsisConfig {
        enabled: true,
        ..Default::default()
    };
    let service = SynopsisService::new(Arc::new(svc_db), config);

    let bytes = service
        .still_for_event(event_id)
        .await
        .expect("render still");
    assert!(bytes.len() > 2, "still has content");
    assert_eq!(&bytes[0..2], &[0xFF, 0xD8], "JPEG SOI marker");

    // No row for a different event → NotFound.
    let err = service.still_for_event(MISSING_EVENT_ID).await.unwrap_err();
    assert!(matches!(
        err,
        zm_api::service::synopsis::SynopsisError::NotFound
    ));
}

#[tokio::test]
#[ignore = "requires the test database (APP_PROFILE=test-db)"]
async fn synopsis_service_computes_layout_from_row() {
    let fixture_db = get_test_db().await.expect("fixture db");
    ensure_schema(&fixture_db).await;
    let svc_db = get_test_db().await.expect("svc db");

    let monitor_id = 990_704;
    let _guard = guard_synopsis_for_monitor(monitor_id);
    let clip_token = unique_name("clip");
    let event_id = 990_804_u64;
    insert_synopsis_row(
        &fixture_db,
        monitor_id,
        Some(event_id),
        &clip_token,
        SynopsisStatus::Pending,
    )
    .await;

    let service = SynopsisService::new(
        Arc::new(svc_db),
        SynopsisConfig {
            enabled: true,
            ..Default::default()
        },
    );

    // The stored manifest has a single tube → one placement, non-empty length.
    let layout = service
        .layout_for_event(event_id, &[])
        .await
        .expect("compute layout");
    assert_eq!(layout.placements.len(), 1);
    assert_eq!(layout.placements[0].track_id, 1);
    assert!(layout.length_us > 0);

    // Class filter that excludes class 0 drops the only tube.
    let filtered = service
        .layout_for_event(event_id, &[99])
        .await
        .expect("compute filtered layout");
    assert!(filtered.placements.is_empty(), "class filter removes tube");
}

// ---------------------------------------------------------------------------
// render queue — request a render, poll to a terminal state, validate the mp4
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires the test database (APP_PROFILE=test-db)"]
async fn render_queue_produces_mp4_or_reports_failure() {
    let fixture_db = get_test_db().await.expect("fixture db");
    ensure_schema(&fixture_db).await;
    let svc_db = get_test_db().await.expect("svc db");

    let monitor_id = 990_705;
    let _guard = guard_synopsis_for_monitor(monitor_id);
    let clip_token = unique_name("clip");
    let event_id = 990_805_u64;
    insert_synopsis_row(
        &fixture_db,
        monitor_id,
        Some(event_id),
        &clip_token,
        SynopsisStatus::Pending,
    )
    .await;

    let cache_dir = std::env::temp_dir().join(format!(
        "zmapi-synopsis-{}-{}",
        std::process::id(),
        event_id
    ));
    let config = SynopsisConfig {
        enabled: true,
        cache_dir: cache_dir.clone(),
        render_timeout_seconds: 30,
        max_concurrent_renders: 1,
        output_fps: 8,
        ..Default::default()
    };
    let service = SynopsisService::new(Arc::new(svc_db), config);

    // Request the render; it runs in the background.
    let view = service
        .render_or_get(event_id)
        .await
        .expect("request render");
    let mut status = view.status;

    // Poll until a terminal state (Ready/Failed), up to ~20s.
    for _ in 0..100 {
        if matches!(status, SynopsisStatus::Ready | SynopsisStatus::Failed) {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        status = service
            .status_for_event(event_id)
            .await
            .expect("poll status")
            .status;
    }

    match status {
        SynopsisStatus::Ready => {
            let path = service
                .mp4_path_for_event(event_id)
                .await
                .expect("ready → mp4 path");
            let bytes = std::fs::read(&path).expect("read rendered mp4");
            assert!(bytes.len() > 64, "mp4 has content");
            assert_eq!(&bytes[4..8], b"ftyp", "ISO-BMFF ftyp box present");
        }
        // No H.264 encoder in this ffmpeg build → graceful failure, not a hang.
        SynopsisStatus::Failed => {}
        other => panic!("render never reached a terminal state (stuck at {other:?})"),
    }

    let _ = std::fs::remove_dir_all(&cache_dir);
}

// ---------------------------------------------------------------------------
// HTTP endpoint — auth, ACL not-found, and the disabled gate
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires the test database (APP_PROFILE=test-db)"]
async fn review_for_unknown_event_is_not_found() {
    let app = TestApp::spawn().await;
    let token = superuser_token();
    let resp = app
        .get(
            &format!("/api/v3/events/{}/synopsis/review", MISSING_EVENT_ID),
            &token,
        )
        .await;
    assert_eq!(
        resp.status(),
        StatusCode::NOT_FOUND,
        "unknown event synopsis should 404; body: {}",
        resp.text()
    );
}

#[tokio::test]
#[ignore = "requires the test database (APP_PROFILE=test-db)"]
async fn review_without_token_is_unauthorized() {
    let app = TestApp::spawn().await;
    let resp = app
        .request(
            Method::GET,
            &format!("/api/v3/events/{}/synopsis/review", MISSING_EVENT_ID),
        )
        .send()
        .await;
    assert_eq!(
        resp.status(),
        StatusCode::UNAUTHORIZED,
        "missing token should 401; body: {}",
        resp.text()
    );
}

#[tokio::test]
#[ignore = "requires the test database (APP_PROFILE=test-db)"]
async fn review_for_authorized_event_reports_disabled_by_default() {
    // The default test config leaves [synopsis].enabled = false. An authorized
    // request for a real event therefore clears ACL and reaches the service,
    // which reports the feature disabled (503) rather than rendering.
    let app = TestApp::spawn().await;
    let token = superuser_token();

    let monitor = insert_monitor(&app.db, "SynopsisDisabled")
        .await
        .expect("insert monitor");
    let _mon = RowGuard::monitor(monitor.id);
    let event = zm_api::entity::events::ActiveModel {
        monitor_id: Set(monitor.id),
        state_id: Set(1),
        name: Set(unique_name("SynopsisDisabled")),
        ..Default::default()
    }
    .insert(&app.db)
    .await
    .expect("insert event");
    let _evt = guard_event(event.id);

    let resp = app
        .get(
            &format!("/api/v3/events/{}/synopsis/review", event.id),
            &token,
        )
        .await;
    assert_eq!(
        resp.status(),
        StatusCode::SERVICE_UNAVAILABLE,
        "synopsis disabled by default should 503 for an authorized event; body: {}",
        resp.text()
    );
}

// ---------------------------------------------------------------------------
// P4 — range/overview montage + retention cleanup
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires the test database (APP_PROFILE=test-db)"]
async fn overview_merges_events_in_window() {
    let fixture_db = get_test_db().await.expect("fixture db");
    ensure_schema(&fixture_db).await;
    let svc_db = get_test_db().await.expect("svc db");

    let monitor_id = 990_706;
    let _guard = guard_synopsis_for_monitor(monitor_id);
    // Two synopses for the same monitor, different events (distinct clip tokens
    // — the (monitor_id, clip_token) key is unique).
    insert_synopsis_row(
        &fixture_db,
        monitor_id,
        Some(990_806),
        &unique_name("clip-a"),
        SynopsisStatus::Ready,
    )
    .await;
    insert_synopsis_row(
        &fixture_db,
        monitor_id,
        Some(990_807),
        &unique_name("clip-b"),
        SynopsisStatus::Ready,
    )
    .await;

    let service = SynopsisService::new(
        Arc::new(svc_db),
        SynopsisConfig {
            enabled: true,
            ..Default::default()
        },
    );

    let from = utc_now() - SignedDuration::from_hours(1);
    let to = utc_now() + SignedDuration::from_hours(1);
    let jpeg = service
        .overview_still(monitor_id, from, to, vec![])
        .await
        .expect("overview still");
    assert_eq!(&jpeg[0..2], &[0xFF, 0xD8], "overview is a JPEG");

    // A window with no synopses → NotFound.
    let empty_from = utc_now() + SignedDuration::from_hours(3650 * 24);
    let empty_to = empty_from + SignedDuration::from_hours(1);
    let err = service
        .overview_still(monitor_id, empty_from, empty_to, vec![])
        .await
        .unwrap_err();
    assert!(matches!(
        err,
        zm_api::service::synopsis::SynopsisError::NotFound
    ));
}

#[tokio::test]
#[ignore = "requires the test database (APP_PROFILE=test-db)"]
async fn retention_removes_expired_rows_and_cached_mp4() {
    let fixture_db = get_test_db().await.expect("fixture db");
    ensure_schema(&fixture_db).await;
    let svc_db = get_test_db().await.expect("svc db");

    let monitor_id = 990_707;
    let _guard = guard_synopsis_for_monitor(monitor_id);

    // A cache dir with a rendered mp4, and a row that expired yesterday.
    let cache_dir = std::env::temp_dir().join(format!(
        "zmapi-synopsis-retention-{}-{}",
        std::process::id(),
        monitor_id
    ));
    std::fs::create_dir_all(&cache_dir).expect("mk cache dir");
    let mp4_path = cache_dir.join("event-990808.mp4");
    std::fs::write(&mp4_path, b"\x00\x00\x00\x18ftypmp42fake").expect("write fake mp4");

    let now = utc_now();
    let row = event_synopsis::ActiveModel {
        event_id: Set(Some(990_808)),
        monitor_id: Set(monitor_id),
        clip_token: Set(unique_name("clip")),
        manifest_json: Set(manifest_json(monitor_id, 990_808, "tok")),
        asset_dir: Set("/nonexistent/clip/synopsis".to_string()),
        status: Set(SynopsisStatus::Ready),
        rendered_path: Set(Some(mp4_path.to_string_lossy().into_owned())),
        tube_count: Set(1),
        source_w: Set(32),
        source_h: Set(24),
        created_at: Set(db_datetime(now - SignedDuration::from_hours(8 * 24))),
        expires_at: Set(Some(db_datetime(now - SignedDuration::from_hours(24)))), // expired yesterday
        ..Default::default()
    }
    .insert(&fixture_db)
    .await
    .expect("insert expired row");

    let service = SynopsisService::new(
        Arc::new(svc_db),
        SynopsisConfig {
            enabled: true,
            cache_dir: cache_dir.clone(),
            ..Default::default()
        },
    );

    let cleaned = service.run_retention_once().await.expect("retention pass");
    assert!(cleaned >= 1, "at least the expired row is cleaned");

    // The mp4 (inside cache_dir) and the row are both gone.
    assert!(!mp4_path.exists(), "expired cached mp4 removed");
    let gone = repo::event_synopsis::find_by_id(&fixture_db, row.id)
        .await
        .unwrap();
    assert!(gone.is_none(), "expired row removed");

    let _ = std::fs::remove_dir_all(&cache_dir);
}
