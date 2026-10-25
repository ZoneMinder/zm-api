//! Live streaming HTTP handlers
//!
//! Provides unified endpoints for live streaming via HLS and WebRTC,
//! both backed by the stream-socket source router.

use axum::{
    body::Body,
    extract::{
        ws::{Message, WebSocket},
        Path, Query, State, WebSocketUpgrade,
    },
    http::{header, StatusCode},
    response::Response,
    Json,
};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::broadcast;
use tracing::{debug, error, info, warn};

use crate::error::{AppError, AppResponseError, AppResult};
use crate::server::state::AppState;
use crate::streaming::live::webrtc::{WebRtcLiveConfig, WebRtcLiveManager};
use crate::streaming::live::{CoordinatorError, LiveStreamConfig};
use crate::streaming::source::{CachedKeyframe, MonitorSource, VideoCodec};

// ============================================================================
// DTOs
// ============================================================================

/// Request to start a live stream
#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct StartLiveRequest {
    /// Enable HLS output
    #[serde(default = "default_true")]
    pub enable_hls: bool,
    /// Enable WebRTC output
    #[serde(default)]
    pub enable_webrtc: bool,
}

fn default_true() -> bool {
    true
}

impl Default for StartLiveRequest {
    fn default() -> Self {
        Self {
            enable_hls: true,
            enable_webrtc: false,
        }
    }
}

/// Response for starting a live stream
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct StartLiveResponse {
    pub monitor_id: u32,
    pub status: String,
    pub hls_playlist: Option<String>,
    pub webrtc_signaling: Option<String>,
}

/// Response for live stream statistics
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct LiveStatsResponse {
    pub monitor_id: u32,
    pub status: String,
    pub packets_processed: u64,
    pub errors: u64,
    pub uptime_seconds: f64,
    pub protocols: LiveProtocolStatus,
    /// Most-recent WebRTC startup profile (connect→offer→first-RTP), when a
    /// session has run since boot. Lets you confirm cold-vs-warm empirically.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub webrtc_startup: Option<WebRtcStartupView>,
}

/// Server-side WebRTC startup timing surfaced on `/live/{id}/stats`. All `*_ms`
/// are from WS connect and nest: `get_source_ms ≤ offer_ms ≤ ice_connected_ms ≤
/// connected_ms ≤ first_rtp_ms`.
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct WebRtcStartupView {
    /// Reader was already hot at connect (no reader spin-up on the offer path).
    pub warm_start: bool,
    /// connect → get_source returned (reader acquire/restart cost).
    pub get_source_ms: Option<u64>,
    /// connect → SDP offer sent. With trickle ICE this no longer waits on ICE
    /// gathering / STUN, so it collapses toward the floor.
    pub offer_ms: Option<u64>,
    /// connect → ICE-connected (first usable candidate pair).
    pub ice_connected_ms: Option<u64>,
    /// connect → peer Connected (ICE+DTLS). `connected_ms − ice_connected_ms` is
    /// the DTLS handshake (the structural ~1.1s offerer-passive cost on a LAN).
    pub connected_ms: Option<u64>,
    /// connect → first video RTP written.
    pub first_rtp_ms: Option<u64>,
}

/// Protocol status in live stats
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct LiveProtocolStatus {
    pub hls: bool,
    pub webrtc: bool,
}

/// Query parameters for HLS media playlist (LL-HLS support)
#[derive(Debug, Deserialize)]
pub struct LlHlsQuery {
    /// Media Sequence Number to wait for
    #[serde(rename = "_HLS_msn")]
    pub msn: Option<u64>,
    /// Part index to wait for
    #[serde(rename = "_HLS_part")]
    pub part: Option<u32>,
    /// Skip parameter
    #[serde(rename = "_HLS_skip")]
    pub skip: Option<String>,
}

// ============================================================================
// Unified Live Stream Control
// ============================================================================

/// Start live streaming for a monitor
#[utoipa::path(
    post,
    path = "/api/v3/live/{monitor_id}/start",
    operation_id = "startLiveStream",
    tag = "Live Streaming",
    params(
        ("monitor_id" = u32, Path, description = "Monitor/Camera ID")
    ),
    request_body(content = StartLiveRequest, description = "Stream configuration"),
    responses(
        (status = 200, description = "Live streaming started", body = StartLiveResponse),
        (status = 400, description = "Bad request", body = AppResponseError),
        (status = 409, description = "Session already exists", body = AppResponseError),
        (status = 503, description = "Service unavailable", body = AppResponseError)
    ),
    security(
        ("jwt" = [])
    )
)]
pub async fn start_live_stream(
    State(state): State<AppState>,
    Path(monitor_id): Path<u32>,
    Json(request): Json<StartLiveRequest>,
) -> AppResult<Json<StartLiveResponse>> {
    info!("Starting live stream for monitor {}", monitor_id);

    let coordinator = state.live_coordinator.as_ref().ok_or_else(|| {
        AppError::ServiceUnavailableError("Live streaming not configured".to_string())
    })?;

    let config = LiveStreamConfig {
        enable_hls: request.enable_hls,
        enable_webrtc: request.enable_webrtc,
    };

    coordinator
        .start_session(monitor_id, config.clone())
        .await
        .map_err(|e| match e {
            CoordinatorError::SessionExists(_) => AppError::ConflictError(format!(
                "Live stream already exists for monitor {}",
                monitor_id
            )),
            CoordinatorError::SourceNotAvailable(_) => {
                AppError::NotFoundError(crate::error::Resource {
                    resource_type: crate::error::ResourceType::Monitor,
                    details: vec![
                        ("monitor_id".to_string(), monitor_id.to_string()),
                        (
                            "reason".to_string(),
                            "stream socket not available".to_string(),
                        ),
                    ],
                })
            }
            _ => AppError::BadRequestError(format!("Failed to start live stream: {}", e)),
        })?;

    let base_url = crate::configure::server::public_path(&format!("/api/v3/live/{}", monitor_id));

    Ok(Json(StartLiveResponse {
        monitor_id,
        status: "started".to_string(),
        hls_playlist: if config.enable_hls {
            Some(format!("{}/hls/live.m3u8", base_url))
        } else {
            None
        },
        webrtc_signaling: if config.enable_webrtc {
            Some(format!("{}/webrtc/ws", base_url))
        } else {
            None
        },
    }))
}

/// Stop live streaming for a monitor
///
/// This ends the session for every viewer, so it needs Monitors Edit on the
/// monitor, not just Stream View. Idempotent: stopping a non-active stream
/// returns 204.
#[utoipa::path(
    delete,
    path = "/api/v3/live/{monitor_id}/stop",
    operation_id = "stopLiveStream",
    tag = "Live Streaming",
    params(
        ("monitor_id" = u32, Path, description = "Monitor/Camera ID")
    ),
    responses(
        (status = 204, description = "Live streaming stopped (or was not active)"),
        (status = 403, description = "Needs Monitors Edit on this monitor", body = AppResponseError),
        (status = 503, description = "Service unavailable", body = AppResponseError)
    ),
    security(
        ("jwt" = [])
    )
)]
pub async fn stop_live_stream(
    State(state): State<AppState>,
    Path(monitor_id): Path<u32>,
    scope: crate::service::monitor_acl::MonitorScope,
) -> AppResult<StatusCode> {
    use crate::util::authz::Level;
    // The monitor's row decides, as `canEdit('Monitors', $mid)` does (#244).
    if !scope.allows(monitor_id, Level::Edit) {
        return Err(AppError::PermissionDeniedError(
            "Monitors Edit on this monitor required to stop its live stream".to_string(),
        ));
    }
    info!("Stopping live stream for monitor {}", monitor_id);

    let coordinator = state.live_coordinator.as_ref().ok_or_else(|| {
        AppError::ServiceUnavailableError("Live streaming not configured".to_string())
    })?;

    match coordinator.stop_session(monitor_id).await {
        Ok(()) => {}
        Err(CoordinatorError::SessionNotFound(_)) => {
            debug!(
                "Stop requested for non-active monitor {}, returning 204",
                monitor_id
            );
        }
        Err(e) => {
            return Err(AppError::BadRequestError(format!(
                "Failed to stop live stream: {}",
                e
            )));
        }
    }

    Ok(StatusCode::NO_CONTENT)
}

/// Get live stream statistics
#[utoipa::path(
    get,
    path = "/api/v3/live/{monitor_id}/stats",
    operation_id = "getLiveStats",
    tag = "Live Streaming",
    params(
        ("monitor_id" = u32, Path, description = "Monitor/Camera ID")
    ),
    responses(
        (status = 200, description = "Live stream statistics", body = LiveStatsResponse),
        (status = 404, description = "Session not found", body = AppResponseError),
        (status = 503, description = "Service unavailable", body = AppResponseError)
    ),
    security(
        ("jwt" = [])
    )
)]
pub async fn get_live_stats(
    State(state): State<AppState>,
    Path(monitor_id): Path<u32>,
) -> AppResult<Json<LiveStatsResponse>> {
    let coordinator = state.live_coordinator.as_ref().ok_or_else(|| {
        AppError::ServiceUnavailableError("Live streaming not configured".to_string())
    })?;

    let stats = coordinator.get_stats(monitor_id).await.ok_or_else(|| {
        AppError::NotFoundError(crate::error::Resource {
            resource_type: crate::error::ResourceType::Monitor,
            details: vec![("monitor_id".to_string(), monitor_id.to_string())],
        })
    })?;

    let webrtc_startup = state
        .source_router
        .as_ref()
        .and_then(|r| r.webrtc_startup(monitor_id))
        .map(|t| WebRtcStartupView {
            warm_start: t.warm_start,
            get_source_ms: t.get_source_ms,
            offer_ms: t.offer_ms,
            ice_connected_ms: t.ice_connected_ms,
            connected_ms: t.connected_ms,
            first_rtp_ms: t.first_rtp_ms,
        });

    Ok(Json(LiveStatsResponse {
        monitor_id: stats.monitor_id,
        status: format!("{:?}", stats.status).to_lowercase(),
        packets_processed: stats.packets_processed,
        errors: stats.errors,
        uptime_seconds: stats.uptime_seconds,
        protocols: LiveProtocolStatus {
            hls: stats.hls_enabled,
            webrtc: stats.webrtc_enabled,
        },
        webrtc_startup,
    }))
}

/// List all active live streams
#[utoipa::path(
    get,
    path = "/api/v3/live/sessions",
    operation_id = "listLiveSessions",
    tag = "Live Streaming",
    responses(
        (status = 200, description = "List of active live sessions", body = Vec<u32>),
        (status = 503, description = "Service unavailable", body = AppResponseError)
    ),
    security(
        ("jwt" = [])
    )
)]
pub async fn list_live_sessions(
    State(state): State<AppState>,
    scope: crate::service::monitor_acl::MonitorScope,
) -> AppResult<Json<Vec<u32>>> {
    let coordinator = state.live_coordinator.as_ref().ok_or_else(|| {
        AppError::ServiceUnavailableError("Live streaming not configured".to_string())
    })?;

    let sessions = coordinator.list_sessions().await;
    Ok(Json(visible_monitor_ids(&scope, sessions)))
}

/// The monitor ids the caller may view; the rest are left out.
fn visible_monitor_ids(
    scope: &crate::service::monitor_acl::MonitorScope,
    ids: Vec<u32>,
) -> Vec<u32> {
    ids.into_iter()
        .filter(|id| scope.allows(*id, crate::util::authz::Level::View))
        .collect()
}

// ============================================================================
// HLS Endpoints (proxied through /api/v3/live/{monitor_id}/hls/...)
// ============================================================================

/// Get HLS master playlist
#[utoipa::path(
    get,
    path = "/api/v3/live/{monitor_id}/hls/master.m3u8",
    operation_id = "getLiveMasterPlaylist",
    tag = "Live Streaming",
    params(
        ("monitor_id" = u32, Path, description = "Monitor/Camera ID")
    ),
    responses(
        (status = 200, description = "Master playlist", content_type = "application/vnd.apple.mpegurl"),
        (status = 404, description = "Session not found", body = AppResponseError)
    ),
    security(("jwt" = []))
)]
pub async fn get_live_master_playlist(
    State(state): State<AppState>,
    Path(monitor_id): Path<u32>,
    grant: Option<axum::Extension<crate::service::media_grant::MediaGrant>>,
    uri: axum::http::Uri,
) -> Result<Response, AppError> {
    debug!("Serving live master playlist for monitor {}", monitor_id);

    let hls_manager = state.hls_session_manager.as_ref().ok_or_else(|| {
        AppError::ServiceUnavailableError("HLS streaming not configured".to_string())
    })?;

    let playlist = hls_manager
        .get_master_playlist(monitor_id)
        .await
        .map_err(|_| {
            AppError::NotFoundError(crate::error::Resource {
                resource_type: crate::error::ResourceType::Monitor,
                details: vec![("monitor_id".to_string(), monitor_id.to_string())],
            })
        })?;
    let playlist = sign_live_playlist(playlist, monitor_id, grant.as_deref(), &uri);

    Ok(Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/vnd.apple.mpegurl")
        .header(header::CACHE_CONTROL, "no-cache, no-store, must-revalidate")
        .body(Body::from(playlist))
        .unwrap())
}

/// Sign a live playlist's URIs when it was fetched with a URL credential, so
/// players that can't send a header can fetch them (#153).
fn sign_live_playlist(
    playlist: String,
    monitor_id: u32,
    grant: Option<&crate::service::media_grant::MediaGrant>,
    uri: &axum::http::Uri,
) -> String {
    use crate::util::media_url::{MediaResource, LIVE_TTL};
    match crate::service::media_grant::uri_signer(
        grant,
        uri,
        MediaResource::Monitor(monitor_id),
        LIVE_TTL,
    ) {
        Some(signer) => crate::streaming::hls::signed_playlist::sign_playlist(&playlist, &signer),
        None => playlist,
    }
}

/// Get HLS media playlist
#[utoipa::path(
    get,
    path = "/api/v3/live/{monitor_id}/hls/live.m3u8",
    operation_id = "getLiveMediaPlaylist",
    tag = "Live Streaming",
    params(
        ("monitor_id" = u32, Path, description = "Monitor/Camera ID")
    ),
    responses(
        (status = 200, description = "Media playlist", content_type = "application/vnd.apple.mpegurl"),
        (status = 404, description = "Session not found", body = AppResponseError)
    ),
    security(("jwt" = []))
)]
pub async fn get_live_media_playlist(
    State(state): State<AppState>,
    Path(monitor_id): Path<u32>,
    Query(ll_hls): Query<LlHlsQuery>,
    grant: Option<axum::Extension<crate::service::media_grant::MediaGrant>>,
    uri: axum::http::Uri,
) -> Result<Response, AppError> {
    debug!("Serving live media playlist for monitor {}", monitor_id);

    let hls_manager = state.hls_session_manager.as_ref().ok_or_else(|| {
        AppError::ServiceUnavailableError("HLS streaming not configured".to_string())
    })?;

    // Handle LL-HLS blocking reload
    if let Some(msn) = ll_hls.msn {
        let timeout = Duration::from_secs(5);
        if hls_manager
            .wait_for_segment(monitor_id, msn, timeout)
            .await
            .is_err()
        {
            debug!("LL-HLS wait timeout for monitor {} msn {}", monitor_id, msn);
        }
    }

    let playlist = hls_manager.get_playlist(monitor_id).await.map_err(|_| {
        AppError::NotFoundError(crate::error::Resource {
            resource_type: crate::error::ResourceType::Monitor,
            details: vec![("monitor_id".to_string(), monitor_id.to_string())],
        })
    })?;
    let playlist = sign_live_playlist(playlist, monitor_id, grant.as_deref(), &uri);

    Ok(Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/vnd.apple.mpegurl")
        .header(header::CACHE_CONTROL, "no-cache, no-store, must-revalidate")
        .body(Body::from(playlist))
        .unwrap())
}

/// Get HLS init segment
#[utoipa::path(
    get,
    path = "/api/v3/live/{monitor_id}/hls/init.mp4",
    operation_id = "getLiveInitSegment",
    tag = "Live Streaming",
    params(
        ("monitor_id" = u32, Path, description = "Monitor/Camera ID")
    ),
    responses(
        (status = 200, description = "Init segment", content_type = "video/mp4"),
        (status = 404, description = "Init segment not ready", body = AppResponseError)
    ),
    security(("jwt" = []))
)]
pub async fn get_live_init_segment(
    State(state): State<AppState>,
    Path(monitor_id): Path<u32>,
) -> Result<Response, AppError> {
    debug!("Serving live init segment for monitor {}", monitor_id);

    let hls_manager = state.hls_session_manager.as_ref().ok_or_else(|| {
        AppError::ServiceUnavailableError("HLS streaming not configured".to_string())
    })?;

    let data = hls_manager
        .get_init_segment(monitor_id)
        .await
        .map_err(|_| {
            AppError::NotFoundError(crate::error::Resource {
                resource_type: crate::error::ResourceType::Monitor,
                details: vec![
                    ("monitor_id".to_string(), monitor_id.to_string()),
                    ("segment".to_string(), "init.mp4".to_string()),
                ],
            })
        })?;

    Ok(Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "video/mp4")
        .header(header::CACHE_CONTROL, "max-age=31536000")
        .body(Body::from(data))
        .unwrap())
}

/// Path parameters for HLS segment
#[derive(Debug, Deserialize)]
pub struct HlsSegmentPath {
    pub monitor_id: u32,
    pub segment: String,
}

/// Get HLS media segment
#[utoipa::path(
    get,
    path = "/api/v3/live/{monitor_id}/hls/{segment}",
    operation_id = "getLiveSegment",
    tag = "Live Streaming",
    params(
        ("monitor_id" = u32, Path, description = "Monitor/Camera ID"),
        ("segment" = String, Path, description = "Segment filename (e.g., segment_00001.m4s)")
    ),
    responses(
        (status = 200, description = "Media segment", content_type = "video/iso.segment"),
        (status = 404, description = "Segment not found", body = AppResponseError)
    ),
    security(("jwt" = []))
)]
pub async fn get_live_segment(
    State(state): State<AppState>,
    Path(path): Path<HlsSegmentPath>,
) -> Result<Response, AppError> {
    debug!(
        "Serving live segment {} for monitor {}",
        path.segment, path.monitor_id
    );

    let sequence = parse_segment_sequence(&path.segment).ok_or_else(|| {
        AppError::BadRequestError(format!("Invalid segment name: {}", path.segment))
    })?;

    let hls_manager = state.hls_session_manager.as_ref().ok_or_else(|| {
        AppError::ServiceUnavailableError("HLS streaming not configured".to_string())
    })?;

    let data = hls_manager
        .get_segment(path.monitor_id, sequence)
        .await
        .map_err(|_| {
            AppError::NotFoundError(crate::error::Resource {
                resource_type: crate::error::ResourceType::Monitor,
                details: vec![
                    ("monitor_id".to_string(), path.monitor_id.to_string()),
                    ("segment".to_string(), path.segment.clone()),
                ],
            })
        })?;

    Ok(Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "video/iso.segment")
        .header(header::CACHE_CONTROL, "max-age=31536000")
        .body(Body::from(data))
        .unwrap())
}

/// Parse sequence number from segment filename
fn parse_segment_sequence(filename: &str) -> Option<u64> {
    let name = filename.strip_suffix(".m4s")?;
    let parts: Vec<&str> = name.split('_').collect();

    if parts.len() >= 2 && parts[0] == "segment" {
        let seq_part = parts[1].split('.').next()?;
        seq_part.parse().ok()
    } else {
        None
    }
}

// ============================================================================
// WebRTC Endpoints (WebSocket signaling)
// ============================================================================

/// WebRTC signaling message types.
///
/// This is the *actual* wire format exchanged over the
/// `/api/v3/live/{monitor_id}/webrtc/ws` WebSocket (text frames, JSON). The
/// discriminator is the `type` field, lowercased: `offer`, `answer`,
/// `icecandidate`, `ready`, `error`, `ping`, `pong`.
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum WebRtcSignalingMessage {
    /// Server offer to client
    Offer {
        session_id: String,
        sdp: String,
    },
    /// Client answer to server
    Answer {
        session_id: String,
        sdp: String,
    },
    /// ICE candidate exchange
    IceCandidate {
        session_id: String,
        candidate: String,
        #[serde(rename = "sdpMid")]
        sdp_mid: Option<String>,
        #[serde(rename = "sdpMLineIndex")]
        sdp_mline_index: Option<u16>,
    },
    /// Session ready notification
    Ready {
        session_id: String,
        monitor_id: u32,
        /// Whether the session carries an audio track (G.711 pass-through
        /// or AAC transcoded to Opus) alongside the video track
        has_audio: bool,
    },
    /// Error message
    Error {
        message: String,
    },
    /// Ping/Pong for keepalive
    Ping,
    Pong,
}

/// WebSocket handler for WebRTC signaling.
///
/// Upgrades to a WebSocket and exchanges WebRTC signaling over JSON **text
/// frames**. OpenAPI cannot type WebSocket message bodies, so the message
/// schema is documented here and as the `WebRtcSignalingMessage` component.
///
/// ## Message envelope
///
/// Every frame is a JSON object discriminated by a lowercase `type` field
/// (see the `WebRtcSignalingMessage` schema). Note the field casing:
/// `icecandidate` (one word) with camelCase `sdpMid` / `sdpMLineIndex`.
///
/// ## Typical flow (client-initiated, the common path)
///
/// 1. **Server → client** `{"type":"offer","session_id":"<id>","sdp":"..."}`
///    — the server sends its SDP offer immediately on connect.
/// 2. **Client → server** `{"type":"answer","session_id":"<id>","sdp":"..."}`.
/// 3. **Both directions** `{"type":"icecandidate","session_id":"<id>",`
///    `"candidate":"candidate:...","sdpMid":"0","sdpMLineIndex":0}` — trickled
///    as discovered.
/// 4. **Server → client** `{"type":"ready","session_id":"<id>","monitor_id":N,`
///    `"has_audio":false}` once media is flowing. `has_audio` tells the client
///    whether an audio track accompanies the video.
/// 5. **Errors** arrive as `{"type":"error","message":"..."}`.
/// 6. **Keepalive**: either side may send `{"type":"ping"}`; the peer replies
///    `{"type":"pong"}`.
#[utoipa::path(
    security(("jwt" = [])),
    get,
    path = "/api/v3/live/{monitor_id}/webrtc/ws",
    operation_id = "webrtcSignalingStream",
    tag = "Live Streaming",
    params(
        ("monitor_id" = u32, Path, description = "Monitor/Camera ID")
    ),
    responses(
        (status = 101, description = "WebSocket upgraded. Frames are JSON `WebRtcSignalingMessage` \
            objects (text). Server sends `offer`/`ready`/`icecandidate`/`error`/`pong`; client \
            sends `answer`/`icecandidate`/`ping`.", body = WebRtcSignalingMessage),
        (status = 404, description = "Monitor not found", body = AppResponseError),
        (status = 503, description = "Service unavailable", body = AppResponseError)
    )
)]
pub async fn webrtc_websocket_handler(
    State(state): State<AppState>,
    Path(monitor_id): Path<u32>,
    claims: Option<axum::Extension<crate::util::claim::UserClaims>>,
    grant: Option<axum::Extension<crate::service::media_grant::MediaGrant>>,
    ws: WebSocketUpgrade,
) -> Result<Response, AppError> {
    info!(
        "WebRTC WebSocket signaling request for monitor {}",
        monitor_id
    );

    // Who is watching, for the access recheck: the token's user, or the
    // user a signed link was made for (from now, as a link has no iat).
    let viewer = match (claims, grant) {
        (Some(c), _) => Viewer {
            uid: c.uid,
            since: c.iat,
        },
        (None, Some(g)) => Viewer {
            uid: g.uid,
            since: chrono::Utc::now().timestamp(),
        },
        (None, None) => {
            return Err(AppError::UnauthorizedError(
                "Authentication required".to_string(),
            ))
        }
    };

    // Check if source router is available
    let source_router = state.source_router.as_ref().ok_or_else(|| {
        AppError::ServiceUnavailableError("Live streaming not configured".to_string())
    })?;

    // Check if the source is available
    if !source_router.is_available(monitor_id) {
        return Err(AppError::NotFoundError(crate::error::Resource {
            resource_type: crate::error::ResourceType::Monitor,
            details: vec![
                ("monitor_id".to_string(), monitor_id.to_string()),
                (
                    "reason".to_string(),
                    "stream socket not available".to_string(),
                ),
            ],
        }));
    }

    let router = Arc::clone(source_router);

    // A slot of `max_connections`, shared with WHEP, held until the session
    // ends.
    let limit = state.config.streaming.webrtc.max_connections as usize;
    let slot = state.webrtc_slots.try_acquire(limit).ok_or_else(|| {
        AppError::ServiceUnavailableError(format!("WebRTC session limit ({limit}) reached"))
    })?;

    // Honor the operator's configured STUN/TURN servers instead of the
    // hardcoded Google STUN defaults (REVIEW_FIXES_PLAN §2.2).
    let webrtc_config = WebRtcLiveConfig::from_webrtc_config(&state.config.streaming.webrtc);

    Ok(ws.on_upgrade(move |socket| async move {
        let _slot = slot;
        handle_webrtc_websocket(state, router, monitor_id, viewer, webrtc_config, socket).await
    }))
}

/// Inject a keyframe at DTLS-connected time so the viewer sees a picture
/// immediately instead of waiting a full GOP for the next natural IDR.
///
/// Crucially this re-reads the source's keyframe cache **live**. On a cold
/// start the cache is empty when signaling begins, but the socket reader almost
/// always populates it during the seconds of ICE/DTLS negotiation. The old code
/// injected the snapshot captured *before* signaling (`pre_signaling`), which on
/// a cold start was `None` — throwing away the freshly-cached keyframe and
/// forcing a GOP-length stall (1–8s depending on the camera). We prefer the live
/// read, fall back to the pre-signaling snapshot, then to nothing (the normal
/// access-unit path then waits for the next keyframe). See REVIEW_FIXES_PLAN §2.1.
async fn inject_startup_keyframe(
    webrtc_manager: &WebRtcLiveManager,
    source: &MonitorSource,
    pre_signaling: &Option<CachedKeyframe>,
    pump: &mut crate::streaming::live::webrtc::MediaPump,
) -> bool {
    let keyframe = source.cached_keyframe().or_else(|| pre_signaling.clone());
    pump.inject_keyframe(webrtc_manager, keyframe.as_ref())
        .await
}

/// The user a WebSocket session streams to, rechecked while it runs.
#[derive(Debug, Clone, Copy)]
struct Viewer {
    uid: u32,
    /// Tokens revoked after this end the session.
    since: i64,
}

async fn handle_webrtc_websocket(
    state: AppState,
    source_router: Arc<crate::streaming::source::SourceRouter>,
    monitor_id: u32,
    viewer: Viewer,
    webrtc_config: WebRtcLiveConfig,
    socket: WebSocket,
) {
    info!(
        "WebRTC signaling WebSocket connected for monitor {}",
        monitor_id
    );

    // t0 for startup instrumentation: connect → offer → first-RTP, surfaced on
    // /live/{id}/stats to confirm cold-vs-warm on real hardware.
    let connect_at = Instant::now();

    // Create WebRTC manager for this session
    let webrtc_manager = WebRtcLiveManager::new(webrtc_config);

    // Whether the reader was already hot — checked *before* get_source (which
    // may (re)start it), so the offer profile distinguishes a fast warm offer
    // from a cold reader spin-up that `warm_start` (keyframe cache) alone missed.
    let warm_start = source_router.is_reader_hot(monitor_id).await;

    // Get source to determine codec. This may start/restart the reader, so it is
    // timed separately — the suspected cold-offer cost.
    let get_source_at = Instant::now();
    let source = match source_router.get_source(monitor_id).await {
        Ok(s) => s,
        Err(e) => {
            error!("Failed to get source for monitor {}: {}", monitor_id, e);
            return;
        }
    };
    let get_source_ms = get_source_at.elapsed().as_millis() as u64;

    // Try the keyframe cache first (warm path: reader already active).
    // If populated, we skip the cold-path codec-detection grace period entirely.
    let cached: Option<CachedKeyframe> = source.cached_keyframe();

    let (codec, profile_level_id) = if let Some(ref ck) = cached {
        info!(
            "Using cached keyframe for monitor {} (profile-level-id={})",
            monitor_id, ck.profile_level_id
        );
        (ck.codec, Some(ck.profile_level_id.clone()))
    } else {
        // Cold path: no cached keyframe yet (first viewer since the reader
        // started). Determine the codec from the source — the socket HELLO sets
        // it as soon as it parses a packet; give it a brief grace period, then
        // default to H.264.
        //
        // We deliberately do NOT wait for an SPS NAL here. The SDP offer
        // advertises fixed, browser-universal H.264 profiles regardless of the
        // camera's real profile-level-id (see `h264_offer_profile_level_ids`),
        // so the detected value only ever reached a debug log — the previous
        // up-to-5s SPS scan added that much startup latency for nothing.
        // See REVIEW_FIXES_PLAN §2.3.
        debug!(
            "No keyframe cache for monitor {}, detecting codec from source",
            monitor_id
        );

        tokio::time::sleep(Duration::from_millis(100)).await;
        let codec = match source.codec().await {
            VideoCodec::Unknown => VideoCodec::H264,
            known => known,
        };

        (codec, None)
    };

    // Decide the audio track for the offer. G.711 passes through unchanged
    // (a mandatory WebRTC codec); AAC is transcoded to Opus — browsers do
    // not implement AAC over RTP. The transcoder is created up front so a
    // missing libopus downgrades the offer to video-only instead of
    // advertising an audio m-line that could never carry media.
    let (audio_kind, audio_transcoder) =
        crate::streaming::live::webrtc::audio_plan(monitor_id, source.audio_codec());

    // Create WebRTC session. With trickle ICE the offer is returned immediately
    // (no ICE-gather wait); candidates stream out on `candidate_rx` and the
    // ICE-connected moment is surfaced on `ice_connected_rx`.
    let crate::streaming::live::webrtc::SessionHandshake {
        session_id,
        offer,
        mut pc_state_rx,
        mut ice_connected_rx,
        mut candidate_rx,
        ..
    } = match webrtc_manager
        .create_session(monitor_id, codec, profile_level_id.as_deref(), audio_kind)
        .await
    {
        Ok(result) => result,
        Err(e) => {
            error!(
                "Failed to create WebRTC session for monitor {}: {}",
                monitor_id, e
            );
            return;
        }
    };

    info!(
        "Created WebRTC session {} for monitor {}",
        session_id, monitor_id
    );

    let (mut ws_sender, mut ws_receiver) = socket.split();

    // Send offer to client
    let offer_msg = WebRtcSignalingMessage::Offer {
        session_id: session_id.clone(),
        sdp: offer.sdp,
    };
    if let Ok(json) = serde_json::to_string(&offer_msg) {
        if ws_sender.send(Message::Text(json.into())).await.is_err() {
            error!("Failed to send offer to client");
            return;
        }
    }
    // Record the offer profile: warm flag, the get_source (reader-acquire) cost,
    // and connect → offer-sent.
    source_router.record_webrtc_offer(
        monitor_id,
        warm_start,
        get_source_ms,
        connect_at.elapsed().as_millis() as u64,
    );

    // Subscribe to video packets for streaming
    let mut video_rx = source.subscribe_video();

    // Subscribe to audio when the offer carries an audio track
    let mut audio_rx = audio_kind.is_some().then(|| source.subscribe_audio());

    // Watch the stream-socket reader's health. If the reader task exits, the watch
    // sender is dropped and `changed()` returns Err — the broadcast channel
    // would then never yield another packet (and never close, since the
    // MonitorSource keeps the Sender alive), so the session must end rather
    // than hang silently.
    let mut reader_health_rx = source.subscribe_reader_health();

    // Assembles individual NAL units into complete access units (frames)
    // so the H264 payloader assigns one RTP timestamp per picture.
    let mut pump =
        crate::streaming::live::webrtc::MediaPump::new(session_id.clone(), audio_transcoder);

    // Track if DTLS handshake is complete and we can stream media
    let mut streaming_started = false;
    // Track if cached keyframe has been injected
    let mut cache_injected = false;
    // Track if the connect → first-video-RTP timing has been recorded.
    let mut first_rtp_recorded = false;
    // Track if we've set the SDP answer (waiting for DTLS)
    let mut answer_set = false;
    // Track if the connect → ICE-connected time has been recorded (once).
    let mut ice_recorded = false;
    // Set once the trickle-candidate channel is drained (end-of-candidates).
    let mut candidates_done = false;
    // Connection timeout: the client has 30s to answer the offer, then 30s
    // more for the connection to come up. A client that never answers would
    // otherwise hold a session slot for good.
    let connection_timeout = tokio::time::sleep(Duration::from_secs(30));
    tokio::pin!(connection_timeout);
    // The viewer's access is checked again every minute: a disabled or
    // revoked user, or one who lost Stream or this monitor, is cut off.
    let mut recheck = tokio::time::interval(crate::streaming::live::whep::RECHECK);
    recheck.tick().await;

    // Main event loop
    loop {
        tokio::select! {
            // Handle incoming signaling messages
            msg = ws_receiver.next() => {
                match msg {
                    Some(Ok(Message::Text(text))) => {
                        if let Ok(signaling_msg) = serde_json::from_str::<WebRtcSignalingMessage>(&text) {
                            match signaling_msg {
                                WebRtcSignalingMessage::Answer { session_id: sid, sdp }
                                    if sid == session_id => {
                                        let answer = match webrtc::peer_connection::RTCSessionDescription::answer(sdp) {
                                            Ok(a) => a,
                                            Err(e) => {
                                                error!("Invalid SDP answer from client: {}", e);
                                                let error_msg = WebRtcSignalingMessage::Error {
                                                    message: format!("Invalid SDP answer: {}", e),
                                                };
                                                if let Ok(json) = serde_json::to_string(&error_msg) {
                                                    let _ = ws_sender.send(Message::Text(json.into())).await;
                                                }
                                                continue;
                                            }
                                        };
                                        if let Err(e) = webrtc_manager.set_answer(&session_id, answer).await {
                                            error!("Failed to set answer: {}", e);
                                            let error_msg = WebRtcSignalingMessage::Error {
                                                message: e.to_string(),
                                            };
                                            if let Ok(json) = serde_json::to_string(&error_msg) {
                                                let _ = ws_sender.send(Message::Text(json.into())).await;
                                            }
                                        } else {
                                            answer_set = true;
                                            // Reset timeout to 30s from now
                                            connection_timeout.as_mut().reset(tokio::time::Instant::now() + Duration::from_secs(30));
                                            info!("SDP answer set for session {}, waiting for DTLS completion", session_id);

                                            // Check if peer connection already reached Connected
                                            // before we started watching. This handles the race
                                            // where DTLS completes very quickly (e.g. on LAN).
                                            let current_state = *pc_state_rx.borrow_and_update();
                                            match current_state {
                                                webrtc::peer_connection::RTCPeerConnectionState::Connected => {
                                                    streaming_started = true;
                                                    source_router.record_webrtc_connected(monitor_id, connect_at.elapsed().as_millis() as u64);
                                                    // Inject the freshest keyframe for instant video start
                                                    if !cache_injected {
                                                        cache_injected = inject_startup_keyframe(&webrtc_manager, &source, &cached, &mut pump).await;
                                                        if cache_injected {
                                                            info!("Injected startup keyframe for session {} (fast start)", session_id);
                                                        }
                                                    }
                                                    let ready_msg = WebRtcSignalingMessage::Ready {
                                                        session_id: session_id.clone(),
                                                        monitor_id,
                                                        has_audio: audio_kind.is_some(),
                                                    };
                                                    if let Ok(json) = serde_json::to_string(&ready_msg) {
                                                        let _ = ws_sender.send(Message::Text(json.into())).await;
                                                    }
                                                    info!("WebRTC session {} already connected, streaming started", session_id);
                                                }
                                                webrtc::peer_connection::RTCPeerConnectionState::Failed => {
                                                    error!("WebRTC peer connection already failed for session {}", session_id);
                                                    let error_msg = WebRtcSignalingMessage::Error {
                                                        message: "Peer connection failed".to_string(),
                                                    };
                                                    if let Ok(json) = serde_json::to_string(&error_msg) {
                                                        let _ = ws_sender.send(Message::Text(json.into())).await;
                                                    }
                                                    break;
                                                }
                                                _ => {
                                                    debug!("WebRTC session {} current state after answer: {:?}", session_id, current_state);
                                                }
                                            }
                                        }
                                    }
                                WebRtcSignalingMessage::IceCandidate { session_id: sid, candidate, sdp_mid, sdp_mline_index }
                                    if sid == session_id => {
                                        if let Err(e) = webrtc_manager.add_ice_candidate(&session_id, &candidate, sdp_mid, sdp_mline_index).await {
                                            warn!("Failed to add ICE candidate: {}", e);
                                        }
                                    }
                                WebRtcSignalingMessage::Ping => {
                                    let pong = WebRtcSignalingMessage::Pong;
                                    if let Ok(json) = serde_json::to_string(&pong) {
                                        let _ = ws_sender.send(Message::Text(json.into())).await;
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                    Some(Ok(Message::Ping(data))) => {
                        let _ = ws_sender.send(Message::Pong(data)).await;
                    }
                    Some(Ok(Message::Close(_))) => {
                        info!("WebRTC signaling client disconnected for monitor {}", monitor_id);
                        break;
                    }
                    Some(Err(e)) => {
                        error!("WebRTC signaling error for monitor {}: {}", monitor_id, e);
                        break;
                    }
                    None => break,
                    _ => {}
                }
            }

            // Wait for peer connection state changes (DTLS handshake)
            result = pc_state_rx.changed(), if answer_set && !streaming_started => {
                match result {
                    Ok(()) => {
                        let state = *pc_state_rx.borrow_and_update();
                        match state {
                            webrtc::peer_connection::RTCPeerConnectionState::Connected => {
                                streaming_started = true;
                                source_router.record_webrtc_connected(monitor_id, connect_at.elapsed().as_millis() as u64);
                                // Inject the freshest keyframe for instant video start
                                if !cache_injected {
                                    cache_injected = inject_startup_keyframe(&webrtc_manager, &source, &cached, &mut pump).await;
                                    if cache_injected {
                                        info!("Injected startup keyframe for session {} (fast start)", session_id);
                                    }
                                }
                                // Send ready notification now that DTLS is complete
                                let ready_msg = WebRtcSignalingMessage::Ready {
                                    session_id: session_id.clone(),
                                    monitor_id,
                                    has_audio: audio_kind.is_some(),
                                };
                                if let Ok(json) = serde_json::to_string(&ready_msg) {
                                    let _ = ws_sender.send(Message::Text(json.into())).await;
                                }
                                info!("WebRTC session {} connected (DTLS complete), streaming started", session_id);
                            }
                            webrtc::peer_connection::RTCPeerConnectionState::Failed => {
                                error!("WebRTC peer connection failed for session {}", session_id);
                                let error_msg = WebRtcSignalingMessage::Error {
                                    message: "Peer connection failed".to_string(),
                                };
                                if let Ok(json) = serde_json::to_string(&error_msg) {
                                    let _ = ws_sender.send(Message::Text(json.into())).await;
                                }
                                break;
                            }
                            webrtc::peer_connection::RTCPeerConnectionState::Disconnected
                            | webrtc::peer_connection::RTCPeerConnectionState::Closed => {
                                warn!("WebRTC session {} peer connection state: {:?}", session_id, state);
                                let error_msg = WebRtcSignalingMessage::Error {
                                    message: format!("Peer connection {:?}", state),
                                };
                                if let Ok(json) = serde_json::to_string(&error_msg) {
                                    let _ = ws_sender.send(Message::Text(json.into())).await;
                                }
                                break;
                            }
                            _ => {
                                debug!("WebRTC session {} state: {:?}", session_id, state);
                            }
                        }
                    }
                    Err(_) => {
                        error!("WebRTC peer connection state watch dropped for session {}", session_id);
                        let error_msg = WebRtcSignalingMessage::Error {
                            message: "Peer connection closed unexpectedly".to_string(),
                        };
                        if let Ok(json) = serde_json::to_string(&error_msg) {
                            let _ = ws_sender.send(Message::Text(json.into())).await;
                        }
                        break;
                    }
                }
            }

            // Trickle ICE: relay each locally-gathered candidate to the browser
            // as it is discovered (the browser trickles its candidates back via
            // the IceCandidate handler above). Always active until the channel
            // signals end-of-candidates.
            maybe_candidate = candidate_rx.recv(), if !candidates_done => {
                match maybe_candidate {
                    Some(c) => {
                        let msg = WebRtcSignalingMessage::IceCandidate {
                            session_id: session_id.clone(),
                            candidate: c.candidate,
                            sdp_mid: c.sdp_mid,
                            sdp_mline_index: c.sdp_mline_index,
                        };
                        if let Ok(json) = serde_json::to_string(&msg) {
                            let _ = ws_sender.send(Message::Text(json.into())).await;
                        }
                    }
                    None => candidates_done = true,
                }
            }

            // Record connect → ICE-connected (the lower bound of the offer→
            // connected window). The remaining gap to `connected_ms` is the DTLS
            // handshake, so the two numbers split the ICE cost from the DTLS cost.
            result = ice_connected_rx.changed(), if !ice_recorded => {
                if result.is_ok() && *ice_connected_rx.borrow_and_update() {
                    ice_recorded = true;
                    source_router
                        .record_webrtc_ice_connected(monitor_id, connect_at.elapsed().as_millis() as u64);
                }
            }

            // Connection timeout: no answer, or no connection after it.
            () = &mut connection_timeout, if !streaming_started => {
                let message = if answer_set {
                    "Connection timeout: DTLS handshake did not complete within 30 seconds"
                } else {
                    "Signalling timeout: no answer to the offer within 30 seconds"
                };
                warn!("WebRTC session {}: {}", session_id, message);
                let error_msg = WebRtcSignalingMessage::Error {
                    message: message.to_string(),
                };
                if let Ok(json) = serde_json::to_string(&error_msg) {
                    let _ = ws_sender.send(Message::Text(json.into())).await;
                }
                break;
            }

            _ = recheck.tick() => {
                if let Err(e) = crate::service::media_grant::check_viewer(
                    &state, viewer.uid, viewer.since, crate::util::authz::Feature::Stream, monitor_id,
                ).await {
                    info!(
                        "WebRTC session {}: user {} lost access ({}); closing",
                        session_id, viewer.uid, e
                    );
                    let error_msg = WebRtcSignalingMessage::Error {
                        message: "Access to this stream has ended".to_string(),
                    };
                    if let Ok(json) = serde_json::to_string(&error_msg) {
                        let _ = ws_sender.send(Message::Text(json.into())).await;
                    }
                    break;
                }
            }

            // Handle video packets for streaming (only after DTLS connected)
            result = video_rx.recv(), if streaming_started => {
                match result {
                    Ok(packet) => {
                        // Feed each NAL into the assembler; when a complete
                        // access unit (frame) is ready, send it as one sample
                        // so all NALs share a single RTP timestamp.
                        if pump.video(&webrtc_manager, &packet).await && !first_rtp_recorded {
                            first_rtp_recorded = true;
                            source_router.record_webrtc_first_rtp(
                                monitor_id,
                                connect_at.elapsed().as_millis() as u64,
                            );
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        warn!("WebRTC session {} lagged {} packets", session_id, n);
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        info!("Video source closed for WebRTC session {}", session_id);
                        break;
                    }
                }
            }

            // Handle audio packets. The branch pends forever for video-only
            // sessions; packets arriving before DTLS completes are dropped
            // (the receiver must keep draining so the broadcast channel
            // doesn't lag).
            result = async {
                match audio_rx.as_mut() {
                    Some(rx) => rx.recv().await,
                    None => std::future::pending().await,
                }
            } => {
                match result {
                    Ok(packet) if streaming_started => pump.audio(&webrtc_manager, &packet).await,
                    Ok(_) => {} // pre-DTLS audio: drained and dropped
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        debug!("WebRTC session {} audio lagged {} packets", session_id, n);
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        info!("Audio source closed for WebRTC session {}", session_id);
                        audio_rx = None;
                    }
                }
            }

            // Reader health: detect a dead socket reader so the session ends
            // instead of waiting forever on a broadcast channel that never
            // closes. A `Reconnecting` transition is transient (the reader
            // retries on its own) and is only logged; `Err` means the reader
            // task is gone for good.
            result = reader_health_rx.changed() => {
                match result {
                    Ok(()) => {
                        let health = *reader_health_rx.borrow_and_update();
                        debug!(
                            "WebRTC session {} reader health for monitor {}: {:?}",
                            session_id, monitor_id, health
                        );
                    }
                    Err(_) => {
                        error!(
                            "Reader task exited for monitor {}, ending WebRTC session {}",
                            monitor_id, session_id
                        );
                        let error_msg = WebRtcSignalingMessage::Error {
                            message: "Video source stopped".to_string(),
                        };
                        if let Ok(json) = serde_json::to_string(&error_msg) {
                            let _ = ws_sender.send(Message::Text(json.into())).await;
                        }
                        break;
                    }
                }
            }
        }
    }

    // Cleanup
    let _ = webrtc_manager.remove_session(&session_id).await;
    info!(
        "WebRTC signaling handler finished for monitor {}",
        monitor_id
    );
}

// ============================================================================
// Monitor Snapshot
// ============================================================================

/// Get a JPEG snapshot from a live monitor
#[utoipa::path(
    get,
    path = "/api/v3/monitors/{monitor_id}/snapshot",
    operation_id = "getMonitorSnapshot",
    tag = "Live Streaming",
    params(
        ("monitor_id" = u32, Path, description = "Monitor/Camera ID")
    ),
    responses(
        (status = 200, description = "JPEG snapshot image", content_type = "image/jpeg"),
        (status = 404, description = "Monitor not found", body = AppResponseError),
        (status = 503, description = "Service unavailable", body = AppResponseError)
    ),
    security(("jwt" = []))
)]
pub async fn get_monitor_snapshot(
    State(state): State<AppState>,
    Path(monitor_id): Path<u32>,
) -> AppResult<Response> {
    let service = state.snapshot_service.as_ref().ok_or_else(|| {
        AppError::ServiceUnavailableError("Snapshot service not configured".to_string())
    })?;

    let jpeg = service.get_snapshot(monitor_id).await.map_err(|e| {
        use crate::streaming::snapshot::SnapshotError;
        match e {
            SnapshotError::SourceNotAvailable(id) | SnapshotError::KeyframeTimeout(id) => {
                AppError::NotFoundError(crate::error::Resource {
                    resource_type: crate::error::ResourceType::Monitor,
                    details: vec![
                        ("monitor_id".to_string(), id.to_string()),
                        ("reason".to_string(), e.to_string()),
                    ],
                })
            }
            _ => AppError::InternalServerError(e.to_string()),
        }
    })?;

    // Match what ZoneMinder's own image view serves: the still comes back
    // already rotated, so a client can size and render it directly (GH #61).
    // Live video is deliberately left alone — rotating that would mean
    // re-encoding, and a client can do it in CSS for free.
    let orientation =
        crate::handlers::events_playback::monitor_orientation(&state, monitor_id).await;
    let jpeg = crate::service::image_orientation::orient_jpeg(jpeg, orientation).await;

    Ok(Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "image/jpeg")
        .header(header::CACHE_CONTROL, "no-cache, no-store")
        .body(Body::from(jpeg))
        .unwrap())
}

/// Take a snapshot now on a zm-next monitor
///
/// Asks the monitor's zm-next worker to encode the current frame (its
/// `store_snapshot` plugin) and returns the JPEG. Unlike `GET`, which decodes
/// the last cached keyframe here, this is a fresh frame from the pipeline, and
/// the file is also kept wherever the plugin's `root` points. The worker's
/// report comes back in `X-Snapshot-*` headers.
#[utoipa::path(
    post,
    path = "/api/v3/monitors/{monitor_id}/snapshot",
    operation_id = "takeMonitorSnapshot",
    tag = "Live Streaming",
    params(
        ("monitor_id" = u32, Path, description = "Monitor/Camera ID"),
        crate::dto::request::monitor_ondemand::SnapshotNowQuery
    ),
    responses(
        (status = 200, description = "JPEG of the current frame", content_type = "image/jpeg"),
        (status = 404, description = "Monitor not found", body = AppResponseError),
        (status = 409, description = "Monitor is not running on zm-next", body = AppResponseError),
        (status = 503, description = "Worker unreachable, no store_snapshot in the pipeline, or timed out", body = AppResponseError)
    ),
    security(("jwt" = []))
)]
pub async fn take_monitor_snapshot(
    State(state): State<AppState>,
    Path(monitor_id): Path<u32>,
    Query(query): Query<crate::dto::request::monitor_ondemand::SnapshotNowQuery>,
    scope: crate::service::monitor_acl::MonitorScope,
) -> AppResult<Response> {
    let snap =
        crate::service::zmnext::ondemand::snapshot(&state, monitor_id, &scope, query.stream_id)
            .await?;
    let orientation =
        crate::handlers::events_playback::monitor_orientation(&state, monitor_id).await;
    let jpeg = crate::service::image_orientation::orient_jpeg(snap.jpeg, orientation).await;

    let mut builder = Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "image/jpeg")
        .header(header::CACHE_CONTROL, "no-cache, no-store");
    if let Ok(v) = header::HeaderValue::from_str(&snap.path) {
        builder = builder.header("X-Snapshot-Path", v);
    }
    for (name, value) in [
        ("X-Snapshot-Width", snap.width.map(i64::from)),
        ("X-Snapshot-Height", snap.height.map(i64::from)),
        ("X-Snapshot-Pts-Usec", snap.pts_usec),
    ] {
        if let Some(v) = value {
            builder = builder.header(name, v);
        }
    }
    Ok(builder.body(Body::from(jpeg)).unwrap())
}

/// Describe the scene now on a zm-next monitor
///
/// Asks the monitor's zm-next worker to run its `describe_vlm` plugin on the
/// current frame, optionally with a one-off prompt, and returns the text. The
/// description is not stored as an event.
#[utoipa::path(
    post,
    path = "/api/v3/monitors/{monitor_id}/describe",
    operation_id = "describeMonitor",
    tag = "Live Streaming",
    params(("monitor_id" = u32, Path, description = "Monitor/Camera ID")),
    request_body(content = crate::dto::request::monitor_ondemand::DescribeNowRequest, description = "Optional prompt and stream", content_type = "application/json"),
    responses(
        (status = 200, description = "Scene description", body = crate::dto::response::monitor_ondemand::DescribeNowResponse),
        (status = 400, description = "Invalid request", body = AppResponseError),
        (status = 404, description = "Monitor not found", body = AppResponseError),
        (status = 409, description = "Monitor is not running on zm-next", body = AppResponseError),
        (status = 503, description = "Worker unreachable, no describe_vlm in the pipeline, VLM failed, or timed out", body = AppResponseError)
    ),
    security(("jwt" = []))
)]
pub async fn describe_monitor(
    State(state): State<AppState>,
    Path(monitor_id): Path<u32>,
    scope: crate::service::monitor_acl::MonitorScope,
    body: Option<Json<crate::dto::request::monitor_ondemand::DescribeNowRequest>>,
) -> AppResult<Json<crate::dto::response::monitor_ondemand::DescribeNowResponse>> {
    use garde::Validate;
    let req = body.map(|Json(r)| r).unwrap_or_default();
    req.validate().map_err(AppError::InvalidInputError)?;
    Ok(Json(
        crate::service::zmnext::ondemand::describe(
            &state,
            monitor_id,
            &scope,
            req.prompt,
            req.stream_id,
        )
        .await?,
    ))
}

/// Stream a monitor's status changes (server-sent events)
///
/// The first event, `status`, is the current state as a `WorkerStatus`. After
/// that, each event is named by what changed (`hello`, `worker_state`,
/// `stream_auth_failed`, `worker_degraded`, `connection_failed`,
/// `connection_restored`, `capture_failed`, `capture_resumed`, `disconnected`)
/// and carries a `StatusChange`. Accepts `?token=` for `EventSource`.
#[utoipa::path(
    get,
    path = "/api/v3/monitors/{monitor_id}/events",
    operation_id = "streamMonitorEvents",
    tag = "Live Streaming",
    params(("monitor_id" = u32, Path, description = "Monitor/Camera ID")),
    responses(
        (status = 200, description = "text/event-stream of status changes", content_type = "text/event-stream"),
        (status = 401, description = "Unauthorized", body = AppResponseError),
        (status = 404, description = "Monitor not found", body = AppResponseError),
        (status = 503, description = "Live streaming disabled", body = AppResponseError)
    ),
    security(("jwt" = []))
)]
pub async fn stream_monitor_events(
    State(state): State<AppState>,
    Path(monitor_id): Path<u32>,
    scope: crate::service::monitor_acl::MonitorScope,
) -> AppResult<
    axum::response::sse::Sse<
        impl futures_util::Stream<Item = Result<axum::response::sse::Event, std::convert::Infallible>>,
    >,
> {
    use axum::response::sse::{Event, KeepAlive, Sse};
    crate::service::monitor::get_by_id(&state, monitor_id, &scope).await?;
    let router = state.source_router.clone().ok_or_else(|| {
        AppError::ServiceUnavailableError("live streaming is disabled".to_string())
    })?;
    // Subscribe before reading the snapshot so no change falls between them,
    // and make sure something is reading the monitor's socket.
    let rx = router.subscribe_worker_status();
    router.ensure_warm(monitor_id).await;
    let snapshot = router.worker_status(monitor_id).unwrap_or_default();

    let first = futures_util::stream::once(async move {
        Ok(Event::default()
            .event("status")
            .json_data(&snapshot)
            .unwrap_or_else(|_| Event::default().event("status")))
    });
    let changes = futures_util::stream::unfold(rx, move |mut rx| async move {
        loop {
            match rx.recv().await {
                Ok(change) if change.monitor_id == monitor_id => {
                    let event = Event::default()
                        .event(change.kind.clone())
                        .json_data(&change)
                        .unwrap_or_else(|_| Event::default().event(change.kind.clone()));
                    return Some((Ok(event), rx));
                }
                Ok(_) => continue,
                // A slow subscriber missed changes: say so, then carry on.
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    let event = Event::default().event("lagged").data(n.to_string());
                    return Some((Ok(event), rx));
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => return None,
            }
        }
    });
    Ok(Sse::new(futures_util::StreamExt::chain(first, changes)).keep_alive(KeepAlive::default()))
}

// ============================================================================
// Source Statistics
// ============================================================================

/// Get source router statistics
#[utoipa::path(
    get,
    path = "/api/v3/live/sources",
    operation_id = "getLiveSources",
    tag = "Live Streaming",
    responses(
        (status = 200, description = "Source statistics"),
        (status = 503, description = "Service unavailable", body = AppResponseError)
    ),
    security(
        ("jwt" = [])
    )
)]
pub async fn get_live_sources(
    State(state): State<AppState>,
    scope: crate::service::monitor_acl::MonitorScope,
) -> AppResult<Json<serde_json::Value>> {
    let coordinator = state.live_coordinator.as_ref().ok_or_else(|| {
        AppError::ServiceUnavailableError("Live streaming not configured".to_string())
    })?;

    let source_router = coordinator.source_router();
    let stats: Vec<_> = source_router
        .stats()
        .await
        .into_iter()
        .filter(|s| scope.allows(s.monitor_id, crate::util::authz::Level::View))
        .collect();

    // Count only what the caller can see, so the total doesn't reveal others.
    Ok(Json(serde_json::json!({
        "active_sources": stats.len(),
        "sources": stats
    })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn live_sessions_hide_monitors_outside_the_scope() {
        use crate::service::monitor_acl::MonitorScope;
        use crate::util::authz::Level;
        let scope = MonitorScope::Restricted(std::collections::HashMap::from([(3, Level::View)]));
        assert_eq!(visible_monitor_ids(&scope, vec![1, 3, 5]), vec![3]);
        assert_eq!(
            visible_monitor_ids(&MonitorScope::All, vec![1, 3]),
            vec![1, 3]
        );
    }

    #[test]
    fn test_parse_segment_sequence() {
        assert_eq!(parse_segment_sequence("segment_00001.m4s"), Some(1));
        assert_eq!(parse_segment_sequence("segment_00100.m4s"), Some(100));
        assert_eq!(parse_segment_sequence("segment_00001.0.m4s"), Some(1));
        assert_eq!(parse_segment_sequence("invalid.m4s"), None);
    }

    /// Plain numeric sequences with zero-padding are parsed to their integer value.
    #[test]
    fn parse_segment_sequence_valid_numbers() {
        assert_eq!(parse_segment_sequence("segment_0.m4s"), Some(0));
        assert_eq!(parse_segment_sequence("segment_00000.m4s"), Some(0));
        assert_eq!(parse_segment_sequence("segment_42.m4s"), Some(42));
        assert_eq!(parse_segment_sequence("segment_999999.m4s"), Some(999_999));
        // Maximum u64 still parses.
        assert_eq!(
            parse_segment_sequence("segment_18446744073709551615.m4s"),
            Some(u64::MAX)
        );
    }

    /// The leading `.`-delimited token before the suffix is what gets parsed,
    /// so a `segment_00001.0.m4s` style name yields the first component.
    #[test]
    fn parse_segment_sequence_accepts_dotted_part_suffix() {
        assert_eq!(parse_segment_sequence("segment_7.0.m4s"), Some(7));
        assert_eq!(parse_segment_sequence("segment_12.999.m4s"), Some(12));
    }

    /// Extra underscore-delimited tokens are allowed as long as `parts[0]` is
    /// `segment` and `parts[1]` is numeric (`parts.len() >= 2`).
    #[test]
    fn parse_segment_sequence_accepts_extra_underscore_tokens() {
        assert_eq!(parse_segment_sequence("segment_5_extra.m4s"), Some(5));
        assert_eq!(parse_segment_sequence("segment_5_a_b_c.m4s"), Some(5));
    }

    /// Inputs that do not match the `segment_<num>.m4s` shape return `None`
    /// rather than panicking.
    #[test]
    fn parse_segment_sequence_rejects_garbage() {
        // Wrong / missing suffix.
        assert_eq!(parse_segment_sequence("segment_00001"), None);
        assert_eq!(parse_segment_sequence("segment_00001.ts"), None);
        assert_eq!(parse_segment_sequence("segment_00001.mp4"), None);
        // Wrong prefix.
        assert_eq!(parse_segment_sequence("seg_00001.m4s"), None);
        assert_eq!(parse_segment_sequence("init_00001.m4s"), None);
        assert_eq!(parse_segment_sequence("Segment_00001.m4s"), None);
        // No underscore -> only one part.
        assert_eq!(parse_segment_sequence("segment.m4s"), None);
        // Non-numeric sequence component.
        assert_eq!(parse_segment_sequence("segment_abc.m4s"), None);
        assert_eq!(parse_segment_sequence("segment_-1.m4s"), None);
        assert_eq!(parse_segment_sequence("segment_1a.m4s"), None);
        // Empty sequence component.
        assert_eq!(parse_segment_sequence("segment_.m4s"), None);
        // Overflows u64 -> parse fails gracefully.
        assert_eq!(
            parse_segment_sequence("segment_99999999999999999999999.m4s"),
            None
        );
        // Empty / suffix-only input.
        assert_eq!(parse_segment_sequence(""), None);
        assert_eq!(parse_segment_sequence(".m4s"), None);
        assert_eq!(parse_segment_sequence("..m4s"), None);
    }

    /// `default_true` underpins the `enable_hls` serde default.
    #[test]
    fn default_true_is_true() {
        assert!(default_true());
    }

    #[test]
    fn test_start_live_request_default() {
        let request = StartLiveRequest::default();
        assert!(request.enable_hls);
        assert!(!request.enable_webrtc);
    }

    /// An empty JSON object applies the serde defaults: HLS on, WebRTC off.
    #[test]
    fn start_live_request_deserializes_empty_object() {
        let request: StartLiveRequest = serde_json::from_str("{}").expect("deserialize");
        assert!(request.enable_hls, "enable_hls defaults to true");
        assert!(!request.enable_webrtc);
    }

    /// Explicit fields override the defaults.
    #[test]
    fn start_live_request_deserializes_explicit_fields() {
        let request: StartLiveRequest =
            serde_json::from_str(r#"{"enable_hls": false, "enable_webrtc": true}"#)
                .expect("deserialize");
        assert!(!request.enable_hls);
        assert!(request.enable_webrtc);
    }

    /// The WebRTC signaling enum round-trips, including the renamed ICE fields.
    #[test]
    fn webrtc_signaling_message_round_trips() {
        let ice = WebRtcSignalingMessage::IceCandidate {
            session_id: "sess-1".to_string(),
            candidate: "candidate:0 1 UDP".to_string(),
            sdp_mid: Some("0".to_string()),
            sdp_mline_index: Some(0),
        };
        let json = serde_json::to_string(&ice).expect("serialize");
        assert!(json.contains("\"sdpMid\""));
        assert!(json.contains("\"sdpMLineIndex\""));
        match serde_json::from_str::<WebRtcSignalingMessage>(&json).expect("deserialize") {
            WebRtcSignalingMessage::IceCandidate {
                session_id,
                sdp_mline_index,
                ..
            } => {
                assert_eq!(session_id, "sess-1");
                assert_eq!(sdp_mline_index, Some(0));
            }
            other => panic!("expected IceCandidate, got {other:?}"),
        }
    }
}
