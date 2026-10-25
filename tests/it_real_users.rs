//! Real users, made by an administrator and logged in through the API, see and
//! change only what their levels and permission rows allow (#235).
//!
//! Most integration tests use `superuser_token()`, a user with no row that
//! sees everything. These use `common::users::UserSpec`, so the login path
//! (levels from the `Users` row and role) and the row-level rules
//! (`Monitors_Permissions`, `Groups_Permissions` and the role tables) are what
//! decide.
//!
//!   APP_PROFILE=test-db cargo test --test it_real_users -- --include-ignored

mod common;

use std::collections::HashSet;

use axum::http::StatusCode;
use common::fixtures::{insert_group, insert_monitor, unique_name, RowGuard};
use common::harness::TestApp;
use common::users::UserSpec;
use rust_decimal::Decimal;
use sea_orm::{ActiveModelTrait, Set};
use serde_json::{json, Value};
use zm_api::entity::sea_orm_active_enums::{Permission, Status};

/// Three monitors, each with an event and a status row, cleaned up on drop.
struct Site {
    a: u32,
    b: u32,
    c: u32,
    events: [u64; 3],
    _guards: Vec<RowGuard>,
}

async fn site(app: &TestApp) -> Site {
    let mut guards = Vec::new();
    let mut ids = Vec::new();
    let mut events = Vec::new();
    for label in ["RealA", "RealB", "RealC"] {
        let m = insert_monitor(&app.db, label).await.expect("monitor");
        guards.push(RowGuard::monitor(m.id));
        let event = zm_api::entity::events::ActiveModel {
            monitor_id: Set(m.id),
            state_id: Set(1),
            name: Set(unique_name(label)),
            length: Set(Decimal::new(10, 0)),
            ..Default::default()
        }
        .insert(&app.db)
        .await
        .expect("event")
        .id;
        guards.push(RowGuard::new(
            format!("Events#{event}"),
            move |db| async move {
                use sea_orm::EntityTrait;
                let _ = zm_api::entity::events::Entity::delete_by_id(event)
                    .exec(&db)
                    .await;
            },
        ));
        let mid = m.id;
        zm_api::entity::monitor_status::ActiveModel {
            monitor_id: Set(mid),
            status: Set(Status::Connected),
            capture_fps: Set(Decimal::new(1000, 2)),
            analysis_fps: Set(Decimal::new(1000, 2)),
            capture_bandwidth: Set(0),
            updated_on: Set(zm_api::util::utc_to_timestamp(chrono::Utc::now())),
        }
        .insert(&app.db)
        .await
        .expect("monitor status");
        guards.push(RowGuard::new(
            format!("Monitor_Status#{mid}"),
            move |db| async move {
                use sea_orm::EntityTrait;
                let _ = zm_api::entity::monitor_status::Entity::delete_by_id(mid)
                    .exec(&db)
                    .await;
            },
        ));
        ids.push(m.id);
        events.push(event);
    }
    Site {
        a: ids[0],
        b: ids[1],
        c: ids[2],
        events: [events[0], events[1], events[2]],
        _guards: guards,
    }
}

/// Which of `of` the user's monitor list includes.
async fn listed(app: &TestApp, token: &str, of: &[u32]) -> HashSet<u32> {
    let r = app.get("/api/v3/monitors?page_size=1000", token).await;
    assert_eq!(r.status(), StatusCode::OK, "{}", r.text());
    r.json::<Value>()["items"]
        .as_array()
        .expect("items")
        .iter()
        .filter_map(|m| m["id"].as_u64().map(|id| id as u32))
        .filter(|id| of.contains(id))
        .collect()
}

async fn status(app: &TestApp, token: &str, path: &str) -> StatusCode {
    app.get(path, token).await.status()
}

#[tokio::test]
#[ignore = "requires the test database (APP_PROFILE=test-db)"]
async fn a_view_user_sees_only_what_their_rows_allow() {
    let app = TestApp::spawn().await;
    let s = site(&app).await;
    // A granted, B denied, C left to the base level (View).
    let user = UserSpec::new(json!({ "monitors": "View", "events": "View", "stream": "View" }))
        .monitor(s.a, "View")
        .monitor(s.b, "None")
        .login(&app)
        .await;
    let t = &user.token;

    assert_eq!(
        listed(&app, t, &[s.a, s.b, s.c]).await,
        HashSet::from([s.a, s.c])
    );
    assert_eq!(
        status(&app, t, &format!("/api/v3/monitors/{}", s.a)).await,
        StatusCode::OK
    );
    assert_eq!(
        status(&app, t, &format!("/api/v3/monitors/{}", s.b)).await,
        StatusCode::NOT_FOUND
    );

    // B's event, status and zones are hidden with it.
    let [ea, eb, _] = s.events;
    assert_eq!(
        status(&app, t, &format!("/api/v3/events/{ea}")).await,
        StatusCode::OK
    );
    assert_eq!(
        status(&app, t, &format!("/api/v3/events/{eb}")).await,
        StatusCode::NOT_FOUND
    );
    let r = app.get("/api/v3/events?page_size=1000", t).await;
    assert_eq!(r.status(), StatusCode::OK, "{}", r.text());
    let event_ids: HashSet<u64> = r.json::<Value>()["items"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|e| e["id"].as_u64())
        .collect();
    assert!(event_ids.contains(&ea), "A's event is listed");
    assert!(!event_ids.contains(&eb), "B's event is not listed");
    assert_eq!(
        status(&app, t, &format!("/api/v3/monitor-status/{}", s.a)).await,
        StatusCode::OK
    );
    assert_eq!(
        status(&app, t, &format!("/api/v3/monitor-status/{}", s.b)).await,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        status(&app, t, &format!("/api/v3/monitors/{}/zones", s.b)).await,
        StatusCode::NOT_FOUND
    );

    // View is not Edit.
    let r = app
        .patch_json(
            &format!("/api/v3/monitors/{}", s.a),
            t,
            &json!({ "name": unique_name("Renamed") }),
        )
        .await;
    assert_eq!(r.status(), StatusCode::FORBIDDEN, "{}", r.text());
}

#[tokio::test]
#[ignore = "requires the test database (APP_PROFILE=test-db)"]
async fn a_role_decides_where_the_user_has_no_row() {
    let app = TestApp::spawn().await;
    let s = site(&app).await;
    // The role gives View and hides B and C. The user's own row on B wins.
    let user = UserSpec::new(json!({}))
        .role(json!({ "monitors": "View", "events": "View" }))
        .role_monitor(s.b, Permission::None)
        .role_monitor(s.c, Permission::None)
        .monitor(s.b, "View")
        .login(&app)
        .await;

    assert_eq!(
        listed(&app, &user.token, &[s.a, s.b, s.c]).await,
        HashSet::from([s.a, s.b])
    );
    assert_eq!(
        status(&app, &user.token, &format!("/api/v3/monitors/{}", s.c)).await,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
#[ignore = "requires the test database (APP_PROFILE=test-db)"]
async fn a_view_row_stops_an_edit_user_changing_that_monitor() {
    let app = TestApp::spawn().await;
    let s = site(&app).await;
    let user = UserSpec::new(json!({ "monitors": "Edit" }))
        .monitor(s.c, "View")
        .login(&app)
        .await;
    let rename = |id: u32| {
        (
            format!("/api/v3/monitors/{id}"),
            json!({ "name": unique_name("Renamed") }),
        )
    };

    let (path, body) = rename(s.a);
    let r = app.patch_json(&path, &user.token, &body).await;
    assert_eq!(r.status(), StatusCode::OK, "{}", r.text());
    let (path, body) = rename(s.c);
    let r = app.patch_json(&path, &user.token, &body).await;
    assert_eq!(r.status(), StatusCode::FORBIDDEN, "{}", r.text());
}

#[tokio::test]
#[ignore = "requires the test database (APP_PROFILE=test-db)"]
async fn a_second_user_with_the_same_name_is_a_conflict() {
    let app = TestApp::spawn().await;
    let user = UserSpec::new(json!({})).login(&app).await;
    let r = app
        .post_json(
            "/api/v3/users",
            &common::harness::superuser_token(),
            &json!({
                "username": user.username,
                "password": "another-password-2",
                "email": "dup@example.invalid",
                "permissions": {},
            }),
        )
        .await;
    assert_eq!(r.status(), StatusCode::CONFLICT, "{}", r.text());
}

#[tokio::test]
#[ignore = "requires the test database (APP_PROFILE=test-db)"]
async fn a_group_row_limits_the_user_to_its_monitors() {
    let app = TestApp::spawn().await;
    let s = site(&app).await;
    let g = insert_group(&app.db, "RealGroup").await.expect("group");
    let _g = RowGuard::group(g.id);
    zm_api::entity::groups_monitors::ActiveModel {
        group_id: Set(g.id),
        monitor_id: Set(s.a),
        ..Default::default()
    }
    .insert(&app.db)
    .await
    .expect("group member");
    // The base level reaches every monitor; the group denies its own.
    let user = UserSpec::new(json!({ "monitors": "View", "groups": "View" }))
        .group(g.id, "None")
        .login(&app)
        .await;

    assert_eq!(
        listed(&app, &user.token, &[s.a, s.b, s.c]).await,
        HashSet::from([s.b, s.c])
    );
    assert_eq!(
        status(&app, &user.token, &format!("/api/v3/monitors/{}", s.a)).await,
        StatusCode::NOT_FOUND
    );
}
