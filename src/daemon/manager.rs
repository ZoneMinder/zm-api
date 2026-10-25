//! Daemon manager - core process control and lifecycle management.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use std::path::{Path, PathBuf};

use rust_decimal::prelude::FromPrimitive;
use rust_decimal::Decimal;
use sea_orm::{
    ActiveModelTrait, ActiveValue::Set, ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter,
    QueryOrder,
};
use tokio::process::Command;
use tokio::sync::{mpsc, Notify, RwLock};
use tracing::{debug, error, info, warn};

use crate::configure::zmnext::ZmNextConfig;
use crate::daemon::config::DaemonConfig;
use crate::daemon::daemons::DaemonDefinition;
use crate::daemon::ipc::{
    canonical_daemon_id, DaemonResponse, ProcessStatus, SystemStats, SystemStatus,
};
use crate::daemon::process::{ManagedProcess, ProcessState};
use crate::daemon::stats;
use crate::entity::sea_orm_active_enums::{Capturing, Function, MonitorType, Status};
use crate::entity::{filters, monitors, servers, storage, zones};
use crate::error::AppResult;
use crate::service::zmnext::pipeline;
use crate::zm_shm::{MonitorShm, DEFAULT_SHM_PATH, DEFAULT_SHM_PREFIX};

/// Runtime context the manager needs to drive zm-next workers: the validated
/// config plus the stream-socket directory (from the streaming config) used to
/// build each worker's `--socket` path.
#[derive(Debug, Clone)]
struct ZmNextRuntime {
    config: ZmNextConfig,
    socks_path: String,
    /// Monitor ids whose pipeline emits motion-synopsis ingredients.
    synopsis_monitors: std::collections::HashSet<u32>,
}

/// Internal command for the daemon manager.
#[derive(Debug)]
pub enum ManagerCommand {
    /// Start a daemon
    Start { id: String, respond: ResponseSender },
    /// Stop a daemon
    Stop { id: String, respond: ResponseSender },
    /// Restart a daemon
    Restart { id: String, respond: ResponseSender },
    /// Reload a daemon (SIGHUP)
    Reload { id: String, respond: ResponseSender },
    /// Get status of all daemons
    Status { respond: ResponseSender },
    /// Shutdown all daemons
    Shutdown { respond: ResponseSender },
}

type ResponseSender = mpsc::Sender<DaemonResponse>;

/// Daemon manager for controlling ZoneMinder daemons.
#[derive(Clone)]
pub struct DaemonManager {
    /// Map of daemon ID to process info
    processes: Arc<RwLock<HashMap<String, ManagedProcess>>>,
    /// Map of PID to daemon ID for signal handling
    pid_map: Arc<RwLock<HashMap<u32, String>>>,
    /// Configuration
    config: Arc<DaemonConfig>,
    /// Shutdown signal
    shutdown: Arc<Notify>,
    /// Latched shutdown flag checked by start/reconcile paths (the `Notify`
    /// signal alone is lost on a task that is mid-tick).
    shutting_down: Arc<AtomicBool>,
    /// Bumped by every `startup()`. Each background loop captures the value
    /// it was spawned under and exits when it changes, so a loop that missed
    /// the shutdown wake-up mid-tick cannot outlive a restart and run beside
    /// its replacement (#80).
    loop_generation: Arc<AtomicU64>,
    /// The whole process is exiting — distinct from `shutting_down`, which
    /// also fires for a daemon *restart* (apply_state, system restart). Long-
    /// lived tasks that must survive a restart, like the ONVIF event
    /// listeners, watch this one (#79).
    process_exiting: Arc<AtomicBool>,
    /// Server ID for ZoneMinder distributed mode
    server_id: Option<u32>,
    /// Set once the orphan sweep has run for this manager. The sweep is for
    /// processes left by a *previous* zm-api, so it must not run again on the
    /// socket `startup`/`pkg_start` or REST startup paths of a live supervisor,
    /// where it SIGKILLed every healthy child (#74).
    orphans_swept: Arc<AtomicBool>,
    /// Which Perl maintenance daemons zm-api's own jobs replace. Set from
    /// `[maintenance.*].enabled`; the automatic singleton start skips the Perl
    /// counterpart so the two never compete over the same rows (#87).
    native_jobs: NativeJobs,
    /// Whether the manager is running
    running: Arc<RwLock<bool>>,
    /// Database connection for querying monitors
    db: Option<Arc<DatabaseConnection>>,
    /// zm-next worker runtime; `None` (the default) means every monitor stays
    /// on legacy zmc/zma regardless of any per-monitor flag.
    zmnext: Option<Arc<ZmNextRuntime>>,
    /// Where zmc keeps its per-monitor shared memory (`ZM_PATH_MAP`), read by
    /// the watchdog for the capture heartbeat (#123). Loaded from `Config` at
    /// `startup`; `/dev/shm` until then.
    shm_dir: Arc<std::sync::RwLock<PathBuf>>,
}

impl DaemonManager {
    /// Create a new daemon manager.
    pub fn new(config: DaemonConfig, server_id: Option<u32>) -> Self {
        Self {
            processes: Arc::new(RwLock::new(HashMap::new())),
            pid_map: Arc::new(RwLock::new(HashMap::new())),
            config: Arc::new(config),
            shutdown: Arc::new(Notify::new()),
            shutting_down: Arc::new(AtomicBool::new(false)),
            loop_generation: Arc::new(AtomicU64::new(0)),
            process_exiting: Arc::new(AtomicBool::new(false)),
            server_id,
            orphans_swept: Arc::new(AtomicBool::new(false)),
            native_jobs: NativeJobs::default(),
            running: Arc::new(RwLock::new(false)),
            db: None,
            zmnext: None,
            shm_dir: Arc::new(std::sync::RwLock::new(PathBuf::from(DEFAULT_SHM_PATH))),
        }
    }

    /// Point the watchdog at a different shared-memory directory.
    pub fn set_shm_dir(&self, dir: impl Into<PathBuf>) {
        *self.shm_dir.write().unwrap_or_else(|e| e.into_inner()) = dir.into();
    }

    fn shm_dir(&self) -> PathBuf {
        self.shm_dir
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// True exactly once per manager: the first caller owns the orphan sweep.
    pub fn orphan_sweep_due(&self) -> bool {
        !self.orphans_swept.swap(true, Ordering::SeqCst)
    }

    /// Set the database connection for querying monitors.
    pub fn set_database(&mut self, db: Arc<DatabaseConnection>) {
        self.db = Some(db);
    }

    /// Declare which Perl maintenance daemons are replaced by zm-api's native
    /// jobs, so `start_all_daemons` leaves them unstarted. Call before
    /// `startup()`.
    pub fn set_native_maintenance(&mut self, jobs: NativeJobs) {
        self.native_jobs = jobs;
    }

    /// Enable zm-next worker control. Call before `startup()` when
    /// `[zmnext].enabled` is set; `socks_path` is the streaming stream-socket
    /// directory used to build each worker's `--socket` path. A disabled config
    /// is ignored, keeping every monitor on legacy zmc/zma.
    pub fn set_zmnext(
        &mut self,
        config: ZmNextConfig,
        socks_path: String,
        synopsis_monitors: std::collections::HashSet<u32>,
    ) {
        if config.enabled {
            self.zmnext = Some(Arc::new(ZmNextRuntime {
                config,
                socks_path,
                synopsis_monitors,
            }));
        }
    }

    /// Create a new daemon manager with database connection.
    pub fn with_database(
        config: DaemonConfig,
        server_id: Option<u32>,
        db: Arc<DatabaseConnection>,
    ) -> Self {
        Self {
            processes: Arc::new(RwLock::new(HashMap::new())),
            pid_map: Arc::new(RwLock::new(HashMap::new())),
            config: Arc::new(config),
            shutdown: Arc::new(Notify::new()),
            shutting_down: Arc::new(AtomicBool::new(false)),
            loop_generation: Arc::new(AtomicU64::new(0)),
            process_exiting: Arc::new(AtomicBool::new(false)),
            server_id,
            orphans_swept: Arc::new(AtomicBool::new(false)),
            native_jobs: NativeJobs::default(),
            running: Arc::new(RwLock::new(false)),
            db: Some(db),
            zmnext: None,
            shm_dir: Arc::new(std::sync::RwLock::new(PathBuf::from(DEFAULT_SHM_PATH))),
        }
    }

    /// Get the server ID.
    pub fn server_id(&self) -> Option<u32> {
        self.server_id
    }

    /// Check if the manager is running.
    pub async fn is_running(&self) -> bool {
        *self.running.read().await
    }

    /// Get the configuration.
    pub fn config(&self) -> &DaemonConfig {
        &self.config
    }

    /// Signal shutdown to all background tasks.
    pub fn signal_shutdown(&self) {
        // Latch before waking waiters: a loop mid-tick isn't awaiting
        // `notified()`, so it must observe the flag to avoid re-spawning.
        self.shutting_down.store(true, Ordering::SeqCst);
        self.shutdown.notify_waiters();
    }

    /// Whether shutdown has been signaled.
    pub fn is_shutting_down(&self) -> bool {
        self.shutting_down.load(Ordering::SeqCst)
    }

    /// The process is going away, not just restarting its daemons.
    pub fn mark_process_exiting(&self) {
        self.process_exiting.store(true, Ordering::SeqCst);
    }

    /// Whether the process is exiting (never reset, unlike `is_shutting_down`).
    pub fn is_process_exiting(&self) -> bool {
        self.process_exiting.load(Ordering::SeqCst)
    }

    /// How many times the background loops have been (re)started.
    pub fn loop_generation(&self) -> u64 {
        self.loop_generation.load(Ordering::SeqCst)
    }

    /// A loop spawned under `generation` must stop when a newer startup has
    /// replaced it or shutdown was signalled.
    fn loop_is_stale(&self, generation: u64) -> bool {
        self.is_shutting_down() || self.loop_generation() != generation
    }

    /// Start a daemon process.
    pub async fn start_daemon(&self, id: &str, args: &[String]) -> AppResult<DaemonResponse> {
        self.start_daemon_with_stdin(id, args, None).await
    }

    /// Like [`start_daemon`], but delivers `stdin_payload` to the child over
    /// stdin and then closes it (the child reads to EOF). Used for the zm-next
    /// worker, whose pipeline config — camera credentials included — is piped in
    /// memory rather than written to a file. When `stdin_payload` is `None`, the
    /// existing process entry's stored payload is reused, so a crash-restart
    /// re-pipes the same config (mirroring how a pipeline file persisted across
    /// restarts).
    pub async fn start_daemon_with_stdin(
        &self,
        id: &str,
        args: &[String],
        stdin_payload: Option<std::sync::Arc<Vec<u8>>>,
    ) -> AppResult<DaemonResponse> {
        // Refuse to spawn during shutdown so a reconcile/health restart can't
        // orphan a daemon that never receives the stop wave.
        if self.is_shutting_down() {
            return Ok(DaemonResponse::error(format!(
                "Manager is shutting down, refusing to start {}",
                id
            )));
        }

        // One map key per process, `"<command> <args>"`, whatever form the
        // caller used: the legacy socket sends daemon "zmc" + args ["-m","1"],
        // which must address the entry start_all_daemons created as
        // "zmc -m 1", not spawn a second zmc under "zmc" (#84).
        let canonical = canonical_daemon_id(id, args);
        let id = canonical.as_str();
        let args: &[String] = &[];

        // Check if already running, and capture any stored stdin payload so a
        // restart (which passes None) re-delivers the same config.
        let existing_payload = {
            let processes = self.processes.read().await;
            match processes.get(id) {
                Some(process) if process.is_running() => {
                    return Ok(DaemonResponse::error(format!(
                        "Daemon {} is already running",
                        id
                    )));
                }
                // SIGTERM sent, exit not yet observed. Spawning now would
                // overwrite this entry's child handle while the old process
                // keeps running, orphaning it (#77). Callers wait_for_stop.
                Some(process) if process.state == ProcessState::Stopping => {
                    return Ok(DaemonResponse::error(format!(
                        "Daemon {} is still stopping",
                        id
                    )));
                }
                Some(process) => process.stdin_payload.clone(),
                None => None,
            }
        };
        let stdin_payload = stdin_payload.or(existing_payload);

        // Parse the daemon command
        let (command, daemon_args) = parse_daemon_command(id, args);

        // Security boundary: reject anything that isn't a known ZM daemon
        // shape before we resolve a path and exec. Without this, an absolute
        // path in `id` would bypass `resolve_daemon_path` (PathBuf::join
        // replaces the base on absolute input) and run arbitrary commands.
        if let Err(reason) = validate_daemon_spec(&command, &daemon_args) {
            warn!(
                "Rejected daemon spawn for id={:?} args={:?}: {}",
                id, args, reason
            );
            return Ok(DaemonResponse::error(format!(
                "Invalid daemon spec: {}",
                reason
            )));
        }

        let full_path = match (command.as_str(), &self.zmnext) {
            ("zm-core", Some(rt)) => self.zmcore_binary(rt),
            _ => self.config.resolve_daemon_path(&command),
        };

        // The daemon will read further inputs from the database itself. Refuse
        // to start it when one of those would reach a shell or module loader
        // (rows written behind the API's back, e.g. straight into MySQL).
        if let Some(db) = &self.db {
            if let Err(reason) = check_stored_inputs(db, &command, &daemon_args, &full_path).await {
                error!("Refusing to start {id}: {reason}");
                return Ok(DaemonResponse::error(format!(
                    "Refusing to start {id}: {reason}"
                )));
            }
        }

        info!("Starting daemon: {} {:?}", full_path.display(), daemon_args);

        // Check if command exists
        if !full_path.exists() {
            return Ok(DaemonResponse::error(format!(
                "Daemon executable not found: {}",
                full_path.display()
            )));
        }

        // Spawn the process with PR_SET_PDEATHSIG on Linux so children die when
        // parent dies. A piped stdin is requested only when we have a payload to
        // deliver (the zm-next worker); other daemons inherit stdin as before.
        let mut child =
            spawn_daemon(&full_path, &daemon_args, stdin_payload.is_some()).map_err(|e| {
                error!("Failed to spawn {}: {}", id, e);
                crate::error::AppError::InternalServerError(format!(
                    "Failed to start {}: {}",
                    id, e
                ))
            })?;

        // Deliver the in-memory payload (pipeline config) over stdin, then close
        // it so the worker sees EOF. Done before taking the processes lock so the
        // (small, sub-pipe-buffer) write never blocks other daemon operations.
        if let Some(bytes) = stdin_payload.as_deref() {
            match child.stdin.take() {
                Some(mut stdin) => {
                    use tokio::io::AsyncWriteExt;
                    if let Err(e) = stdin.write_all(bytes).await {
                        warn!("Failed to write stdin payload to {}: {}", id, e);
                    }
                    // dropping `stdin` here closes the pipe → child reads EOF
                }
                None => warn!("{}: stdin payload set but child has no stdin pipe", id),
            }
        }

        let pid = child.id();

        // Create or update the process entry
        let mut processes = self.processes.write().await;
        let process = processes.entry(id.to_string()).or_insert_with(|| {
            let monitor_id = extract_monitor_id(&daemon_args);
            ManagedProcess::new(id, id, &command, daemon_args.clone(), true, monitor_id)
        });

        // Always sync args so existing entries (e.g. previously corrupted by
        // restart arg duplication) heal back to the canonical exec list.
        process.args = daemon_args.clone();
        // Persist the payload so a future restart re-delivers it.
        process.stdin_payload = stdin_payload;
        // Automatic restarts skip a given-up process, so reaching here with
        // `gave_up` set means someone started it on purpose: count afresh.
        if process.gave_up.take().is_some() {
            process.exit_history = Default::default();
        }
        process.set_child(child);

        // Update PID map
        if let Some(p) = pid {
            let mut pid_map = self.pid_map.write().await;
            pid_map.insert(p, id.to_string());
        }

        info!("Started daemon {} with PID {:?}", id, pid);
        Ok(DaemonResponse::ok(format!(
            "Started {} (PID: {:?})",
            id, pid
        )))
    }

    /// Where zm-core would be run from, and whether it's there. `None` when
    /// zm-next isn't enabled.
    pub fn zmcore_installed(&self) -> Option<(PathBuf, bool)> {
        let rt = self.zmnext.as_ref()?;
        let path = self.zmcore_binary(rt);
        let present = path.is_file();
        Some((path, present))
    }

    /// The zm-core binary: `[zmnext.worker].binary` when absolute, otherwise
    /// looked up like any other daemon.
    fn zmcore_binary(&self, rt: &ZmNextRuntime) -> PathBuf {
        let configured = std::path::Path::new(&rt.config.worker.binary);
        if configured.is_absolute() {
            configured.to_path_buf()
        } else {
            self.config.resolve_daemon_path(&rt.config.worker.binary)
        }
    }

    /// Stop a daemon process gracefully.
    ///
    /// This follows zmdc.pl behavior:
    /// 1. Send SIGTERM to allow graceful shutdown
    /// 2. The health check loop monitors terminating processes
    /// 3. After shutdown_timeout (30s), SIGKILL is sent
    pub async fn stop_daemon(&self, id: &str) -> AppResult<DaemonResponse> {
        let mut processes = self.processes.write().await;

        let process = match processes.get_mut(id) {
            Some(p) => p,
            None => {
                return Ok(DaemonResponse::error(format!("Daemon {} not found", id)));
            }
        };

        if !process.can_stop() {
            return Ok(DaemonResponse::error(format!(
                "Daemon {} is not running (state: {})",
                id, process.state
            )));
        }

        // Disable auto-restart during intentional stop
        process.auto_restart = false;

        // Get the PID for sending signal
        let pid = match process.pid {
            Some(p) => p,
            None => {
                process.set_state(ProcessState::Stopped);
                return Ok(DaemonResponse::ok(format!("Stopped {} (no PID)", id)));
            }
        };

        // Send SIGTERM for graceful shutdown (like zmdc.pl's send_stop)
        #[cfg(unix)]
        {
            use nix::sys::signal::{kill, Signal};
            use nix::unistd::Pid;

            match kill(Pid::from_raw(pid as i32), Signal::SIGTERM) {
                Ok(_) => {
                    info!("Sent SIGTERM to daemon {} (PID {})", id, pid);
                    process.mark_term_sent();
                }
                Err(e) => {
                    // Process may have already exited
                    warn!(
                        "Failed to send SIGTERM to daemon {} (PID {}): {}",
                        id, pid, e
                    );
                    process.set_state(ProcessState::Stopped);

                    // Clean up PID map
                    drop(processes); // Release lock before acquiring pid_map lock
                    let mut pid_map = self.pid_map.write().await;
                    pid_map.remove(&pid);

                    return Ok(DaemonResponse::ok(format!(
                        "Stopped {} (process already gone)",
                        id
                    )));
                }
            }
        }

        #[cfg(not(unix))]
        {
            // On non-Unix, fall back to tokio's kill (which is SIGKILL)
            if let Some(child) = process.child_mut() {
                let _ = child.kill().await;
            }
            process.set_state(ProcessState::Stopped);

            drop(processes);
            let mut pid_map = self.pid_map.write().await;
            pid_map.remove(&pid);

            return Ok(DaemonResponse::ok(format!("Stopped {}", id)));
        }

        Ok(DaemonResponse::ok(format!(
            "Sent stop signal to {} (PID {}), waiting for graceful shutdown",
            id, pid
        )))
    }

    /// Force kill a daemon process immediately with SIGKILL.
    ///
    /// Use this when graceful shutdown has timed out or immediate termination is needed.
    pub async fn kill_daemon(&self, id: &str) -> AppResult<DaemonResponse> {
        let mut processes = self.processes.write().await;

        let process = match processes.get_mut(id) {
            Some(p) => p,
            None => {
                return Ok(DaemonResponse::error(format!("Daemon {} not found", id)));
            }
        };

        let pid = process.pid;

        // Try to kill via child handle first
        if let Some(child) = process.child_mut() {
            match child.kill().await {
                Ok(_) => {
                    info!("Sent SIGKILL to daemon {} via child handle", id);
                }
                Err(e) => {
                    warn!("Failed to kill daemon {} via child handle: {}", id, e);
                }
            }
        }

        // Also try sending SIGKILL directly to PID (in case child handle is stale)
        #[cfg(unix)]
        if let Some(p) = pid {
            use nix::sys::signal::{kill, Signal};
            use nix::unistd::Pid;

            let _ = kill(Pid::from_raw(p as i32), Signal::SIGKILL);
        }

        // Update state
        process.set_state(ProcessState::Stopped);
        process.term_sent_at = None;

        // Clean up PID map
        if let Some(p) = pid {
            drop(processes);
            let mut pid_map = self.pid_map.write().await;
            pid_map.remove(&p);
        }

        Ok(DaemonResponse::ok(format!("Killed {}", id)))
    }

    /// Check for processes that need to be force-killed after timeout.
    ///
    /// This matches zmdc.pl's check_for_processes_to_kill behavior:
    /// processes that were sent SIGTERM but haven't died after KILL_DELAY
    /// get sent SIGKILL.
    pub async fn check_terminating_processes(&self) {
        let timeout = self.config.shutdown_timeout();
        let mut to_kill = Vec::new();

        {
            let processes = self.processes.read().await;
            for (id, process) in processes.iter() {
                if process.state == ProcessState::Stopping && process.term_timeout_expired(timeout)
                {
                    to_kill.push(id.clone());
                }
            }
        }

        for id in to_kill {
            warn!(
                "Daemon {} has not stopped after {:?}, sending SIGKILL",
                id, timeout
            );
            if let Err(e) = self.kill_daemon(&id).await {
                error!("Failed to force kill daemon {}: {}", id, e);
            }
        }
    }

    /// Restart a daemon process.
    ///
    /// SIGTERM is sent and we wait for the child to actually exit (escalating
    /// to SIGKILL after `shutdown_timeout`) before spawning the replacement;
    /// otherwise the new spawn overwrites the manager's record while the old
    /// process keeps running, leaking it as an orphan.
    ///
    /// If `provided_args` is non-empty, those args are used. Otherwise, we
    /// derive the "extras" (args not already encoded in `id`) from the existing
    /// process entry so they don't get duplicated when `start_daemon` re-parses
    /// `id` and concatenates.
    pub async fn restart_daemon(
        &self,
        id: &str,
        provided_args: &[String],
    ) -> AppResult<DaemonResponse> {
        let args = if !provided_args.is_empty() {
            provided_args.to_vec()
        } else {
            let processes = self.processes.read().await;
            processes
                .get(id)
                .map(|p| extra_args_from_id(id, &p.args))
                .unwrap_or_default()
        };

        // Stop the running process and wait for it to actually exit.
        let needs_stop = {
            let processes = self.processes.read().await;
            processes.get(id).is_some_and(|p| p.is_running())
        };

        if needs_stop {
            self.stop_daemon(id).await?;
            self.wait_for_stop(id).await;
        }

        // Re-enable auto-restart for the fresh spawn (stop_daemon disabled it).
        {
            let mut processes = self.processes.write().await;
            if let Some(process) = processes.get_mut(id) {
                process.auto_restart = true;
            }
        }

        self.start_daemon(id, &args).await
    }

    /// Wait for a daemon to exit after `stop_daemon` sent SIGTERM, escalating
    /// to SIGKILL once `shutdown_timeout` elapses. Mirrors the wait loop in
    /// `shutdown_all` but scoped to a single daemon. Returns once the process
    /// is no longer in `Stopping` state (whether it exited, was killed, or was
    /// never present).
    async fn wait_for_stop(&self, id: &str) {
        let timeout = self.config.shutdown_timeout();
        let tick = std::time::Duration::from_millis(100);
        let start = std::time::Instant::now();
        let mut escalated = false;

        loop {
            // Drive try_wait() ourselves so we don't have to wait for the
            // health-check loop's longer interval (default 10s).
            self.check_daemons().await;

            let still_stopping = {
                let processes = self.processes.read().await;
                processes
                    .get(id)
                    .is_some_and(|p| p.state == ProcessState::Stopping)
            };

            if !still_stopping {
                return;
            }

            if !escalated && start.elapsed() >= timeout {
                warn!(
                    "Daemon {} has not stopped after {:?}, escalating to SIGKILL",
                    id, timeout
                );
                self.check_terminating_processes().await;
                escalated = true;
            }

            if start.elapsed() >= timeout * 2 {
                warn!(
                    "Daemon {} still in Stopping state after SIGKILL escalation; giving up wait",
                    id
                );
                return;
            }

            tokio::time::sleep(tick).await;
        }
    }

    /// Send SIGHUP to reload daemon configuration.
    pub async fn reload_daemon(&self, id: &str) -> AppResult<DaemonResponse> {
        // zm-next has no live config reload: "reload" means regenerate the
        // pipeline JSON from the current DB rows and restart the worker so it
        // re-reads it (in-memory, over stdin). SIGHUP would be a no-op for it.
        if let Some(monitor_id) = zmnext_monitor_id_from_id(id) {
            let Some(rt) = self.zmnext.clone() else {
                return Ok(DaemonResponse::error("zm-next is not configured"));
            };
            let monitor = match &self.db {
                Some(db) => {
                    monitors::Entity::find_by_id(monitor_id)
                        .one(db.as_ref())
                        .await?
                }
                None => None,
            };
            let Some(monitor) = monitor else {
                return Ok(DaemonResponse::error(format!(
                    "Monitor {monitor_id} not found for zm-next reload"
                )));
            };
            // Regenerate the in-memory payload and stash it on the process entry
            // so the restart (which passes no payload) re-delivers the fresh
            // config over stdin. If no entry exists yet, start the worker fresh.
            let payload =
                std::sync::Arc::new(self.build_zmnext_pipeline_payload(&monitor, &rt).await?);
            let had_entry = {
                let mut processes = self.processes.write().await;
                match processes.get_mut(id) {
                    Some(p) => {
                        p.stdin_payload = Some(payload);
                        true
                    }
                    None => false,
                }
            };
            info!("Regenerated zm-next pipeline for monitor {monitor_id}, restarting worker");
            if had_entry {
                return self.restart_daemon(id, &[]).await;
            }
            return self.start_zmnext_worker(&monitor).await;
        }

        let processes = self.processes.read().await;

        let process = match processes.get(id) {
            Some(p) => p,
            None => {
                return Ok(DaemonResponse::error(format!("Daemon {} not found", id)));
            }
        };

        if !process.is_running() {
            return Ok(DaemonResponse::error(format!(
                "Daemon {} is not running",
                id
            )));
        }

        let pid = match process.pid {
            Some(p) => p,
            None => {
                return Ok(DaemonResponse::error(format!("Daemon {} has no PID", id)));
            }
        };

        // Send SIGHUP
        #[cfg(unix)]
        {
            use nix::sys::signal::{kill, Signal};
            use nix::unistd::Pid;

            match kill(Pid::from_raw(pid as i32), Signal::SIGHUP) {
                Ok(_) => {
                    info!("Sent SIGHUP to daemon {} (PID {})", id, pid);
                    Ok(DaemonResponse::ok(format!(
                        "Sent reload signal to {} (PID {})",
                        id, pid
                    )))
                }
                Err(e) => Ok(DaemonResponse::error(format!(
                    "Failed to send SIGHUP to {}: {}",
                    id, e
                ))),
            }
        }

        #[cfg(not(unix))]
        {
            Ok(DaemonResponse::error(
                "SIGHUP not supported on this platform",
            ))
        }
    }

    /// Get status of all daemons.
    pub async fn get_status(&self) -> SystemStatus {
        let processes = self.processes.read().await;
        let running = *self.running.read().await;

        let daemons: Vec<ProcessStatus> = processes.values().map(ProcessStatus::from).collect();

        // Collect current system stats
        let stats: Option<SystemStats> = stats::collect_stats().ok();

        SystemStatus {
            running,
            daemons,
            stats,
        }
    }

    /// Get status of a specific daemon.
    pub async fn get_daemon_status(&self, id: &str) -> Option<ProcessStatus> {
        let processes = self.processes.read().await;
        processes.get(id).map(ProcessStatus::from)
    }

    /// Shutdown all daemons gracefully.
    ///
    /// This matches zmdc.pl's shutdownAll behavior:
    /// 1. Send SIGTERM to all processes
    /// 2. Wait for processes to exit (up to shutdown_timeout)
    /// 3. Force kill any remaining processes with SIGKILL
    pub async fn shutdown_all(&self) -> AppResult<DaemonResponse> {
        info!("Shutting down all daemons");

        // Quiesce background loops first so reconciliation/health checks can't
        // re-spawn the daemons we're about to stop.
        self.signal_shutdown();

        let timeout = self.config.shutdown_timeout();
        let check_interval = std::time::Duration::from_millis(500);

        // Step 1: Send SIGTERM to all running processes
        let ids: Vec<String> = {
            let processes = self.processes.read().await;
            processes
                .iter()
                .filter(|(_, p)| p.is_running() || p.state == ProcessState::Starting)
                .map(|(id, _)| id.clone())
                .collect()
        };

        let total = ids.len();
        info!("Sending stop signal to {} daemons", total);

        for id in &ids {
            let _ = self.stop_daemon(id).await;
        }

        // Step 2: Wait for processes to exit, checking periodically
        let start = std::time::Instant::now();
        loop {
            // Check for exited processes
            self.check_daemons().await;

            // Count how many are still stopping
            let still_stopping = {
                let processes = self.processes.read().await;
                processes
                    .values()
                    .filter(|p| p.state == ProcessState::Stopping)
                    .count()
            };

            if still_stopping == 0 {
                info!("All daemons have stopped");
                break;
            }

            if start.elapsed() >= timeout {
                warn!(
                    "{} daemons still running after {:?}, sending SIGKILL",
                    still_stopping, timeout
                );
                // Force kill remaining processes
                self.check_terminating_processes().await;
                break;
            }

            debug!(
                "Waiting for {} daemons to stop ({:?} elapsed)",
                still_stopping,
                start.elapsed()
            );
            tokio::time::sleep(check_interval).await;
        }

        // Mark as not running
        *self.running.write().await = false;

        // Count final results
        let stopped = {
            let processes = self.processes.read().await;
            processes
                .values()
                .filter(|p| p.state == ProcessState::Stopped)
                .count()
        };

        if stopped == total {
            Ok(DaemonResponse::ok(format!(
                "Shutdown complete: {} daemons stopped",
                stopped
            )))
        } else {
            Ok(DaemonResponse::ok(format!(
                "Shutdown complete: {} of {} daemons stopped",
                stopped, total
            )))
        }
    }

    /// Start the daemon manager and background health check task.
    pub async fn startup(self: &Arc<Self>) -> AppResult<DaemonResponse> {
        let mut running = self.running.write().await;
        if *running {
            return Ok(DaemonResponse::error("Daemon manager already running"));
        }

        *running = true;
        self.shutting_down.store(false, Ordering::SeqCst);
        // Retire any loop from a previous startup that is still mid-tick.
        let generation = self.loop_generation.fetch_add(1, Ordering::SeqCst) + 1;
        drop(running); // Release lock before spawning

        // The watchdog reads zmc's shared memory from wherever ZoneMinder
        // puts it; the default only holds for a stock install.
        if let Some(db) = &self.db {
            match crate::repo::config::get_config_value(db, "ZM_PATH_MAP").await {
                Ok(Some(dir)) if !dir.trim().is_empty() => self.set_shm_dir(dir.trim()),
                Ok(_) => {}
                Err(e) => {
                    warn!("Could not read ZM_PATH_MAP: {e} (watchdog uses {DEFAULT_SHM_PATH})")
                }
            }
        }

        // Start background health check task
        let manager = Arc::clone(self);
        let check_interval = self.config.watch_check_interval();
        let max_delay = self.config.watch_max_delay();

        tokio::spawn(async move {
            manager
                .run_health_check_loop(check_interval, max_delay, generation)
                .await;
        });

        // Start server status update loop if server_id is configured
        if let (Some(server_id), Some(db)) = (self.server_id, &self.db) {
            let manager = Arc::clone(self);
            let db = Arc::clone(db);
            info!(
                "Starting server status update loop for server_id={}",
                server_id
            );
            tokio::spawn(async move {
                manager
                    .run_server_status_loop(db, server_id, generation)
                    .await;
            });
        }

        // Start monitor reconciliation loop (syncs DB state with running daemons)
        if self.db.is_some() {
            let manager = Arc::clone(self);
            info!("Starting monitor reconciliation loop");
            tokio::spawn(async move {
                manager.run_reconciliation_loop(generation).await;
            });
        }

        info!("Daemon manager started with health monitoring");

        Ok(DaemonResponse::ok("Daemon manager started"))
    }

    /// Apply a named ZoneMinder run state: reconfigure the monitor rows in the
    /// database, then restart the supervised daemons so they pick up the new
    /// config. This is the active/takeover-mode path used by the legacy IPC
    /// `ApplyState` command — the socket only runs when this process supervises
    /// the daemons. The HTTP path is `service::daemon::apply_state`, and both
    /// share the DB-side logic via `service::daemon::apply_state_to_db`.
    pub async fn apply_state(self: &Arc<Self>, state_name: &str) -> DaemonResponse {
        let db = match &self.db {
            Some(db) => db.clone(),
            None => {
                return DaemonResponse::error(
                    "Database not configured - cannot apply state".to_string(),
                )
            }
        };

        let updated = match crate::service::daemon::apply_state_to_db(db.as_ref(), state_name).await
        {
            Ok(n) => n,
            Err(e) => return DaemonResponse::error(format!("apply state '{state_name}': {e}")),
        };

        // We supervise the daemons here, so restart them to apply the change.
        let _ = self.shutdown_all().await;
        tokio::time::sleep(Duration::from_millis(500)).await;
        match self.start_all_daemons().await {
            Ok(resp) => DaemonResponse::ok(format!(
                "State '{state_name}' applied: {updated} monitors updated. {}",
                resp.message
            )),
            Err(e) => DaemonResponse::error(format!(
                "state '{state_name}' applied ({updated} monitors) but daemon restart failed: {e}"
            )),
        }
    }

    /// Start all ZoneMinder daemons.
    ///
    /// This matches the behavior of zmpkg.pl startup:
    /// 1. Ensure state table sanity (default state exists, one active)
    /// 2. Query monitors that are not deleted and have Capturing != None
    /// 3. Skip WebSite type monitors (they don't need capture daemons)
    /// 4. For Local type monitors: start `zmc -d <device>`
    /// 5. For other types: start `zmc -m <id>`
    /// 6. Start zma for monitors requiring motion detection (Modect/Mocord)
    /// 7. Start zmcontrol.pl + zmtrack.pl for controllable monitors (gated on `ZM_OPT_CONTROL`)
    /// 8. Start one `zmfilter.pl --filter_id=<id> --daemon` per Background=1 filter
    /// 9. Start singleton daemons, each gated by the matching `ZM_*` Config key
    ///    and (in multi-server mode) the corresponding `Servers` column.
    pub async fn start_all_daemons(self: &Arc<Self>) -> AppResult<DaemonResponse> {
        let db = match &self.db {
            Some(db) => db.clone(),
            None => {
                return Ok(DaemonResponse::error(
                    "Database not configured - cannot query monitors",
                ));
            }
        };

        // First ensure we're running (starts health check loop)
        if !self.is_running().await {
            self.startup().await?;
        }

        // Kill daemons left behind by a previous zm-api, once. Later startup
        // calls arrive with our own children running and must leave them be.
        // Never over a live zmdc.pl: that is ZoneMinder still supervising, and
        // the sweep would kill its children out from under it (#118).
        if !self.orphans_swept.load(Ordering::SeqCst) {
            let socket = self.config.socket_file();
            if crate::daemon::ipc::socket::legacy_socket_is_live(&socket).await {
                let msg = format!(
                    "{} is being served (zmdc.pl, or another zm-api); refusing to take over \
                     daemon control while it runs",
                    socket.display()
                );
                error!("{msg}");
                return Ok(DaemonResponse::error(msg));
            }
            if self.orphan_sweep_due() {
                kill_orphan_daemons().await;
            }
        }

        // Ensure state table sanity (matches zmpkg.pl behavior)
        if let Err(e) = crate::service::daemon::ensure_state_sanity(db.as_ref()).await {
            warn!("Failed to ensure state sanity: {}", e);
            // Continue anyway - this is not fatal
        }

        // Resolve all upstream-equivalent gates (Config keys + per-server overrides)
        // up front so each daemon decision is a cheap field lookup.
        let gates = self.load_startup_gates(db.as_ref()).await;

        let mut started = 0;
        let mut failed = 0;
        let mut errors: Vec<String> = Vec::new();

        // Query monitors from database
        // Match zmpkg.pl logic: Deleted => 0, Capturing != 'None'
        let monitor_list = monitors::Entity::find()
            .filter(monitors::Column::Deleted.eq(false))
            .all(db.as_ref())
            .await?;

        // Filter monitors that need capture (Capturing != None)
        // Also filter by server_id if configured (multi-server support)
        let monitors_to_start: Vec<_> = monitor_list
            .iter()
            .filter(|m| {
                // Skip if Capturing is None
                if matches!(m.capturing, Capturing::None) {
                    return false;
                }
                // Skip WebSite type monitors (they don't need zmc)
                if matches!(m.r#type, MonitorType::WebSite) {
                    return false;
                }
                // Multi-server support: if server_id is configured, filter by it
                if let Some(our_server_id) = self.server_id {
                    if let Some(monitor_server_id) = m.server_id {
                        return monitor_server_id == our_server_id;
                    }
                    // If monitor has no server_id, include it (backwards compatibility)
                }
                true
            })
            .collect();

        info!(
            "Found {} monitors to start (of {} total)",
            monitors_to_start.len(),
            monitor_list.len()
        );

        // Start per-monitor daemons
        for monitor in &monitors_to_start {
            let monitor_id = monitor.id;
            let function = &monitor.function;
            let monitor_type = &monitor.r#type;

            // zm-next monitors run a single self-contained worker (capture +
            // analysis + record) instead of zmc/zma/zmcontrol. PTZ for these is
            // handled out-of-band via zm-api's ONVIF client, not a daemon.
            if self.use_zmnext(monitor_id).await {
                match self.start_zmnext_worker(monitor).await {
                    Ok(resp) if resp.success => {
                        started += 1;
                        info!("Started zm-next worker for monitor {}", monitor_id);
                    }
                    Ok(resp) => {
                        if !resp.message.contains("already running") {
                            failed += 1;
                            errors.push(format!("zm-core[{}]: {}", monitor_id, resp.message));
                            warn!(
                                "Failed to start zm-next worker for monitor {}: {}",
                                monitor_id, resp.message
                            );
                        }
                    }
                    Err(e) => {
                        failed += 1;
                        errors.push(format!("zm-core[{}]: {}", monitor_id, e));
                        error!(
                            "Error starting zm-next worker for monitor {}: {}",
                            monitor_id, e
                        );
                    }
                }
                continue;
            }

            // Determine if we need analysis daemon (motion detection)
            let needs_analysis = matches!(function, Function::Modect | Function::Mocord);

            debug!(
                "Monitor {} ({}): type={:?}, function={:?}, analysis={}",
                monitor_id, monitor.name, monitor_type, function, needs_analysis
            );

            // Start zmc (capture daemon). The id form (-d <device> vs -m <id>)
            // must match every other call site — see `zmc_daemon_id`.
            let daemon_id = zmc_daemon_id(monitor_type, &monitor.device, monitor_id);
            let daemon_desc = format!("zmc for monitor {}", monitor_id);

            match self.start_daemon(&daemon_id, &[]).await {
                Ok(resp) if resp.success => {
                    started += 1;
                    info!("Started {}", daemon_desc);
                }
                Ok(resp) => {
                    if !resp.message.contains("already running") {
                        failed += 1;
                        errors.push(format!("{}: {}", daemon_id, resp.message));
                        warn!("Failed to start {}: {}", daemon_desc, resp.message);
                    }
                }
                Err(e) => {
                    failed += 1;
                    errors.push(format!("{}: {}", daemon_id, e));
                    error!("Error starting {}: {}", daemon_desc, e);
                }
            }

            // No zma here: analysis runs inside zmc (zmpkg.pl stopped starting
            // zma per monitor in 1.36), and on 1.39 `zma` is a per-event
            // re-analysis tool that exits at once (#160).

            // PTZ control + motion tracking - both gated on ZM_OPT_CONTROL and
            // a controllable monitor (matches zmpkg.pl:222-233 nesting).
            if gates.zm_opt_control && monitor.controllable != 0 {
                let daemon_id = format!("zmcontrol.pl --id {}", monitor_id);
                match self.start_daemon(&daemon_id, &[]).await {
                    Ok(resp) if resp.success => {
                        started += 1;
                        info!("Started zmcontrol.pl for monitor {}", monitor_id);
                    }
                    Ok(resp) => {
                        if !resp.message.contains("already running")
                            && !resp.message.contains("not found")
                        {
                            warn!(
                                "Could not start zmcontrol.pl for monitor {}: {}",
                                monitor_id, resp.message
                            );
                        }
                    }
                    Err(e) => {
                        // zmcontrol.pl may not be installed - just warn
                        warn!(
                            "Could not start zmcontrol.pl for monitor {}: {}",
                            monitor_id, e
                        );
                    }
                }

                if monitor.track_motion != 0 {
                    if !needs_analysis {
                        warn!(
                            "Monitor {} is set to track motion but motion detection is not enabled",
                            monitor_id
                        );
                    } else {
                        let daemon_id = format!("zmtrack.pl -m {}", monitor_id);
                        match self.start_daemon(&daemon_id, &[]).await {
                            Ok(resp) if resp.success => {
                                started += 1;
                                info!("Started zmtrack.pl for monitor {}", monitor_id);
                            }
                            Ok(resp) => {
                                if !resp.message.contains("already running")
                                    && !resp.message.contains("not found")
                                {
                                    warn!(
                                        "Could not start zmtrack.pl for monitor {}: {}",
                                        monitor_id, resp.message
                                    );
                                }
                            }
                            Err(e) => {
                                // zmtrack.pl may not be installed - just warn
                                warn!(
                                    "Could not start zmtrack.pl for monitor {}: {}",
                                    monitor_id, e
                                );
                            }
                        }
                    }
                }
            }
        }

        // Per-filter zmfilter.pl processes (mirrors zmpkg.pl: one --daemon
        // process per Background=1 filter, pinned with --filter_id).
        match filters::Entity::find()
            .filter(filters::Column::Background.eq(1u8))
            .all(db.as_ref())
            .await
        {
            Ok(bg_filters) => {
                info!("Found {} background filters to run", bg_filters.len());
                for f in bg_filters {
                    let daemon_id = format!("zmfilter.pl --filter_id={}", f.id);
                    let args = vec!["--daemon".to_string()];
                    match self.start_daemon(&daemon_id, &args).await {
                        Ok(resp) if resp.success => {
                            started += 1;
                            info!("Started zmfilter for filter {} ({})", f.id, f.name);
                        }
                        Ok(resp) => {
                            if !resp.message.contains("already running") {
                                warn!(
                                    "Could not start zmfilter for filter {}: {}",
                                    f.id, resp.message
                                );
                            }
                        }
                        Err(e) => {
                            warn!("Error starting zmfilter for filter {}: {}", f.id, e);
                        }
                    }
                }
            }
            Err(e) => {
                warn!("Could not query background filters: {}", e);
            }
        }

        // Start singleton daemons in priority order, gated to match zmpkg.pl.
        let mut singletons: Vec<_> = DaemonDefinition::singletons()
            .filter(|d| d.requires_db)
            .collect();
        singletons.sort_by_key(|d| d.priority);

        for daemon in singletons {
            // A Perl daemon whose job zm-api now does natively is not started,
            // whatever ZoneMinder's own Config/Servers flags say: running both
            // has them competing over the same rows. An explicit `start` over
            // the socket or REST is still honoured — this gates the automatic
            // start only.
            if let Some(table) = native_replacement_for(daemon.name, self.native_jobs) {
                info!(
                    "Not starting {}: {} is enabled and replaces it",
                    daemon.command, table
                );
                continue;
            }

            // Per-daemon gating that mirrors zmpkg.pl. Each `continue` includes
            // a debug log so operators can see why a daemon was skipped.
            match daemon.name {
                // zmfilter is started per-filter above; skip the global instance.
                "zmfilter" => continue,
                "zmaudit" => {
                    if !gates.zm_run_audit {
                        debug!("Skipping zmaudit.pl: ZM_RUN_AUDIT is disabled");
                        continue;
                    }
                    if !gates.server_zmaudit {
                        debug!("Skipping zmaudit.pl: disabled for this server");
                        continue;
                    }
                }
                "zmtrigger" => {
                    if !gates.zm_opt_triggers {
                        debug!("Skipping zmtrigger.pl: ZM_OPT_TRIGGERS is disabled");
                        continue;
                    }
                    if !gates.server_zmtrigger {
                        debug!("Skipping zmtrigger.pl: disabled for this server");
                        continue;
                    }
                }
                "zmtelemetry" if !gates.zm_telemetry_data => {
                    debug!("Skipping zmtelemetry.pl: ZM_TELEMETRY_DATA is disabled");
                    continue;
                }
                "zmeventnotification" => {
                    if !gates.zm_opt_use_eventnotification {
                        debug!(
                            "Skipping zmeventnotification.pl: ZM_OPT_USE_EVENTNOTIFICATION is disabled"
                        );
                        continue;
                    }
                    if !gates.server_zmeventnotification {
                        debug!("Skipping zmeventnotification.pl: disabled for this server");
                        continue;
                    }
                }
                "zmstats" if !gates.server_zmstats => {
                    debug!("Skipping zmstats.pl: disabled for this server");
                    continue;
                }
                // zmpkg.pl starts the restream server whenever an RTSP port
                // is configured (#161).
                "zm_rtsp_server" if !gates.zm_rtsp_server => {
                    debug!("Skipping zm_rtsp_server: ZM_MIN_RTSP_PORT is not set");
                    continue;
                }
                _ => {}
            }

            debug!(
                "Starting singleton daemon: {} (priority {})",
                daemon.name, daemon.priority
            );

            let args: Vec<String> = daemon.default_args.iter().map(|s| s.to_string()).collect();
            match self.start_daemon(daemon.command, &args).await {
                Ok(resp) if resp.success => {
                    started += 1;
                    info!("Started {}", daemon.name);
                }
                Ok(resp) => {
                    if !resp.message.contains("already running") {
                        warn!("Could not start {}: {}", daemon.name, resp.message);
                    }
                }
                Err(e) => {
                    warn!("Could not start {}: {}", daemon.name, e);
                }
            }
        }

        let message = if failed > 0 {
            format!(
                "System startup completed: {} daemons started, {} failed. Errors: {}",
                started,
                failed,
                errors.join("; ")
            )
        } else {
            format!("System startup completed: {} daemons started", started)
        };

        info!("{}", message);

        if failed > 0 && started == 0 {
            Ok(DaemonResponse::error(message))
        } else {
            Ok(DaemonResponse::ok(message))
        }
    }

    /// Resolve the upstream-equivalent startup gates from `Config` and the
    /// per-server `Servers` row (in multi-server mode).
    async fn load_startup_gates(&self, db: &DatabaseConnection) -> StartupGates {
        let (
            zm_opt_control,
            zm_opt_triggers,
            zm_opt_use_eventnotification,
            zm_telemetry_data,
            zm_run_audit,
            zm_min_rtsp_port,
        ) = tokio::join!(
            read_bool_config(db, "ZM_OPT_CONTROL"),
            read_bool_config(db, "ZM_OPT_TRIGGERS"),
            read_bool_config(db, "ZM_OPT_USE_EVENTNOTIFICATION"),
            read_bool_config(db, "ZM_TELEMETRY_DATA"),
            read_bool_config(db, "ZM_RUN_AUDIT"),
            crate::repo::config::get_config_value(db, "ZM_MIN_RTSP_PORT"),
        );
        let zm_rtsp_server = rtsp_port_configured(zm_min_rtsp_port.ok().flatten().as_deref());

        let (server_zmstats, server_zmaudit, server_zmtrigger, server_zmeventnotification) =
            match self.server_id {
                Some(id) => match servers::Entity::find_by_id(id).one(db).await {
                    Ok(Some(s)) => (
                        s.zmstats != 0,
                        s.zmaudit != 0,
                        s.zmtrigger != 0,
                        s.zmeventnotification != 0,
                    ),
                    Ok(None) => {
                        warn!(
                            "Server id {} not found in DB; treating per-server daemon flags as enabled",
                            id
                        );
                        (true, true, true, true)
                    }
                    Err(e) => {
                        warn!(
                            "Failed to load Server row for id {}: {} (treating per-server daemon flags as enabled)",
                            id, e
                        );
                        (true, true, true, true)
                    }
                },
                None => (true, true, true, true),
            };

        StartupGates {
            zm_opt_control,
            zm_opt_triggers,
            zm_opt_use_eventnotification,
            zm_telemetry_data,
            zm_run_audit,
            zm_rtsp_server,
            server_zmstats,
            server_zmaudit,
            server_zmtrigger,
            server_zmeventnotification,
        }
    }

    /// Run the background health check loop.
    async fn run_health_check_loop(
        &self,
        check_interval: std::time::Duration,
        max_delay: std::time::Duration,
        generation: u64,
    ) {
        // Initial delay before first check (like zmwatch.pl's 30 second delay)
        let startup_delay = std::time::Duration::from_secs(30);

        info!(
            "Health monitor starting (startup delay: {:?}, check interval: {:?}, max delay: {:?})",
            startup_delay, check_interval, max_delay
        );

        tokio::select! {
            _ = tokio::time::sleep(startup_delay) => {},
            _ = self.shutdown.notified() => {
                info!("Health monitor shutdown during startup delay");
                return;
            }
        }

        let mut interval = tokio::time::interval(check_interval);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            tokio::select! {
                _ = interval.tick() => {
                    if self.loop_is_stale(generation) {
                        info!("Health monitor retired (generation {generation})");
                        break;
                    }
                    self.perform_health_check(max_delay).await;
                }
                _ = self.shutdown.notified() => {
                    info!("Health monitor received shutdown signal");
                    break;
                }
            }
        }

        info!("Health monitor stopped");
    }

    /// Run periodic server status updates (every
    /// `[daemon].stats_update_interval_seconds`).
    ///
    /// This matches the behavior of zmdc.pl which updates the Server record
    /// with CPU load, memory usage, and other statistics.
    async fn run_server_status_loop(
        &self,
        db: Arc<DatabaseConnection>,
        server_id: u32,
        generation: u64,
    ) {
        let update_interval = self.config.stats_update_interval();

        info!(
            "Server status loop starting (server_id={}, interval={:?})",
            server_id, update_interval
        );

        let mut interval = tokio::time::interval(update_interval);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        // Immediately set status to Running on first tick
        let _ = interval.tick().await;
        if let Err(e) = self.update_server_status(&db, server_id).await {
            error!("Failed initial server status update: {}", e);
        }

        loop {
            tokio::select! {
                _ = interval.tick() => {
                    if self.loop_is_stale(generation) {
                        info!("Server status loop retired (generation {generation})");
                        break;
                    }
                    if let Err(e) = self.update_server_status(&db, server_id).await {
                        error!("Failed to update server status: {}", e);
                    }
                }
                _ = self.shutdown.notified() => {
                    // Set status to NotRunning on shutdown
                    if let Err(e) = self.set_server_not_running(&db, server_id).await {
                        error!("Failed to set server NotRunning status: {}", e);
                    }
                    break;
                }
            }
        }

        info!("Server status loop stopped");
    }

    /// Update the Server record with current system statistics.
    async fn update_server_status(&self, db: &DatabaseConnection, server_id: u32) -> AppResult<()> {
        let stats = crate::daemon::stats::collect_stats().map_err(|e| {
            crate::error::AppError::InternalServerError(format!("Failed to collect stats: {}", e))
        })?;

        let server = servers::Entity::find_by_id(server_id)
            .one(db)
            .await?
            .ok_or_else(|| {
                crate::error::AppError::NotFoundError(crate::error::Resource {
                    resource_type: crate::error::ResourceType::Config,
                    details: vec![("server_id".to_string(), server_id.to_string())],
                })
            })?;

        let mut active: servers::ActiveModel = server.into();
        active.status = Set(Status::Running);
        active.cpu_load = Set(Decimal::from_f64(stats.cpu_load));
        active.cpu_user_percent = Set(Decimal::from_f64(stats.cpu_user_percent));
        active.cpu_nice_percent = Set(Decimal::from_f64(stats.cpu_nice_percent));
        active.cpu_system_percent = Set(Decimal::from_f64(stats.cpu_system_percent));
        active.cpu_idle_percent = Set(Decimal::from_f64(stats.cpu_idle_percent));
        active.cpu_usage_percent = Set(Decimal::from_f64(stats.cpu_usage_percent));
        active.total_mem = Set(Some(stats.total_mem));
        active.free_mem = Set(Some(stats.free_mem));
        active.total_swap = Set(Some(stats.total_swap));
        active.free_swap = Set(Some(stats.free_swap));
        active.update(db).await?;

        debug!(
            "Updated server {} status: cpu_load={:.1}, cpu_usage={:.1}%, mem_used={:.1}%",
            server_id,
            stats.cpu_load,
            stats.cpu_usage_percent,
            if stats.total_mem > 0 {
                ((stats.total_mem - stats.free_mem) as f64 / stats.total_mem as f64) * 100.0
            } else {
                0.0
            }
        );

        Ok(())
    }

    /// Set the Server status to NotRunning (called on shutdown).
    async fn set_server_not_running(
        &self,
        db: &DatabaseConnection,
        server_id: u32,
    ) -> AppResult<()> {
        let server = servers::Entity::find_by_id(server_id).one(db).await?;

        if let Some(server) = server {
            let mut active: servers::ActiveModel = server.into();
            active.status = Set(Status::NotRunning);
            active.update(db).await?;
            info!("Set server {} status to NotRunning", server_id);
        }

        Ok(())
    }

    /// Run the monitor reconciliation loop.
    ///
    /// This periodically compares the desired state (from database) with the
    /// actual running state (daemons) and starts/stops processes as needed.
    /// This provides self-healing when:
    /// - The API crashes between DB update and daemon control
    /// - Daemons are started/stopped externally
    /// - System restarts
    async fn run_reconciliation_loop(&self, generation: u64) {
        // Reconciliation interval (check every 60 seconds)
        let interval = Duration::from_secs(60);
        // Initial delay before first reconciliation (allow startup to complete)
        let startup_delay = Duration::from_secs(45);

        info!(
            "Reconciliation loop starting (startup delay: {:?}, interval: {:?})",
            startup_delay, interval
        );

        tokio::select! {
            _ = tokio::time::sleep(startup_delay) => {},
            _ = self.shutdown.notified() => {
                info!("Reconciliation loop shutdown during startup delay");
                return;
            }
        }

        let mut ticker = tokio::time::interval(interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    if self.loop_is_stale(generation) {
                        info!("Reconciliation loop retired (generation {generation})");
                        break;
                    }
                    if let Err(e) = self.reconcile_monitors().await {
                        error!("Monitor reconciliation failed: {}", e);
                    }
                }
                _ = self.shutdown.notified() => {
                    info!("Reconciliation loop received shutdown signal");
                    break;
                }
            }
        }

        info!("Reconciliation loop stopped");
    }

    /// Reconcile monitor daemon state with database.
    ///
    /// Compares what should be running (capturing monitors in DB) with what is
    /// actually running (tracked daemons) and corrects any discrepancies.
    async fn reconcile_monitors(&self) -> AppResult<()> {
        if self.is_shutting_down() {
            return Ok(());
        }

        let db = match &self.db {
            Some(db) => db.clone(),
            None => return Ok(()), // No DB, nothing to reconcile
        };

        debug!("Running monitor reconciliation");

        // Query monitors from database
        let monitor_list = monitors::Entity::find()
            .filter(monitors::Column::Deleted.eq(false))
            .all(db.as_ref())
            .await?;

        // What each monitor wants, folded per capture daemon: Local monitors
        // on one device share a single `zmc -d <device>`, which must run while
        // any of them captures — not be stopped and restarted every tick
        // because one of them is set to None (#113).
        let wants: Vec<(u32, String, bool)> = monitor_list
            .iter()
            .map(|m| {
                let should_run = !matches!(m.capturing, Capturing::None)
                    && !matches!(m.r#type, MonitorType::WebSite)
                    && self
                        .server_id
                        .is_none_or(|ours| m.server_id.is_none_or(|sid| sid == ours));
                (m.id, zmc_daemon_id(&m.r#type, &m.device, m.id), should_run)
            })
            .collect();
        let per_daemon = fold_should_run(&wants);

        let mut started = 0;
        let mut stopped = 0;

        for (monitor_id, zmc_id, _) in &wants {
            let monitor_id = *monitor_id;
            let should_run = per_daemon.get(zmc_id).copied().unwrap_or(false);

            // Only an absent or Stopped/Failed daemon is ours to start, and
            // only a Running/Starting one is ours to stop. Stopping and
            // Restarting entries belong to whoever put them there: starting
            // over a Stopping one orphans the old process (#77), and
            // respawning a Restarting one every tick caps the documented
            // backoff at this loop's interval (#115).
            let state = self.monitor_daemon_state(monitor_id).await;
            // Supervision gave up on this worker; only an explicit start retries.
            if self.monitor_daemon_gave_up(monitor_id).await {
                continue;
            }
            let idle = matches!(
                state,
                None | Some(ProcessState::Stopped) | Some(ProcessState::Failed)
            );
            let active = matches!(
                state,
                Some(ProcessState::Running) | Some(ProcessState::Starting)
            );

            if should_run && idle {
                // Monitor should be running but isn't - start it
                debug!(
                    "Reconciliation: starting monitor {} (capturing but not running)",
                    monitor_id
                );
                match self.start_monitor(monitor_id).await {
                    Ok(resp) if resp.success => {
                        started += 1;
                        info!("Reconciliation: started monitor {}", monitor_id);
                    }
                    Ok(resp) => {
                        warn!(
                            "Reconciliation: failed to start monitor {}: {}",
                            monitor_id, resp.message
                        );
                    }
                    Err(e) => {
                        error!(
                            "Reconciliation: error starting monitor {}: {}",
                            monitor_id, e
                        );
                    }
                }
            } else if !should_run && active {
                // Monitor shouldn't be running but is - stop it
                debug!(
                    "Reconciliation: stopping monitor {} (not capturing but running)",
                    monitor_id
                );
                match self.stop_monitor(monitor_id).await {
                    Ok(resp) if resp.success => {
                        stopped += 1;
                        info!("Reconciliation: stopped monitor {}", monitor_id);
                    }
                    Ok(resp) => {
                        warn!(
                            "Reconciliation: failed to stop monitor {}: {}",
                            monitor_id, resp.message
                        );
                    }
                    Err(e) => {
                        error!(
                            "Reconciliation: error stopping monitor {}: {}",
                            monitor_id, e
                        );
                    }
                }
            }
        }

        // Daemons for monitors that no longer exist. Nothing stopped them
        // before: a soft-deleted monitor kept capturing, and a hard-deleted
        // one left a zmc crash-looping on a missing row forever (#76).
        let live: std::collections::HashSet<u32> = monitor_list.iter().map(|m| m.id).collect();
        stopped += self.stop_daemons_for_absent_monitors(&live).await;

        if started > 0 || stopped > 0 {
            info!(
                "Reconciliation complete: started {} monitors, stopped {} monitors",
                started, stopped
            );
        } else {
            debug!("Reconciliation complete: no changes needed");
        }

        Ok(())
    }

    /// Perform a single health check cycle.
    async fn perform_health_check(&self, max_delay: std::time::Duration) {
        debug!("Performing health check");

        // Check for exited processes and handle restarts
        self.check_daemons().await;

        // Check for processes that need SIGKILL after graceful shutdown timeout
        // (like zmdc.pl's check_for_processes_to_kill)
        self.check_terminating_processes().await;

        // Check for hung processes (the zmwatch.pl half of this loop; exit
        // detection and crash restarts above are never optional).
        let hung_processes = self.hung_candidates(max_delay).await;

        for id in hung_processes {
            warn!(
                "Process {} appears hung (no heartbeat or CPU activity for {:?}), restarting",
                id, max_delay
            );
            // Pass empty args - restart_daemon will get args from existing process entry
            if let Err(e) = self.restart_daemon(&id, &[]).await {
                error!("Failed to restart hung daemon {}: {}", id, e);
            }
        }
    }

    /// Hung processes to restart this tick — none when the watchdog is
    /// switched off (`[daemon].enable_watchdog`, #88).
    async fn hung_candidates(&self, max_delay: std::time::Duration) -> Vec<String> {
        if !self.config.enable_watchdog {
            return Vec::new();
        }
        self.check_for_hung_processes(max_delay).await
    }

    /// Check for processes that appear to be hung: a `zmc` whose capture
    /// heartbeat has gone stale, or any daemon whose CPU time has stopped
    /// advancing when no heartbeat can be read for it.
    async fn check_for_hung_processes(&self, max_delay: std::time::Duration) -> Vec<String> {
        let mut hung = Vec::new();
        let shm_dir = self.shm_dir();
        let mut processes = self.processes.write().await;

        for (id, process) in processes.iter_mut() {
            // Update activity tracking: the shared-memory heartbeat where
            // there is one, CPU time otherwise.
            let heartbeat = capture_heartbeat_age(process, &shm_dir, max_delay);
            process.set_heartbeat_age(heartbeat);
            if heartbeat.is_none() {
                process.check_activity();
            }

            // The zm-infer daemon sleeps on GPU waits (blocking-sync) when idle, so
            // its CPU time barely advances and appears_hung would false-positive.
            // Its liveness is covered by an app-level probe, not the CPU heuristic.
            if id.starts_with("zm-infer") {
                continue;
            }

            // Check if process appears hung
            if process.appears_hung(max_delay) {
                hung.push(id.clone());
            }
        }

        hung
    }

    /// Check daemon health and restart if needed.
    pub async fn check_daemons(&self) {
        let mut to_restart = Vec::new();
        let mut pids_to_remove = Vec::new();

        {
            let mut processes = self.processes.write().await;

            for (id, process) in processes.iter_mut() {
                // Poll for exit, then drop the child borrow before mutating the
                // process below. `try_wait()` is non-consuming and, once a
                // child has exited, tokio caches the status and returns it on
                // every subsequent call — so we MUST take the handle when we
                // first observe the exit, or each cycle would re-detect the
                // same dead child, re-run prepare_restart (resetting the
                // backoff clock) and never actually restart it.
                let uptime = process.uptime().unwrap_or_default();
                let wait_result = process.child_mut().map(|child| child.try_wait());

                match wait_result {
                    Some(Ok(Some(status))) => {
                        // Process exited — drop the dead handle so try_wait()
                        // can't re-fire on the cached status next cycle.
                        let _ = process.take_child();

                        let was_stopping = process.state == ProcessState::Stopping;

                        if was_stopping {
                            info!("Daemon {} gracefully stopped with status: {:?}", id, status);
                        } else {
                            info!("Daemon {} exited with status: {:?}", id, status);
                        }

                        // Collect PID for removal (will remove after releasing lock)
                        if let Some(pid) = process.pid {
                            pids_to_remove.push(pid);
                        }
                        // The pid is dead and may be reused; a later stop/kill
                        // of this Restarting entry must not signal it (#81).
                        process.pid = None;

                        // Clear term_sent_at since process has exited
                        process.term_sent_at = None;

                        // If the process was being stopped (Stopping state) or
                        // auto_restart is disabled, just mark it stopped
                        // zm-next workers: give up on a status a restart can't
                        // fix instead of restart-looping it forever.
                        let decision = if id.starts_with("zm-core ") {
                            process.exit_history.record(status.code(), uptime)
                        } else {
                            crate::daemon::exit_policy::Decision::Restart
                        };

                        if was_stopping || !process.auto_restart {
                            process.set_state(ProcessState::Stopped);
                        } else if let crate::daemon::exit_policy::Decision::GiveUp(reason) =
                            decision
                        {
                            error!("Daemon {id}: {reason}; start it again once the cause is fixed");
                            process.gave_up = Some(reason);
                            process.set_state(ProcessState::Failed);
                        } else {
                            // Prepare for restart with backoff
                            process.prepare_restart(
                                self.config.min_backoff(),
                                self.config.max_backoff(),
                            );
                            debug!(
                                "Daemon {} will restart in {:?}",
                                id, process.current_backoff
                            );
                        }
                    }
                    Some(Ok(None)) => {
                        // Still running — once it has outlived the max backoff
                        // delay it counts as stable, so forgive its crash
                        // history and let a future restart start fresh instead
                        // of inheriting an old escalation.
                        process.reset_backoff_if_stable(
                            self.config.max_backoff(),
                            self.config.min_backoff(),
                        );
                    }
                    Some(Err(e)) => {
                        warn!("Error checking daemon {} status: {}", id, e);
                    }
                    None => {}
                }

                // Check for pending restarts. Pass only the "extras" — the
                // id-parts will be reconstructed by start_daemon, so passing
                // process.args directly would double them up.
                if process.state == ProcessState::Restarting && process.backoff_elapsed() {
                    let extras = extra_args_from_id(id, &process.args);
                    to_restart.push((id.clone(), extras));
                }
            }
        }

        // Remove PIDs from map (now that processes lock is released)
        if !pids_to_remove.is_empty() {
            let mut pid_map = self.pid_map.write().await;
            for pid in pids_to_remove {
                pid_map.remove(&pid);
            }
        }

        // Restart pending daemons
        for (id, args) in to_restart {
            info!("Restarting daemon {} after backoff", id);
            if let Err(e) = self.start_daemon(&id, &args).await {
                error!("Failed to restart {}: {}", id, e);
            }
        }
    }

    /// Register a daemon without starting it.
    pub async fn register_daemon(&self, process: ManagedProcess) {
        let mut processes = self.processes.write().await;
        processes.insert(process.id.clone(), process);
    }

    /// Get list of all daemon IDs.
    pub async fn list_daemon_ids(&self) -> Vec<String> {
        let processes = self.processes.read().await;
        processes.keys().cloned().collect()
    }

    // =========================================================================
    // Monitor-specific daemon control
    // =========================================================================

    /// Start all daemons for a specific monitor.
    ///
    /// This starts the appropriate daemons based on the monitor's configuration:
    /// - `zmc -m {id}`: Capture daemon (always started; analysis runs in it)
    ///
    /// # Arguments
    ///
    /// * `monitor_id` - The monitor ID to start daemons for
    ///
    /// # Returns
    ///
    /// A response indicating success/failure and details about what was started.
    /// Resolve the `zmc` daemon id for a monitor by looking up its type/device,
    /// so stop/status derive the same key `start` used. Falls back to the `-m`
    /// form when no DB is configured or the monitor row is missing.
    async fn zmc_id_for_monitor(&self, monitor_id: u32) -> String {
        if let Some(db) = &self.db {
            if let Ok(Some(m)) = monitors::Entity::find_by_id(monitor_id)
                .one(db.as_ref())
                .await
            {
                return zmc_daemon_id(&m.r#type, &m.device, monitor_id);
            }
        }
        format!("zmc -m {}", monitor_id)
    }

    /// Whether this monitor should run on a zm-next worker instead of legacy
    /// zmc/zma: requires zm-next to be enabled in config AND the per-monitor
    /// `UseZmNext` flag to be set. The flag read is isolated so the column's
    /// absence (the ZoneMinder fork has not added it yet) degrades to `false`.
    async fn use_zmnext(&self, monitor_id: u32) -> bool {
        if self.zmnext.is_none() {
            return false;
        }
        match &self.db {
            Some(db) => crate::repo::monitors::use_zmnext(db.as_ref(), monitor_id).await,
            None => false,
        }
    }

    /// Generate the monitor's pipeline JSON and (re)start its `zm-core` worker.
    /// The worker is keyed by the stable id `zm-core --monitor-id N`; the
    /// generated `--pipeline`/`--socket` paths ride as extra args so stop and
    /// restart can re-derive the id without recomputing them.
    /// Ensure the per-GPU `zm-infer` daemon is running (idempotent). Called before
    /// spawning any zm-next worker so the worker can reach it on its first detected
    /// frame. No-op unless `[zmnext.share_inference].enabled`. The daemon owns the
    /// only CUDA context + ORT session on its GPU; workers connect over its UDS.
    async fn ensure_infer_daemon(&self, rt: &ZmNextRuntime) -> AppResult<()> {
        let si = &rt.config.share_inference;
        if !si.enabled {
            return Ok(());
        }
        let id = zminfer_daemon_id(si.gpu_id);
        if self
            .processes
            .read()
            .await
            .get(&id)
            .map(|p| p.is_running())
            .unwrap_or(false)
        {
            return Ok(());
        }
        let socket = si.socket_name.replace("{gpu}", &si.gpu_id.to_string());
        let socket_path = format!("{}/{}", rt.socks_path.trim_end_matches('/'), socket);
        let extra = vec!["--socket".to_string(), socket_path];
        // No stdin payload: the daemon takes its config from argv/env.
        let _ = self.start_daemon_with_stdin(&id, &extra, None).await?;
        Ok(())
    }

    async fn start_zmnext_worker(&self, monitor: &monitors::Model) -> AppResult<DaemonResponse> {
        let Some(rt) = self.zmnext.clone() else {
            return Ok(DaemonResponse::error("zm-next is not configured"));
        };
        // Shared inference: make sure the per-GPU daemon is up before the worker,
        // which connects to it on its first detected frame.
        self.ensure_infer_daemon(&rt).await?;
        let monitor_id = monitor.id;

        let payload = std::sync::Arc::new(self.build_zmnext_pipeline_payload(monitor, &rt).await?);
        let socket_path = format!(
            "{}/stream_{}.sock",
            rt.socks_path.trim_end_matches('/'),
            monitor_id
        );
        let id = zmnext_daemon_id(monitor_id);
        // Deliver the pipeline config in-memory over stdin ("--pipeline -") so
        // the worker's camera credentials never touch disk.
        let mut extra = vec![
            "--pipeline".to_string(),
            "-".to_string(),
            "--socket".to_string(),
            socket_path,
        ];
        // Name zm-api as the worker's control peer, when this zm-core knows the
        // flag. An older one exits 1 on an unknown flag, so ask first.
        let binary = self.zmcore_binary(&rt);
        if zmcore_supports_control_uid(&binary).await {
            extra.push("--control-uid".to_string());
            extra.push(nix::unistd::geteuid().as_raw().to_string());
        }
        self.start_daemon_with_stdin(&id, &extra, Some(payload))
            .await
    }

    /// (Re)generate the monitor's pipeline JSON from its current Monitors/Zones
    /// rows and serialize it for in-memory delivery to the worker over stdin.
    /// Credentials are split out of the URL into separate fields so they stay
    /// out of any logged/persisted URL. This is the unit of "reload" for
    /// zm-next — the worker has no live config reload, so a restart re-reads the
    /// regenerated payload from stdin.
    async fn build_zmnext_pipeline_payload(
        &self,
        monitor: &monitors::Model,
        rt: &ZmNextRuntime,
    ) -> AppResult<Vec<u8>> {
        let (value, _) = self.build_zmnext_pipeline(monitor, rt, false).await?;
        serde_json::to_vec(&value).map_err(|e| {
            crate::error::AppError::InternalServerError(format!(
                "Failed to serialize zm-next pipeline for monitor {}: {e}",
                monitor.id
            ))
        })
    }

    /// The monitor's pipeline and its secrets, in the form a worker that
    /// speaks the control protocol takes in `configure`: every secret, camera
    /// credentials included, as a `$secret` reference with the values in the
    /// map. `None` when zm-next isn't enabled or the monitor doesn't exist.
    pub async fn zmnext_configure_pipeline(
        &self,
        monitor_id: u32,
    ) -> AppResult<
        Option<(
            serde_json::Value,
            crate::service::zmnext::secrets::SecretMap,
        )>,
    > {
        let (Some(rt), Some(db)) = (self.zmnext.clone(), self.db.clone()) else {
            return Ok(None);
        };
        let Some(monitor) = monitors::Entity::find_by_id(monitor_id)
            .one(db.as_ref())
            .await?
        else {
            return Ok(None);
        };
        self.build_zmnext_pipeline(&monitor, &rt, true)
            .await
            .map(Some)
    }

    /// Build the monitor's pipeline. With `configure_form` false, secrets are
    /// resolved to their values in place (for a worker that reads its pipeline
    /// from stdin) and the map is empty; with it true they stay references
    /// and the map carries the values.
    async fn build_zmnext_pipeline(
        &self,
        monitor: &monitors::Model,
        rt: &ZmNextRuntime,
        configure_form: bool,
    ) -> AppResult<(
        serde_json::Value,
        crate::service::zmnext::secrets::SecretMap,
    )> {
        let monitor_id = monitor.id;
        let mut graph_secrets = crate::service::zmnext::secrets::SecretMap::new();
        let zones = match &self.db {
            Some(db) => {
                zones::Entity::find()
                    .filter(zones::Column::MonitorId.eq(monitor_id))
                    .all(db.as_ref())
                    .await?
            }
            None => Vec::new(),
        };
        let zone_specs = pipeline::zone_specs_from_models(&zones);
        let events_root = self.zmnext_events_root(monitor).await;
        let mode = pipeline::StoreMode::from_function(&monitor.function);

        // Strip any credentials embedded in Path; prefer the dedicated User/Pass
        // columns when set, else fall back to the userinfo parsed from Path.
        let raw_path = monitor.path.clone().unwrap_or_default();
        let (clean_url, embedded) = pipeline::split_url_credentials(&raw_path);
        let username = monitor
            .user
            .clone()
            .filter(|s| !s.is_empty())
            .or_else(|| embedded.as_ref().map(|(u, _)| u.clone()))
            .unwrap_or_default();
        let password = monitor
            .pass
            .clone()
            .filter(|s| !s.is_empty())
            .or_else(|| embedded.as_ref().map(|(_, p)| p.clone()))
            .unwrap_or_default();

        let synopsis = rt.synopsis_monitors.contains(&monitor_id);
        // The default generator (used when the monitor has no stored processing
        // graph, or a stored graph that fails to parse/validate).
        let default = || {
            pipeline::generate_pipeline(
                monitor_id,
                &clean_url,
                &username,
                &password,
                &zone_specs,
                &rt.config.pipeline,
                mode,
                &events_root,
                synopsis,
            )
        };

        // If the monitor has a stored processing graph (the free graph), compose
        // it with the monitor-derived capture + store nodes; otherwise fall back
        // to the default generated pipeline.
        let stored = match &self.db {
            Some(db) => crate::repo::monitor_pipeline::find_by_monitor(db.as_ref(), monitor_id)
                .await
                .ok()
                .flatten(),
            None => None,
        };
        let mut value = match stored {
            Some(row) => match serde_json::from_str::<serde_json::Value>(&row.graph_json) {
                Ok(graph) if crate::service::zmnext::graph::validate_graph(&graph).is_ok() => {
                    // A worker that reads a plain pipeline gets the values in
                    // place; a configure keeps the references.
                    match self
                        .graph_secrets(monitor_id, rt, graph, !configure_form)
                        .await
                    {
                        Ok((graph, map)) => {
                            graph_secrets = map;
                            pipeline::compose_pipeline(
                                monitor_id,
                                &clean_url,
                                &username,
                                &password,
                                &graph,
                                &zone_specs,
                                &rt.config.pipeline,
                                mode,
                                &events_root,
                            )
                            .unwrap_or_else(default)
                        }
                        Err(e) => {
                            error!(
                                "zm-next: monitor {monitor_id} stored pipeline graph needs \
                                 secrets that can't be loaded ({e}); using the default \
                                 generated pipeline"
                            );
                            default()
                        }
                    }
                }
                Ok(_) => {
                    warn!(
                        "zm-next: monitor {monitor_id} stored pipeline graph failed validation; \
                         using the default generated pipeline"
                    );
                    default()
                }
                Err(e) => {
                    warn!(
                        "zm-next: monitor {monitor_id} stored pipeline graph is not valid JSON \
                         ({e}); using the default generated pipeline"
                    );
                    default()
                }
            },
            None => default(),
        };

        // The synopsis stages need the decoded frame size and a plate directory,
        // which only the monitor row knows (#128).
        pipeline::fill_monitor_frame(
            &mut value,
            u32::from(monitor.width),
            u32::from(monitor.height),
            &events_root.join("plates").join(monitor_id.to_string()),
        );

        // Shared inference (Phase 1): when enabled, route detection to the per-GPU
        // zm-infer daemon by injecting `infer_endpoint`/`gpu_id` into each detect
        // node's cfg. Injected here (after graph validation, never persisted); off
        // by default. The daemon itself is supervised separately.
        let si = &rt.config.share_inference;
        if si.enabled {
            let socket = si.socket_name.replace("{gpu}", &si.gpu_id.to_string());
            let endpoint = format!("{}/{}", rt.socks_path.trim_end_matches('/'), socket);
            pipeline::inject_shared_inference(&mut value, &endpoint, si.gpu_id);
        }

        if configure_form {
            let camera = crate::service::zmnext::control::split_camera_credentials(&mut value);
            graph_secrets.extend(camera);
        }
        Ok((value, graph_secrets))
    }

    /// Load the secrets a stored graph references and, with `resolve`, put
    /// their values in place of the references.
    async fn graph_secrets(
        &self,
        monitor_id: u32,
        rt: &ZmNextRuntime,
        mut graph: serde_json::Value,
        resolve: bool,
    ) -> AppResult<(
        serde_json::Value,
        crate::service::zmnext::secrets::SecretMap,
    )> {
        use crate::service::zmnext::secrets;
        if secrets::references(&graph).is_empty() {
            return Ok((graph, secrets::SecretMap::new()));
        }
        let db = self.db.as_ref().ok_or_else(|| {
            crate::error::AppError::InternalServerError("no database for secrets".into())
        })?;
        let map =
            secrets::load_secrets(db.as_ref(), &rt.config.secrets.key_file, monitor_id, &graph)
                .await?;
        if resolve {
            secrets::resolve_in_place(&mut graph, &map)
                .map_err(crate::error::AppError::InternalServerError)?;
        } else if let Some(missing) = secrets::references(&graph)
            .into_iter()
            .find(|n| !map.contains_key(n))
        {
            return Err(crate::error::AppError::InternalServerError(format!(
                "secret `{missing}` is referenced but not stored"
            )));
        }
        Ok((graph, map))
    }

    /// Resolve the directory the worker's `store` plugin writes clips to:
    /// the monitor's storage path, else the default (lowest-id) storage, else a
    /// conventional fallback.
    async fn zmnext_events_root(&self, monitor: &monitors::Model) -> PathBuf {
        if let Some(db) = &self.db {
            let by_id = match monitor.storage_id {
                Some(sid) if sid != 0 => storage::Entity::find_by_id(sid)
                    .one(db.as_ref())
                    .await
                    .ok()
                    .flatten(),
                _ => None,
            };
            let resolved = match by_id {
                Some(s) => Some(s),
                None => storage::Entity::find()
                    .order_by_asc(storage::Column::Id)
                    .one(db.as_ref())
                    .await
                    .ok()
                    .flatten(),
            };
            if let Some(s) = resolved {
                if !s.path.is_empty() {
                    return PathBuf::from(s.path);
                }
            }
        }
        PathBuf::from("/var/lib/zm/events")
    }

    pub async fn start_monitor(&self, monitor_id: u32) -> AppResult<DaemonResponse> {
        let db = match &self.db {
            Some(db) => db.clone(),
            None => {
                return Ok(DaemonResponse::error(
                    "Database not configured - cannot query monitor",
                ));
            }
        };

        // Query monitor to determine what daemons to start
        let monitor = monitors::Entity::find_by_id(monitor_id)
            .one(db.as_ref())
            .await?
            .ok_or_else(|| {
                crate::error::AppError::NotFoundError(crate::error::Resource {
                    resource_type: crate::error::ResourceType::Monitor,
                    details: vec![("id".to_string(), monitor_id.to_string())],
                })
            })?;

        // zm-next monitors run a single worker instead of zmc/zma. Tear down
        // any legacy daemons first so flipping the flag swaps cleanly.
        if self.use_zmnext(monitor_id).await {
            let _ = self
                .stop_daemon(&zmc_daemon_id(&monitor.r#type, &monitor.device, monitor_id))
                .await;
            let _ = self.stop_daemon(&format!("zma -m {}", monitor_id)).await;
            self.wait_for_monitor_stop(monitor_id).await;
            return self.start_zmnext_worker(&monitor).await;
        }

        let mut started = Vec::new();
        let mut errors = Vec::new();

        // Not a zm-next monitor: tear down any stale worker before starting the
        // legacy daemons (handles flipping the flag back off).
        if self.zmnext.is_some() {
            let _ = self.stop_daemon(&zmnext_daemon_id(monitor_id)).await;
            self.wait_for_stop(&zmnext_daemon_id(monitor_id)).await;
        }

        // Start zmc (capture daemon). Use the shared id derivation so Local
        // monitors get the same `-d <device>` key that stop/status expect.
        let zmc_id = zmc_daemon_id(&monitor.r#type, &monitor.device, monitor_id);
        match self.start_daemon(&zmc_id, &[]).await {
            Ok(resp) if resp.success => {
                started.push("zmc".to_string());
                info!("Started zmc for monitor {}", monitor_id);
            }
            Ok(resp) => {
                if !resp.message.contains("already running") {
                    errors.push(format!("zmc: {}", resp.message));
                }
            }
            Err(e) => {
                errors.push(format!("zmc: {}", e));
            }
        }

        // No zma: analysis runs inside zmc (#160).

        if errors.is_empty() {
            Ok(DaemonResponse::ok(format!(
                "Started monitor {}: {}",
                monitor_id,
                started.join(", ")
            )))
        } else if !started.is_empty() {
            Ok(DaemonResponse::ok(format!(
                "Partially started monitor {}: started [{}], errors [{}]",
                monitor_id,
                started.join(", "),
                errors.join("; ")
            )))
        } else {
            Ok(DaemonResponse::error(format!(
                "Failed to start monitor {}: {}",
                monitor_id,
                errors.join("; ")
            )))
        }
    }

    /// Stop all daemons for a specific monitor.
    ///
    /// This stops both zmc and zma daemons for the specified monitor.
    pub async fn stop_monitor(&self, monitor_id: u32) -> AppResult<DaemonResponse> {
        let mut stopped = Vec::new();
        let mut errors = Vec::new();

        // Stop zmc — resolve the same id form `start` used (Local monitors are
        // keyed by `-d <device>`).
        let zmc_id = self.zmc_id_for_monitor(monitor_id).await;
        match self.stop_daemon(&zmc_id).await {
            Ok(resp) if resp.success => {
                stopped.push("zmc".to_string());
                info!("Stopped zmc for monitor {}", monitor_id);
            }
            Ok(resp) => {
                if !resp.message.contains("not found") && !resp.message.contains("not running") {
                    errors.push(format!("zmc: {}", resp.message));
                }
            }
            Err(e) => {
                errors.push(format!("zmc: {}", e));
            }
        }

        // Stop zma
        let zma_id = format!("zma -m {}", monitor_id);
        match self.stop_daemon(&zma_id).await {
            Ok(resp) if resp.success => {
                stopped.push("zma".to_string());
                info!("Stopped zma for monitor {}", monitor_id);
            }
            Ok(resp) => {
                // zma might not be running if monitor doesn't need analysis
                if !resp.message.contains("not found") && !resp.message.contains("not running") {
                    errors.push(format!("zma: {}", resp.message));
                }
            }
            Err(e) => {
                errors.push(format!("zma: {}", e));
            }
        }

        // Stop any zm-next worker (no-op when the monitor is on legacy capture).
        if self.zmnext.is_some() {
            match self.stop_daemon(&zmnext_daemon_id(monitor_id)).await {
                Ok(resp) if resp.success => {
                    stopped.push("zm-core".to_string());
                    info!("Stopped zm-next worker for monitor {}", monitor_id);
                }
                Ok(resp) => {
                    if !resp.message.contains("not found") && !resp.message.contains("not running")
                    {
                        errors.push(format!("zm-core: {}", resp.message));
                    }
                }
                Err(e) => errors.push(format!("zm-core: {}", e)),
            }
        }

        if errors.is_empty() {
            Ok(DaemonResponse::ok(format!(
                "Stopped monitor {}: {}",
                monitor_id,
                if stopped.is_empty() {
                    "no daemons were running".to_string()
                } else {
                    stopped.join(", ")
                }
            )))
        } else {
            Ok(DaemonResponse::error(format!(
                "Errors stopping monitor {}: {}",
                monitor_id,
                errors.join("; ")
            )))
        }
    }

    /// Restart all daemons for a specific monitor.
    ///
    /// This stops then starts the monitor's daemons.
    pub async fn restart_monitor(&self, monitor_id: u32) -> AppResult<DaemonResponse> {
        info!("Restarting daemons for monitor {}", monitor_id);

        // Stop first
        let stop_result = self.stop_monitor(monitor_id).await?;
        if !stop_result.success {
            warn!(
                "Stop phase had issues for monitor {}: {}",
                monitor_id, stop_result.message
            );
        }

        // Wait for what was SIGTERM'd to actually exit. A fixed sleep let the
        // new spawn overwrite the entry while the old process was still
        // running, leaking it as an orphan (#77).
        self.wait_for_monitor_stop(monitor_id).await;

        // Start
        self.start_monitor(monitor_id).await
    }

    /// Wait for every daemon a monitor can own to leave `Stopping`.
    async fn wait_for_monitor_stop(&self, monitor_id: u32) {
        let zmc_id = self.zmc_id_for_monitor(monitor_id).await;
        for id in [
            zmc_id,
            format!("zma -m {}", monitor_id),
            zmnext_daemon_id(monitor_id),
        ] {
            self.wait_for_stop(&id).await;
        }
    }

    /// Check if a specific monitor's daemons are running.
    ///
    /// Returns true if the monitor's primary capture daemon is running — the
    /// `zm-core` worker for a zm-next monitor, otherwise `zmc`. Keying off the
    /// flag-appropriate daemon lets reconciliation swap them when the flag
    /// flips: the now-correct daemon reads as "not running", so `start_monitor`
    /// fires and tears down the other.
    pub async fn is_monitor_running(&self, monitor_id: u32) -> bool {
        self.monitor_daemon_state(monitor_id).await == Some(ProcessState::Running)
    }

    /// State of one tracked daemon by its id, `None` if untracked.
    pub async fn daemon_state(&self, id: &str) -> Option<ProcessState> {
        self.processes.read().await.get(id).map(|p| p.state)
    }

    /// Ask one daemon to reopen its log files: SIGWINCH, as zmdc.pl sends.
    /// SIGHUP is "reload" — zmc drops its camera and the Perl daemons exit for
    /// a restart — which is not what nightly logrotate means (#86). zm-next
    /// workers have no log-rotation signal and are left alone.
    pub async fn logrot_daemon(&self, id: &str) -> AppResult<DaemonResponse> {
        if zmnext_monitor_id_from_id(id).is_some() {
            return Ok(DaemonResponse::error(format!(
                "{id} is a zm-next worker; log rotation does not apply"
            )));
        }
        let pid = {
            let processes = self.processes.read().await;
            match processes.get(id) {
                Some(p) if p.is_running() => p.pid,
                Some(_) => return Ok(DaemonResponse::error(format!("Daemon {id} is not running"))),
                None => return Ok(DaemonResponse::error(format!("Daemon {id} not found"))),
            }
        };
        let Some(pid) = pid else {
            return Ok(DaemonResponse::error(format!("Daemon {id} has no PID")));
        };
        #[cfg(unix)]
        {
            use nix::sys::signal::{kill, Signal};
            use nix::unistd::Pid;
            match kill(Pid::from_raw(pid as i32), Signal::SIGWINCH) {
                Ok(()) => Ok(DaemonResponse::ok(format!(
                    "Sent SIGWINCH to {id} (PID {pid})"
                ))),
                Err(e) => Ok(DaemonResponse::error(format!(
                    "Failed to send SIGWINCH to {id}: {e}"
                ))),
            }
        }
        #[cfg(not(unix))]
        {
            Ok(DaemonResponse::error("log rotation signals are unix-only"))
        }
    }

    /// `logrot_daemon` for every tracked daemon; how many were signalled.
    pub async fn logrot_all(&self) -> usize {
        let mut count = 0;
        for id in self.list_daemon_ids().await {
            if let Ok(resp) = self.logrot_daemon(&id).await {
                if resp.success {
                    count += 1;
                }
            }
        }
        count
    }

    /// State of the monitor's primary capture daemon, `None` if untracked.
    /// Status of a monitor's zm-next worker entry, whether or not the monitor is
    /// currently flagged for zm-next.
    pub async fn zmnext_worker_status(&self, monitor_id: u32) -> Option<ProcessStatus> {
        let processes = self.processes.read().await;
        processes
            .get(&zmnext_daemon_id(monitor_id))
            .map(ProcessStatus::from)
    }

    /// Whether supervision gave up on this monitor's capture daemon.
    async fn monitor_daemon_gave_up(&self, monitor_id: u32) -> bool {
        let id = if self.use_zmnext(monitor_id).await {
            zmnext_daemon_id(monitor_id)
        } else {
            self.zmc_id_for_monitor(monitor_id).await
        };
        let processes = self.processes.read().await;
        processes.get(&id).is_some_and(|p| p.gave_up.is_some())
    }

    async fn monitor_daemon_state(&self, monitor_id: u32) -> Option<ProcessState> {
        let id = if self.use_zmnext(monitor_id).await {
            zmnext_daemon_id(monitor_id)
        } else {
            self.zmc_id_for_monitor(monitor_id).await
        };
        let processes = self.processes.read().await;
        processes.get(&id).map(|process| process.state)
    }

    /// Stop every tracked per-monitor daemon whose monitor is not in `live`.
    /// Returns how many were told to stop.
    pub async fn stop_daemons_for_absent_monitors(
        &self,
        live: &std::collections::HashSet<u32>,
    ) -> usize {
        let stale: Vec<String> = {
            let processes = self.processes.read().await;
            processes
                .iter()
                .filter(|(_, p)| p.monitor_id.is_some_and(|m| !live.contains(&m)))
                .filter(|(_, p)| p.can_stop())
                .map(|(id, _)| id.clone())
                .collect()
        };
        for id in &stale {
            info!("Stopping {id}: its monitor no longer exists");
            if let Err(e) = self.stop_daemon(id).await {
                warn!("Could not stop {id} for a removed monitor: {e}");
            }
        }
        stale.len()
    }

    /// Get status of a specific monitor's daemons.
    pub async fn get_monitor_daemon_status(&self, monitor_id: u32) -> Vec<ProcessStatus> {
        let zmc_id = self.zmc_id_for_monitor(monitor_id).await;
        let processes = self.processes.read().await;
        let mut statuses = Vec::new();

        // Check zmc
        if let Some(p) = processes.get(&zmc_id) {
            statuses.push(ProcessStatus::from(p));
        }

        // Check zma
        let zma_id = format!("zma -m {}", monitor_id);
        if let Some(p) = processes.get(&zma_id) {
            statuses.push(ProcessStatus::from(p));
        }

        statuses
    }
}

/// Check the database values a daemon will act on before starting it.
///
/// The API validates these fields on write, but rows can also arrive from
/// ZoneMinder's own UI or straight from SQL. ZoneMinder puts them where they
/// can change a command:
/// - `Monitors.Device` into `zmdc.pl start zmc -d '<Device>'` (shell);
/// - `Monitors.Path`/`SecondPath` into go2rtc, which runs `exec:` sources;
/// - `Controls.Protocol` into a Perl module load in `zmcontrol.pl`;
/// - `Storage.Path` into `df` and `rm -rf` (shell) in `zmfilter.pl`/`zmaudit.pl`.
///
/// `EventStartCommand`/`EventEndCommand` and `AutoExecuteCmd` are commands by
/// design; who may set them is enforced on write (System Edit).
async fn check_stored_inputs(
    db: &DatabaseConnection,
    command: &str,
    args: &[String],
    binary: &std::path::Path,
) -> Result<(), String> {
    let monitor_id = match (command, args) {
        ("zmc" | "zma" | "zmtrack.pl", [flag, id]) if flag == "-m" => id.parse::<u32>().ok(),
        ("zmcontrol.pl", [flag, id]) if flag == "--id" => id.parse::<u32>().ok(),
        _ => None,
    };
    let db_err = |e: crate::error::AppError| format!("could not read its settings: {e}");

    if let Some(mid) = monitor_id {
        let Some(monitor) = crate::repo::monitors::find_by_id(db, mid)
            .await
            .map_err(db_err)?
        else {
            return Ok(());
        };
        let protocol = if matches!(command, "zmcontrol.pl" | "zmtrack.pl") {
            match monitor.control_id {
                Some(cid) => crate::repo::controls::find_by_id(db, cid)
                    .await
                    .map_err(db_err)?
                    .map(|c| {
                        crate::ptz::capabilities::perl_module_name(c.protocol.as_deref(), &c.name)
                            .to_string()
                    }),
                None => None,
            }
        } else {
            None
        };
        return unsafe_monitor_inputs(&monitor, protocol.as_deref());
    }

    if command == "zmfilter.pl" && !zmfilter_escapes_tags(binary) {
        for filter in crate::repo::filters::find_all(db, None)
            .await
            .map_err(db_err)?
        {
            let cmd = filter.auto_execute_cmd.as_deref().unwrap_or("");
            if filter.auto_execute != 0 && uses_text_tags(cmd) {
                return Err(format!(
                    "filter {} puts event or monitor text into its AutoExecute command, and \
                     this zmfilter.pl passes that text to the shell unescaped. Upgrade \
                     ZoneMinder or use only %EID% and %MID% in the command",
                    filter.id
                ));
            }
        }
    }

    if matches!(command, "zmfilter.pl" | "zmaudit.pl") {
        for storage in crate::repo::storage::find_all(db).await.map_err(db_err)? {
            if crate::dto::request::monitor::is_safe_storage_path(&storage.path, &()).is_err() {
                return Err(format!(
                    "Storage {} has a Path a shell would not read literally",
                    storage.id
                ));
            }
        }
    }
    Ok(())
}

/// Every stored value that ZoneMinder could turn into a changed command line,
/// one line per finding. Reports only: nothing is changed. zm-api already
/// refuses to start the daemons that would use these rows, but ZoneMinder's
/// own tools can still run outside zm-api, so an operator needs to know.
fn unsafe_stored_values(
    monitors: &[crate::entity::monitors::Model],
    controls: &[crate::entity::controls::Model],
    storages: &[crate::entity::storage::Model],
    filters: &[crate::entity::filters::Model],
    zmfilter_escapes: bool,
) -> Vec<String> {
    let modules: std::collections::HashMap<u32, &str> = controls
        .iter()
        .map(|c| {
            (
                c.id,
                crate::ptz::capabilities::perl_module_name(c.protocol.as_deref(), &c.name),
            )
        })
        .collect();
    let mut out = Vec::new();
    for m in monitors {
        let module = m.control_id.and_then(|id| modules.get(&id).copied());
        if let Err(reason) = unsafe_monitor_inputs(m, module) {
            out.push(reason);
        }
    }
    for c in controls {
        let module = modules[&c.id];
        if crate::dto::request::monitor::is_perl_module_name(module, &()).is_err() {
            out.push(format!(
                "control {} loads {module:?}, which is not a module name",
                c.id
            ));
        }
    }
    for st in storages {
        if crate::dto::request::monitor::is_safe_storage_path(&st.path, &()).is_err() {
            out.push(format!(
                "Storage {} has a Path a shell would not read literally",
                st.id
            ));
        }
    }
    if !zmfilter_escapes {
        for f in filters {
            let cmd = f.auto_execute_cmd.as_deref().unwrap_or("");
            if f.auto_execute != 0 && uses_text_tags(cmd) {
                out.push(format!(
                    "filter {} puts event or monitor text into its AutoExecute command, \
                     which this ZoneMinder passes to the shell unescaped",
                    f.id
                ));
            }
        }
    }
    out
}

/// Load the rows [`unsafe_stored_values`] inspects and report its findings.
pub async fn find_unsafe_stored_values(
    db: &DatabaseConnection,
    zmfilter: &std::path::Path,
) -> crate::error::AppResult<Vec<String>> {
    let monitors = crate::repo::monitors::find_all(db, None).await?;
    let controls = crate::repo::controls::find_all(db).await?;
    let storages = crate::repo::storage::find_all(db).await?;
    let filters = crate::repo::filters::find_all(db, None).await?;
    Ok(unsafe_stored_values(
        &monitors,
        &controls,
        &storages,
        &filters,
        zmfilter_escapes_tags(zmfilter),
    ))
}

impl DaemonManager {
    /// Log [`find_unsafe_stored_values`] at startup and then hourly, so rows
    /// written through ZoneMinder's UI or straight into MySQL get noticed.
    pub fn spawn_unsafe_value_report(self: &Arc<Self>) {
        let Some(db) = self.db.clone() else {
            return;
        };
        let zmfilter = self.config.resolve_daemon_path("zmfilter.pl");
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(3600));
            loop {
                tick.tick().await;
                match find_unsafe_stored_values(&db, &zmfilter).await {
                    Ok(findings) => {
                        for f in findings {
                            error!("Unsafe stored value (reported, not changed): {f}");
                        }
                    }
                    Err(e) => warn!("Could not check stored values for command safety: {e}"),
                }
            }
        });
    }
}

/// Whether the installed `zmfilter.pl` keeps tag values out of the shell's
/// parsing. Upstream's fix (commit 2cd086392) passes them in `ZM_TAG_n`
/// environment variables; earlier versions paste them into the command line.
fn zmfilter_escapes_tags(binary: &std::path::Path) -> bool {
    std::fs::read_to_string(binary).is_ok_and(|src| src.contains("ZM_TAG_"))
}

/// Whether an AutoExecute command uses a `%TAG%` other than the numeric event
/// and monitor ids. The rest (names, cause, notes, tags, paths) carry text a
/// non-admin user can set.
fn uses_text_tags(cmd: &str) -> bool {
    let mut rest = cmd;
    while let Some(start) = rest.find('%') {
        let after = &rest[start + 1..];
        let Some(end) = after.find('%') else {
            return false;
        };
        let tag = &after[..end];
        if !tag.is_empty()
            && tag.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
            && !matches!(tag, "EID" | "MID")
        {
            return true;
        }
        rest = &after[end..];
    }
    false
}

/// The first stored monitor value that could change a command, if any.
fn unsafe_monitor_inputs(
    monitor: &crate::entity::monitors::Model,
    protocol: Option<&str>,
) -> Result<(), String> {
    use crate::dto::request::monitor as v;
    let id = monitor.id;
    if v::is_safe_device_path(&monitor.device, &()).is_err() {
        return Err(format!("monitor {id} has an unsafe Device"));
    }
    for (name, value) in [
        ("Path", &monitor.path),
        ("SecondPath", &monitor.second_path),
    ] {
        if let Some(value) = value {
            if v::is_safe_capture_path(value, &()).is_err() {
                return Err(format!("monitor {id} has an unsafe {name}"));
            }
        }
    }
    if let Some(p) = protocol {
        if v::is_perl_module_name(p, &()).is_err() {
            return Err(format!(
                "monitor {id}'s control Protocol is not a module name"
            ));
        }
    }
    Ok(())
}

/// Whitelist validator for daemon spawn requests.
///
/// `start_daemon` (and therefore every restart path) routes through this
/// before exec. The HTTP `/api/v3/daemons/{id}/...` endpoints and the
/// `/run/zm/zmdc.sock` IPC handler both forward untrusted strings into
/// `start_daemon`; without this check, `PathBuf::join` with an absolute path
/// (e.g. `"/bin/bash"`) would replace the daemon base and run anything.
///
/// Mirrors `zmdc.pl`'s hardcoded daemon-name regex (line 131-138) plus
/// `zmpkg.pl`'s `^/dev/[\w/.\-]+$` device path check (line 214).
pub(crate) fn validate_daemon_spec(command: &str, args: &[String]) -> Result<(), String> {
    use regex::Regex;
    use std::sync::OnceLock;

    static DEVICE_RE: OnceLock<Regex> = OnceLock::new();
    let device_re = DEVICE_RE.get_or_init(|| Regex::new(r"^/dev/[\w/.\-]+$").unwrap());
    static FILTER_ID_RE: OnceLock<Regex> = OnceLock::new();
    let filter_id_re = FILTER_ID_RE.get_or_init(|| Regex::new(r"^--filter_id=\d+$").unwrap());

    let int_arg = |s: &String| s.parse::<u32>().is_ok();

    let result: Result<(), String> = match command {
        // Singletons that take no arguments.
        "zmstats.pl"
        | "zmtelemetry.pl"
        | "zmtrigger.pl"
        | "zmeventnotification.pl"
        | "zm_rtsp_server" => {
            if args.is_empty() {
                Ok(())
            } else {
                Err(format!("{} takes no arguments", command))
            }
        }
        // zmfilter: global `--daemon`, or per-filter `--filter_id=N --daemon`.
        "zmfilter.pl" => match args {
            [a] if a == "--daemon" => Ok(()),
            [a, b] if filter_id_re.is_match(a) && b == "--daemon" => Ok(()),
            _ => Err(format!("zmfilter.pl args invalid: {:?}", args)),
        },
        // zmaudit: -c / --continuous (upstream's two spellings of the same flag).
        "zmaudit.pl" => match args {
            [a] if a == "-c" || a == "--continuous" => Ok(()),
            _ => Err(format!("zmaudit.pl args invalid: {:?}", args)),
        },
        // zmc: -m <monitor_id> or -d /dev/<safe-path>. The regex matches
        // upstream's `/^\/dev\/[\w\/.\-]+$/` but we additionally reject `..`
        // so the path can't traverse out of /dev.
        "zmc" => match args {
            [m, id] if m == "-m" && int_arg(id) => Ok(()),
            [d, path] if d == "-d" && device_re.is_match(path) && !path.contains("..") => Ok(()),
            _ => Err(format!("zmc args invalid: {:?}", args)),
        },
        // zma re-analyses one event: `-e <event_id>`, optionally `-m <id>`.
        "zma" => match args {
            [e, ev] if e == "-e" && int_arg(ev) => Ok(()),
            [e, ev, m, id] if e == "-e" && int_arg(ev) && m == "-m" && int_arg(id) => Ok(()),
            _ => Err(format!("zma args invalid: {:?}", args)),
        },
        "zmcontrol.pl" => match args {
            [k, id] if k == "--id" && int_arg(id) => Ok(()),
            _ => Err(format!("zmcontrol.pl args invalid: {:?}", args)),
        },
        "zmtrack.pl" => match args {
            [m, id] if m == "-m" && int_arg(id) => Ok(()),
            _ => Err(format!("zmtrack.pl args invalid: {:?}", args)),
        },
        // zm-next worker: `--monitor-id <int> --pipeline <abs path> --socket
        // <abs path>` in any order. zm-api generates these paths itself; the
        // validator still enforces them as absolute and traversal-free so a
        // forwarded IPC/HTTP request can't smuggle arbitrary args.
        "zm-core" => validate_zmnext_args(args),
        // zm-infer per-GPU inference daemon: `--gpu <int> --socket <abs path>`.
        "zm-infer" => validate_zminfer_args(args),
        _ => Err(format!("unknown daemon: {:?}", command)),
    };
    result
}

/// Whether the `zm-core` at `binary` accepts `--control-uid` (zm-next's worker
/// control protocol), from its `--help`. Cached per path and modification
/// time, so an upgraded binary is asked again.
async fn zmcore_supports_control_uid(binary: &std::path::Path) -> bool {
    use std::collections::HashMap;
    use std::sync::Mutex;
    use std::time::SystemTime;
    static CACHE: std::sync::OnceLock<Mutex<HashMap<PathBuf, (SystemTime, bool)>>> =
        std::sync::OnceLock::new();
    let Ok(mtime) = std::fs::metadata(binary).and_then(|m| m.modified()) else {
        return false;
    };
    let cache = CACHE.get_or_init(Default::default);
    if let Some((cached_mtime, supported)) = cache.lock().unwrap().get(binary) {
        if *cached_mtime == mtime {
            return *supported;
        }
    }
    let output = tokio::time::timeout(
        std::time::Duration::from_secs(3),
        Command::new(binary).arg("--help").output(),
    )
    .await;
    let supported = match output {
        Ok(Ok(out)) => {
            help_mentions_control_uid(&out.stdout) || help_mentions_control_uid(&out.stderr)
        }
        _ => false,
    };
    cache
        .lock()
        .unwrap()
        .insert(binary.to_path_buf(), (mtime, supported));
    supported
}

fn help_mentions_control_uid(text: &[u8]) -> bool {
    String::from_utf8_lossy(text).contains("--control-uid")
}

/// Validate the `zm-core` worker arg list (order-independent). Requires exactly
/// `--monitor-id`, `--pipeline` and `--socket`, each with a valid value, and
/// allows one `--control-uid <uid>`. The
/// pipeline value is either an absolute path or `-` (read the config from stdin,
/// which is how zm-api delivers it in-memory so credentials never touch disk).
fn validate_zmnext_args(args: &[String]) -> Result<(), String> {
    let safe_path = |p: &str| p.starts_with('/') && !p.contains("..");
    // Must be whole flag/value pairs. Checking the length up front lets us
    // iterate `chunks(2)` with every chunk guaranteed to have two elements
    // (avoids `chunks_exact`, which a newer clippy flags for `as_chunks`).
    if !args.len().is_multiple_of(2) {
        return Err(format!("zm-core args must be flag/value pairs: {:?}", args));
    }
    let (mut have_monitor, mut have_pipeline, mut have_socket) = (false, false, false);
    let mut have_control_uid = false;
    for pair in args.chunks(2) {
        match (pair[0].as_str(), pair[1].as_str()) {
            ("--control-uid", v) if !have_control_uid && v.parse::<u32>().is_ok() => {
                have_control_uid = true
            }
            ("--monitor-id", v) if v.parse::<u32>().is_ok() => have_monitor = true,
            ("--pipeline", v) if v == "-" || safe_path(v) => have_pipeline = true,
            ("--socket", v) if safe_path(v) => have_socket = true,
            _ => return Err(format!("zm-core args invalid: {:?}", args)),
        }
    }
    if have_monitor && have_pipeline && have_socket {
        Ok(())
    } else {
        Err(format!("zm-core missing required args: {:?}", args))
    }
}

/// Validate the `zm-infer` daemon arg list: `--gpu <int> --socket <abs path>`.
/// zm-api generates these itself; the validator still enforces the shape so a
/// forwarded IPC/HTTP request can't smuggle arbitrary args.
fn validate_zminfer_args(args: &[String]) -> Result<(), String> {
    let safe_path = |p: &str| p.starts_with('/') && !p.contains("..");
    match args {
        [g, gpu, s, sock]
            if g == "--gpu" && gpu.parse::<u32>().is_ok() && s == "--socket" && safe_path(sock) =>
        {
            Ok(())
        }
        _ => Err(format!("zm-infer args invalid: {:?}", args)),
    }
}

/// Compute the "extras" — args present on a `ManagedProcess` that are NOT
/// already encoded in the daemon id. `parse_daemon_command` will re-derive
/// the id-parts on the next spawn and concatenate, so we have to subtract
/// them on the way back in or they get duplicated each restart.
fn extra_args_from_id(id: &str, full_args: &[String]) -> Vec<String> {
    let id_arg_count = id.split_whitespace().count().saturating_sub(1);
    full_args.iter().skip(id_arg_count).cloned().collect()
}

/// Parse a daemon command string into command and args.
/// Whether `id` and `args` name a daemon zm-api will run, as `start_daemon`
/// decides it. For callers that answer before reaching the manager.
pub(crate) fn check_daemon_spec(id: &str, args: &[String]) -> Result<(), String> {
    let (command, daemon_args) = parse_daemon_command(id, args);
    validate_daemon_spec(&command, &daemon_args)
}

fn parse_daemon_command(id: &str, extra_args: &[String]) -> (String, Vec<String>) {
    let parts: Vec<&str> = id.split_whitespace().collect();
    if parts.is_empty() {
        return (id.to_string(), extra_args.to_vec());
    }

    let command = parts[0].to_string();
    let mut args: Vec<String> = parts[1..].iter().map(|s| s.to_string()).collect();
    args.extend(extra_args.iter().cloned());

    (command, args)
}

/// Build the `zmc` capture-daemon id for a monitor. This id doubles as the key
/// in the process map, so **every** call site (start / stop / restart / status)
/// must derive it identically — otherwise a daemon started under one form is
/// invisible to the others.
///
/// Local (V4L2) monitors with a device path are addressed as `zmc -d <device>`
/// to match how ZoneMinder launches local capture; everything else uses
/// `zmc -m <id>`. Previously `start_all_daemons` used the `-d` form while
/// stop/status hard-coded `-m`, so Local monitors were started, never found
/// again, and churn-restarted on every reconcile tick. See REVIEW_FIXES_PLAN §4.1.
///
/// A device containing whitespace falls back to the `-m` form: the id is later
/// re-split on whitespace by [`parse_daemon_command`], so a spaced path would
/// fan out into bogus args and be rejected by `validate_daemon_spec` anyway.
/// Whether each capture daemon should run, given what every monitor wants.
/// A daemon shared by several monitors (`zmc -d <device>`) runs if any of
/// them wants it.
fn fold_should_run(wants: &[(u32, String, bool)]) -> std::collections::HashMap<String, bool> {
    let mut per_daemon = std::collections::HashMap::new();
    for (_, daemon_id, should_run) in wants {
        let entry = per_daemon.entry(daemon_id.clone()).or_insert(false);
        *entry |= *should_run;
    }
    per_daemon
}

pub(crate) fn zmc_daemon_id(monitor_type: &MonitorType, device: &str, monitor_id: u32) -> String {
    if matches!(monitor_type, MonitorType::Local) && !device.is_empty() {
        if device.chars().any(char::is_whitespace) {
            warn!(
                "Monitor {} device path {:?} contains whitespace; using -m form \
                 (a spaced device would fail daemon-spec validation)",
                monitor_id, device
            );
        } else {
            return format!("zmc -d {}", device);
        }
    }
    format!("zmc -m {}", monitor_id)
}

/// Interpret a ZoneMinder Config `Value` as a boolean.
///
/// ZoneMinder stores booleans as text in the `Config` table, mostly as `"1"`/`"0"`
/// but human edits and older rows can produce `"yes"`/`"no"` or `"true"`/`"false"`.
fn parse_zm_bool(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

/// Read a `ZM_*` config key and interpret it as a boolean.
///
/// Returns false on missing key, parse failure, or DB error — matching upstream
/// Perl truthiness: `if ($Config{KEY})` treats undef/0/empty as false.
/// Age of the capture heartbeat for a supervised `zmc`, read from the
/// monitor's shared memory the way zmwatch.pl does (#123).
///
/// `None` means the signal does not apply and the caller falls back to CPU
/// time: the process is not a running `zmc`, it is still inside its startup
/// grace (`max_delay`, as zmwatch allows — the segment may not exist yet), or
/// the segment cannot be read. That last case is deliberately *not* treated
/// as hung, unlike zmwatch's "shared data not valid" restart: a wrong
/// `ZM_PATH_MAP` or a permissions problem would otherwise restart every
/// camera on every tick.
fn capture_heartbeat_age(
    process: &ManagedProcess,
    shm_dir: &Path,
    max_delay: Duration,
) -> Option<Duration> {
    let monitor_id = process.monitor_id?;
    if process.state != ProcessState::Running || !process.id.starts_with("zmc ") {
        return None;
    }
    let uptime = process.started_at?.elapsed();
    if uptime < max_delay {
        return None;
    }
    let shm = match MonitorShm::connect_with_path(
        monitor_id,
        &shm_dir.to_string_lossy(),
        DEFAULT_SHM_PREFIX,
    ) {
        Ok(shm) => shm,
        Err(e) => {
            debug!(
                "{}: shared memory unreadable ({e}); judging liveness by CPU time",
                process.id
            );
            return None;
        }
    };
    Some(heartbeat_age(
        shm.get_heartbeat_time(),
        std::time::SystemTime::now(),
        uptime,
    ))
}

/// How long since zmc last touched its heartbeat. A segment with no heartbeat
/// at all, past the startup grace, is as old as the process; a heartbeat in
/// the future (clock step) counts as now.
fn heartbeat_age(
    heartbeat: Option<std::time::SystemTime>,
    now: std::time::SystemTime,
    uptime: Duration,
) -> Duration {
    match heartbeat {
        Some(at) => now.duration_since(at).unwrap_or(Duration::ZERO),
        None => uptime,
    }
}

async fn read_bool_config(db: &DatabaseConnection, key: &str) -> bool {
    match crate::repo::config::get_config_value(db, key).await {
        Ok(Some(v)) => parse_zm_bool(&v),
        Ok(None) => false,
        Err(e) => {
            warn!("Could not read {}: {} (defaulting to off)", key, e);
            false
        }
    }
}

/// Which of ZoneMinder's Perl maintenance daemons are superseded by zm-api's
/// own `[maintenance.*]` jobs.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct NativeJobs {
    pub stats: bool,
    pub audit: bool,
    pub telemetry: bool,
}

impl From<&crate::configure::maintenance::MaintenanceConfig> for NativeJobs {
    fn from(m: &crate::configure::maintenance::MaintenanceConfig) -> Self {
        Self {
            stats: m.stats.enabled,
            audit: m.audit.enabled,
            telemetry: m.telemetry.enabled,
        }
    }
}

/// The config table whose native job replaces `daemon_name`, if that job is
/// enabled — the reason the supervisor must not start the Perl daemon.
fn native_replacement_for(daemon_name: &str, jobs: NativeJobs) -> Option<&'static str> {
    match daemon_name {
        "zmstats" if jobs.stats => Some("[maintenance.stats]"),
        "zmaudit" if jobs.audit => Some("[maintenance.audit]"),
        "zmtelemetry" if jobs.telemetry => Some("[maintenance.telemetry]"),
        _ => None,
    }
}

/// Snapshot of upstream-equivalent startup gates pulled from `Config` and
/// (when multi-server) the current `Servers` row.
///
/// `server_*` fields default to `true` when not in multi-server mode, so
/// single-server installs ignore the per-server columns entirely — matching
/// `zmpkg.pl`'s `if ($Server and exists $$Server{x} and !$$Server{x})` check.
struct StartupGates {
    zm_opt_control: bool,
    zm_opt_triggers: bool,
    zm_opt_use_eventnotification: bool,
    zm_telemetry_data: bool,
    zm_run_audit: bool,
    /// `ZM_MIN_RTSP_PORT` is set, so the RTSP restream server runs.
    zm_rtsp_server: bool,
    server_zmstats: bool,
    server_zmaudit: bool,
    server_zmtrigger: bool,
    server_zmeventnotification: bool,
}

/// Whether `ZM_MIN_RTSP_PORT` turns the restream server on: zmpkg.pl tests
/// the value for truth, so unset, empty and `0` are off.
fn rtsp_port_configured(value: Option<&str>) -> bool {
    match value.map(str::trim) {
        None | Some("") => false,
        Some(v) => v.parse::<u32>().map(|p| p != 0).unwrap_or(true),
    }
}

/// Extract monitor ID from daemon arguments.
fn extract_monitor_id(args: &[String]) -> Option<u32> {
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        // `-m N` (zmc/zma) or `--monitor-id N` (zm-core worker).
        if arg == "-m" || arg == "--monitor-id" {
            if let Some(id_str) = iter.next() {
                return id_str.parse().ok();
            }
        }
    }
    None
}

/// Stable process-map id for a monitor's zm-next worker. The generated
/// `--pipeline`/`--socket` paths ride as extra args, not in the id, so every
/// call site derives the same key without recomputing those paths.
fn zmnext_daemon_id(monitor_id: u32) -> String {
    format!("zm-core --monitor-id {monitor_id}")
}

/// Recover the monitor id from a `zm-core --monitor-id N` daemon id, or `None`
/// when `id` is not a zm-next worker id.
fn zmnext_monitor_id_from_id(id: &str) -> Option<u32> {
    let parts: Vec<&str> = id.split_whitespace().collect();
    match parts.as_slice() {
        ["zm-core", "--monitor-id", n] => n.parse().ok(),
        _ => None,
    }
}

/// Stable process-map id for the per-GPU `zm-infer` inference daemon. The
/// `--socket` path rides as an extra arg, not in the id, so every call site
/// derives the same key. One daemon per GPU.
fn zminfer_daemon_id(gpu: u32) -> String {
    format!("zm-infer --gpu {gpu}")
}

/// Spawn a daemon process with PR_SET_PDEATHSIG on Linux.
///
/// This ensures that child processes receive SIGTERM when the parent process dies,
/// preventing orphaned daemons when zm-api crashes or is killed.
#[cfg(target_os = "linux")]
fn spawn_daemon(
    path: &std::path::Path,
    args: &[String],
    with_stdin: bool,
) -> Result<tokio::process::Child, std::io::Error> {
    let mut cmd = Command::new(path);
    cmd.args(args);
    // Run from the binary's own directory. The zm-next worker dlopen's its
    // pipeline plugins via a relative `plugins/...` prefix, so its cwd must be
    // the install/build dir; for stock daemons (zmc in /usr/bin) this is inert.
    if let Some(dir) = path.parent() {
        if !dir.as_os_str().is_empty() {
            cmd.current_dir(dir);
        }
    }
    // Pipe stdin only when the caller will write a payload (the zm-next worker
    // reads its pipeline config from stdin); otherwise inherit as before.
    if with_stdin {
        cmd.stdin(std::process::Stdio::piped());
        // Shared-inference Phase 0: defer CUDA kernel-module loading so unused
        // kernels aren't faulted into the worker's RSS at startup. No-op/default
        // on CUDA >= 12.2; explicit here for older toolkits. zm-next worker only.
        cmd.env("CUDA_MODULE_LOADING", "LAZY");
    }

    // The per-GPU zm-infer daemon: pin it to its GPU via CUDA_VISIBLE_DEVICES so
    // its (device-0) ORT/CUDA code lands on the right card, and lazy-load CUDA
    // modules. The --gpu value is the visible-device index.
    if path.file_name().and_then(|n| n.to_str()) == Some("zm-infer") {
        if let Some(gpu) = args
            .iter()
            .position(|a| a == "--gpu")
            .and_then(|i| args.get(i + 1))
        {
            cmd.env("CUDA_VISIBLE_DEVICES", gpu);
        }
        cmd.env("CUDA_MODULE_LOADING", "LAZY");
    }

    // SAFETY: prctl is async-signal-safe and we're only setting PR_SET_PDEATHSIG
    // which is a simple flag operation with no memory allocation or locks.
    unsafe {
        cmd.pre_exec(|| {
            // PR_SET_PDEATHSIG causes the child to receive the specified signal
            // when its parent process terminates.
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }

    cmd.spawn()
}

/// Spawn a daemon process (non-Linux fallback).
#[cfg(not(target_os = "linux"))]
fn spawn_daemon(
    path: &std::path::Path,
    args: &[String],
    with_stdin: bool,
) -> Result<tokio::process::Child, std::io::Error> {
    let mut cmd = Command::new(path);
    cmd.args(args);
    // See the Linux variant: the worker resolves plugins relative to its cwd.
    if let Some(dir) = path.parent() {
        if !dir.as_os_str().is_empty() {
            cmd.current_dir(dir);
        }
    }
    if with_stdin {
        cmd.stdin(std::process::Stdio::piped());
        // See the Linux variant: defer CUDA module loading for the zm-next worker.
        cmd.env("CUDA_MODULE_LOADING", "LAZY");
    }
    // See the Linux variant: pin the zm-infer daemon to its GPU.
    if path.file_name().and_then(|n| n.to_str()) == Some("zm-infer") {
        if let Some(gpu) = args
            .iter()
            .position(|a| a == "--gpu")
            .and_then(|i| args.get(i + 1))
        {
            cmd.env("CUDA_VISIBLE_DEVICES", gpu);
        }
        cmd.env("CUDA_MODULE_LOADING", "LAZY");
    }
    cmd.spawn()
}

/// Kill orphaned ZoneMinder daemons that may be left over from a crash.
///
/// This should be called on startup before starting new daemons to ensure
/// a clean slate. Orphaned processes can cause shared memory conflicts
/// and resource contention.
pub async fn kill_orphan_daemons() {
    // Everything this supervisor spawns (#118): the ZoneMinder daemons it
    // knows plus the zm-next workers. Derived, so it cannot drift from the
    // spawn list the way a hand-written copy did.
    let daemon_names: Vec<&str> = DaemonDefinition::commands()
        .chain(["zm-core", "zm-infer"])
        .collect();

    for daemon in &daemon_names {
        // Match the program name as a whole word of the full command line:
        // `(^|/)name( |$)`. The default pkill pattern is an unanchored regex on
        // the 15-char comm, so "zma" matched zmaudit.pl and "zmc" matched
        // zmcontrol.pl, and names longer than 15 chars could never match.
        let pattern = format!("(^|/){}( |$)", regex::escape(daemon));
        match Command::new("pkill")
            .args(["-9", "-f", "--", &pattern])
            .output()
            .await
        {
            Ok(output) => {
                if output.status.success() {
                    info!("Killed orphaned {} processes", daemon);
                }
                // Exit code 1 means no processes matched - that's fine
            }
            Err(e) => {
                // pkill might not be available, try killall as fallback
                debug!("pkill failed for {}: {}, trying killall", daemon, e);
                if let Err(e2) = Command::new("killall")
                    .args(["-9", "--", daemon])
                    .output()
                    .await
                {
                    debug!("killall also failed for {}: {}", daemon, e2);
                }
            }
        }
    }

    // Brief pause to allow processes to terminate and release resources
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    info!("Orphan daemon cleanup complete");
}

#[cfg(test)]
mod stored_input_tests {
    use super::*;

    fn monitor(device: &str, path: Option<&str>) -> crate::entity::monitors::Model {
        crate::entity::monitors::Model {
            id: 7,
            device: device.into(),
            path: path.map(Into::into),
            ..crate::ptz::manager::tests::test_monitor()
        }
    }

    #[test]
    fn a_device_that_breaks_out_of_its_quotes_is_refused() {
        let m = monitor("/dev/video0'; touch /tmp/pwned; '", None);
        assert!(unsafe_monitor_inputs(&m, None).is_err());
    }

    #[test]
    fn a_go2rtc_exec_path_is_refused() {
        let m = monitor("", Some("exec:/bin/sh -c id"));
        assert!(unsafe_monitor_inputs(&m, None).is_err());
    }

    #[test]
    fn a_traversing_control_protocol_is_refused() {
        let m = monitor("", Some("rtsp://10.0.0.5/live"));
        assert!(unsafe_monitor_inputs(&m, Some("../../../../tmp/x")).is_err());
        assert!(unsafe_monitor_inputs(&m, Some("onvif")).is_ok());
    }

    #[test]
    fn text_tags_are_spotted_but_id_tags_are_not() {
        assert!(uses_text_tags("/usr/local/bin/notify '%EN%'"));
        assert!(uses_text_tags("notify %EID% %MN%"));
        assert!(uses_text_tags("notify %ETAGS%"));
        assert!(!uses_text_tags("notify %EID% %MID%"));
        assert!(!uses_text_tags("printf '50%' done"));
        assert!(!uses_text_tags(""));
    }

    #[test]
    fn an_old_zmfilter_is_detected_by_its_missing_tag_escaping() {
        let dir = tempfile::tempdir().unwrap();
        let old = dir.path().join("old.pl");
        std::fs::write(&old, "my $output = qx($command);\n").unwrap();
        let new = dir.path().join("new.pl");
        std::fs::write(&new, "$tag_env{'ZM_TAG_'.$n++} = $value;\n").unwrap();
        assert!(!zmfilter_escapes_tags(&old));
        assert!(zmfilter_escapes_tags(&new));
        assert!(!zmfilter_escapes_tags(&dir.path().join("missing.pl")));
    }

    fn control(id: u32, protocol: Option<&str>, name: &str) -> crate::entity::controls::Model {
        let mut c = crate::entity::controls::Model {
            id: 0,
            name: String::new(),
            protocol: None,
            can_wake: 0,
            can_sleep: 0,
            can_reset: 0,
            can_reboot: 0,
            can_zoom: 0,
            can_auto_zoom: 0,
            can_zoom_abs: 0,
            can_zoom_rel: 0,
            can_zoom_con: 0,
            min_zoom_range: None,
            max_zoom_range: None,
            min_zoom_step: None,
            max_zoom_step: None,
            has_zoom_speed: 0,
            min_zoom_speed: None,
            max_zoom_speed: None,
            can_focus: 0,
            can_auto_focus: 0,
            can_focus_abs: 0,
            can_focus_rel: 0,
            can_focus_con: 0,
            min_focus_range: None,
            max_focus_range: None,
            min_focus_step: None,
            max_focus_step: None,
            has_focus_speed: 0,
            min_focus_speed: None,
            max_focus_speed: None,
            can_iris: 0,
            can_auto_iris: 0,
            can_iris_abs: 0,
            can_iris_rel: 0,
            can_iris_con: 0,
            min_iris_range: None,
            max_iris_range: None,
            min_iris_step: None,
            max_iris_step: None,
            has_iris_speed: 0,
            min_iris_speed: None,
            max_iris_speed: None,
            can_gain: 0,
            can_auto_gain: 0,
            can_gain_abs: 0,
            can_gain_rel: 0,
            can_gain_con: 0,
            min_gain_range: None,
            max_gain_range: None,
            min_gain_step: None,
            max_gain_step: None,
            has_gain_speed: 0,
            min_gain_speed: None,
            max_gain_speed: None,
            can_white: 0,
            can_auto_white: 0,
            can_white_abs: 0,
            can_white_rel: 0,
            can_white_con: 0,
            min_white_range: None,
            max_white_range: None,
            min_white_step: None,
            max_white_step: None,
            has_white_speed: 0,
            min_white_speed: None,
            max_white_speed: None,
            has_presets: 0,
            num_presets: 0,
            has_home_preset: 0,
            can_set_presets: 0,
            can_move: 0,
            can_move_diag: 0,
            can_move_map: 0,
            can_move_abs: 0,
            can_move_rel: 0,
            can_move_con: 0,
            can_pan: 0,
            min_pan_range: None,
            max_pan_range: None,
            min_pan_step: None,
            max_pan_step: None,
            has_pan_speed: 0,
            min_pan_speed: None,
            max_pan_speed: None,
            has_turbo_pan: 0,
            turbo_pan_speed: None,
            can_tilt: 0,
            min_tilt_range: None,
            max_tilt_range: None,
            min_tilt_step: None,
            max_tilt_step: None,
            has_tilt_speed: 0,
            min_tilt_speed: None,
            max_tilt_speed: None,
            has_turbo_tilt: 0,
            turbo_tilt_speed: None,
            can_auto_scan: 0,
            num_scan_paths: 0,
            can_light: 0,
            can_indicator_light: 0,
            can_audio_play: 0,
            min_audio_file: None,
            max_audio_file: None,
            can_audio_volume: 0,
        };
        c.id = id;
        c.protocol = protocol.map(Into::into);
        c.name = name.into();
        c
    }

    #[test]
    fn the_report_lists_every_unsafe_row_and_nothing_else() {
        let mut bad_dev = monitor("/dev/video0'; id; '", None);
        bad_dev.id = 1;
        let mut uses_bad_control = monitor("", Some("rtsp://cam/1"));
        uses_bad_control.id = 2;
        uses_bad_control.control_id = Some(9);
        let good = monitor("/dev/video1", None);
        let controls = vec![
            control(9, None, "../../tmp/x"),
            control(10, Some("onvif"), "ONVIF"),
        ];
        let findings = unsafe_stored_values(
            &[bad_dev, uses_bad_control, good],
            &controls,
            &[],
            &[],
            true,
        );
        assert_eq!(findings.len(), 3, "{findings:?}");
        assert!(findings[0].contains("monitor 1"));
        assert!(findings[1].contains("monitor 2"));
        assert!(findings[2].contains("control 9"));
    }

    #[test]
    fn ordinary_monitors_pass() {
        assert!(unsafe_monitor_inputs(&monitor("/dev/video0", None), None).is_ok());
        assert!(unsafe_monitor_inputs(&monitor("", Some("rtsp://cam/1")), None).is_ok());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rtsp_server_runs_when_a_port_is_configured() {
        assert!(rtsp_port_configured(Some("8554")));
        assert!(!rtsp_port_configured(Some("0")));
        assert!(!rtsp_port_configured(Some("")));
        assert!(!rtsp_port_configured(None));
    }

    #[test]
    fn test_parse_daemon_command_simple() {
        let (cmd, args) = parse_daemon_command("zmfilter.pl", &[]);
        assert_eq!(cmd, "zmfilter.pl");
        assert!(args.is_empty());
    }

    #[test]
    fn test_parse_daemon_command_with_args() {
        let (cmd, args) = parse_daemon_command("zmc -m 1", &[]);
        assert_eq!(cmd, "zmc");
        assert_eq!(args, vec!["-m", "1"]);
    }

    #[test]
    fn test_parse_daemon_command_with_extra_args() {
        let (cmd, args) = parse_daemon_command("zmc", &["-m".to_string(), "5".to_string()]);
        assert_eq!(cmd, "zmc");
        assert_eq!(args, vec!["-m", "5"]);
    }

    #[test]
    fn zmc_daemon_id_local_with_device_uses_dash_d() {
        assert_eq!(
            zmc_daemon_id(&MonitorType::Local, "/dev/video0", 7),
            "zmc -d /dev/video0"
        );
    }

    #[test]
    fn zmc_daemon_id_local_without_device_uses_dash_m() {
        assert_eq!(zmc_daemon_id(&MonitorType::Local, "", 7), "zmc -m 7");
    }

    #[test]
    fn zmc_daemon_id_non_local_uses_dash_m() {
        // Network camera (Ffmpeg type): always -m, even if a device is set.
        assert_eq!(
            zmc_daemon_id(&MonitorType::Ffmpeg, "/dev/video0", 12),
            "zmc -m 12"
        );
    }

    #[test]
    fn zmc_daemon_id_whitespace_device_falls_back_to_dash_m() {
        // A spaced device would be re-split by parse_daemon_command and rejected
        // by validate_daemon_spec, so we never emit the -d form for it.
        assert_eq!(
            zmc_daemon_id(&MonitorType::Local, "/dev/video 0", 3),
            "zmc -m 3"
        );
    }

    #[test]
    fn zmc_daemon_id_roundtrips_through_validation() {
        // The id `start` produces must parse + validate (so stop/status, which
        // reconstruct from the same id, agree).
        let id = zmc_daemon_id(&MonitorType::Local, "/dev/video0", 1);
        let (cmd, parsed) = parse_daemon_command(&id, &[]);
        assert_eq!(cmd, "zmc");
        assert!(validate_daemon_spec(&cmd, &parsed).is_ok());
    }

    fn args(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn test_validate_daemon_spec_accepts_known_shapes() {
        // Singletons
        assert!(validate_daemon_spec("zmstats.pl", &[]).is_ok());
        assert!(validate_daemon_spec("zmtelemetry.pl", &[]).is_ok());
        assert!(validate_daemon_spec("zmtrigger.pl", &[]).is_ok());
        assert!(validate_daemon_spec("zmeventnotification.pl", &[]).is_ok());

        // zmfilter both shapes
        assert!(validate_daemon_spec("zmfilter.pl", &args(&["--daemon"])).is_ok());
        assert!(validate_daemon_spec("zmfilter.pl", &args(&["--filter_id=7", "--daemon"])).is_ok());

        // zmaudit both spellings
        assert!(validate_daemon_spec("zmaudit.pl", &args(&["-c"])).is_ok());
        assert!(validate_daemon_spec("zmaudit.pl", &args(&["--continuous"])).is_ok());

        // Per-monitor
        assert!(validate_daemon_spec("zmc", &args(&["-m", "3"])).is_ok());
        assert!(validate_daemon_spec("zmc", &args(&["-d", "/dev/video0"])).is_ok());
        assert!(validate_daemon_spec("zma", &args(&["-e", "42"])).is_ok());
        assert!(validate_daemon_spec("zma", &args(&["-e", "42", "-m", "3"])).is_ok());
        // zma no longer runs per monitor (#160).
        assert!(validate_daemon_spec("zma", &args(&["-m", "3"])).is_err());
        assert!(validate_daemon_spec("zm_rtsp_server", &[]).is_ok());
        assert!(validate_daemon_spec("zm_rtsp_server", &args(&["-x"])).is_err());
        assert!(validate_daemon_spec("zmcontrol.pl", &args(&["--id", "3"])).is_ok());
        assert!(validate_daemon_spec("zmtrack.pl", &args(&["-m", "3"])).is_ok());
    }

    #[test]
    fn test_validate_zmnext_worker_spec() {
        // Valid, order-independent.
        assert!(validate_daemon_spec(
            "zm-core",
            &args(&[
                "--monitor-id",
                "7",
                "--pipeline",
                "/var/lib/zm-api/pipelines/monitor_7.json",
                "--socket",
                "/run/zm/stream_7.sock",
            ])
        )
        .is_ok());
        assert!(validate_daemon_spec(
            "zm-core",
            &args(&[
                "--socket",
                "/run/zm/stream_7.sock",
                "--pipeline",
                "/p/monitor_7.json",
                "--monitor-id",
                "7",
            ])
        )
        .is_ok());

        // Path traversal and relative paths are rejected.
        assert!(validate_daemon_spec(
            "zm-core",
            &args(&[
                "--monitor-id",
                "7",
                "--pipeline",
                "/p/../../etc/passwd",
                "--socket",
                "/run/zm/stream_7.sock",
            ])
        )
        .is_err());
        assert!(validate_daemon_spec(
            "zm-core",
            &args(&[
                "--monitor-id",
                "7",
                "--pipeline",
                "relative.json",
                "--socket",
                "/s.sock"
            ])
        )
        .is_err());

        // Missing a required flag, non-numeric id, and odd arg count all fail.
        assert!(validate_daemon_spec(
            "zm-core",
            &args(&["--monitor-id", "7", "--pipeline", "/p.json"])
        )
        .is_err());
        assert!(validate_daemon_spec(
            "zm-core",
            &args(&[
                "--monitor-id",
                "x",
                "--pipeline",
                "/p.json",
                "--socket",
                "/s.sock"
            ])
        )
        .is_err());
        assert!(validate_daemon_spec("zm-core", &args(&["--monitor-id"])).is_err());
    }

    #[test]
    fn test_zmnext_daemon_id_round_trips() {
        let id = zmnext_daemon_id(42);
        assert_eq!(id, "zm-core --monitor-id 42");
        assert_eq!(zmnext_monitor_id_from_id(&id), Some(42));
        // Non-worker ids are rejected.
        assert_eq!(zmnext_monitor_id_from_id("zmc -m 42"), None);
        assert_eq!(zmnext_monitor_id_from_id("zm-core -m 42"), None);
        // The worker id parses back to the same exec arg list.
        let (command, parsed) = parse_daemon_command(&id, &[]);
        assert_eq!(command, "zm-core");
        assert_eq!(parsed, vec!["--monitor-id".to_string(), "42".to_string()]);
    }

    #[test]
    fn test_zminfer_daemon_id_and_validation() {
        let id = zminfer_daemon_id(0);
        assert_eq!(id, "zm-infer --gpu 0");
        // id + the `--socket` extra parse back into a validated arg list.
        let (command, parsed) =
            parse_daemon_command(&id, &args(&["--socket", "/run/zm/zm_infer_gpu0.sock"]));
        assert_eq!(command, "zm-infer");
        assert!(validate_daemon_spec(&command, &parsed).is_ok());
        // Reject a relative/traversal socket, a non-numeric gpu, and a missing gpu.
        assert!(
            validate_daemon_spec("zm-infer", &args(&["--gpu", "0", "--socket", "rel.sock"]))
                .is_err()
        );
        assert!(validate_daemon_spec(
            "zm-infer",
            &args(&["--gpu", "x", "--socket", "/run/zm/s.sock"])
        )
        .is_err());
        assert!(validate_daemon_spec("zm-infer", &args(&["--socket", "/run/zm/s.sock"])).is_err());
    }

    #[test]
    fn test_validate_daemon_spec_rejects_injection_attempts() {
        // Absolute paths are the main attack vector against PathBuf::join
        assert!(validate_daemon_spec("/bin/bash", &args(&["-c", "id"])).is_err());
        assert!(validate_daemon_spec("/usr/bin/rm", &args(&["-rf", "/"])).is_err());

        // Common system tools that aren't ZM daemons
        assert!(validate_daemon_spec("rm", &args(&["-rf", "/home"])).is_err());
        assert!(validate_daemon_spec("sh", &args(&["-c", "echo pwned"])).is_err());
        assert!(validate_daemon_spec("python3", &[]).is_err());

        // Known daemon, wrong args
        assert!(validate_daemon_spec("zmc", &args(&["-x", "5"])).is_err());
        assert!(validate_daemon_spec("zmc", &args(&["-m", "not_a_number"])).is_err());
        assert!(validate_daemon_spec("zmc", &args(&["-m"])).is_err());
        assert!(validate_daemon_spec("zmstats.pl", &args(&["extra"])).is_err());

        // Bogus device paths (upstream regex blocks anything outside /dev/...)
        assert!(validate_daemon_spec("zmc", &args(&["-d", "/etc/passwd"])).is_err());
        assert!(validate_daemon_spec("zmc", &args(&["-d", "/dev/../etc/passwd"])).is_err());
        assert!(validate_daemon_spec("zmc", &args(&["-d", "/dev/video0; rm -rf /"])).is_err());

        // Filter id must be a number
        assert!(
            validate_daemon_spec("zmfilter.pl", &args(&["--filter_id=evil", "--daemon"])).is_err()
        );
    }

    #[tokio::test]
    async fn test_start_daemon_rejects_invalid_id() {
        // End-to-end: start_daemon refuses arbitrary executables. We use an
        // ID that would otherwise resolve and (potentially) spawn.
        let config = DaemonConfig::default();
        let manager = DaemonManager::new(config, None);
        let resp = manager
            .start_daemon("/bin/sh -c whoami", &[])
            .await
            .unwrap();
        assert!(!resp.success, "expected start_daemon to reject /bin/sh");
        assert!(
            resp.message.contains("Invalid daemon spec"),
            "got: {}",
            resp.message
        );
    }

    #[test]
    fn test_extra_args_from_id() {
        // Plain command id, args are all extras
        assert_eq!(
            extra_args_from_id("zmfilter.pl", &["--daemon".to_string()]),
            vec!["--daemon".to_string()]
        );

        // id encodes the args; nothing extra
        let args = vec!["-m".to_string(), "2".to_string()];
        assert!(extra_args_from_id("zmc -m 2", &args).is_empty());

        // id encodes some, plus an extra
        let args = vec!["--filter_id=5".to_string(), "--daemon".to_string()];
        assert_eq!(
            extra_args_from_id("zmfilter.pl --filter_id=5", &args),
            vec!["--daemon".to_string()]
        );

        // Empty inputs
        assert!(extra_args_from_id("", &[]).is_empty());
        assert!(extra_args_from_id("zmc -m 2", &[]).is_empty());
    }

    #[test]
    fn test_parse_zm_bool() {
        // Truthy values ZoneMinder may write
        for v in ["1", "true", "TRUE", "yes", "Yes", "on", "ON", " 1 "] {
            assert!(parse_zm_bool(v), "expected `{v}` to be true");
        }
        // Falsy / unrecognized values default to false (matches the
        // conservative "off if unsure" behavior of the auto-start gate)
        for v in ["0", "false", "no", "off", "", "garbage", "2"] {
            assert!(!parse_zm_bool(v), "expected `{v}` to be false");
        }
    }

    #[test]
    fn test_extract_monitor_id() {
        assert_eq!(
            extract_monitor_id(&["-m".to_string(), "5".to_string()]),
            Some(5)
        );
        assert_eq!(
            extract_monitor_id(&["-d".to_string(), "/dev/video0".to_string()]),
            None
        );
        // The zm-core worker uses the long flag form.
        assert_eq!(
            extract_monitor_id(&[
                "--monitor-id".to_string(),
                "9".to_string(),
                "--pipeline".to_string(),
                "/p.json".to_string(),
            ]),
            Some(9)
        );
        assert_eq!(extract_monitor_id(&[]), None);
    }

    #[tokio::test]
    async fn test_daemon_manager_creation() {
        let config = DaemonConfig::default();
        let manager = DaemonManager::new(config, Some(1));

        assert_eq!(manager.server_id(), Some(1));
        assert!(!manager.is_running().await);
    }

    #[tokio::test]
    async fn test_daemon_manager_startup() {
        let config = DaemonConfig::default();
        let manager = Arc::new(DaemonManager::new(config, None));

        let resp = manager.startup().await.unwrap();
        assert!(resp.success);
        assert!(manager.is_running().await);

        // Signal shutdown to stop background task
        manager.signal_shutdown();
    }

    #[tokio::test]
    async fn test_daemon_manager_double_startup() {
        let config = DaemonConfig::default();
        let manager = Arc::new(DaemonManager::new(config, None));

        manager.startup().await.unwrap();
        let resp = manager.startup().await.unwrap();
        assert!(!resp.success); // Already running

        // Signal shutdown to stop background task
        manager.signal_shutdown();
    }

    #[tokio::test]
    async fn test_get_status_empty() {
        let config = DaemonConfig::default();
        let manager = DaemonManager::new(config, None);

        let status = manager.get_status().await;
        assert!(!status.running);
        assert!(status.daemons.is_empty());
    }

    #[tokio::test]
    async fn test_register_daemon() {
        let config = DaemonConfig::default();
        let manager = DaemonManager::new(config, None);

        let process = ManagedProcess::new("test", "Test", "test", vec![], true, None);
        manager.register_daemon(process).await;

        let ids = manager.list_daemon_ids().await;
        assert_eq!(ids, vec!["test"]);
    }

    /// Regression: a daemon that exits once must be handled exactly once.
    ///
    /// The dead `Child` handle used to be left in place after exit detection.
    /// tokio's `try_wait()` caches the exit status and returns it on every
    /// subsequent call, so each health-check cycle re-entered the exit branch,
    /// re-ran `prepare_restart` (which resets the backoff clock via
    /// `set_state`) and therefore never let the backoff elapse — the daemon
    /// stayed dead while the log filled with a phantom "exited" line every
    /// interval. This is what stopped `zmfilter.pl` (PurgeWhenFull) from ever
    /// being restarted and let the disk fill.
    #[tokio::test]
    async fn test_exited_daemon_handled_once_not_re_detected_each_cycle() {
        let config = DaemonConfig::default();
        let manager = DaemonManager::new(config, None);

        // A real child process that has already exited and been reaped.
        let mut child = Command::new("sh")
            .arg("-c")
            .arg("exit 0")
            .spawn()
            .expect("spawn test child");
        let _ = child.wait().await;

        let mut process = ManagedProcess::new("test-daemon", "Test", "test", vec![], true, None);
        process.set_child(child);
        manager.register_daemon(process).await;

        // Several cycles. The default min backoff (5s) has not elapsed, so no
        // real restart is attempted within the test window.
        manager.check_daemons().await;
        manager.check_daemons().await;
        manager.check_daemons().await;

        let processes = manager.processes.read().await;
        let process = processes.get("test-daemon").expect("process present");
        assert_eq!(
            process.restart_count, 1,
            "exit must be handled once, not re-detected every cycle"
        );
        assert_eq!(process.state, ProcessState::Restarting);
        assert!(
            !process.has_child(),
            "dead child handle must be cleared so try_wait() cannot re-fire"
        );
        assert!(
            process.pid.is_none(),
            "a dead pid may be reused; it must be forgotten so stop/kill never signal it (#81)"
        );
    }

    #[test]
    fn zm_core_args_accept_one_control_uid() {
        let base = |extra: &[&str]| {
            let mut v: Vec<String> = [
                "--monitor-id",
                "3",
                "--pipeline",
                "-",
                "--socket",
                "/run/zm/stream_3.sock",
            ]
            .iter()
            .map(|s| s.to_string())
            .collect();
            v.extend(extra.iter().map(|s| s.to_string()));
            v
        };
        assert!(validate_zmnext_args(&base(&[])).is_ok());
        assert!(validate_zmnext_args(&base(&["--control-uid", "997"])).is_ok());
        assert!(validate_zmnext_args(&base(&["--control-uid", "root"])).is_err());
        assert!(
            validate_zmnext_args(&base(&["--control-uid", "1", "--control-uid", "2"])).is_err()
        );
    }

    /// The `--help` text zm-next's Phase 1 zm-core prints (dev box, 2026-09-14).
    #[tokio::test]
    async fn control_uid_support_is_read_from_help() {
        let help = b"Options:\n  --socket <path>      Unix socket\n  --control-uid <uid>  Also accept commands from this uid";
        assert!(help_mentions_control_uid(help));
        assert!(!help_mentions_control_uid(
            b"Usage: zm-core --pipeline <pipeline.json | ->"
        ));

        let dir = std::env::temp_dir().join(format!("zmcore_probe_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for (name, text, want) in [
            ("new", "  --control-uid <uid>  Also accept commands", true),
            ("old", "  --socket <path>  Unix socket", false),
        ] {
            let bin = dir.join(name);
            std::fs::write(&bin, format!("#!/bin/sh\necho '{text}'\n")).unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
            }
            // A test forking in parallel can briefly hold the just-written
            // file open, and exec then fails with ETXTBSY. Wait it out.
            for _ in 0..100 {
                if std::process::Command::new(&bin).output().is_ok() {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            assert_eq!(zmcore_supports_control_uid(&bin).await, want, "{name}");
        }
        assert!(!zmcore_supports_control_uid(&dir.join("missing")).await);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `[zmnext.worker].binary` as an absolute path is the one checked and
    /// run; with zm-next off there's nothing to check.
    #[test]
    fn zmcore_binary_honours_an_absolute_path() {
        let mut manager = DaemonManager::new(DaemonConfig::default(), None);
        assert!(manager.zmcore_installed().is_none());

        let mut cfg = crate::configure::zmnext::ZmNextConfig {
            enabled: true,
            ..Default::default()
        };
        cfg.worker.binary = "/nonexistent/zm-next/zm-core".into();
        manager.set_zmnext(cfg, "/run/zm".into(), Default::default());
        assert_eq!(
            manager.zmcore_installed(),
            Some((PathBuf::from("/nonexistent/zm-next/zm-core"), false))
        );
    }

    /// A zm-next worker that keeps failing to load its pipeline (exit 3) is
    /// left Failed with a reason after three fast exits, instead of cycling
    /// through backoff forever. A plain daemon with the same exits keeps
    /// restarting.
    #[tokio::test]
    async fn a_worker_that_keeps_failing_to_load_is_given_up_on() {
        async fn exited(code: i32) -> tokio::process::Child {
            let mut child = Command::new("sh")
                .arg("-c")
                .arg(format!("exit {code}"))
                .spawn()
                .expect("spawn test child");
            let _ = child.wait().await;
            child
        }

        let manager = DaemonManager::new(DaemonConfig::default(), None);
        let worker = "zm-core --monitor-id 9";
        for (id, name) in [(worker, "worker"), ("other-daemon", "other")] {
            manager
                .register_daemon(ManagedProcess::new(id, name, "x", vec![], true, Some(9)))
                .await;
        }

        for round in 1..=3 {
            for id in [worker, "other-daemon"] {
                let child = exited(3).await;
                manager
                    .processes
                    .write()
                    .await
                    .get_mut(id)
                    .unwrap()
                    .set_child(child);
            }
            manager.check_daemons().await;
            let processes = manager.processes.read().await;
            let w = &processes[worker];
            if round < 3 {
                assert_eq!(w.state, ProcessState::Restarting, "round {round}");
                assert!(w.gave_up.is_none());
            } else {
                assert_eq!(w.state, ProcessState::Failed);
                let reason = w.gave_up.as_deref().expect("a reason");
                assert!(reason.contains("pipeline failed to load"), "{reason}");
                let status = ProcessStatus::from(w);
                assert_eq!(status.last_exit_code, Some(3));
                assert_eq!(status.failure_reason.as_deref(), Some(reason));
            }
            assert_eq!(processes["other-daemon"].state, ProcessState::Restarting);
        }
    }

    /// Regression for #77: an entry that has been sent SIGTERM but whose exit
    /// has not been observed must refuse a start; spawning over it overwrote
    /// the child handle and leaked the old process.
    #[tokio::test]
    async fn start_is_refused_while_the_old_process_is_still_stopping() {
        let manager = DaemonManager::new(DaemonConfig::default(), None);
        let mut process = ManagedProcess::for_monitor(1, None);
        process.set_state(ProcessState::Stopping);
        manager.register_daemon(process).await;

        let resp = manager
            .start_daemon("zmc", &["-m".to_string(), "1".to_string()])
            .await
            .unwrap();
        assert!(!resp.success);
        assert!(resp.message.contains("still stopping"), "{}", resp.message);
    }

    /// Regression for #76: reconcile only ever diffed the database against the
    /// process map in one direction, so a deleted monitor's daemons ran on.
    #[tokio::test]
    async fn daemons_of_absent_monitors_are_stopped() {
        let manager = DaemonManager::new(DaemonConfig::default(), None);
        for m in [7, 9] {
            let mut p = ManagedProcess::for_monitor(m, None);
            p.set_state(ProcessState::Running);
            manager.register_daemon(p).await;
        }

        let live: std::collections::HashSet<u32> = [7].into_iter().collect();
        assert_eq!(manager.stop_daemons_for_absent_monitors(&live).await, 1);

        let processes = manager.processes.read().await;
        assert_eq!(processes["zmc -m 9"].state, ProcessState::Stopped);
        assert!(!processes["zmc -m 9"].auto_restart);
        assert_eq!(processes["zmc -m 7"].state, ProcessState::Running);
    }

    /// Regression for #113: two Local monitors share one `zmc -d /dev/video0`;
    /// the device must run while either wants it.
    #[test]
    fn a_shared_capture_daemon_runs_while_any_monitor_wants_it() {
        let wants = vec![
            (1, "zmc -d /dev/video0".to_string(), true),
            (2, "zmc -d /dev/video0".to_string(), false),
            (3, "zmc -m 3".to_string(), false),
        ];
        let folded = fold_should_run(&wants);
        assert!(folded["zmc -d /dev/video0"]);
        assert!(!folded["zmc -m 3"]);
    }

    /// Regression for #80: every startup retires the loops of the previous
    /// one, so a loop that missed the shutdown wake-up mid-tick exits at its
    /// next tick instead of running beside its replacement forever.
    #[tokio::test]
    async fn each_startup_retires_the_previous_loops() {
        let manager = Arc::new(DaemonManager::new(DaemonConfig::default(), None));
        manager.startup().await.unwrap();
        assert_eq!(manager.loop_generation(), 1);
        assert!(!manager.loop_is_stale(1));

        manager.signal_shutdown();
        assert!(manager.loop_is_stale(1));
        *manager.running.write().await = false;
        manager.startup().await.unwrap();
        assert_eq!(manager.loop_generation(), 2);
        assert!(manager.loop_is_stale(1), "generation-1 loops must exit");
        assert!(!manager.loop_is_stale(2));
        manager.signal_shutdown();
    }

    /// Regression for #88: `[daemon].enable_watchdog` was parsed, documented
    /// and shipped in zm-api.env but never read. It gates only the hung-process
    /// check; exit detection and crash restarts stay on regardless.
    #[tokio::test]
    async fn enable_watchdog_false_disables_only_the_hung_check() {
        let max = Duration::from_secs(30);
        let stale = || {
            let mut p = ManagedProcess::new("zmc -m 7", "zmc[7]", "zmc", vec![], true, Some(7));
            p.set_state(ProcessState::Running);
            p.last_active_at = Some(std::time::Instant::now() - max - Duration::from_secs(1));
            p
        };

        let on = DaemonManager::new(DaemonConfig::default(), None);
        on.register_daemon(stale()).await;
        assert_eq!(on.hung_candidates(max).await, vec!["zmc -m 7".to_string()]);

        let off = DaemonManager::new(
            DaemonConfig {
                enable_watchdog: false,
                ..DaemonConfig::default()
            },
            None,
        );
        off.register_daemon(stale()).await;
        assert!(off.hung_candidates(max).await.is_empty());
    }

    /// A `zmc` is judged by the heartbeat in its shared memory when the
    /// segment can be read (#123): a stale heartbeat is hung even while the
    /// process burns CPU, and a fresh one is healthy even when it does not.
    #[tokio::test]
    async fn zmc_liveness_comes_from_the_shared_memory_heartbeat() {
        use std::time::{SystemTime, UNIX_EPOCH};

        let max = Duration::from_secs(30);
        let dir = tempfile::tempdir().unwrap();
        let write_segment = |heartbeat_age: u64| {
            // SharedData: `size` at +0, `valid` at +92, `heartbeat_time` at
            // +128; then TriggerData with its `size`, as zmc writes them.
            let mut bytes = vec![0u8; 872 + 560];
            bytes[0..4].copy_from_slice(&872u32.to_ne_bytes());
            bytes[872..876].copy_from_slice(&560u32.to_ne_bytes());
            bytes[92] = 1;
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs() as i64;
            bytes[128..136].copy_from_slice(&(now - heartbeat_age as i64).to_le_bytes());
            std::fs::write(dir.path().join("zm.mmap.7"), bytes).unwrap();
        };
        let past_grace = || {
            let mut p = ManagedProcess::new("zmc -m 7", "zmc[7]", "zmc", vec![], true, Some(7));
            p.set_state(ProcessState::Running);
            p.started_at = Some(std::time::Instant::now() - max - Duration::from_secs(1));
            p
        };

        // Stale heartbeat, busy CPU: hung.
        write_segment(120);
        let manager = DaemonManager::new(DaemonConfig::default(), None);
        manager.set_shm_dir(dir.path());
        let mut p = past_grace();
        p.last_active_at = Some(std::time::Instant::now());
        manager.register_daemon(p).await;
        assert_eq!(
            manager.hung_candidates(max).await,
            vec!["zmc -m 7".to_string()]
        );

        // Fresh heartbeat, idle CPU: healthy.
        write_segment(2);
        let manager = DaemonManager::new(DaemonConfig::default(), None);
        manager.set_shm_dir(dir.path());
        let mut p = past_grace();
        p.last_active_at = Some(std::time::Instant::now() - max - Duration::from_secs(1));
        manager.register_daemon(p).await;
        assert!(manager.hung_candidates(max).await.is_empty());

        // No segment at all: back to the CPU heuristic, so that same idle
        // process is hung — and a busy one is not.
        std::fs::remove_file(dir.path().join("zm.mmap.7")).unwrap();
        assert_eq!(
            manager.hung_candidates(max).await,
            vec!["zmc -m 7".to_string()]
        );
    }

    #[test]
    fn heartbeat_age_handles_missing_and_future_heartbeats() {
        use std::time::SystemTime;
        let now = SystemTime::now();
        let uptime = Duration::from_secs(90);
        assert_eq!(heartbeat_age(None, now, uptime), uptime);
        assert_eq!(
            heartbeat_age(Some(now - Duration::from_secs(40)), now, uptime),
            Duration::from_secs(40)
        );
        assert_eq!(
            heartbeat_age(Some(now + Duration::from_secs(5)), now, uptime),
            Duration::ZERO
        );
    }

    /// Regression for #87: base.toml told operators to "enable the Rust job
    /// and disable the matching Perl daemon together", but nothing in zm-api
    /// did the second half — takeover started zmstats.pl/zmaudit.pl/
    /// zmtelemetry.pl regardless of `[maintenance.*].enabled`, and both then
    /// wrote the same rows. The native job must suppress its Perl counterpart,
    /// and only its counterpart.
    #[test]
    fn native_job_suppresses_only_its_perl_counterpart() {
        let none = NativeJobs::default();
        for d in ["zmstats", "zmaudit", "zmtelemetry", "zmtrigger", "zmfilter"] {
            assert_eq!(
                native_replacement_for(d, none),
                None,
                "{d} with no native jobs"
            );
        }

        let stats_only = NativeJobs {
            stats: true,
            ..NativeJobs::default()
        };
        assert_eq!(
            native_replacement_for("zmstats", stats_only),
            Some("[maintenance.stats]")
        );
        assert_eq!(native_replacement_for("zmaudit", stats_only), None);
        assert_eq!(native_replacement_for("zmtelemetry", stats_only), None);

        let all = NativeJobs {
            stats: true,
            audit: true,
            telemetry: true,
        };
        assert_eq!(
            native_replacement_for("zmaudit", all),
            Some("[maintenance.audit]")
        );
        assert_eq!(
            native_replacement_for("zmtelemetry", all),
            Some("[maintenance.telemetry]")
        );
        // Daemons with no native replacement are never touched by this gate.
        assert_eq!(native_replacement_for("zmtrigger", all), None);
        assert_eq!(native_replacement_for("zmeventnotification", all), None);
    }

    /// Regression for #74: the `pkill -9` orphan sweep ran inside
    /// `start_all_daemons`, which is also what the legacy socket `startup` /
    /// `pkg_start` commands and `POST /system/startup` call. On a running
    /// supervisor that SIGKILLed every healthy child. The sweep is for
    /// leftovers from a previous *process*, so it runs once per manager.
    #[tokio::test]
    async fn orphan_sweep_is_due_once_per_manager_lifetime() {
        let manager = DaemonManager::new(DaemonConfig::default(), None);

        assert!(manager.orphan_sweep_due(), "first start must sweep");
        assert!(!manager.orphan_sweep_due(), "second start must not sweep");
        assert!(!manager.orphan_sweep_due());
    }

    /// Regression: once shutdown is signaled, `start_daemon` must refuse so a
    /// reconcile/health restart can't orphan a daemon that misses the stop wave
    /// (which previously hung shutdown until systemd SIGKILL'd the unit).
    #[tokio::test]
    async fn test_no_daemon_starts_while_shutting_down() {
        let config = DaemonConfig::default();
        let manager = DaemonManager::new(config, None);

        assert!(!manager.is_shutting_down());
        manager.signal_shutdown();
        assert!(manager.is_shutting_down());

        // A would-be reconcile/health restart must be refused before it can
        // resolve a path or fork a process.
        let resp = manager
            .start_daemon("zmc", &["-m".to_string(), "1".to_string()])
            .await
            .unwrap();

        assert!(
            !resp.success,
            "start must be refused while shutting down to avoid orphaned daemons"
        );
        assert!(
            manager.list_daemon_ids().await.is_empty(),
            "no process entry should be created for a refused start"
        );
    }
}
