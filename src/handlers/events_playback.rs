//! Event playback HTTP handlers
//!
//! Provides endpoints for playing back recorded events via HLS or direct video access.
//! Supports byte-range requests for efficient seeking in fragmented MP4 files.

use axum::{
    body::Body,
    extract::{Path, State},
    http::{header, HeaderMap, StatusCode},
    response::Response,
    Json,
};
use serde::{Deserialize, Serialize};
use std::path::{Path as StdPath, PathBuf};
use tokio::fs::File;
use tokio::io::AsyncReadExt;
use tokio_util::io::ReaderStream;
use tracing::{debug, warn};

use crate::entity::events::Model as EventModel;
use crate::error::{AppError, AppResponseError, AppResult, Resource, ResourceType};
use crate::repo;
use crate::server::state::AppState;
use crate::service::event_storage::{build_event_directory_path, resolve_event_storage_path};
use crate::service::monitor_acl::MonitorScope;
use crate::util::authz::Level;

// ============================================================================
// DTOs
// ============================================================================

/// Path parameters for event playback
#[derive(Debug, Deserialize)]
pub struct EventPlaybackPath {
    pub id: u64,
}

/// Path parameters for an event HLS-VOD media segment.
#[derive(Debug, Deserialize)]
pub struct EventSegmentPath {
    pub id: u64,
    /// `N.m4s` as the playlist lists it, or a bare `N` (earlier playlists).
    pub seq: String,
}

/// The segment index from `N.m4s` or `N`.
fn segment_index(seq: &str) -> Option<usize> {
    seq.strip_suffix(".m4s").unwrap_or(seq).parse().ok()
}

/// Response for event video metadata
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct EventVideoInfo {
    pub event_id: u64,
    /// Detected video codec: "H264", "H265" or "Unknown".
    pub video_codec: String,
    pub width: u32,
    pub height: u32,
    pub duration_seconds: f64,
    pub file_size: u64,
    /// True when the codec plays in any browser `<video>` (H.264). HEVC is
    /// Safari / hardware-Chrome only.
    pub playable_direct: bool,
    /// Suggested playback endpoint: "direct" (progressive MP4), "hls" (VOD), or
    /// "event" (in-progress live HLS via `playlist.m3u8`).
    pub recommended_mode: String,
    /// True while the event is still recording (no `EndDateTime`). Codec/size
    /// fields may be unknown until it finalizes; play via `playlist.m3u8`.
    pub in_progress: bool,
}

// ============================================================================
// Helpers
// ============================================================================

/// True iff `name` is safe to join onto the event directory: a single,
/// non-traversing filename. `Events.default_video` is DB-controlled, and a
/// malformed value like `"../../../etc/passwd"` would otherwise escape the
/// event directory via `PathBuf::join`.
fn is_safe_event_filename(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && !name.contains('/')
        && !name.contains('\\')
        && !name.contains('\0')
}

/// True iff `name` looks like a playable video container we can serve / probe.
/// ZoneMinder records HLS-mode monitors with `DefaultVideo = index.m3u8` (a
/// playlist, not media); accepting it would make the API hand clients a
/// playlist in place of the video and make ffmpeg probing fail.
fn is_video_container(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    [".mp4", ".mkv", ".mov", ".m4v", ".webm", ".avi"]
        .iter()
        .any(|ext| lower.ends_with(ext))
}

/// Pick the filename to look for under the event directory. Falls back to the
/// canonical `{event_id}-video.mp4` name when `default_video` is empty, fails
/// the safety check, or isn't a real video container (e.g. ZoneMinder's
/// `index.m3u8` for HLS-recorded events). The fallback name lets the
/// alternative-name search in [`get_event_video_path`] locate the real
/// `{event_id}-video.h264.mp4` on disk.
pub(crate) fn select_video_filename(event_id: u64, default_video: &str) -> String {
    if default_video.is_empty() {
        return format!("{}-video.mp4", event_id);
    }
    if !is_safe_event_filename(default_video) {
        warn!(
            "Refusing unsafe default_video for event {}: {:?}; using default name",
            event_id, default_video
        );
        return format!("{}-video.mp4", event_id);
    }
    if !is_video_container(default_video) {
        warn!(
            "default_video {:?} for event {} is not a playable container \
             (likely an HLS playlist); using default name",
            default_video, event_id
        );
        return format!("{}-video.mp4", event_id);
    }
    default_video.to_string()
}

/// Get the video file path for an event
///
/// ZoneMinder supports multiple storage schemes:
/// - **Deep**: `{storage_path}/{MonitorId}/{YY/MM/DD/HH/MM/SS}/{video_file}`
/// - **Medium**: `{storage_path}/{MonitorId}/{YYYY-MM-DD}/{EventId}/{video_file}`
/// - **Shallow**: `{storage_path}/{MonitorId}/{EventId}/{video_file}`
async fn get_event_video_path(
    state: &AppState,
    event_id: u64,
    scope: &MonitorScope,
) -> AppResult<PathBuf> {
    // Get event from database (using repo to get raw entity)
    let event = get_event_entity(state, event_id, scope).await?;

    // Get the video filename from the event (e.g., "467-video.h264.mp4"),
    // rejecting traversal attempts in the DB-supplied default_video field.
    let video_filename = select_video_filename(event_id, &event.default_video);

    let storage_path = resolve_event_storage_path(state, &event).await?;

    // Build event directory path based on scheme
    let event_dir = build_event_directory_path(
        &storage_path,
        event.monitor_id,
        event_id,
        event.start_date_time,
        &event.scheme,
    );

    // Try the computed path
    let video_path = event_dir.join(&video_filename);
    debug!("Looking for video at: {:?}", video_path);

    if tokio::fs::metadata(&video_path).await.is_ok() {
        return Ok(video_path);
    }

    // Try alternative video filenames if the default doesn't exist
    let alternative_names = [
        format!("{}-video.mp4", event_id),
        format!("{}-video.h264.mp4", event_id),
        format!("{}.mp4", event_id),
    ];

    for alt_name in &alternative_names {
        let alt_path = event_dir.join(alt_name);
        if tokio::fs::metadata(&alt_path).await.is_ok() {
            debug!("Found video at alternative path: {:?}", alt_path);
            return Ok(alt_path);
        }
    }

    // Log what we tried for debugging
    warn!(
        "Video not found for event {}. Tried directory: {:?}, filename: {}, alternatives: {:?}",
        event_id, event_dir, video_filename, alternative_names
    );

    Err(AppError::NotFoundError(Resource {
        resource_type: ResourceType::Event,
        details: vec![
            ("event_id".to_string(), event_id.to_string()),
            ("reason".to_string(), "Video file not found".to_string()),
            ("tried_path".to_string(), event_dir.display().to_string()),
        ],
    }))
}

/// Helper to get raw event entity from database, enforcing row-level ACL.
///
/// Every playback handler resolves its event through here, so the scope check
/// below is the single choke point that keeps a monitor-restricted caller from
/// streaming, downloading, or thumbnailing events belonging to monitors outside
/// their allowlist. Out-of-scope events return the same `NotFound` as a missing
/// event so the API doesn't leak which event ids exist (REVIEW_FIXES_PLAN §1.3).
/// The monitor's configured `Orientation`, or `ROTATE_0` if it cannot be read.
///
/// Defaulting rather than failing is deliberate: a still that is served
/// unrotated is a cosmetic problem, whereas failing the request over a lookup
/// would take the thumbnail away entirely.
pub(crate) async fn monitor_orientation(
    state: &AppState,
    monitor_id: u32,
) -> crate::entity::sea_orm_active_enums::Orientation {
    use crate::entity::sea_orm_active_enums::Orientation;
    match repo::monitors::find_by_id(state.db(), monitor_id).await {
        Ok(Some(monitor)) => monitor.orientation,
        Ok(None) => Orientation::Rotate0,
        Err(e) => {
            warn!("could not read orientation for monitor {monitor_id}: {e}");
            Orientation::Rotate0
        }
    }
}

pub(crate) async fn get_event_entity(
    state: &AppState,
    event_id: u64,
    scope: &MonitorScope,
) -> AppResult<EventModel> {
    let event = repo::events::find_by_id(state, event_id)
        .await?
        .ok_or_else(|| not_found_event(event_id))?;
    if !scope.allows(event.monitor_id, Level::View) {
        return Err(not_found_event(event_id));
    }
    Ok(event)
}

/// The canonical `NotFound` for an event id — used for both genuinely missing
/// events and ones hidden by row-level ACL (identical responses avoid an
/// existence oracle).
pub(crate) fn not_found_event(event_id: u64) -> AppError {
    AppError::NotFoundError(Resource {
        resource_type: ResourceType::Event,
        details: vec![("event_id".to_string(), event_id.to_string())],
    })
}

/// True iff the event is still recording. ZoneMinder sets `EndDateTime` on
/// close, so a NULL end means the event is in progress — there is no finalized
/// `{id}-video.*.mp4` yet, only a growing `incomplete.*.mp4` plus ZoneMinder's
/// live `index.m3u8`.
pub(crate) fn event_is_in_progress(event: &EventModel) -> bool {
    event.end_date_time.is_none()
}

/// The on-disk directory for an event, resolving storage + scheme.
async fn event_directory(
    state: &AppState,
    event: &EventModel,
    event_id: u64,
) -> AppResult<PathBuf> {
    let storage_path = resolve_event_storage_path(state, event).await?;
    Ok(build_event_directory_path(
        &storage_path,
        event.monitor_id,
        event_id,
        event.start_date_time,
        &event.scheme,
    ))
}

/// Locate the growing `incomplete.*.mp4` file an in-progress event records into.
pub(crate) async fn find_incomplete_media(dir: &StdPath) -> Option<PathBuf> {
    let mut rd = tokio::fs::read_dir(dir).await.ok()?;
    while let Ok(Some(entry)) = rd.next_entry().await {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with("incomplete.") && name.ends_with(".mp4") {
            return Some(entry.path());
        }
    }
    None
}

/// Resolve the media file to byte-serve for an event: the growing
/// `incomplete.*.mp4` while recording, else the finalized recorded video.
async fn get_event_media_path(
    state: &AppState,
    event_id: u64,
    scope: &MonitorScope,
) -> AppResult<PathBuf> {
    let event = get_event_entity(state, event_id, scope).await?;
    if event_is_in_progress(&event) {
        let dir = event_directory(state, &event, event_id).await?;
        if let Some(p) = find_incomplete_media(&dir).await {
            return Ok(p);
        }
    }
    get_event_video_path(state, event_id, scope).await
}

/// Rewrite ZoneMinder's in-progress `index.m3u8` so its init/segment URIs point
/// at our native Range-served media route (`media_uri`) instead of the legacy
/// `index.php?view=view_video…` PHP path. The `#EXT-X-BYTERANGE` offsets, the
/// `#EXT-X-PLAYLIST-TYPE:EVENT` tag, and the absence of `#EXT-X-ENDLIST` (still
/// recording) are preserved verbatim — they index the same growing file we
/// serve, so a player resolves `media_uri` + each byte range against it.
fn rewrite_zm_event_playlist(zm_m3u8: &str, media_uri: &str) -> String {
    let mut out = String::with_capacity(zm_m3u8.len());
    for line in zm_m3u8.lines() {
        if line.starts_with("#EXT-X-MAP:") {
            out.push_str(&replace_map_uri(line, media_uri));
        } else if !line.is_empty() && !line.starts_with('#') {
            // A bare URI line is a segment reference — swap the index.php target.
            out.push_str(media_uri);
        } else {
            out.push_str(line);
        }
        out.push('\n');
    }
    out
}

/// Replace the `URI="…"` value inside an `#EXT-X-MAP` line, leaving the rest of
/// the line (notably `BYTERANGE="…"`) untouched.
fn replace_map_uri(line: &str, media_uri: &str) -> String {
    const KEY: &str = "URI=\"";
    if let Some(start) = line.find(KEY) {
        let val_start = start + KEY.len();
        if let Some(rel_end) = line[val_start..].find('"') {
            let val_end = val_start + rel_end;
            return format!("{}{}{}", &line[..val_start], media_uri, &line[val_end..]);
        }
    }
    line.to_string()
}

/// Parse HTTP Range header
fn parse_range_header(range_header: Option<&str>, file_size: u64) -> Option<(u64, u64)> {
    let range_str = range_header?;

    if !range_str.starts_with("bytes=") {
        return None;
    }

    // A zero-length file has no satisfiable byte range; bail before the
    // `file_size - 1` arithmetic below, which would otherwise underflow.
    if file_size == 0 {
        return None;
    }

    let range_spec = &range_str[6..];
    let parts: Vec<&str> = range_spec.split('-').collect();

    if parts.len() != 2 {
        return None;
    }

    let (start, end) = if parts[0].is_empty() {
        // Suffix range: -500 means last 500 bytes
        let suffix_len: u64 = parts[1].parse().ok()?;
        let start = file_size.saturating_sub(suffix_len);
        (start, file_size - 1)
    } else {
        let start: u64 = parts[0].parse().ok()?;
        // A last-pos at or past the end means "to the end" (RFC 9110
        // §14.1.2), as players send for a file of unknown length.
        let end = if parts[1].is_empty() {
            file_size - 1
        } else {
            parts[1].parse::<u64>().ok()?.min(file_size - 1)
        };
        (start, end)
    };

    if start <= end && end < file_size {
        Some((start, end))
    } else {
        None
    }
}

// ============================================================================
// Handlers
// ============================================================================

/// Get HLS playlist for event playback
///
/// Generates an HLS playlist with byte-range segments from a fragmented MP4 file.
/// This enables efficient seeking without having separate segment files.
#[utoipa::path(
    get,
    path = "/api/v3/events/{id}/stream/playlist.m3u8",
    operation_id = "getEventPlaylist",
    tag = "Event Playback",
    params(
        ("id" = u64, Path, description = "Event ID")
    ),
    responses(
        (status = 200, description = "HLS playlist", content_type = "application/vnd.apple.mpegurl"),
        (status = 404, description = "Event not found", body = AppResponseError),
        (status = 500, description = "Internal server error", body = AppResponseError)
    ),
    security(
        ("jwt" = [])
    )
)]
pub async fn get_event_playlist(
    State(state): State<AppState>,
    Path(path): Path<EventPlaybackPath>,
    scope: MonitorScope,
    grant: Option<axum::Extension<crate::service::media_grant::MediaGrant>>,
    uri: axum::http::Uri,
) -> Result<Response, AppError> {
    let event = get_event_entity(&state, path.id, &scope).await?;

    // Fetched with a URL credential: sign the listed URIs so players that
    // can't send a header can fetch them (#153).
    let signer = crate::service::media_grant::uri_signer(
        grant.as_deref(),
        &uri,
        crate::util::media_url::MediaResource::Event {
            event_id: path.id,
            monitor_id: event.monitor_id,
        },
        crate::util::media_url::RECORDING_TTL,
    );
    let sign = |p: String| match &signer {
        Some(s) => crate::streaming::hls::signed_playlist::sign_playlist(&p, s),
        None => p,
    };

    // In-progress events have no finalized file to package as VOD. ZoneMinder
    // already maintains a growing `#EXT-X-PLAYLIST-TYPE:EVENT` playlist
    // (index.m3u8) of byte-range segments into the recording; re-expose it with
    // native URIs instead of 404ing until the event closes.
    if event_is_in_progress(&event) {
        debug!("Getting in-progress (EVENT) playlist for event {}", path.id);
        let dir = event_directory(&state, &event, path.id).await?;
        let zm_m3u8 = tokio::fs::read_to_string(dir.join("index.m3u8"))
            .await
            .map_err(|_| {
                AppError::NotFoundError(Resource {
                    resource_type: ResourceType::Event,
                    details: vec![
                        ("event_id".to_string(), path.id.to_string()),
                        (
                            "reason".to_string(),
                            "in-progress playlist not available yet".to_string(),
                        ),
                    ],
                })
            })?;
        let body = sign(rewrite_zm_event_playlist(&zm_m3u8, "media.mp4"));
        return Ok(Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "application/vnd.apple.mpegurl")
            .header(header::CACHE_CONTROL, "no-cache")
            .body(Body::from(body))
            .unwrap());
    }

    debug!("Getting HLS-VOD playlist for event {}", path.id);
    let assets = event_vod_assets(&state, path.id, &scope).await?;
    Ok(Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/vnd.apple.mpegurl")
        .header(header::CACHE_CONTROL, "no-cache")
        .body(Body::from(sign(assets.playlist.clone())))
        .unwrap())
}

/// Probe + package (cached) the HLS-VOD assets for an event.
async fn event_vod_assets(
    state: &AppState,
    id: u64,
    scope: &MonitorScope,
) -> Result<std::sync::Arc<crate::streaming::hls::vod::VodAssets>, AppError> {
    let video_path = get_event_video_path(state, id, scope).await?;
    let info = crate::streaming::probe::probe_event_media(id, video_path.clone())
        .await
        .map_err(AppError::InternalServerError)?;
    crate::streaming::hls::vod::get_or_build(id, video_path, info)
        .await
        .map_err(AppError::InternalServerError)
}

/// Get the fMP4 initialization segment for an event's HLS-VOD stream.
#[utoipa::path(
    get,
    path = "/api/v3/events/{id}/stream/init.mp4",
    params(("id" = u64, Path, description = "Event ID")),
    responses(
        (status = 200, description = "fMP4 init segment", content_type = "video/mp4"),
        (status = 404, description = "Event not found", body = AppResponseError)
    ),
    tag = "Event Playback",
    security(("jwt" = []))
)]
pub async fn get_event_init(
    State(state): State<AppState>,
    Path(path): Path<EventPlaybackPath>,
    scope: MonitorScope,
) -> Result<Response, AppError> {
    let assets = event_vod_assets(&state, path.id, &scope).await?;
    Ok(Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "video/mp4")
        .header(header::CACHE_CONTROL, "max-age=31536000, immutable")
        .body(Body::from(assets.init.clone()))
        .unwrap())
}

/// Get an fMP4 media segment for an event's HLS-VOD stream.
#[utoipa::path(
    get,
    path = "/api/v3/events/{id}/stream/segment/{seq}",
    params(
        ("id" = u64, Path, description = "Event ID"),
        ("seq" = String, Path, description = "Zero-based segment index with its `.m4s` extension, as the playlist lists it; a bare index is also accepted")
    ),
    responses(
        (status = 200, description = "fMP4 media segment", content_type = "video/mp4"),
        (status = 404, description = "Event or segment not found", body = AppResponseError)
    ),
    tag = "Event Playback",
    security(("jwt" = []))
)]
pub async fn get_event_segment(
    State(state): State<AppState>,
    Path(path): Path<EventSegmentPath>,
    scope: MonitorScope,
) -> Result<Response, AppError> {
    let index = segment_index(&path.seq)
        .ok_or_else(|| AppError::BadRequestError(format!("invalid segment `{}`", path.seq)))?;
    let assets = event_vod_assets(&state, path.id, &scope).await?;
    let segment = assets.segments.get(index).ok_or_else(|| {
        AppError::NotFoundError(crate::error::Resource {
            resource_type: crate::error::ResourceType::Event,
            details: vec![
                ("event_id".to_string(), path.id.to_string()),
                ("segment".to_string(), path.seq.to_string()),
            ],
        })
    })?;
    Ok(Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "video/mp4")
        .header(header::CACHE_CONTROL, "max-age=31536000, immutable")
        .body(Body::from(segment.clone()))
        .unwrap())
}

/// Get event video file with Range support
///
/// Serves the event video file with HTTP Range support for efficient seeking.
#[utoipa::path(
    get,
    path = "/api/v3/events/{id}/video",
    operation_id = "getEventVideo",
    tag = "Event Playback",
    params(
        ("id" = u64, Path, description = "Event ID")
    ),
    responses(
        (status = 200, description = "Video file (full or partial)", content_type = "video/mp4"),
        (status = 206, description = "Partial content", content_type = "video/mp4"),
        (status = 404, description = "Event not found", body = AppResponseError),
        (status = 416, description = "Range not satisfiable", body = AppResponseError),
        (status = 500, description = "Internal server error", body = AppResponseError)
    ),
    security(
        ("jwt" = [])
    )
)]
pub async fn get_event_video(
    State(state): State<AppState>,
    Path(path): Path<EventPlaybackPath>,
    scope: MonitorScope,
    headers: HeaderMap,
) -> Result<Response, AppError> {
    debug!("Getting video for event {}", path.id);
    let video_path = get_event_video_path(&state, path.id, &scope).await?;
    serve_file_with_range(&video_path, &headers).await
}

/// Serve a file as `video/mp4` with HTTP Range support — 200 for a full request,
/// 206 for a byte range. Streams from disk so multi-GB recordings (and the
/// growing in-progress file) don't buffer into memory.
async fn serve_file_with_range(path: &StdPath, headers: &HeaderMap) -> Result<Response, AppError> {
    let metadata = tokio::fs::metadata(path).await.map_err(|e| {
        AppError::InternalServerError(format!("Failed to read video metadata: {}", e))
    })?;
    let file_size = metadata.len();

    let range_header = headers.get(header::RANGE).and_then(|v| v.to_str().ok());

    if let Some(range_str) = range_header {
        if let Some((start, end)) = parse_range_header(Some(range_str), file_size) {
            let length = end - start + 1;

            let mut file = File::open(path).await.map_err(|e| {
                AppError::InternalServerError(format!("Failed to open video: {}", e))
            })?;

            use tokio::io::AsyncSeekExt;
            file.seek(std::io::SeekFrom::Start(start))
                .await
                .map_err(|e| AppError::InternalServerError(format!("Failed to seek: {}", e)))?;

            // Stream exactly `length` bytes from the seek point rather than
            // buffering the whole range into memory.
            let stream = ReaderStream::new(file.take(length));

            return Ok(Response::builder()
                .status(StatusCode::PARTIAL_CONTENT)
                .header(header::CONTENT_TYPE, "video/mp4")
                .header(header::ACCEPT_RANGES, "bytes")
                .header(
                    header::CONTENT_RANGE,
                    format!("bytes {}-{}/{}", start, end, file_size),
                )
                .header(header::CONTENT_LENGTH, length.to_string())
                .body(Body::from_stream(stream))
                .unwrap());
        } else if range_str.starts_with("bytes=") {
            // Nothing in the file satisfies it: 416 with the length, so the
            // client can ask again (RFC 9110 §15.5.17).
            let body = crate::error::AppResponseError::new(
                "RANGE_NOT_SATISFIABLE",
                format!("Range {range_str:?} is outside the {file_size}-byte file"),
                None,
                vec![],
            );
            return Ok(axum::response::IntoResponse::into_response((
                StatusCode::RANGE_NOT_SATISFIABLE,
                [(header::CONTENT_RANGE, format!("bytes */{file_size}"))],
                axum::Json(body),
            )));
        }
        // Another range unit: ignore the header and send the whole file.
    }

    // No range request - stream the full file from disk rather than reading it
    // entirely into memory (events can be multiple GB).
    let file = File::open(path)
        .await
        .map_err(|e| AppError::InternalServerError(format!("Failed to open video: {}", e)))?;
    let stream = ReaderStream::new(file);

    Ok(Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "video/mp4")
        .header(header::ACCEPT_RANGES, "bytes")
        .header(header::CONTENT_LENGTH, file_size.to_string())
        .body(Body::from_stream(stream))
        .unwrap())
}

/// Byte-serve an event's media file for the in-progress HLS playlist's
/// `#EXT-X-BYTERANGE` segments — the growing `incomplete.*.mp4` while recording,
/// or the finalized file once closed. Range support is what makes the rewritten
/// `media.mp4` URIs resolve.
#[utoipa::path(
    get,
    path = "/api/v3/events/{id}/stream/media.mp4",
    operation_id = "getEventStreamMedia",
    tag = "Event Playback",
    params(("id" = u64, Path, description = "Event ID")),
    responses(
        (status = 200, description = "Media file (full)", content_type = "video/mp4"),
        (status = 206, description = "Partial content", content_type = "video/mp4"),
        (status = 404, description = "Event not found", body = AppResponseError),
        (status = 416, description = "Range not satisfiable", body = AppResponseError)
    ),
    security(("jwt" = []))
)]
pub async fn get_event_stream_media(
    State(state): State<AppState>,
    Path(path): Path<EventPlaybackPath>,
    scope: MonitorScope,
    headers: HeaderMap,
) -> Result<Response, AppError> {
    debug!("Getting media (range) for event {}", path.id);
    let media_path = get_event_media_path(&state, path.id, &scope).await?;
    serve_file_with_range(&media_path, &headers).await
}

/// Get event video metadata (codec, dimensions, duration).
///
/// Probes the recorded file with the ffmpeg libraries so the client can pick a
/// playback path: H.264 plays in any browser; HEVC is Safari / hardware-Chrome
/// only (`playable_direct` / `recommended_mode` reflect this).
#[utoipa::path(
    get,
    path = "/api/v3/events/{id}/info",
    params(("id" = u64, Path, description = "Event ID")),
    responses(
        (status = 200, description = "Event video metadata", body = EventVideoInfo),
        (status = 404, description = "Event or video not found", body = AppResponseError)
    ),
    tag = "Events Playback",
    summary = "Get recorded event video metadata.",
    description = "- Requires a valid JWT (header or `?token=`).",
    security(("jwt" = []))
)]
pub async fn get_event_info(
    State(state): State<AppState>,
    Path(path): Path<EventPlaybackPath>,
    scope: MonitorScope,
) -> Result<Json<EventVideoInfo>, AppError> {
    debug!("Getting media info for event {}", path.id);

    let event = get_event_entity(&state, path.id, &scope).await?;

    // While recording there is no finalized file to probe (the growing MP4 has
    // no moov atom yet), so codec/dimensions are unknown. Return 200 with
    // `in_progress: true` and `recommended_mode: "event"` so the client can pick
    // the in-progress HLS path instead of seeing a 404.
    if event_is_in_progress(&event) {
        let file_size = match event_directory(&state, &event, path.id).await {
            Ok(dir) => match find_incomplete_media(&dir).await {
                Some(p) => tokio::fs::metadata(&p).await.map(|m| m.len()).unwrap_or(0),
                None => 0,
            },
            Err(_) => 0,
        };
        return Ok(Json(EventVideoInfo {
            event_id: path.id,
            video_codec: "Unknown".to_string(),
            width: 0,
            height: 0,
            duration_seconds: 0.0,
            file_size,
            playable_direct: false,
            recommended_mode: "event".to_string(),
            in_progress: true,
        }));
    }

    let video_path = get_event_video_path(&state, path.id, &scope).await?;
    let file_size = tokio::fs::metadata(&video_path)
        .await
        .map(|m| m.len())
        .unwrap_or(0);

    let info = crate::streaming::probe::probe_event_media(path.id, video_path)
        .await
        .map_err(AppError::InternalServerError)?;

    let playable_direct = info.playable_direct();
    Ok(Json(EventVideoInfo {
        event_id: path.id,
        video_codec: info.codec.as_str().to_string(),
        width: info.width,
        height: info.height,
        duration_seconds: info.duration_seconds,
        file_size,
        playable_direct,
        recommended_mode: if playable_direct { "direct" } else { "hls" }.to_string(),
        in_progress: false,
    }))
}

/// Get event video for HLS streaming (same as video endpoint but different path)
#[utoipa::path(
    get,
    path = "/api/v3/events/{id}/stream/video.mp4",
    operation_id = "getEventStreamVideo",
    tag = "Event Playback",
    params(
        ("id" = u64, Path, description = "Event ID")
    ),
    responses(
        (status = 200, description = "Video file (full or partial)", content_type = "video/mp4"),
        (status = 206, description = "Partial content", content_type = "video/mp4"),
        (status = 404, description = "Event not found", body = AppResponseError),
        (status = 500, description = "Internal server error", body = AppResponseError)
    ),
    security(
        ("jwt" = [])
    )
)]
pub async fn get_event_stream_video(
    state: State<AppState>,
    path: Path<EventPlaybackPath>,
    scope: MonitorScope,
    headers: HeaderMap,
) -> Result<Response, AppError> {
    // Delegate to the main video endpoint
    get_event_video(state, path, scope, headers).await
}

/// Get event thumbnail
#[utoipa::path(
    get,
    path = "/api/v3/events/{id}/thumbnail",
    operation_id = "getEventThumbnail",
    tag = "Event Playback",
    params(
        ("id" = u64, Path, description = "Event ID")
    ),
    responses(
        (status = 200, description = "Thumbnail image", content_type = "image/jpeg"),
        (status = 404, description = "Event or thumbnail not found", body = AppResponseError),
        (status = 500, description = "Internal server error", body = AppResponseError)
    ),
    security(
        ("jwt" = [])
    )
)]
pub async fn get_event_thumbnail(
    State(state): State<AppState>,
    Path(path): Path<EventPlaybackPath>,
    scope: MonitorScope,
) -> Result<Response, AppError> {
    debug!("Getting thumbnail for event {}", path.id);

    // Get event from database (using repo to get raw entity)
    let event = get_event_entity(&state, path.id, &scope).await?;

    let storage_path = resolve_event_storage_path(&state, &event).await?;

    // ZoneMinder's own web UI serves an already-rotated still, and clients size
    // their thumbnails on that assumption, so apply the monitor's orientation
    // here rather than leaving each client to do it (GH #61). One lookup, and
    // the common ROTATE_0 case still serves the bytes straight off disk.
    let orientation = monitor_orientation(&state, event.monitor_id).await;

    // Build event directory path based on scheme
    let event_dir = build_event_directory_path(
        &storage_path,
        event.monitor_id,
        path.id,
        event.start_date_time,
        &event.scheme,
    );

    // Try various thumbnail/snapshot filenames
    let thumbnail_candidates = [
        format!("{}-snapshot.jpg", path.id),
        "snapshot.jpg".to_string(),
        format!("{:05}-capture.jpg", 1), // First capture frame
        format!("{:05}-analyse.jpg", 1), // First analyse frame
        format!("{}-00001-analyse.jpg", path.id),
        format!("{}-00001-capture.jpg", path.id),
    ];

    for filename in &thumbnail_candidates {
        let thumb_path = event_dir.join(filename);
        if let Ok(data) = tokio::fs::read(&thumb_path).await {
            debug!("Found thumbnail at: {:?}", thumb_path);
            let data = crate::service::image_orientation::orient_jpeg(data, orientation).await;
            return Ok(Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, "image/jpeg")
                .header(header::CACHE_CONTROL, "max-age=86400")
                .body(Body::from(data))
                .unwrap());
        }
    }

    // No pre-rendered snapshot on disk — fall back to extracting a frame from
    // the event's MP4. This is the case for in-flight events or recordings
    // where ZoneMinder hasn't materialised a snapshot.jpg yet.
    let video_candidates = [
        select_video_filename(path.id, &event.default_video),
        format!("{}-video.mp4", path.id),
        format!("{}-video.h264.mp4", path.id),
        format!("{}.mp4", path.id),
    ];
    let mut tried_videos: Vec<PathBuf> = Vec::with_capacity(video_candidates.len());

    for name in &video_candidates {
        let video_path = event_dir.join(name);
        if tokio::fs::metadata(&video_path).await.is_err() {
            tried_videos.push(video_path);
            continue;
        }
        debug!("Extracting thumbnail from MP4: {:?}", video_path);
        match crate::streaming::snapshot::extract_mp4_thumbnail(video_path.clone(), 320).await {
            Ok(jpeg) => {
                let jpeg =
                    crate::service::image_orientation::orient_jpeg(jpeg, orientation.clone()).await;
                return Ok(Response::builder()
                    .status(StatusCode::OK)
                    .header(header::CONTENT_TYPE, "image/jpeg")
                    .header(header::CACHE_CONTROL, "max-age=86400")
                    .body(Body::from(jpeg))
                    .unwrap());
            }
            Err(e) => {
                warn!(
                    "Failed to extract thumbnail from {:?} for event {}: {}",
                    video_path, path.id, e
                );
                tried_videos.push(video_path);
                continue;
            }
        }
    }

    warn!(
        "Thumbnail not found for event {}. Tried directory: {:?}",
        path.id, event_dir
    );

    Err(AppError::NotFoundError(Resource {
        resource_type: ResourceType::Event,
        details: vec![
            ("event_id".to_string(), path.id.to_string()),
            ("reason".to_string(), "Thumbnail not found".to_string()),
            ("tried_path".to_string(), event_dir.display().to_string()),
        ],
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::service::event_storage::is_default_storage;

    #[test]
    fn segment_index_takes_the_listed_name_and_the_bare_index() {
        assert_eq!(segment_index("0.m4s"), Some(0));
        assert_eq!(segment_index("12.m4s"), Some(12));
        assert_eq!(segment_index("3"), Some(3));
        for bad in ["", ".m4s", "x.m4s", "1.ts", "-1", "1.m4s.m4s"] {
            assert_eq!(segment_index(bad), None, "{bad}");
        }
    }

    // ------------------------------------------------------------------
    // parse_range_header
    // ------------------------------------------------------------------

    #[test]
    fn parse_range_header_normal_closed_range() {
        assert_eq!(
            parse_range_header(Some("bytes=0-499"), 1000),
            Some((0, 499))
        );
        assert_eq!(
            parse_range_header(Some("bytes=500-999"), 1000),
            Some((500, 999))
        );
    }

    #[test]
    fn parse_range_header_open_ended_range() {
        // `bytes=500-` means from 500 to the end of file.
        assert_eq!(
            parse_range_header(Some("bytes=500-"), 1000),
            Some((500, 999))
        );
        // From the very first byte to the end.
        assert_eq!(parse_range_header(Some("bytes=0-"), 1000), Some((0, 999)));
    }

    #[test]
    fn parse_range_header_suffix_range() {
        // `bytes=-500` means the last 500 bytes.
        assert_eq!(
            parse_range_header(Some("bytes=-500"), 1000),
            Some((500, 999))
        );
        // A suffix larger than the file clamps the start to 0.
        assert_eq!(
            parse_range_header(Some("bytes=-5000"), 1000),
            Some((0, 999))
        );
    }

    #[test]
    fn parse_range_header_single_byte() {
        assert_eq!(parse_range_header(Some("bytes=0-0"), 1000), Some((0, 0)));
        assert_eq!(
            parse_range_header(Some("bytes=999-999"), 1000),
            Some((999, 999))
        );
    }

    #[test]
    fn parse_range_header_rejects_start_after_end() {
        assert_eq!(parse_range_header(Some("bytes=500-400"), 1000), None);
    }

    /// RFC 9110 §14.1.2: a last-pos at or past the end means "to the end".
    #[test]
    fn parse_range_header_clamps_end_beyond_file() {
        assert_eq!(
            parse_range_header(Some("bytes=0-2000"), 1000),
            Some((0, 999))
        );
        assert_eq!(
            parse_range_header(Some("bytes=0-1000"), 1000),
            Some((0, 999))
        );
        assert_eq!(parse_range_header(Some("bytes=1000-1500"), 1000), None);
    }

    #[test]
    fn parse_range_header_rejects_start_beyond_file() {
        assert_eq!(parse_range_header(Some("bytes=2000-3000"), 1000), None);
    }

    #[test]
    fn parse_range_header_rejects_missing_or_garbage() {
        assert_eq!(parse_range_header(None, 1000), None);
        assert_eq!(parse_range_header(Some(""), 1000), None);
        assert_eq!(parse_range_header(Some("invalid"), 1000), None);
        // Missing the `bytes=` unit prefix.
        assert_eq!(parse_range_header(Some("0-499"), 1000), None);
        // Wrong unit.
        assert_eq!(parse_range_header(Some("items=0-499"), 1000), None);
    }

    #[test]
    fn parse_range_header_rejects_malformed_spec() {
        // Too many dashes -> more than two parts.
        assert_eq!(parse_range_header(Some("bytes=0-1-2"), 1000), None);
        // Empty start and empty end.
        assert_eq!(parse_range_header(Some("bytes=-"), 1000), None);
        // Non-numeric start.
        assert_eq!(parse_range_header(Some("bytes=abc-499"), 1000), None);
        // Non-numeric end.
        assert_eq!(parse_range_header(Some("bytes=0-xyz"), 1000), None);
        // Non-numeric suffix.
        assert_eq!(parse_range_header(Some("bytes=-abc"), 1000), None);
    }

    #[test]
    fn parse_range_header_zero_length_file_is_unsatisfiable() {
        // A zero-length file has no valid byte range; every form must yield
        // `None` rather than panicking on the internal `file_size - 1`.
        assert_eq!(parse_range_header(Some("bytes=0-"), 0), None);
        assert_eq!(parse_range_header(Some("bytes=-100"), 0), None);
        assert_eq!(parse_range_header(Some("bytes=0-0"), 0), None);
    }

    // Event directory/scheme resolution is tested in `service::event_storage`,
    // the module that now owns `build_event_directory_path` / `is_default_storage`.

    #[test]
    fn is_safe_event_filename_accepts_normal_video_names() {
        assert!(is_safe_event_filename("467-video.mp4"));
        assert!(is_safe_event_filename("467-video.h264.mp4"));
        assert!(is_safe_event_filename("snapshot.jpg"));
        assert!(is_safe_event_filename("a"));
    }

    #[test]
    fn is_safe_event_filename_rejects_traversal_and_separators() {
        // Path components that would escape the event directory
        assert!(!is_safe_event_filename(""));
        assert!(!is_safe_event_filename("."));
        assert!(!is_safe_event_filename(".."));
        assert!(!is_safe_event_filename("../etc/passwd"));
        assert!(!is_safe_event_filename("../../etc/passwd"));
        assert!(!is_safe_event_filename("/etc/passwd"));
        assert!(!is_safe_event_filename("subdir/file.mp4"));
        assert!(!is_safe_event_filename("foo\\bar"));
        assert!(!is_safe_event_filename("foo\0bar"));
    }

    #[test]
    fn select_video_filename_uses_default_when_empty() {
        assert_eq!(select_video_filename(42, ""), "42-video.mp4");
    }

    #[test]
    fn select_video_filename_passes_through_safe_names() {
        assert_eq!(
            select_video_filename(42, "42-video.h264.mp4"),
            "42-video.h264.mp4"
        );
    }

    #[test]
    fn select_video_filename_falls_back_on_unsafe_input() {
        // Path traversal in default_video must NOT be honored.
        assert_eq!(
            select_video_filename(42, "../../../etc/passwd"),
            "42-video.mp4"
        );
        assert_eq!(
            select_video_filename(42, "subdir/escape.mp4"),
            "42-video.mp4"
        );
    }

    #[test]
    fn select_video_filename_rejects_hls_playlist_default() {
        // ZoneMinder records HLS-mode monitors with `DefaultVideo = index.m3u8`
        // (a playlist, not a playable container). Serving it as the event video
        // hands the client a playlist; fall back to the canonical name so the
        // alternative-name search finds the real `{id}-video.h264.mp4`.
        assert_eq!(select_video_filename(7238, "index.m3u8"), "7238-video.mp4");
        assert_eq!(select_video_filename(7238, "stream.M3U8"), "7238-video.mp4");
        assert_eq!(select_video_filename(7238, "index.ts"), "7238-video.mp4");
    }

    #[test]
    fn select_video_filename_accepts_video_containers_case_insensitively() {
        assert_eq!(select_video_filename(9, "9-video.mp4"), "9-video.mp4");
        assert_eq!(
            select_video_filename(9, "9-video.h264.mp4"),
            "9-video.h264.mp4"
        );
        assert_eq!(select_video_filename(9, "clip.MKV"), "clip.MKV");
        assert_eq!(select_video_filename(9, "clip.MOV"), "clip.MOV");
    }

    #[test]
    fn is_video_container_matches_known_extensions() {
        assert!(is_video_container("a.mp4"));
        assert!(is_video_container("a.h264.mp4"));
        assert!(is_video_container("a.MKV"));
        assert!(is_video_container("a.mov"));
        assert!(is_video_container("a.m4v"));
        assert!(is_video_container("a.webm"));
        assert!(!is_video_container("index.m3u8"));
        assert!(!is_video_container("seg.ts"));
        assert!(!is_video_container("snapshot.jpg"));
        assert!(!is_video_container("noext"));
    }

    #[test]
    fn is_default_storage_treats_zero_and_null_as_default() {
        // ZoneMinder uses StorageId = 0 (and sometimes NULL) as a sentinel for
        // the primary/default storage, not a real foreign key.
        assert!(is_default_storage(None));
        assert!(is_default_storage(Some(0)));
        assert!(!is_default_storage(Some(1)));
        assert!(!is_default_storage(Some(7)));
        // (owned by `service::event_storage`; kept here as a smoke check)
    }

    #[test]
    fn replace_map_uri_swaps_uri_keeps_byterange() {
        let line = r#"#EXT-X-MAP:URI="index.php?view=view_video&eid=7848&file=incomplete.h264.mp4",BYTERANGE="868@0""#;
        assert_eq!(
            replace_map_uri(line, "media.mp4"),
            r#"#EXT-X-MAP:URI="media.mp4",BYTERANGE="868@0""#
        );
    }

    #[test]
    fn replace_map_uri_passes_through_without_uri() {
        let line = "#EXT-X-MAP:BYTERANGE=\"10@0\"";
        assert_eq!(replace_map_uri(line, "media.mp4"), line);
    }

    #[test]
    fn rewrite_zm_event_playlist_rewrites_uris_preserves_structure() {
        // Verbatim shape of ZoneMinder's in-progress index.m3u8.
        let zm = "#EXTM3U\n\
                  #EXT-X-VERSION:7\n\
                  #EXT-X-TARGETDURATION:1\n\
                  #EXT-X-MEDIA-SEQUENCE:0\n\
                  #EXT-X-PLAYLIST-TYPE:EVENT\n\
                  #EXT-X-MAP:URI=\"index.php?view=view_video&eid=7848&file=incomplete.h264.mp4\",BYTERANGE=\"868@0\"\n\
                  #EXTINF:1.000,\n\
                  #EXT-X-BYTERANGE:631575@868\n\
                  index.php?view=view_video&eid=7848&file=incomplete.h264.mp4\n\
                  #EXTINF:1.000,\n\
                  #EXT-X-BYTERANGE:627822@632443\n\
                  index.php?view=view_video&eid=7848&file=incomplete.h264.mp4\n";

        let out = rewrite_zm_event_playlist(zm, "media.mp4");

        // No legacy PHP path survives anywhere.
        assert!(!out.contains("index.php"), "index.php leaked: {out}");
        // EVENT type and byte-range offsets are preserved; no premature ENDLIST.
        assert!(out.contains("#EXT-X-PLAYLIST-TYPE:EVENT"));
        assert!(out.contains("#EXT-X-BYTERANGE:631575@868"));
        assert!(out.contains("#EXT-X-BYTERANGE:627822@632443"));
        assert!(!out.contains("#EXT-X-ENDLIST"));
        // MAP + every segment line now target the native media route.
        assert!(out.contains(r#"#EXT-X-MAP:URI="media.mp4",BYTERANGE="868@0""#));
        assert_eq!(out.matches("\nmedia.mp4\n").count(), 2);
        // EXTINF timing untouched.
        assert_eq!(out.matches("#EXTINF:1.000,").count(), 2);
    }

    #[test]
    fn rewrite_zm_event_playlist_keeps_endlist_when_present() {
        let zm =
            "#EXTM3U\nindex.php?view=view_video&eid=1&file=incomplete.h264.mp4\n#EXT-X-ENDLIST\n";
        let out = rewrite_zm_event_playlist(zm, "media.mp4");
        assert!(out.contains("#EXT-X-ENDLIST"));
        assert!(out.contains("\nmedia.mp4\n"));
        assert!(!out.contains("index.php"));
    }
}
