//! `linked_monitors` may only name monitors the caller can see, and must be
//! an expression zmc can read.
//!
//!   APP_PROFILE=test-db cargo test --test it_monitor_linked -- --include-ignored

mod common;

use axum::http::StatusCode;
use common::fixtures::{grant_monitor_permission, insert_monitor, insert_user_with_id, RowGuard};
use common::harness::{superuser_token, token_for, TestApp};
use serde_json::json;
use zm_api::entity::sea_orm_active_enums::Permission;
use zm_api::util::authz::{Level, UserPermissions};

#[tokio::test]
#[ignore = "requires the test database (APP_PROFILE=test-db)"]
async fn linked_monitors_must_be_visible_to_the_caller() {
    let app = TestApp::spawn().await;
    let own = insert_monitor(&app.db, "LinkOwn").await.expect("own");
    let _a = RowGuard::monitor(own.id);
    let seen = insert_monitor(&app.db, "LinkSeen").await.expect("seen");
    let _b = RowGuard::monitor(seen.id);
    let hidden = insert_monitor(&app.db, "LinkHidden").await.expect("hidden");
    let _c = RowGuard::monitor(hidden.id);

    const UID: u32 = 990_502;
    insert_user_with_id(&app.db, UID, "Linker")
        .await
        .expect("user");
    let _u = RowGuard::user(UID);
    grant_monitor_permission(&app.db, own.id, UID, Permission::Edit)
        .await
        .expect("grant own");
    grant_monitor_permission(&app.db, seen.id, UID, Permission::View)
        .await
        .expect("grant seen");
    // A monitor with no row inherits the base level, so hiding takes a None row.
    grant_monitor_permission(&app.db, hidden.id, UID, Permission::None)
        .await
        .expect("hide");
    let token = token_for(
        UID,
        UserPermissions {
            monitors: Level::Edit,
            ..Default::default()
        },
    );
    let path = format!("/api/v3/monitors/{}", own.id);
    let link = |expr: String| json!({ "linked_monitors": expr });

    for expr in [
        format!("{}", hidden.id),
        format!("{}|{}", seen.id, hidden.id),
        format!("({}:2)&{}", hidden.id, seen.id),
    ] {
        let r = app.patch_json(&path, &token, &link(expr.clone())).await;
        assert_eq!(r.status(), StatusCode::BAD_REQUEST, "{expr}: {}", r.text());
        assert!(
            !r.text().contains("Link"),
            "{expr}: names a hidden monitor: {}",
            r.text()
        );
    }

    let expr = format!("({}:2|{}),{}", seen.id, seen.id, own.id);
    let r = app.patch_json(&path, &token, &link(expr.clone())).await;
    assert_eq!(r.status(), StatusCode::OK, "{expr}: {}", r.text());
    assert_eq!(r.json::<serde_json::Value>()["linked_monitors"], expr);

    // Clearing the links is always allowed.
    let r = app.patch_json(&path, &token, &link(String::new())).await;
    assert_eq!(r.status(), StatusCode::OK, "{}", r.text());
}

#[tokio::test]
#[ignore = "requires the test database (APP_PROFILE=test-db)"]
async fn linked_monitors_must_parse() {
    let app = TestApp::spawn().await;
    let admin = superuser_token();
    let m = insert_monitor(&app.db, "LinkParse").await.expect("monitor");
    let _m = RowGuard::monitor(m.id);
    let path = format!("/api/v3/monitors/{}", m.id);

    for expr in ["abc", "1;2", "1:x", "99999999999", "1 2", "-1", "1::2"] {
        let r = app
            .patch_json(&path, &admin, &json!({ "linked_monitors": expr }))
            .await;
        assert_eq!(r.status(), StatusCode::BAD_REQUEST, "{expr}: {}", r.text());
    }
    let r = app
        .post_json(
            "/api/v3/monitors",
            &admin,
            &json!({ "name": common::fixtures::unique_name("LinkNew"), "type": "Ffmpeg",
                     "linked_monitors": "abc" }),
        )
        .await;
    assert_eq!(r.status(), StatusCode::BAD_REQUEST, "{}", r.text());
}
