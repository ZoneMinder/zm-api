//! Event playback routes
//!
//! Routes for playing back recorded events via HLS or direct video access.
//!
//! These routes use `media_auth_middleware` which accepts authentication via:
//! - `Authorization: Bearer <token>` header (standard)
//! - `?token=<token>` query parameter (for HTML5 media elements)

use axum::{
    routing::{get, post},
    Router,
};

/// `POST /api/v3/events/{id}/stream/link`: mint a signed playlist link.
///
/// Kept out of the Events-gated playback router, whose method-derived gate
/// would demand Events:Edit for a POST; issuing a read-only link needs only
/// View (`protect_at` in routes/mod.rs). Header token only.
pub fn add_event_link_route(router: Router<AppState>) -> Router<AppState> {
    router.route(
        "/api/v3/events/{id}/stream/link",
        post(crate::handlers::media_links::create_event_link).route_layer(
            axum::middleware::from_fn(crate::util::middleware::auth_middleware),
        ),
    )
}

/// `POST /api/v3/events/export` (#30): a zip, so it sits with the streaming
/// routes, outside the compression layer. Events-gated at View by the caller.
pub fn add_event_export_route(router: Router<AppState>) -> Router<AppState> {
    router.route(
        "/api/v3/events/export",
        post(crate::handlers::event_export::export_events).route_layer(axum::middleware::from_fn(
            crate::util::middleware::auth_middleware,
        )),
    )
}

use crate::handlers::{events_playback, frame_images, synopsis};
use crate::server::state::AppState;
use crate::util::middleware::media_auth_middleware;

/// Add event playback routes to the router.
///
/// Every route that names an event reads its files, so each is relayed to
/// the server that holds them (multi-server, see `cluster::forward`). The
/// relay sits inside the media authentication: this server checks who is
/// asking, and that they may see the monitor, before anything is sent on. The
/// range montage names no event and spans many, so it stays on this server.
pub fn add_events_playback_routes(router: Router<AppState>, state: AppState) -> Router<AppState> {
    let to_storage_owner = || {
        axum::middleware::from_fn_with_state(
            state.clone(),
            crate::cluster::forward::forward_to_storage_owner,
        )
    };
    router
        // HLS streaming endpoints
        .route(
            "/api/v3/events/{id}/stream/playlist.m3u8",
            get(events_playback::get_event_playlist)
                .route_layer(to_storage_owner())
                .route_layer(axum::middleware::from_fn(media_auth_middleware)),
        )
        .route(
            "/api/v3/events/{id}/stream/video.mp4",
            get(events_playback::get_event_stream_video)
                .route_layer(to_storage_owner())
                .route_layer(axum::middleware::from_fn(media_auth_middleware)),
        )
        // Range-served media for the in-progress (EVENT) playlist's byte-range
        // segments — the growing incomplete.*.mp4 while recording.
        .route(
            "/api/v3/events/{id}/stream/media.mp4",
            get(events_playback::get_event_stream_media)
                .route_layer(to_storage_owner())
                .route_layer(axum::middleware::from_fn(media_auth_middleware)),
        )
        // HLS-VOD init + media segments
        .route(
            "/api/v3/events/{id}/stream/init.mp4",
            get(events_playback::get_event_init)
                .route_layer(to_storage_owner())
                .route_layer(axum::middleware::from_fn(media_auth_middleware)),
        )
        .route(
            "/api/v3/events/{id}/stream/segment/{seq}",
            get(events_playback::get_event_segment)
                .route_layer(to_storage_owner())
                .route_layer(axum::middleware::from_fn(media_auth_middleware)),
        )
        // Direct video access
        .route(
            "/api/v3/events/{id}/video",
            get(events_playback::get_event_video)
                .route_layer(to_storage_owner())
                .route_layer(axum::middleware::from_fn(media_auth_middleware)),
        )
        // Thumbnail
        .route(
            "/api/v3/events/{id}/thumbnail",
            get(events_playback::get_event_thumbnail)
                .route_layer(to_storage_owner())
                .route_layer(axum::middleware::from_fn(media_auth_middleware)),
        )
        // One frame of an event as a JPEG (GH #26)
        .route(
            "/api/v3/events/{id}/frames/{fid}/image",
            get(frame_images::get_event_frame_image)
                .route_layer(to_storage_owner())
                .route_layer(axum::middleware::from_fn(media_auth_middleware)),
        )
        // Codec / dimensions / duration metadata
        .route(
            "/api/v3/events/{id}/info",
            get(events_playback::get_event_info)
                .route_layer(to_storage_owner())
                .route_layer(axum::middleware::from_fn(media_auth_middleware)),
        )
        // Motion synopsis — glanceable composite still (P1).
        .route(
            "/api/v3/events/{id}/synopsis/review",
            get(synopsis::get_event_synopsis_review)
                .route_layer(to_storage_owner())
                .route_layer(axum::middleware::from_fn(media_auth_middleware)),
        )
        // Motion synopsis — temporal layout preview (P2).
        .route(
            "/api/v3/events/{id}/synopsis/layout",
            get(synopsis::get_event_synopsis_layout)
                .route_layer(to_storage_owner())
                .route_layer(axum::middleware::from_fn(media_auth_middleware)),
        )
        // Motion synopsis — request/poll the rendered mp4 (P3).
        .route(
            "/api/v3/events/{id}/synopsis",
            get(synopsis::get_event_synopsis)
                .route_layer(to_storage_owner())
                .route_layer(axum::middleware::from_fn(media_auth_middleware)),
        )
        // Motion synopsis — stream the rendered mp4 (P3).
        .route(
            "/api/v3/events/{id}/synopsis/mp4",
            get(synopsis::get_event_synopsis_mp4)
                .route_layer(to_storage_owner())
                .route_layer(axum::middleware::from_fn(media_auth_middleware)),
        )
        // Motion synopsis — range/overview montage across events (P4). The static
        // `synopsis` segment sits alongside the `{id}` routes above.
        .route(
            "/api/v3/events/synopsis",
            get(synopsis::get_range_synopsis)
                .route_layer(axum::middleware::from_fn(media_auth_middleware)),
        )
}
