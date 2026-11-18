//! Multi-server installs: how this zm-api reaches the zm-api on each other
//! ZoneMinder server.
//!
//! Shared memory, zmc's stream socket and the daemon control socket exist
//! only on the server that runs a monitor. A request for another server's
//! monitor is relayed to the zm-api there (see `crate::cluster`). Nothing
//! here matters on a single-server install: with no server id, every monitor
//! is local.

use std::collections::HashMap;
use std::path::PathBuf;

use serde::Deserialize;

#[derive(Debug, Deserialize, Clone)]
pub struct ClusterConfig {
    /// Relay requests for another server's monitors to that server. Off
    /// leaves them to fail locally, as they did before.
    #[serde(default = "default_true")]
    pub enabled: bool,

    /// The port zm-api listens on at the other servers. Defaults to this
    /// server's own port.
    #[serde(default)]
    pub peer_port: Option<u16>,

    /// How long to wait for a connection to another server.
    #[serde(default = "default_connect_timeout_secs")]
    pub connect_timeout_secs: u64,

    /// How long another server may go without sending anything.
    #[serde(default = "default_read_timeout_secs")]
    pub read_timeout_secs: u64,

    /// A PEM file of extra root certificates to trust for the other servers,
    /// for a private CA on the LAN.
    #[serde(default)]
    pub tls_ca_file: Option<PathBuf>,

    /// A base URL per `Servers.Id`, for a server whose zm-api isn't at
    /// `<scheme>://<Servers.Hostname>:<peer_port>`.
    #[serde(default)]
    pub peers: HashMap<String, String>,
}

impl Default for ClusterConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            peer_port: None,
            connect_timeout_secs: default_connect_timeout_secs(),
            read_timeout_secs: default_read_timeout_secs(),
            tls_ca_file: None,
            peers: HashMap::new(),
        }
    }
}

impl ClusterConfig {
    /// The configured base URLs by server id. An entry whose key isn't a
    /// number is dropped with a warning.
    pub fn peer_overrides(&self) -> HashMap<u32, String> {
        self.peers
            .iter()
            .filter_map(|(id, url)| match id.parse::<u32>() {
                Ok(id) => Some((id, url.trim_end_matches('/').to_string())),
                Err(_) => {
                    tracing::warn!("cluster.peers: {id:?} is not a Servers.Id; ignored");
                    None
                }
            })
            .collect()
    }
}

fn default_true() -> bool {
    true
}

fn default_connect_timeout_secs() -> u64 {
    5
}

fn default_read_timeout_secs() -> u64 {
    30
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overrides_are_keyed_by_server_id_and_lose_a_trailing_slash() {
        let mut c = ClusterConfig::default();
        c.peers.insert("2".into(), "https://zm2.lan:8443/".into());
        c.peers.insert("two".into(), "https://nope".into());
        let o = c.peer_overrides();
        assert_eq!(o.len(), 1);
        assert_eq!(o[&2], "https://zm2.lan:8443");
    }
}
