//! GH #20: GET /events cause/name/notes/tag_id filters + widened sort.
//!
//! Requires the test database — run with:
//!   APP_PROFILE=test-db cargo test --test it_event_filters -- --include-ignored

mod common;

use axum::http::StatusCode;
use common::assertions::assert_status;
use common::fixtures::{insert_monitor, unique_name, RowGuard};
use common::harness::{superuser_token, TestApp};
use sea_orm::{ActiveModelTrait, DatabaseConnection, EntityTrait, Set};
use zm_api::dto::response::events::PaginatedEventsResponse;

async fn insert_event(db: &DatabaseConnection, monitor_id: u32, name: &str, cause: &str) -> u64 {
    zm_api::entity::events::ActiveModel {
        monitor_id: Set(monitor_id),
        state_id: Set(1),
        name: Set(unique_name(name)),
        cause: Set(Some(cause.to_string())),
        start_date_time: Set(Some(zm_api::util::time::db_utc_now())),
        ..Default::default()
    }
    .insert(db)
    .await
    .expect("insert event")
    .id
}

fn guard_event(id: u64) -> RowGuard {
    RowGuard::new(format!("Events#{id}"), move |db| async move {
        let _ = zm_api::entity::events::Entity::delete_by_id(id)
            .exec(&db)
            .await;
    })
}

#[tokio::test]
#[ignore = "requires the test database (APP_PROFILE=test-db)"]
async fn events_filter_by_cause_name_and_tag() {
    let app = TestApp::spawn().await;
    let token = superuser_token();

    let monitor = insert_monitor(&app.db, "EvtFilterMon")
        .await
        .expect("monitor");
    let _mg = RowGuard::monitor(monitor.id);

    let motion = insert_event(&app.db, monitor.id, "FrontDoor", "Motion").await;
    let cont = insert_event(&app.db, monitor.id, "BackYard", "Continuous").await;
    let (_g1, _g2) = (guard_event(motion), guard_event(cont));

    // Tag the motion event.
    let tag = zm_api::entity::tags::ActiveModel {
        name: Set(unique_name("evtfilt")),
        ..Default::default()
    }
    .insert(&app.db)
    .await
    .expect("tag");
    let _tg = RowGuard::tag(tag.id);
    zm_api::entity::events_tags::ActiveModel {
        tag_id: Set(tag.id),
        event_id: Set(motion),
        ..Default::default()
    }
    .insert(&app.db)
    .await
    .expect("event tag");

    // cause substring → only the Motion event.
    let resp = app
        .get(
            &format!(
                "/api/v3/events?monitor_id={}&cause=Motion&page_size=100",
                monitor.id
            ),
            &token,
        )
        .await;
    assert_status(&resp, StatusCode::OK);
    let body: PaginatedEventsResponse = resp.json();
    assert!(
        body.items.iter().all(|e| e.id == motion),
        "cause=Motion should return only the motion event"
    );
    assert!(body.items.iter().any(|e| e.id == motion));

    // name substring → only FrontDoor.
    let resp = app
        .get(
            &format!(
                "/api/v3/events?monitor_id={}&name=FrontDoor&page_size=100",
                monitor.id
            ),
            &token,
        )
        .await;
    let body: PaginatedEventsResponse = resp.json();
    assert!(body.items.iter().all(|e| e.id == motion));

    // tag_id → only the tagged (motion) event.
    let resp = app
        .get(
            &format!(
                "/api/v3/events?monitor_id={}&tag_id={}&page_size=100",
                monitor.id, tag.id
            ),
            &token,
        )
        .await;
    let body: PaginatedEventsResponse = resp.json();
    assert_eq!(body.items.len(), 1, "only the tagged event");
    assert_eq!(body.items[0].id, motion);

    // sort=name must be accepted (previously 400).
    let resp = app
        .get(
            &format!(
                "/api/v3/events?monitor_id={}&sort=name&direction=asc",
                monitor.id
            ),
            &token,
        )
        .await;
    assert_status(&resp, StatusCode::OK);
}
