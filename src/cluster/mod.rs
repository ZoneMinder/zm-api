//! Multi-server installs: which server owns a monitor, and how to reach the
//! zm-api there.
//!
//! A ZoneMinder install can spread its monitors over several servers that
//! share one database. Shared memory, zmc's stream socket and the daemon
//! control socket exist only on the server that runs a monitor, so a zm-api
//! can serve live video, snapshots, alarms and daemon control for its own
//! monitors alone (#230). Every server runs a zm-api, and a request that
//! reaches the wrong one is relayed to the right one ([`forward`]).
//!
//! The servers talk over the v3 API itself, with the caller's own
//! credentials: the owning server authenticates the user and applies their
//! permissions exactly as it would for a direct request. No server holds a
//! credential that can act as anyone. That requires every server to share
//! the same JWT key files.

pub mod forward;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use sea_orm::DatabaseConnection;

use crate::configure::cluster::ClusterConfig;
use crate::error::AppResult;

/// How long a monitor's owner and the `Servers` rows are remembered. Short:
/// a monitor moved to another server is followed within seconds.
const CACHE_TTL: Duration = Duration::from_secs(5);

/// Where a monitor's node-local resources live.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Owner {
    /// This server, or no server in particular: handle it here.
    Here,
    /// The server with this `Servers.Id`.
    Peer(u32),
}

/// Which server answers for a monitor.
///
/// A host with no server id is a single-server install, and everything is
/// local. Otherwise a monitor belongs to the server its `ServerId` names. A
/// monitor with no `ServerId` belongs to nobody in particular, so it is
/// handled wherever the request landed.
pub fn owner(this_server: Option<u32>, monitor_server: Option<u32>) -> Owner {
    match (this_server, monitor_server) {
        (None, _) | (_, None) | (_, Some(0)) => Owner::Here,
        (Some(here), Some(there)) if here == there => Owner::Here,
        (Some(_), Some(there)) => Owner::Peer(there),
    }
}

/// Another server's zm-api.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Peer {
    pub id: u32,
    pub name: String,
    /// `https://host:port`, no trailing slash.
    pub base_url: String,
}

impl Peer {
    /// `server 2 (zm2)`, for messages.
    pub fn label(&self) -> String {
        format!("server {} ({})", self.id, self.name)
    }
}

#[derive(Debug, Clone)]
struct ServerRow {
    name: String,
    hostname: Option<String>,
}

type Cached<T> = Mutex<Option<(Instant, T)>>;

pub struct Cluster {
    this_server: Option<u32>,
    enabled: bool,
    /// The scheme and port zm-api is assumed to use on the other servers:
    /// this server's own, unless `cluster.peer_port` says otherwise.
    scheme: &'static str,
    port: u16,
    overrides: HashMap<u32, String>,
    client: reqwest::Client,
    servers: Cached<Arc<HashMap<u32, ServerRow>>>,
    owners: Mutex<HashMap<u32, (Instant, Option<u32>)>>,
}

impl std::fmt::Debug for Cluster {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Cluster")
            .field("this_server", &self.this_server)
            .field("enabled", &self.enabled)
            .finish_non_exhaustive()
    }
}

impl Cluster {
    /// `tls` is whether this server's own listener speaks TLS.
    pub fn new(this_server: Option<u32>, config: &ClusterConfig, own_port: u16, tls: bool) -> Self {
        Self {
            this_server,
            enabled: config.enabled,
            scheme: if tls { "https" } else { "http" },
            port: config.peer_port.unwrap_or(own_port),
            overrides: config.peer_overrides(),
            client: build_client(config),
            servers: Mutex::new(None),
            owners: Mutex::new(HashMap::new()),
        }
    }

    /// A single-server install: nothing is ever relayed.
    pub fn single() -> Self {
        Self::new(None, &ClusterConfig::default(), 0, false)
    }

    pub fn this_server(&self) -> Option<u32> {
        self.this_server
    }

    /// Whether any request could be relayed at all. False on a single-server
    /// install, where the relay layer does no work.
    pub fn relays(&self) -> bool {
        self.enabled && self.this_server.is_some()
    }

    pub(crate) fn client(&self) -> &reqwest::Client {
        &self.client
    }

    /// The server that owns a monitor. `Here` for a monitor that doesn't
    /// exist, so the local handler answers 404 as it always did.
    pub async fn monitor_owner(
        &self,
        db: &DatabaseConnection,
        monitor_id: u32,
    ) -> AppResult<Owner> {
        if !self.relays() {
            return Ok(Owner::Here);
        }
        let cached = self
            .owners
            .lock()
            .unwrap()
            .get(&monitor_id)
            .filter(|(at, _)| at.elapsed() < CACHE_TTL)
            .map(|(_, server)| *server);
        let monitor_server = match cached {
            Some(server) => server,
            None => {
                let server = crate::repo::monitors::find_by_id(db, monitor_id)
                    .await?
                    .and_then(|m| m.server_id);
                let mut owners = self.owners.lock().unwrap();
                owners.retain(|_, (at, _)| at.elapsed() < CACHE_TTL);
                owners.insert(monitor_id, (Instant::now(), server));
                server
            }
        };
        Ok(owner(self.this_server, monitor_server))
    }

    /// Drop what is remembered about a monitor, after its row changed here.
    pub fn forget_monitor(&self, monitor_id: u32) {
        self.owners.lock().unwrap().remove(&monitor_id);
    }

    /// Where another server's zm-api is. `None` when there is no `Servers`
    /// row for it, or the row has no hostname and settings name none.
    pub async fn peer(&self, db: &DatabaseConnection, server_id: u32) -> AppResult<Option<Peer>> {
        let servers = self.servers(db).await?;
        let row = servers.get(&server_id);
        let name = row.map_or_else(|| server_id.to_string(), |r| r.name.clone());
        let hostname = row.and_then(|r| r.hostname.as_deref());
        Ok(self.base_url(server_id, hostname).map(|base_url| Peer {
            id: server_id,
            name,
            base_url,
        }))
    }

    /// The v3 address of a server: the settings override, else this server's
    /// scheme and the peer port on the row's hostname.
    pub fn base_url(&self, server_id: u32, hostname: Option<&str>) -> Option<String> {
        if let Some(url) = self.overrides.get(&server_id) {
            return Some(url.clone());
        }
        let host = hostname.map(str::trim).filter(|h| !h.is_empty())?;
        Some(format!("{}://{}:{}", self.scheme, host, self.port))
    }

    async fn servers(&self, db: &DatabaseConnection) -> AppResult<Arc<HashMap<u32, ServerRow>>> {
        if let Some((at, rows)) = self.servers.lock().unwrap().as_ref() {
            if at.elapsed() < CACHE_TTL {
                return Ok(rows.clone());
            }
        }
        let rows: HashMap<u32, ServerRow> = crate::repo::servers::find_all(db)
            .await?
            .into_iter()
            .map(|s| {
                (
                    s.id,
                    ServerRow {
                        name: s.name,
                        hostname: s.hostname,
                    },
                )
            })
            .collect();
        let rows = Arc::new(rows);
        *self.servers.lock().unwrap() = Some((Instant::now(), rows.clone()));
        Ok(rows)
    }
}

/// How often a server of a multi-server install re-reads the token floors.
const REVOCATION_SYNC: Duration = Duration::from_secs(10);

/// Keep this server's token-revocation floors in step with the database.
///
/// A logout or password change raises `Users.TokenMinExpiry` and the floor
/// in the memory of the server that handled it. Access tokens are checked
/// against memory only, so without this another server would go on accepting
/// the revoked token until it expired. Floors only rise, so re-reading is
/// safe to repeat.
pub fn sync_revocations(
    db: Arc<DatabaseConnection>,
    revocations: Arc<crate::util::revocation::TokenRevocations>,
) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(REVOCATION_SYNC);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            match crate::repo::users::token_floors(db.as_ref()).await {
                Ok(floors) => {
                    for (uid, floor) in floors {
                        revocations.revoke(uid, floor);
                    }
                }
                Err(e) => tracing::debug!("re-reading token floors failed: {e}"),
            }
        }
    });
}

/// The client for requests to the other servers. No overall timeout, because
/// a relayed live stream runs as long as its viewer watches. A server that
/// goes quiet is dropped by the read timeout.
fn build_client(config: &ClusterConfig) -> reqwest::Client {
    let mut builder = reqwest::Client::builder()
        .no_proxy()
        // A relayed response is passed on as it is, redirects included.
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(config.connect_timeout_secs.max(1)))
        .read_timeout(Duration::from_secs(config.read_timeout_secs.max(1)));
    if let Some(path) = &config.tls_ca_file {
        match std::fs::read(path).map(|pem| reqwest::Certificate::from_pem_bundle(&pem)) {
            Ok(Ok(certs)) => {
                for cert in certs {
                    builder = builder.add_root_certificate(cert);
                }
            }
            Ok(Err(e)) => tracing::warn!("cluster.tls_ca_file {}: {e}", path.display()),
            Err(e) => tracing::warn!("cluster.tls_ca_file {}: {e}", path.display()),
        }
    }
    builder.build().unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cluster(this: Option<u32>, config: &ClusterConfig) -> Cluster {
        Cluster::new(this, config, 8443, true)
    }

    #[test]
    fn a_monitor_belongs_to_the_server_its_row_names() {
        assert_eq!(owner(Some(1), Some(1)), Owner::Here);
        assert_eq!(owner(Some(1), Some(2)), Owner::Peer(2));
    }

    #[test]
    fn a_single_server_install_owns_everything() {
        assert_eq!(owner(None, None), Owner::Here);
        assert_eq!(owner(None, Some(2)), Owner::Here);
    }

    /// Nobody in particular owns it, so there is nowhere to relay it to.
    #[test]
    fn a_monitor_with_no_server_is_handled_where_the_request_landed() {
        assert_eq!(owner(Some(1), None), Owner::Here);
        assert_eq!(owner(Some(1), Some(0)), Owner::Here);
    }

    #[test]
    fn a_peer_is_at_this_servers_scheme_and_port_on_its_hostname() {
        let c = cluster(Some(1), &ClusterConfig::default());
        assert_eq!(
            c.base_url(2, Some("zm2.lan")).as_deref(),
            Some("https://zm2.lan:8443")
        );
        let plain = Cluster::new(Some(1), &ClusterConfig::default(), 8080, false);
        assert_eq!(
            plain.base_url(2, Some(" zm2 ")).as_deref(),
            Some("http://zm2:8080")
        );
    }

    #[test]
    fn settings_override_the_address_and_the_port() {
        let mut config = ClusterConfig {
            peer_port: Some(9000),
            ..Default::default()
        };
        config
            .peers
            .insert("3".into(), "https://nvr3.example:444/".into());
        let c = cluster(Some(1), &config);
        assert_eq!(
            c.base_url(2, Some("zm2")).as_deref(),
            Some("https://zm2:9000")
        );
        assert_eq!(
            c.base_url(3, Some("ignored")).as_deref(),
            Some("https://nvr3.example:444")
        );
        // An override works for a server whose row has no hostname.
        assert_eq!(
            c.base_url(3, None).as_deref(),
            Some("https://nvr3.example:444")
        );
    }

    #[test]
    fn a_server_with_no_hostname_and_no_override_has_no_address() {
        let c = cluster(Some(1), &ClusterConfig::default());
        assert_eq!(c.base_url(2, None), None);
        assert_eq!(c.base_url(2, Some("  ")), None);
    }

    #[test]
    fn relaying_needs_a_server_id_and_the_setting() {
        assert!(cluster(Some(1), &ClusterConfig::default()).relays());
        assert!(!cluster(None, &ClusterConfig::default()).relays());
        let off = ClusterConfig {
            enabled: false,
            ..Default::default()
        };
        assert!(!cluster(Some(1), &off).relays());
        assert!(!Cluster::single().relays());
    }
}
