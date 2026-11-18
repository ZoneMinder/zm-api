use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::dto::response::events_tags::TagSummary;
use crate::dto::wrappers::{DateTimeWrapper, DecimalWrapper, NaiveDateTimeWrapper, SchemeWrapper};
use crate::entity::events::Model as EventModel;
use crate::entity::sea_orm_active_enums::Orientation;

#[derive(Debug, Deserialize, Serialize, ToSchema)]
pub struct EventResponse {
    #[schema(example = 1)]
    pub id: u64,

    #[schema(example = 1)]
    pub monitor_id: u32,

    #[schema(example = 1)]
    pub storage_id: u16,

    #[schema(example = "null", nullable = true)]
    pub secondary_storage_id: Option<u16>,

    #[schema(example = "Motion Detection")]
    pub name: String,

    #[schema(example = "Motion in zone 1", nullable = true)]
    pub cause: Option<String>,

    #[schema(example = "2025-04-29T10:00:00Z", nullable = true)]
    pub start_date_time: Option<DateTimeWrapper>,

    #[schema(example = "2025-04-29T10:02:00Z", nullable = true)]
    pub end_date_time: Option<DateTimeWrapper>,

    #[schema(example = 1920)]
    pub width: u16,

    #[schema(example = 1080)]
    pub height: u16,

    #[schema(example = "2.00")]
    pub length: DecimalWrapper,

    #[schema(example = 60)]
    pub frames: u32,

    #[schema(example = 10)]
    pub alarm_frames: u32,

    #[schema(example = "video.mp4")]
    pub default_video: String,

    #[schema(example = 1, nullable = true)]
    pub save_jpe_gs: Option<i8>,

    #[schema(example = 100)]
    pub tot_score: u32,

    #[schema(example = 50, nullable = true)]
    pub avg_score: Option<u16>,

    #[schema(example = 100, nullable = true)]
    pub max_score: Option<u16>,

    /// The frame with the highest score (`MaxScoreFrameId`): what a client
    /// reports as the matching frame (#176).
    #[schema(example = 42, nullable = true)]
    pub max_score_frame_id: Option<u32>,

    #[schema(example = 0)]
    pub archived: u8,

    #[schema(example = 1)]
    pub videoed: u8,

    #[schema(example = 0)]
    pub uploaded: u8,

    #[schema(example = 0)]
    pub emailed: u8,

    #[schema(example = 0)]
    pub messaged: u8,

    #[schema(example = 0)]
    pub executed: u8,

    #[schema(example = "Important security event", nullable = true)]
    pub notes: Option<String>,

    #[schema(example = 1)]
    pub state_id: u32,

    #[schema(example = "ROTATE_0")]
    #[serde(rename = "orientation")]
    pub orientation: Orientation,

    #[schema(example = 1048576, nullable = true)]
    pub disk_space: Option<u64>,

    #[schema(example = "Deep")]
    pub scheme: SchemeWrapper,

    #[schema(example = 0)]
    pub locked: i8,

    #[schema(example = "45.12345678", nullable = true)]
    pub latitude: Option<DecimalWrapper>,

    #[schema(example = "-75.12345678", nullable = true)]
    pub longitude: Option<DecimalWrapper>,

    /// Tags associated with this event
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tags: Option<Vec<TagSummary>>,

    /// The event's directory, as the server resolves it from its storage and
    /// scheme (what ZoneMinder's event page shows as Path). Only on
    /// `GET /events/{id}`; whether the media is still there is what
    /// `GET /events/{id}/info` answers (#140).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
}

impl From<EventModel> for EventResponse {
    fn from(model: EventModel) -> Self {
        // ZoneMinder stores DATETIME in server-local time; convert to true UTC
        // (GH #16).
        let to_utc = crate::util::datetime_to_utc;

        Self {
            id: model.id,
            monitor_id: model.monitor_id,
            storage_id: model.storage_id.unwrap_or(0),
            secondary_storage_id: model.secondary_storage_id,
            name: model.name,
            cause: model.cause,
            // Corrected: Use tuple struct constructor directly
            start_date_time: model
                .start_date_time
                .map(|ndt| DateTimeWrapper(to_utc(ndt))),
            // Corrected: Use tuple struct constructor directly
            end_date_time: model.end_date_time.map(|ndt| DateTimeWrapper(to_utc(ndt))),
            width: model.width,
            height: model.height,
            length: DecimalWrapper::from(model.length),
            frames: model.frames.unwrap_or(0),
            alarm_frames: model.alarm_frames.unwrap_or(0),
            default_video: model.default_video,
            save_jpe_gs: model.save_jpe_gs,
            tot_score: model.tot_score,
            avg_score: model.avg_score,
            max_score: model.max_score,
            max_score_frame_id: model.max_score_frame_id,
            archived: model.archived,
            videoed: model.videoed,
            uploaded: model.uploaded,
            emailed: model.emailed,
            messaged: model.messaged,
            executed: model.executed,
            notes: model.notes,
            state_id: model.state_id,
            orientation: model.orientation,
            disk_space: model.disk_space,
            scheme: SchemeWrapper::from(model.scheme),
            locked: model.locked,
            latitude: model.latitude.map(DecimalWrapper::from),
            longitude: model.longitude.map(DecimalWrapper::from),
            tags: None,
            path: None,
        }
    }
}

impl EventResponse {
    /// Create an EventResponse with tags
    pub fn with_tags(model: EventModel, tags: Vec<TagSummary>) -> Self {
        let mut response = Self::from(model);
        response.tags = if tags.is_empty() { None } else { Some(tags) };
        response
    }
}

#[derive(Debug, Deserialize, Serialize, ToSchema)]
pub struct PaginatedEventsResponse {
    pub items: Vec<EventResponse>,
    pub total: u64,
    pub per_page: u64,
    pub current_page: u64,
    pub last_page: u64,
    /// Summed `length` of every matching event, in seconds. Only present when
    /// the request asked for it with `totals=true`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_length_seconds: Option<f64>,
    /// Summed `disk_space` of every matching event, in bytes. Only with
    /// `totals=true`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_disk_space: Option<u64>,
}

#[derive(Debug, Deserialize, Serialize, ToSchema)]
pub struct EventCountResponse {
    pub count: u64,
    #[schema(example = "2025-04-29T10:00:00Z")]
    pub date: NaiveDateTimeWrapper,
}

#[derive(Debug, Deserialize, Serialize, ToSchema)]
pub struct EventCountsResponse {
    pub counts: Vec<EventCountResponse>,
    pub hours: i64,
}

/// Event count per monitor (for console view)
#[derive(Debug, Deserialize, Serialize, ToSchema)]
pub struct MonitorEventCount {
    pub monitor_id: u32,
    pub count: u64,
}

/// Response for event counts grouped by monitor
#[derive(Debug, Deserialize, Serialize, ToSchema)]
pub struct EventCountsByMonitorResponse {
    pub counts: Vec<MonitorEventCount>,
    pub hours: i64,
}
