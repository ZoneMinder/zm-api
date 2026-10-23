use sea_orm::prelude::DateTimeUtc;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::dto::PaginatedResponse;
use crate::entity::tags::Model as TagModel;

#[derive(Debug, Clone, Deserialize, Serialize, ToSchema)]
pub struct TagResponse {
    pub id: u64,
    pub name: String,
    #[schema(value_type = Option<String>, example = "2025-01-15T10:30:00Z")]
    pub create_date: Option<DateTimeUtc>,
    /// Number of events with this tag
    #[serde(skip_serializing_if = "Option::is_none")]
    pub event_count: Option<u64>,
}

impl From<&TagModel> for TagResponse {
    fn from(model: &TagModel) -> Self {
        Self {
            id: model.id,
            name: model.name.clone(),
            create_date: model.create_date.map(crate::util::timestamp_to_utc),
            event_count: None,
        }
    }
}

impl TagResponse {
    /// Create a TagResponse with event count
    pub fn with_event_count(model: &TagModel, count: u64) -> Self {
        Self {
            id: model.id,
            name: model.name.clone(),
            create_date: model.create_date.map(crate::util::timestamp_to_utc),
            event_count: Some(count),
        }
    }
}

/// Paginated response for tags
#[derive(Debug, Clone, Deserialize, Serialize, ToSchema)]
pub struct PaginatedTagsResponse {
    pub items: Vec<TagResponse>,
    pub total: u64,
    pub per_page: u64,
    pub current_page: u64,
    pub last_page: u64,
}

impl From<PaginatedResponse<TagResponse>> for PaginatedTagsResponse {
    fn from(r: PaginatedResponse<TagResponse>) -> Self {
        Self {
            items: r.items,
            total: r.total,
            per_page: r.per_page,
            current_page: r.current_page,
            last_page: r.last_page,
        }
    }
}
