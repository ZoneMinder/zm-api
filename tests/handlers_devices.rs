mod common;

use axum::body::{self, Body};
use axum::http::{header, Request, StatusCode};
use sea_orm::{DatabaseBackend, MockDatabase, MockExecResult, Value};
use std::collections::BTreeMap;
use tower::ServiceExt;
use zm_api::dto::response::devices::PaginatedDevicesResponse;
use zm_api::dto::response::DeviceResponse;
use zm_api::entity::devices::Model as DeviceModel;
use zm_api::entity::sea_orm_active_enums::DeviceType;

fn sample_device() -> DeviceModel {
    DeviceModel {
        id: 1,
        name: "X10 Controller".into(),
        r#type: DeviceType::X10,
        key_string: "A1".into(),
    }
}

fn auth_header() -> String {
    let token = zm_api::service::token::generate_tokens(
        "tester".to_string(),
        1,
        zm_api::util::authz::UserPermissions::superuser(),
    )
    .expect("token")
    .access_token;
    format!("Bearer {}", token)
}

#[tokio::test]
async fn test_list_devices() {
    let devices = vec![sample_device()];
    // Mock both the COUNT query (for num_items) and the data query (for
    // fetch_page). The paginator reads the count as i64.
    let count_result: BTreeMap<String, Value> =
        BTreeMap::from([("num_items".to_string(), Value::BigInt(Some(1)))]);
    let db = MockDatabase::new(DatabaseBackend::MySql)
        .append_query_results(vec![vec![count_result]])
        .append_query_results::<DeviceModel, _, _>(vec![devices])
        .into_connection();
    let state = zm_api::server::state::AppState::for_test_with_db(db);
    let app = common::schema::checked(zm_api::routes::create_router_app(state));

    let response = app
        .oneshot(
            Request::get("/api/v3/devices")
                .header(header::AUTHORIZATION, auth_header())
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let bytes = body::to_bytes(response.into_body(), 64 * 1024)
        .await
        .unwrap();
    let body: PaginatedDevicesResponse = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(body.items.len(), 1);
    assert_eq!(body.items[0].name, "X10 Controller");
    assert_eq!(body.total, 1);
}

#[tokio::test]
async fn test_get_device_by_id() {
    let device = sample_device();
    let db = MockDatabase::new(DatabaseBackend::MySql)
        .append_query_results::<DeviceModel, _, _>(vec![vec![device]])
        .into_connection();
    let state = zm_api::server::state::AppState::for_test_with_db(db);
    let app = common::schema::checked(zm_api::routes::create_router_app(state));

    let response = app
        .oneshot(
            Request::get("/api/v3/devices/1")
                .header(header::AUTHORIZATION, auth_header())
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let bytes = body::to_bytes(response.into_body(), 64 * 1024)
        .await
        .unwrap();
    let body: DeviceResponse = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(body.id, 1);
    assert_eq!(body.name, "X10 Controller");
    assert_eq!(body.key_string, "A1");
}

#[tokio::test]
async fn test_get_device_not_found() {
    let db = MockDatabase::new(DatabaseBackend::MySql)
        .append_query_results::<DeviceModel, _, _>(vec![vec![]])
        .into_connection();
    let state = zm_api::server::state::AppState::for_test_with_db(db);
    let app = common::schema::checked(zm_api::routes::create_router_app(state));

    let response = app
        .oneshot(
            Request::get("/api/v3/devices/999")
                .header(header::AUTHORIZATION, auth_header())
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_delete_device() {
    let db = MockDatabase::new(DatabaseBackend::MySql)
        .append_exec_results(vec![MockExecResult {
            last_insert_id: 0,
            rows_affected: 1,
        }])
        .into_connection();
    let state = zm_api::server::state::AppState::for_test_with_db(db);
    let app = common::schema::checked(zm_api::routes::create_router_app(state));

    let response = app
        .oneshot(
            Request::delete("/api/v3/devices/1")
                .header(header::AUTHORIZATION, auth_header())
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn test_delete_device_not_found() {
    let db = MockDatabase::new(DatabaseBackend::MySql)
        .append_exec_results(vec![MockExecResult {
            last_insert_id: 0,
            rows_affected: 0,
        }])
        .into_connection();
    let state = zm_api::server::state::AppState::for_test_with_db(db);
    let app = common::schema::checked(zm_api::routes::create_router_app(state));

    let response = app
        .oneshot(
            Request::delete("/api/v3/devices/999")
                .header(header::AUTHORIZATION, auth_header())
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}
