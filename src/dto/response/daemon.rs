//! Response DTOs for daemon controller API.

use serde::Serialize;
use utoipa::ToSchema;

use crate::daemon::ipc::{ProcessStatus, SystemStats, SystemStatus};
use crate::daemon::ProcessState;

/// Response containing a single daemon's status.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct DaemonStatusResponse {
    /// Process identifier
    pub id: String,
    /// Display name
    pub name: String,
    /// Current state
    pub state: ProcessState,
    /// Process ID if running
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    /// Uptime in seconds if running
    #[serde(skip_serializing_if = "Option::is_none")]
    pub uptime_seconds: Option<u64>,
    /// Number of restart attempts
    pub restart_count: u32,
    /// Associated monitor ID if applicable
    #[serde(skip_serializing_if = "Option::is_none")]
    pub monitor_id: Option<u32>,
    /// Exit status of the most recent exit, when it exited normally
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_exit_code: Option<i32>,
    /// Why supervision stopped restarting this daemon. It stays stopped until
    /// started again explicitly.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure_reason: Option<String>,
}

impl From<ProcessStatus> for DaemonStatusResponse {
    fn from(status: ProcessStatus) -> Self {
        Self {
            id: status.id,
            name: status.name,
            state: status.state,
            pid: status.pid,
            uptime_seconds: status.uptime_seconds,
            restart_count: status.restart_count,
            monitor_id: status.monitor_id,
            last_exit_code: status.last_exit_code,
            failure_reason: status.failure_reason,
        }
    }
}

/// Response containing list of all daemons.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct DaemonListResponse {
    /// List of daemon statuses
    pub daemons: Vec<DaemonStatusResponse>,
}

/// Response containing system-wide status.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct SystemStatusResponse {
    /// Whether the daemon controller is running
    pub running: bool,
    /// Status of all managed daemons
    pub daemons: Vec<DaemonStatusResponse>,
    /// System statistics
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stats: Option<SystemStatsResponse>,
}

impl From<SystemStatus> for SystemStatusResponse {
    fn from(status: SystemStatus) -> Self {
        Self {
            running: status.running,
            daemons: status.daemons.into_iter().map(Into::into).collect(),
            stats: status.stats.map(Into::into),
        }
    }
}

/// Response containing system statistics.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct SystemStatsResponse {
    /// CPU load average (1 minute)
    pub cpu_load: f64,
    /// CPU usage percentage
    pub cpu_usage_percent: f64,
    /// Total memory in bytes
    pub total_mem: u64,
    /// Free memory in bytes
    pub free_mem: u64,
    /// Total swap in bytes
    pub total_swap: u64,
    /// Free swap in bytes
    pub free_swap: u64,
    /// Total disk space in bytes (root filesystem)
    pub total_disk: u64,
    /// Used disk space in bytes
    pub used_disk: u64,
    /// Free disk space in bytes
    pub free_disk: u64,
    /// Disk usage percentage
    pub disk_usage_percent: f64,
    /// Database connections currently held by the pool (the legacy header's
    /// `DB: n/max`). `None` when the pool cannot be inspected.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub db_connections: Option<u32>,
    /// Configured maximum size of the database connection pool.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub db_max_connections: Option<u32>,
}

impl From<SystemStats> for SystemStatsResponse {
    fn from(stats: SystemStats) -> Self {
        Self {
            cpu_load: stats.cpu_load,
            cpu_usage_percent: stats.cpu_usage_percent,
            total_mem: stats.total_mem,
            free_mem: stats.free_mem,
            total_swap: stats.total_swap,
            free_swap: stats.free_swap,
            total_disk: stats.total_disk,
            used_disk: stats.used_disk,
            db_connections: None,
            db_max_connections: None,
            free_disk: stats.free_disk,
            disk_usage_percent: stats.disk_usage_percent,
        }
    }
}

/// Response for daemon control actions.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct DaemonActionResponse {
    /// Whether the action succeeded
    pub success: bool,
    /// Human-readable message
    pub message: String,
    /// What the other servers of a multi-server install did, when the action
    /// reaches them too (applying a run state does). `success` and `message`
    /// describe this server only. Absent on a single-server install and for
    /// actions that stay on one server.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub servers: Option<Vec<ServerResult>>,
}

impl DaemonActionResponse {
    /// Create a success response.
    pub fn success(message: impl Into<String>) -> Self {
        Self {
            success: true,
            message: message.into(),
            servers: None,
        }
    }

    /// Create an error response.
    pub fn error(message: impl Into<String>) -> Self {
        Self {
            success: false,
            message: message.into(),
            servers: None,
        }
    }
}

/// What one server of a multi-server install answered to a request that went
/// to several of them.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct ServerResult {
    /// The server's `Servers.Id`. Null for the one server of an install whose
    /// host has no server id.
    pub server_id: Option<u32>,
    /// The server's name.
    pub name: String,
    /// Whether the server answered with a success status.
    pub ok: bool,
    /// The HTTP status the server answered with, or the one this server
    /// gives for not reaching it (502 no answer, 421 no address).
    pub status: u16,
    /// The JSON the server answered, as it would for the same request sent to
    /// it alone.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<serde_json::Value>)]
    pub result: Option<serde_json::Value>,
    /// Why the request failed on this server.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl ServerResult {
    /// A server answered with `status` and `body`.
    pub fn answered(server_id: Option<u32>, name: String, status: u16, body: &[u8]) -> Self {
        let ok = (200..300).contains(&status);
        let json: Option<serde_json::Value> = serde_json::from_slice(body).ok();
        let error = (!ok).then(|| {
            json.as_ref()
                .and_then(|v| v.get("error_message").or_else(|| v.get("message")))
                .and_then(|m| m.as_str())
                .map_or_else(|| format!("answered {status}"), str::to_string)
        });
        Self {
            server_id,
            name,
            ok,
            status,
            result: if ok { json } else { None },
            error,
        }
    }

    /// The request never got an answer from the server.
    pub fn failed(server_id: Option<u32>, name: String, status: u16, error: String) -> Self {
        Self {
            server_id,
            name,
            ok: false,
            status,
            result: None,
            error: Some(error),
        }
    }
}

/// The answer to a request sent with `server_id=all`: one entry per server,
/// this server first, then the others by id. The HTTP status is 207 whenever
/// the request ran, even if every server failed; each entry says how it went.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct AllServersResponse {
    pub servers: Vec<ServerResult>,
}

/// Which server a request is for.
#[derive(Debug, Clone, utoipa::IntoParams)]
#[allow(dead_code)]
#[into_params(parameter_in = Query)]
pub struct ServerTargetQuery {
    /// Absent: this server. A server's number: that server, relayed to its
    /// zm-api when it is not this one. `all`: every server, answered with 207
    /// and one entry per server (see `AllServersResponse`). A request that
    /// another server already relayed is always handled here.
    pub server_id: Option<String>,
}
