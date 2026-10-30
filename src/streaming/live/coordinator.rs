//! Live streaming coordinator
//!
//! Manages the lifecycle of live streaming sessions, coordinating between
//! the source router and output protocols (HLS, WebRTC).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::RwLock;
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

use crate::streaming::hls::HlsSessionManager;
use crate::streaming::source::router::ReaderHealth;
use crate::streaming::source::SourceRouter;

/// Status of a live streaming session
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SessionStatus {
    Starting,
    Active,
    Stopping,
    Stopped,
    Error,
}

/// Statistics for a live streaming session
#[derive(Debug, Clone, serde::Serialize)]
pub struct LiveSessionStats {
    pub monitor_id: u32,
    pub status: SessionStatus,
    pub packets_processed: u64,
    pub errors: u64,
    pub uptime_seconds: f64,
    pub hls_enabled: bool,
    pub webrtc_enabled: bool,
}

/// Configuration for which protocols to enable
#[derive(Debug, Clone)]
pub struct LiveStreamConfig {
    pub enable_hls: bool,
    pub enable_webrtc: bool,
}

impl Default for LiveStreamConfig {
    fn default() -> Self {
        Self {
            enable_hls: true,
            enable_webrtc: false, // Not implemented yet
        }
    }
}

/// Internal state for a live session
struct LiveSession {
    monitor_id: u32,
    status: SessionStatus,
    config: LiveStreamConfig,
    task_handle: Option<JoinHandle<()>>,
    started_at: std::time::Instant,
    packets_processed: u64,
    errors: u64,
}

impl LiveSession {
    fn new(monitor_id: u32, config: LiveStreamConfig) -> Self {
        Self {
            monitor_id,
            status: SessionStatus::Starting,
            config,
            task_handle: None,
            started_at: std::time::Instant::now(),
            packets_processed: 0,
            errors: 0,
        }
    }

    fn stats(&self) -> LiveSessionStats {
        LiveSessionStats {
            monitor_id: self.monitor_id,
            status: self.status,
            packets_processed: self.packets_processed,
            errors: self.errors,
            uptime_seconds: self.started_at.elapsed().as_secs_f64(),
            hls_enabled: self.config.enable_hls,
            webrtc_enabled: self.config.enable_webrtc,
        }
    }
}

/// Coordinator errors
#[derive(Debug, thiserror::Error)]
pub enum CoordinatorError {
    #[error("Session already exists for monitor {0}")]
    SessionExists(u32),

    #[error("Session not found for monitor {0}")]
    SessionNotFound(u32),

    #[error("Source not available for monitor {0}")]
    SourceNotAvailable(u32),

    #[error("HLS not configured")]
    HlsNotConfigured,

    #[error("Source error: {0}")]
    SourceError(String),

    #[error("HLS error: {0}")]
    HlsError(String),
}

/// Maximum time to wait for the stream-socket handshake to reveal the
/// monitor's stream topology (video codec, audio presence) when starting a
/// session. The HELLOs arrive immediately after connect when zmc is up.
const STREAM_INFO_TIMEOUT: Duration = Duration::from_secs(5);

/// Coordinates live streaming from per-monitor stream sockets to output
/// protocols
pub struct LiveStreamCoordinator {
    /// Source router for stream-socket access
    source_router: Arc<SourceRouter>,
    /// HLS session manager
    hls_manager: Option<Arc<HlsSessionManager>>,
    /// Active sessions
    sessions: Arc<RwLock<HashMap<u32, LiveSession>>>,
    /// Handle to the idle-session reaper task, retained so it is aborted when
    /// the coordinator is dropped (REVIEW_FIXES_PLAN §3.2).
    idle_watchdog: std::sync::Mutex<Option<JoinHandle<()>>>,
    /// Readers `stop_session` left running because something else still read
    /// them. The watchdog stops each once nothing does and no session has
    /// taken it back (#256).
    kept_readers: std::sync::Mutex<std::collections::HashSet<u32>>,
}

impl LiveStreamCoordinator {
    /// Create a new coordinator
    pub fn new(
        source_router: Arc<SourceRouter>,
        hls_manager: Option<Arc<HlsSessionManager>>,
    ) -> Self {
        Self {
            source_router,
            hls_manager,
            sessions: Arc::new(RwLock::new(HashMap::new())),
            idle_watchdog: std::sync::Mutex::new(None),
            kept_readers: Default::default(),
        }
    }

    /// Spawn the idle-session reaper. Every `check_interval` it asks the HLS
    /// manager which sessions have gone untouched for at least `idle_timeout`
    /// and fully tears each one down (`stop_session`: reader + segmenter +
    /// storage). Without this, a viewer who closes their tab leaves the stream-socket
    /// reader and segmenter running forever (sessions otherwise end only via an
    /// explicit `DELETE /stop`).
    ///
    /// `idle_timeout == 0` disables reaping. The task holds only a `Weak` ref to
    /// the coordinator, so it does not keep it alive and exits once the
    /// coordinator is dropped — the retained handle aborts it promptly on drop.
    pub fn start_idle_watchdog(self: &Arc<Self>, idle_timeout: Duration, check_interval: Duration) {
        if idle_timeout.is_zero() {
            info!("HLS idle-session reaping disabled (idle_timeout = 0)");
            return;
        }
        if self.hls_manager.is_none() {
            return;
        }

        let weak = Arc::downgrade(self);
        let handle = tokio::spawn(async move {
            let mut ticker = tokio::time::interval(check_interval);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                ticker.tick().await;
                let Some(coord) = weak.upgrade() else { break };
                let Some(hls) = coord.hls_manager.as_ref() else {
                    break;
                };
                let idle = hls.idle_sessions(idle_timeout).await;
                for monitor_id in idle {
                    info!(
                        "Reaping idle HLS session for monitor {} (idle ≥ {:?})",
                        monitor_id, idle_timeout
                    );
                    if let Err(e) = coord.stop_session(monitor_id).await {
                        warn!("Failed to reap idle session {}: {}", monitor_id, e);
                    }
                }
                coord.stop_unread_kept_readers().await;
                // Drop the strong ref before sleeping so it never pins the
                // coordinator across the idle interval.
                drop(coord);
            }
        });

        if let Some(old) = self.idle_watchdog.lock().unwrap().replace(handle) {
            old.abort();
        }
    }

    /// Start a live streaming session for a monitor
    pub async fn start_session(
        &self,
        monitor_id: u32,
        config: LiveStreamConfig,
    ) -> Result<(), CoordinatorError> {
        // Check if session already exists
        {
            let sessions = self.sessions.read().await;
            if sessions.contains_key(&monitor_id) {
                return Err(CoordinatorError::SessionExists(monitor_id));
            }
        }

        // Check if HLS is enabled and required
        if config.enable_hls && self.hls_manager.is_none() {
            return Err(CoordinatorError::HlsNotConfigured);
        }

        // Check if source is available
        if !self.source_router.is_available(monitor_id) {
            return Err(CoordinatorError::SourceNotAvailable(monitor_id));
        }

        // Create the source WITHOUT starting the reader — this gives us a chance
        // to subscribe before any packets flow, so we never miss SPS/PPS.
        let source = self
            .source_router
            .create_source(monitor_id)
            .await
            .map_err(|e| CoordinatorError::SourceError(e.to_string()))?;

        // Subscribe BEFORE starting the reader — guarantees we see all packets
        let video_rx = source.subscribe_video();
        let audio_rx = Some(source.subscribe_audio());
        let reader_health_rx = source.subscribe_reader_health();

        // NOW start the stream-socket reader
        self.source_router
            .start_reader(monitor_id)
            .await
            .map_err(|e| CoordinatorError::SourceError(e.to_string()))?;

        // Start HLS session if enabled
        if config.enable_hls {
            if let Some(hls_manager) = &self.hls_manager {
                // Whether the monitor has audio is only known once the
                // socket's connect handshake completes (HELLOs precede all
                // media). Normally that is near-instant; on timeout (zmc not
                // delivering yet) start video-only — the segmenter tolerates
                // a late audio track by warning and disabling it.
                let has_audio = source
                    .wait_for_stream_info(STREAM_INFO_TIMEOUT)
                    .await
                    .is_some_and(|info| info.audio_codec.is_some());
                hls_manager
                    .start_session(monitor_id, has_audio)
                    .await
                    .map_err(|e| CoordinatorError::HlsError(e.to_string()))?;
            }
        }

        // Store session BEFORE spawning the processing task so the task
        // can find and update it (status, packet counts). Status starts as
        // Starting and transitions to Active when the first packet arrives.
        {
            let session = LiveSession::new(monitor_id, config.clone());
            let mut sessions = self.sessions.write().await;
            sessions.insert(monitor_id, session);
        }

        // Start processing task with pre-subscribed receivers
        let task_handle =
            self.start_processing_task(monitor_id, config, video_rx, audio_rx, reader_health_rx);

        // Store the task handle
        {
            let mut sessions = self.sessions.write().await;
            if let Some(session) = sessions.get_mut(&monitor_id) {
                session.task_handle = Some(task_handle);
            }
        }

        info!("Started live streaming session for monitor {}", monitor_id);
        Ok(())
    }

    /// Start the packet processing task with a pre-subscribed receiver.
    ///
    /// The receiver must be created BEFORE the stream-socket reader starts to guarantee
    /// no packets (especially initial SPS/PPS) are lost.
    ///
    /// The task transitions the session from Starting → Active on first packet,
    /// or Starting → Error if no packets arrive within the startup timeout.
    fn start_processing_task(
        &self,
        monitor_id: u32,
        config: LiveStreamConfig,
        video_rx: tokio::sync::broadcast::Receiver<crate::streaming::source::VideoPacket>,
        audio_rx: Option<tokio::sync::broadcast::Receiver<crate::streaming::source::AudioPacket>>,
        reader_health_rx: tokio::sync::watch::Receiver<ReaderHealth>,
    ) -> JoinHandle<()> {
        /// Maximum time to wait for the first video packet before marking the
        /// session as errored. Covers socket connect retries + ZoneMinder startup.
        const STARTUP_TIMEOUT: Duration = Duration::from_secs(15);

        let hls_manager = self.hls_manager.clone();
        let sessions = Arc::clone(&self.sessions);

        tokio::spawn(async move {
            info!("Starting packet processing task for monitor {}", monitor_id);
            let mut video_rx = video_rx;
            let mut audio_rx = audio_rx;
            let mut reader_health_rx = reader_health_rx;
            let mut received_first_packet = false;
            let startup_deadline = tokio::time::Instant::now() + STARTUP_TIMEOUT;

            loop {
                // Until the first packet arrives, race recv against a startup
                // timeout so we detect dead readers (zmc not running).
                // Also monitor reader health — if the watch sender is dropped
                // (reader task exited), we stop instead of hanging forever on the
                // broadcast channel (which never closes while MonitorSource lives).
                let recv_result = if !received_first_packet {
                    tokio::select! {
                        result = video_rx.recv() => result,
                        () = tokio::time::sleep_until(startup_deadline) => {
                            warn!(
                                "No video packets received within {}s for monitor {}, marking session as error",
                                STARTUP_TIMEOUT.as_secs(),
                                monitor_id,
                            );
                            let mut sessions_guard = sessions.write().await;
                            if let Some(session) = sessions_guard.get_mut(&monitor_id) {
                                session.status = SessionStatus::Error;
                            }
                            return;
                        }
                        health = reader_health_rx.changed() => {
                            match health {
                                Ok(()) => {
                                    // Reader health changed — log it but keep waiting for packets
                                    let state = *reader_health_rx.borrow();
                                    debug!(
                                        "Reader health for monitor {}: {:?}",
                                        monitor_id, state
                                    );
                                    continue;
                                }
                                Err(_) => {
                                    // Watch sender dropped — reader task exited
                                    warn!(
                                        "Reader task exited for monitor {}, marking session as error",
                                        monitor_id,
                                    );
                                    let mut sessions_guard = sessions.write().await;
                                    if let Some(session) = sessions_guard.get_mut(&monitor_id) {
                                        session.status = SessionStatus::Error;
                                    }
                                    return;
                                }
                            }
                        }
                    }
                } else {
                    tokio::select! {
                        result = video_rx.recv() => result,
                        // Audio rides alongside video once the stream is up.
                        // The branch pends forever when the monitor has no
                        // audio (or its channel closed).
                        audio = async {
                            match audio_rx.as_mut() {
                                Some(rx) => rx.recv().await,
                                None => std::future::pending().await,
                            }
                        } => {
                            match audio {
                                Ok(packet) => {
                                    if config.enable_hls {
                                        if let Some(hls) = &hls_manager {
                                            if let Err(e) =
                                                hls.process_audio_packet(&packet).await
                                            {
                                                debug!(
                                                    "HLS audio process error for monitor {}: {}",
                                                    monitor_id, e
                                                );
                                            }
                                        }
                                    }
                                }
                                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                                    debug!(
                                        "Audio lagged {} packets for monitor {}",
                                        n, monitor_id
                                    );
                                }
                                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                                    // Audio producer gone; stop polling it.
                                    audio_rx = None;
                                }
                            }
                            continue;
                        }
                        health = reader_health_rx.changed() => {
                            match health {
                                Ok(()) => {
                                    let state = *reader_health_rx.borrow();
                                    debug!(
                                        "Reader health for monitor {}: {:?}",
                                        monitor_id, state
                                    );
                                    continue;
                                }
                                Err(_) => {
                                    warn!(
                                        "Reader task exited for monitor {}, stopping processing",
                                        monitor_id,
                                    );
                                    break;
                                }
                            }
                        }
                    }
                };

                match recv_result {
                    Ok(packet) => {
                        // Transition Starting → Active on first packet
                        if !received_first_packet {
                            received_first_packet = true;
                            let mut sessions_guard = sessions.write().await;
                            if let Some(session) = sessions_guard.get_mut(&monitor_id) {
                                session.status = SessionStatus::Active;
                            }
                            info!(
                                "First video packet received for monitor {}, session active",
                                monitor_id
                            );
                        }

                        // Process through HLS if enabled
                        if config.enable_hls {
                            if let Some(hls) = &hls_manager {
                                if let Err(e) = hls.process_packet(&packet).await {
                                    debug!("HLS process error for monitor {}: {}", monitor_id, e);
                                    let mut sessions_guard = sessions.write().await;
                                    if let Some(session) = sessions_guard.get_mut(&monitor_id) {
                                        session.errors += 1;
                                    }
                                }
                            }
                        }

                        // Update packet count
                        {
                            let mut sessions_guard = sessions.write().await;
                            if let Some(session) = sessions_guard.get_mut(&monitor_id) {
                                session.packets_processed += 1;
                            }
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        // Lagged means packets ARE flowing — we just missed some
                        if !received_first_packet {
                            received_first_packet = true;
                            let mut sessions_guard = sessions.write().await;
                            if let Some(session) = sessions_guard.get_mut(&monitor_id) {
                                session.status = SessionStatus::Active;
                            }
                            info!(
                                "Video packets flowing for monitor {} (lagged {}), session active",
                                monitor_id, n
                            );
                        } else {
                            warn!(
                                "Processing task lagged {} packets for monitor {}",
                                n, monitor_id
                            );
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        info!(
                            "Video channel closed for monitor {}, stopping task",
                            monitor_id
                        );
                        break;
                    }
                }
            }

            // Mark session as stopped
            {
                let mut sessions_guard = sessions.write().await;
                if let Some(session) = sessions_guard.get_mut(&monitor_id) {
                    session.status = SessionStatus::Stopped;
                }
            }

            info!("Packet processing task stopped for monitor {}", monitor_id);
        })
    }

    /// Stop a live streaming session
    pub async fn stop_session(&self, monitor_id: u32) -> Result<(), CoordinatorError> {
        let mut sessions = self.sessions.write().await;

        let session = sessions
            .get_mut(&monitor_id)
            .ok_or(CoordinatorError::SessionNotFound(monitor_id))?;

        session.status = SessionStatus::Stopping;

        // Abort the processing task, and wait for it to go so its
        // subscriptions to the source are dropped before they're counted.
        if let Some(handle) = session.task_handle.take() {
            handle.abort();
            let _ = handle.await;
        }

        // Stop HLS session if enabled
        if session.config.enable_hls {
            if let Some(hls_manager) = &self.hls_manager {
                let _ = hls_manager.stop_session(monitor_id).await;
            }
        }

        // Stop the source reader, unless something else still reads it: a
        // WebRTC or WHEP viewer, or an RTSP client, shares this reader, and
        // the idle reaper ends a session nobody fetches HLS from (#256).
        let others = self
            .source_router
            .get_existing_source(monitor_id)
            .map_or(0, |s| {
                s.video_subscriber_count() + s.audio_subscriber_count()
            });
        if others == 0 {
            let _ = self.source_router.stop_reader(monitor_id).await;
        } else {
            info!(
                "Leaving monitor {}'s reader running for {} other subscriber(s)",
                monitor_id, others
            );
            self.kept_readers.lock().unwrap().insert(monitor_id);
        }

        sessions.remove(&monitor_id);

        info!("Stopped live streaming session for monitor {}", monitor_id);
        Ok(())
    }

    /// Stop the readers [`stop_session`](Self::stop_session) left running
    /// for other subscribers, once none are left. A reader a new session has
    /// taken back is that session's again. Called by the idle watchdog each
    /// tick.
    pub async fn stop_unread_kept_readers(&self) {
        let kept: Vec<u32> = self.kept_readers.lock().unwrap().iter().copied().collect();
        if kept.is_empty() {
            return;
        }
        let sessions = self.sessions.read().await;
        for monitor_id in kept {
            let unread = self
                .source_router
                .get_existing_source(monitor_id)
                .is_none_or(|s| s.video_subscriber_count() + s.audio_subscriber_count() == 0);
            if sessions.contains_key(&monitor_id) {
                self.kept_readers.lock().unwrap().remove(&monitor_id);
            } else if unread {
                let _ = self.source_router.stop_reader(monitor_id).await;
                self.kept_readers.lock().unwrap().remove(&monitor_id);
                info!("Stopped monitor {}'s reader: nothing reads it", monitor_id);
            }
        }
    }

    /// Check if a session exists
    pub async fn has_session(&self, monitor_id: u32) -> bool {
        let sessions = self.sessions.read().await;
        sessions.contains_key(&monitor_id)
    }

    /// Get session statistics
    pub async fn get_stats(&self, monitor_id: u32) -> Option<LiveSessionStats> {
        let sessions = self.sessions.read().await;
        sessions.get(&monitor_id).map(|s| s.stats())
    }

    /// List all active sessions
    pub async fn list_sessions(&self) -> Vec<u32> {
        let sessions = self.sessions.read().await;
        sessions.keys().copied().collect()
    }

    /// Get statistics for all sessions
    pub async fn all_stats(&self) -> Vec<LiveSessionStats> {
        let sessions = self.sessions.read().await;
        sessions.values().map(|s| s.stats()).collect()
    }

    /// Get the source router reference
    pub fn source_router(&self) -> &Arc<SourceRouter> {
        &self.source_router
    }

    /// Get the HLS manager reference
    pub fn hls_manager(&self) -> Option<&Arc<HlsSessionManager>> {
        self.hls_manager.as_ref()
    }
}

impl Drop for LiveStreamCoordinator {
    fn drop(&mut self) {
        if let Some(handle) = self.idle_watchdog.lock().unwrap().take() {
            handle.abort();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_live_stream_config_default() {
        let config = LiveStreamConfig::default();
        assert!(config.enable_hls);
        assert!(!config.enable_webrtc);
    }

    #[test]
    fn test_session_status_serialize() {
        let status = SessionStatus::Active;
        let json = serde_json::to_string(&status).unwrap();
        assert_eq!(json, "\"active\"");
    }

    #[tokio::test]
    async fn test_coordinator_creation() {
        let source_router = Arc::new(SourceRouter::new());
        let coordinator = LiveStreamCoordinator::new(source_router, None);

        assert!(coordinator.list_sessions().await.is_empty());
    }

    #[tokio::test]
    async fn test_session_not_found() {
        let source_router = Arc::new(SourceRouter::new());
        let coordinator = LiveStreamCoordinator::new(source_router, None);

        let result = coordinator.stop_session(99999).await;
        assert!(matches!(
            result,
            Err(CoordinatorError::SessionNotFound(99999))
        ));
    }

    #[tokio::test]
    async fn test_hls_not_configured() {
        let source_router = Arc::new(SourceRouter::new());
        let coordinator = LiveStreamCoordinator::new(source_router, None);

        let config = LiveStreamConfig {
            enable_hls: true,
            enable_webrtc: false,
        };

        let result = coordinator.start_session(1, config).await;
        assert!(matches!(result, Err(CoordinatorError::HlsNotConfigured)));
    }
}
