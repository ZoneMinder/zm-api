// Integration tests for auth endpoints with a real database
// Run with: cargo test --test handlers_auth_integration -- --include-ignored

mod common;

use axum::body::{self, Body};
use axum::http::{header, Request, StatusCode};
use common::test_db::{get_test_db, test_prefix};
use sea_orm::{ActiveModelTrait, Set};
use tower::ServiceExt;
use zm_api::dto::request::{LoginRequest, RefreshTokenRequest};
use zm_api::dto::response::{MeResponse, MessageResponse, TokenResponse};

async fn create_user_db(
    db: &sea_orm::DatabaseConnection,
    username: &str,
    password: &str,
) -> Result<(), sea_orm::DbErr> {
    use zm_api::entity::users::ActiveModel;

    let hashed = zm_api::util::password::hash(password.to_string())
        .await
        .expect("hash password");
    let user = ActiveModel {
        username: Set(username.to_string()),
        password: Set(hashed),
        enabled: Set(1),
        ..Default::default()
    };
    user.insert(db).await?;
    Ok(())
}

/// A default `ConnectInfo` for oneshot requests. Production serves with
/// connect-info, which the auth-route rate limiter reads for peer-IP keying;
/// without it the limiter cannot extract a key and the request 500s.
fn ci() -> axum::extract::ConnectInfo<std::net::SocketAddr> {
    axum::extract::ConnectInfo(std::net::SocketAddr::from(([127, 0, 0, 1], 50000)))
}

/// Delete a single user by exact username. `test_prefix()` is process-wide, so
/// prefix-based cleanup would let one test delete a sibling's user mid-run when
/// the suite runs in parallel; exact-username cleanup keeps the tests isolated.
async fn delete_user_by_username(
    db: &sea_orm::DatabaseConnection,
    username: &str,
) -> Result<(), sea_orm::DbErr> {
    use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};
    zm_api::entity::users::Entity::delete_many()
        .filter(zm_api::entity::users::Column::Username.eq(username))
        .exec(db)
        .await?;
    Ok(())
}

#[tokio::test]
#[ignore = "Requires running test database - run with: ./scripts/db-manager.sh mysql"]
async fn test_api_auth_login_refresh_logout() {
    let username = format!("{}auth_user", test_prefix());
    let password = "TestPass123!";

    let setup_db = get_test_db()
        .await
        .expect("Failed to connect to test database");
    create_user_db(&setup_db, &username, password)
        .await
        .expect("Failed to create test user");

    let state = zm_api::server::state::AppState::for_test_with_db(setup_db);
    let app = common::schema::checked(zm_api::routes::create_router_app(state));

    let login_body = serde_json::to_vec(&LoginRequest {
        username: username.clone(),
        password: password.to_string(),
    })
    .expect("serialize login");

    let response = app
        .clone()
        .oneshot(
            Request::post("/api/v3/auth/login")
                .header(header::CONTENT_TYPE, "application/json")
                .extension(ci())
                .body(Body::from(login_body))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let bytes = body::to_bytes(response.into_body(), 64 * 1024)
        .await
        .unwrap();
    let token: TokenResponse = serde_json::from_slice(&bytes).unwrap();
    assert!(!token.access_token.is_empty());
    assert!(!token.refresh_token.is_empty());

    let refresh_body = serde_json::to_vec(&RefreshTokenRequest {
        token: token.refresh_token.clone(),
    })
    .expect("serialize refresh");

    let response = app
        .clone()
        .oneshot(
            Request::post("/api/v3/auth/refresh")
                .header(header::CONTENT_TYPE, "application/json")
                .extension(ci())
                .body(Body::from(refresh_body))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let bytes = body::to_bytes(response.into_body(), 64 * 1024)
        .await
        .unwrap();
    let refreshed: TokenResponse = serde_json::from_slice(&bytes).unwrap();
    assert!(!refreshed.access_token.is_empty());

    let response = app
        .oneshot(
            Request::get("/api/v3/auth/logout")
                .header(
                    header::AUTHORIZATION,
                    format!("Bearer {}", token.access_token),
                )
                .extension(ci())
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
    assert_eq!(body.message(), "Logout successful");

    let cleanup_db = get_test_db()
        .await
        .expect("Failed to get cleanup connection");
    delete_user_by_username(&cleanup_db, &username)
        .await
        .expect("Failed to cleanup user");
}

/// Helper: POST /api/v3/auth/login and return the token pair.
async fn login(app: &axum::Router, username: &str, password: &str) -> TokenResponse {
    let body = serde_json::to_vec(&LoginRequest {
        username: username.to_string(),
        password: password.to_string(),
    })
    .unwrap();
    let resp = app
        .clone()
        .oneshot(
            Request::post("/api/v3/auth/login")
                .header(header::CONTENT_TYPE, "application/json")
                .extension(ci())
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "login should succeed");
    let bytes = body::to_bytes(resp.into_body(), 64 * 1024).await.unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

#[tokio::test]
#[ignore = "Requires running test database - run with: ./scripts/db-manager.sh mysql"]
async fn test_me_returns_authenticated_user() {
    let username = format!("{}me_user", test_prefix());
    let password = "MePass123!";

    let setup_db = get_test_db().await.expect("connect test db");
    create_user_db(&setup_db, &username, password)
        .await
        .expect("create user");
    let state = zm_api::server::state::AppState::for_test_with_db(setup_db);
    let app = common::schema::checked(zm_api::routes::create_router_app(state));

    let tokens = login(&app, &username, password).await;

    let resp = app
        .clone()
        .oneshot(
            Request::get("/api/v3/me")
                .header(
                    header::AUTHORIZATION,
                    format!("Bearer {}", tokens.access_token),
                )
                .extension(ci())
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = body::to_bytes(resp.into_body(), 64 * 1024).await.unwrap();
    let me: MeResponse = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(me.user.username, username);
    // Token metadata is echoed so clients need not decode the JWT (GH #37).
    assert_eq!(me.token_type, "access");
    assert!(
        me.expires_at > me.issued_at,
        "token expiry follows issued-at"
    );
    // The password hash must never appear in the /me payload.
    let raw = String::from_utf8_lossy(&bytes);
    assert!(
        !raw.contains("password"),
        "me must not expose password fields"
    );

    // /me requires a token.
    let resp = app
        .oneshot(
            Request::get("/api/v3/me")
                .extension(ci())
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

    let cleanup_db = get_test_db().await.expect("cleanup conn");
    delete_user_by_username(&cleanup_db, &username)
        .await
        .expect("cleanup");
}

#[tokio::test]
#[ignore = "Requires running test database - run with: ./scripts/db-manager.sh mysql"]
async fn test_change_password_flow() {
    let username = format!("{}pwchange_user", test_prefix());
    let password = "OldPass123!";
    let new_password = "NewPass456!";

    let setup_db = get_test_db().await.expect("connect test db");
    create_user_db(&setup_db, &username, password)
        .await
        .expect("create user");
    let state = zm_api::server::state::AppState::for_test_with_db(setup_db);
    let app = common::schema::checked(zm_api::routes::create_router_app(state));

    let tokens = login(&app, &username, password).await;
    let auth = format!("Bearer {}", tokens.access_token);

    // Wrong current password is rejected.
    let wrong = serde_json::json!({
        "current_password": "totally-wrong",
        "new_password": new_password,
    });
    let resp = app
        .clone()
        .oneshot(
            Request::put("/api/v3/me/password")
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::AUTHORIZATION, &auth)
                .extension(ci())
                .body(Body::from(serde_json::to_vec(&wrong).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::UNAUTHORIZED,
        "wrong current password must be rejected"
    );

    // Correct current password succeeds.
    let good = serde_json::json!({
        "current_password": password,
        "new_password": new_password,
    });
    let resp = app
        .clone()
        .oneshot(
            Request::put("/api/v3/me/password")
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::AUTHORIZATION, &auth)
                .extension(ci())
                .body(Body::from(serde_json::to_vec(&good).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "password change should succeed"
    );

    // The old access token is now revoked.
    let resp = app
        .clone()
        .oneshot(
            Request::get("/api/v3/me")
                .header(header::AUTHORIZATION, &auth)
                .extension(ci())
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::UNAUTHORIZED,
        "the pre-change token must be revoked"
    );

    // The old password no longer logs in; the new one does.
    let resp = app
        .clone()
        .oneshot(
            Request::post("/api/v3/auth/login")
                .header(header::CONTENT_TYPE, "application/json")
                .extension(ci())
                .body(Body::from(
                    serde_json::to_vec(&LoginRequest {
                        username: username.clone(),
                        password: password.to_string(),
                    })
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::UNAUTHORIZED,
        "old password must no longer work"
    );

    let _new_tokens = login(&app, &username, new_password).await;

    let cleanup_db = get_test_db().await.expect("cleanup conn");
    delete_user_by_username(&cleanup_db, &username)
        .await
        .expect("cleanup");
}

/// Create a user with an explicit permission set. `System: None` with a real
/// `Monitors` grant is a legal ZoneMinder account, and the one the /me
/// regression below turns on.
async fn create_user_with_perms(
    db: &sea_orm::DatabaseConnection,
    username: &str,
    password: &str,
    system: zm_api::entity::sea_orm_active_enums::System,
    monitors: zm_api::entity::sea_orm_active_enums::Monitors,
) -> Result<(), sea_orm::DbErr> {
    use zm_api::entity::users::ActiveModel;

    let hashed = zm_api::util::password::hash(password.to_string())
        .await
        .expect("hash password");
    ActiveModel {
        username: Set(username.to_string()),
        password: Set(hashed),
        enabled: Set(1),
        system: Set(system),
        monitors: Set(monitors),
        ..Default::default()
    }
    .insert(db)
    .await?;
    Ok(())
}

#[tokio::test]
#[ignore = "Requires running test database - run with: ./scripts/db-manager.sh mysql"]
async fn me_is_readable_by_a_user_with_no_system_permission() {
    // GH #56. `/me` must sit on `authenticated_middleware`, not
    // `protect(Feature::System)`. Folding auth_routes into the System-gated
    // group looks like tidying and would 403 exactly the accounts that most
    // need to read their own permissions — reinstating the CakePHP flaw where
    // an operator cannot discover what it may do. Nothing else fails if that
    // happens, so this test is the guard.
    use zm_api::entity::sea_orm_active_enums::{Monitors, System};

    let username = format!("{}nosys", test_prefix());
    let password = "NoSysPass123!";

    let setup_db = get_test_db().await.expect("connect test db");
    create_user_with_perms(&setup_db, &username, password, System::None, Monitors::Edit)
        .await
        .expect("create permission-poor user");
    let state = zm_api::server::state::AppState::for_test_with_db(setup_db);
    let app = common::schema::checked(zm_api::routes::create_router_app(state));

    let tokens = login(&app, &username, password).await;
    let resp = app
        .clone()
        .oneshot(
            Request::get("/api/v3/me")
                .header(
                    header::AUTHORIZATION,
                    format!("Bearer {}", tokens.access_token),
                )
                .extension(ci())
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "/me must stay readable at System: None"
    );

    let bytes = body::to_bytes(resp.into_body(), 64 * 1024).await.unwrap();
    let me: MeResponse = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(me.user.username, username);
    assert_eq!(me.user.system, "None");
    // The whole point: the other five columns are legible, so a client can
    // gate its UI on them rather than inferring from a 401.
    assert_eq!(
        me.user.monitors, "Edit",
        "a System:None account must still be able to read its own Monitors grant"
    );

    // Sanity: this really is a permission-poor account — a System-gated route
    // must refuse it, proving the /me success above is not a superuser token.
    let resp = app
        .oneshot(
            Request::get("/api/v3/logs")
                .header(
                    header::AUTHORIZATION,
                    format!("Bearer {}", tokens.access_token),
                )
                .extension(ci())
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::FORBIDDEN,
        "System-gated routes must still refuse a System:None account"
    );

    let cleanup_db = get_test_db().await.expect("cleanup conn");
    delete_user_by_username(&cleanup_db, &username)
        .await
        .expect("cleanup");
}
