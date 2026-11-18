//! The live event stream's watcher against the test database: an event row
//! appearing, closing and getting notes reaches a subscriber as
//! `event_start`, `event_end` and `event_updated`; a reconnect replays it.
//!
//!   APP_PROFILE=test-db cargo test --test it_event_stream -- --include-ignored

mod common;

use std::time::Duration;

use axum::http::{Method, StatusCode};
use common::fixtures::{insert_monitor, unique_name, RowGuard};
use common::harness::TestApp;
use common::test_db::get_test_db;
use sea_orm::{ActiveModelTrait, EntityTrait, Set};
use tokio::sync::broadcast;
use zm_api::server::state::AppState;
use zm_api::service::event_stream::StreamMessage;

fn guard_event(id: u64) -> RowGuard {
    RowGuard::new(format!("Events#{id}"), move |db| async move {
        let _ = zm_api::entity::events::Entity::delete_by_id(id)
            .exec(&db)
            .await;
    })
}

/// The next message about `event_id`, skipping everything else.
async fn next_for(rx: &mut broadcast::Receiver<StreamMessage>, event_id: u64) -> StreamMessage {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match rx.recv().await {
                Ok(m) if m.event_id() == Some(event_id) => return m,
                Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(e) => panic!("stream closed: {e}"),
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("no message about event {event_id} within 10 s"))
}

#[tokio::test]
#[ignore = "requires the test database (APP_PROFILE=test-db)"]
async fn an_event_starts_ends_and_gets_notes() {
    let state = AppState::for_test_with_db(get_test_db().await.expect("test db"));
    let db = get_test_db().await.expect("test db");
    let mut rx = state.event_hub.subscribe(&state);
    // The watcher starts from the newest event; let it read where that is.
    tokio::time::sleep(Duration::from_millis(1500)).await;

    let monitor = insert_monitor(&db, "EvStreamMon").await.expect("monitor");
    let _m = RowGuard::monitor(monitor.id);
    let now = zm_api::util::time::db_utc_now();
    let event = zm_api::entity::events::ActiveModel {
        monitor_id: Set(monitor.id),
        state_id: Set(1),
        name: Set(unique_name("EvStream")),
        cause: Set(Some("Motion".into())),
        start_date_time: Set(Some(now)),
        ..Default::default()
    }
    .insert(&db)
    .await
    .expect("event");
    let _e = guard_event(event.id);

    let m = next_for(&mut rx, event.id).await;
    assert_eq!(m.kind(), "event_start");
    let v = serde_json::to_value(&m).unwrap();
    assert_eq!(v["monitor_id"], monitor.id);
    assert_eq!(v["cause"], "Motion");
    assert!(v["end_date_time"].is_null());

    let mut am: zm_api::entity::events::ActiveModel = event.clone().into();
    am.end_date_time = Set(Some(now));
    am.alarm_frames = Set(Some(4));
    let event = am.update(&db).await.unwrap();
    let m = next_for(&mut rx, event.id).await;
    assert_eq!(m.kind(), "event_end");
    assert_eq!(serde_json::to_value(&m).unwrap()["alarm_frames"], 4);

    let mut am: zm_api::entity::events::ActiveModel = event.clone().into();
    am.notes = Set(Some("detected:person:97%".into()));
    am.update(&db).await.unwrap();
    let m = next_for(&mut rx, event.id).await;
    assert_eq!(m.kind(), "event_updated");
    assert_eq!(
        serde_json::to_value(&m).unwrap()["notes"],
        "detected:person:97%"
    );

    // A reconnect after the event replays its start and end.
    let replayed =
        zm_api::service::event_stream::replay(&state, event.id - 1, Some(vec![monitor.id]))
            .await
            .unwrap();
    let kinds: Vec<_> = replayed.iter().map(|m| (m.kind(), m.event_id())).collect();
    assert_eq!(
        kinds,
        [
            ("event_start", Some(event.id)),
            ("event_end", Some(event.id))
        ]
    );
}

#[tokio::test]
#[ignore = "requires the test database (APP_PROFILE=test-db)"]
async fn the_stream_needs_a_token() {
    let app = TestApp::spawn().await;
    let resp = app
        .request(Method::GET, "/api/v3/events/stream")
        .send()
        .await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

/// `/events/stream` is its own route, not `/events/{id}` with id "stream"
/// (which answers 400): the response is an event stream.
#[tokio::test]
#[ignore = "requires the test database (APP_PROFILE=test-db)"]
async fn the_stream_route_is_not_an_event_id() {
    use tower::ServiceExt;
    let app = TestApp::spawn().await;
    let req = axum::http::Request::builder()
        .uri("/api/v3/events/stream")
        .header(
            axum::http::header::AUTHORIZATION,
            format!("Bearer {}", common::harness::superuser_token()),
        )
        .extension(axum::extract::ConnectInfo(std::net::SocketAddr::from((
            [127, 0, 0, 1],
            50000,
        ))))
        .body(axum::body::Body::empty())
        .unwrap();
    let resp = tokio::time::timeout(Duration::from_secs(10), app.router().oneshot(req))
        .await
        .expect("response head within 10 s")
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers()[axum::http::header::CONTENT_TYPE],
        "text/event-stream"
    );
}
