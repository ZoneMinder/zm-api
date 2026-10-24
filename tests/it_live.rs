//! Integration tests for the live-streaming API (`src/handlers/live.rs`).
//!
//! Exercises the non-WebSocket endpoints through the full middleware + routing
//! stack via `oneshot`:
//!   - per-monitor: `/live/{id}/{stats,hls/*}`
//!   - global:      `/live/sessions`, `/live/sources`
//!   - snapshot:    `/monitors/{id}/snapshot`
//!
//! The test `AppState` is built with no streaming services wired (see
//! `AppState::for_test_with_db`), so any request that clears authentication
//! reaches a handler that returns `503 Service Unavailable`. The tests assert
//! on that contract: auth gating yields `401`, and a cleared request never
//! yields `200` for a stream-less monitor.
//!
//! The `/webrtc/ws` WebSocket-upgrade route is in `it_live_ws.rs`, which
//! serves a real socket: `oneshot` cannot drive a protocol upgrade.
//!
//! Requires the test database — run with:
//!   APP_PROFILE=test-db cargo test --test it_live -- --include-ignored

mod common;

use axum::http::{Method, StatusCode};
use common::assertions::assert_error;
use common::fixtures::{insert_monitor, RowGuard};
use common::harness::{superuser_token, TestApp};

/// A monitor id far outside the range any real ZoneMinder row would use.
const MISSING_MONITOR_ID: u32 = 999_000_111;

/// Assert the response is an error status (4xx or 5xx) and explicitly not 200.
fn assert_is_error(resp: &common::harness::TestResponse, ctx: &str) {
    assert!(
        resp.status().is_client_error() || resp.status().is_server_error(),
        "{ctx}: expected a 4xx/5xx error status, got {}; body: {}",
        resp.status(),
        resp.text()
    );
}

// ---------------------------------------------------------------------------
// unauthenticated: no token at all -> 401
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires the test database (APP_PROFILE=test-db)"]
async fn live_endpoints_reject_unauthenticated_access() {
    let app = TestApp::spawn().await;

    let per_monitor = [
        format!("/api/v3/live/{MISSING_MONITOR_ID}/stats"),
        format!("/api/v3/live/{MISSING_MONITOR_ID}/hls/master.m3u8"),
        format!("/api/v3/live/{MISSING_MONITOR_ID}/hls/live.m3u8"),
        format!("/api/v3/live/{MISSING_MONITOR_ID}/hls/init.mp4"),
        format!("/api/v3/live/{MISSING_MONITOR_ID}/hls/segment_00001.m4s"),
    ];
    for path in &per_monitor {
        let resp = app.request(Method::GET, path).send().await;
        assert_error(&resp, StatusCode::UNAUTHORIZED, "UNAUTHORIZED_ERROR");
    }

    for path in [
        "/api/v3/live/sessions",
        "/api/v3/live/sources",
        &format!("/api/v3/monitors/{MISSING_MONITOR_ID}/snapshot"),
    ] {
        let resp = app.request(Method::GET, path).send().await;
        assert_eq!(
            resp.status(),
            StatusCode::UNAUTHORIZED,
            "unauthenticated {path} should be 401; body: {}",
            resp.text()
        );
    }
}

#[tokio::test]
#[ignore = "requires the test database (APP_PROFILE=test-db)"]
async fn stop_live_stream_rejects_unauthenticated_access() {
    let app = TestApp::spawn().await;
    let resp = app
        .request(
            Method::DELETE,
            &format!("/api/v3/live/{MISSING_MONITOR_ID}/stop"),
        )
        .send()
        .await;
    assert_error(&resp, StatusCode::UNAUTHORIZED, "UNAUTHORIZED_ERROR");
}

// ---------------------------------------------------------------------------
// global endpoints: authenticated, but streaming is not configured in tests
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires the test database (APP_PROFILE=test-db)"]
async fn list_live_sessions_reports_service_unavailable() {
    let app = TestApp::spawn().await;
    let token = superuser_token();

    // With no `live_coordinator` wired into the test state the handler returns
    // 503. A cleared-auth request must never 401 here.
    let resp = app.get("/api/v3/live/sessions", &token).await;
    assert_eq!(
        resp.status(),
        StatusCode::SERVICE_UNAVAILABLE,
        "sessions without a coordinator should be 503; body: {}",
        resp.text()
    );
}

#[tokio::test]
#[ignore = "requires the test database (APP_PROFILE=test-db)"]
async fn list_live_sources_reports_service_unavailable() {
    let app = TestApp::spawn().await;
    let token = superuser_token();

    let resp = app.get("/api/v3/live/sources", &token).await;
    assert_eq!(
        resp.status(),
        StatusCode::SERVICE_UNAVAILABLE,
        "sources without a coordinator should be 503; body: {}",
        resp.text()
    );
}

// ---------------------------------------------------------------------------
// per-monitor HLS / stats on a stream-less monitor: never 200
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires the test database (APP_PROFILE=test-db)"]
async fn hls_and_stats_for_streamless_monitor_error() {
    let app = TestApp::spawn().await;
    let token = superuser_token();
    let monitor = insert_monitor(&app.db, "LiveStreamless")
        .await
        .expect("insert monitor fixture");
    let _mon = RowGuard::monitor(monitor.id);

    let paths = [
        format!("/api/v3/live/{}/stats", monitor.id),
        format!("/api/v3/live/{}/hls/master.m3u8", monitor.id),
        format!("/api/v3/live/{}/hls/live.m3u8", monitor.id),
        format!("/api/v3/live/{}/hls/init.mp4", monitor.id),
        format!("/api/v3/live/{}/hls/segment_00001.m4s", monitor.id),
    ];
    for path in &paths {
        let resp = app.get(path, &token).await;
        assert_is_error(&resp, path);
        assert_ne!(
            resp.status(),
            StatusCode::OK,
            "{path}: a stream-less monitor must not yield 200"
        );
    }
}

#[tokio::test]
#[ignore = "requires the test database (APP_PROFILE=test-db)"]
async fn invalid_hls_segment_name_is_rejected() {
    let app = TestApp::spawn().await;
    let token = superuser_token();
    let monitor = insert_monitor(&app.db, "LiveBadSegment")
        .await
        .expect("insert monitor fixture");
    let _mon = RowGuard::monitor(monitor.id);

    // A garbage segment name fails `parse_segment_sequence` -> 4xx error.
    let resp = app
        .get(
            &format!("/api/v3/live/{}/hls/not-a-segment.txt", monitor.id),
            &token,
        )
        .await;
    assert_is_error(&resp, "invalid segment name");
}

// ---------------------------------------------------------------------------
// non-existent monitor: handlers still return an error status, not 200
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires the test database (APP_PROFILE=test-db)"]
async fn hls_and_stats_for_unknown_monitor_error() {
    let app = TestApp::spawn().await;
    let token = superuser_token();

    let paths = [
        format!("/api/v3/live/{MISSING_MONITOR_ID}/stats"),
        format!("/api/v3/live/{MISSING_MONITOR_ID}/hls/master.m3u8"),
        format!("/api/v3/live/{MISSING_MONITOR_ID}/hls/live.m3u8"),
        format!("/api/v3/live/{MISSING_MONITOR_ID}/hls/init.mp4"),
    ];
    for path in &paths {
        let resp = app.get(path, &token).await;
        assert_is_error(&resp, path);
        assert_ne!(resp.status(), StatusCode::OK, "{path}: must not yield 200");
    }
}

// ---------------------------------------------------------------------------
// snapshot endpoint
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires the test database (APP_PROFILE=test-db)"]
async fn snapshot_for_streamless_monitor_errors() {
    let app = TestApp::spawn().await;
    let token = superuser_token();
    let monitor = insert_monitor(&app.db, "LiveSnapshot")
        .await
        .expect("insert monitor fixture");
    let _mon = RowGuard::monitor(monitor.id);

    // No snapshot service is wired in the test state -> the handler errors
    // rather than returning a JPEG.
    let resp = app
        .get(&format!("/api/v3/monitors/{}/snapshot", monitor.id), &token)
        .await;
    assert_is_error(&resp, "snapshot for stream-less monitor");
    assert_ne!(
        resp.status(),
        StatusCode::OK,
        "snapshot without a service must not yield 200"
    );
}

#[tokio::test]
#[ignore = "requires the test database (APP_PROFILE=test-db)"]
async fn snapshot_without_token_is_unauthorized() {
    let app = TestApp::spawn().await;
    let resp = app
        .request(
            Method::GET,
            &format!("/api/v3/monitors/{MISSING_MONITOR_ID}/snapshot"),
        )
        .send()
        .await;
    assert_eq!(
        resp.status(),
        StatusCode::UNAUTHORIZED,
        "unauthenticated snapshot should be 401; body: {}",
        resp.text()
    );
}

// ---- WHEP (RFC 9725) -------------------------------------------------------

const OFFER: &str =
    "v=0\r\no=- 1 1 IN IP4 0.0.0.0\r\ns=-\r\nt=0 0\r\nm=video 9 UDP/TLS/RTP/SAVPF 96\r\n";

#[tokio::test]
#[ignore = "requires the test database (APP_PROFILE=test-db)"]
async fn whep_needs_a_header_token() {
    let app = TestApp::spawn().await;
    let resp = app
        .request(Method::POST, "/api/v3/live/1/webrtc/whep")
        .raw_body(OFFER, "application/sdp")
        .send()
        .await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
#[ignore = "requires the test database (APP_PROFILE=test-db)"]
async fn whep_takes_only_sdp() {
    let app = TestApp::spawn().await;
    let resp = app
        .request(Method::POST, "/api/v3/live/1/webrtc/whep")
        .bearer(&superuser_token())
        .raw_body(OFFER, "application/json")
        .send()
        .await;
    assert_eq!(resp.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
}

#[tokio::test]
#[ignore = "requires the test database (APP_PROFILE=test-db)"]
async fn whep_without_live_streaming_is_unavailable() {
    // The test state has no stream-socket router.
    let app = TestApp::spawn().await;
    let resp = app
        .request(Method::POST, "/api/v3/live/1/webrtc/whep")
        .bearer(&superuser_token())
        .raw_body(OFFER, "application/sdp")
        .send()
        .await;
    assert_eq!(
        resp.status(),
        StatusCode::SERVICE_UNAVAILABLE,
        "{}",
        resp.text()
    );
}

#[tokio::test]
#[ignore = "requires the test database (APP_PROFILE=test-db)"]
async fn an_unknown_whep_session_is_not_found() {
    let app = TestApp::spawn().await;
    let path = "/api/v3/live/1/webrtc/whep/00000000-0000-0000-0000-000000000000";
    let resp = app
        .request(Method::PATCH, path)
        .bearer(&superuser_token())
        .raw_body("a=end-of-candidates\r\n", "application/trickle-ice-sdpfrag")
        .send()
        .await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    let resp = app
        .request(Method::DELETE, path)
        .bearer(&superuser_token())
        .send()
        .await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

/// Stopping a live session ends it for every viewer, so it needs Monitors
/// Edit on that monitor. Stream View alone used to be enough.
#[tokio::test]
#[ignore = "requires the test database (APP_PROFILE=test-db)"]
async fn stopping_a_live_session_needs_monitor_edit() {
    use common::fixtures::{grant_monitor_permission, insert_user_with_id};
    use common::harness::{token_for, token_with};
    use zm_api::entity::sea_orm_active_enums::Permission;
    use zm_api::util::authz::{Level, UserPermissions};

    let app = TestApp::spawn().await;
    let monitor = insert_monitor(&app.db, "LiveStop").await.expect("monitor");
    let _m = RowGuard::monitor(monitor.id);
    let stop = format!("/api/v3/live/{}/stop", monitor.id);

    let viewer = UserPermissions {
        stream: Level::View,
        monitors: Level::View,
        ..Default::default()
    };
    let editor = UserPermissions {
        monitors: Level::Edit,
        ..viewer
    };

    let r = app.delete(&stop, &token_with(viewer)).await;
    assert_error(&r, StatusCode::FORBIDDEN, "PERMISSION_DENIED_ERROR");

    // Through the check: no live streaming in tests, so 503.
    let r = app.delete(&stop, &token_with(editor)).await;
    assert_eq!(r.status(), StatusCode::SERVICE_UNAVAILABLE, "{}", r.text());

    // Monitors Edit overall, but only View on this monitor.
    const UID: u32 = 990_501;
    insert_user_with_id(&app.db, UID, "LiveStop")
        .await
        .expect("user");
    let _u = RowGuard::user(UID);
    grant_monitor_permission(&app.db, monitor.id, UID, Permission::View)
        .await
        .expect("grant");
    let r = app.delete(&stop, &token_for(UID, editor)).await;
    assert_error(&r, StatusCode::FORBIDDEN, "PERMISSION_DENIED_ERROR");
}
