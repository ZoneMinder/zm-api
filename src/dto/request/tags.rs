use garde::Validate;
use jiff::Timestamp;
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};

#[derive(Debug, Clone, Deserialize, Serialize, ToSchema, Validate)]
pub struct CreateTagRequest {
    #[garde(length(chars, max = 64))]
    pub name: String,
    #[schema(value_type = Option<String>, example = "2025-01-15T10:30:00Z")]
    #[garde(skip)]
    pub create_date: Option<Timestamp>,
}

#[derive(Debug, Clone, Deserialize, Serialize, ToSchema, Validate)]
pub struct UpdateTagRequest {
    #[garde(inner(length(chars, max = 64)))]
    pub name: Option<String>,
    #[schema(value_type = Option<String>, example = "2025-01-15T10:30:00Z")]
    #[garde(skip)]
    pub create_date: Option<Timestamp>,
}

/// Query params for tag detail with paginated events
#[derive(Debug, Deserialize, IntoParams)]
pub struct TagDetailQuery {
    /// Page number (1-indexed)
    #[param(minimum = 1)]
    pub page: Option<u64>,
    /// Items per page
    #[param(minimum = 1, maximum = 100)]
    pub page_size: Option<u64>,
}
