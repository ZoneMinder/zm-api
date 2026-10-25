// Integration tests for server and stats endpoints with a real database
// Run with: cargo test --test handlers_server_stats_integration -- --include-ignored
#![allow(clippy::needless_borrows_for_generic_args)]

mod common;

use axum::body::{self, Body};
use axum::http::{header, Request, StatusCode};
use common::fixtures::RowGuard;
use sea_orm::EntityTrait;
use tower::ServiceExt;
use zm_api::constant::API_VERSION;
use zm_api::dto::request::server_stats::CreateServerStatRequest;
use zm_api::dto::request::stats::{CreateStatRequest, UpdateStatRequest};
use zm_api::dto::response::{MessageResponse, ServerStatResponse, StatResponse, VersionResponse};
use zm_api::entity::{server_stats, stats};

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

fn build_app(db: sea_orm::DatabaseConnection) -> axum::Router {
    let state = zm_api::server::state::AppState::for_test_with_db(db);
    common::schema::checked(zm_api::routes::create_router_app(state))
}

#[tokio::test]
#[ignore = "Requires running test database - run with: ./scripts/db-manager.sh mysql"]
async fn test_api_server_health_check() {
    let db = common::test_db::get_test_db()
        .await
        .expect("Failed to connect to test database");
    let app = build_app(db);

    let response = app
        .oneshot(
            Request::get("/api/v3/server/health_check")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let bytes = body::to_bytes(response.into_body(), 64 * 1024)
        .await
        .unwrap();
    let body: MessageResponse = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(body.message(), "Ok");
}

#[tokio::test]
#[ignore = "Requires running test database - run with: ./scripts/db-manager.sh mysql"]
async fn test_api_server_get_version() {
    let db = common::test_db::get_test_db()
        .await
        .expect("Failed to connect to test database");
    let app = build_app(db);

    let response = app
        .oneshot(
            Request::get("/api/v3/host/getVersion")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let bytes = body::to_bytes(response.into_body(), 64 * 1024)
        .await
        .unwrap();
    let body: VersionResponse = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(body.api_version, API_VERSION.to_string());
    assert!(!body.version.is_empty());
    assert!(!body.db_version.is_empty());
}

#[tokio::test]
#[ignore = "Requires running test database - run with: ./scripts/db-manager.sh mysql"]
async fn test_api_server_stats_create_get_delete() {
    let db = common::test_db::get_test_db()
        .await
        .expect("Failed to connect to test database");
    let app = build_app(db);

    let create_body = serde_json::to_vec(&CreateServerStatRequest {
        server_id: Some(1),
        cpu_load: Some("1.5".to_string()),
        cpu_user_percent: Some("10.0".to_string()),
        cpu_nice_percent: None,
        cpu_system_percent: None,
        cpu_idle_percent: Some("90.0".to_string()),
        cpu_usage_percent: Some("10.0".to_string()),
        total_mem: Some(1024),
        free_mem: Some(512),
        total_swap: None,
        free_swap: None,
    })
    .expect("serialize server stats");

    let response = app
        .clone()
        .oneshot(
            Request::post("/api/v3/server-stats")
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
    let created: ServerStatResponse = serde_json::from_slice(&bytes).unwrap();
    let stat_id = created.id;
    let _g_server_stat = RowGuard::new(format!("Server_Stats#{stat_id}"), move |db| async move {
        let _ = server_stats::Entity::delete_by_id(stat_id).exec(&db).await;
    });
    assert_eq!(created.server_id, Some(1));
    assert_eq!(created.cpu_load.as_deref(), Some("1.5"));

    let response = app
        .clone()
        .oneshot(
            Request::get(&format!("/api/v3/server-stats/{}", created.id))
                .header(header::AUTHORIZATION, auth_header())
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);

    let response = app
        .oneshot(
            Request::delete(&format!("/api/v3/server-stats/{}", created.id))
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
async fn test_api_stats_create_get_update_delete() {
    let db = common::test_db::get_test_db()
        .await
        .expect("Failed to connect to test database");
    let app = build_app(db);

    let create_body = serde_json::to_vec(&CreateStatRequest {
        monitor_id: 1,
        zone_id: 2,
        event_id: 3,
        frame_id: 4,
        pixel_diff: 1,
        alarm_pixels: 10,
        filter_pixels: 20,
        blob_pixels: 30,
        blobs: 2,
        min_blob_size: 1,
        max_blob_size: 2,
        min_x: 1,
        max_x: 2,
        min_y: 1,
        max_y: 2,
        score: 5,
    })
    .expect("serialize stat");

    let response = app
        .clone()
        .oneshot(
            Request::post("/api/v3/stats")
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
    let created: StatResponse = serde_json::from_slice(&bytes).unwrap();
    let stat_id = created.id;
    let _g_stat = RowGuard::new(format!("Stats#{stat_id}"), move |db| async move {
        let _ = stats::Entity::delete_by_id(stat_id).exec(&db).await;
    });
    assert_eq!(created.monitor_id, 1);

    let response = app
        .clone()
        .oneshot(
            Request::get(&format!("/api/v3/stats/{}", created.id))
                .header(header::AUTHORIZATION, auth_header())
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);

    let update_body = serde_json::to_vec(&UpdateStatRequest {
        monitor_id: None,
        zone_id: None,
        event_id: None,
        frame_id: None,
        pixel_diff: Some(2),
        alarm_pixels: None,
        filter_pixels: None,
        blob_pixels: None,
        blobs: None,
        min_blob_size: None,
        max_blob_size: None,
        min_x: None,
        max_x: None,
        min_y: None,
        max_y: None,
        score: Some(6),
    })
    .expect("serialize stat update");

    let response = app
        .clone()
        .oneshot(
            Request::patch(&format!("/api/v3/stats/{}", created.id))
                .header(header::AUTHORIZATION, auth_header())
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(update_body))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let bytes = body::to_bytes(response.into_body(), 64 * 1024)
        .await
        .unwrap();
    let updated: StatResponse = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(updated.pixel_diff, 2);
    assert_eq!(updated.score, 6);

    let response = app
        .oneshot(
            Request::delete(&format!("/api/v3/stats/{}", created.id))
                .header(header::AUTHORIZATION, auth_header())
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::NO_CONTENT);
}
