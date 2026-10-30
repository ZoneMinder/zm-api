//! `POST /menu-items/reset` (#197). It rewrites the whole Menu_Items table,
//! so it lives in a binary of its own: no other test in it can lose rows
//! mid-run.
//!
//!   APP_PROFILE=test-db cargo test --test it_menu_reset -- --include-ignored

mod common;

use axum::http::StatusCode;
use common::fixtures::unique_name;
use common::harness::{superuser_token, token_with, TestApp};
use serde_json::json;
use zm_api::dto::response::settings_tables::MenuItemResponse;
use zm_api::util::authz::{Level, UserPermissions};

#[tokio::test]
#[ignore = "requires the test database (APP_PROFILE=test-db)"]
async fn reset_puts_back_a_fresh_installs_menu() {
    let app = TestApp::spawn().await;
    let admin = superuser_token();

    // Change the menu: a custom item, and Console disabled and renamed.
    let key = unique_name("Custom");
    let key = &key[key.len().saturating_sub(32)..];
    let r = app
        .post_json("/api/v3/menu-items", &admin, &json!({ "menu_key": key }))
        .await;
    assert_eq!(r.status(), StatusCode::CREATED, "{}", r.text());
    let list: Vec<MenuItemResponse> = app.get("/api/v3/menu-items", &admin).await.json();
    if let Some(console) = list.iter().find(|m| m.menu_key == "Console") {
        let r = app
            .put_json(
                &format!("/api/v3/menu-items/{}", console.id),
                &admin,
                &json!({ "enabled": false, "label": "Home" }),
            )
            .await;
        assert_eq!(r.status(), StatusCode::OK, "{}", r.text());
    }

    // System View can't reset.
    let viewer = token_with(UserPermissions {
        system: Level::View,
        ..Default::default()
    });
    let r = app
        .post_json("/api/v3/menu-items/reset", &viewer, &json!({}))
        .await;
    assert_eq!(r.status(), StatusCode::FORBIDDEN, "{}", r.text());

    let r = app
        .post_json("/api/v3/menu-items/reset", &admin, &json!({}))
        .await;
    assert_eq!(r.status(), StatusCode::OK, "{}", r.text());
    let reset: Vec<MenuItemResponse> = r.json();
    let got: Vec<(&str, i16, bool, Option<&str>)> = reset
        .iter()
        .map(|m| {
            (
                m.menu_key.as_str(),
                m.sort_order,
                m.enabled,
                m.label.as_deref(),
            )
        })
        .collect();
    let want: Vec<(&str, i16, bool, Option<&str>)> = zm_api::service::settings_tables::DEFAULT_MENU
        .iter()
        .map(|(k, o)| (*k, *o, true, None))
        .collect();
    assert_eq!(got, want);

    // It's what the table now holds.
    let list: Vec<MenuItemResponse> = app.get("/api/v3/menu-items", &admin).await.json();
    assert_eq!(list.len(), want.len());
    assert!(!list.iter().any(|m| m.menu_key == key));
}
