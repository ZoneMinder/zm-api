//! Mint signed playlist links (#153, #34).
//!
//! An integration that can only put a credential in the URL (Home Assistant,
//! FFmpeg, VLC) gets one playlist URL signed for one monitor's live stream or
//! one event's recording, read-only, with a lifetime the caller chooses. Its
//! playlists then sign every URI they list with short-lived signatures. Nothing
//! is stored; revoke a link by raising its user's revocation floor (log the
//! user out everywhere, change its password or disable it), which is why an
//! integration is best given its own user.

use std::time::Duration;

use axum::extract::{Path, State};
use axum::http::{header::HOST, HeaderMap};
use axum::{Extension, Json};
use garde::Validate;

use crate::dto::request::CreateMediaLinkRequest;
use crate::dto::response::{MediaLinkResponse, RtspLinkResponse};
use crate::error::{AppError, AppResponseError, AppResult, Resource, ResourceType};
use crate::server::state::AppState;
use crate::service::monitor_acl::MonitorScope;
use crate::util::authz::Level;
use crate::util::claim::UserClaims;
use crate::util::media_url::{self, MediaResource, LINK_DEFAULT_TTL};

fn link(
    resource: MediaResource,
    uid: u32,
    req: &CreateMediaLinkRequest,
    path: String,
) -> AppResult<MediaLinkResponse> {
    req.validate().map_err(AppError::InvalidInputError)?;
    let ttl = req
        .ttl_seconds
        .map(Duration::from_secs)
        .unwrap_or(LINK_DEFAULT_TTL);
    let expires_at = chrono::Utc::now().timestamp() + ttl.as_secs() as i64;
    let token = media_url::sign_until(resource, uid, expires_at);
    Ok(MediaLinkResponse {
        playlist_url: format!(
            "{path}?{}={}",
            media_url::PARAM,
            urlencoding::encode(&token)
        ),
        token,
        expires_at,
    })
}

/// Mint a signed link to a monitor's live HLS playlist
#[utoipa::path(
    post,
    path = "/api/v3/live/{monitor_id}/hls/link",
    operation_id = "createLiveMediaLink",
    tag = "Live Streaming",
    params(("monitor_id" = u32, Path, description = "Monitor/Camera ID")),
    request_body = CreateMediaLinkRequest,
    responses(
        (status = 200, description = "Signed playlist link", body = MediaLinkResponse),
        (status = 404, description = "Monitor not found", body = AppResponseError)
    ),
    security(("jwt" = []))
)]
pub async fn create_live_link(
    State(state): State<AppState>,
    Path(monitor_id): Path<u32>,
    Extension(claims): Extension<UserClaims>,
    Json(req): Json<CreateMediaLinkRequest>,
) -> AppResult<Json<MediaLinkResponse>> {
    // Stream access and this monitor's ACL are enforced by the route layers.
    crate::repo::monitors::find_by_id(state.db(), monitor_id)
        .await?
        .ok_or_else(|| {
            AppError::NotFoundError(Resource {
                details: vec![("monitor_id".to_string(), monitor_id.to_string())],
                resource_type: ResourceType::Monitor,
            })
        })?;
    Ok(Json(link(
        MediaResource::Monitor(monitor_id),
        claims.uid,
        &req,
        crate::configure::server::public_path(&format!(
            "/api/v3/live/{monitor_id}/hls/master.m3u8"
        )),
    )?))
}

/// Mint a signed RTSP link to a monitor's live video
///
/// For players that take one URL and can't send headers: Home Assistant's
/// stream component, VLC, NVRs. The link opens this monitor's live video and
/// nothing else, and stops working when it expires or its user is logged
/// out, disabled, or loses Stream access or the monitor. Needs
/// `streaming.rtsp.enabled`.
#[utoipa::path(
    post,
    path = "/api/v3/live/{monitor_id}/rtsp/link",
    operation_id = "createLiveRtspLink",
    tag = "Live Streaming",
    params(("monitor_id" = u32, Path, description = "Monitor/Camera ID")),
    request_body = CreateMediaLinkRequest,
    responses(
        (status = 200, description = "Signed RTSP link", body = RtspLinkResponse),
        (status = 404, description = "Monitor not found", body = AppResponseError),
        (status = 503, description = "The RTSP server is off", body = AppResponseError)
    ),
    security(("jwt" = []))
)]
pub async fn create_rtsp_link(
    State(state): State<AppState>,
    Path(monitor_id): Path<u32>,
    Extension(claims): Extension<UserClaims>,
    headers: HeaderMap,
    Json(req): Json<CreateMediaLinkRequest>,
) -> AppResult<Json<RtspLinkResponse>> {
    let cfg = &state.config.streaming.rtsp;
    if !cfg.enabled {
        return Err(AppError::ServiceUnavailableError(
            "The RTSP server is off (streaming.rtsp.enabled)".to_string(),
        ));
    }
    // Stream access and this monitor's ACL are enforced by the route layers.
    crate::repo::monitors::find_by_id(state.db(), monitor_id)
        .await?
        .ok_or_else(|| {
            AppError::NotFoundError(Resource {
                details: vec![("monitor_id".to_string(), monitor_id.to_string())],
                resource_type: ResourceType::Monitor,
            })
        })?;
    let host = match &cfg.public_host {
        Some(h) => h.clone(),
        None => headers
            .get(HOST)
            .and_then(|v| v.to_str().ok())
            .and_then(crate::streaming::rtsp::host_from_header)
            .ok_or_else(|| {
                AppError::BadRequestError(
                    "No usable Host header; set streaming.rtsp.public_host".to_string(),
                )
            })?,
    };
    let signed = link(
        MediaResource::Monitor(monitor_id),
        claims.uid,
        &req,
        String::new(),
    )?;
    let (url, tls_url) = crate::streaming::rtsp::link_urls(cfg, &host, monitor_id, &signed.token);
    Ok(Json(RtspLinkResponse {
        url,
        tls_url,
        token: signed.token,
        expires_at: signed.expires_at,
    }))
}

/// Mint a signed link to an event's HLS playlist
#[utoipa::path(
    post,
    path = "/api/v3/events/{id}/stream/link",
    operation_id = "createEventMediaLink",
    tag = "Event Playback",
    params(("id" = u64, Path, description = "Event ID")),
    request_body = CreateMediaLinkRequest,
    responses(
        (status = 200, description = "Signed playlist link", body = MediaLinkResponse),
        (status = 404, description = "Event not found", body = AppResponseError)
    ),
    security(("jwt" = []))
)]
pub async fn create_event_link(
    State(state): State<AppState>,
    Path(event_id): Path<u64>,
    Extension(claims): Extension<UserClaims>,
    scope: MonitorScope,
    Json(req): Json<CreateMediaLinkRequest>,
) -> AppResult<Json<MediaLinkResponse>> {
    // Events:View is enforced by the route layer; the event's monitor ACL here.
    let not_found = || {
        AppError::NotFoundError(Resource {
            details: vec![("event_id".to_string(), event_id.to_string())],
            resource_type: ResourceType::Event,
        })
    };
    let event = crate::repo::events::find_by_id(&state, event_id)
        .await?
        .ok_or_else(not_found)?;
    if !scope.allows(event.monitor_id, Level::View) {
        return Err(not_found());
    }
    Ok(Json(link(
        MediaResource::Event {
            event_id,
            monitor_id: event.monitor_id,
        },
        claims.uid,
        &req,
        crate::configure::server::public_path(&format!(
            "/api/v3/events/{event_id}/stream/playlist.m3u8"
        )),
    )?))
}
