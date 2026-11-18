//! `POST /api/v3/events/export` (#30): a zip of events, built while it
//! streams. Each event gets a `{event_id}/` folder with the parts asked for.
//! Media files are stored as they are (they are compressed already); the
//! JSON is deflated.
//!
//! Every event is checked before the first byte goes out, so a missing or
//! out-of-scope id is a clean 404. A read error after that can only cut the
//! stream short; it is logged.

use std::path::{Path, PathBuf};

use async_zip::tokio::write::ZipFileWriter;
use async_zip::{Compression, ZipDateTime, ZipDateTimeBuilder, ZipEntryBuilder};
use futures::io::AsyncWriteExt;
use tokio::io::{AsyncWrite, DuplexStream};
use tokio_util::compat::TokioAsyncReadCompatExt;
use tracing::warn;

use crate::dto::request::event_export::ExportPart;
use crate::dto::response::events::EventResponse;
use crate::dto::response::frames::FrameResponse;
use crate::entity::events::Model as EventModel;
use crate::error::{AppError, AppResult, Resource, ResourceType};
use crate::server::state::AppState;
use crate::service::monitor_acl::MonitorScope;
use crate::util::authz::Level;

/// An event cleared for export.
pub struct Export {
    event: EventModel,
    response: EventResponse,
    dir: PathBuf,
}

/// Which part an event-directory file belongs to.
pub fn classify(name: &str) -> ExportPart {
    let ext = Path::new(name)
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase);
    match ext.as_deref() {
        Some("jpg" | "jpeg") => ExportPart::Images,
        Some("mp4" | "mkv" | "webm" | "avi") => ExportPart::Video,
        _ => ExportPart::Misc,
    }
}

/// Look every id up and check the caller may see it.
pub async fn prepare(
    state: &AppState,
    scope: &MonitorScope,
    ids: &[u64],
) -> AppResult<Vec<Export>> {
    let mut out = Vec::with_capacity(ids.len());
    let mut seen = std::collections::HashSet::new();
    for &id in ids.iter().filter(|id| seen.insert(**id)) {
        let not_found = || {
            AppError::NotFoundError(Resource {
                details: vec![("id".into(), id.to_string())],
                resource_type: ResourceType::Event,
            })
        };
        let (event, tags) = crate::repo::events::find_by_id_with_tags(state, id)
            .await?
            .ok_or_else(not_found)?;
        if !scope.allows(event.monitor_id, Level::View) {
            return Err(not_found());
        }
        let dir = crate::service::event_storage::event_directory(state, &event).await?;
        let summaries = tags
            .iter()
            .map(crate::dto::response::events_tags::TagSummary::from)
            .collect();
        let mut response = EventResponse::with_tags(event.clone(), summaries);
        response.path = Some(dir.display().to_string());
        out.push(Export {
            event,
            response,
            dir,
        });
    }
    Ok(out)
}

/// Write the zip into `pipe`; the response body reads the other end.
pub async fn write(
    state: AppState,
    exports: Vec<Export>,
    parts: Vec<ExportPart>,
    pipe: DuplexStream,
) {
    if let Err(e) = write_zip(&state, &exports, &parts, pipe).await {
        warn!("event export cut short: {e}");
    }
}

async fn write_zip<W: AsyncWrite + Unpin>(
    state: &AppState,
    exports: &[Export],
    parts: &[ExportPart],
    out: W,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let mut zip = ZipFileWriter::with_tokio(out);
    let now = zip_time(jiff::Timestamp::now());
    for ex in exports {
        let folder = ex.event.id.to_string();
        if parts.contains(&ExportPart::Details) {
            let json = serde_json::to_vec_pretty(&ex.response)?;
            let entry =
                ZipEntryBuilder::new(format!("{folder}/event.json").into(), Compression::Deflate)
                    .last_modification_date(now);
            zip.write_entry_whole(entry, &json).await?;
        }
        if parts.contains(&ExportPart::Frames) {
            let frames: Vec<FrameResponse> =
                crate::repo::frames::find_all(state.db(), ex.event.id, None)
                    .await?
                    .iter()
                    .map(FrameResponse::from)
                    .collect();
            let json = serde_json::to_vec_pretty(&frames)?;
            let entry =
                ZipEntryBuilder::new(format!("{folder}/frames.json").into(), Compression::Deflate)
                    .last_modification_date(now);
            zip.write_entry_whole(entry, &json).await?;
        }
        let file_parts = [ExportPart::Images, ExportPart::Video, ExportPart::Misc];
        if !file_parts.iter().any(|p| parts.contains(p)) {
            continue;
        }
        let mut dir = match tokio::fs::read_dir(&ex.dir).await {
            Ok(d) => d,
            Err(e) => {
                // An event whose files are gone still exports its details.
                warn!("event {} export: {}: {e}", ex.event.id, ex.dir.display());
                continue;
            }
        };
        let mut files = Vec::new();
        while let Some(entry) = dir.next_entry().await? {
            // Regular files only: no symlinks out of the event directory.
            if !entry.file_type().await?.is_file() {
                continue;
            }
            let Some(name) = entry.file_name().to_str().map(str::to_string) else {
                continue;
            };
            if parts.contains(&classify(&name)) {
                files.push((name, entry.path()));
            }
        }
        files.sort();
        for (name, path) in files {
            let modified = tokio::fs::metadata(&path)
                .await?
                .modified()
                .ok()
                .and_then(|t| jiff::Timestamp::try_from(t).ok())
                .map(zip_time)
                .unwrap_or(now);
            let entry =
                ZipEntryBuilder::new(format!("{folder}/{name}").into(), Compression::Stored)
                    .last_modification_date(modified);
            let mut writer = zip.write_entry_stream(entry).await?;
            let file = tokio::fs::File::open(&path).await?;
            futures::io::copy(&mut file.compat(), &mut writer).await?;
            writer.close().await?;
        }
    }
    let mut inner = zip.close().await?;
    inner.close().await?;
    Ok(())
}

/// An instant as a ZIP entry date: its UTC wall-clock, to the 2 s the format keeps.
fn zip_time(t: jiff::Timestamp) -> ZipDateTime {
    let utc = crate::util::time::utc_civil(t);
    ZipDateTimeBuilder::new()
        .year(i32::from(utc.year()))
        .month(utc.month() as u32)
        .day(utc.day() as u32)
        .hour(utc.hour() as u32)
        .minute(utc.minute() as u32)
        .second(utc.second() as u32)
        .build()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_zip_date_carries_the_utc_wall_clock() {
        let z = zip_time("2026-10-23T21:30:14Z".parse().unwrap());
        assert_eq!(
            (
                z.year(),
                z.month(),
                z.day(),
                z.hour(),
                z.minute(),
                z.second()
            ),
            (2026, 10, 23, 21, 30, 14)
        );
    }

    #[test]
    fn files_are_sorted_into_parts_by_extension() {
        assert_eq!(classify("00001-capture.jpg"), ExportPart::Images);
        assert_eq!(classify("snapshot.JPEG"), ExportPart::Images);
        assert_eq!(classify("12345-video.mp4"), ExportPart::Video);
        assert_eq!(classify("event.mkv"), ExportPart::Video);
        assert_eq!(classify(".ZM"), ExportPart::Misc);
        assert_eq!(classify("objdetect.json"), ExportPart::Misc);
    }
}
