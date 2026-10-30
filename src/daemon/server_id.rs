//! Which `Servers` row is this host.
//!
//! ZoneMinder's own daemons learn their server identity from `zm.conf`:
//! `ZM_SERVER_ID` directly, else `ZM_SERVER_HOST` looked up against
//! `Servers.Name`. zm-api used to hard-code `None` for the daemon manager and
//! read an environment variable nothing sets for the stats job (#75, #100),
//! so on a multi-server install every host started every monitor's zmc and
//! logged stats under `ServerId = 0`. This resolves it the way ZoneMinder
//! does, with the machine hostname as a last resort, and `None` on a
//! single-server install (no `Servers` rows), where the per-server gates are
//! meant to be ignored.

use sea_orm::{DatabaseConnection, EntityTrait};
use tracing::{info, warn};

use crate::configure::zmconf::ZmConfig;
use crate::entity::servers;

/// One `Servers` row, reduced to what identity matching needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerCandidate {
    pub id: u32,
    pub name: String,
    pub hostname: Option<String>,
}

/// Pick this host's server id from the available evidence, in ZoneMinder's
/// order of precedence: an explicit `ZM_SERVER_ID`, then `ZM_SERVER_HOST`
/// matched against `Servers`, then the machine's own hostname. A name matches
/// a row when it equals `Name` or `Hostname` case-insensitively, or when the
/// short labels (before the first `.`) do — `cam1` and `cam1.lan` are the same
/// box, and both spellings turn up in `Servers` tables in the wild.
pub fn pick_server_id(
    conf_id: Option<&str>,
    conf_host: Option<&str>,
    machine_hostname: Option<&str>,
    servers: &[ServerCandidate],
) -> Option<u32> {
    if let Some(id) = conf_id.and_then(|v| v.trim().parse::<u32>().ok()) {
        if id != 0 {
            return Some(id);
        }
    }
    conf_host
        .and_then(|h| match_server(h, servers))
        .or_else(|| machine_hostname.and_then(|h| match_server(h, servers)))
}

fn match_server(host: &str, servers: &[ServerCandidate]) -> Option<u32> {
    let host = host.trim();
    if host.is_empty() {
        return None;
    }
    fn same(a: &str, b: &str) -> bool {
        a.eq_ignore_ascii_case(b)
    }
    fn short(s: &str) -> &str {
        s.split('.').next().unwrap_or(s)
    }

    let exact = servers
        .iter()
        .find(|s| same(&s.name, host) || s.hostname.as_deref().is_some_and(|h| same(h, host)));
    if let Some(s) = exact {
        return Some(s.id);
    }
    servers
        .iter()
        .find(|s| {
            same(short(&s.name), short(host))
                || s.hostname
                    .as_deref()
                    .is_some_and(|h| same(short(h), short(host)))
        })
        .map(|s| s.id)
}

/// The server id ZoneMinder's own Perl daemons use (`ZoneMinder::Config`):
/// `ZM_SERVER_ID` when set and not 0, else the `Servers` row whose `Name`
/// is `ZM_SERVER_NAME`, else the one whose `Name` is `ZM_SERVER_HOST`. No
/// hostname fallback, unlike [`pick_server_id`]. zmdc.pl names its socket
/// after this id, so this is what finding that socket needs.
pub fn zoneminder_server_id(
    conf_id: Option<&str>,
    conf_name: Option<&str>,
    conf_host: Option<&str>,
    servers: &[ServerCandidate],
) -> Option<u32> {
    if let Some(id) = conf_id.and_then(|v| v.trim().parse::<u32>().ok()) {
        if id != 0 {
            return Some(id);
        }
    }
    // `WHERE Name=?` under MySQL's case-insensitive collation.
    let by_name = |name: &str| {
        let name = name.trim();
        servers
            .iter()
            .find(|s| s.name.eq_ignore_ascii_case(name))
            .map(|s| s.id)
    };
    match (conf_name.filter(|n| !n.trim().is_empty()), conf_host) {
        (Some(name), _) => by_name(name),
        (None, Some(host)) if !host.trim().is_empty() => by_name(host),
        _ => None,
    }
}

/// zmdc.pl's socket file name: `zmdc<ZM_SERVER_ID>.sock` on a host with a
/// server id, `zmdc.sock` otherwise (`scripts/zmdc.pl.in`, `SOCK_FILE`).
pub fn zmdc_socket_name(zoneminder_id: Option<u32>) -> String {
    match zoneminder_id {
        Some(id) => format!("zmdc{id}.sock"),
        None => "zmdc.sock".to_string(),
    }
}

/// Who this host is in `Servers`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ThisServer {
    /// This host's `Servers.Id` by [`pick_server_id`]; `None` on a
    /// single-server install or when nothing matches.
    pub id: Option<u32>,
    /// The id ZoneMinder's daemons use, which names zmdc.pl's socket.
    pub zoneminder_id: Option<u32>,
}

/// Resolve this host's `Servers.Id` from `zm.conf`, the `Servers` table and
/// the machine hostname. `None` means single-server (or unresolvable, which
/// is logged so a multi-server operator can see why the gates are open).
pub async fn resolve_server_id(db: &DatabaseConnection) -> Option<u32> {
    resolve_this_server(db).await.id
}

/// [`resolve_server_id`], plus the id ZoneMinder's daemons go by.
pub async fn resolve_this_server(db: &DatabaseConnection) -> ThisServer {
    let conf = ZmConfig::load();
    let rows = match servers::Entity::find().all(db).await {
        Ok(rows) => rows,
        Err(e) => {
            warn!("Could not read Servers to resolve this host's server id: {e}");
            Vec::new()
        }
    };
    let candidates: Vec<ServerCandidate> = rows
        .into_iter()
        .map(|s| ServerCandidate {
            id: s.id,
            name: s.name,
            hostname: s.hostname,
        })
        .collect();
    let zoneminder_id = zoneminder_server_id(
        conf.get("ZM_SERVER_ID"),
        conf.get("ZM_SERVER_NAME"),
        conf.get("ZM_SERVER_HOST"),
        &candidates,
    );
    if candidates.is_empty() {
        // Single-server install. zmdc.pl still goes by ZM_SERVER_ID if set.
        return ThisServer {
            id: None,
            zoneminder_id,
        };
    }
    let machine = nix::unistd::gethostname()
        .ok()
        .and_then(|h| h.into_string().ok());

    let picked = pick_server_id(
        conf.get("ZM_SERVER_ID"),
        conf.get("ZM_SERVER_HOST"),
        machine.as_deref(),
        &candidates,
    );
    match picked {
        Some(id) => info!("This host is Servers.Id {id}"),
        None => warn!(
            "Servers has {} rows but none matches ZM_SERVER_ID, ZM_SERVER_HOST or hostname {:?}; \
             per-server daemon gates and ServerId filtering are off",
            candidates.len(),
            machine
        ),
    }
    ThisServer {
        id: picked,
        zoneminder_id,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn servers() -> Vec<ServerCandidate> {
        vec![
            ServerCandidate {
                id: 1,
                name: "nvr-a".into(),
                hostname: Some("nvr-a.lan".into()),
            },
            ServerCandidate {
                id: 2,
                name: "nvr-b".into(),
                hostname: None,
            },
        ]
    }

    #[test]
    fn zoneminders_id_follows_its_perl_rule() {
        let s = servers();
        assert_eq!(
            zoneminder_server_id(Some("7"), Some("nvr-a"), None, &s),
            Some(7)
        );
        assert_eq!(
            zoneminder_server_id(Some("0"), Some("NVR-B"), None, &s),
            Some(2)
        );
        // ZM_SERVER_NAME is tried before ZM_SERVER_HOST, and only it.
        assert_eq!(
            zoneminder_server_id(None, Some("nope"), Some("nvr-a"), &s),
            None
        );
        assert_eq!(zoneminder_server_id(None, None, Some("nvr-a"), &s), Some(1));
        // Name only: no Hostname or short-label matching, no machine hostname.
        assert_eq!(
            zoneminder_server_id(None, None, Some("nvr-a.lan"), &s),
            None
        );
        assert_eq!(zoneminder_server_id(None, None, None, &s), None);
        // A ZM_SERVER_ID with no Servers rows still names the socket.
        assert_eq!(zoneminder_server_id(Some("3"), None, None, &[]), Some(3));
    }

    #[test]
    fn zmdc_socket_is_named_after_the_server_id() {
        assert_eq!(zmdc_socket_name(Some(2)), "zmdc2.sock");
        assert_eq!(zmdc_socket_name(None), "zmdc.sock");
    }

    #[test]
    fn explicit_zm_server_id_wins() {
        assert_eq!(
            pick_server_id(Some("2"), Some("nvr-a"), Some("nvr-a"), &servers()),
            Some(2)
        );
    }

    #[test]
    fn zero_or_garbage_zm_server_id_is_ignored() {
        assert_eq!(
            pick_server_id(Some("0"), Some("nvr-a"), None, &servers()),
            Some(1)
        );
        assert_eq!(
            pick_server_id(Some("x"), None, Some("nvr-b"), &servers()),
            Some(2)
        );
    }

    #[test]
    fn zm_server_host_matches_name_or_hostname_case_insensitively() {
        assert_eq!(
            pick_server_id(None, Some("NVR-B"), None, &servers()),
            Some(2)
        );
        assert_eq!(
            pick_server_id(None, Some("nvr-a.lan"), None, &servers()),
            Some(1)
        );
    }

    #[test]
    fn machine_hostname_is_the_fallback_and_short_labels_match() {
        assert_eq!(
            pick_server_id(None, None, Some("nvr-b.example.com"), &servers()),
            Some(2)
        );
        assert_eq!(
            pick_server_id(None, None, Some("nvr-a"), &servers()),
            Some(1)
        );
    }

    #[test]
    fn no_match_and_no_rows_are_single_server() {
        assert_eq!(pick_server_id(None, None, Some("other"), &servers()), None);
        assert_eq!(pick_server_id(None, Some("nvr-a"), None, &[]), None);
    }
}
