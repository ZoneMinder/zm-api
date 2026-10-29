//! `GET /system/sounds/{name}` (#194): the watch page's alarm sound.
//!
//! Sets `ZM_CONF_DIR` for the whole process, so it's a binary of its own.

mod common;

use axum::http::{Method, StatusCode};
use common::harness::{superuser_token, token_with, TestApp};
use zm_api::util::authz::{Level, UserPermissions};

#[tokio::test]
async fn the_alarm_sound_is_served_to_whoever_may_watch() {
    let conf = tempfile::tempdir().unwrap();
    let web = conf.path().join("www");
    std::fs::create_dir_all(web.join("sounds")).unwrap();
    std::fs::write(web.join("sounds/snap.ogg"), b"OggS-test").unwrap();
    std::fs::write(
        conf.path().join("zm.conf"),
        format!("ZM_PATH_WEB={}\n", web.display()),
    )
    .unwrap();
    // SAFETY: this binary's only test; nothing else reads the environment.
    unsafe { std::env::set_var("ZM_CONF_DIR", conf.path()) };

    let app = TestApp::mock();
    let viewer = token_with(UserPermissions {
        stream: Level::View,
        ..Default::default()
    });

    let r = app.get("/api/v3/system/sounds/snap.ogg", &viewer).await;
    assert_eq!(r.status(), StatusCode::OK, "{}", r.text());
    assert_eq!(r.body, b"OggS-test");
    assert_eq!(
        r.headers.get("content-type").unwrap().to_str().unwrap(),
        "audio/ogg"
    );

    // Safari's <audio> asks for byte ranges.
    let r = app
        .request(Method::GET, "/api/v3/system/sounds/snap.ogg")
        .bearer(&viewer)
        .header("range", "bytes=0-3")
        .send()
        .await;
    assert_eq!(r.status(), StatusCode::PARTIAL_CONTENT, "{}", r.text());
    assert_eq!(r.body, b"OggS");

    // An <audio> element can only send the token in the URL.
    let r = app
        .request(
            Method::GET,
            &format!("/api/v3/system/sounds/snap.ogg?token={viewer}"),
        )
        .send()
        .await;
    assert_eq!(r.status(), StatusCode::OK, "{}", r.text());

    let r = app
        .get("/api/v3/system/sounds/missing.ogg", &superuser_token())
        .await;
    assert_eq!(r.status(), StatusCode::NOT_FOUND);
    let r = app
        .get("/api/v3/system/sounds/..%2Fzm.conf", &superuser_token())
        .await;
    assert_eq!(r.status(), StatusCode::NOT_FOUND, "{}", r.text());

    let no_stream = token_with(UserPermissions {
        events: Level::View,
        ..Default::default()
    });
    let r = app.get("/api/v3/system/sounds/snap.ogg", &no_stream).await;
    assert_eq!(r.status(), StatusCode::FORBIDDEN);
}
