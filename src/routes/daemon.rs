//! Routes for daemon controller API.

use axum::{
    middleware,
    routing::{get, post},
    Router,
};

use crate::handlers::daemon;
use crate::server::state::AppState;
use crate::util::middleware::auth_middleware;

/// Add daemon routes to a router.
///
/// These endpoints control ZoneMinder process and system lifecycle, so the
/// whole group is gated behind JWT auth via [`auth_middleware`].
pub fn add_daemon_routes(router: Router<AppState>, state: AppState) -> Router<AppState> {
    // Every route but the run state takes `?server_id=`; the state route
    // fans out by itself, because it changes the database first.
    let targeted = Router::new()
        // Daemon management
        .route("/api/v3/daemons", get(daemon::list_daemons))
        .route("/api/v3/daemons/{id}", get(daemon::get_daemon))
        .route("/api/v3/daemons/{id}/start", post(daemon::start_daemon))
        .route("/api/v3/daemons/{id}/stop", post(daemon::stop_daemon))
        .route("/api/v3/daemons/{id}/restart", post(daemon::restart_daemon))
        .route("/api/v3/daemons/{id}/reload", post(daemon::reload_daemon))
        // System management
        .route("/api/v3/system/status", get(daemon::get_system_status))
        .route("/api/v3/system/startup", post(daemon::system_startup))
        .route("/api/v3/system/shutdown", post(daemon::system_shutdown))
        .route("/api/v3/system/restart", post(daemon::system_restart))
        .route("/api/v3/system/logrot", post(daemon::system_logrot))
        .route_layer(middleware::from_fn_with_state(
            state,
            crate::cluster::system::target_server,
        ));
    let protected_routes = targeted
        .route("/api/v3/system/state", post(daemon::apply_state))
        .layer(middleware::from_fn(auth_middleware));

    router.merge(protected_routes)
}
