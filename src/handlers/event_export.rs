//! `POST /api/v3/events/export` (#30).

use axum::body::Body;
use axum::extract::State;
use axum::http::{header, HeaderValue};
use axum::response::{IntoResponse, Response};
use axum::Json;
use garde::Validate;

use crate::dto::request::event_export::EventExportRequest;
use crate::error::{AppError, AppResponseError, AppResult};
use crate::server::state::AppState;
use crate::service::event_export as svc;
use crate::service::monitor_acl::MonitorScope;

/// Export events as a zip
///
/// Streams a zip, built as it goes, with a `{event_id}/` folder per event
/// holding the parts asked for: `details` (`event.json`), `frames`
/// (`frames.json`), `images`, `video` and `misc` (the event directory's other
/// files). Every id is checked first: one that doesn't exist or isn't
/// visible to the caller is a 404 and nothing is sent.
/// The most exports written at once. One reads up to 500 events' recordings
/// off the disks the cameras are recording to, and it takes only View access
/// to start one, so without a limit a viewer could start enough to stall
/// recording.
const MAX_CONCURRENT_EXPORTS: usize = 4;

static EXPORT_SLOTS: once_cell::sync::Lazy<std::sync::Arc<tokio::sync::Semaphore>> =
    once_cell::sync::Lazy::new(|| {
        std::sync::Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENT_EXPORTS))
    });

#[utoipa::path(
    post,
    path = "/api/v3/events/export",
    operation_id = "exportEvents",
    request_body = EventExportRequest,
    responses(
        (status = 200, description = "The zip", content_type = "application/zip", body = Vec<u8>),
        (status = 400, description = "No ids, or more than 500", body = AppResponseError),
        (status = 404, description = "An event doesn't exist or isn't visible", body = AppResponseError)
    ),
    tag = "Events",
    security(("jwt" = []))
)]
pub async fn export_events(
    State(state): State<AppState>,
    scope: MonitorScope,
    Json(req): Json<EventExportRequest>,
) -> AppResult<Response> {
    req.validate().map_err(AppError::InvalidInputError)?;
    // Taken before any work, held until the archive is written.
    let slot = EXPORT_SLOTS.clone().try_acquire_owned().map_err(|_| {
        AppError::ServiceUnavailableError(format!(
            "{MAX_CONCURRENT_EXPORTS} exports are already running; try again when one finishes"
        ))
    })?;
    let exports = svc::prepare(&state, &scope, &req.ids).await?;
    let (writer, reader) = tokio::io::duplex(64 * 1024);
    let (state_for_write, parts) = (state.clone(), req.parts());
    tokio::spawn(async move {
        let _slot = slot;
        svc::write(state_for_write, exports, parts, writer).await;
    });

    let name = format!(
        "attachment; filename=\"zm-events-{}.zip\"",
        crate::util::time::local_now().strftime("%Y%m%d-%H%M%S")
    );
    let mut res = Body::from_stream(tokio_util::io::ReaderStream::new(reader)).into_response();
    let h = res.headers_mut();
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/zip"),
    );
    if let Ok(v) = HeaderValue::from_str(&name) {
        h.insert(header::CONTENT_DISPOSITION, v);
    }
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    Ok(res)
}
