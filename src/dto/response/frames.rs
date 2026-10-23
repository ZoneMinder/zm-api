use crate::dto::PaginatedResponse;
use crate::entity::frames::Model as FrameModel;
use crate::entity::sea_orm_active_enums::FrameType;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

#[derive(Debug, Clone, Deserialize, Serialize, ToSchema)]
pub struct FrameResponse {
    /// Event ID this frame belongs to; with `frame_id`, the frame's key
    pub event_id: u64,
    /// Frame number within the event
    pub frame_id: u32,
    /// Frame type (Normal, Bulk, Alarm)
    #[schema(value_type = String)]
    pub r#type: FrameType,
    /// Timestamp of the frame
    #[schema(value_type = String, example = "2025-01-15T10:30:00Z")]
    pub time_stamp: String,
    /// Time delta from previous frame
    #[schema(value_type = String, example = "0.05")]
    pub delta: Decimal,
    /// Motion detection score
    pub score: u16,
    /// Peak audio level since the previous frame row, 0-100 on the scale
    /// `Monitors.AudioThreshold` uses; 0 means silence or no audio
    pub audio_level: u8,
}

impl From<&FrameModel> for FrameResponse {
    fn from(model: &FrameModel) -> Self {
        Self {
            event_id: model.event_id,
            frame_id: model.frame_id,
            r#type: model.r#type.clone(),
            time_stamp: crate::util::timestamp_to_utc(model.time_stamp).to_string(),
            delta: model.delta,
            score: model.score,
            audio_level: model.audio_level,
        }
    }
}

/// Paginated response for frames
#[derive(Debug, Clone, Deserialize, Serialize, ToSchema)]
pub struct PaginatedFramesResponse {
    pub items: Vec<FrameResponse>,
    pub total: u64,
    pub per_page: u64,
    pub current_page: u64,
    pub last_page: u64,
}

impl From<PaginatedResponse<FrameResponse>> for PaginatedFramesResponse {
    fn from(r: PaginatedResponse<FrameResponse>) -> Self {
        Self {
            items: r.items,
            total: r.total,
            per_page: r.per_page,
            current_page: r.current_page,
            last_page: r.last_page,
        }
    }
}
