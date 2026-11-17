//! In passive mode, monitor changes reach ZoneMinder's daemons through its
//! `zmdc.sock`, and daemon status is read from it (#155, #156).
//!
//! A fake zmdc.sock records what zm-api sends and answers like zmdc.pl.
//! Requires the test database:
//!   APP_PROFILE=test-db cargo nextest run --test it_passive_daemon_control --run-ignored all

mod common;

use std::sync::{Arc, Mutex};

use axum::http::StatusCode;
use common::fixtures::{insert_monitor, RowGuard};
use common::harness::{superuser_token, TestApp};
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Listen on `dir/zmdc.sock`; record each message and reply with `reply`.
fn fake_zmdc(dir: &std::path::Path, reply: &'static str) -> Arc<Mutex<Vec<String>>> {
    let listener = tokio::net::UnixListener::bind(dir.join("zmdc.sock")).unwrap();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = Arc::clone(&seen);
    tokio::spawn(async move {
        while let Ok((mut s, _)) = listener.accept().await {
            let mut msg = String::new();
            let _ = s.read_to_string(&mut msg).await;
            log.lock().unwrap().push(msg);
            let _ = s.write_all(reply.as_bytes()).await;
        }
    });
    seen
}

async fn passive_app(dir: &std::path::Path) -> TestApp {
    let dir = dir.to_path_buf();
    TestApp::spawn_with(move |state| {
        assert!(state.daemon_manager.is_none(), "passive: no daemon manager");
        let mut config = (*state.config).clone();
        config.daemon.socket_path = dir;
        config.daemon.socket_name = "zmdc.sock".to_string();
        state.config = Arc::new(config);
    })
    .await
}

#[tokio::test]
#[ignore = "requires the test database (APP_PROFILE=test-db)"]
async fn monitor_state_and_delete_go_to_zmdc() {
    let tmp = tempfile::tempdir().unwrap();
    let seen = fake_zmdc(tmp.path(), "");
    let app = passive_app(tmp.path()).await;
    let token = superuser_token();
    let m = insert_monitor(&app.db, "PassiveCtl").await.unwrap();
    let _g = RowGuard::monitor(m.id);

    let resp = app
        .patch_json(
            &format!("/api/v3/monitors/{}/state", m.id),
            &token,
            &json!({ "state": "stop" }),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK, "{}", resp.text());
    assert!(
        seen.lock()
            .unwrap()
            .contains(&format!("stop;zmc;-m;{}", m.id)),
        "zmdc got {:?}",
        seen.lock().unwrap()
    );

    seen.lock().unwrap().clear();
    let resp = app
        .delete(&format!("/api/v3/monitors/{}", m.id), &token)
        .await;
    assert!(resp.status().is_success(), "{}", resp.text());
    assert!(seen
        .lock()
        .unwrap()
        .contains(&format!("stop;zmc;-m;{}", m.id)));
}

#[tokio::test]
#[ignore = "requires the test database (APP_PROFILE=test-db)"]
async fn daemon_and_system_status_come_from_zmdc() {
    let tmp = tempfile::tempdir().unwrap();
    let seen = fake_zmdc(
        tmp.path(),
        "'zmc -m 1' running since 26/10/18 10:00:00, pid = 4321, valid\n",
    );
    let app = passive_app(tmp.path()).await;
    let token = superuser_token();

    let resp = app.get("/api/v3/daemons", &token).await;
    assert_eq!(resp.status(), StatusCode::OK, "{}", resp.text());
    assert!(resp.text().contains("4321"), "{}", resp.text());

    let resp = app.get("/api/v3/system/status", &token).await;
    assert_eq!(resp.status(), StatusCode::OK, "{}", resp.text());
    let body: serde_json::Value = resp.json();
    assert_eq!(body["running"], true);
    assert_eq!(body["daemons"][0]["pid"], 4321);
    assert!(seen.lock().unwrap().iter().all(|m| m == "status"));
}

/// On a multi-server install this host's zmdc.pl runs only the monitors
/// whose ServerId is this host; another server's are left to it (#229).
#[tokio::test]
#[ignore = "requires the test database (APP_PROFILE=test-db)"]
async fn another_servers_monitor_is_left_to_that_server() {
    use sea_orm::{ActiveModelTrait, Set};
    let tmp = tempfile::tempdir().unwrap();
    let seen = fake_zmdc(tmp.path(), "");
    let dir = tmp.path().to_path_buf();
    let app = TestApp::spawn_with(move |state| {
        let mut config = (*state.config).clone();
        config.daemon.socket_path = dir;
        config.daemon.socket_name = "zmdc.sock".to_string();
        state.config = Arc::new(config);
        state.server_id = Some(1);
    })
    .await;
    let token = superuser_token();

    let mut ids = Vec::new();
    let mut guards = Vec::new();
    for server in [1u32, 2] {
        let m = insert_monitor(&app.db, "MultiSrv").await.unwrap();
        guards.push(RowGuard::monitor(m.id));
        let mut am: zm_api::entity::monitors::ActiveModel = m.clone().into();
        am.server_id = Set(Some(server));
        am.update(&app.db).await.unwrap();
        ids.push(m.id);
    }
    for id in &ids {
        let resp = app
            .patch_json(
                &format!("/api/v3/monitors/{id}/state"),
                &token,
                &json!({ "state": "stop" }),
            )
            .await;
        assert_eq!(resp.status(), StatusCode::OK, "{}", resp.text());
    }
    let seen = seen.lock().unwrap().clone();
    assert!(
        seen.contains(&format!("stop;zmc;-m;{}", ids[0])),
        "{seen:?}"
    );
    assert!(
        !seen.iter().any(|m| m.contains(&format!("-m;{}", ids[1]))),
        "server 2's monitor went to server 1's zmdc: {seen:?}"
    );
}

/// Applying a run state in passive mode moves the daemons of the monitors
/// it changes, as an edit does, instead of only rewriting their rows (#204).
#[tokio::test]
#[ignore = "requires the test database (APP_PROFILE=test-db)"]
async fn a_run_state_moves_the_daemons_it_changes() {
    use sea_orm::{ActiveModelTrait, ColumnTrait, EntityTrait, QueryFilter, Set};
    use zm_api::entity::states;
    let tmp = tempfile::tempdir().unwrap();
    let seen = fake_zmdc(tmp.path(), "");
    let app = passive_app(tmp.path()).await;
    let token = superuser_token();

    let off = insert_monitor(&app.db, "StateOff").await.unwrap();
    let _g1 = RowGuard::monitor(off.id);
    let same = insert_monitor(&app.db, "StateSame").await.unwrap();
    let _g2 = RowGuard::monitor(same.id);
    let as_is = |m: &zm_api::entity::monitors::Model| {
        format!(
            "{}:{}:{}:{}",
            m.id,
            serde_json::to_value(&m.capturing)
                .unwrap()
                .as_str()
                .unwrap(),
            serde_json::to_value(&m.analysing)
                .unwrap()
                .as_str()
                .unwrap(),
            serde_json::to_value(&m.recording)
                .unwrap()
                .as_str()
                .unwrap(),
        )
    };

    // Put the active-state flags back afterwards: applying a state moves them.
    let was_active: Vec<u32> = states::Entity::find()
        .filter(states::Column::IsActive.eq(1u8))
        .all(&app.db)
        .await
        .unwrap()
        .into_iter()
        .map(|s| s.id)
        .collect();
    let name = common::fixtures::unique_name("Off");
    let st = states::ActiveModel {
        name: Set(name.clone()),
        definition: Set(format!("{}:None:None:None,{}", off.id, as_is(&same))),
        is_active: Set(0),
        ..Default::default()
    }
    .insert(&app.db)
    .await
    .unwrap();
    let st_id = st.id;
    let _gs = RowGuard::new(format!("States#{st_id}"), move |db| async move {
        let _ = states::Entity::delete_by_id(st_id).exec(&db).await;
        for id in was_active {
            let _ = states::ActiveModel {
                id: Set(id),
                is_active: Set(1),
                ..Default::default()
            }
            .update(&db)
            .await;
        }
    });

    let resp = app
        .post_json(
            "/api/v3/system/state",
            &token,
            &json!({ "state_name": name }),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK, "{}", resp.text());
    // The restarts run after the answer.
    let stop = format!("stop;zmc;-m;{}", off.id);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !seen.lock().unwrap().contains(&stop) && std::time::Instant::now() < deadline {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    let seen = seen.lock().unwrap().clone();
    assert!(
        seen.contains(&stop),
        "the monitor turned off was stopped: {seen:?}"
    );
    assert!(
        !seen
            .iter()
            .any(|m| m.contains(&format!("zmc;-m;{}", off.id)) && m.starts_with("start")),
        "and not started again: {seen:?}"
    );
    assert!(
        !seen.iter().any(|m| m.contains(&format!("-m;{}", same.id))),
        "an unchanged monitor is left alone: {seen:?}"
    );
}
