//! Integration tests for the logs API (GH #21): inverted-scale `min_level`,
//! `search`, and `DELETE` clear-logs.
//!
//! Requires the test database — run with:
//!   APP_PROFILE=test-db cargo test --test it_logs -- --include-ignored

mod common;

use axum::http::{Method, StatusCode};
use common::fixtures::RowGuard;
use common::harness::{superuser_token, TestApp};
use common::test_db::{get_test_db, test_run_id};
use rust_decimal::Decimal;
use sea_orm::{ActiveModelTrait, ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter, Set};
use zm_api::dto::response::logs::PaginatedLogsResponse;

/// `offset` is seconds past now. A current `TimeKey` keeps the rows clear of
/// it_maintenance's log prune, which deletes the oldest rows table-wide and can
/// run at the same moment (GH #134).
async fn insert_log(
    db: &DatabaseConnection,
    component: &str,
    level: i8,
    message: &str,
    offset: i64,
) {
    let t = zm_api::util::time::unix_now() + offset;
    zm_api::entity::logs::ActiveModel {
        time_key: Set(Decimal::new(t, 0)),
        component: Set(component.to_string()),
        server_id: Set(None),
        pid: Set(None),
        level: Set(level),
        code: Set("TST".to_string()),
        message: Set(message.to_string()),
        file: Set(None),
        line: Set(None),
        ..Default::default()
    }
    .insert(db)
    .await
    .expect("insert log");
}

/// Delete every log row for a component (test cleanup).
fn logs_guard(component: String) -> RowGuard {
    RowGuard::new(
        format!("Logs(component={component})"),
        move |db| async move {
            let _ = zm_api::entity::logs::Entity::delete_many()
                .filter(zm_api::entity::logs::Column::Component.eq(component))
                .exec(&db)
                .await;
        },
    )
}

#[tokio::test]
#[ignore = "requires the test database (APP_PROFILE=test-db)"]
async fn min_level_returns_that_severity_or_worse() {
    let app = TestApp::spawn().await;
    let token = superuser_token();
    // Unique component so this test only sees its own rows on a shared DB.
    let rid = test_run_id();
    let component = format!("mlog{}", &rid[rid.len().saturating_sub(12)..]);
    let _g = logs_guard(component.clone());

    // Info(0), Warning(-1), Error(-2), Fatal(-3).
    insert_log(&app.db, &component, 0, "info msg", 0).await;
    insert_log(&app.db, &component, -1, "warning msg", 1).await;
    insert_log(&app.db, &component, -2, "error msg alpha", 2).await;
    insert_log(&app.db, &component, -3, "fatal msg", 3).await;

    // min_level=error → only Error + Fatal (the inverted-scale fix; the old
    // code's >= would have returned Info+Warning+Error).
    let resp = app
        .get(
            &format!("/api/v3/logs?component={component}&min_level=error&page_size=100"),
            &token,
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK, "body: {}", resp.text());
    let body: PaginatedLogsResponse = resp.json();
    assert_eq!(body.total, 2, "error+fatal only");
    assert!(
        body.items.iter().all(|l| l.level <= -2),
        "every returned row is Error or worse: {:?}",
        body.items.iter().map(|l| l.level).collect::<Vec<_>>()
    );

    // search filters by message substring.
    let resp = app
        .get(
            &format!("/api/v3/logs?component={component}&search=alpha&page_size=100"),
            &token,
        )
        .await;
    let body: PaginatedLogsResponse = resp.json();
    assert_eq!(body.total, 1, "only the 'error msg alpha' row");
    assert!(body.items[0].message.contains("alpha"));
}

#[tokio::test]
#[ignore = "requires the test database (APP_PROFILE=test-db)"]
async fn clear_logs_deletes_matching_rows() {
    let app = TestApp::spawn().await;
    let token = superuser_token();
    let rid = test_run_id();
    let component = format!("clog{}", &rid[rid.len().saturating_sub(12)..]);
    let _g = logs_guard(component.clone());

    for (i, lvl) in [0i8, -1, -2, -3].into_iter().enumerate() {
        insert_log(&app.db, &component, lvl, "clear me", i as i64).await;
    }

    // DELETE scoped to this component clears all four.
    let del = app
        .request(
            Method::DELETE,
            &format!("/api/v3/logs?component={component}"),
        )
        .bearer(&token)
        .send()
        .await;
    assert_eq!(del.status(), StatusCode::OK, "body: {}", del.text());

    // A verification connection: nothing left for this component.
    let db = get_test_db().await.expect("db");
    let remaining = zm_api::entity::logs::Entity::find()
        .filter(zm_api::entity::logs::Column::Component.eq(component.as_str()))
        .all(&db)
        .await
        .unwrap();
    assert!(
        remaining.is_empty(),
        "clear should remove all matching rows"
    );
}

#[tokio::test]
#[ignore = "requires the test database (APP_PROFILE=test-db)"]
async fn zero_and_oversized_page_size_are_rejected_not_panics() {
    // LogQueryParams declared range(min = 1, max = 1000) but the handler never
    // called validate(), so the bounds were inert. page_size=0 reached
    // `total.div_ceil(0)` and SeaORM's `paginate(0)` — both panic on zero.
    let app = TestApp::spawn().await;
    let token = superuser_token();

    for qs in ["page_size=0", "page_size=100000", "page=0"] {
        let resp = app.get(&format!("/api/v3/logs?{qs}"), &token).await;
        assert_eq!(
            resp.status(),
            StatusCode::BAD_REQUEST,
            "{qs} must be a 400, not a panic or an unbounded query; body: {}",
            resp.text()
        );
    }

    // A value inside the declared range still works.
    let resp = app.get("/api/v3/logs?page_size=10&page=1", &token).await;
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "an in-range page_size must still succeed; body: {}",
        resp.text()
    );
}

/// A component name unique to one test, so it only ever sees its own rows.
fn unique_component(prefix: &str) -> String {
    let rid = test_run_id();
    format!("{prefix}{}", &rid[rid.len().saturating_sub(12)..])
}

/// GH #142: "only warnings and audit rows" is a list of exact levels, which a
/// `min_level` threshold can't express.
#[tokio::test]
#[ignore = "requires the test database (APP_PROFILE=test-db)"]
async fn level_takes_a_list_of_exact_levels() {
    let app = TestApp::spawn().await;
    let token = superuser_token();
    let component = unique_component("llog");
    let _g = logs_guard(component.clone());

    for (i, lvl) in [0i8, -1, -2, -3, -5].into_iter().enumerate() {
        insert_log(&app.db, &component, lvl, "lvl", i as i64).await;
    }

    let levels = |resp: &common::harness::TestResponse| {
        let body: PaginatedLogsResponse = resp.json();
        let mut l: Vec<i8> = body.items.iter().map(|r| r.level).collect();
        l.sort_unstable();
        l
    };

    let resp = app
        .get(
            &format!("/api/v3/logs?component={component}&level=-1,-5"),
            &token,
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK, "body: {}", resp.text());
    assert_eq!(levels(&resp), vec![-5, -1]);

    // A single value still works, as it did before the list.
    let resp = app
        .get(
            &format!("/api/v3/logs?component={component}&level=-2"),
            &token,
        )
        .await;
    assert_eq!(levels(&resp), vec![-2]);

    let resp = app
        .get(
            &format!("/api/v3/logs?component={component}&level=-1,x"),
            &token,
        )
        .await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "{}", resp.text());
}

#[tokio::test]
#[ignore = "requires the test database (APP_PROFILE=test-db)"]
async fn sort_by_orders_on_the_named_column() {
    let app = TestApp::spawn().await;
    let token = superuser_token();
    let component = unique_component("slog");
    let _g = logs_guard(component.clone());

    // Time order is c, a, b; message order is a, b, c.
    insert_log(&app.db, &component, 0, "c", 0).await;
    insert_log(&app.db, &component, -2, "a", 1).await;
    insert_log(&app.db, &component, -1, "b", 2).await;

    let messages = |resp: &common::harness::TestResponse| {
        let body: PaginatedLogsResponse = resp.json();
        body.items
            .iter()
            .map(|r| r.message.clone())
            .collect::<Vec<_>>()
    };

    let resp = app
        .get(
            &format!("/api/v3/logs?component={component}&sort_by=message&sort=asc"),
            &token,
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK, "body: {}", resp.text());
    assert_eq!(messages(&resp), ["a", "b", "c"]);

    let resp = app
        .get(
            &format!("/api/v3/logs?component={component}&sort_by=level&sort=asc"),
            &token,
        )
        .await;
    assert_eq!(messages(&resp), ["a", "b", "c"], "levels -2, -1, 0");

    // Without sort_by the order is by time, newest first, as before.
    let resp = app
        .get(&format!("/api/v3/logs?component={component}"), &token)
        .await;
    assert_eq!(messages(&resp), ["b", "a", "c"]);
}

#[tokio::test]
#[ignore = "requires the test database (APP_PROFILE=test-db)"]
async fn components_lists_each_component_once() {
    let app = TestApp::spawn().await;
    let token = superuser_token();
    let component = unique_component("dlog");
    let _g = logs_guard(component.clone());

    insert_log(&app.db, &component, 0, "one", 0).await;
    insert_log(&app.db, &component, 0, "two", 1).await;

    let resp = app.get("/api/v3/logs/components", &token).await;
    assert_eq!(resp.status(), StatusCode::OK, "body: {}", resp.text());
    let names: Vec<String> = resp.json();
    assert_eq!(names.iter().filter(|n| **n == component).count(), 1);
    let mut sorted = names.clone();
    sorted.sort();
    assert_eq!(names, sorted, "components come back sorted");
}

/// Legacy's log page deletes the rows the operator ticked.
#[tokio::test]
#[ignore = "requires the test database (APP_PROFILE=test-db)"]
async fn clear_logs_by_ids_deletes_only_those_rows() {
    let app = TestApp::spawn().await;
    let token = superuser_token();
    let component = unique_component("ilog");
    let _g = logs_guard(component.clone());

    for i in 0..3 {
        insert_log(&app.db, &component, 0, &format!("row{i}"), i).await;
    }
    let resp = app
        .get(
            &format!("/api/v3/logs?component={component}&sort=asc"),
            &token,
        )
        .await;
    let body: PaginatedLogsResponse = resp.json();
    let ids: Vec<u32> = body.items.iter().map(|r| r.id).collect();
    assert_eq!(ids.len(), 3);

    let resp = app
        .delete(&format!("/api/v3/logs?ids={},{}", ids[0], ids[2]), &token)
        .await;
    assert_eq!(resp.status(), StatusCode::OK, "body: {}", resp.text());

    let resp = app
        .get(&format!("/api/v3/logs?component={component}"), &token)
        .await;
    let body: PaginatedLogsResponse = resp.json();
    assert_eq!(
        body.items.iter().map(|r| r.id).collect::<Vec<_>>(),
        vec![ids[1]]
    );

    // An `ids` that parses to nothing must not fall through to "clear all".
    for bad in ["ids=", "ids=abc", "ids=,"] {
        let resp = app.delete(&format!("/api/v3/logs?{bad}"), &token).await;
        assert_eq!(
            resp.status(),
            StatusCode::BAD_REQUEST,
            "{bad}: {}",
            resp.text()
        );
    }
}

#[tokio::test]
#[ignore = "requires the test database (APP_PROFILE=test-db)"]
async fn unfiltered_total_is_opt_in() {
    let app = TestApp::spawn().await;
    let token = superuser_token();
    let component = unique_component("ulog");
    let _g = logs_guard(component.clone());
    insert_log(&app.db, &component, 0, "only", 0).await;

    let resp = app
        .get(&format!("/api/v3/logs?component={component}"), &token)
        .await;
    let body: serde_json::Value = resp.json();
    assert!(body.get("total_unfiltered").is_none_or(|v| v.is_null()));

    let resp = app
        .get(
            &format!("/api/v3/logs?component={component}&unfiltered_total=true"),
            &token,
        )
        .await;
    let body: serde_json::Value = resp.json();
    assert_eq!(body["total"], 1);
    assert!(body["total_unfiltered"].as_u64().expect("a count") >= 1);
}

/// `component` takes a comma list on GET and DELETE (#186).
#[tokio::test]
#[ignore = "requires the test database (APP_PROFILE=test-db)"]
async fn component_takes_a_comma_list() {
    let app = TestApp::spawn().await;
    let token = superuser_token();
    let rid = test_run_id();
    let tag = &rid[rid.len().saturating_sub(10)..];
    let (a, b, c) = (
        format!("cla{tag}"),
        format!("clb{tag}"),
        format!("clc{tag}"),
    );
    let _ga = logs_guard(a.clone());
    let _gb = logs_guard(b.clone());
    let _gc = logs_guard(c.clone());
    for comp in [&a, &b, &c] {
        insert_log(&app.db, comp, 0, "list me", 0).await;
    }

    let list: PaginatedLogsResponse = app
        .get(
            &format!("/api/v3/logs?component={a},{b}&page_size=100"),
            &token,
        )
        .await
        .json();
    let mut comps: Vec<String> = list.items.iter().map(|l| l.component.clone()).collect();
    comps.sort();
    assert_eq!(comps, vec![a.clone(), b.clone()]);

    let del = app
        .request(Method::DELETE, &format!("/api/v3/logs?component={a},{b}"))
        .bearer(&token)
        .send()
        .await;
    assert_eq!(del.status(), StatusCode::OK, "{}", del.text());
    let db = get_test_db().await.expect("db");
    let left: Vec<String> = zm_api::entity::logs::Entity::find()
        .filter(zm_api::entity::logs::Column::Component.is_in([a.as_str(), b.as_str(), c.as_str()]))
        .all(&db)
        .await
        .unwrap()
        .into_iter()
        .map(|l| l.component)
        .collect();
    assert_eq!(
        left,
        vec![c.clone()],
        "only the unselected component is left"
    );
}
