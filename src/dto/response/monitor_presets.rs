use crate::dto::PaginatedResponse;
use crate::entity::monitor_presets::Model as MonitorPresetModel;
use crate::entity::sea_orm_active_enums::MonitorType;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

#[derive(Debug, Clone, Deserialize, Serialize, ToSchema)]
pub struct MonitorPresetResponse {
    pub id: u32,
    pub model_id: Option<u32>,
    pub name: String,
    #[serde(rename = "type")]
    pub r#type: MonitorType,
    pub device: Option<String>,
    pub channel: Option<u8>,
    pub format: Option<u32>,
    pub protocol: Option<String>,
    pub method: Option<String>,
    pub host: Option<String>,
    pub port: Option<String>,
    pub path: Option<String>,
    pub sub_path: Option<String>,
    pub width: Option<u16>,
    pub height: Option<u16>,
    pub palette: Option<u32>,
    /// A decimal string such as `"5.000"`, as other decimals in the API.
    #[schema(value_type = Option<String>, example = "5.000")]
    pub max_fps: Option<Decimal>,
    pub controllable: u8,
    pub control_id: Option<String>,
    pub control_device: Option<String>,
    pub control_address: Option<String>,
    pub default_rate: u16,
    pub default_scale: String,
}

impl From<&MonitorPresetModel> for MonitorPresetResponse {
    fn from(model: &MonitorPresetModel) -> Self {
        Self {
            id: model.id,
            model_id: model.model_id,
            name: model.name.clone(),
            r#type: model.r#type.clone(),
            device: model.device.clone(),
            channel: model.channel,
            format: model.format,
            protocol: model.protocol.clone(),
            method: model.method.clone(),
            host: model.host.clone(),
            port: model.port.clone(),
            path: model.path.clone(),
            sub_path: model.sub_path.clone(),
            width: model.width,
            height: model.height,
            palette: model.palette,
            max_fps: model.max_fps,
            controllable: model.controllable,
            control_id: model.control_id.clone(),
            control_device: model.control_device.clone(),
            control_address: model.control_address.clone(),
            default_rate: model.default_rate,
            default_scale: model.default_scale.clone(),
        }
    }
}

/// Paginated response for monitor presets
#[derive(Debug, Clone, Deserialize, Serialize, ToSchema)]
pub struct PaginatedMonitorPresetsResponse {
    pub items: Vec<MonitorPresetResponse>,
    pub total: u64,
    pub per_page: u64,
    pub current_page: u64,
    pub last_page: u64,
}

impl From<PaginatedResponse<MonitorPresetResponse>> for PaginatedMonitorPresetsResponse {
    fn from(r: PaginatedResponse<MonitorPresetResponse>) -> Self {
        Self {
            items: r.items,
            total: r.total,
            per_page: r.per_page,
            current_page: r.current_page,
            last_page: r.last_page,
        }
    }
}
