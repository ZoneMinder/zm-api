use garde::Validate;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::dto::wrappers::{DateTimeWrapper, DecimalWrapper};
use crate::entity::sea_orm_active_enums::Orientation;

/// Query for `GET /events/counts-by-monitor/{hours}`.
#[derive(Debug, Default, Deserialize, utoipa::IntoParams)]
pub struct EventCountsByMonitorQuery {
    /// `false` leaves archived events out, `true` counts only them; absent
    /// counts both. ZoneMinder's `consoleEvents/<interval>/Archived=:0`.
    pub archived: Option<bool>,
}

/// Field to sort events by
#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, ToSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EventSortField {
    /// Sort by event start time (default)
    #[default]
    StartTime,
    /// Sort by event end time
    EndTime,
    /// Sort by number of alarm frames
    AlarmFrames,
    /// Sort by maximum score
    MaxScore,
    /// Sort by average score
    AvgScore,
    /// Sort by total score
    TotScore,
    /// Sort by event duration/length
    Length,
    /// Sort by event ID
    Id,
    /// Sort by event name
    Name,
    /// Sort by cause
    Cause,
    /// Sort by monitor id
    MonitorId,
    /// Sort by notes
    Notes,
    /// Sort by total frame count
    Frames,
    /// Sort by the archived flag
    Archived,
    /// Sort by the emailed flag
    Emailed,
    /// Sort by bytes on disk
    DiskSpace,
}

/// What `tag_id` asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TagFilter {
    /// Events with no tag at all (`none`, or legacy's `0`).
    Untagged,
    /// Events with at least one tag (`any`, or legacy's `-1`).
    Tagged,
    /// Events carrying at least one of these tags.
    AnyOf(Vec<u64>),
}

/// Parse `tag_id`. A value that doesn't parse is an error rather than "no
/// filter", which would return every event to a caller who asked for a few.
pub fn parse_tag_filter(raw: &str) -> Result<TagFilter, garde::Error> {
    match raw.trim() {
        "none" | "0" => return Ok(TagFilter::Untagged),
        "any" | "-1" => return Ok(TagFilter::Tagged),
        _ => {}
    }
    let ids: Result<Vec<u64>, _> = raw.split(',').map(|t| t.trim().parse::<u64>()).collect();
    match ids {
        Ok(ids) if !ids.is_empty() && !ids.contains(&0) => Ok(TagFilter::AnyOf(ids)),
        _ => Err(garde::Error::new(
            "must be `none`, `any`, or a comma-separated list of tag ids",
        )),
    }
}

fn valid_tag_filter(value: &Option<String>, _: &()) -> garde::Result {
    match value {
        Some(raw) => parse_tag_filter(raw).map(|_| ()),
        None => Ok(()),
    }
}

/// How `tag_id` is applied.
#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, ToSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TagOp {
    /// Keep events the tag filter matches (default).
    #[default]
    In,
    /// Keep events the tag filter does not match.
    NotIn,
}

/// Sort direction
#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, ToSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SortDirection {
    /// Ascending order (oldest first, lowest score first)
    Asc,
    /// Descending order (newest first, highest score first) - default
    #[default]
    Desc,
}

#[derive(Debug, Deserialize, Serialize, ToSchema, Validate)]
pub struct EventQueryParams {
    #[schema(example = "1")]
    #[garde(range(min = 1))]
    pub page: Option<u64>,

    #[schema(example = "20")]
    #[garde(range(min = 1, max = 1000))]
    pub page_size: Option<u64>,

    #[schema(example = "1")]
    #[garde(range(min = 1, max = 1000000))]
    pub monitor_id: Option<u32>,

    #[schema(example = "2025-04-28T00:00:00Z")]
    #[garde(skip)]
    pub start_time: Option<DateTimeWrapper>,

    #[schema(example = "2025-04-29T23:59:59Z")]
    #[garde(skip)]
    pub end_time: Option<DateTimeWrapper>,

    /// Field to sort results by (default: start_time)
    #[schema(example = "start_time")]
    #[garde(skip)]
    pub sort: Option<EventSortField>,

    /// Sort direction (default: desc)
    #[schema(example = "desc")]
    #[garde(skip)]
    pub direction: Option<SortDirection>,

    /// Filter events with at least this many alarm frames
    #[schema(example = "5")]
    #[garde(range(min = 0))]
    pub alarm_frames_min: Option<u32>,

    /// Filter to show only archived events
    #[schema(example = "true")]
    #[garde(skip)]
    pub archived: Option<bool>,

    /// Case-insensitive substring match on the event cause.
    #[schema(example = "Motion")]
    #[garde(skip)]
    pub cause: Option<String>,

    /// Case-insensitive substring match on the event name.
    #[schema(example = "Front")]
    #[garde(skip)]
    pub name: Option<String>,

    /// Case-insensitive substring match on the event notes.
    #[garde(skip)]
    pub notes: Option<String>,

    /// Free-text search, as legacy's events search box: a substring match
    /// over Id, MonitorId, StorageId, Name, Cause, StartDateTime,
    /// EndDateTime, Length, Frames, AlarmFrames, TotScore, AvgScore,
    /// MaxScore, Archived, Emailed, Notes and DiskSpace, any of which may
    /// match (#136).
    #[schema(example = "Front")]
    #[garde(inner(length(max = 255)))]
    pub q: Option<String>,

    /// Filter by the storage the event's media lives on.
    #[schema(example = 1)]
    #[garde(skip)]
    pub storage_id: Option<u16>,

    /// Filter to events carrying any of these tag ids (comma-separated, e.g.
    /// `tag_id=1,4,5`). Standard query strings can't repeat a key into a list,
    /// so a comma-separated value is used. `none` keeps events with no tag and
    /// `any` keeps events with at least one; legacy's `0` and `-1` mean the same.
    #[schema(example = "1,4")]
    #[garde(custom(valid_tag_filter))]
    pub tag_id: Option<String>,

    /// `in` (default) keeps what `tag_id` matches, `not_in` keeps the rest.
    #[garde(skip)]
    pub tag_op: Option<TagOp>,

    /// Also return `total_length_seconds` and `total_disk_space` summed over
    /// the whole result, not the page. Off by default: it is a second
    /// aggregate query.
    #[serde(default)]
    #[garde(skip)]
    pub totals: bool,
}

#[derive(Debug, Deserialize, Serialize, ToSchema, Validate)]
pub struct EventCreateRequest {
    #[schema(example = 1)]
    #[garde(skip)]
    pub monitor_id: u32,

    #[schema(example = 1)]
    #[garde(skip)]
    pub storage_id: u16,

    #[schema(example = "null", nullable = true)]
    #[garde(skip)]
    pub secondary_storage_id: Option<u16>,

    #[schema(example = "Motion Detection")]
    // DB column `Events.Name` is varchar(64).
    #[garde(length(min = 1, max = 64))]
    pub name: String,

    #[schema(example = "Motion in zone 1", nullable = true)]
    #[garde(skip)]
    pub cause: Option<String>,

    #[schema(example = "2025-04-29T10:00:00Z", nullable = true)]
    #[garde(skip)]
    pub start_date_time: Option<DateTimeWrapper>,

    #[schema(example = "2025-04-29T10:02:00Z", nullable = true)]
    #[garde(skip)]
    pub end_date_time: Option<DateTimeWrapper>,

    #[schema(example = 1920)]
    #[garde(range(min = 320, max = 7680))]
    pub width: u16,

    #[schema(example = 1080)]
    #[garde(range(min = 240, max = 4320))]
    pub height: u16,

    #[schema(example = "2.00")]
    #[garde(skip)]
    pub length: DecimalWrapper,

    #[schema(example = "Important security event", nullable = true)]
    #[garde(length(min = 0, max = 1000))]
    pub notes: Option<String>,

    #[schema(example = "ROTATE_0")]
    #[garde(skip)]
    pub orientation: Orientation,
}

#[derive(Debug, Deserialize, Serialize, ToSchema, Validate)]
pub struct EventUpdateRequest {
    #[schema(example = "Motion Detection")]
    // DB column `Events.Name` is varchar(64).
    #[garde(length(min = 1, max = 64))]
    pub name: Option<String>,

    #[schema(example = "Motion in zone 1", nullable = true)]
    #[garde(skip)]
    pub cause: Option<String>,

    #[schema(example = "Important security event", nullable = true)]
    #[garde(length(min = 0, max = 1000))]
    pub notes: Option<String>,

    #[schema(example = "ROTATE_0")]
    #[garde(skip)]
    pub orientation: Option<Orientation>,

    /// Mark event as archived (protected from auto-deletion)
    #[schema(example = true)]
    #[garde(skip)]
    pub archived: Option<bool>,

    /// Lock event to prevent deletion by filters
    #[schema(example = false)]
    #[garde(skip)]
    pub locked: Option<bool>,

    /// Mark event as having been emailed
    #[schema(example = false)]
    #[garde(skip)]
    pub emailed: Option<bool>,

    /// Mark event as having been sent via push notification
    #[schema(example = false)]
    #[garde(skip)]
    pub messaged: Option<bool>,

    /// Mark event as having been uploaded to external storage
    #[schema(example = false)]
    #[garde(skip)]
    pub uploaded: Option<bool>,

    /// Mark event as having triggered an external action
    #[schema(example = false)]
    #[garde(skip)]
    pub executed: Option<bool>,
}
