//! Responses for the media job queue (#187).

use serde::Serialize;
use utoipa::ToSchema;

use crate::dto::wrappers::DateTimeWrapper;
use crate::entity::media_jobs::Model;

#[derive(Debug, Serialize, ToSchema)]
pub struct MediaJobResponse {
    pub id: u32,
    /// `video` for now.
    pub kind: String,
    pub event_id: u64,
    pub monitor_id: u32,
    /// The parameters the job was queued with.
    #[schema(value_type = Object)]
    pub params: serde_json::Value,
    /// `queued`, `running`, `done`, `failed` or `cancelled`.
    pub status: String,
    /// For a video, the file name in the event's directory.
    pub output: Option<String>,
    pub error: Option<String>,
    pub created_by: Option<u32>,
    pub created_at: DateTimeWrapper,
    pub started_at: Option<DateTimeWrapper>,
    pub finished_at: Option<DateTimeWrapper>,
}

fn utc(t: sea_orm::prelude::DateTime) -> DateTimeWrapper {
    DateTimeWrapper(crate::util::time::utc_instant(t))
}

impl From<&Model> for MediaJobResponse {
    fn from(m: &Model) -> Self {
        Self {
            id: m.id,
            kind: m.kind.clone(),
            event_id: m.event_id,
            monitor_id: m.monitor_id,
            params: serde_json::from_str(&m.params).unwrap_or(serde_json::Value::Null),
            status: m.status.clone(),
            output: m.output.clone(),
            error: m.error.clone(),
            created_by: m.created_by,
            created_at: utc(m.created_at),
            started_at: m.started_at.map(utc),
            finished_at: m.finished_at.map(utc),
        }
    }
}

/// A video file in an event's directory.
#[derive(Debug, Serialize, ToSchema)]
pub struct EventVideoResponse {
    pub name: String,
    pub size_bytes: u64,
    /// The file's extension.
    pub format: String,
    /// The event's own recording (`DefaultVideo`); it can't be deleted here.
    pub recorded: bool,
    /// From a generated file's name (zmvideo.pl's `-r`, `-R`, `-s` and `-S`
    /// parts), when present.
    pub rate: Option<f64>,
    pub fps: Option<f64>,
    pub scale: Option<f64>,
    pub size: Option<String>,
}
