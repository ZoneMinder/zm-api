//! Tests for per-resource RBAC enforcement (`zm_api::util::authz`).
//!
//! RBAC rejects requests before any handler runs, so these need no database.

mod common;

use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use sea_orm::{DatabaseBackend, MockDatabase};
use tower::util::ServiceExt;
use zm_api::service::token::generate_tokens;
use zm_api::util::authz::{Level, UserPermissions};

fn router() -> axum::Router {
    let db = MockDatabase::new(DatabaseBackend::MySql).into_connection();
    let state = zm_api::server::state::AppState::for_test_with_db(db);
    common::schema::checked(zm_api::routes::create_router_app(state))
}

fn token(perms: UserPermissions) -> String {
    generate_tokens("rbac-tester".to_string(), 1, perms)
        .expect("token")
        .access_token
}

async fn status(method: &str, uri: &str, bearer: Option<&str>) -> StatusCode {
    let mut builder = Request::builder().method(method).uri(uri);
    if let Some(t) = bearer {
        builder = builder.header("Authorization", format!("Bearer {t}"));
    }
    router()
        .oneshot(builder.body(Body::empty()).unwrap())
        .await
        .unwrap()
        .status()
}

#[tokio::test]
async fn no_token_is_unauthorized() {
    assert_eq!(
        status("GET", "/api/v3/monitors", None).await,
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn missing_feature_permission_is_forbidden() {
    // A user with no `Monitors` permission cannot read monitors.
    let t = token(UserPermissions::default());
    assert_eq!(
        status("GET", "/api/v3/monitors", Some(&t)).await,
        StatusCode::FORBIDDEN
    );
}

#[tokio::test]
async fn view_permission_cannot_perform_writes() {
    // `View` is enough to read but not to mutate — writes require `Edit`.
    let perms = UserPermissions {
        monitors: Level::View,
        ..UserPermissions::default()
    };
    let t = token(perms);
    assert_eq!(
        status("POST", "/api/v3/monitors", Some(&t)).await,
        StatusCode::FORBIDDEN
    );
}

#[tokio::test]
async fn sufficient_permission_passes_rbac() {
    // A superuser token clears RBAC; the request then reaches the handler
    // (which may itself succeed or fail), so it is neither 401 nor 403.
    let t = token(UserPermissions::superuser());
    let got = status("GET", "/api/v3/monitors", Some(&t)).await;
    assert_ne!(got, StatusCode::UNAUTHORIZED);
    assert_ne!(got, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn wrong_feature_permission_does_not_grant_access() {
    // Holding `System` permission does not grant `Monitors` access.
    let perms = UserPermissions {
        system: Level::Edit,
        ..UserPermissions::default()
    };
    let t = token(perms);
    assert_eq!(
        status("GET", "/api/v3/monitors", Some(&t)).await,
        StatusCode::FORBIDDEN
    );
}

// ---------------------------------------------------------------------------
// Granting permissions to users is an administrative operation. The
// `/groups-permissions` and `/monitors-permissions` POST/PATCH/DELETE routes
// must require `System:Edit`, not the feature they manage — otherwise a user
// with `Groups:Edit` or `Monitors:Edit` could grant themselves or others
// elevated row-level access.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn groups_edit_alone_cannot_grant_group_permissions() {
    let perms = UserPermissions {
        groups: Level::Edit,
        ..UserPermissions::default()
    };
    let t = token(perms);
    assert_eq!(
        status("POST", "/api/v3/groups-permissions", Some(&t)).await,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        status("DELETE", "/api/v3/groups-permissions/1", Some(&t)).await,
        StatusCode::FORBIDDEN
    );
}

#[tokio::test]
async fn monitors_edit_alone_cannot_grant_monitor_permissions() {
    let perms = UserPermissions {
        monitors: Level::Edit,
        ..UserPermissions::default()
    };
    let t = token(perms);
    assert_eq!(
        status("POST", "/api/v3/monitors-permissions", Some(&t)).await,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        status("DELETE", "/api/v3/monitors-permissions/1", Some(&t)).await,
        StatusCode::FORBIDDEN
    );
}

#[tokio::test]
async fn state_change_requires_system_edit() {
    // POST /api/v3/server/control/{action} invokes `systemctl restart` on
    // zoneminder. Any token-holder must not be able to trigger it; require
    // System:Edit even though Monitors:Edit is the "biggest" feature most
    // operators carry. Both the canonical path and the deprecated
    // `/states/change` alias must enforce it.
    let perms = UserPermissions {
        monitors: Level::Edit,
        ..UserPermissions::default()
    };
    let t = token(perms);
    assert_eq!(
        status("POST", "/api/v3/server/control/restart", Some(&t)).await,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        status("POST", "/api/v3/states/change/restart", Some(&t)).await,
        StatusCode::FORBIDDEN
    );
}

#[tokio::test]
async fn system_edit_clears_rbac_for_permission_crud() {
    let perms = UserPermissions {
        system: Level::Edit,
        ..UserPermissions::default()
    };
    let t = token(perms);
    // Both routes should clear RBAC for a System:Edit caller — what happens
    // beyond that (bad JSON body, etc.) is a handler concern, not an RBAC one.
    let got = status("POST", "/api/v3/groups-permissions", Some(&t)).await;
    assert_ne!(got, StatusCode::FORBIDDEN);
    assert_ne!(got, StatusCode::UNAUTHORIZED);
    let got = status("POST", "/api/v3/monitors-permissions", Some(&t)).await;
    assert_ne!(got, StatusCode::FORBIDDEN);
    assert_ne!(got, StatusCode::UNAUTHORIZED);
}

// ---------------------------------------------------------------------------
// Feature-gating matrix (GH #56).
//
// Every router in `routes/mod.rs` is wrapped in `protect(..., Feature::X)`,
// which derives the required level from the HTTP method: reads need `View`,
// writes need `Edit`. The mechanism is shared, so these pass today — that is
// the point. They catch the case where someone adds a router to `mod.rs` and
// forgets to wrap it, or wires the wrong `Feature`, which currently ships
// silently.
//
// `rbac.rs` previously covered only Monitors, System, and Groups. The five
// added here are the ones a client actually surfaces: Stream gates live video,
// Events gates the event list and playback, Control gates PTZ.
// ---------------------------------------------------------------------------

/// Build a permission set with exactly one feature raised to `level`.
fn only(feature: &str, level: Level) -> UserPermissions {
    let mut p = UserPermissions::default();
    match feature {
        "stream" => p.stream = level,
        "events" => p.events = level,
        "control" => p.control = level,
        "devices" => p.devices = level,
        "snapshots" => p.snapshots = level,
        "monitors" => p.monitors = level,
        "groups" => p.groups = level,
        "system" => p.system = level,
        other => panic!("unknown feature {other}"),
    }
    p
}

/// RBAC runs before any handler, so "allowed" means "not 401/403" — the
/// request goes on to hit a mock database and may fail there, which is not an
/// authorization concern.
fn assert_allowed(got: StatusCode, what: &str) {
    assert_ne!(got, StatusCode::UNAUTHORIZED, "{what} should clear RBAC");
    assert_ne!(got, StatusCode::FORBIDDEN, "{what} should clear RBAC");
}

/// (feature, read route, write method, write route)
const READ_WRITE_FEATURES: &[(&str, &str, &str, &str)] = &[
    ("events", "/api/v3/events", "POST", "/api/v3/events"),
    ("control", "/api/v3/controls", "POST", "/api/v3/controls"),
    ("devices", "/api/v3/devices", "POST", "/api/v3/devices"),
    (
        "snapshots",
        "/api/v3/snapshots",
        "POST",
        "/api/v3/snapshots",
    ),
];

#[tokio::test]
async fn no_permission_is_forbidden_for_every_feature() {
    for (feature, read, write_method, write) in READ_WRITE_FEATURES {
        let t = token(UserPermissions::default());
        assert_eq!(
            status("GET", read, Some(&t)).await,
            StatusCode::FORBIDDEN,
            "{feature}: GET {read} must be forbidden at Level::None"
        );
        assert_eq!(
            status(write_method, write, Some(&t)).await,
            StatusCode::FORBIDDEN,
            "{feature}: {write_method} {write} must be forbidden at Level::None"
        );
    }
}

#[tokio::test]
async fn view_reads_but_cannot_write_for_every_feature() {
    for (feature, read, write_method, write) in READ_WRITE_FEATURES {
        let t = token(only(feature, Level::View));
        assert_allowed(
            status("GET", read, Some(&t)).await,
            &format!("{feature}: GET {read} at Level::View"),
        );
        assert_eq!(
            status(write_method, write, Some(&t)).await,
            StatusCode::FORBIDDEN,
            "{feature}: {write_method} {write} must need Edit, not View"
        );
    }
}

#[tokio::test]
async fn edit_permits_both_reads_and_writes_for_every_feature() {
    for (feature, read, write_method, write) in READ_WRITE_FEATURES {
        let t = token(only(feature, Level::Edit));
        assert_allowed(
            status("GET", read, Some(&t)).await,
            &format!("{feature}: GET {read} at Level::Edit"),
        );
        assert_allowed(
            status(write_method, write, Some(&t)).await,
            &format!("{feature}: {write_method} {write} at Level::Edit"),
        );
    }
}

#[tokio::test]
async fn one_feature_does_not_unlock_another() {
    // Each feature's grant must be inert against every other feature's routes.
    for (holder, _, _, _) in READ_WRITE_FEATURES {
        let t = token(only(holder, Level::Edit));
        for (target, read, _, _) in READ_WRITE_FEATURES {
            if holder == target {
                continue;
            }
            assert_eq!(
                status("GET", read, Some(&t)).await,
                StatusCode::FORBIDDEN,
                "{holder}:Edit must not grant access to {target} route {read}"
            );
        }
    }
}

#[tokio::test]
async fn stream_gates_live_video() {
    // `Stream` has no Edit tier in ZoneMinder (`UserPermissions::superuser`
    // sets it to View), so the matrix here is None -> denied, View -> allowed.
    let none = token(UserPermissions::default());
    assert_eq!(
        status("GET", "/api/v3/live/sources", Some(&none)).await,
        StatusCode::FORBIDDEN,
        "live sources must require Stream"
    );
    assert_eq!(
        status("GET", "/api/v3/live/1/stats", Some(&none)).await,
        StatusCode::FORBIDDEN,
        "live stats must require Stream"
    );

    let view = token(only("stream", Level::View));
    assert_allowed(
        status("GET", "/api/v3/live/sources", Some(&view)).await,
        "Stream:View on /live/sources",
    );

    // Holding every *other* feature must not open live video.
    let mut perms = UserPermissions::superuser();
    perms.stream = Level::None;
    let t = token(perms);
    assert_eq!(
        status("GET", "/api/v3/live/sources", Some(&t)).await,
        StatusCode::FORBIDDEN,
        "everything-but-Stream must not reach live video"
    );
}

#[tokio::test]
async fn control_gates_ptz() {
    // PTZ wraps a row-level monitor guard *inside* the feature check, so a
    // caller without Control:View must be refused before any DB query runs.
    let none = token(UserPermissions::default());
    assert_eq!(
        status("GET", "/api/v3/ptz/monitors/1/capabilities", Some(&none)).await,
        StatusCode::FORBIDDEN,
        "PTZ must require Control"
    );

    let mut perms = UserPermissions::superuser();
    perms.control = Level::None;
    let t = token(perms);
    assert_eq!(
        status("GET", "/api/v3/ptz/monitors/1/capabilities", Some(&t)).await,
        StatusCode::FORBIDDEN,
        "everything-but-Control must not reach PTZ"
    );
}

#[tokio::test]
async fn monitors_edit_cannot_add_monitors() {
    // ZoneMinder: `Edit` changes existing monitors; adding one needs `Create`.
    let perms = UserPermissions {
        monitors: Level::Edit,
        ..UserPermissions::default()
    };
    let t = token(perms);
    let req = Request::builder()
        .method("POST")
        .uri("/api/v3/monitors")
        .header("Authorization", format!("Bearer {t}"))
        .header("Content-Type", "application/json")
        .body(Body::from(r#"{"name":"x"}"#))
        .unwrap();
    let resp = router().oneshot(req).await.unwrap();
    let got = resp.status();
    let body = axum::body::to_bytes(resp.into_body(), 1 << 16)
        .await
        .unwrap();
    assert_eq!(
        got,
        StatusCode::FORBIDDEN,
        "{}",
        String::from_utf8_lossy(&body)
    );
}
