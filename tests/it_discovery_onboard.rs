//! Onboarding a discovered camera adds a monitor, so it needs the same
//! Monitors Create (or System Edit) as `POST /monitors`.
//!
//!   APP_PROFILE=test-db cargo test --test it_discovery_onboard -- --include-ignored

mod common;

use axum::http::StatusCode;
use common::harness::{token_with, TestApp};
use serde_json::json;
use zm_api::util::authz::{Level, UserPermissions};

#[tokio::test]
#[ignore = "requires the test database (APP_PROFILE=test-db)"]
async fn onboarding_needs_monitor_create() {
    let app = TestApp::spawn().await;
    // Loopback is refused by the target check, so nothing is contacted.
    let body = json!({
        "xaddr": "http://127.0.0.1/onvif/device_service",
        "username": "", "password": "",
    });

    let editor = UserPermissions {
        monitors: Level::Edit,
        ..Default::default()
    };
    let r = app
        .post_json("/api/v3/discovery/onboard", &token_with(editor), &body)
        .await;
    assert_eq!(r.status(), StatusCode::FORBIDDEN, "{}", r.text());
    assert!(r.text().contains("Create"), "{}", r.text());

    // With Create it gets past the permission check to the target check.
    let creator = UserPermissions {
        monitors: Level::Create,
        ..Default::default()
    };
    let r = app
        .post_json("/api/v3/discovery/onboard", &token_with(creator), &body)
        .await;
    assert!(!r.text().contains("Create access"), "{}", r.text());
}
