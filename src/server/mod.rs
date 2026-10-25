use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use self::state::AppState;
use crate::configure::server::AcmeChallenge;
use crate::configure::AppConfig;
use crate::daemon::ipc::socket::DaemonSocketServer;
use crate::error::AppResult;
use crate::routes::create_router_app;
use futures::StreamExt;
use rustls_acme::caches::DirCache;
use rustls_acme::{AcmeConfig, UseChallenge};
pub mod state;

/// Resolve once either a Ctrl+C (SIGINT) or a SIGTERM is received.
///
/// Used to trigger graceful shutdown: the server stops accepting new
/// connections and drains in-flight requests before exiting.
async fn shutdown_signal() {
    use tokio::signal;

    let ctrl_c = async {
        signal::ctrl_c()
            .await
            .expect("failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        signal::unix::signal(signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => tracing::info!("Received SIGINT (Ctrl+C)"),
        _ = terminate => tracing::info!("Received SIGTERM"),
    }

    tracing::info!("Shutdown signal received; draining in-flight requests.");
}

/// Spawn a task that triggers `axum_server` graceful shutdown on a signal,
/// bounding the drain with `timeout`.
fn spawn_graceful_shutdown(handle: axum_server::Handle<SocketAddr>, timeout: Duration) {
    tokio::spawn(async move {
        shutdown_signal().await;
        handle.graceful_shutdown(Some(timeout));
    });
}

pub struct AppServer {
    pub state: AppState,
}
impl AppServer {
    pub async fn new(config: AppConfig) -> AppResult<Self> {
        let state = AppState::new(config).await?;
        Ok(Self { state })
    }

    pub async fn run(self) -> AppResult<()> {
        let config = self.state.config.clone();
        let addr = config.server.get_socket_addr()?;
        let tls_config = config.server.tls.clone();
        let acme_config = config.server.acme.clone();
        let shutdown_timeout = Duration::from_secs(config.daemon.shutdown_timeout_seconds);

        // Start daemon-related background services if enabled
        if config.daemon.enabled {
            if let Some(ref daemon_manager) = self.state.daemon_manager {
                // Start the daemon manager (includes health monitoring)
                if let Err(e) = daemon_manager.startup().await {
                    tracing::error!("Failed to start daemon manager: {}", e);
                }

                // Start all ZoneMinder daemons (zmc, zma, singletons, etc.)
                if let Err(e) = daemon_manager.start_all_daemons().await {
                    // A database still coming up at boot must not leave the
                    // singletons and per-filter daemons unstarted for the
                    // life of the process (#114): keep trying.
                    tracing::error!("Failed to start ZoneMinder daemons: {e}; retrying every 30s");
                    let manager = Arc::clone(daemon_manager);
                    tokio::spawn(async move {
                        loop {
                            tokio::time::sleep(Duration::from_secs(30)).await;
                            if manager.is_process_exiting() {
                                break;
                            }
                            match manager.start_all_daemons().await {
                                Ok(_) => {
                                    tracing::info!("ZoneMinder daemons started after retry");
                                    break;
                                }
                                Err(e) => tracing::error!("Retrying daemon start: {e}"),
                            }
                        }
                    });
                }

                // Start the Unix socket server for legacy zmdc.pl compatibility
                if config.daemon.enable_socket_ipc {
                    let socket_path = config.daemon.socket_file();
                    let manager = Arc::clone(daemon_manager);
                    let socket_server = DaemonSocketServer::new(socket_path.clone(), manager)
                        .with_socket_group(config.daemon.socket_group.clone());

                    tracing::info!("Starting daemon socket server at {:?}", socket_path);

                    tokio::spawn(async move {
                        if let Err(e) = socket_server.run().await {
                            tracing::error!("Daemon socket server error: {}", e);
                        }
                    });
                }
            }
        }

        // Spawn ONVIF PullPoint event listeners for monitors that enable them
        // (no-op when the daemon manager is disabled). Done after the daemon
        // manager has started so the listeners can observe its shutdown signal.
        #[cfg(feature = "onvif-events")]
        self.state.spawn_onvif_event_listeners().await;

        // Capture the daemon manager before `self.state` is consumed by the
        // router, so managed daemons can be drained after the server exits.
        let daemon_manager = self.state.daemon_manager.clone();
        if self.state.schema_behind.is_none() {
            tokio::spawn(crate::service::media_jobs::worker(self.state.clone()));
        }
        let rtsp_state = self.state.clone();
        let router = create_router_app(self.state);

        // One shutdown signal, observed by the HTTP drain and by the daemon
        // stop wave — which starts at once rather than after the drain (#116).
        // systemd's TimeoutStopSec bounds the whole stop, so a slow in-flight
        // response used to eat the time zmc/zma needed to be SIGTERM'd.
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        // zm-api's RTSP server, when `streaming.rtsp.enabled`.
        crate::streaming::rtsp::start(rtsp_state, shutdown_rx.clone()).await;
        let daemon_stop = {
            let dm = daemon_manager.clone();
            tokio::spawn(async move {
                shutdown_signal().await;
                let _ = shutdown_tx.send(true);
                if let Some(dm) = dm {
                    dm.mark_process_exiting();
                    tracing::info!("Shutting down managed daemons...");
                    match dm.shutdown_all().await {
                        Ok(resp) => tracing::info!("Daemon shutdown complete: {}", resp.message),
                        Err(e) => tracing::error!("Daemon shutdown error: {}", e),
                    }
                }
            })
        };
        let signalled = || {
            let mut rx = shutdown_rx.clone();
            async move {
                let _ = rx.wait_for(|v| *v).await;
            }
        };

        // Serve until a shutdown signal drains the listener. Every path wires a
        // real graceful drain (`axum_server::Handle` / `with_graceful_shutdown`)
        // so in-flight requests complete before exit.
        let server_result: AppResult<()> = async {
            if let Some(acme) = acme_config.filter(|acme| acme.enabled) {
                if tls_config.as_ref().is_some_and(|tls| tls.enabled) {
                    return Err(crate::error::AppError::InvalidPayloadError(
                        "server.tls.enabled cannot be true when server.acme.enabled is true"
                            .to_string(),
                    ));
                }

                let domains: Vec<String> = acme
                    .domains
                    .iter()
                    .filter_map(|domain| {
                        let trimmed = domain.trim();
                        if trimmed.is_empty() {
                            None
                        } else {
                            Some(trimmed.to_string())
                        }
                    })
                    .collect();
                if domains.is_empty() {
                    return Err(crate::error::AppError::InvalidPayloadError(
                        "server.acme.domains must include at least one domain".to_string(),
                    ));
                }
                if acme.production && acme.cache_dir.is_none() {
                    tracing::warn!(
                        "server.acme.cache_dir is unset; production ACME should persist cache to avoid rate limits"
                    );
                }

                let contacts = acme
                    .contact_emails
                    .iter()
                    .filter_map(|email| {
                        let trimmed = email.trim();
                        if trimmed.is_empty() {
                            None
                        } else if trimmed.starts_with("mailto:") {
                            Some(trimmed.to_string())
                        } else {
                            Some(format!("mailto:{trimmed}"))
                        }
                    })
                    .collect::<Vec<_>>();

                let mut state = AcmeConfig::new(domains)
                    .contact(contacts)
                    .cache_option(
                        acme.cache_dir
                            .as_ref()
                            .map(|dir| DirCache::new(dir.clone())),
                    )
                    .directory_lets_encrypt(acme.production)
                    .challenge_type(match acme.challenge {
                        AcmeChallenge::TlsAlpn01 => UseChallenge::TlsAlpn01,
                        AcmeChallenge::Http01 => UseChallenge::Http01,
                    })
                    .state();

                let acceptor = state.axum_acceptor(acme_server_config(&state));
                let http01_service = match acme.challenge {
                    AcmeChallenge::Http01 => Some(state.http01_challenge_tower_service()),
                    AcmeChallenge::TlsAlpn01 => None,
                };

                tokio::spawn(async move {
                    while let Some(event) = state.next().await {
                        match event {
                            Ok(ok) => tracing::info!("acme event: {:?}", ok),
                            Err(err) => tracing::error!("acme error: {:?}", err),
                        }
                    }
                });

                tracing::info!("Starting HTTPS server (ACME) on: {addr}");
                let handle = axum_server::Handle::new();
                spawn_graceful_shutdown(handle.clone(), shutdown_timeout);
                if let Some(challenge_service) = http01_service {
                    let http_port = acme.http_port.unwrap_or(80);
                    let http_addr = format!("{}:{}", config.server.addr, http_port).parse()?;
                    tracing::info!("Starting HTTP-01 challenge listener on: {http_addr}");
                    let http_router = axum::Router::new().route_service(
                        "/.well-known/acme-challenge/{challenge_token}",
                        challenge_service,
                    );
                    let http_server = axum_server::bind(http_addr)
                        .handle(handle.clone())
                        .serve(http_router.into_make_service());
                    let https_server = axum_server::bind(addr)
                        .handle(handle)
                        .acceptor(acceptor)
                        .serve(router.into_make_service_with_connect_info::<SocketAddr>());
                    tokio::try_join!(https_server, http_server)?;
                } else {
                    axum_server::bind(addr)
                        .handle(handle)
                        .acceptor(acceptor)
                        .serve(router.into_make_service_with_connect_info::<SocketAddr>())
                        .await?;
                }
            } else if let Some(tls) = tls_config.filter(|tls| tls.enabled) {
                let cert_path = tls.cert_path.as_ref().ok_or_else(|| {
                    crate::error::AppError::InvalidPayloadError(
                        "server.tls.cert_path is required".to_string(),
                    )
                })?;
                let key_path = tls.key_path.as_ref().ok_or_else(|| {
                    crate::error::AppError::InvalidPayloadError(
                        "server.tls.key_path is required".to_string(),
                    )
                })?;
                tracing::info!("Starting HTTPS server on: {addr}");
                let tls = tls_from_pem_files(cert_path, key_path).await?;
                let handle = axum_server::Handle::new();
                spawn_graceful_shutdown(handle.clone(), shutdown_timeout);
                axum_server::bind_rustls(addr, tls)
                    .handle(handle)
                    .serve(router.into_make_service_with_connect_info::<SocketAddr>())
                    .await?;
            } else {
                tracing::info!("Starting HTTP server on: {addr}");
                let tcp = tokio::net::TcpListener::bind(addr).await?;
                // Bounded like the TLS paths: an unbounded drain let one slow
                // response hold the exit past TimeoutStopSec (#116).
                let deadline = async {
                    signalled().await;
                    tokio::time::sleep(shutdown_timeout).await;
                };
                tokio::select! {
                    served = axum::serve(
                        tcp,
                        router.into_make_service_with_connect_info::<SocketAddr>(),
                    )
                    .with_graceful_shutdown(signalled()) => served?,
                    _ = deadline => tracing::warn!(
                        "HTTP drain exceeded {:?}; exiting with requests in flight",
                        shutdown_timeout
                    ),
                }
            }
            Ok(())
        }
        .await;

        // Always runs — drain managed daemons cleanly, whatever the outcome.
        // After a signal the stop wave above is already under way and this
        // just waits for it; after a server error it is the only wave.
        if let Some(ref dm) = daemon_manager {
            dm.mark_process_exiting();
            match dm.shutdown_all().await {
                Ok(resp) => tracing::info!("Daemon shutdown complete: {}", resp.message),
                Err(e) => tracing::error!("Daemon shutdown error: {}", e),
            }
        }
        daemon_stop.abort();

        server_result
    }
}

/// The TLS config for ACME mode: rustls-acme's, which advertises no ALPN
/// protocol, with HTTP/2 and HTTP/1.1 added. Without ALPN a browser speaks
/// HTTP/1.1, and its six connections per host stall a grid of live views.
/// The acceptor still answers TLS-ALPN-01 challenges with its own config.
pub fn acme_server_config<EC, EA>(
    state: &rustls_acme::AcmeState<EC, EA>,
) -> std::sync::Arc<rustls::ServerConfig>
where
    EC: std::fmt::Debug + 'static,
    EA: std::fmt::Debug + 'static,
{
    let mut config = (*state.default_rustls_config()).clone();
    config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    std::sync::Arc::new(config)
}

/// The TLS config for certificate-file mode (`server.tls`). axum-server's
/// advertises HTTP/2 and HTTP/1.1.
pub async fn tls_from_pem_files(
    cert_path: impl AsRef<std::path::Path>,
    key_path: impl AsRef<std::path::Path>,
) -> std::io::Result<axum_server::tls_rustls::RustlsConfig> {
    axum_server::tls_rustls::RustlsConfig::from_pem_file(cert_path, key_path).await
}
