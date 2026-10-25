//! Regression tests for routes that previously registered handlers with no
//! authentication middleware. These run without a database — the auth layer
//! rejects the request before any handler (and therefore any DB query) runs.

mod common;

use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use sea_orm::{DatabaseBackend, MockDatabase};
use tower::util::ServiceExt; // for `oneshot`

fn router() -> axum::Router {
    let db = MockDatabase::new(DatabaseBackend::MySql).into_connection();
    let state = zm_api::server::state::AppState::for_test_with_db(db);
    common::schema::checked(zm_api::routes::create_router_app(state))
}

async fn status_of(method: &str, uri: &str) -> StatusCode {
    let request = Request::builder()
        .method(method)
        .uri(uri)
        .body(Body::empty())
        .unwrap();
    router().oneshot(request).await.unwrap().status()
}

#[tokio::test]
async fn daemon_routes_require_auth() {
    // System lifecycle control must never be reachable without a token.
    assert_eq!(
        status_of("GET", "/api/v3/daemons").await,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        status_of("POST", "/api/v3/system/shutdown").await,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        status_of("POST", "/api/v3/system/restart").await,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        status_of("POST", "/api/v3/daemons/1/stop").await,
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn event_routes_require_auth() {
    assert_eq!(
        status_of("GET", "/api/v3/events").await,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        status_of("GET", "/api/v3/events/1").await,
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn event_summaries_routes_require_auth() {
    assert_eq!(
        status_of("GET", "/api/v3/event-summaries").await,
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn frames_routes_require_auth() {
    // Regression: the frames routes were nested under `/frames` instead of
    // `/api/v3/frames`, so a spec-conformant request to `/api/v3/frames` hit
    // no route at all (HTTP 500 "No matched path found") rather than the
    // authenticated handler. A 401 here proves the route is wired correctly.
    assert_eq!(
        status_of("GET", "/api/v3/frames").await,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        status_of("GET", "/api/v3/frames/1/1").await,
        StatusCode::UNAUTHORIZED
    );
}
