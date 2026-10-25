//! PTZ through ZoneMinder's control daemon, end to end (#240).
//!
//! For a control without a native implementation zm-api sends each command
//! to zmcontrol.pl's socket, `zmcontrol-{id}.sock` in `ZM_PATH_SOCKS`, as
//! ZoneMinder's own web UI does. Here a stand-in listens on that socket and
//! records what arrives, and every bridge-backed PTZ endpoint is called once.
//! The camera address is in TEST-NET-1 (192.0.2.0/24), which nothing routes:
//! only the stand-in is ever contacted. A relative move arrives as
//! ZoneMinder's own `moveRel<Dir>`; an absolute one is refused (#248).
//!
//!   APP_PROFILE=test-db cargo test --test it_ptz_bridge -- --include-ignored

mod common;

use std::path::Path;
use std::sync::{Arc, Mutex};

use axum::http::{Method, StatusCode};
use common::fixtures::{insert_monitor, unique_name, RowGuard};
use common::harness::{superuser_token, TestApp};
use sea_orm::{ActiveModelTrait, Set};
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixListener;
use zm_api::ptz::PtzManager;

/// Listen on `zmcontrol-{monitor}.sock` and keep every command received.
fn fake_zmcontrol(dir: &Path, monitor_id: u32) -> Arc<Mutex<Vec<Value>>> {
    let received = Arc::new(Mutex::new(Vec::new()));
    let listener = UnixListener::bind(dir.join(format!("zmcontrol-{monitor_id}.sock"))).unwrap();
    let log = received.clone();
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            let log = log.clone();
            tokio::spawn(async move {
                let mut buf = Vec::new();
                if stream.read_to_end(&mut buf).await.is_err() {
                    return;
                }
                let command: Value = serde_json::from_slice(&buf).unwrap_or(Value::Null);
                let wants = command["wants_response"] == 1;
                log.lock().unwrap().push(command);
                if wants {
                    let _ = stream.write_all(br#"{"result":"on"}"#).await;
                }
            });
        }
    });
    received
}

#[tokio::test]
#[ignore = "requires the test database (APP_PROFILE=test-db)"]
async fn every_bridge_command_reaches_zmcontrol() {
    let tmp = tempfile::tempdir().unwrap();
    let socks = tmp.path().to_string_lossy().into_owned();
    let app = TestApp::spawn_with(|state| {
        let mut config = (*state.config).clone();
        config.streaming.zoneminder.socks_path = socks.clone();
        state.config = Arc::new(config);
        state.ptz_manager = Arc::new(PtzManager::with_socket_dir(&socks));
    })
    .await;
    let token = superuser_token();

    // A control zmcontrol.pl would load (Netcat.pm), able to do everything.
    let r = app
        .post_json(
            "/api/v3/controls",
            &token,
            &json!({
                "name": unique_name("BridgeCtl"), "type": "Ffmpeg", "protocol": "Netcat",
                "can_move": 1, "can_move_con": 1, "can_move_diag": 1, "can_pan": 1, "can_tilt": 1,
                "can_zoom": 1, "can_zoom_con": 1, "can_focus": 1, "can_focus_con": 1,
                "can_auto_focus": 1, "can_iris": 1, "can_iris_con": 1, "can_auto_iris": 1,
                "has_presets": 1, "num_presets": 8, "can_set_presets": 1, "has_home_preset": 1,
                "can_reset": 1, "can_reboot": 1, "can_sleep": 1, "can_wake": 1,
                "can_light": 1, "can_indicator_light": 1, "can_audio_play": 1,
                "can_audio_volume": 1, "min_audio_file": 1, "max_audio_file": 4
            }),
        )
        .await;
    assert_eq!(r.status(), StatusCode::CREATED, "{}", r.text());
    let control_id = r.json::<Value>()["id"].as_u64().unwrap() as u32;
    let _c = RowGuard::control(control_id);
    let m = insert_monitor(&app.db, "BridgeCam").await.unwrap();
    let _m = RowGuard::monitor(m.id);
    let mut am: zm_api::entity::monitors::ActiveModel = m.clone().into();
    am.controllable = Set(1);
    am.control_id = Set(Some(control_id));
    am.control_address = Set(Some("192.0.2.10:80".into()));
    am.update(&app.db).await.unwrap();
    let received = fake_zmcontrol(tmp.path(), m.id);

    let base = format!("/api/v3/ptz/monitors/{}", m.id);
    let calls: &[(Method, &str, &str)] = &[
        (Method::POST, "move/up", "moveConUp"),
        (Method::POST, "move/down", "moveConDown"),
        (Method::POST, "move/left", "moveConLeft"),
        (Method::POST, "move/right", "moveConRight"),
        (Method::POST, "move/up-left", "moveConUpLeft"),
        (Method::POST, "move/up-right", "moveConUpRight"),
        (Method::POST, "move/down-left", "moveConDownLeft"),
        (Method::POST, "move/down-right", "moveConDownRight"),
        (Method::POST, "move/stop", "moveStop"),
        (Method::POST, "zoom/in", "zoomConTele"),
        (Method::POST, "zoom/out", "zoomConWide"),
        (Method::POST, "zoom/stop", "zoomStop"),
        (Method::POST, "focus/near", "focusConNear"),
        (Method::POST, "focus/far", "focusConFar"),
        (Method::POST, "focus/stop", "focusStop"),
        (Method::POST, "focus/auto", "focusAuto"),
        (Method::POST, "iris/open", "irisConOpen"),
        (Method::POST, "iris/close", "irisConClose"),
        (Method::POST, "iris/stop", "irisStop"),
        (Method::POST, "iris/auto", "irisAuto"),
        (Method::POST, "home", "presetHome"),
        (Method::POST, "presets/3/goto", "presetGoto"),
        (Method::POST, "presets/3/set", "presetSet"),
        (Method::DELETE, "presets/3", "presetClear"),
        (Method::POST, "power/reboot", "reboot"),
        (Method::POST, "power/reset", "reset"),
        (Method::POST, "power/sleep", "sleep"),
        (Method::POST, "power/wake", "wake"),
        (Method::POST, "light/on", "lightOn"),
        (Method::POST, "light/off", "lightOff"),
        (Method::POST, "indicator-light/on", "indicatorLightOn"),
        (Method::POST, "indicator-light/off", "indicatorLightOff"),
        (Method::POST, "audio/play/2", "audioPlay"),
        (Method::POST, "audio/stop", "audioStop"),
        (Method::POST, "audio/volume/up", "audioVolumeUp"),
        (Method::POST, "audio/volume/down", "audioVolumeDown"),
    ];
    for (method, path, command) in calls {
        let before = received.lock().unwrap().len();
        let r = app
            .request(method.clone(), &format!("{base}/{path}"))
            .bearer(&token)
            .json(&json!({}))
            .send()
            .await;
        assert_eq!(r.status(), StatusCode::OK, "{method} {path}: {}", r.text());
        // The bridge closes its side before answering, so the command has
        // been read by the time the request returns, or very shortly after.
        let mut got = None;
        for _ in 0..50 {
            if let Some(c) = received.lock().unwrap().get(before) {
                got = Some(c.clone());
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        let got = got.unwrap_or_else(|| panic!("{path}: nothing reached zmcontrol"));
        assert_eq!(got["command"], *command, "{path}: {got}");
        if path.starts_with("presets/3") {
            assert_eq!(got["preset"], 3, "{path}: {got}");
        }
        if *path == "audio/play/2" {
            assert_eq!(got["file"], 2, "{path}: {got}");
        }
    }

    // A relative move and zoom: two calls, as ZoneMinder's web UI names them.
    let before = received.lock().unwrap().len();
    let r = app
        .post_json(
            &format!("{base}/relative"),
            &token,
            &json!({ "pan_delta": -5, "tilt_delta": 3, "zoom_delta": 2 }),
        )
        .await;
    assert_eq!(r.status(), StatusCode::OK, "relative: {}", r.text());
    let mut got = Vec::new();
    for _ in 0..50 {
        got = received.lock().unwrap()[before..].to_vec();
        if got.len() >= 2 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert_eq!(got.len(), 2, "relative: {got:?}");
    assert_eq!(got[0]["command"], "moveRelUpLeft", "{}", got[0]);
    assert_eq!(got[0]["panstep"], 5, "{}", got[0]);
    assert_eq!(got[0]["tiltstep"], 3, "{}", got[0]);
    assert_eq!(got[1]["command"], "zoomRelTele", "{}", got[1]);
    assert_eq!(got[1]["step"], 2, "{}", got[1]);

    // No module takes a position from zmcontrol.pl, so nothing is sent.
    let before = received.lock().unwrap().len();
    let r = app
        .post_json(&format!("{base}/absolute"), &token, &json!({ "pan": 10 }))
        .await;
    assert_eq!(
        r.status(),
        StatusCode::BAD_REQUEST,
        "absolute: {}",
        r.text()
    );
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert_eq!(received.lock().unwrap().len(), before);
}

#[tokio::test]
#[ignore = "requires the test database (APP_PROFILE=test-db)"]
async fn a_manual_monitor_action_runs_through_zmcontrol() {
    let tmp = tempfile::tempdir().unwrap();
    let socks = tmp.path().to_string_lossy().into_owned();
    let app = TestApp::spawn_with(|state| {
        let mut config = (*state.config).clone();
        config.streaming.zoneminder.socks_path = socks.clone();
        state.config = Arc::new(config);
        state.ptz_manager = Arc::new(PtzManager::with_socket_dir(&socks));
    })
    .await;
    let token = superuser_token();

    // A speaker that plays sounds 1-4, and a camera whose action plays one.
    let r = app
        .post_json(
            "/api/v3/controls",
            &token,
            &json!({ "name": unique_name("ActBridge"), "protocol": "Netcat",
                     "can_audio_play": 1, "min_audio_file": 1, "max_audio_file": 4 }),
        )
        .await;
    assert_eq!(r.status(), StatusCode::CREATED, "{}", r.text());
    let control_id = r.json::<Value>()["id"].as_u64().unwrap() as u32;
    let _c = RowGuard::control(control_id);
    let speaker = insert_monitor(&app.db, "ActSpeaker").await.unwrap();
    let _s = RowGuard::monitor(speaker.id);
    let mut am: zm_api::entity::monitors::ActiveModel = speaker.clone().into();
    am.controllable = Set(1);
    am.control_id = Set(Some(control_id));
    am.control_address = Set(Some("192.0.2.11:80".into()));
    am.update(&app.db).await.unwrap();
    let camera = insert_monitor(&app.db, "ActCam").await.unwrap();
    let _m = RowGuard::monitor(camera.id);
    let cam = camera.id;
    let _a = RowGuard::new(format!("MonitorActions#{cam}"), move |db| async move {
        use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};
        use zm_api::entity::monitor_actions as ma;
        let _ = ma::Entity::delete_many()
            .filter(ma::Column::MonitorId.eq(cam))
            .exec(&db)
            .await;
    });
    let received = fake_zmcontrol(tmp.path(), speaker.id);

    let base = format!("/api/v3/monitors/{cam}/actions");
    let r = app
        .post_json(
            &base,
            &token,
            &json!({ "action_type": "AudioPlay", "target_monitor_id": speaker.id,
                     "audio_file": 3, "trigger_on": "Manual", "label": "Chime" }),
        )
        .await;
    assert_eq!(r.status(), StatusCode::CREATED, "{}", r.text());
    let aid = r.json::<Value>()["id"].as_u64().unwrap();

    let r = app.get(&format!("{base}/{aid}"), &token).await;
    assert_eq!(r.status(), StatusCode::OK, "{}", r.text());
    assert_eq!(r.json::<Value>()["label"], "Chime");

    let r = app
        .post_json(&format!("{base}/{aid}/run"), &token, &json!({}))
        .await;
    assert_eq!(r.status(), StatusCode::OK, "{}", r.text());
    let mut got = None;
    for _ in 0..50 {
        if let Some(c) = received.lock().unwrap().first() {
            got = Some(c.clone());
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    let got = got.expect("the action reached the speaker's zmcontrol");
    assert_eq!(got["command"], "audioPlay", "{got}");
    assert_eq!(got["file"], 3, "{got}");
}
