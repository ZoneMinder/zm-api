use garde::Validate;
use jiff::civil::DateTime as Civil;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

#[derive(Debug, Clone, Deserialize, Serialize, ToSchema, Validate)]
pub struct CreateSnapshotRequest {
    #[garde(inner(length(chars, max = 64)))]
    pub name: Option<String>,
    #[garde(skip)]
    pub description: Option<String>,
    #[garde(skip)]
    pub created_by: Option<i32>,
    #[schema(value_type = Option<String>, example = "2025-01-15T10:30:00")]
    #[garde(skip)]
    #[serde(default, with = "crate::util::datetime_format::optional")]
    pub created_on: Option<Civil>,
}

#[derive(Debug, Clone, Deserialize, Serialize, ToSchema, Validate)]
pub struct UpdateSnapshotRequest {
    #[garde(inner(length(chars, max = 64)))]
    pub name: Option<String>,
    #[garde(skip)]
    pub description: Option<String>,
    #[garde(skip)]
    pub created_by: Option<i32>,
    #[schema(value_type = Option<String>, example = "2025-01-15T10:30:00")]
    #[garde(skip)]
    #[serde(default, with = "crate::util::datetime_format::optional")]
    pub created_on: Option<Civil>,
}
