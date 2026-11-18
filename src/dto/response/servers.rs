use crate::dto::PaginatedResponse;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ServerResponse {
    pub id: u32,
    pub name: String,
    pub protocol: Option<String>,
    pub hostname: Option<String>,
    pub port: Option<u32>,
    pub path_to_index: Option<String>,
    pub path_to_zms: Option<String>,
    pub path_to_api: Option<String>,
    pub state_id: Option<u32>,
    pub status: String,
    /// Per-daemon enable flags (0/1). ZoneMinder's `Servers` table has no
    /// `zmtelemetry` column, so only these four are exposed.
    pub zmstats: i8,
    pub zmaudit: i8,
    pub zmtrigger: i8,
    pub zmeventnotification: i8,
    pub latitude: Option<f64>,
    pub longitude: Option<f64>,
    /// Monitors assigned to this server (`Monitors.ServerId`).
    #[serde(default)]
    pub monitor_count: u64,
    /// Where this server's zm-api is, as the server answering works it out:
    /// its `cluster.peers` setting, else its own scheme and port on this
    /// row's hostname. Absent when the row has no hostname. A client does not
    /// need it: any server relays requests for another server's monitors.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_url: Option<String>,
    /// Whether this is the server that answered.
    #[serde(default)]
    pub is_this_server: bool,
}

impl From<&crate::entity::servers::Model> for ServerResponse {
    fn from(m: &crate::entity::servers::Model) -> Self {
        use rust_decimal::prelude::ToPrimitive;
        Self {
            id: m.id,
            name: m.name.clone(),
            protocol: m.protocol.clone(),
            hostname: m.hostname.clone(),
            port: m.port,
            path_to_index: m.path_to_index.clone(),
            path_to_zms: m.path_to_zms.clone(),
            path_to_api: m.path_to_api.clone(),
            state_id: m.state_id,
            status: m.status.to_string(),
            zmstats: m.zmstats,
            zmaudit: m.zmaudit,
            zmtrigger: m.zmtrigger,
            zmeventnotification: m.zmeventnotification,
            latitude: m.latitude.and_then(|d| d.to_f64()),
            longitude: m.longitude.and_then(|d| d.to_f64()),
            monitor_count: 0,
            api_url: None,
            is_this_server: false,
        }
    }
}

/// Paginated response for servers
#[derive(Debug, Clone, Deserialize, Serialize, ToSchema)]
pub struct PaginatedServersResponse {
    pub items: Vec<ServerResponse>,
    pub total: u64,
    pub per_page: u64,
    pub current_page: u64,
    pub last_page: u64,
}

impl From<PaginatedResponse<ServerResponse>> for PaginatedServersResponse {
    fn from(r: PaginatedResponse<ServerResponse>) -> Self {
        Self {
            items: r.items,
            total: r.total,
            per_page: r.per_page,
            current_page: r.current_page,
            last_page: r.last_page,
        }
    }
}
