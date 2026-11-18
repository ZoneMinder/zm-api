use crate::dto::PaginatedResponse;
use crate::entity::snapshots::Model as SnapshotModel;
use jiff::civil::DateTime as Civil;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

#[derive(Debug, Clone, Deserialize, Serialize, ToSchema)]
pub struct SnapshotResponse {
    pub id: u32,
    pub name: Option<String>,
    pub description: Option<String>,
    pub created_by: Option<i32>,
    #[schema(value_type = Option<String>, example = "2025-01-15T10:30:00")]
    #[serde(default, with = "crate::util::datetime_format::optional")]
    pub created_on: Option<Civil>,
}

impl From<&SnapshotModel> for SnapshotResponse {
    fn from(model: &SnapshotModel) -> Self {
        Self {
            id: model.id,
            name: model.name.clone(),
            description: model.description.clone(),
            created_by: model.created_by,
            created_on: model.created_on.map(crate::util::time::civil),
        }
    }
}

/// Paginated response for snapshots
#[derive(Debug, Clone, Deserialize, Serialize, ToSchema)]
pub struct PaginatedSnapshotsResponse {
    pub items: Vec<SnapshotResponse>,
    pub total: u64,
    pub per_page: u64,
    pub current_page: u64,
    pub last_page: u64,
}

impl From<PaginatedResponse<SnapshotResponse>> for PaginatedSnapshotsResponse {
    fn from(r: PaginatedResponse<SnapshotResponse>) -> Self {
        Self {
            items: r.items,
            total: r.total,
            per_page: r.per_page,
            current_page: r.current_page,
            last_page: r.last_page,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The fraction keeps chrono's width, and a body without the field parses.
    #[test]
    fn created_on_keeps_its_spelling() {
        let mut r = SnapshotResponse {
            id: 1,
            name: None,
            description: None,
            created_by: None,
            created_on: Some("2025-01-15T10:30:00.5".parse().unwrap()),
        };
        let json = serde_json::to_value(&r).unwrap();
        assert_eq!(json["created_on"], "2025-01-15T10:30:00.500");
        r.created_on = Some("2025-01-15T10:30:00".parse().unwrap());
        let json = serde_json::to_value(&r).unwrap();
        assert_eq!(json["created_on"], "2025-01-15T10:30:00");
        let back: SnapshotResponse = serde_json::from_value(json).unwrap();
        assert_eq!(back.created_on, r.created_on);
        let bare: SnapshotResponse = serde_json::from_str(r#"{"id":1}"#).unwrap();
        assert_eq!(bare.created_on, None);
    }
}
