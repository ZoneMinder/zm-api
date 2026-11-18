//! Signed media URLs (`?st=`) and the links minted for header-less players
//! (#153, #34).
//!
//! The mock-router tests prove a bad `st` is refused before any handler or
//! database is reached. The DB-backed ones (ignored by default) prove a minted
//! link opens its own monitor or event and nothing else, and dies with its
//! user. Run them with:
//!   APP_PROFILE=test-db cargo nextest run --test it_media_links --run-ignored all

mod common;

use std::time::Duration;

use axum::http::{Method, StatusCode};
use common::fixtures::{insert_monitor, insert_user_with_id, unique_name, RowGuard};
use common::harness::{token_for, TestApp};
use sea_orm::{ActiveModelTrait, EntityTrait, Set};
use serde_json::json;
use zm_api::dto::response::{MediaLinkResponse, RtspLinkResponse};
use zm_api::entity::sea_orm_active_enums::{Events, Monitors, Stream};
use zm_api::util::authz::UserPermissions;
use zm_api::util::media_url::{sign, sign_until, MediaResource};

async fn status(app: &TestApp, method: Method, path: &str) -> StatusCode {
    app.request(method, path).send().await.status()
}

#[tokio::test]
async fn a_bad_signature_is_unauthorized() {
    let app = TestApp::mock();
    for path in [
        "/api/v3/live/1/hls/live.m3u8?st=garbage",
        "/api/v3/live/1/hls/live.m3u8?st=1.m.1.3.0.9999999999.AAAA",
        "/api/v3/events/1/stream/segment/0.m4s?st=garbage",
    ] {
        assert_eq!(
            status(&app, Method::GET, path).await,
            StatusCode::UNAUTHORIZED,
            "{path}"
        );
    }
}

#[tokio::test]
async fn an_expired_signature_is_unauthorized() {
    let app = TestApp::mock();
    let st = sign_until(
        MediaResource::Monitor(1),
        7,
        zm_api::util::time::unix_now() - 1,
    );
    let path = format!(
        "/api/v3/live/1/hls/init.mp4?st={}",
        urlencoding::encode(&st)
    );
    assert_eq!(
        status(&app, Method::GET, &path).await,
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn a_signature_opens_only_its_own_resource() {
    let app = TestApp::mock();
    let live1 = urlencoding::encode(&sign(MediaResource::Monitor(1), 7, Duration::from_secs(60)))
        .into_owned();
    let rec = urlencoding::encode(&sign(
        MediaResource::Event {
            event_id: 5,
            monitor_id: 1,
        },
        7,
        Duration::from_secs(60),
    ))
    .into_owned();
    for path in [
        // Another monitor.
        format!("/api/v3/live/2/hls/live.m3u8?st={live1}"),
        // The monitor's recordings, with its live signature.
        format!("/api/v3/events/5/stream/playlist.m3u8?st={live1}"),
        // Another event.
        format!("/api/v3/events/6/stream/playlist.m3u8?st={rec}"),
        // The event's monitor's live stream, with a recording signature.
        format!("/api/v3/live/1/hls/live.m3u8?st={rec}"),
        // A streaming route that isn't one monitor's or one event's media.
        format!("/api/v3/live/sessions?st={live1}"),
    ] {
        assert_eq!(
            status(&app, Method::GET, &path).await,
            StatusCode::FORBIDDEN,
            "{path}"
        );
    }
}

#[tokio::test]
async fn a_signature_cannot_write_or_mint() {
    let app = TestApp::mock();
    let st = urlencoding::encode(&sign(MediaResource::Monitor(1), 7, Duration::from_secs(60)))
        .into_owned();
    for (method, path) in [
        (Method::POST, format!("/api/v3/live/1/hls/link?st={st}")),
        (Method::POST, format!("/api/v3/live/1/start?st={st}")),
        (Method::DELETE, format!("/api/v3/live/1/stop?st={st}")),
    ] {
        assert_eq!(
            status(&app, method.clone(), &path).await,
            StatusCode::FORBIDDEN,
            "{method} {path}"
        );
    }
}

/// A user who may watch streams and events, and a monitor. Each test passes
/// its own `uid`: nextest runs them in parallel.
async fn viewer_and_monitor(app: &TestApp, uid: u32) -> (RowGuard, RowGuard, u32) {
    insert_user_with_id(&app.db, uid, &format!("MediaLinkUser{uid}"))
        .await
        .expect("user fixture");
    let user = zm_api::entity::users::Entity::find_by_id(uid)
        .one(&app.db)
        .await
        .unwrap()
        .unwrap();
    let mut am: zm_api::entity::users::ActiveModel = user.into();
    am.enabled = Set(1);
    am.stream = Set(Stream::View);
    am.events = Set(Events::View);
    // With no per-monitor grants, ZoneMinder shows a user the monitors their
    // Monitors level allows (`visibleMonitor`).
    am.monitors = Set(Monitors::View);
    am.update(&app.db)
        .await
        .expect("grant stream/events/monitors view");
    let user_guard = RowGuard::user(uid);
    let monitor = insert_monitor(&app.db, "MediaLinkMon")
        .await
        .expect("monitor fixture");
    let monitor_guard = RowGuard::monitor(monitor.id);
    (user_guard, monitor_guard, monitor.id)
}

fn viewer_token(uid: u32) -> String {
    let perms = UserPermissions {
        stream: zm_api::util::authz::Level::View,
        events: zm_api::util::authz::Level::View,
        monitors: zm_api::util::authz::Level::View,
        ..Default::default()
    };
    token_for(uid, perms)
}

/// Not 401/403: the request got past authentication to the handler, whatever
/// the handler then made of a monitor with no camera behind it.
fn authenticated(code: StatusCode) -> bool {
    code != StatusCode::UNAUTHORIZED && code != StatusCode::FORBIDDEN
}

#[tokio::test]
#[ignore = "requires the test database (APP_PROFILE=test-db)"]
async fn a_minted_live_link_opens_its_monitor_until_the_user_is_disabled() {
    const UID: u32 = 990_771;
    let app = TestApp::spawn().await;
    let (_u, _m, monitor_id) = viewer_and_monitor(&app, UID).await;

    let resp = app
        .post_json(
            &format!("/api/v3/live/{monitor_id}/hls/link"),
            &viewer_token(UID),
            &json!({ "ttl_seconds": 3600 }),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK, "{}", resp.text());
    let link: MediaLinkResponse = resp.json();
    assert!(link
        .playlist_url
        .starts_with(&format!("/api/v3/live/{monitor_id}/hls/master.m3u8?st=")));
    let st = urlencoding::encode(&link.token).into_owned();

    let snapshot = format!("/api/v3/monitors/{monitor_id}/snapshot?st={st}");
    let code = status(&app, Method::GET, &snapshot).await;
    assert!(authenticated(code), "link should authenticate, got {code}");

    // Disabling the user kills every link it minted.
    let user = zm_api::entity::users::Entity::find_by_id(UID)
        .one(&app.db)
        .await
        .unwrap()
        .unwrap();
    let mut am: zm_api::entity::users::ActiveModel = user.into();
    am.enabled = Set(0);
    am.update(&app.db).await.unwrap();
    assert_eq!(
        status(&app, Method::GET, &snapshot).await,
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
#[ignore = "requires the test database (APP_PROFILE=test-db)"]
async fn a_minted_event_link_opens_that_event_only() {
    const UID: u32 = 990_772;
    let app = TestApp::spawn().await;
    let (_u, _m, monitor_id) = viewer_and_monitor(&app, UID).await;
    let event = zm_api::entity::events::ActiveModel {
        monitor_id: Set(monitor_id),
        state_id: Set(1),
        name: Set(unique_name("MediaLinkEvent")),
        ..Default::default()
    }
    .insert(&app.db)
    .await
    .expect("event fixture");
    let event_id = event.id;
    let _e = RowGuard::new(format!("Events#{event_id}"), move |db| async move {
        let _ = zm_api::entity::events::Entity::delete_by_id(event_id)
            .exec(&db)
            .await;
    });

    let resp = app
        .post_json(
            &format!("/api/v3/events/{event_id}/stream/link"),
            &viewer_token(UID),
            &json!({}),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK, "{}", resp.text());
    let link: MediaLinkResponse = resp.json();
    let st = urlencoding::encode(&link.token).into_owned();

    let code = status(
        &app,
        Method::GET,
        &format!("/api/v3/events/{event_id}/info?st={st}"),
    )
    .await;
    assert!(authenticated(code), "link should authenticate, got {code}");
    assert_eq!(
        status(
            &app,
            Method::GET,
            &format!("/api/v3/events/{}/info?st={st}", event_id + 1)
        )
        .await,
        StatusCode::FORBIDDEN
    );
}

#[tokio::test]
#[ignore = "requires the test database (APP_PROFILE=test-db)"]
async fn minting_needs_a_header_token() {
    const UID: u32 = 990_773;
    let app = TestApp::spawn().await;
    let (_u, _m, monitor_id) = viewer_and_monitor(&app, UID).await;
    let path = format!(
        "/api/v3/live/{monitor_id}/hls/link?token={}",
        viewer_token(UID)
    );
    let code = app
        .request(Method::POST, &path)
        .json(&json!({}))
        .send()
        .await
        .status();
    assert_eq!(code, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
#[ignore = "requires the test database (APP_PROFILE=test-db)"]
async fn an_rtsp_link_is_a_signed_link_to_the_monitor() {
    const UID: u32 = 990_774;
    let app = TestApp::spawn_with(|state| {
        let mut config = (*state.config).clone();
        config.streaming.rtsp.enabled = true;
        config.streaming.rtsp.public_host = Some("cams.example.com".into());
        config.streaming.rtsp.tls_port = Some(8322);
        state.config = std::sync::Arc::new(config);
    })
    .await;
    let (_u, _m, monitor_id) = viewer_and_monitor(&app, UID).await;

    let resp = app
        .post_json(
            &format!("/api/v3/live/{monitor_id}/rtsp/link"),
            &viewer_token(UID),
            &json!({ "ttl_seconds": 3600 }),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK, "{}", resp.text());
    let link: RtspLinkResponse = resp.json();
    let st = urlencoding::encode(&link.token).into_owned();
    assert_eq!(
        link.url,
        format!("rtsp://cams.example.com:8554/live/{monitor_id}?st={st}")
    );
    assert_eq!(
        link.tls_url,
        Some(format!(
            "rtsps://cams.example.com:8322/live/{monitor_id}?st={st}"
        ))
    );
    let grant = zm_api::util::media_url::verify(&link.token).unwrap();
    assert_eq!(grant.resource, MediaResource::Monitor(monitor_id));
    assert_eq!(grant.uid, UID);
}

#[tokio::test]
#[ignore = "requires the test database (APP_PROFILE=test-db)"]
async fn no_rtsp_link_while_the_server_is_off() {
    const UID: u32 = 990_775;
    let app = TestApp::spawn().await;
    let (_u, _m, monitor_id) = viewer_and_monitor(&app, UID).await;
    let resp = app
        .post_json(
            &format!("/api/v3/live/{monitor_id}/rtsp/link"),
            &viewer_token(UID),
            &json!({}),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
}
