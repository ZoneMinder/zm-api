mod common;

use axum::{
    body::{self, Body},
    http::Request,
};
use sea_orm::{DatabaseBackend, MockDatabase};
use tower::util::ServiceExt; // for `oneshot`

#[tokio::test]
async fn health_check_ok() {
    let db = MockDatabase::new(DatabaseBackend::MySql).into_connection();
    let state = zm_api::server::state::AppState::for_test_with_db(db);
    let app = common::schema::checked(zm_api::routes::create_router_app(state));
    let response = app
        .oneshot(
            Request::get("/api/v3/server/health_check")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert!(response.status().is_success());
}

#[tokio::test]
async fn get_version_returns_values() {
    // Mock two config queries: ZM_VERSION and ZM_DB_VERSION
    use zm_api::entity::config::Model as ConfigModel;
    let v1 = ConfigModel {
        id: 1,
        name: "ZM_DYN_CURR_VERSION".into(),
        value: "1.36.33".into(),
        r#type: "string".into(),
        default_value: None,
        hint: None,
        pattern: None,
        format: None,
        prompt: None,
        help: None,
        category: "General".into(),
        readonly: 0,
        private: 0,
        system: 0,
        requires: None,
    };
    let v2 = ConfigModel {
        id: 2,
        name: "ZM_DYN_DB_VERSION".into(),
        value: "1.36.33-db".into(),
        r#type: "string".into(),
        default_value: None,
        hint: None,
        pattern: None,
        format: None,
        prompt: None,
        help: None,
        category: "General".into(),
        readonly: 0,
        private: 0,
        system: 0,
        requires: None,
    };
    let db = MockDatabase::new(DatabaseBackend::MySql)
        .append_query_results::<ConfigModel, _, _>(vec![vec![v1]])
        .append_query_results::<ConfigModel, _, _>(vec![vec![v2]])
        .into_connection();
    let state = zm_api::server::state::AppState::for_test_with_db(db);
    let app = common::schema::checked(zm_api::routes::create_router_app(state));
    let response = app
        .oneshot(
            Request::get("/api/v3/host/getVersion")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert!(response.status().is_success());
    let bytes = body::to_bytes(response.into_body(), 64 * 1024)
        .await
        .unwrap();
    let body: zm_api::dto::response::VersionResponse = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(body.version, "1.36.33");
    assert_eq!(body.db_version, "1.36.33-db");
}
