use crate::dto::PaginatedResponse;
use crate::entity::reports::Model as ReportModel;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ReportResponse {
    pub id: u32,
    pub name: Option<String>,
    pub filter_id: Option<u32>,
    pub start_date_time: Option<String>,
    pub end_date_time: Option<String>,
    /// Report interval in seconds (e.g. 604800 = 7 days).
    pub interval: Option<u32>,
    /// User who created the report. Set by the server from the authenticated
    /// caller, never accepted from the request.
    pub created_by: Option<u32>,
}

/// The column's digits labelled UTC, as this endpoint has always reported them.
fn rfc3339(column: sea_orm::prelude::DateTime) -> String {
    crate::util::time::rfc3339_offset(crate::util::time::utc_instant(column))
}

impl From<&ReportModel> for ReportResponse {
    fn from(model: &ReportModel) -> Self {
        Self {
            id: model.id,
            name: model.name.clone(),
            filter_id: model.filter_id,
            start_date_time: model.start_date_time.map(rfc3339),
            end_date_time: model.end_date_time.map(rfc3339),
            interval: model.interval,
            created_by: model.created_by,
        }
    }
}

/// Paginated response for reports
#[derive(Debug, Clone, Deserialize, Serialize, ToSchema)]
pub struct PaginatedReportsResponse {
    pub items: Vec<ReportResponse>,
    pub total: u64,
    pub per_page: u64,
    pub current_page: u64,
    pub last_page: u64,
}

impl From<PaginatedResponse<ReportResponse>> for PaginatedReportsResponse {
    fn from(r: PaginatedResponse<ReportResponse>) -> Self {
        Self {
            items: r.items,
            total: r.total,
            per_page: r.per_page,
            current_page: r.current_page,
            last_page: r.last_page,
        }
    }
}
