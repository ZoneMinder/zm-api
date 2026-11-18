//! Motion-synopsis HTTP handlers.
//!
//! Serves the glanceable composite still (P1) for an event's synopsis, behind
//! the same row-level ACL as event playback: the monitor that grants
//! `Events:View` grants synopsis view, and an ACL-denied event returns the exact
//! same `NotFound` as a missing one (no existence oracle).

use axum::{
    body::Body,
    extract::{Path, Query, State},
    http::{header, HeaderMap, StatusCode},
    response::Response,
    Json,
};
use std::path::Path as StdPath;
use tokio::fs::File;
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio_util::io::ReaderStream;
use tracing::debug;

use crate::error::{AppError, AppResult, Resource, ResourceType};
use crate::handlers::events_playback::{get_event_entity, not_found_event};
use crate::server::state::AppState;
use crate::service::monitor_acl::MonitorScope;
use crate::service::synopsis::optimiser::SynopsisLayout;
use crate::service::synopsis::{SynopsisError, SynopsisStatusView};
use crate::util::authz::Level;

/// Path parameters for the synopsis endpoints.
#[derive(Debug, serde::Deserialize)]
pub struct SynopsisPath {
    pub id: u64,
}

/// Query parameters for the layout preview.
#[derive(Debug, serde::Deserialize)]
pub struct LayoutQuery {
    /// Comma-separated `class_id`s to keep (e.g. `0` for people-only). Absent or
    /// empty keeps every class.
    pub class: Option<String>,
}

/// Parse a `?class=0,2` filter into class ids, ignoring non-numeric entries.
fn parse_classes(raw: &Option<String>) -> Vec<i64> {
    raw.as_deref()
        .map(|s| {
            s.split(',')
                .filter_map(|p| p.trim().parse::<i64>().ok())
                .collect()
        })
        .unwrap_or_default()
}

/// Canonical `NotFound` for a monitor — used by the overview endpoint for both
/// missing data and ACL-deny (no existence oracle).
fn not_found_monitor(monitor_id: u32) -> AppError {
    AppError::NotFoundError(Resource {
        resource_type: ResourceType::Monitor,
        details: vec![("monitor_id".to_string(), monitor_id.to_string())],
    })
}

/// Map a [`SynopsisError`] onto an HTTP error. `NotFound` mirrors the event
/// not-found response so an absent synopsis is indistinguishable from an absent
/// (or ACL-hidden) event.
fn map_err(event_id: u64, err: SynopsisError) -> AppError {
    match err {
        SynopsisError::Disabled => {
            AppError::ServiceUnavailableError("motion synopsis is disabled".to_string())
        }
        SynopsisError::NotFound | SynopsisError::NoAssets => not_found_event(event_id),
        SynopsisError::EncoderUnavailable(m) => AppError::ServiceUnavailableError(m),
        SynopsisError::InvalidManifest(m) => {
            AppError::InternalServerError(format!("synopsis manifest unusable: {m}"))
        }
        SynopsisError::RenderFailed(m) => {
            AppError::InternalServerError(format!("synopsis render failed: {m}"))
        }
        SynopsisError::Db(e) => AppError::DatabaseError(e),
        SynopsisError::Io(e) => AppError::IoError(e),
        SynopsisError::Join(e) => {
            AppError::InternalServerError(format!("synopsis render task failed: {e}"))
        }
    }
}

/// Resolve the synopsis service from state, or report it unavailable.
fn service(
    state: &AppState,
) -> AppResult<&std::sync::Arc<crate::service::synopsis::SynopsisService>> {
    state
        .synopsis_service
        .as_ref()
        .ok_or_else(|| AppError::ServiceUnavailableError("synopsis service not initialised".into()))
}

/// `GET /api/v3/events/{id}/synopsis/review` — the P1 glanceable composite still.
///
/// Composites one representative cutout per tube over the time-central plate and
/// returns a JPEG. Degrades over pruned assets; 404 when no synopsis row exists.
#[utoipa::path(
    get,
    path = "/api/v3/events/{id}/synopsis/review",
    operation_id = "getEventSynopsisReview",
    tag = "Motion Synopsis",
    params(("id" = u64, Path, description = "Event ID")),
    responses(
        (status = 200, description = "Composite still (JPEG)", content_type = "image/jpeg"),
        (status = 404, description = "No synopsis / event not found", body = crate::error::AppResponseError),
        (status = 503, description = "Synopsis disabled or unavailable", body = crate::error::AppResponseError),
    ),
    security(("jwt" = []))
)]
pub async fn get_event_synopsis_review(
    State(state): State<AppState>,
    Path(path): Path<SynopsisPath>,
    scope: MonitorScope,
) -> Result<Response, AppError> {
    // ACL: identical NotFound for missing event and ACL-deny.
    let _event = get_event_entity(&state, path.id, &scope).await?;

    let svc = service(&state)?;
    if !svc.enabled() {
        return Err(AppError::ServiceUnavailableError(
            "motion synopsis is disabled".to_string(),
        ));
    }

    debug!("Rendering synopsis still for event {}", path.id);
    let jpeg = svc
        .still_for_event(path.id)
        .await
        .map_err(|e| map_err(path.id, e))?;

    Ok(Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "image/jpeg")
        .header(header::CACHE_CONTROL, "private, max-age=60")
        .header(header::CONTENT_LENGTH, jpeg.len().to_string())
        .body(Body::from(jpeg.to_vec()))
        .expect("valid synopsis still response"))
}

/// `GET /api/v3/events/{id}/synopsis/layout` — the P2 temporal-layout preview.
///
/// Returns the per-tube time-shifts the optimiser computed, the resulting
/// synopsis length, and any tubes dropped (too crowded). Supports `?class=` to
/// preview a class-filtered (e.g. people-only) synopsis.
#[utoipa::path(
    get,
    path = "/api/v3/events/{id}/synopsis/layout",
    operation_id = "getEventSynopsisLayout",
    tag = "Motion Synopsis",
    params(
        ("id" = u64, Path, description = "Event ID"),
        ("class" = Option<String>, Query, description = "Comma-separated class_ids to keep"),
    ),
    responses(
        (status = 200, description = "Optimiser layout", body = SynopsisLayout),
        (status = 404, description = "No synopsis / event not found", body = crate::error::AppResponseError),
        (status = 503, description = "Synopsis disabled or unavailable", body = crate::error::AppResponseError),
    ),
    security(("jwt" = []))
)]
pub async fn get_event_synopsis_layout(
    State(state): State<AppState>,
    Path(path): Path<SynopsisPath>,
    Query(query): Query<LayoutQuery>,
    scope: MonitorScope,
) -> Result<Json<SynopsisLayout>, AppError> {
    let _event = get_event_entity(&state, path.id, &scope).await?;
    let svc = service(&state)?;
    if !svc.enabled() {
        return Err(AppError::ServiceUnavailableError(
            "motion synopsis is disabled".to_string(),
        ));
    }
    let classes = parse_classes(&query.class);
    let layout = svc
        .layout_for_event(path.id, &classes)
        .await
        .map_err(|e| map_err(path.id, e))?;
    Ok(Json(layout))
}

/// `GET /api/v3/events/{id}/synopsis` — request/poll the rendered synopsis mp4.
///
/// If not yet rendered, enqueues a background render (bounded by
/// `max_concurrent_renders`) and returns `status:"generating"`; the client polls
/// until `status:"ready"`, then fetches `url` (the mp4 endpoint).
#[utoipa::path(
    get,
    path = "/api/v3/events/{id}/synopsis",
    operation_id = "getEventSynopsis",
    tag = "Motion Synopsis",
    params(("id" = u64, Path, description = "Event ID")),
    responses(
        (status = 200, description = "Render status (poll until ready)", body = SynopsisStatusView),
        (status = 404, description = "No synopsis / event not found", body = crate::error::AppResponseError),
        (status = 503, description = "Synopsis disabled or unavailable", body = crate::error::AppResponseError),
    ),
    security(("jwt" = []))
)]
pub async fn get_event_synopsis(
    State(state): State<AppState>,
    Path(path): Path<SynopsisPath>,
    scope: MonitorScope,
) -> Result<Json<SynopsisStatusView>, AppError> {
    let _event = get_event_entity(&state, path.id, &scope).await?;
    let svc = service(&state)?;
    if !svc.enabled() {
        return Err(AppError::ServiceUnavailableError(
            "motion synopsis is disabled".to_string(),
        ));
    }
    let view = svc
        .render_or_get(path.id)
        .await
        .map_err(|e| map_err(path.id, e))?;
    Ok(Json(view))
}

/// `GET /api/v3/events/{id}/synopsis/mp4` — stream the cached synopsis mp4.
///
/// Supports HTTP 206 byte ranges (for `<video>` seeking), a weak `ETag`, and
/// `Cache-Control`, mirroring the event-video endpoint. 404 until the render is
/// ready (poll `…/synopsis` first).
#[utoipa::path(
    get,
    path = "/api/v3/events/{id}/synopsis/mp4",
    operation_id = "getEventSynopsisMp4",
    tag = "Motion Synopsis",
    params(("id" = u64, Path, description = "Event ID")),
    responses(
        (status = 200, description = "Synopsis mp4", content_type = "video/mp4"),
        (status = 206, description = "Partial content (range)", content_type = "video/mp4"),
        (status = 404, description = "Not rendered yet / not found", body = crate::error::AppResponseError),
    ),
    security(("jwt" = []))
)]
pub async fn get_event_synopsis_mp4(
    State(state): State<AppState>,
    Path(path): Path<SynopsisPath>,
    scope: MonitorScope,
    headers: HeaderMap,
) -> Result<Response, AppError> {
    let _event = get_event_entity(&state, path.id, &scope).await?;
    let svc = service(&state)?;
    let mp4 = svc
        .mp4_path_for_event(path.id)
        .await
        .map_err(|e| map_err(path.id, e))?;
    serve_mp4_with_range(&mp4, &headers).await
}

/// A weak ETag derived from the file size + mtime — cheap and stable per render,
/// without hashing potentially-large mp4 bytes on every request.
fn weak_etag(meta: &std::fs::Metadata) -> String {
    let mtime = meta
        .modified()
        .ok()
        .and_then(|m| m.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("W/\"{:x}-{:x}\"", meta.len(), mtime)
}

/// Parse a single `bytes=start-end` range against `file_size`.
fn parse_range(range: Option<&str>, file_size: u64) -> Option<(u64, u64)> {
    let spec = range?.strip_prefix("bytes=")?;
    if file_size == 0 {
        return None;
    }
    let (a, b) = spec.split_once('-')?;
    let (start, end) = if a.is_empty() {
        let suffix: u64 = b.parse().ok()?;
        (file_size.saturating_sub(suffix), file_size - 1)
    } else {
        let start: u64 = a.parse().ok()?;
        let end = if b.is_empty() {
            file_size - 1
        } else {
            b.parse().ok()?
        };
        (start, end)
    };
    (start <= end && end < file_size).then_some((start, end))
}

/// Serve an mp4 with Range, ETag and Cache-Control. Streams from disk so a large
/// clip never buffers into memory.
async fn serve_mp4_with_range(path: &StdPath, headers: &HeaderMap) -> Result<Response, AppError> {
    let meta = tokio::fs::metadata(path)
        .await
        .map_err(|e| AppError::InternalServerError(format!("synopsis mp4 metadata: {e}")))?;
    let file_size = meta.len();
    let etag = weak_etag(&meta);

    // Conditional GET: unchanged render → 304.
    if let Some(inm) = headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
    {
        if inm == etag {
            return Ok(Response::builder()
                .status(StatusCode::NOT_MODIFIED)
                .header(header::ETAG, &etag)
                .header(header::CACHE_CONTROL, "private, max-age=300")
                .body(Body::empty())
                .expect("304 response"));
        }
    }

    let range = headers.get(header::RANGE).and_then(|v| v.to_str().ok());
    if let Some(range_str) = range {
        let Some((start, end)) = parse_range(Some(range_str), file_size) else {
            return Err(AppError::BadRequestError(
                "invalid Range header".to_string(),
            ));
        };
        let length = end - start + 1;
        let mut file = File::open(path)
            .await
            .map_err(|e| AppError::InternalServerError(format!("open synopsis mp4: {e}")))?;
        file.seek(std::io::SeekFrom::Start(start))
            .await
            .map_err(|e| AppError::InternalServerError(format!("seek synopsis mp4: {e}")))?;
        let stream = ReaderStream::new(file.take(length));
        return Ok(Response::builder()
            .status(StatusCode::PARTIAL_CONTENT)
            .header(header::CONTENT_TYPE, "video/mp4")
            .header(header::ACCEPT_RANGES, "bytes")
            .header(header::ETAG, &etag)
            .header(header::CACHE_CONTROL, "private, max-age=300")
            .header(
                header::CONTENT_RANGE,
                format!("bytes {start}-{end}/{file_size}"),
            )
            .header(header::CONTENT_LENGTH, length.to_string())
            .body(Body::from_stream(stream))
            .expect("206 response"));
    }

    let file = File::open(path)
        .await
        .map_err(|e| AppError::InternalServerError(format!("open synopsis mp4: {e}")))?;
    let stream = ReaderStream::new(file);
    Ok(Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "video/mp4")
        .header(header::ACCEPT_RANGES, "bytes")
        .header(header::ETAG, &etag)
        .header(header::CACHE_CONTROL, "private, max-age=300")
        .header(header::CONTENT_LENGTH, file_size.to_string())
        .body(Body::from_stream(stream))
        .expect("200 response"))
}

/// Query parameters for the range/overview synopsis.
#[derive(Debug, serde::Deserialize)]
pub struct OverviewQuery {
    pub monitor_id: u32,
    /// Window start/end as unix-epoch **seconds**.
    pub from: i64,
    pub to: i64,
    /// Comma-separated `class_id`s to keep (optional).
    pub class: Option<String>,
}

/// `GET /api/v3/events/synopsis?from=&to=&monitor_id=[&class=]` — the P4
/// range/overview synopsis: a montage still of object cutouts drawn from every
/// event in the window for a monitor. Capped (rows + tubes); excess is logged.
#[utoipa::path(
    get,
    path = "/api/v3/events/synopsis",
    operation_id = "getRangeSynopsis",
    tag = "Motion Synopsis",
    params(
        ("monitor_id" = u32, Query, description = "Monitor id"),
        ("from" = i64, Query, description = "Window start (unix epoch seconds)"),
        ("to" = i64, Query, description = "Window end (unix epoch seconds)"),
        ("class" = Option<String>, Query, description = "Comma-separated class_ids to keep"),
    ),
    responses(
        (status = 200, description = "Overview montage (JPEG)", content_type = "image/jpeg"),
        (status = 404, description = "No synopses in window / monitor not visible", body = crate::error::AppResponseError),
        (status = 503, description = "Synopsis disabled or unavailable", body = crate::error::AppResponseError),
    ),
    security(("jwt" = []))
)]
pub async fn get_range_synopsis(
    State(state): State<AppState>,
    Query(query): Query<OverviewQuery>,
    scope: MonitorScope,
) -> Result<Response, AppError> {
    // ACL by monitor; deny looks identical to "no data".
    if !scope.allows(query.monitor_id, Level::View) {
        return Err(not_found_monitor(query.monitor_id));
    }
    let svc = service(&state)?;
    if !svc.enabled() {
        return Err(AppError::ServiceUnavailableError(
            "motion synopsis is disabled".to_string(),
        ));
    }
    if query.to < query.from {
        return Err(AppError::BadRequestError(
            "`to` must be >= `from`".to_string(),
        ));
    }
    let from = jiff::Timestamp::from_second(query.from)
        .map(crate::util::time::utc_civil)
        .map_err(|_| AppError::BadRequestError("invalid `from` timestamp".to_string()))?;
    let to = jiff::Timestamp::from_second(query.to)
        .map(crate::util::time::utc_civil)
        .map_err(|_| AppError::BadRequestError("invalid `to` timestamp".to_string()))?;
    let classes = parse_classes(&query.class);

    let jpeg = match svc
        .overview_still(query.monitor_id, from, to, classes)
        .await
    {
        Ok(bytes) => bytes,
        Err(SynopsisError::NotFound) => return Err(not_found_monitor(query.monitor_id)),
        Err(e) => return Err(map_err(0, e)),
    };

    Ok(Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "image/jpeg")
        .header(header::CACHE_CONTROL, "private, max-age=60")
        .header(header::CONTENT_LENGTH, jpeg.len().to_string())
        .body(Body::from(jpeg))
        .expect("valid overview still response"))
}
