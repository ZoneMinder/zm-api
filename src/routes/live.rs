//! Live streaming routes
//!
//! Provides unified routes for live streaming via HLS and WebRTC.

use axum::{
    routing::{delete, get, post},
    Router,
};

use crate::handlers::live;
use crate::server::state::AppState;
use crate::util::middleware::{auth_middleware, media_auth_middleware};

/// Create live streaming routes
pub fn routes() -> Router<AppState> {
    Router::new()
        // Session control (requires auth)
        .route("/start", post(live::start_live_stream))
        .route("/stop", delete(live::stop_live_stream))
        .route("/stats", get(live::get_live_stats))
        // HLS endpoints (no auth for media delivery)
        .route("/hls/master.m3u8", get(live::get_live_master_playlist))
        .route("/hls/live.m3u8", get(live::get_live_media_playlist))
        .route("/hls/init.mp4", get(live::get_live_init_segment))
        // Signed playlist link for header-less players. Header token only.
        .route(
            "/hls/link",
            post(crate::handlers::media_links::create_live_link)
                .route_layer(axum::middleware::from_fn(auth_middleware)),
        )
        // Signed RTSP link (zm-api's RTSP server). Header token only.
        .route(
            "/rtsp/link",
            post(crate::handlers::media_links::create_rtsp_link)
                .route_layer(axum::middleware::from_fn(auth_middleware)),
        )
        .route("/hls/{segment}", get(live::get_live_segment))
        // WebRTC endpoints (WebSocket signaling)
        .route("/webrtc/ws", get(live::webrtc_websocket_handler))
        // WHEP (RFC 9725): the viewer sends the offer. Header token only.
        .route(
            "/webrtc/whep",
            post(crate::handlers::whep::whep_offer)
                .route_layer(axum::middleware::from_fn(auth_middleware)),
        )
        .route(
            "/webrtc/whep/{session_id}",
            axum::routing::patch(crate::handlers::whep::whep_trickle)
                .delete(crate::handlers::whep::whep_delete)
                .route_layer(axum::middleware::from_fn(auth_middleware)),
        )
}

/// Add live streaming routes to the router
pub fn add_live_routes(router: Router<AppState>, state: AppState) -> Router<AppState> {
    router
        // Per-monitor live streaming endpoints
        .nest("/api/v3/live/{monitor_id}", routes())
        // Global live streaming endpoints
        .route(
            "/api/v3/live/sessions",
            get(live::list_live_sessions)
                // `server_id=<n>|all`, as on the daemon routes. Inside the
                // login check, so nothing leaves this server unauthenticated.
                .route_layer(axum::middleware::from_fn_with_state(
                    state.clone(),
                    crate::cluster::system::target_server,
                ))
                .route_layer(axum::middleware::from_fn(auth_middleware)),
        )
        .route(
            "/api/v3/live/sources",
            get(live::get_live_sources)
                // `server_id=<n>|all`, as on the daemon routes. Inside the
                // login check, so nothing leaves this server unauthenticated.
                .route_layer(axum::middleware::from_fn_with_state(
                    state.clone(),
                    crate::cluster::system::target_server,
                ))
                .route_layer(axum::middleware::from_fn(auth_middleware)),
        )
        // Monitor snapshot (supports token query param for <img> tags)
        // `POST` takes a fresh snapshot through a zm-next worker. Header auth
        // only: it does work on the server, so no `?token=` fallback.
        .route(
            "/api/v3/monitors/{monitor_id}/snapshot",
            get(live::get_monitor_snapshot)
                .route_layer(axum::middleware::from_fn(media_auth_middleware))
                .merge(
                    post(live::take_monitor_snapshot)
                        .route_layer(axum::middleware::from_fn(auth_middleware)),
                ),
        )
        // Status push for EventSource clients, so `?token=` is accepted.
        .route(
            "/api/v3/monitors/{monitor_id}/events",
            get(live::stream_monitor_events)
                .route_layer(axum::middleware::from_fn(media_auth_middleware)),
        )
        .route(
            "/api/v3/monitors/{monitor_id}/describe",
            post(live::describe_monitor).route_layer(axum::middleware::from_fn(auth_middleware)),
        )
}
