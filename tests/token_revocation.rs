//! Server-side token revocation coverage.
//!
//! Runs against the mock-database harness: the revocation check in the RBAC
//! layer is purely in-memory, and logout's single `UPDATE Users` is served by
//! a mock exec result — no real database needed.

mod common;

use axum::http::{Method, StatusCode};
use common::harness::{token_for, TestApp};
use sea_orm::MockExecResult;
use zm_api::util::authz::UserPermissions;

/// Distinct uids per test: the revocation registry is per-state, but distinct
/// ids keep the tests independent of each other's minting time.
const REVOKED_UID: u32 = 991_001_001;
const LOGOUT_UID: u32 = 991_001_002;

#[tokio::test]
async fn revoked_token_is_rejected_on_protected_routes() {
    let state = common::create_test_state();
    // Mint first, then raise the floor above the token's iat.
    let token = token_for(REVOKED_UID, UserPermissions::superuser());
    state
        .revocations
        .revoke(REVOKED_UID, chrono::Utc::now().timestamp() + 1);
    let app = TestApp::from_state(state);

    let resp = app.get("/api/v3/monitors", &token).await;
    assert_eq!(
        resp.status,
        StatusCode::UNAUTHORIZED,
        "revoked token must be rejected, body: {}",
        resp.text()
    );

    // A different user with no revocation floor still clears the auth/RBAC
    // layer (the mock DB may then fail the query — anything but 401/403).
    let other = token_for(REVOKED_UID + 1, UserPermissions::superuser());
    let resp = app.get("/api/v3/monitors", &other).await;
    assert_ne!(resp.status, StatusCode::UNAUTHORIZED);
    assert_ne!(resp.status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn logout_revokes_the_current_token() {
    // One exec result for logout's `UPDATE Users SET TokenMinExpiry`.
    let state = common::create_test_state_with_exec(vec![MockExecResult {
        last_insert_id: 0,
        rows_affected: 1,
    }]);
    let app = TestApp::from_state(state);
    let token = token_for(LOGOUT_UID, UserPermissions::superuser());

    // The token authenticates its own logout...
    let resp = app.get("/api/v3/auth/logout", &token).await;
    assert_eq!(resp.status, StatusCode::OK, "body: {}", resp.text());

    // ...and is dead immediately afterwards, on every protected surface.
    let resp = app.get("/api/v3/monitors", &token).await;
    assert_eq!(
        resp.status,
        StatusCode::UNAUTHORIZED,
        "post-logout use of the token must be rejected, body: {}",
        resp.text()
    );

    // Media-style query-parameter auth is rejected too.
    let resp = app
        .request(
            Method::GET,
            &format!("/api/v3/monitors/1/snapshot?token={token}"),
        )
        .send()
        .await;
    assert_eq!(resp.status, StatusCode::UNAUTHORIZED);
}

/// Routes that answer without a login, so a revoked token changes nothing.
const PUBLIC_PATHS: &[&str] = &[
    "/api/v3/server/health_check",
    "/api/v3/host/getVersion",
    "/api/v3/auth/login",
    "/api/v3/auth/refresh",
];

/// Every documented route refuses a revoked token. Walks the OpenAPI document
/// so a route added behind a login-only layer that skips the revocation check
/// fails here (`/notifications`, `/system/menu` and `/system/locale` did).
#[tokio::test]
async fn revoked_token_is_rejected_on_every_documented_route() {
    const UID: u32 = 991_001_003;
    let state = common::create_test_state();
    let token = token_for(UID, UserPermissions::superuser());
    state
        .revocations
        .revoke(UID, chrono::Utc::now().timestamp() + 1);
    let app = TestApp::from_state(state);

    let doc = serde_json::to_value(zm_api::routes::served_openapi()).expect("openapi as json");
    let paths = doc["paths"].as_object().expect("paths");
    let mut checked = 0;
    let mut accepted = Vec::new();
    for (template, item) in paths {
        if PUBLIC_PATHS.contains(&template.as_str()) {
            continue;
        }
        // Any `{param}` becomes 1: the auth layers run before extraction.
        let mut path = String::new();
        let mut in_param = false;
        for c in template.chars() {
            match c {
                '{' => {
                    in_param = true;
                    path.push('1');
                }
                '}' => in_param = false,
                _ if !in_param => path.push(c),
                _ => {}
            }
        }
        for (name, method) in [
            ("get", Method::GET),
            ("post", Method::POST),
            ("put", Method::PUT),
            ("patch", Method::PATCH),
            ("delete", Method::DELETE),
        ] {
            if item.get(name).is_none() {
                continue;
            }
            let resp = app
                .request(method.clone(), &path)
                .bearer(&token)
                .send()
                .await;
            checked += 1;
            if resp.status != StatusCode::UNAUTHORIZED {
                accepted.push(format!("{method} {template} -> {}", resp.status));
            }
        }
    }
    assert!(checked > 100, "only {checked} routes checked");
    assert!(
        accepted.is_empty(),
        "routes that took a revoked token:\n{}",
        accepted.join("\n")
    );
}

/// A response to a URL that carries its credential (`?token=`) must not be
/// kept by a shared cache: the cache would go on serving it to anyone with
/// the URL after the token expired or was revoked.
#[tokio::test]
async fn a_response_to_a_token_in_the_url_is_private() {
    const UID: u32 = 991_001_004;
    let app = TestApp::from_state(common::create_test_state());
    let token = token_for(UID, UserPermissions::superuser());

    let resp = app
        .request(
            Method::GET,
            &format!("/api/v3/events/1/thumbnail?token={token}"),
        )
        .send()
        .await;
    let cache = resp
        .headers
        .get(axum::http::header::CACHE_CONTROL)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    assert!(
        cache.contains("private") || cache.contains("no-store"),
        "status {}, Cache-Control {cache:?}",
        resp.status
    );
}

/// JSON API answers are not to be cached anywhere, whatever their status.
#[tokio::test]
async fn api_answers_are_marked_no_store() {
    let app = TestApp::from_state(common::create_test_state());
    let token = token_for(991_001_005, UserPermissions::superuser());
    for path in ["/api/v3/monitors", "/api/v3/users", "/api/v3/me"] {
        let resp = app.get(path, &token).await;
        assert_eq!(
            resp.headers
                .get(axum::http::header::CACHE_CONTROL)
                .and_then(|v| v.to_str().ok()),
            Some("no-store"),
            "{path} answered {}",
            resp.status
        );
    }
}
