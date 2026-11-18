//! Response DTO for the per-monitor zm-next processing-graph endpoints.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use utoipa::ToSchema;

use crate::entity::monitor_pipeline;

/// A monitor's stored zm-next processing plugin graph (the "free graph").
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct MonitorPipelineResponse {
    pub monitor_id: u32,
    /// Document/schema version.
    pub version: u32,
    /// The processing plugin graph document: `{ "plugins": [ {id,kind,cfg,children}, ... ] }`.
    #[schema(value_type = Object)]
    pub graph: Value,
    /// RFC 3339 timestamps.
    pub created_at: String,
    pub updated_at: String,
}

/// The column holds UTC digits; label them as the instant they are.
fn rfc3339(column: sea_orm::prelude::DateTime) -> String {
    crate::util::time::rfc3339_offset(crate::util::time::utc_instant(column))
}

impl From<monitor_pipeline::Model> for MonitorPipelineResponse {
    fn from(m: monitor_pipeline::Model) -> Self {
        // The stored graph is validated on write, so it parses; fall back to Null
        // only defensively (e.g. a hand-edited DB row).
        let graph = serde_json::from_str(&m.graph_json).unwrap_or(Value::Null);
        Self {
            monitor_id: m.monitor_id,
            version: m.version,
            graph,
            created_at: rfc3339(m.created_at),
            updated_at: rfc3339(m.updated_at),
        }
    }
}

/// A monitor's zm-next worker, as the daemon manager sees it.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct ZmNextWorkerStatusResponse {
    pub monitor_id: u32,
    /// `UseZmNext` is set and `[zmnext].enabled` is on.
    pub use_zmnext: bool,
    /// zm-api supervises daemons on this server (not passive mode).
    pub supervised: bool,
    /// The worker's process entry; absent if it was never started.
    pub worker: Option<crate::dto::response::daemon::DaemonStatusResponse>,
    /// What the worker last reported over its socket (hello and status
    /// events): state, pipeline hash, per-stream health, degraded components.
    /// Absent until zm-api has read from the monitor's socket.
    pub live: Option<crate::streaming::source::worker_status::WorkerStatus>,
}

/// Result of checking a pipeline graph without saving it.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct PipelineValidationResponse {
    pub valid: bool,
    /// `builtin` (zm-api's plugin list, for a worker that offers no schemas) or
    /// the zm-next version whose plugin schemas were used.
    pub checked_against: String,
    pub errors: Vec<crate::service::zmnext::control::PathError>,
}
