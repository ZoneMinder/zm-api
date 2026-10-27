use crate::handlers::zones;
use crate::server::state::AppState;
use crate::util::middleware::auth_middleware;
use axum::{middleware, routing::get, Router};

/// Zones listed and added under their monitor: `{id}` is the monitor's.
pub fn add_monitor_zone_routes(router: Router<AppState>) -> Router<AppState> {
    let protected = Router::new()
        .route(
            "/api/v3/monitors/{id}/zones",
            get(zones::list_by_monitor).post(zones::create),
        )
        .layer(middleware::from_fn(auth_middleware));

    router.merge(protected)
}

/// One zone by its own id.
pub fn add_zone_routes(router: Router<AppState>) -> Router<AppState> {
    let protected = Router::new()
        .route(
            "/api/v3/zones/{id}",
            get(zones::get).put(zones::update).delete(zones::delete),
        )
        .layer(middleware::from_fn(auth_middleware));

    router.merge(protected)
}
