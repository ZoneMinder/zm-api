//! Create, update, read back and delete each simple resource (#240).
//!
//! A route sweep over the suite found these updates (and some reads) had
//! never once answered 2xx in a test: they were reached only by the
//! permission sweep, which checks that they refuse a token without
//! permissions. Each resource here goes through its full life through the
//! API, and the update is checked by reading it back.
//!
//!   APP_PROFILE=test-db cargo test --test it_crud_round_trips -- --include-ignored

mod common;

use axum::http::{Method, StatusCode};
use common::fixtures::{insert_group, insert_monitor, insert_user_with_id, unique_name, RowGuard};
use common::harness::{superuser_token, TestApp};
use rust_decimal::Decimal;
use sea_orm::{ActiveModelTrait, Set};
use serde_json::{json, Value};

/// One resource's round trip.
struct Trip {
    /// Collection path, POSTed to create.
    base: String,
    create: Value,
    /// The item path, from the created item.
    item: Box<dyn Fn(&Value) -> String>,
    update_method: Method,
    update: Value,
    /// A field of the item and the value the update must leave in it.
    check: (&'static str, Value),
}

fn by_id(base: &str) -> Box<dyn Fn(&Value) -> String> {
    let base = base.to_string();
    Box::new(move |v| format!("{base}/{}", v["id"]))
}

async fn run(app: &TestApp, token: &str, trip: Trip) -> Result<(), String> {
    let r = app.post_json(&trip.base, token, &trip.create).await;
    if !r.status().is_success() {
        return Err(format!("create {}: {} {}", trip.base, r.status(), r.text()));
    }
    let created: Value = r.json();
    let item = (trip.item)(&created);

    let r = app
        .request(trip.update_method.clone(), &item)
        .bearer(token)
        .json(&trip.update)
        .send()
        .await;
    if r.status() != StatusCode::OK {
        let _ = app.delete(&item, token).await;
        return Err(format!(
            "{} {item}: {} {}",
            trip.update_method,
            r.status(),
            r.text()
        ));
    }
    let r = app.get(&item, token).await;
    let got: Value = r.json();
    let (field, want) = &trip.check;
    let result = if r.status() != StatusCode::OK {
        Err(format!("GET {item}: {} {got}", r.status()))
    } else if got[field] != *want {
        Err(format!(
            "GET {item}: {field} is {} after the update, not {want}",
            got[field]
        ))
    } else {
        let list = app.get(&trip.base, token).await;
        if list.status() != StatusCode::OK {
            Err(format!(
                "GET {}: {} {}",
                trip.base,
                list.status(),
                list.text()
            ))
        } else {
            Ok(())
        }
    };

    let r = app.delete(&item, token).await;
    if result.is_ok() && !r.status().is_success() {
        return Err(format!("DELETE {item}: {} {}", r.status(), r.text()));
    }
    if result.is_ok() && app.get(&item, token).await.status() != StatusCode::NOT_FOUND {
        return Err(format!("GET {item} after DELETE isn't 404"));
    }
    result
}

const UID: u32 = 990_801;

#[tokio::test]
#[ignore = "requires the test database (APP_PROFILE=test-db)"]
async fn each_resource_survives_a_round_trip() {
    let app = TestApp::spawn().await;
    let token = superuser_token();
    let name = |label: &str| unique_name(label);

    insert_user_with_id(&app.db, UID, "CrudUser").await.unwrap();
    let _u = RowGuard::user(UID);
    let monitor = insert_monitor(&app.db, "CrudMon").await.unwrap();
    let _m = RowGuard::monitor(monitor.id);
    let group = insert_group(&app.db, "CrudGroup").await.unwrap();
    let _g = RowGuard::group(group.id);
    let event = zm_api::entity::events::ActiveModel {
        monitor_id: Set(monitor.id),
        state_id: Set(1),
        name: Set(name("CrudEvent")),
        length: Set(Decimal::new(1, 0)),
        ..Default::default()
    }
    .insert(&app.db)
    .await
    .unwrap();
    let eid = event.id;
    let _e = RowGuard::new(format!("Events#{eid}"), move |db| async move {
        use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};
        let _ = zm_api::entity::frames::Entity::delete_many()
            .filter(zm_api::entity::frames::Column::EventId.eq(eid))
            .exec(&db)
            .await;
        let _ = zm_api::entity::events::Entity::delete_by_id(eid)
            .exec(&db)
            .await;
    });
    let r = app
        .post_json(
            "/api/v3/ai/datasets",
            &token,
            &json!({ "name": name("CrudSet"), "num_classes": 80 }),
        )
        .await;
    assert!(r.status().is_success(), "dataset: {}", r.text());
    let dataset: Value = r.json();
    let dataset_id = dataset["id"].clone();

    let mid = monitor.id;
    let now = "2026-10-01T10:00:00Z";
    let later = "2026-10-01T11:00:00Z";
    let patch = Method::PATCH;
    let put = Method::PUT;
    let trips = vec![
        Trip {
            base: "/api/v3/devices".into(),
            create: json!({ "name": name("Dev"), "type": "X10", "key_string": "A1" }),
            item: by_id("/api/v3/devices"),
            update_method: patch.clone(),
            update: json!({ "key_string": "B2" }),
            check: ("key_string", json!("B2")),
        },
        Trip {
            base: "/api/v3/manufacturers".into(),
            create: json!({ "name": name("Maker") }),
            item: by_id("/api/v3/manufacturers"),
            update_method: patch.clone(),
            update: json!({ "name": name("Maker2") }),
            check: ("name", json!(name("Maker2"))),
        },
        Trip {
            base: "/api/v3/models".into(),
            create: json!({ "name": name("Model") }),
            item: by_id("/api/v3/models"),
            update_method: patch.clone(),
            update: json!({ "name": name("Model2") }),
            check: ("name", json!(name("Model2"))),
        },
        Trip {
            base: "/api/v3/monitor_presets".into(),
            create: json!({ "name": name("Preset"), "type": "Ffmpeg" }),
            item: by_id("/api/v3/monitor_presets"),
            update_method: patch.clone(),
            update: json!({ "path": "rtsp://192.0.2.1/stream" }),
            check: ("path", json!("rtsp://192.0.2.1/stream")),
        },
        Trip {
            base: "/api/v3/montage_layouts".into(),
            create: json!({ "name": name("Layout"), "user_id": UID }),
            item: by_id("/api/v3/montage_layouts"),
            update_method: patch.clone(),
            update: json!({ "positions": "{\"default\":{}}" }),
            check: ("positions", json!("{\"default\":{}}")),
        },
        Trip {
            base: "/api/v3/object-types".into(),
            create: json!({ "name": name("Obj"), "human": "Thing" }),
            item: by_id("/api/v3/object-types"),
            update_method: patch.clone(),
            update: json!({ "human": "Other thing" }),
            check: ("human", json!("Other thing")),
        },
        Trip {
            base: "/api/v3/reports".into(),
            create: json!({ "name": "CrudReport", "start_date_time": now, "end_date_time": later }),
            item: by_id("/api/v3/reports"),
            update_method: patch.clone(),
            update: json!({ "name": "CrudReport2" }),
            check: ("name", json!("CrudReport2")),
        },
        Trip {
            base: "/api/v3/states".into(),
            create: json!({ "name": name("State"), "definition": "", "is_active": 0 }),
            item: by_id("/api/v3/states"),
            update_method: patch.clone(),
            update: json!({ "definition": format!("{mid}:Monitor:1") }),
            check: ("definition", json!(format!("{mid}:Monitor:1"))),
        },
        Trip {
            base: "/api/v3/tags".into(),
            create: json!({ "name": name("Tag") }),
            item: by_id("/api/v3/tags"),
            update_method: patch.clone(),
            update: json!({ "name": name("Tag2") }),
            check: ("name", json!(name("Tag2"))),
        },
        Trip {
            base: "/api/v3/user_preferences".into(),
            create: json!({ "user_id": UID, "name": name("Pref"), "value": "a" }),
            item: by_id("/api/v3/user_preferences"),
            update_method: patch.clone(),
            update: json!({ "value": "b" }),
            check: ("value", json!("b")),
        },
        Trip {
            base: "/api/v3/snapshots".into(),
            create: json!({ "name": name("Snap") }),
            item: by_id("/api/v3/snapshots"),
            update_method: patch.clone(),
            update: json!({ "description": "kept" }),
            check: ("description", json!("kept")),
        },
        Trip {
            base: "/api/v3/filters".into(),
            create: json!({ "name": name("Filter") }),
            item: by_id("/api/v3/filters"),
            update_method: put.clone(),
            update: json!({ "auto_archive": 1 }),
            check: ("auto_archive", json!(1)),
        },
        Trip {
            base: "/api/v3/ai/object-classes".into(),
            create: json!({ "dataset_id": dataset_id, "class_name": name("cls"), "class_index": 7 }),
            item: by_id("/api/v3/ai/object-classes"),
            update_method: patch.clone(),
            update: json!({ "description": "seven" }),
            check: ("description", json!("seven")),
        },
        Trip {
            base: "/api/v3/encoder-templates".into(),
            create: json!({ "name": name("Enc"), "encoder": "libx264", "params": "crf=23" }),
            item: by_id("/api/v3/encoder-templates"),
            update_method: put.clone(),
            update: json!({ "description": "small" }),
            check: ("description", json!("small")),
        },
        Trip {
            base: "/api/v3/roles".into(),
            create: json!({ "name": name("Role") }),
            item: by_id("/api/v3/roles"),
            update_method: put.clone(),
            update: json!({ "description": "ops" }),
            check: ("description", json!("ops")),
        },
        Trip {
            base: "/api/v3/notifications".into(),
            create: json!({ "token": name("push"), "platform": "android", "user_id": UID }),
            item: by_id("/api/v3/notifications"),
            update_method: patch.clone(),
            update: json!({ "badge_count": 2 }),
            check: ("badge_count", json!(2)),
        },
        Trip {
            base: "/api/v3/monitors-permissions".into(),
            create: json!({ "monitor_id": mid, "user_id": UID, "permission": "View" }),
            item: by_id("/api/v3/monitors-permissions"),
            update_method: patch.clone(),
            update: json!({ "permission": "Edit" }),
            check: ("permission", json!("Edit")),
        },
        Trip {
            base: "/api/v3/groups-permissions".into(),
            create: json!({ "group_id": group.id, "user_id": UID, "permission": "View" }),
            item: by_id("/api/v3/groups-permissions"),
            update_method: patch.clone(),
            update: json!({ "permission": "Edit" }),
            check: ("permission", json!("Edit")),
        },
        Trip {
            base: "/api/v3/control_presets".into(),
            create: json!({ "monitor_id": mid, "preset": 4, "label": "Gate" }),
            item: Box::new(move |_| format!("/api/v3/control_presets/{mid}/4")),
            update_method: patch.clone(),
            update: json!({ "label": "Door" }),
            check: ("label", json!("Door")),
        },
        Trip {
            base: "/api/v3/triggers_x10".into(),
            create: json!({ "monitor_id": mid }),
            item: Box::new(move |_| format!("/api/v3/triggers_x10/{mid}")),
            update_method: patch.clone(),
            update: json!({ "activation": "A1" }),
            check: ("activation", json!("A1")),
        },
        Trip {
            base: "/api/v3/event-data".into(),
            create: json!({ "event_id": eid, "monitor_id": mid, "timestamp": now, "data": "{}" }),
            item: by_id("/api/v3/event-data"),
            update_method: patch.clone(),
            update: json!({ "data": "{\"k\":1}" }),
            check: ("data", json!("{\"k\":1}")),
        },
        Trip {
            base: "/api/v3/events".into(),
            create: json!({ "monitor_id": mid, "storage_id": 0, "name": name("Ev"),
                            "width": 320, "height": 240, "length": "1.00",
                            "orientation": "ROTATE_0" }),
            item: by_id("/api/v3/events"),
            update_method: patch.clone(),
            update: json!({ "notes": "checked" }),
            check: ("notes", json!("checked")),
        },
        Trip {
            base: "/api/v3/frames".into(),
            create: json!({ "event_id": eid, "frame_id": 1, "type": "Normal",
                            "time_stamp": now, "delta": "0.10", "score": 0 }),
            item: Box::new(move |_| format!("/api/v3/frames/{eid}/1")),
            update_method: patch.clone(),
            update: json!({ "score": 42 }),
            check: ("score", json!(42)),
        },
    ];

    let mut failures = Vec::new();
    for trip in trips {
        let base = trip.base.clone();
        if let Err(e) = run(&app, &token, trip).await {
            failures.push(format!("{base}: {e}"));
        }
    }
    let _ = app
        .delete(&format!("/api/v3/ai/datasets/{dataset_id}"), &token)
        .await;
    assert!(
        failures.is_empty(),
        "{} failed:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

#[tokio::test]
#[ignore = "requires the test database (APP_PROFILE=test-db)"]
async fn the_remaining_lists_answer() {
    let app = TestApp::spawn().await;
    let token = superuser_token();
    let mut failures = Vec::new();
    for path in [
        "/api/v3/jobs",
        "/api/v3/server-stats",
        "/api/v3/menu-items",
        "/api/v3/ai/datasets",
        "/api/v3/ai/models",
    ] {
        let r = app.get(path, &token).await;
        if r.status() != StatusCode::OK {
            failures.push(format!("{path}: {} {}", r.status(), r.text()));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
