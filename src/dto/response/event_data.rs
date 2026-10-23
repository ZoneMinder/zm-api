use crate::dto::PaginatedResponse;
use crate::entity::event_data::Model as EventDataModel;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct EventDataResponse {
    pub id: u64,
    pub event_id: Option<u64>,
    pub monitor_id: Option<u32>,
    pub frame_id: Option<u32>,
    pub timestamp: Option<String>,
    pub data: Option<String>,
}

impl From<&EventDataModel> for EventDataResponse {
    fn from(model: &EventDataModel) -> Self {
        Self {
            id: model.id,
            event_id: model.event_id,
            monitor_id: model.monitor_id,
            frame_id: model.frame_id,
            timestamp: model
                .timestamp
                .map(|dt| crate::util::timestamp_to_utc(dt).to_rfc3339()),
            data: model.data.clone(),
        }
    }
}

/// Paginated response for event data
#[derive(Debug, Clone, Deserialize, Serialize, ToSchema)]
pub struct PaginatedEventDataResponse {
    pub items: Vec<EventDataResponse>,
    pub total: u64,
    pub per_page: u64,
    pub current_page: u64,
    pub last_page: u64,
}

impl From<PaginatedResponse<EventDataResponse>> for PaginatedEventDataResponse {
    fn from(r: PaginatedResponse<EventDataResponse>) -> Self {
        Self {
            items: r.items,
            total: r.total,
            per_page: r.per_page,
            current_page: r.current_page,
            last_page: r.last_page,
        }
    }
}
