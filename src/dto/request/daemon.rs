//! Request DTOs for daemon controller API.

use garde::Validate;
use serde::Deserialize;
use utoipa::ToSchema;

/// Request to start a daemon.
#[derive(Debug, Deserialize, ToSchema)]
pub struct StartDaemonRequest {
    /// Optional additional arguments
    #[serde(default)]
    pub args: Vec<String>,
}

/// Request to apply a system state.
#[derive(Debug, Deserialize, ToSchema, Validate)]
pub struct ApplyStateRequest {
    /// Name of the state to apply
    #[garde(length(chars, max = 64))]
    pub state_name: String,
    /// Leave the database alone and only restart this server's daemons for
    /// the monitors the state names. This is what a server sends the other
    /// servers after it has applied the state, so the change is written once.
    /// It is handled on this server and never passed on.
    #[serde(default)]
    #[garde(skip)]
    pub daemons_only: bool,
}
