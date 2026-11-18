//! zm-api's RTSP server: a monitor's live video at
//! `rtsp://host:port/live/{monitor_id}?st=<signed link>`, for players that
//! only take a URL (Home Assistant, VLC, NVRs).
//!
//! The credential is the same signed link as live HLS
//! (`util::media_url`): one monitor, view only, an expiry, and revoked with
//! its user. A leaked URL opens one camera's live view and nothing else,
//! which ZoneMinder's `zm_rtsp_server` (username and password in the URL)
//! can't offer. Video only, H.264 or H.265, RTP over the RTSP connection
//! (interleaved TCP); RTSPS when a port and `server.tls` are configured.

pub mod message;
pub mod rtp;
pub mod sdp;
pub mod session;

use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use tokio::net::TcpListener;
use tokio::sync::{watch, Semaphore};
use tokio_rustls::TlsAcceptor;
use tracing::{error, info, warn};

use crate::error::AppError;
use crate::server::state::AppState;
use crate::service::media_grant::{self, RouteResource};
pub use session::{Backend, Grant, Media};

/// How long a new RTSPS connection has to finish its TLS handshake.
const TLS_HANDSHAKE: Duration = Duration::from_secs(10);
/// How long `DESCRIBE` waits for the monitor's first keyframe.
const KEYFRAME_WAIT: Duration = Duration::from_secs(10);

/// The backend that serves real monitors: signed links checked by
/// `media_grant::authorize`, video from the stream-socket router.
#[derive(Clone)]
pub struct ZmBackend {
    state: AppState,
}

fn status(e: &AppError) -> u16 {
    match e {
        AppError::UnauthorizedError(_) => 401,
        AppError::PermissionDeniedError(_) => 403,
        AppError::NotFoundError(_) => 404,
        AppError::ServiceUnavailableError(_) => 503,
        _ => 500,
    }
}

impl Backend for ZmBackend {
    async fn authorize(&self, token: &str, monitor_id: u32) -> Result<Grant, u16> {
        media_grant::authorize(&self.state, token, RouteResource::Monitor(monitor_id))
            .await
            .map(|g| Grant {
                uid: g.uid,
                exp: g.exp,
            })
            .map_err(|e| status(&e))
    }

    async fn open(&self, monitor_id: u32) -> Result<Media, u16> {
        let router = self.state.source_router.as_ref().ok_or(503u16)?;
        if !router.is_available(monitor_id) {
            return Err(404);
        }
        router.ensure_warm(monitor_id).await;
        let source = router.get_source(monitor_id).await.map_err(|e| {
            warn!("rtsp: monitor {monitor_id}: {e}");
            503u16
        })?;
        // Subscribe first, so nothing between the keyframe and the live
        // packets is missed.
        let packets = source.subscribe_video();
        let keyframe = match source.cached_keyframe() {
            Some(k) => k,
            None => {
                let mut cache = source.subscribe_keyframe_cache();
                tokio::time::timeout(KEYFRAME_WAIT, cache.wait_for(|k| k.is_some()))
                    .await
                    .ok()
                    .and_then(|r| r.ok().and_then(|k| k.clone()))
                    .ok_or(503u16)?
            }
        };
        Ok(Media {
            codec: keyframe.codec,
            keyframe: keyframe.keyframe_au,
            keyframe_us: keyframe.timestamp_us,
            packets,
        })
    }
}

fn tls_acceptor(cert: &Path, key: &Path) -> Result<TlsAcceptor, String> {
    use rustls_pki_types::pem::PemObject;
    use rustls_pki_types::{CertificateDer, PrivateKeyDer};
    let certs = CertificateDer::pem_file_iter(cert)
        .and_then(|it| it.collect::<Result<Vec<_>, _>>())
        .map_err(|e| format!("{}: {e}", cert.display()))?;
    let key = PrivateKeyDer::from_pem_file(key).map_err(|e| format!("{}: {e}", key.display()))?;
    let config = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .map_err(|e| e.to_string())?
    .with_no_client_auth()
    .with_single_cert(certs, key)
    .map_err(|e| e.to_string())?;
    Ok(TlsAcceptor::from(Arc::new(config)))
}

/// Start the configured listeners. Returns at once; they stop when
/// `shutdown` turns true.
pub async fn start(state: AppState, shutdown: watch::Receiver<bool>) {
    let cfg = state.config.streaming.rtsp.clone();
    if !cfg.enabled {
        return;
    }
    if state.source_router.is_none() {
        warn!("streaming.rtsp is enabled but live streaming is off; not starting RTSP");
        return;
    }
    let backend = ZmBackend {
        state: state.clone(),
    };
    let slots = Arc::new(Semaphore::new(cfg.max_connections.max(1)));
    // Shared by the plain and TLS listeners: an address is one client.
    let per_address = PerAddress::default();

    let tls = match cfg.tls_port {
        None => None,
        Some(port) => {
            let paths = state
                .config
                .server
                .tls
                .as_ref()
                .and_then(|t| Some((t.cert_path.clone()?, t.key_path.clone()?)));
            match paths.map(|(c, k)| tls_acceptor(&c, &k)) {
                Some(Ok(acceptor)) => Some((port, acceptor)),
                Some(Err(e)) => {
                    error!("streaming.rtsp.tls_port: can't load the certificate: {e}");
                    None
                }
                None => {
                    error!("streaming.rtsp.tls_port needs server.tls.cert_path and key_path");
                    None
                }
            }
        }
    };

    let listeners =
        std::iter::once((cfg.port, None)).chain(tls.map(|(port, acceptor)| (port, Some(acceptor))));
    for (port, acceptor) in listeners {
        let addr = format!("{}:{port}", cfg.bind);
        let listener = match TcpListener::bind(&addr).await {
            Ok(l) => l,
            Err(e) => {
                error!("rtsp: can't listen on {addr}: {e}");
                continue;
            }
        };
        let scheme = if acceptor.is_some() { "rtsps" } else { "rtsp" };
        info!("Starting {scheme} server on {addr}");
        tokio::spawn(accept_loop(
            listener,
            acceptor,
            backend.clone(),
            slots.clone(),
            per_address.clone(),
            shutdown.clone(),
        ));
    }
}

/// The most connections one address may hold at a time. A connection takes a
/// slot before it has shown a signed link, and keeps it for up to 15 seconds
/// without one, so with only the overall limit a single host could hold every
/// slot and shut all viewers out. Sixteen leaves room for a recorder pulling
/// many cameras from one address.
const MAX_PER_ADDRESS: usize = 16;

/// How many connections each address holds.
#[derive(Clone, Default)]
struct PerAddress(Arc<std::sync::Mutex<std::collections::HashMap<std::net::IpAddr, usize>>>);

/// One address's connection, counted until dropped.
struct AddressSlot {
    counts: PerAddress,
    ip: std::net::IpAddr,
}

impl PerAddress {
    /// A slot for `ip`, unless it already holds `limit`.
    fn try_acquire(&self, ip: std::net::IpAddr, limit: usize) -> Option<AddressSlot> {
        let mut counts = self.0.lock().unwrap_or_else(|p| p.into_inner());
        let held = counts.entry(ip).or_insert(0);
        if *held >= limit {
            return None;
        }
        *held += 1;
        Some(AddressSlot {
            counts: self.clone(),
            ip,
        })
    }
}

impl Drop for AddressSlot {
    fn drop(&mut self) {
        let mut counts = self.counts.0.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(held) = counts.get_mut(&self.ip) {
            *held -= 1;
            if *held == 0 {
                counts.remove(&self.ip);
            }
        }
    }
}

async fn accept_loop(
    listener: TcpListener,
    tls: Option<TlsAcceptor>,
    backend: ZmBackend,
    slots: Arc<Semaphore>,
    per_address: PerAddress,
    mut shutdown: watch::Receiver<bool>,
) {
    loop {
        let (sock, peer): (_, SocketAddr) = tokio::select! {
            r = listener.accept() => match r {
                Ok(c) => c,
                Err(e) => {
                    warn!("rtsp: accept failed: {e}");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    continue;
                }
            },
            _ = async { let _ = shutdown.wait_for(|v| *v).await; } => return,
        };
        let Some(address_slot) = per_address.try_acquire(peer.ip(), MAX_PER_ADDRESS) else {
            warn!("rtsp: {peer} already holds {MAX_PER_ADDRESS} connections; refusing another");
            continue;
        };
        let Ok(permit) = slots.clone().try_acquire_owned() else {
            warn!("rtsp: connection limit reached; refusing {peer}");
            continue;
        };
        let _ = sock.set_nodelay(true);
        let backend = backend.clone();
        let tls = tls.clone();
        tokio::spawn(async move {
            let _permit = permit;
            let _address_slot = address_slot;
            let peer = peer.to_string();
            match tls {
                None => session::serve(sock, backend, peer).await,
                Some(acceptor) => {
                    match tokio::time::timeout(TLS_HANDSHAKE, acceptor.accept(sock)).await {
                        Ok(Ok(stream)) => session::serve(stream, backend, peer).await,
                        _ => tracing::debug!("rtsps {peer}: TLS handshake failed"),
                    }
                }
            }
        });
    }
}

/// The links for `monitor_id` with signed link `token`: plain RTSP, and
/// RTSPS when configured. `host` is the configured public host, or else the
/// host the request came to.
pub fn link_urls(
    cfg: &crate::configure::streaming::RtspConfig,
    host: &str,
    monitor_id: u32,
    token: &str,
) -> (String, Option<String>) {
    let path = format!(
        "/live/{monitor_id}?{}={}",
        message::TOKEN_PARAM,
        urlencoding::encode(token)
    );
    let host = cfg.public_host.as_deref().unwrap_or(host);
    let host = if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]")
    } else {
        host.to_string()
    };
    (
        format!("rtsp://{host}:{}{path}", cfg.port),
        cfg.tls_port.map(|p| format!("rtsps://{host}:{p}{path}")),
    )
}

/// The host name of an HTTP `Host` header, without its port, if it only
/// holds characters a host name or address can.
pub fn host_from_header(value: &str) -> Option<String> {
    let host = if let Some(rest) = value.strip_prefix('[') {
        rest.split(']').next()?
    } else {
        value.rsplit_once(':').map(|(h, _)| h).unwrap_or(value)
    };
    let ok = !host.is_empty()
        && host.len() <= 253
        && host
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b':'));
    ok.then(|| host.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::configure::streaming::RtspConfig;

    #[test]
    fn one_address_cannot_take_every_connection() {
        let counts = PerAddress::default();
        let hog: std::net::IpAddr = "203.0.113.7".parse().unwrap();
        let viewer: std::net::IpAddr = "198.51.100.2".parse().unwrap();

        let held: Vec<_> = (0..3)
            .map(|_| counts.try_acquire(hog, 3).unwrap())
            .collect();
        assert!(counts.try_acquire(hog, 3).is_none());
        // Someone else is not affected.
        assert!(counts.try_acquire(viewer, 3).is_some());
        // A closed connection frees its place, and an idle address is forgotten.
        drop(held);
        assert!(counts.try_acquire(hog, 3).is_some());
        assert!(counts.0.lock().unwrap().is_empty());
    }

    #[test]
    fn links_use_the_public_host_or_the_request_host() {
        let mut cfg = RtspConfig::default();
        let (url, tls) = link_urls(&cfg, "zm.lan", 3, "1.m.3.a+b");
        assert_eq!(url, "rtsp://zm.lan:8554/live/3?st=1.m.3.a%2Bb");
        assert_eq!(tls, None);
        cfg.public_host = Some("cams.example.com".into());
        cfg.tls_port = Some(8322);
        let (url, tls) = link_urls(&cfg, "ignored", 3, "t");
        assert_eq!(url, "rtsp://cams.example.com:8554/live/3?st=t");
        assert_eq!(
            tls.as_deref(),
            Some("rtsps://cams.example.com:8322/live/3?st=t")
        );
        cfg.public_host = None;
        let (url, _) = link_urls(&cfg, "fe80::1", 3, "t");
        assert_eq!(url, "rtsp://[fe80::1]:8554/live/3?st=t");
    }

    #[test]
    fn host_headers() {
        assert_eq!(host_from_header("zm.lan:8080").as_deref(), Some("zm.lan"));
        assert_eq!(host_from_header("10.0.0.5").as_deref(), Some("10.0.0.5"));
        assert_eq!(
            host_from_header("[fe80::1]:443").as_deref(),
            Some("fe80::1")
        );
        for bad in ["", "a b", "x/y:80", "evil\r\nX: y", ":80"] {
            assert_eq!(host_from_header(bad), None, "{bad:?}");
        }
    }
}
