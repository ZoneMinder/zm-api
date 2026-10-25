//! Integration tests for the Monitor Status API — list, get and patch.
//!
//! Requires the test database — run with:
//!   APP_PROFILE=test-db cargo test --test it_monitor_status -- --include-ignored

mod common;

use axum::http::StatusCode;
use common::assertions::{assert_error, assert_status};
use common::fixtures::{insert_monitor, RowGuard};
use common::harness::{superuser_token, TestApp};
use sea_orm::prelude::Decimal;
use sea_orm::{ActiveModelTrait, EntityTrait, Set};
use serde_json::json;
use zm_api::dto::response::{MonitorStatusResponse, PaginatedMonitorStatusesResponse};
use zm_api::entity::sea_orm_active_enums::Status;

/// Insert a `Monitor_Status` row for a monitor.
async fn insert_status(db: &sea_orm::DatabaseConnection, monitor_id: u32) {
    zm_api::entity::monitor_status::ActiveModel {
        monitor_id: Set(monitor_id),
        status: Set(Status::Connected),
        capture_fps: Set(Decimal::new(1500, 2)),
        analysis_fps: Set(Decimal::new(1480, 2)),
        capture_bandwidth: Set(1024),
        // Local wall-clock, as zmc's NOW() writes it in a SYSTEM-zone session.
        updated_on: Set(zm_api::util::utc_to_timestamp(chrono::Utc::now())),
    }
    .insert(db)
    .await
    .expect("insert monitor_status fixture");
}

/// Guard a `Monitor_Status` row (keyed by monitor id).
fn status_guard(monitor_id: u32) -> RowGuard {
    RowGuard::new(
        format!("Monitor_Status#{monitor_id}"),
        move |db| async move {
            let _ = zm_api::entity::monitor_status::Entity::delete_by_id(monitor_id)
                .exec(&db)
                .await;
        },
    )
}

#[tokio::test]
#[ignore = "requires the test database (APP_PROFILE=test-db)"]
async fn list_monitor_statuses_returns_inserted_row() {
    let app = TestApp::spawn().await;
    let token = superuser_token();
    let monitor = insert_monitor(&app.db, "StatusList")
        .await
        .expect("insert monitor");
    let _mon = RowGuard::monitor(monitor.id);
    insert_status(&app.db, monitor.id).await;
    let _status = status_guard(monitor.id);

    let resp = app
        .get("/api/v3/monitor-status?page=1&page_size=1000", &token)
        .await;
    assert_status(&resp, StatusCode::OK);
    let body: PaginatedMonitorStatusesResponse = resp.json();
    assert!(
        body.items.iter().any(|s| s.monitor_id == monitor.id),
        "monitor status list should contain the fixture row"
    );
}

#[tokio::test]
#[ignore = "requires the test database (APP_PROFILE=test-db)"]
async fn get_monitor_status_returns_the_row() {
    let app = TestApp::spawn().await;
    let token = superuser_token();
    let monitor = insert_monitor(&app.db, "StatusGet")
        .await
        .expect("insert monitor");
    let _mon = RowGuard::monitor(monitor.id);
    insert_status(&app.db, monitor.id).await;
    let _status = status_guard(monitor.id);

    let resp = app
        .get(&format!("/api/v3/monitor-status/{}", monitor.id), &token)
        .await;
    assert_status(&resp, StatusCode::OK);
    let body: MonitorStatusResponse = resp.json();
    assert_eq!(body.monitor_id, monitor.id);
    assert_eq!(body.status, "Connected");
    // A row written just now must read back as just now, not shifted by the
    // server's UTC offset: zm-web marks a monitor Offline when it is older
    // than 90 s.
    let updated = chrono::DateTime::parse_from_rfc3339(&body.updated_on).expect("RFC 3339");
    let age = chrono::Utc::now()
        .signed_duration_since(updated)
        .num_seconds()
        .abs();
    assert!(
        age < 60,
        "updated_on {} is {age}s from now",
        body.updated_on
    );
}

#[tokio::test]
#[ignore = "requires the test database (APP_PROFILE=test-db)"]
async fn get_missing_monitor_status_is_not_found() {
    let app = TestApp::spawn().await;
    let token = superuser_token();

    let resp = app.get("/api/v3/monitor-status/999000111", &token).await;
    assert_error(&resp, StatusCode::NOT_FOUND, "MONITOR_NOT_FOUND_ERROR");
}

#[tokio::test]
#[ignore = "requires the test database (APP_PROFILE=test-db)"]
async fn patch_monitor_status_updates_the_row() {
    let app = TestApp::spawn().await;
    let token = superuser_token();
    let monitor = insert_monitor(&app.db, "StatusPatch")
        .await
        .expect("insert monitor");
    let _mon = RowGuard::monitor(monitor.id);
    insert_status(&app.db, monitor.id).await;
    let _status = status_guard(monitor.id);

    let resp = app
        .patch_json(
            &format!("/api/v3/monitor-status/{}", monitor.id),
            &token,
            &json!({ "status": "Running", "capture_bandwidth": 2048 }),
        )
        .await;
    assert_status(&resp, StatusCode::OK);
    let body: MonitorStatusResponse = resp.json();
    assert_eq!(body.status, "Running");
    assert_eq!(body.capture_bandwidth, 2048);
}

#[tokio::test]
#[ignore = "requires the test database (APP_PROFILE=test-db)"]
async fn patch_monitor_status_with_invalid_body_is_rejected() {
    let app = TestApp::spawn().await;
    let token = superuser_token();
    let monitor = insert_monitor(&app.db, "StatusBadPatch")
        .await
        .expect("insert monitor");
    let _mon = RowGuard::monitor(monitor.id);
    insert_status(&app.db, monitor.id).await;
    let _status = status_guard(monitor.id);

    // `capture_bandwidth` must be an integer, not a string.
    let resp = app
        .patch_json(
            &format!("/api/v3/monitor-status/{}", monitor.id),
            &token,
            &json!({ "capture_bandwidth": "not-a-number" }),
        )
        .await;
    assert!(
        resp.status().is_client_error(),
        "malformed patch body should be a 4xx, got {}; body: {}",
        resp.status(),
        resp.text()
    );
}
