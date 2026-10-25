//! Tests for the cross-cutting HTTP middleware stack: security headers and
//! response compression. Run without a database.

mod common;

use axum::{
    body::Body,
    http::{header, Request},
};
use sea_orm::{DatabaseBackend, MockDatabase};
use tower::util::ServiceExt; // for `oneshot`

fn router() -> axum::Router {
    let db = MockDatabase::new(DatabaseBackend::MySql).into_connection();
    let state = zm_api::server::state::AppState::for_test_with_db(db);
    common::schema::checked(zm_api::routes::create_router_app(state))
}

#[tokio::test]
async fn security_headers_present_on_responses() {
    let response = router()
        .oneshot(
            Request::get("/api/v3/server/health_check")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    let headers = response.headers();
    assert_eq!(
        headers.get(header::X_CONTENT_TYPE_OPTIONS).unwrap(),
        "nosniff"
    );
    assert_eq!(headers.get(header::X_FRAME_OPTIONS).unwrap(), "DENY");
    assert_eq!(headers.get("referrer-policy").unwrap(), "no-referrer");
    // HSTS is gated on TLS being enabled; the test profile runs plain HTTP.
    assert!(headers.get(header::STRICT_TRANSPORT_SECURITY).is_none());
}

#[tokio::test]
async fn json_api_response_is_compressed_when_requested() {
    // The OpenAPI document is a large JSON body served from the compressed
    // `api` sub-router.
    let response = router()
        .oneshot(
            Request::get("/api-docs/openapi.json")
                .header(header::ACCEPT_ENCODING, "gzip")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(
        response
            .headers()
            .get(header::CONTENT_ENCODING)
            .map(|v| v.to_str().unwrap()),
        Some("gzip"),
        "JSON API responses should be gzip-compressed when the client accepts it"
    );
}
