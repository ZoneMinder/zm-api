use std::sync::Arc;
use std::time::Duration;

use sea_orm::DatabaseConnection;

use crate::client::{
    database::{DatabaseClient, DatabaseClientExt},
    http::HttpClient,
};
use crate::configure::{self, env::get_env_source, AppConfig};
use crate::constant::ENV_PREFIX;
use crate::daemon::DaemonManager;
use crate::error::{AppError, AppResult};
use crate::ptz::PtzManager;
use crate::service::search::SearchService;
use crate::service::synopsis::SynopsisService;
use crate::streaming::hls::HlsSessionManager;
use crate::streaming::live::LiveStreamCoordinator;
use crate::streaming::snapshot::SnapshotService;
use crate::streaming::source::SourceRouter;

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<AppConfig>,
    pub db: Arc<DatabaseClient>,
    pub http: HttpClient,
    // HLS Streaming (Phase 3)
    pub hls_session_manager: Option<Arc<HlsSessionManager>>,
    // Live Streaming Coordinator
    pub source_router: Option<Arc<SourceRouter>>,
    pub live_coordinator: Option<Arc<LiveStreamCoordinator>>,
    // Daemon Controller
    pub daemon_manager: Option<Arc<DaemonManager>>,
    // Snapshot Service
    pub snapshot_service: Option<Arc<SnapshotService>>,
    // Motion-synopsis renderer/serving
    pub synopsis_service: Option<Arc<SynopsisService>>,
    // Natural-language / semantic event search
    pub search_service: Option<Arc<SearchService>>,
    // PTZ Manager
    pub ptz_manager: Arc<PtzManager>,
    // Per-user token-revocation floors (hot-path mirror of Users.TokenMinExpiry)
    pub revocations: Arc<crate::util::revocation::TokenRevocations>,
    /// Set when ZoneMinder's schema is older than this build needs (#147):
    /// the API answers 503 with this message, and the daemons are left alone.
    pub schema_behind: Option<Arc<str>>,
    /// Wakes the media job worker when a job is queued.
    pub media_job_wake: Arc<tokio::sync::Notify>,
    /// The live event stream's hub (`GET /api/v3/events/stream`).
    pub event_hub: Arc<crate::service::event_stream::EventHub>,
    /// Live WebRTC sessions started by a viewer's offer (WHEP).
    pub whep: Arc<crate::streaming::live::whep::WhepSessions>,
    /// Where media jobs are kept: zm-api's table if the database has it,
    /// else a file in zm-api's state directory. Decided on first use.
    pub media_job_store: crate::service::media_job_store::SharedJobStore,
    /// Live WebRTC session slots, shared by WebSocket signalling and WHEP
    /// (`streaming.webrtc.max_connections`).
    pub webrtc_slots: Arc<crate::streaming::live::slots::WebRtcSlots>,
}

impl AppState {
    pub async fn new(mut config: AppConfig) -> AppResult<Self> {
        crate::configure::server::set_public_prefix(&config.server.public_prefix)
            .map_err(|e| AppError::ConfigError(config::ConfigError::Message(e)))?;
        let db = Arc::new(DatabaseClient::build_from_config(&config).await?);

        // zm-next is on only where it is installed: `enabled = "auto"` (the
        // default) looks for the Monitors.UseZmNext column a zm-next-capable
        // ZoneMinder adds.
        let column = crate::repo::monitors::use_zmnext_column_exists(db.as_ref()).await;
        config.zmnext.resolve(column);
        tracing::info!(
            "zm-next {} (enabled = {:?}, Monitors.UseZmNext {})",
            if config.zmnext.enabled { "on" } else { "off" },
            config.zmnext.setting,
            if column { "present" } else { "absent" }
        );

        // Apply zm-api-owned migrations (additive, IF NOT EXISTS; only our own
        // tables — never ZoneMinder's). Non-fatal: a failure degrades the owned
        // features (event_synopsis, the zm-next pipeline store) rather than
        // blocking boot, since zm-api primarily reads ZoneMinder's own schema.
        //
        // A ZoneMinder schema older than this build mirrors is not upgraded
        // here: ZoneMinder's own binaries may still be the old version. zm-api
        // refuses to serve until the operator upgrades it (#147).
        let schema_behind: Option<Arc<str>> =
            match crate::migration::schema_check::check(db.as_ref()).await {
                Ok(state) => state.explain().map(Arc::from),
                Err(e) => {
                    tracing::warn!("could not check the ZoneMinder schema version: {e}");
                    None
                }
            };
        if let Some(msg) = &schema_behind {
            tracing::error!("{msg}. Until then the API answers 503 and capture is not touched");
        } else if let Err(e) = crate::client::database::migrate_database(db.as_ref()).await {
            tracing::warn!("zm-api owned-table migrations failed (features may degrade): {e}");
        }
        // Stored zm-next graphs saved before secrets were split out still hold
        // them in plain text; move them into the encrypted store now.
        crate::service::zmnext::secrets::migrate_stored_graphs(
            db.as_ref(),
            &config.zmnext.secrets.key_file,
        )
        .await;

        let http = reqwest::Client::builder()
            .no_proxy()
            .build()
            .expect("http client");

        // Natural-language / semantic event search. Resolves its vector backend
        // (probing the DB only when enabled) and creates its schema; off by
        // default → a no-op store. Built early so the zm-next ingest can index
        // event embeddings at ingest time.
        let search_service = Some(Arc::new(
            SearchService::new(http.clone(), db.clone(), config.search.clone()).await,
        ));

        // Initialize HLS session manager (Phase 3)
        let hls_session_manager = if config.streaming.hls.enabled {
            tracing::info!("HLS streaming enabled, initializing session manager");
            let manager = Arc::new(HlsSessionManager::new(
                config.streaming.hls.clone(),
                &crate::configure::server::public_path("/api/v3/live"),
            ));
            // Start the background segment-cleanup loop (previously never
            // started — its handle was dropped on the floor). See §3.1.
            manager.start_cleanup_task();
            Some(manager)
        } else {
            tracing::info!("HLS streaming disabled in configuration");
            None
        };

        // Initialize source router and live coordinator
        let (source_router, live_coordinator) = if config.streaming.enabled {
            tracing::info!("Live streaming enabled, initializing source router and coordinator");
            let mut router =
                SourceRouter::from_zoneminder_config(config.streaming.zoneminder.clone());

            // zm-next event ingest: the router forwards decoded monitor EVENTs
            // to a bounded channel drained by a background ingest task that
            // writes Events/Frames rows. Only wired when zm-next is enabled.
            if config.zmnext.enabled {
                let (event_tx, event_rx) =
                    tokio::sync::mpsc::channel(config.zmnext.ingest.channel_capacity);
                router.set_event_sink(event_tx);
                let ingestor = crate::service::zmnext::EventIngestor::new(
                    db.clone(),
                    config.zmnext.ingest.clone(),
                    config.synopsis.clone(),
                    search_service.clone(),
                );
                tokio::spawn(ingestor.run(event_rx));
                tracing::info!("zm-next event ingest enabled");
            }

            let router = Arc::new(router);

            // Keep configured monitors' readers hot so the first viewer skips
            // cold spin-up (pre-populated keyframe cache → instant codec + a
            // ready keyframe). No-op when the list is empty.
            router.spawn_prewarm_task(
                config.streaming.source.prewarm_monitors.clone(),
                Duration::from_secs(config.streaming.source.prewarm_interval_seconds),
            );

            let coordinator = Arc::new(LiveStreamCoordinator::new(
                Arc::clone(&router),
                hls_session_manager.clone(),
            ));
            // Reap HLS sessions abandoned by viewers who navigated away, so the
            // socket reader + segmenter don't run forever (§3.2). Disabled when
            // idle_timeout_seconds is 0.
            if let Some(hls) = &hls_session_manager {
                coordinator.start_idle_watchdog(hls.idle_timeout(), Duration::from_secs(15));
            }
            (Some(router), Some(coordinator))
        } else {
            tracing::info!("Live streaming disabled in configuration");
            (None, None)
        };

        // Initialize snapshot service (reuses source router)
        let snapshot_service = source_router
            .as_ref()
            .map(|r| Arc::new(SnapshotService::with_defaults(Arc::clone(r))));

        // Initialize the motion-synopsis service. Always constructed (it only
        // needs the db + config) so the endpoints can report a clear "disabled"
        // status rather than 404 when `[synopsis].enabled` is false.
        let synopsis_service = Some(Arc::new(SynopsisService::new(
            db.clone(),
            config.synopsis.clone(),
        )));
        // Hourly retention sweep of expired rendered synopses (only when enabled
        // with a finite retention window).
        if config.synopsis.enabled && config.synopsis.retention_days > 0 {
            if let Some(svc) = &synopsis_service {
                Arc::clone(svc).spawn_retention_task(Duration::from_secs(3600));
                tracing::info!(
                    "synopsis retention enabled ({} day window)",
                    config.synopsis.retention_days
                );
            }
        }

        // Native replacements for the Perl maintenance daemons. Each is
        // independently switchable and all default off, so an existing install
        // keeps running zmstats/zmaudit/zmtelemetry until the operator moves
        // over deliberately — running both at once would have them competing
        // over the same rows.
        // Which Servers row this host is (None = single-server). Used by the
        // stats job and the daemon manager; resolved the way ZoneMinder's
        // own daemons do it, from zm.conf and the Servers table.
        let server_id = crate::daemon::server_id::resolve_server_id(db.as_ref()).await;

        {
            let m = &config.maintenance;
            if m.stats.enabled {
                Arc::new(
                    crate::service::maintenance::stats::StatsService::new(
                        db.clone(),
                        m.stats.clone(),
                    )
                    .with_server_id(server_id),
                )
                .spawn();
                tracing::info!(
                    "stats maintenance enabled (every {}s) — disable zmstats.pl",
                    m.stats.interval_seconds
                );
            }
            if m.audit.enabled {
                Arc::new(crate::service::maintenance::audit::AuditService::new(
                    db.clone(),
                    m.audit.clone(),
                ))
                .spawn();
                tracing::info!(
                    "audit maintenance enabled (every {}s, min_age {}s, dry_run {}) \
                     — disable zmaudit.pl",
                    m.audit.interval_seconds,
                    m.audit.min_age_seconds,
                    m.audit.dry_run,
                );
            }
            if m.telemetry.enabled {
                Arc::new(
                    crate::service::maintenance::telemetry::TelemetryService::new(
                        db.clone(),
                        m.telemetry.clone(),
                        http.clone(),
                    ),
                )
                .spawn();
                tracing::info!(
                    "telemetry enabled (every {}s to {}) — disable zmtelemetry.pl",
                    m.telemetry.interval_seconds,
                    m.telemetry.endpoint,
                );
            }
        }

        // Automatic recording-disk cleanup. Bounds per-storage usage by
        // free-space floor / age / quota so the disk can't silently fill. Off
        // by default; never stored on AppState (no handlers need it).
        if config.retention.enabled {
            let svc = Arc::new(crate::service::retention::RetentionService::new(
                db.clone(),
                config.retention.clone(),
            ));
            Arc::clone(&svc).spawn();
            tracing::info!(
                "event retention enabled (every {}s, min_free {}%, max_age {}d, max_bytes {}, dry_run {})",
                config.retention.interval_seconds,
                config.retention.min_free_pct,
                config.retention.max_age_days,
                config.retention.max_bytes,
                config.retention.dry_run,
            );
        } else {
            tracing::info!("event retention disabled in configuration");
        }

        // Initialize daemon manager if enabled
        let daemon_manager = if config.daemon.enabled && schema_behind.is_some() {
            // The supervisor's monitor queries would fail on this schema and
            // capture would stop; leave the daemons alone instead.
            tracing::error!(
                "not taking over ZoneMinder's daemons: the database is behind this build. \
                 Running capture daemons are left alone; if none are running, \
                 `zm-api-takeover --revert` hands supervision back to ZoneMinder"
            );
            None
        } else if config.daemon.enabled {
            tracing::info!("Daemon controller enabled, initializing manager");
            let mut manager =
                DaemonManager::with_database(config.daemon.clone(), server_id, db.clone());
            // Enable zm-next worker control (no-op unless [zmnext].enabled).
            // Synopsis-opted-in monitors get the extra export stages in their
            // generated pipeline.
            let synopsis_monitors: std::collections::HashSet<u32> = if config.synopsis.enabled {
                config.synopsis.enabled_monitors.iter().copied().collect()
            } else {
                std::collections::HashSet::new()
            };
            manager.set_zmnext(
                config.zmnext.clone(),
                config.streaming.zoneminder.socks_path.clone(),
                synopsis_monitors,
            );
            // Native [maintenance.*] jobs replace their Perl daemons; the
            // supervisor must not start both.
            manager.set_native_maintenance(crate::daemon::manager::NativeJobs::from(
                &config.maintenance,
            ));
            let manager = Arc::new(manager);
            manager.spawn_unsafe_value_report();
            Some(manager)
        } else {
            tracing::info!("Daemon controller disabled in configuration");
            None
        };

        // Initialize PTZ manager
        let ptz_manager = Arc::new(PtzManager::with_defaults());
        tracing::info!("PTZ manager initialized");

        // Hydrate the in-memory token-revocation floors from
        // Users.TokenMinExpiry so logout/password-change revocations survive
        // restarts. Non-fatal: on failure the floors rebuild as revocations
        // happen, and refresh tokens are still DB-checked.
        let revocations = Arc::new(crate::util::revocation::TokenRevocations::default());
        match crate::repo::users::find_all(db.as_ref()).await {
            Ok(users) => {
                for u in &users {
                    if u.token_min_expiry > 0 {
                        revocations.revoke(u.id, u.token_min_expiry as i64);
                    }
                }
            }
            Err(e) => tracing::warn!("failed to hydrate token revocations from Users: {e}"),
        }

        Ok(Self {
            config: Arc::new(config),
            db,
            http,
            hls_session_manager,
            source_router,
            live_coordinator,
            snapshot_service,
            synopsis_service,
            search_service,
            daemon_manager,
            ptz_manager,
            revocations,
            schema_behind,
            media_job_wake: Arc::new(tokio::sync::Notify::new()),
            event_hub: Default::default(),
            whep: Default::default(),
            media_job_store: Default::default(),
            webrtc_slots: Default::default(),
        })
    }

    /// Returns a reference to the DatabaseConnection for use with Sea-ORM.
    /// This handles dereferencing the Arc<DatabaseClient> automatically.
    pub fn db(&self) -> &DatabaseConnection {
        &self.db
    }

    pub fn for_test_with_db(db: DatabaseConnection) -> Self {
        let config =
            configure::AppConfig::read(get_env_source(ENV_PREFIX)).expect("read config for test");
        let http = crate::client::http::HttpClient::builder()
            .no_proxy()
            .build()
            .expect("http client");
        let db = std::sync::Arc::new(db);
        let synopsis_service = Some(std::sync::Arc::new(SynopsisService::new(
            db.clone(),
            config.synopsis.clone(),
        )));
        let search_service = Some(std::sync::Arc::new(SearchService::disabled(
            config.search.clone(),
        )));
        Self {
            config: std::sync::Arc::new(config),
            db,
            http,
            hls_session_manager: None,
            source_router: None,
            live_coordinator: None,
            snapshot_service: None,
            synopsis_service,
            search_service,
            daemon_manager: None,
            ptz_manager: std::sync::Arc::new(PtzManager::with_defaults()),
            revocations: std::sync::Arc::new(crate::util::revocation::TokenRevocations::default()),
            schema_behind: None,
            media_job_wake: std::sync::Arc::new(tokio::sync::Notify::new()),
            event_hub: Default::default(),
            whep: Default::default(),
            media_job_store: Default::default(),
            webrtc_slots: Default::default(),
        }
    }

    /// Returns a reference to the PTZ manager
    pub fn ptz_manager(&self) -> &PtzManager {
        &self.ptz_manager
    }

    /// Spawn one ONVIF PullPoint event listener per monitor that has the ONVIF
    /// event listener enabled (`onvif_event_listener != 0`) and a usable ONVIF
    /// URL. Each listener subscribes, long-polls `PullMessages`, and translates
    /// alarm on/off notifications into ZoneMinder events, quiescing with the
    /// daemon manager on shutdown. No-op when the daemon manager is disabled.
    #[cfg(feature = "onvif-events")]
    pub async fn spawn_onvif_event_listeners(&self) {
        let Some(manager) = self.daemon_manager.clone() else {
            return;
        };
        let monitors = match crate::repo::monitors::find_all(self.db(), None).await {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!("ONVIF event listeners: failed to load monitors: {e}");
                return;
            }
        };

        let mut spawned = 0usize;
        for m in monitors {
            // A deleted monitor must not get a listener inserting events (#120).
            if m.deleted != 0 || m.onvif_event_listener == 0 || m.onvif_url.trim().is_empty() {
                continue;
            }
            // Events service endpoint = onvif_url joined with onvif_events_path.
            let events_xaddr = join_onvif_url(&m.onvif_url, &m.onvif_events_path);
            let creds = (!m.onvif_username.is_empty()).then(|| {
                crate::onvif::types::Credentials::new(
                    m.onvif_username.clone(),
                    m.onvif_password.clone(),
                )
            });
            let transport = crate::onvif::transport::OnvifTransport::new(self.http.clone());
            let client = crate::onvif::events::EventsClient::new(transport, events_xaddr, creds);
            let alarm_cause = m
                .onvif_alarm_text
                .clone()
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "ONVIF Alarm".to_string());

            crate::daemon::onvif_event_listener::spawn_monitor_event_listener(
                self.clone(),
                m.id,
                m.name.clone(),
                alarm_cause,
                client,
                manager.clone(),
                crate::daemon::onvif_event_listener::OnvifEventListenerConfig::default(),
            );
            spawned += 1;
        }

        if spawned > 0 {
            tracing::info!("Spawned {spawned} ONVIF event listener(s)");
        }
    }
}

/// Join an ONVIF base URL with an events service path, tolerating an empty path
/// (returns the base) and avoiding duplicate slashes at the join.
#[cfg(feature = "onvif-events")]
fn join_onvif_url(base: &str, path: &str) -> String {
    let path = path.trim();
    if path.is_empty() {
        return base.to_string();
    }
    if path.starts_with("http://") || path.starts_with("https://") {
        return path.to_string();
    }
    format!(
        "{}/{}",
        base.trim_end_matches('/'),
        path.trim_start_matches('/')
    )
}
