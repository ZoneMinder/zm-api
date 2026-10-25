// Integration tests for configs, storage, logs, reports, and tags with a real database
// Run with: cargo test --test handlers_meta_integration -- --include-ignored
#![allow(clippy::needless_borrows_for_generic_args)]

mod common;

use axum::body::{self, Body};
use axum::http::{header, Request, StatusCode};
use common::test_db::{get_test_db, test_prefix};
use rust_decimal::Decimal;
use sea_orm::{ActiveModelTrait, DatabaseConnection, DbErr, EntityTrait, Set};
use tower::ServiceExt;
use zm_api::dto::request::reports::CreateReportRequest;
use zm_api::dto::request::tags::CreateTagRequest;
use zm_api::dto::request::CreateStorageRequest;
use zm_api::dto::response::{
    PaginatedConfigsResponse, PaginatedLogsResponse, ReportResponse, StorageResponse, TagResponse,
};
use zm_api::entity::logs;

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

fn build_app(db: DatabaseConnection) -> axum::Router {
    let state = zm_api::server::state::AppState::for_test_with_db(db);
    common::schema::checked(zm_api::routes::create_router_app(state))
}

async fn create_log_db(db: &DatabaseConnection) -> Result<logs::Model, DbErr> {
    let model = logs::ActiveModel {
        time_key: Set(Decimal::new(123456, 6)),
        component: Set("test".to_string()),
        level: Set(1),
        code: Set("INF".to_string()),
        message: Set(format!("{}log", test_prefix())),
        ..Default::default()
    };
    model.insert(db).await
}

async fn cleanup_log_db(db: &DatabaseConnection, id: u32) -> Result<(), DbErr> {
    logs::Entity::delete_by_id(id).exec(db).await?;
    Ok(())
}

#[tokio::test]
#[ignore = "Requires running test database - run with: ./scripts/db-manager.sh mysql"]
async fn test_api_configs_list_get_not_found() {
    let db = get_test_db()
        .await
        .expect("Failed to connect to test database");
    let app = build_app(db);

    let response = app
        .clone()
        .oneshot(
            Request::get("/api/v3/configs")
                .header(header::AUTHORIZATION, auth_header())
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let bytes = body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let _body: PaginatedConfigsResponse = serde_json::from_slice(&bytes).unwrap();

    let response = app
        .oneshot(
            Request::get("/api/v3/configs/does_not_exist")
                .header(header::AUTHORIZATION, auth_header())
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
#[ignore = "Requires running test database - run with: ./scripts/db-manager.sh mysql"]
async fn test_api_storage_create_get_delete() {
    let db = get_test_db()
        .await
        .expect("Failed to connect to test database");
    let app = build_app(db);

    let name = format!("{}storage", test_prefix());
    let create_body = serde_json::to_vec(&CreateStorageRequest {
        do_delete: None,
        name: name.clone(),
        path: "/tmp".to_string(),
        r#type: "local".to_string(),
        enabled: 1,
        scheme: None,
        server_id: None,
        url: None,
    })
    .expect("serialize storage");

    let response = app
        .clone()
        .oneshot(
            Request::post("/api/v3/storage")
                .header(header::AUTHORIZATION, auth_header())
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(create_body))
                .unwrap(),
        )
        .await
        .unwrap();

    let status = response.status();
    let bytes = body::to_bytes(response.into_body(), 64 * 1024)
        .await
        .unwrap();
    if status != StatusCode::CREATED {
        panic!(
            "Unexpected status {}: {}",
            status,
            String::from_utf8_lossy(&bytes)
        );
    }
    let created: StorageResponse = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(created.name, name);

    let response = app
        .clone()
        .oneshot(
            Request::get(&format!("/api/v3/storage/{}", created.id))
                .header(header::AUTHORIZATION, auth_header())
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let response = app
        .oneshot(
            Request::delete(&format!("/api/v3/storage/{}", created.id))
                .header(header::AUTHORIZATION, auth_header())
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
}

#[tokio::test]
#[ignore = "Requires running test database - run with: ./scripts/db-manager.sh mysql"]
async fn test_api_logs_list_get() {
    let db = get_test_db()
        .await
        .expect("Failed to connect to test database");
    let log = create_log_db(&db).await.expect("Failed to create log");
    let app = build_app(db);

    let response = app
        .clone()
        .oneshot(
            Request::get("/api/v3/logs")
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
    let body: PaginatedLogsResponse = serde_json::from_slice(&bytes).unwrap();
    assert!(body.items.iter().any(|l| l.id == log.id));

    let response = app
        .oneshot(
            Request::get(&format!("/api/v3/logs/{}", log.id))
                .header(header::AUTHORIZATION, auth_header())
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let cleanup_db = get_test_db()
        .await
        .expect("Failed to get cleanup connection");
    cleanup_log_db(&cleanup_db, log.id)
        .await
        .expect("Failed to cleanup log");
}

#[tokio::test]
#[ignore = "Requires running test database - run with: ./scripts/db-manager.sh mysql"]
async fn test_api_reports_create_get_delete() {
    let db = get_test_db()
        .await
        .expect("Failed to connect to test database");
    let app = build_app(db);

    let prefix = test_prefix();
    let short = if prefix.len() > 8 {
        &prefix[prefix.len() - 8..]
    } else {
        prefix.as_str()
    };
    let name = format!("rep_{short}");
    let create_body = serde_json::to_vec(&CreateReportRequest {
        name: Some(name.clone()),
        filter_id: None,
        start_date_time: None,
        end_date_time: None,
        interval: None,
    })
    .expect("serialize report");

    let response = app
        .clone()
        .oneshot(
            Request::post("/api/v3/reports")
                .header(header::AUTHORIZATION, auth_header())
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(create_body))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = body::to_bytes(response.into_body(), 64 * 1024)
        .await
        .unwrap();
    if status != StatusCode::CREATED {
        panic!(
            "Unexpected status {}: {}",
            status,
            String::from_utf8_lossy(&bytes)
        );
    }
    let created: ReportResponse = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(created.name.as_deref(), Some(name.as_str()));

    let response = app
        .clone()
        .oneshot(
            Request::get(&format!("/api/v3/reports/{}", created.id))
                .header(header::AUTHORIZATION, auth_header())
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let response = app
        .oneshot(
            Request::delete(&format!("/api/v3/reports/{}", created.id))
                .header(header::AUTHORIZATION, auth_header())
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
}

#[tokio::test]
#[ignore = "Requires running test database - run with: ./scripts/db-manager.sh mysql"]
async fn test_api_tags_create_get_delete() {
    let db = get_test_db()
        .await
        .expect("Failed to connect to test database");
    let app = build_app(db);

    let name = format!("{}tag", test_prefix());
    let create_body = serde_json::to_vec(&CreateTagRequest {
        name: name.clone(),
        create_date: None,
    })
    .expect("serialize tag");

    let response = app
        .clone()
        .oneshot(
            Request::post("/api/v3/tags")
                .header(header::AUTHORIZATION, auth_header())
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(create_body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    let bytes = body::to_bytes(response.into_body(), 64 * 1024)
        .await
        .unwrap();
    let created: TagResponse = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(created.name, name);

    let response = app
        .clone()
        .oneshot(
            Request::get(&format!("/api/v3/tags/{}", created.id))
                .header(header::AUTHORIZATION, auth_header())
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let response = app
        .oneshot(
            Request::delete(&format!("/api/v3/tags/{}", created.id))
                .header(header::AUTHORIZATION, auth_header())
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
}
