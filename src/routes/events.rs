use axum::{
    middleware,
    routing::{delete, get},
    Router,
};

use crate::util::middleware::auth_middleware;
use crate::{handlers, server::state::AppState};

/// Create events router using JWT middleware
pub fn add_event_routes(router: Router<AppState>, state: AppState) -> Router<AppState> {
    router.nest(
        "/api/v3/events",
        routes(state).layer(middleware::from_fn(auth_middleware)),
    )
}

pub fn routes(state: AppState) -> Router<AppState> {
    Router::new()
        // Use fully qualified handler paths
        .route(
            "/",
            get(handlers::events::list_events).post(handlers::events::create_event),
        )
        .route(
            "/{id}",
            get(handlers::events::get_event).patch(handlers::events::update_event),
        )
        // Deleting removes the files too, and only the server that holds
        // them can. Reading or editing the row works on any server.
        .route(
            "/{id}",
            delete(handlers::events::delete_event).route_layer(middleware::from_fn_with_state(
                state,
                crate::cluster::forward::forward_to_storage_owner,
            )),
        )
        // Live events and alarm states (Server-Sent Events)
        .route("/stream", get(handlers::event_stream::stream_events))
        // Event counts grouped by hour
        .route("/counts/{hours}", get(handlers::events::get_event_counts))
        // Event counts grouped by monitor (for console view)
        .route(
            "/counts-by-monitor/{hours}",
            get(handlers::events::get_event_counts_by_monitor),
        )
}
