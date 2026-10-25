// Integration tests with real database
// Run with: cargo test --test handlers_devices_integration -- --include-ignored

mod common;

use axum::body::{self, Body};
use axum::http::{header, Request, StatusCode};
use common::fixtures::RowGuard;
use common::test_db::get_test_db;
use common::test_db::test_prefix;
use sea_orm::{ActiveModelTrait, EntityTrait, Set};
use tower::ServiceExt;
use zm_api::dto::response::{DeviceResponse, PaginatedDevicesResponse};
use zm_api::entity::devices::Model as DeviceModel;
use zm_api::entity::sea_orm_active_enums::DeviceType;

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

/// Insert a device with a name unique to this test, so concurrent tests in
/// this binary never collide on or clean up each other's fixture rows.
async fn create_device_db(
    db: &sea_orm::DatabaseConnection,
    label: &str,
) -> Result<DeviceModel, sea_orm::DbErr> {
    use zm_api::entity::devices::ActiveModel;

    let device = ActiveModel {
        name: Set(format!("{}{}", test_prefix(), label)),
        r#type: Set(DeviceType::X10),
        key_string: Set("A1".to_string()),
        ..Default::default()
    };

    device.insert(db).await
}

#[tokio::test]
#[ignore = "Requires running test database - run with: ./scripts/test-db.sh start"]
async fn test_api_devices_list() {
    let db = get_test_db()
        .await
        .expect("Failed to connect to test database");

    // Setup: Create a test device
    let device = create_device_db(&db, "ListController")
        .await
        .expect("Failed to create test device");
    let _g_device = RowGuard::device(device.id);

    // Test: List devices
    let state = zm_api::server::state::AppState::for_test_with_db(db);
    let app = common::schema::checked(zm_api::routes::create_router_app(state));

    let response = app
        .oneshot(
            Request::get("/api/v3/devices?page=1&page_size=1000")
                .header(header::AUTHORIZATION, auth_header())
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let bytes = body::to_bytes(response.into_body(), 256 * 1024)
        .await
        .unwrap();
    let body: PaginatedDevicesResponse = serde_json::from_slice(&bytes).unwrap();

    // Verify our test device is in the list
    assert!(body.items.iter().any(|d| d.id == device.id));
}

#[tokio::test]
#[ignore = "Requires running test database - run with: ./scripts/test-db.sh start"]
async fn test_api_devices_get() {
    let db = get_test_db()
        .await
        .expect("Failed to connect to test database");

    // Setup: Create a test device
    let device = create_device_db(&db, "GetController")
        .await
        .expect("Failed to create test device");
    let _g_device = RowGuard::device(device.id);

    // Test: Get device by ID
    let state = zm_api::server::state::AppState::for_test_with_db(db);
    let app = common::schema::checked(zm_api::routes::create_router_app(state));

    let response = app
        .oneshot(
            Request::get(format!("/api/v3/devices/{}", device.id))
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

    assert_eq!(body.id, device.id);
    assert_eq!(body.name, device.name);
    assert_eq!(body.key_string, "A1");
}

#[tokio::test]
#[ignore = "Requires running test database - run with: ./scripts/test-db.sh start"]
async fn test_api_devices_create_delete() {
    let db = get_test_db()
        .await
        .expect("Failed to connect to test database");

    // Create a device through the API
    // Note: You'll need to implement the POST endpoint for this test
    // For now, we'll create directly and test delete

    let device = create_device_db(&db, "DeleteController")
        .await
        .expect("Failed to create test device");
    let device_id = device.id;
    let _g_device = RowGuard::device(device_id);

    // Test: Delete device
    let state = zm_api::server::state::AppState::for_test_with_db(db);
    let app = common::schema::checked(zm_api::routes::create_router_app(state));

    let response = app
        .oneshot(
            Request::delete(format!("/api/v3/devices/{}", device_id))
                .header(header::AUTHORIZATION, auth_header())
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::NO_CONTENT);

    // Verify it's actually deleted
    use zm_api::entity::devices::Entity as Device;
    let verify_db = get_test_db()
        .await
        .expect("Failed to get verify connection");
    let found = Device::find_by_id(device_id).one(&verify_db).await.unwrap();
    assert!(found.is_none(), "Device should be deleted");
}
