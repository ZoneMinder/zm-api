//! Daemon control in takeover mode, through the API, on a real process (#240).
//!
//! With `[daemon] enabled`, zm-api supervises ZoneMinder's daemons itself.
//! Here its daemon manager runs a stand-in `zmc` (a shell script on a temp
//! `bin_path`) that writes each start and each SIGHUP to a log, so start,
//! status, reload, restart and stop are checked against what the process did.
//!
//!   APP_PROFILE=test-db cargo test --test it_daemon_control -- --include-ignored

mod common;

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::http::StatusCode;
use common::harness::{superuser_token, TestApp};
use serde_json::{json, Value};
use zm_api::daemon::{DaemonConfig, DaemonManager};

/// A `zmc` that logs `start <args>`, `hup` and `winch`, and exits on SIGTERM.
/// The traps go first: a signal sent once `start` is logged must find them.
fn fake_zmc(dir: &Path) -> PathBuf {
    let log = dir.join("zmc.log");
    let script = dir.join("zmc");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\n\
             trap 'echo hup >> \"{log}\"' HUP\n\
             trap 'echo winch >> \"{log}\"' WINCH\n\
             trap 'exit 0' TERM\n\
             echo \"start $*\" >> '{log}'\n\
             while true; do sleep 1 & wait $!; done\n",
            log = log.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    log
}

async fn app_with_manager(bin: &Path) -> TestApp {
    let bin = bin.to_path_buf();
    TestApp::spawn_with(move |state| {
        let config = DaemonConfig {
            enabled: true,
            bin_path: bin.clone(),
            script_path: bin,
            min_backoff_seconds: 1,
            shutdown_timeout_seconds: 5,
            ..DaemonConfig::default()
        };
        let manager = DaemonManager::with_database(config, None, state.db.clone());
        state.daemon_manager = Some(Arc::new(manager));
    })
    .await
}

fn log_lines(log: &Path) -> Vec<String> {
    std::fs::read_to_string(log)
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect()
}

async fn wait_for(log: &Path, what: &str, count: usize) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while log_lines(log).iter().filter(|l| l.as_str() == what).count() < count {
        assert!(
            Instant::now() < deadline,
            "waited for {count}× {what:?}; log: {:?}",
            log_lines(log)
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[tokio::test]
#[ignore = "requires the test database (APP_PROFILE=test-db)"]
async fn a_daemon_is_started_reloaded_restarted_and_stopped() {
    let tmp = tempfile::tempdir().unwrap();
    let log = fake_zmc(tmp.path());
    let app = app_with_manager(tmp.path()).await;
    let token = superuser_token();

    let r = app
        .post_json(
            "/api/v3/daemons/zmc/start",
            &token,
            &json!({ "args": ["-m", "7"] }),
        )
        .await;
    assert_eq!(r.status(), StatusCode::OK, "{}", r.text());
    wait_for(&log, "start -m 7", 1).await;

    // It's listed, and its status can be read by id.
    let r = app.get("/api/v3/daemons", &token).await;
    assert_eq!(r.status(), StatusCode::OK, "{}", r.text());
    let list: Value = r.json();
    let id = list["daemons"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|d| d["id"].as_str())
        .find(|id| id.starts_with("zmc"))
        .expect("zmc listed")
        .to_string();
    let path = |action: &str| {
        let id = urlencoding::encode(&id);
        if action.is_empty() {
            format!("/api/v3/daemons/{id}")
        } else {
            format!("/api/v3/daemons/{id}/{action}")
        }
    };
    let r = app.get(&path(""), &token).await;
    assert_eq!(r.status(), StatusCode::OK, "{}", r.text());

    let r = app.post_json(&path("reload"), &token, &json!({})).await;
    assert_eq!(r.status(), StatusCode::OK, "{}", r.text());
    wait_for(&log, "hup", 1).await;

    let r = app.post_json(&path("restart"), &token, &json!({})).await;
    assert_eq!(r.status(), StatusCode::OK, "{}", r.text());
    wait_for(&log, "start -m 7", 2).await;

    let r = app.post_json(&path("stop"), &token, &json!({})).await;
    assert_eq!(r.status(), StatusCode::OK, "{}", r.text());
    let r = app.get(&path(""), &token).await;
    let running = r.json::<Value>()["running"].as_bool();
    assert_ne!(running, Some(true), "stopped: {}", r.text());
}

#[tokio::test]
#[ignore = "requires the test database (APP_PROFILE=test-db)"]
async fn an_unknown_daemon_is_not_found_and_a_foreign_command_is_refused() {
    let tmp = tempfile::tempdir().unwrap();
    fake_zmc(tmp.path());
    let app = app_with_manager(tmp.path()).await;
    let token = superuser_token();

    let r = app.get("/api/v3/daemons/zmc%20-m%20424242", &token).await;
    assert_eq!(r.status(), StatusCode::NOT_FOUND, "{}", r.text());
    // Only ZoneMinder's daemons may be started.
    let r = app
        .post_json(
            "/api/v3/daemons/sh/start",
            &token,
            &json!({ "args": ["-c", "true"] }),
        )
        .await;
    assert_eq!(r.status(), StatusCode::BAD_REQUEST, "{}", r.text());
}

/// A `zmpkg.pl` that logs the action it was given.
fn fake_zmpkg(dir: &Path) -> PathBuf {
    let log = dir.join("zmpkg.log");
    let script = dir.join("zmpkg.pl");
    std::fs::write(
        &script,
        format!("#!/bin/sh\necho \"$1\" >> '{}'\n", log.display()),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    log
}

#[tokio::test]
#[ignore = "requires the test database (APP_PROFILE=test-db)"]
async fn system_startup_runs_capture_and_logrot_signals_it() {
    use common::fixtures::{insert_monitor, RowGuard};
    use sea_orm::{ActiveModelTrait, Set};
    use zm_api::entity::sea_orm_active_enums::Capturing;

    let tmp = tempfile::tempdir().unwrap();
    let log = fake_zmc(tmp.path());
    let app = app_with_manager(tmp.path()).await;
    let token = superuser_token();
    let m = insert_monitor(&app.db, "DaemonStartup")
        .await
        .expect("monitor");
    let _m = RowGuard::monitor(m.id);
    let mut capturing: zm_api::entity::monitors::ActiveModel = m.clone().into();
    capturing.capturing = Set(Capturing::Always);
    capturing.update(&app.db).await.expect("capturing");

    let r = app
        .post_json("/api/v3/system/startup", &token, &json!({}))
        .await;
    assert_eq!(r.status(), StatusCode::OK, "{}", r.text());
    let started = format!("start -m {}", m.id);
    wait_for(&log, &started, 1).await;

    // Log rotation reopens logs (SIGWINCH), it doesn't reload capture.
    let r = app
        .post_json("/api/v3/system/logrot", &token, &json!({}))
        .await;
    assert_eq!(r.status(), StatusCode::OK, "{}", r.text());
    wait_for(&log, "winch", 1).await;
    assert!(
        !log_lines(&log).contains(&"hup".to_string()),
        "{:?}",
        log_lines(&log)
    );

    // A system restart stops capture and starts it again.
    let r = app
        .post_json("/api/v3/system/restart", &token, &json!({}))
        .await;
    assert_eq!(r.status(), StatusCode::OK, "{}", r.text());
    wait_for(&log, &started, 2).await;

    let r = app
        .post_json("/api/v3/system/shutdown", &token, &json!({}))
        .await;
    assert_eq!(r.status(), StatusCode::OK, "{}", r.text());
}

#[tokio::test]
#[ignore = "requires the test database (APP_PROFILE=test-db)"]
async fn server_control_runs_zmpkg_when_systemd_cant() {
    let tmp = tempfile::tempdir().unwrap();
    let log = fake_zmpkg(tmp.path());
    let bin = tmp.path().to_path_buf();
    let app = TestApp::spawn_with(move |state| {
        let mut config = (*state.config).clone();
        config.daemon.bin_path = bin;
        state.config = Arc::new(config);
    })
    .await;
    let token = superuser_token();

    for action in ["restart", "stop", "start"] {
        let r = app
            .post_json(
                &format!("/api/v3/server/control/{action}"),
                &token,
                &json!({}),
            )
            .await;
        assert_eq!(r.status(), StatusCode::OK, "{action}: {}", r.text());
    }
    assert_eq!(log_lines(&log), ["restart", "stop", "start"]);

    let r = app
        .post_json("/api/v3/server/control/reboot", &token, &json!({}))
        .await;
    assert_eq!(r.status(), StatusCode::BAD_REQUEST, "{}", r.text());
}
