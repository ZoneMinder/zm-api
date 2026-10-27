//! Every route must be behind an authorization gate (GH #56).
//!
//! The per-feature matrix in `rbac.rs` proves the mechanism works on the routes
//! it names. This proves there are no routes it *doesn't* name: it walks the
//! served OpenAPI document — the same one the router builds — and asserts that
//! a token carrying no permissions at all is refused everywhere.
//!
//! That catches the failure the matrix cannot: someone adds a router to
//! `routes/mod.rs` and forgets to wrap it in `protect`, or wires it to the
//! wrong `Feature`. Today that ships silently.
//!
//! Routes that are deliberately reachable without a feature grant are listed in
//! `UNGATED` below, which doubles as the documentation of that decision. Adding
//! to that list should take a reviewer noticing.

mod common;

use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use sea_orm::{DatabaseBackend, MockDatabase};
use tower::util::ServiceExt;
use utoipa::OpenApi;
use zm_api::service::token::generate_tokens;
use zm_api::util::authz::UserPermissions;

/// Reachable with no token at all. Every entry here is a deliberate decision to
/// serve an anonymous caller, so the list should stay very short.
const PUBLIC: &[(&str, &str)] = &[
    ("/api/v3/server/health_check", "reverse-proxy health check"),
    ("/api/v3/host/getVersion", "version probe"),
    ("/api/v3/auth/login", "issues tokens"),
    ("/api/v3/auth/refresh", "issues tokens"),
];

/// Requires a valid token but deliberately carries no feature gate, so a
/// permission-poor account can still reach it.
///
/// `/me` is the load-bearing one: it must stay readable at `System: None` so a
/// client can discover its own permissions. Gating it would reinstate the
/// CakePHP flaw this API exists to fix (GH #56).
const AUTHENTICATED_UNGATED: &[(&str, &str)] = &[
    ("/api/v3/me", "self-service: must work at System:None"),
    ("/api/v3/me/password", "self-service password change"),
    ("/api/v3/auth/logout", "revokes the caller's own tokens"),
    (
        "/api/v3/notifications",
        "the caller's own push tokens, as ZoneMinder's API allows any user",
    ),
    (
        "/api/v3/notifications/{id}",
        "the caller's own push tokens; another user's row is a 404",
    ),
    (
        "/api/v3/system/menu",
        "the navbar's entries: every user's UI renders it, including a \
         System:None operator",
    ),
    (
        "/api/v3/system/locale",
        "timezone and date formats: every client needs these to render \
         timestamps, including a System:None operator",
    ),
];

/// The auth-endpoint rate limiter keys on the peer address, so requests to
/// `/auth/*` and `/me*` need a `ConnectInfo` extension; production supplies it
/// via `into_make_service_with_connect_info`. Without it those routes 500
/// before authentication is even reached.
fn ci() -> axum::extract::ConnectInfo<std::net::SocketAddr> {
    axum::extract::ConnectInfo(std::net::SocketAddr::from(([127, 0, 0, 1], 50000)))
}

/// The mock answers one query with no rows. On a route about one monitor
/// the gate looks up the caller's permission rows first, since a row can
/// grant what the base level doesn't (#244); finding none, the base level
/// (None) refuses.
fn router() -> axum::Router {
    let db = MockDatabase::new(DatabaseBackend::MySql)
        .append_query_results([Vec::<sea_orm::MockRow>::new()])
        .into_connection();
    let state = zm_api::server::state::AppState::for_test_with_db(db);
    common::schema::checked(zm_api::routes::create_router_app(state))
}

/// Fill path parameters so the request routes to the handler. The value never
/// matters — authorization runs as a layer, before anything reads it.
fn concrete_path(template: &str) -> String {
    let mut out = String::with_capacity(template.len());
    let mut depth = 0usize;
    for ch in template.chars() {
        match ch {
            '{' => {
                if depth == 0 {
                    out.push('1');
                }
                depth += 1;
            }
            '}' => depth = depth.saturating_sub(1),
            c if depth == 0 => out.push(c),
            _ => {}
        }
    }
    out
}

#[tokio::test]
async fn every_route_refuses_a_token_with_no_permissions() {
    let spec = zm_api::handlers::openapi::ApiDoc::openapi();
    let token = generate_tokens("nobody".to_string(), 1, UserPermissions::default())
        .expect("token")
        .access_token;

    let mut checked = 0usize;
    let mut leaks: Vec<String> = Vec::new();

    for (template, item) in spec.paths.paths.iter() {
        if PUBLIC
            .iter()
            .chain(AUTHENTICATED_UNGATED)
            .any(|(p, _)| p == template)
        {
            continue;
        }
        let path = concrete_path(template);

        let verbs = [
            ("GET", item.get.is_some()),
            ("POST", item.post.is_some()),
            ("PUT", item.put.is_some()),
            ("PATCH", item.patch.is_some()),
            ("DELETE", item.delete.is_some()),
        ];

        for (verb, _) in verbs.into_iter().filter(|(_, present)| *present) {
            let resp = router()
                .oneshot(
                    Request::builder()
                        .method(verb)
                        .uri(&path)
                        .header("Authorization", format!("Bearer {token}"))
                        .extension(ci())
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();

            checked += 1;
            let status = resp.status();
            // 401/403 are the correct refusals. 404/405 mean the probe didn't
            // reach a real route, which is a fault in this test rather than a
            // missing gate, so they are tolerated but not counted as proof.
            let refused = matches!(status, StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN)
                || matches!(
                    status,
                    StatusCode::NOT_FOUND | StatusCode::METHOD_NOT_ALLOWED
                );
            if !refused {
                leaks.push(format!("{verb} {template} -> {status}"));
            }
        }
    }

    assert!(
        checked > 100,
        "expected to probe the whole API surface, only reached {checked} operations \
         — the spec walk is broken, not the authorization"
    );
    assert!(
        leaks.is_empty(),
        "{} route(s) served a request from a token with no permissions. Each is \
         either missing `protect(...)` in routes/mod.rs or belongs in this \
         test's PUBLIC / AUTHENTICATED_UNGATED list with a reason:\n  {}",
        leaks.len(),
        leaks.join("\n  ")
    );
}

#[tokio::test]
async fn the_exemption_lists_only_name_routes_that_exist() {
    // A stale entry would silently excuse a route that had been renamed,
    // leaving its replacement unchecked.
    let spec = zm_api::handlers::openapi::ApiDoc::openapi();
    for (path, reason) in PUBLIC.iter().chain(AUTHENTICATED_UNGATED) {
        assert!(
            spec.paths.paths.contains_key(*path),
            "exemption list names {path} ({reason}) but no such path is served — \
             remove the entry or fix the path"
        );
    }
}

#[tokio::test]
async fn ungated_routes_still_require_a_token() {
    // Dropping the feature gate must not mean dropping authentication. Without
    // this, moving a route into AUTHENTICATED_UNGATED could quietly make it
    // anonymous.
    for (path, reason) in AUTHENTICATED_UNGATED {
        let resp = router()
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri(concrete_path(path))
                    .extension(ci())
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::UNAUTHORIZED,
            "{path} ({reason}) is exempt from feature gating but must still \
             reject an anonymous caller"
        );
    }
}
