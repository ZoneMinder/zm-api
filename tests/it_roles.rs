//! ZoneMinder 1.39 roles: `/roles` CRUD, `role_id` on users, and a role's
//! permissions taking effect at login (#143).
//!
//! Requires the test database:
//!   APP_PROFILE=test-db cargo nextest run --test it_roles --run-ignored all

mod common;

use axum::http::{Method, StatusCode};
use common::fixtures::{unique_name, RowGuard};
use common::harness::{superuser_token, TestApp};
use serde_json::json;
use zm_api::dto::response::roles::RoleResponse;
use zm_api::dto::response::UserResponse;

fn role_guard(id: u32) -> RowGuard {
    RowGuard::new(format!("User_Roles#{id}"), move |db| async move {
        use sea_orm::EntityTrait;
        let _ = zm_api::entity::user_roles::Entity::delete_by_id(id)
            .exec(&db)
            .await;
    })
}

async fn login(app: &TestApp, username: &str, password: &str) -> String {
    let resp = app
        .request(Method::POST, "/api/v3/auth/login")
        .json(&json!({ "username": username, "password": password }))
        .send()
        .await;
    assert_eq!(resp.status(), StatusCode::OK, "login: {}", resp.text());
    resp.json::<serde_json::Value>()["access_token"]
        .as_str()
        .unwrap()
        .to_string()
}

#[tokio::test]
#[ignore = "requires the test database (APP_PROFILE=test-db)"]
async fn role_crud_round_trips() {
    let app = TestApp::spawn().await;
    let token = superuser_token();
    let name = unique_name("RoleCrud");
    let resp = app
        .post_json(
            "/api/v3/roles",
            &token,
            &json!({ "name": name, "description": "ops", "events": "View" }),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::CREATED, "{}", resp.text());
    let role: RoleResponse = resp.json();
    let _g = role_guard(role.id);
    assert_eq!(role.events, "View");
    assert_eq!(role.system, "None", "unnamed levels default to None");

    let resp = app
        .put_json(
            &format!("/api/v3/roles/{}", role.id),
            &token,
            &json!({ "system": "View", "description": null }),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK, "{}", resp.text());
    let updated: RoleResponse = resp.json();
    assert_eq!(updated.system, "View");
    assert_eq!(updated.description, None);

    let list = app.get("/api/v3/roles?page_size=1000", &token).await;
    assert!(list.text().contains(&name));

    let del = app
        .delete(&format!("/api/v3/roles/{}", role.id), &token)
        .await;
    assert_eq!(del.status(), StatusCode::NO_CONTENT);
    let gone = app.get(&format!("/api/v3/roles/{}", role.id), &token).await;
    assert_eq!(gone.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
#[ignore = "requires the test database (APP_PROFILE=test-db)"]
async fn a_roles_permissions_apply_at_login_and_go_with_it() {
    let app = TestApp::spawn().await;
    let admin = superuser_token();
    let role: RoleResponse = app
        .post_json(
            "/api/v3/roles",
            &admin,
            &json!({ "name": unique_name("RoleAdmin"), "system": "Edit" }),
        )
        .await
        .json();
    let _rg = role_guard(role.id);

    let username = unique_name("roleuser");
    let password = "role-user-pw-1";
    let resp = app
        .post_json(
            "/api/v3/users",
            &admin,
            &json!({
                "username": username, "password": password, "email": "",
                "role_id": role.id,
                "stream": "None", "events": "None", "control": "None", "monitors": "None",
                "groups": "None", "devices": "None", "snapshots": "None", "system": "None",
            }),
        )
        .await;
    assert!(resp.status().is_success(), "{}", resp.text());
    let user: UserResponse = resp.json();
    let _ug = RowGuard::user(user.id);
    assert_eq!(user.role_id, Some(role.id));
    assert_eq!(user.system, "None");

    // Their own System level is None; the role's Edit lets them in.
    let token = login(&app, &username, password).await;
    assert_eq!(
        app.get("/api/v3/users", &token).await.status(),
        StatusCode::OK
    );

    // Without the role, the next login is back to their own levels.
    let resp = app
        .put_json(
            &format!("/api/v3/users/{}", user.id),
            &admin,
            &json!({ "role_id": null }),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK, "{}", resp.text());
    let token = login(&app, &username, password).await;
    assert_eq!(
        app.get("/api/v3/users", &token).await.status(),
        StatusCode::FORBIDDEN
    );
}

#[tokio::test]
#[ignore = "requires the test database (APP_PROFILE=test-db)"]
async fn a_user_cannot_be_given_a_role_that_does_not_exist() {
    let app = TestApp::spawn().await;
    let resp = app
        .post_json(
            "/api/v3/users",
            &superuser_token(),
            &json!({ "username": unique_name("norole"), "password": "x-pw-12345", "email": "", "role_id": 999_999_999u32 }),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "{}", resp.text());
}

/// A role's per-monitor rows can be listed, set and cleared, and they take
/// effect for the role's users the way ZoneMinder applies them (#196).
#[tokio::test]
#[ignore = "requires the test database (APP_PROFILE=test-db)"]
async fn a_roles_monitor_and_group_rows_round_trip_and_apply() {
    use common::fixtures::{insert_group, insert_monitor};
    use zm_api::dto::response::role_permissions::RolePermissionResponse;
    let app = TestApp::spawn().await;
    let admin = superuser_token();
    let role: RoleResponse = app
        .post_json(
            "/api/v3/roles",
            &admin,
            &json!({ "name": unique_name("RoleRows"), "monitors": "View" }),
        )
        .await
        .json();
    let _rg = role_guard(role.id);
    let shown = insert_monitor(&app.db, "RoleShown").await.unwrap();
    let _m1 = RowGuard::monitor(shown.id);
    let hidden = insert_monitor(&app.db, "RoleHidden").await.unwrap();
    let _m2 = RowGuard::monitor(hidden.id);

    let username = unique_name("rolerows");
    let password = "role-rows-pw-1";
    let resp = app
        .post_json(
            "/api/v3/users",
            &admin,
            &json!({
                "username": username, "password": password, "email": "",
                "role_id": role.id,
                "stream": "None", "events": "None", "control": "None", "monitors": "None",
                "groups": "None", "devices": "None", "snapshots": "None", "system": "None",
            }),
        )
        .await;
    assert!(resp.status().is_success(), "{}", resp.text());
    let user: UserResponse = resp.json();
    let _ug = RowGuard::user(user.id);

    let base = format!("/api/v3/roles/{}/monitors-permissions", role.id);
    let set = |permission: &'static str, mid: u32| {
        let app = &app;
        let admin = admin.clone();
        let base = base.clone();
        async move {
            app.put_json(
                &format!("{base}/{mid}"),
                &admin,
                &json!({ "permission": permission }),
            )
            .await
        }
    };
    let r = set("View", hidden.id).await;
    assert_eq!(r.status(), StatusCode::OK, "{}", r.text());
    let first: RolePermissionResponse = r.json();
    let r = set("None", hidden.id).await;
    let row: RolePermissionResponse = r.json();
    assert_eq!(row.id, first.id, "a second set updates the row");
    assert_eq!(
        (row.target_id, row.permission.as_str()),
        (hidden.id, "None")
    );
    assert_eq!(
        set("Admin", hidden.id).await.status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(set("View", u32::MAX).await.status(), StatusCode::NOT_FOUND);
    let list: Vec<RolePermissionResponse> = app.get(&base, &admin).await.json();
    assert_eq!(list.len(), 1);

    // The role's user sees every monitor but the one its row denies.
    let token = login(&app, &username, password).await;
    let get = |id: u32| {
        let app = &app;
        let token = token.clone();
        async move {
            app.get(&format!("/api/v3/monitors/{id}"), &token)
                .await
                .status()
        }
    };
    assert_eq!(get(shown.id).await, StatusCode::OK);
    assert_eq!(get(hidden.id).await, StatusCode::NOT_FOUND);

    let r = app.delete(&format!("{base}/{}", hidden.id), &admin).await;
    assert_eq!(r.status(), StatusCode::NO_CONTENT);
    assert_eq!(
        get(hidden.id).await,
        StatusCode::OK,
        "cleared: the role's level applies"
    );
    let r = app.delete(&format!("{base}/{}", hidden.id), &admin).await;
    assert_eq!(r.status(), StatusCode::NOT_FOUND);

    // Groups go the same way.
    let group = insert_group(&app.db, "RoleGroup").await.unwrap();
    let _gg = RowGuard::group(group.id);
    let gbase = format!("/api/v3/roles/{}/groups-permissions", role.id);
    let r = app
        .put_json(
            &format!("{gbase}/{}", group.id),
            &admin,
            &json!({ "permission": "Edit" }),
        )
        .await;
    assert_eq!(r.status(), StatusCode::OK, "{}", r.text());
    let list: Vec<RolePermissionResponse> = app.get(&gbase, &admin).await.json();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].permission, "Edit");
    let r = app.delete(&format!("{gbase}/{}", group.id), &admin).await;
    assert_eq!(r.status(), StatusCode::NO_CONTENT);

    // Unknown role.
    let r = app
        .get("/api/v3/roles/4294967295/monitors-permissions", &admin)
        .await;
    assert_eq!(r.status(), StatusCode::NOT_FOUND);
}
