//! Integration tests for the PTZ API.
//!
//! PTZ movement endpoints actuate real camera hardware, so this suite covers
//! only the hardware-independent surface: the static protocol list and the
//! not-found paths for status/capabilities on a missing monitor.
//!
//! Requires the test database — run with:
//!   APP_PROFILE=test-db cargo test --test it_ptz -- --include-ignored

mod common;

use axum::http::StatusCode;
use common::assertions::{assert_error, assert_status};
use common::harness::{superuser_token, TestApp};

#[tokio::test]
#[ignore = "requires the test database (APP_PROFILE=test-db)"]
async fn list_ptz_protocols_succeeds() {
    let app = TestApp::spawn().await;
    let token = superuser_token();

    let resp = app.get("/api/v3/ptz/protocols", &token).await;
    assert_status(&resp, StatusCode::OK);
    let body: serde_json::Value = resp.json();
    assert!(
        body.get("protocols").map(|p| p.is_array()).unwrap_or(false),
        "protocols response should carry a `protocols` array; body: {}",
        resp.text()
    );
}

#[tokio::test]
#[ignore = "requires the test database (APP_PROFILE=test-db)"]
async fn get_status_for_missing_monitor_is_not_found() {
    let app = TestApp::spawn().await;
    let token = superuser_token();

    let resp = app
        .get("/api/v3/ptz/monitors/999000111/status", &token)
        .await;
    assert_error(&resp, StatusCode::NOT_FOUND, "MONITOR_NOT_FOUND_ERROR");
}

#[tokio::test]
#[ignore = "requires the test database (APP_PROFILE=test-db)"]
async fn get_capabilities_for_missing_monitor_is_not_found() {
    let app = TestApp::spawn().await;
    let token = superuser_token();

    let resp = app
        .get("/api/v3/ptz/monitors/999000111/capabilities", &token)
        .await;
    assert_error(&resp, StatusCode::NOT_FOUND, "MONITOR_NOT_FOUND_ERROR");
}

/// A control address that reaches this server is refused before any request
/// is sent. `ControlAddress` is set by anyone with Monitors Edit.
#[tokio::test]
#[ignore = "requires the test database (APP_PROFILE=test-db)"]
async fn ptz_refuses_a_control_address_on_this_server() {
    use common::fixtures::{unique_name, RowGuard};
    use serde_json::json;

    let app = TestApp::spawn().await;
    let token = superuser_token();
    let control = app
        .post_json(
            "/api/v3/controls",
            &token,
            &json!({ "name": unique_name("PtzLoop"), "protocol": "onvif", "can_move": 1,
                     "can_move_con": 1 }),
        )
        .await;
    assert!(control.status().is_success(), "{}", control.text());
    let control_id = control.json::<serde_json::Value>()["id"].as_u64().unwrap() as u32;
    let _c = RowGuard::control(control_id);

    for address in [
        "http://127.0.0.1:9/onvif/ptz_service",
        "admin:x@169.254.169.254:80",
    ] {
        let monitor = app
            .post_json(
                "/api/v3/monitors",
                &token,
                &json!({ "name": unique_name("PtzLoop"), "type": "Ffmpeg", "controllable": 1,
                         "control_id": control_id, "control_address": address }),
            )
            .await;
        assert!(monitor.status().is_success(), "{}", monitor.text());
        let mid = monitor.json::<serde_json::Value>()["id"].as_u64().unwrap() as u32;
        let _m = RowGuard::monitor(mid);

        let r = app
            .post_json(
                &format!("/api/v3/ptz/monitors/{mid}/move/up"),
                &token,
                &json!({}),
            )
            .await;
        assert_eq!(
            r.status(),
            StatusCode::BAD_REQUEST,
            "{address}: {}",
            r.text()
        );
        assert!(r.text().contains("this server"), "{address}: {}", r.text());
    }
}
