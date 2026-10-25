//! The response check in `common::schema` fails a test when a handler's JSON
//! disagrees with the OpenAPI document (#237), and passes one that agrees.
//! A stand-in router answers documented paths, so no database is needed.

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::routing::get;
use axum::{Json, Router};
use serde_json::json;
use tower::ServiceExt;

async fn call(router: Router, path: &str) -> StatusCode {
    let response = common::schema::checked(router)
        .oneshot(Request::get(path).body(Body::empty()).unwrap())
        .await
        .unwrap();
    response.status()
}

#[tokio::test]
async fn a_response_matching_its_schema_passes() {
    let router = Router::new().route(
        "/api/v3/server/health_check",
        get(|| async { Json(json!({ "message": "ok" })) }),
    );
    assert_eq!(call(router, "/api/v3/server/health_check").await, StatusCode::OK);
}

#[tokio::test]
#[should_panic(expected = "doesn't describe")]
async fn a_response_missing_a_required_field_fails() {
    let router = Router::new().route(
        "/api/v3/server/health_check",
        get(|| async { Json(json!({ "msg": "ok" })) }),
    );
    call(router, "/api/v3/server/health_check").await;
}

#[tokio::test]
#[should_panic(expected = "doesn't describe")]
async fn an_error_body_of_the_wrong_shape_fails() {
    // Not an AppResponseError, which every undocumented status must be.
    let router = Router::new().route(
        "/api/v3/server/health_check",
        get(|| async { (StatusCode::IM_A_TEAPOT, Json(json!({ "oops": true }))) }),
    );
    call(router, "/api/v3/server/health_check").await;
}

#[tokio::test]
#[should_panic(expected = "unknown-route")]
async fn an_undocumented_route_that_answers_fails() {
    let router = Router::new().route(
        "/api/v3/not-in-the-spec",
        get(|| async { Json(json!({})) }),
    );
    call(router, "/api/v3/not-in-the-spec").await;
}
