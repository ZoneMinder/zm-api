//! A role's group grants limit which groups a user can see and change, and
//! moving a group needs Edit where it comes from and where it goes.
//!
//!   APP_PROFILE=test-db cargo test --test it_group_scope -- --include-ignored

mod common;

use axum::http::StatusCode;
use common::fixtures::{insert_group, insert_user_with_id, RowGuard};
use common::harness::{superuser_token, token_for, TestApp};
use sea_orm::{ActiveModelTrait, EntityTrait, Set};
use serde_json::json;
use zm_api::entity::sea_orm_active_enums::Permission;
use zm_api::util::authz::{Level, UserPermissions};

const UID: u32 = 990_503;

#[tokio::test]
#[ignore = "requires the test database (APP_PROFILE=test-db)"]
async fn role_group_grants_scope_group_moves() {
    let app = TestApp::spawn().await;
    let admin = superuser_token();

    let g = insert_group(&app.db, "ScopeG").await.expect("g");
    let _g = RowGuard::group(g.id);
    let h = insert_group(&app.db, "ScopeH").await.expect("h");
    let _h = RowGuard::group(h.id);
    let h2 = insert_group(&app.db, "ScopeH2").await.expect("h2");
    let _h2 = RowGuard::group(h2.id);
    let p = insert_group(&app.db, "ScopeP").await.expect("p");
    let _p = RowGuard::group(p.id);

    let r = app
        .post_json(
            "/api/v3/roles",
            &admin,
            &json!({ "name": common::fixtures::unique_name("ScopeRole"), "groups": "Edit" }),
        )
        .await;
    assert_eq!(r.status(), StatusCode::CREATED, "{}", r.text());
    let role_id = r.json::<serde_json::Value>()["id"].as_u64().unwrap() as u32;
    let _role = RowGuard::new(format!("User_Roles#{role_id}"), move |db| async move {
        let _ = zm_api::entity::user_roles::Entity::delete_by_id(role_id)
            .exec(&db)
            .await;
    });
    for (group_id, permission) in [
        (g.id, Permission::Edit),
        (h2.id, Permission::Edit),
        (p.id, Permission::View),
    ] {
        zm_api::entity::role_groups_permissions::ActiveModel {
            role_id: Set(role_id),
            group_id: Set(group_id),
            permission: Set(permission),
            ..Default::default()
        }
        .insert(&app.db)
        .await
        .expect("role grant");
    }

    insert_user_with_id(&app.db, UID, "ScopeUser")
        .await
        .expect("user");
    let _u = RowGuard::user(UID);
    let mut user: zm_api::entity::users::ActiveModel =
        zm_api::entity::users::Entity::find_by_id(UID)
            .one(&app.db)
            .await
            .unwrap()
            .unwrap()
            .into();
    user.role_id = Set(Some(role_id));
    user.update(&app.db).await.expect("set role");

    let token = token_for(
        UID,
        UserPermissions {
            groups: Level::Edit,
            ..Default::default()
        },
    );
    let group = |id: u32| format!("/api/v3/groups/{id}");

    // The role grants nothing on H, so the user can't see or move it.
    let r = app.get(&group(h.id), &token).await;
    assert_eq!(r.status(), StatusCode::NOT_FOUND, "{}", r.text());
    let r = app
        .put_json(&group(h.id), &token, &json!({ "parent_id": g.id }))
        .await;
    assert_eq!(r.status(), StatusCode::NOT_FOUND, "{}", r.text());

    // P is only View: no moving a group under it, or creating one there.
    let r = app
        .put_json(&group(h2.id), &token, &json!({ "parent_id": p.id }))
        .await;
    assert_eq!(r.status(), StatusCode::FORBIDDEN, "{}", r.text());
    let r = app
        .post_json(
            "/api/v3/groups",
            &token,
            &json!({ "name": common::fixtures::unique_name("ScopeNew"), "parent_id": p.id }),
        )
        .await;
    assert_eq!(r.status(), StatusCode::FORBIDDEN, "{}", r.text());

    // Under G (Edit) is fine, and back out to the top again.
    let r = app
        .put_json(&group(h2.id), &token, &json!({ "parent_id": g.id }))
        .await;
    assert_eq!(r.status(), StatusCode::OK, "{}", r.text());
    let r = app
        .put_json(&group(h2.id), &token, &json!({ "parent_id": null }))
        .await;
    assert_eq!(r.status(), StatusCode::OK, "{}", r.text());

    // Out from under a View-only parent is refused: it would lift what P
    // decides for the group's monitors.
    let mut moved: zm_api::entity::groups::ActiveModel = h2.clone().into();
    moved.parent_id = Set(Some(p.id));
    moved.update(&app.db).await.expect("park under p");
    let r = app
        .put_json(&group(h2.id), &token, &json!({ "parent_id": null }))
        .await;
    assert_eq!(r.status(), StatusCode::FORBIDDEN, "{}", r.text());
}
