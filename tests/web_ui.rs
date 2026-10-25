//! Serving the zm-web UI from this binary (GH #58, "Option D").
//!
//! The router is built from the process-wide `CONFIG`, so these drive the
//! serving layer directly rather than standing a configured server up. The
//! property that actually matters — that the SPA fallback never swallows an API
//! path — is asserted against the real router in `spa_fallback_does_not_shadow`.

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::util::ServiceExt;

fn ui_dir(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("zm-web-{}-{}", tag, std::process::id()));
    std::fs::create_dir_all(dir.join("assets")).unwrap();
    std::fs::write(
        dir.join("index.html"),
        b"<!doctype html><title>zm-web</title>",
    )
    .unwrap();
    std::fs::write(dir.join("assets/app-a1b2c3.js"), b"console.log(1)").unwrap();
    dir
}

/// Build just the UI-serving stack the router mounts, with an explicit config.
fn ui_service(root: &std::path::Path) -> axum::Router {
    let index = root.join("index.html");
    let serve = tower_http::services::ServeDir::new(root)
        .fallback(tower_http::services::ServeFile::new(index));
    axum::Router::new().fallback_service(serve)
}

#[tokio::test]
async fn a_client_side_route_serves_index_html() {
    // `/events/123` is a zm-web route, not a file. Without the SPA fallback a
    // browser refresh on that URL 404s.
    let dir = ui_dir("spa");
    let resp = ui_service(&dir)
        .oneshot(
            Request::builder()
                .uri("/events/123")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(resp.into_body(), 64 * 1024)
        .await
        .unwrap();
    assert!(
        String::from_utf8_lossy(&body).contains("zm-web"),
        "a client-side route must fall back to index.html"
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn a_real_asset_is_served_from_disk() {
    let dir = ui_dir("asset");
    let resp = ui_service(&dir)
        .oneshot(
            Request::builder()
                .uri("/assets/app-a1b2c3.js")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(resp.into_body(), 64 * 1024)
        .await
        .unwrap();
    assert_eq!(&body[..], b"console.log(1)");
    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn path_traversal_is_refused() {
    // ServeDir resolves against the root and rejects escapes; assert it rather
    // than assume it, since the root is operator-configured.
    let dir = ui_dir("traversal");
    for uri in ["/../../etc/passwd", "/assets/../../../etc/passwd"] {
        let resp = ui_service(&dir)
            .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        let body = axum::body::to_bytes(resp.into_body(), 64 * 1024)
            .await
            .unwrap();
        assert!(
            !String::from_utf8_lossy(&body).contains("root:"),
            "{uri} escaped the UI root"
        );
    }
    std::fs::remove_dir_all(&dir).ok();
}

/// The one that would bite hardest if it regressed: with the UI mounted as the
/// router fallback, a mistyped API path must still return the JSON 404
/// envelope. If it returned `index.html` with status 200 instead, every client
/// error would look like a success and clients would try to parse HTML as JSON.
///
/// Driven against the real router, which serves the API without the UI in this
/// configuration — so this pins the API side of the boundary.
#[tokio::test]
async fn spa_fallback_does_not_shadow_api_404s() {
    use sea_orm::{DatabaseBackend, MockDatabase};

    let db = MockDatabase::new(DatabaseBackend::MySql).into_connection();
    let state = zm_api::server::state::AppState::for_test_with_db(db);
    let app = common::schema::checked(zm_api::routes::create_router_app(state));

    // Genuinely unmatched paths: these reach the fallback, so they prove the
    // fallback itself does the right thing.
    for uri in ["/api/v3/no-such-endpoint", "/api-docs/nope.json"] {
        let resp = app
            .clone()
            .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::NOT_FOUND,
            "{uri} must 404, not fall through to the UI"
        );
        let text = String::from_utf8_lossy(
            &axum::body::to_bytes(resp.into_body(), 64 * 1024)
                .await
                .unwrap(),
        )
        .into_owned();
        assert!(
            text.contains("NOT_FOUND_ERROR"),
            "{uri} must keep the JSON error envelope, got: {text}"
        );
    }

    // Paths that *do* match a route but fail earlier (auth, method) must also
    // stay JSON. `/api/v3/monitors/{id}` matches, so this one 401s rather than
    // 404ing — either is fine; HTML never is.
    for uri in [
        "/api/v3/monitors/definitely-not-a-route",
        "/api/v3/monitors",
        "/api/v3/events/1",
    ] {
        let resp = app
            .clone()
            .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = resp.status();
        assert!(
            status.is_client_error() || status.is_server_error(),
            "{uri} unexpectedly succeeded ({status}) — is the UI shadowing it?"
        );
        let text = String::from_utf8_lossy(
            &axum::body::to_bytes(resp.into_body(), 64 * 1024)
                .await
                .unwrap(),
        )
        .into_owned();
        assert!(
            !text.contains("<!doctype") && !text.contains("<html"),
            "{uri} returned HTML — the SPA fallback is shadowing the API: {text}"
        );
    }
}
