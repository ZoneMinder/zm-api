//! A browser page load must not be rate-limited (GH #70).
//!
//! zm-web could not render a single screen: every request in a page load came
//! back 429. The cause was a burst of 0 being clamped silently to 1, so one
//! request succeeded and everything after it was refused — with a setting whose
//! name (`rate_limit_per_second`) read as a rate while meaning a period, so
//! `4` gave one request every four seconds rather than four a second.
//!
//! Drives the real router, so it tests the layer as wired rather than the
//! config in isolation.

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::util::ServiceExt;

/// How many requests one screen of a single-page app takes. zm-web's events
/// page needs about this many: the event list, monitors, storage, tags,
/// groups, /me, config, version and system status.
const REQUESTS_PER_PAGE: usize = 11;

fn router() -> axum::Router {
    use sea_orm::{DatabaseBackend, MockDatabase};
    let db = MockDatabase::new(DatabaseBackend::MySql).into_connection();
    let state = zm_api::server::state::AppState::for_test_with_db(db);
    common::schema::checked(zm_api::routes::create_router_app(state))
}

fn ci() -> axum::extract::ConnectInfo<std::net::SocketAddr> {
    axum::extract::ConnectInfo(std::net::SocketAddr::from(([127, 0, 0, 1], 50001)))
}

/// A page's worth of requests, back to back, must not produce a 429.
///
/// The public health check is used so the result depends on the middleware
/// stack rather than on auth or the mock database.
#[tokio::test]
async fn a_page_load_worth_of_requests_is_not_throttled() {
    let app = router();

    let mut statuses = Vec::new();
    for _ in 0..REQUESTS_PER_PAGE * 2 {
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/v3/server/health_check")
                    .extension(ci())
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        statuses.push(resp.status());
    }

    let throttled = statuses
        .iter()
        .filter(|s| **s == StatusCode::TOO_MANY_REQUESTS)
        .count();
    assert_eq!(
        throttled,
        0,
        "{throttled} of {} requests were rate-limited — a client cannot render \
         a page whose requests do not all get through. Statuses: {statuses:?}",
        statuses.len()
    );
}

/// The auth endpoints keep their tighter limit — that one exists on purpose,
/// and relaxing the global limiter must not have relaxed it too.
#[tokio::test]
async fn the_auth_endpoints_are_still_throttled() {
    let app = router();

    let mut saw_429 = false;
    for _ in 0..40 {
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v3/auth/login")
                    .header("content-type", "application/json")
                    .extension(ci())
                    .body(Body::from(
                        r#"{"username":"nobody","password":"wrongpass"}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        if resp.status() == StatusCode::TOO_MANY_REQUESTS {
            saw_429 = true;
            break;
        }
    }
    assert!(
        saw_429,
        "credential brute-forcing must still be throttled; the tight limit on \
         /auth is why the global one can afford to be generous"
    );
}
