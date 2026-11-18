//! System-wide control across servers: `?server_id=` on the daemon and system
//! routes, and the run state reaching every server.
//!
//! The app under test is server `here`. Server `there` is a stub that records
//! what reaches it. Each test uses its own pair of server ids, because the
//! `Servers` rows are shared database state.
//!
//! Requires the test database. Run with:
//!   APP_PROFILE=test-db cargo test --test it_cluster_system -- --include-ignored

mod common;

use axum::http::{Method, StatusCode};
use common::assertions::{assert_error, assert_status};
use common::cluster::{peer, peer_json, server_as, server_row};
use common::fixtures::{insert_monitor, unique_name, RowGuard};
use common::harness::{superuser_token, token_with, TestApp};
use sea_orm::{ActiveModelTrait, EntityTrait, Set};
use serde_json::{json, Value};
use zm_api::util::authz::{Level, UserPermissions};

/// The server ids test number `n` uses.
fn ids(n: u32) -> (u32, u32) {
    (7200 + 2 * n, 7201 + 2 * n)
}

fn entry(body: &Value, server_id: u32) -> &Value {
    body["servers"]
        .as_array()
        .expect("servers array")
        .iter()
        .find(|e| e["server_id"] == server_id)
        .unwrap_or_else(|| panic!("no entry for server {server_id} in {body}"))
}

const DAEMONS: &str = "/api/v3/daemons";
const STATUS: &str = "/api/v3/system/status";

#[tokio::test]
#[ignore = "requires the test database (APP_PROFILE=test-db)"]
async fn a_request_for_another_server_is_relayed_with_the_callers_token() {
    let (here, there) = ids(0);
    let stub = peer_json(StatusCode::OK, json!({ "daemons": [] })).await;
    let app = server_as(here, there, Some(&stub.url)).await;
    let _a = server_row(&app.db, here, "zm-a").await;
    let _b = server_row(&app.db, there, "zm-b").await;
    let token = superuser_token();

    let r = app
        .get(&format!("{DAEMONS}?server_id={there}"), &token)
        .await;

    assert_status(&r, StatusCode::OK);
    assert_eq!(r.json::<Value>(), json!({ "daemons": [] }));
    let seen = stub.seen();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].method, Method::GET);
    assert_eq!(
        seen[0].path_and_query,
        format!("{DAEMONS}?server_id={there}")
    );
    assert_eq!(seen[0].headers["authorization"], format!("Bearer {token}"));
    assert_eq!(seen[0].headers["x-zm-forwarded-by"], here.to_string());
}

#[tokio::test]
#[ignore = "requires the test database (APP_PROFILE=test-db)"]
async fn a_write_for_another_server_keeps_its_method_path_and_body() {
    let (here, there) = ids(1);
    let stub = peer_json(
        StatusCode::OK,
        json!({ "success": true, "message": "restarted" }),
    )
    .await;
    let app = server_as(here, there, Some(&stub.url)).await;
    let _a = server_row(&app.db, here, "zm-a").await;
    let _b = server_row(&app.db, there, "zm-b").await;

    let r = app
        .post_json(
            &format!("/api/v3/daemons/zmc%20-m%201/start?server_id={there}"),
            &superuser_token(),
            &json!({ "args": ["x"] }),
        )
        .await;

    assert_status(&r, StatusCode::OK);
    let seen = stub.seen();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].method, Method::POST);
    assert_eq!(
        seen[0].path_and_query,
        format!("/api/v3/daemons/zmc%20-m%201/start?server_id={there}")
    );
    assert_eq!(
        serde_json::from_slice::<Value>(&seen[0].body).unwrap(),
        json!({ "args": ["x"] })
    );
}

#[tokio::test]
#[ignore = "requires the test database (APP_PROFILE=test-db)"]
async fn no_server_id_acts_here_and_contacts_nobody() {
    let (here, there) = ids(2);
    let stub = peer(StatusCode::OK).await;
    let app = server_as(here, there, Some(&stub.url)).await;
    let _a = server_row(&app.db, here, "zm-a").await;
    let _b = server_row(&app.db, there, "zm-b").await;

    // Passive mode with no zmdc.sock: this server's own answer is a 503.
    let r = app.get(DAEMONS, &superuser_token()).await;

    assert_status(&r, StatusCode::SERVICE_UNAVAILABLE);
    assert!(stub.seen().is_empty());
}

#[tokio::test]
#[ignore = "requires the test database (APP_PROFILE=test-db)"]
async fn naming_this_server_is_handled_here() {
    let (here, there) = ids(3);
    let stub = peer(StatusCode::OK).await;
    let app = server_as(here, there, Some(&stub.url)).await;
    let _a = server_row(&app.db, here, "zm-a").await;
    let _b = server_row(&app.db, there, "zm-b").await;

    let r = app
        .get(&format!("{STATUS}?server_id={here}"), &superuser_token())
        .await;

    assert_status(&r, StatusCode::OK);
    assert!(r.json::<Value>().get("running").is_some());
    assert!(stub.seen().is_empty());
}

#[tokio::test]
#[ignore = "requires the test database (APP_PROFILE=test-db)"]
async fn an_unknown_server_is_404_and_a_bad_value_is_400() {
    let (here, there) = ids(4);
    let stub = peer(StatusCode::OK).await;
    let app = server_as(here, there, Some(&stub.url)).await;
    let _a = server_row(&app.db, here, "zm-a").await;
    let _b = server_row(&app.db, there, "zm-b").await;
    let token = superuser_token();

    let r = app.get(&format!("{STATUS}?server_id=7999"), &token).await;
    assert_status(&r, StatusCode::NOT_FOUND);

    for bad in ["two", "", "-1"] {
        let r = app.get(&format!("{STATUS}?server_id={bad}"), &token).await;
        assert_status(&r, StatusCode::BAD_REQUEST);
    }
    assert!(stub.seen().is_empty());
}

#[tokio::test]
#[ignore = "requires the test database (APP_PROFILE=test-db)"]
async fn a_server_with_no_address_is_misdirected() {
    let (here, there) = ids(5);
    let app = server_as(here, there, None).await;
    let _a = server_row(&app.db, here, "zm-a").await;
    let _b = server_row(&app.db, there, "zm-b").await;

    let r = app
        .get(&format!("{STATUS}?server_id={there}"), &superuser_token())
        .await;

    assert_error(
        &r,
        StatusCode::MISDIRECTED_REQUEST,
        "MISDIRECTED_REQUEST_ERROR",
    );
    assert!(
        r.text().contains(&format!("server {there}")),
        "{}",
        r.text()
    );
}

#[tokio::test]
#[ignore = "requires the test database (APP_PROFILE=test-db)"]
async fn all_returns_one_entry_per_server() {
    let (here, there) = ids(6);
    let stub = peer_json(
        StatusCode::OK,
        json!({ "running": true, "daemons": [], "marker": "from the stub" }),
    )
    .await;
    let app = server_as(here, there, Some(&stub.url)).await;
    let _a = server_row(&app.db, here, "zm-a").await;
    let _b = server_row(&app.db, there, "zm-b").await;

    let r = app
        .get(&format!("{STATUS}?server_id=all"), &superuser_token())
        .await;

    assert_status(&r, StatusCode::MULTI_STATUS);
    let body = r.json::<Value>();
    let servers = body["servers"].as_array().unwrap();
    assert_eq!(servers[0]["server_id"], here, "this server comes first");

    let local = entry(&body, here);
    assert_eq!(local["ok"], true);
    assert_eq!(local["name"], "zm-a");
    assert!(local["result"].get("running").is_some());

    let remote = entry(&body, there);
    assert_eq!(remote["ok"], true);
    assert_eq!(remote["status"], 200);
    assert_eq!(remote["name"], "zm-b");
    assert_eq!(remote["result"]["marker"], "from the stub");

    // The stub is asked as itself, once.
    let seen = stub.seen();
    assert_eq!(seen.len(), 1);
    assert_eq!(
        seen[0].path_and_query,
        format!("{STATUS}?server_id={there}")
    );
}

#[tokio::test]
#[ignore = "requires the test database (APP_PROFILE=test-db)"]
async fn a_server_that_is_down_does_not_fail_the_rest() {
    let (here, there) = ids(7);
    // Nothing listens on port 1.
    let app = server_as(here, there, Some("http://127.0.0.1:1")).await;
    let _a = server_row(&app.db, here, "zm-a").await;
    let _b = server_row(&app.db, there, "zm-b").await;

    let r = app
        .get(&format!("{STATUS}?server_id=all"), &superuser_token())
        .await;

    assert_status(&r, StatusCode::MULTI_STATUS);
    let body = r.json::<Value>();
    assert_eq!(entry(&body, here)["ok"], true);
    let down = entry(&body, there);
    assert_eq!(down["ok"], false);
    assert_eq!(down["status"], 502);
    let error = down["error"].as_str().unwrap();
    assert!(error.contains(&format!("server {there} (zm-b)")), "{error}");
    assert!(down.get("result").is_none());
}

#[tokio::test]
#[ignore = "requires the test database (APP_PROFILE=test-db)"]
async fn a_relayed_request_is_never_relayed_again() {
    let (here, there) = ids(8);
    let stub = peer(StatusCode::OK).await;
    let app = server_as(here, there, Some(&stub.url)).await;
    let _a = server_row(&app.db, here, "zm-a").await;
    let _b = server_row(&app.db, there, "zm-b").await;
    let token = superuser_token();

    for value in ["all", &there.to_string(), "nonsense"] {
        let r = app
            .request(Method::GET, &format!("{STATUS}?server_id={value}"))
            .bearer(&token)
            .header("x-zm-forwarded-by", "9")
            .send()
            .await;
        assert_status(&r, StatusCode::OK);
        let body = r.json::<Value>();
        assert!(body.get("servers").is_none(), "{value}: {body}");
        assert!(body.get("running").is_some(), "{value}: {body}");
    }
    assert!(stub.seen().is_empty());
}

#[tokio::test]
#[ignore = "requires the test database (APP_PROFILE=test-db)"]
async fn someone_without_the_permission_is_refused_before_anything_is_relayed() {
    let (here, there) = ids(9);
    let stub = peer(StatusCode::OK).await;
    let app = server_as(here, there, Some(&stub.url)).await;
    let _a = server_row(&app.db, here, "zm-a").await;
    let _b = server_row(&app.db, there, "zm-b").await;
    let token = token_with(UserPermissions {
        system: Level::None,
        ..UserPermissions::superuser()
    });

    for suffix in [there.to_string(), "all".to_string()] {
        let r = app
            .get(&format!("{DAEMONS}?server_id={suffix}"), &token)
            .await;
        assert_status(&r, StatusCode::FORBIDDEN);
        let r = app
            .post_json(
                &format!("/api/v3/system/restart?server_id={suffix}"),
                &token,
                &json!({}),
            )
            .await;
        assert_status(&r, StatusCode::FORBIDDEN);
    }
    assert!(stub.seen().is_empty());
}

#[tokio::test]
#[ignore = "requires the test database (APP_PROFILE=test-db)"]
async fn a_single_server_install_answers_all_with_its_one_server() {
    // The harness default: no server id, nothing relayed.
    let app = TestApp::spawn().await;
    let token = superuser_token();

    let r = app.get(&format!("{STATUS}?server_id=all"), &token).await;

    assert_status(&r, StatusCode::MULTI_STATUS);
    let body = r.json::<Value>();
    let servers = body["servers"].as_array().unwrap();
    assert_eq!(servers.len(), 1);
    assert_eq!(servers[0]["server_id"], Value::Null);
    assert_eq!(servers[0]["ok"], true);
    assert!(servers[0]["result"].get("running").is_some());

    // Any number is some other host, which this install does not know.
    let r = app.get(&format!("{STATUS}?server_id=2"), &token).await;
    assert_status(&r, StatusCode::NOT_FOUND);
    let r = app.get(STATUS, &token).await;
    assert_status(&r, StatusCode::OK);
}

/// Applying a state deactivates every other state, so the tests that apply or
/// check one take turns.
static STATES: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// A run state naming one monitor, and guards for both rows.
async fn state_with_monitor(
    db: &sea_orm::DatabaseConnection,
    label: &str,
) -> (String, u32, u32, RowGuard, RowGuard) {
    let monitor = insert_monitor(db, "SysState").await.expect("monitor");
    let name = unique_name(label);
    let state = zm_api::entity::states::ActiveModel {
        name: Set(name.clone()),
        definition: Set(format!("{}:Always:Always:OnMotion", monitor.id)),
        is_active: Set(0),
        ..Default::default()
    }
    .insert(db)
    .await
    .expect("state");
    (
        name,
        state.id,
        monitor.id,
        RowGuard::state(state.id),
        RowGuard::monitor(monitor.id),
    )
}

#[tokio::test]
#[ignore = "requires the test database (APP_PROFILE=test-db)"]
async fn applying_a_state_tells_each_other_server_once() {
    let (here, there) = ids(10);
    let _turn = STATES.lock().await;
    let stub = peer_json(
        StatusCode::OK,
        json!({ "success": true, "message": "2 monitors with daemons restarting" }),
    )
    .await;
    let app = server_as(here, there, Some(&stub.url)).await;
    let _a = server_row(&app.db, here, "zm-a").await;
    let _b = server_row(&app.db, there, "zm-b").await;
    let (name, state_id, monitor_id, _s, _m) = state_with_monitor(&app.db, "SysTold").await;
    let token = superuser_token();

    let r = app
        .post_json(
            "/api/v3/system/state",
            &token,
            &json!({ "state_name": name }),
        )
        .await;

    assert_status(&r, StatusCode::OK);
    let body = r.json::<Value>();
    // The fields clients already read describe this server.
    assert_eq!(body["success"], true);
    assert!(body["message"].as_str().unwrap().contains(&name));
    let other = entry(&body, there);
    assert_eq!(other["ok"], true);
    assert_eq!(
        other["result"]["message"],
        "2 monitors with daemons restarting"
    );

    // The rows changed here, once.
    let row = zm_api::entity::monitors::Entity::find_by_id(monitor_id)
        .one(&app.db)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        row.recording,
        zm_api::entity::sea_orm_active_enums::Recording::OnMotion
    );
    let active = zm_api::entity::states::Entity::find_by_id(state_id)
        .one(&app.db)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(active.is_active, 1);

    // The other server was told once, to restart daemons and not to write.
    let seen = stub.seen();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].method, Method::POST);
    assert_eq!(seen[0].headers["authorization"], format!("Bearer {token}"));
    assert_eq!(seen[0].headers["x-zm-forwarded-by"], here.to_string());
    assert_eq!(
        serde_json::from_slice::<Value>(&seen[0].body).unwrap(),
        json!({ "state_name": name, "daemons_only": true })
    );
}

#[tokio::test]
#[ignore = "requires the test database (APP_PROFILE=test-db)"]
async fn a_relayed_state_is_applied_here_and_goes_no_further() {
    let (here, there) = ids(11);
    let _turn = STATES.lock().await;
    let stub = peer(StatusCode::OK).await;
    let app = server_as(here, there, Some(&stub.url)).await;
    let _a = server_row(&app.db, here, "zm-a").await;
    let _b = server_row(&app.db, there, "zm-b").await;
    let (name, _sid, _mid, _s, _m) = state_with_monitor(&app.db, "SysRelayed").await;

    let r = app
        .request(Method::POST, "/api/v3/system/state?server_id=all")
        .bearer(&superuser_token())
        .header("x-zm-forwarded-by", "9")
        .json(&json!({ "state_name": name }))
        .send()
        .await;

    assert_status(&r, StatusCode::OK);
    assert!(r.json::<Value>().get("servers").is_none());
    assert!(stub.seen().is_empty());
}

#[tokio::test]
#[ignore = "requires the test database (APP_PROFILE=test-db)"]
async fn daemons_only_leaves_the_database_alone() {
    let (here, there) = ids(12);
    let _turn = STATES.lock().await;
    let stub = peer(StatusCode::OK).await;
    let app = server_as(here, there, Some(&stub.url)).await;
    let _a = server_row(&app.db, here, "zm-a").await;
    let _b = server_row(&app.db, there, "zm-b").await;
    let (name, state_id, monitor_id, _s, _m) = state_with_monitor(&app.db, "SysDaemons").await;
    let before = zm_api::entity::monitors::Entity::find_by_id(monitor_id)
        .one(&app.db)
        .await
        .unwrap()
        .unwrap();

    let r = app
        .post_json(
            "/api/v3/system/state",
            &superuser_token(),
            &json!({ "state_name": name, "daemons_only": true }),
        )
        .await;

    assert_status(&r, StatusCode::OK);
    assert!(r.json::<Value>().get("servers").is_none());
    let after = zm_api::entity::monitors::Entity::find_by_id(monitor_id)
        .one(&app.db)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(before.recording, after.recording);
    let state = zm_api::entity::states::Entity::find_by_id(state_id)
        .one(&app.db)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(state.is_active, 0, "a daemons-only request writes nothing");
    assert!(stub.seen().is_empty(), "and is not passed on");
}

#[tokio::test]
#[ignore = "requires the test database (APP_PROFILE=test-db)"]
async fn a_state_cannot_be_aimed_at_one_server() {
    let (here, there) = ids(13);
    let _turn = STATES.lock().await;
    let stub = peer(StatusCode::OK).await;
    let app = server_as(here, there, Some(&stub.url)).await;
    let _a = server_row(&app.db, here, "zm-a").await;
    let _b = server_row(&app.db, there, "zm-b").await;
    let (name, state_id, _mid, _s, _m) = state_with_monitor(&app.db, "SysOne").await;

    let r = app
        .post_json(
            &format!("/api/v3/system/state?server_id={there}"),
            &superuser_token(),
            &json!({ "state_name": name }),
        )
        .await;

    assert_status(&r, StatusCode::BAD_REQUEST);
    let state = zm_api::entity::states::Entity::find_by_id(state_id)
        .one(&app.db)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(state.is_active, 0);
    assert!(stub.seen().is_empty());
}

/// What the server that did not apply the state does: restart the daemons of
/// its own monitors that the state names, through its zmdc.pl.
#[tokio::test]
#[ignore = "requires the test database (APP_PROFILE=test-db)"]
async fn daemons_only_restarts_this_servers_monitors_through_zmdc() {
    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let (here, there) = ids(14);
    let _turn = STATES.lock().await;
    let tmp = tempfile::tempdir().unwrap();
    let listener = tokio::net::UnixListener::bind(tmp.path().join("zmdc.sock")).unwrap();
    let got = Arc::new(Mutex::new(Vec::<String>::new()));
    let log = Arc::clone(&got);
    tokio::spawn(async move {
        while let Ok((mut s, _)) = listener.accept().await {
            let mut msg = String::new();
            let _ = s.read_to_string(&mut msg).await;
            log.lock().unwrap().push(msg);
            let _ = s.write_all(b"").await;
        }
    });
    let dir = tmp.path().to_path_buf();
    let app = TestApp::spawn_with(move |state| {
        state.server_id = Some(here);
        state.cluster = Arc::new(zm_api::cluster::Cluster::new(
            Some(here),
            &zm_api::configure::cluster::ClusterConfig::default(),
            0,
            false,
        ));
        let mut config = (*state.config).clone();
        config.daemon.socket_path = dir;
        config.daemon.socket_name = "zmdc.sock".to_string();
        state.config = Arc::new(config);
    })
    .await;
    let _a = server_row(&app.db, here, "zm-a").await;
    let _b = server_row(&app.db, there, "zm-b").await;
    let (name, _sid, monitor_id, _s, _m) = state_with_monitor(&app.db, "SysZmdc").await;
    let mut row: zm_api::entity::monitors::ActiveModel =
        zm_api::entity::monitors::Entity::find_by_id(monitor_id)
            .one(&app.db)
            .await
            .unwrap()
            .unwrap()
            .into();
    row.server_id = Set(Some(here));
    row.capturing = Set(zm_api::entity::sea_orm_active_enums::Capturing::Always);
    row.update(&app.db).await.unwrap();

    let r = app
        .post_json(
            "/api/v3/system/state",
            &superuser_token(),
            &json!({ "state_name": name, "daemons_only": true }),
        )
        .await;
    assert_status(&r, StatusCode::OK);
    assert!(r.json::<Value>()["message"]
        .as_str()
        .unwrap()
        .contains("1 monitors"));

    let stop = format!("stop;zmc;-m;{monitor_id}");
    let start = format!("start;zmc;-m;{monitor_id}");
    for _ in 0..40 {
        if got.lock().unwrap().contains(&start) {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    let got = got.lock().unwrap();
    assert!(got.contains(&stop), "zmdc got {got:?}");
    assert!(got.contains(&start), "zmdc got {got:?}");
}
