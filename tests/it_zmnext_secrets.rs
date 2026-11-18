//! Secrets in stored zm-next graphs: saved encrypted, referenced by
//! `{"$secret": name}` in the graph, never returned by the API.
//!
//!   APP_PROFILE=test-db cargo test --test it_zmnext_secrets -- --include-ignored

mod common;

use std::sync::Arc;

use axum::http::{Method, StatusCode};
use common::fixtures::{insert_monitor, RowGuard};
use common::harness::{superuser_token, TestApp};
use common::test_db::{get_test_db, migrate_test_db};
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};
use serde_json::{json, Value};
use zm_api::entity::{monitor_pipeline, zmnext_secret};
use zm_api::server::state::AppState;

const SECRETS: [&str; 3] = ["Bearer t0k3n-zz", "hunter2-zz", "sk-live-zz"];

async fn app_with_key(tag: &str) -> (TestApp, std::path::PathBuf) {
    let db = get_test_db().await.expect("test database");
    migrate_test_db(&db).await;
    zmnext_secret::Entity::find()
        .all(&db)
        .await
        .expect("zmnext_secret table exists");
    let key_dir = std::env::temp_dir().join(format!("zm_secrets_{tag}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&key_dir);
    let mut state = AppState::for_test_with_db(db);
    let mut config = (*state.config).clone();
    config.zmnext.secrets.key_file = key_dir.join("key");
    state.config = Arc::new(config);
    (TestApp::from_state(state), key_dir)
}

fn graph_with_secrets() -> Value {
    json!({ "plugins": [
        { "id": "detect", "kind": "decode_detect", "cfg": {}, "children": [
            { "id": "notify", "kind": "output_webhook", "cfg": { "auth_header": SECRETS[0] } },
            { "id": "mqtt", "kind": "output_mqtt", "cfg": { "username": "cam", "password": SECRETS[1] } }
        ] },
        { "id": "review", "kind": "llm_event_review", "cfg": { "api_key": SECRETS[2] } }
    ]})
}

fn assert_no_secret(text: &str, ctx: &str) {
    for s in SECRETS {
        assert!(!text.contains(s), "{ctx} leaks {s}: {text}");
    }
}

async fn cleanup(db: &sea_orm::DatabaseConnection, monitor_id: u32) {
    let _ = monitor_pipeline::Entity::delete_by_id(monitor_id)
        .exec(db)
        .await;
    let _ = zmnext_secret::Entity::delete_many()
        .filter(zmnext_secret::Column::MonitorId.eq(monitor_id))
        .exec(db)
        .await;
}

#[tokio::test]
#[ignore = "requires the test database (APP_PROFILE=test-db)"]
async fn secrets_are_stored_encrypted_and_never_returned() {
    let (app, key_dir) = app_with_key("store").await;
    let db = get_test_db().await.unwrap();
    let monitor = insert_monitor(&db, "zmnext_secrets").await.unwrap();
    let _mon = RowGuard::monitor(monitor.id);
    let path = format!("/api/v3/monitors/{}/pipeline", monitor.id);
    let token = superuser_token();

    // Save: the response holds references, not values.
    let put = app
        .request(Method::PUT, &path)
        .bearer(&token)
        .json(&graph_with_secrets())
        .send()
        .await;
    assert_eq!(put.status(), StatusCode::OK, "{}", put.text());
    assert_no_secret(&put.text(), "PUT response");
    let body: Value = put.json();
    assert_eq!(
        body["graph"]["plugins"][0]["children"][0]["cfg"]["auth_header"],
        json!({ "$secret": "notify.auth_header" })
    );
    assert_eq!(
        body["graph"]["plugins"][0]["children"][1]["cfg"]["username"],
        "cam"
    );

    // Stored: neither the graph row nor the secret rows hold plain text.
    let row = monitor_pipeline::Entity::find_by_id(monitor.id)
        .one(&db)
        .await
        .unwrap()
        .expect("graph row");
    assert_no_secret(&row.graph_json, "stored graph");
    let secrets = zmnext_secret::Entity::find()
        .filter(zmnext_secret::Column::MonitorId.eq(monitor.id))
        .all(&db)
        .await
        .unwrap();
    let mut names: Vec<_> = secrets.iter().map(|s| s.name.as_str()).collect();
    names.sort();
    assert_eq!(
        names,
        ["mqtt.password", "notify.auth_header", "review.api_key"]
    );
    for s in &secrets {
        assert_no_secret(&s.ciphertext, "ciphertext");
    }

    // Read back: references only.
    let get = app.get(&path, &token).await;
    assert_eq!(get.status(), StatusCode::OK);
    assert_no_secret(&get.text(), "GET response");

    // Re-save the graph as returned (references) minus the webhook: the kept
    // references still resolve, the dropped secret is pruned.
    let mut edited = body["graph"].clone();
    edited["plugins"][0]["children"]
        .as_array_mut()
        .unwrap()
        .remove(0);
    let put2 = app
        .request(Method::PUT, &path)
        .bearer(&token)
        .json(&edited)
        .send()
        .await;
    assert_eq!(put2.status(), StatusCode::OK, "{}", put2.text());
    let left: Vec<String> = zmnext_secret::Entity::find()
        .filter(zmnext_secret::Column::MonitorId.eq(monitor.id))
        .all(&db)
        .await
        .unwrap()
        .into_iter()
        .map(|s| s.name)
        .collect();
    assert_eq!(left.len(), 2, "{left:?}");
    assert!(!left.contains(&"notify.auth_header".to_string()));

    // A reference to a secret this monitor doesn't have is refused.
    let bogus = json!({ "plugins": [
        { "kind": "output_webhook", "cfg": { "auth_header": { "$secret": "nope.auth_header" } } }
    ]});
    let put3 = app
        .request(Method::PUT, &path)
        .bearer(&token)
        .json(&bogus)
        .send()
        .await;
    assert_eq!(put3.status(), StatusCode::BAD_REQUEST, "{}", put3.text());

    // Deleting the graph removes its secrets.
    let del = app.delete(&path, &token).await;
    assert_eq!(del.status(), StatusCode::OK, "{}", del.text());
    let remaining = zmnext_secret::Entity::find()
        .filter(zmnext_secret::Column::MonitorId.eq(monitor.id))
        .all(&db)
        .await
        .unwrap();
    assert!(remaining.is_empty(), "{remaining:?}");

    cleanup(&db, monitor.id).await;
    let _ = std::fs::remove_dir_all(&key_dir);
}

/// A graph saved before this change still holds plain secrets. Reading it
/// moves them into the store first, and the worker path resolves them back.
#[tokio::test]
#[ignore = "requires the test database (APP_PROFILE=test-db)"]
async fn a_legacy_graph_is_migrated_on_read_and_resolves_for_the_worker() {
    let (app, key_dir) = app_with_key("legacy").await;
    let db = get_test_db().await.unwrap();
    let monitor = insert_monitor(&db, "zmnext_secrets_legacy").await.unwrap();
    let _mon = RowGuard::monitor(monitor.id);
    let now = zm_api::util::time::utc_now();
    zm_api::repo::monitor_pipeline::upsert(
        &db,
        monitor.id,
        graph_with_secrets().to_string(),
        1,
        now,
    )
    .await
    .unwrap();

    let get = app
        .get(
            &format!("/api/v3/monitors/{}/pipeline", monitor.id),
            &superuser_token(),
        )
        .await;
    assert_eq!(get.status(), StatusCode::OK, "{}", get.text());
    assert_no_secret(&get.text(), "GET of a legacy row");

    let row = monitor_pipeline::Entity::find_by_id(monitor.id)
        .one(&db)
        .await
        .unwrap()
        .unwrap();
    assert_no_secret(&row.graph_json, "migrated row");

    // What the worker gets: the original values.
    let mut graph: Value = serde_json::from_str(&row.graph_json).unwrap();
    let map = zm_api::service::zmnext::secrets::load_secrets(
        &db,
        &key_dir.join("key"),
        monitor.id,
        &graph,
    )
    .await
    .unwrap();
    zm_api::service::zmnext::secrets::resolve_in_place(&mut graph, &map).unwrap();
    assert_eq!(graph, graph_with_secrets());

    cleanup(&db, monitor.id).await;
    let _ = std::fs::remove_dir_all(&key_dir);
}
