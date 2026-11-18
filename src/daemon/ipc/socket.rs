//! Unix domain socket server for legacy IPC compatibility.

use jiff::civil::DateTime as Civil;
use std::path::PathBuf;
use std::sync::Arc;

use std::path::Path;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::Notify;
use tracing::{debug, error, info, warn};

use crate::daemon::commands::{format_response, parse_command};
use crate::daemon::ipc::{canonical_daemon_id, DaemonCommand, DaemonResponse, SystemStatus};
use crate::daemon::manager::DaemonManager;
use crate::daemon::ProcessState;
use crate::error::{AppError, AppResult};

/// Longest command line a client may send, and how long it has to send it.
/// zmdc.pl commands are a few dozen bytes; anything else is a stuck or
/// hostile peer holding a task open (#119).
const MAX_COMMAND_BYTES: u64 = 4096;
const COMMAND_TIMEOUT: Duration = Duration::from_secs(30);

/// Unix domain socket server for the daemon controller.
pub struct DaemonSocketServer {
    /// Path to the socket file
    socket_path: PathBuf,
    /// Reference to the daemon manager
    manager: Arc<DaemonManager>,
    /// Shutdown signal
    shutdown: Arc<Notify>,
    /// Group the socket is chgrp'd to so ZoneMinder's web user can connect.
    socket_group: Option<String>,
}

/// Whether something is already answering on the legacy socket — a live
/// zmdc.pl, or another zm-api. Taking over would unlink its socket and
/// pkill its children (#118).
pub async fn legacy_socket_is_live(path: &Path) -> bool {
    if !path.exists() {
        return false;
    }
    tokio::time::timeout(Duration::from_secs(2), UnixStream::connect(path))
        .await
        .is_ok_and(|r| r.is_ok())
}

impl DaemonSocketServer {
    /// Create a new socket server.
    pub fn new(socket_path: PathBuf, manager: Arc<DaemonManager>) -> Self {
        Self {
            socket_path,
            manager,
            shutdown: Arc::new(Notify::new()),
            socket_group: None,
        }
    }

    /// Group to chgrp the socket to. `None` falls back to `ZM_WEB_GROUP`
    /// from zm.conf, which is who zmdc.pl clients run as.
    pub fn with_socket_group(mut self, group: Option<String>) -> Self {
        self.socket_group = group;
        self
    }

    /// Get the socket path.
    pub fn socket_path(&self) -> &PathBuf {
        &self.socket_path
    }

    /// Signal the server to shutdown.
    pub fn signal_shutdown(&self) {
        self.shutdown.notify_waiters();
    }

    /// Start the socket server.
    ///
    /// This runs until shutdown is signaled.
    pub async fn run(&self) -> AppResult<()> {
        if legacy_socket_is_live(&self.socket_path).await {
            return Err(AppError::ServiceUnavailableError(format!(
                "something is already serving {} (zmdc.pl, or another zm-api); \
                 stop it before enabling daemon control here",
                self.socket_path.display()
            )));
        }
        // Remove a stale socket file if present
        if self.socket_path.exists() {
            std::fs::remove_file(&self.socket_path)?;
        }

        // Ensure parent directory exists
        if let Some(parent) = self.socket_path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        // Bind to the socket
        let listener = UnixListener::bind(&self.socket_path)?;
        info!("Daemon socket server listening on {:?}", self.socket_path);

        // Anything writing this socket can spawn daemon processes, so it is
        // never world-writable. The legitimate clients — the web console,
        // zmpkg.pl, zmsystemctl.pl — run as ZoneMinder's web user, which is
        // not this unit's user, so the socket is chgrp'd to the web group
        // (config `socket_group`, else ZM_WEB_GROUP from zm.conf) before the
        // 0660 mode means anything (#85). chgrp needs the service account in
        // that group; setup-instance.sh adds it.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let group = self.socket_group.clone().or_else(|| {
                crate::configure::zmconf::ZmConfig::load()
                    .get("ZM_WEB_GROUP")
                    .map(str::to_string)
            });
            match group {
                Some(name) => match nix::unistd::Group::from_name(&name) {
                    Ok(Some(g)) => {
                        if let Err(e) = nix::unistd::chown(&self.socket_path, None, Some(g.gid)) {
                            warn!(
                                "could not chgrp {} to {name}: {e}; zmdc.pl clients running as \
                                 the web user will not be able to connect — add the zm-api \
                                 service user to group {name}",
                                self.socket_path.display()
                            );
                        }
                    }
                    Ok(None) => {
                        warn!("socket group {name} does not exist; zmdc.sock stays owner-group")
                    }
                    Err(e) => warn!("could not look up group {name}: {e}"),
                },
                None => warn!(
                    "no socket_group and no ZM_WEB_GROUP in zm.conf; zmdc.sock is only \
                     reachable by the zm-api service user's own group"
                ),
            }
            let perms = std::fs::Permissions::from_mode(0o660);
            std::fs::set_permissions(&self.socket_path, perms)?;
        }

        loop {
            tokio::select! {
                result = listener.accept() => {
                    match result {
                        Ok((stream, _addr)) => {
                            let manager = Arc::clone(&self.manager);
                            tokio::spawn(async move {
                                if let Err(e) = handle_client(stream, manager).await {
                                    warn!("Error handling client: {}", e);
                                }
                            });
                        }
                        Err(e) => {
                            error!("Error accepting connection: {}", e);
                        }
                    }
                }
                _ = self.shutdown.notified() => {
                    info!("Socket server shutting down");
                    break;
                }
            }
        }

        // Clean up socket file
        if self.socket_path.exists() {
            let _ = std::fs::remove_file(&self.socket_path);
        }

        Ok(())
    }
}

/// Handle a single client connection.
async fn handle_client(stream: UnixStream, manager: Arc<DaemonManager>) -> AppResult<()> {
    let (reader, mut writer) = stream.into_split();
    let mut reader = BufReader::new(reader.take(MAX_COMMAND_BYTES));
    let mut line = String::new();

    // Read a single command line, bounded in size and time (#119).
    match tokio::time::timeout(COMMAND_TIMEOUT, reader.read_line(&mut line)).await {
        Ok(Ok(0)) => {
            debug!("Client disconnected");
            return Ok(());
        }
        Ok(Ok(_)) if !line.ends_with('\n') && line.len() as u64 >= MAX_COMMAND_BYTES => {
            warn!("Rejecting over-long command from client");
            writer.write_all(b"ERR;command too long\n").await?;
            return Ok(());
        }
        Ok(Ok(_)) => {}
        Ok(Err(e)) => {
            warn!("Error reading from client: {}", e);
            return Err(e.into());
        }
        Err(_) => {
            warn!(
                "Client sent no command within {:?}; dropping",
                COMMAND_TIMEOUT
            );
            return Ok(());
        }
    }

    let line = line.trim();
    debug!("Received command: {}", line);

    // Detect if client wants JSON response
    let json_format = line.starts_with('{');

    // Parse the command
    let response = match parse_command(line) {
        Ok(cmd) => execute_command(cmd, &manager).await,
        Err(e) => DaemonResponse::error(e),
    };

    // Send response
    let response_text = format_response(&response, json_format);
    writer.write_all(response_text.as_bytes()).await?;
    writer.write_all(b"\n").await?;
    writer.flush().await?;

    Ok(())
}

/// Execute a daemon command and return a response.
async fn execute_command(cmd: DaemonCommand, manager: &Arc<DaemonManager>) -> DaemonResponse {
    match cmd {
        DaemonCommand::Startup => match manager.start_all_daemons().await {
            Ok(resp) => resp,
            Err(e) => DaemonResponse::error(e.to_string()),
        },
        DaemonCommand::Shutdown => match manager.shutdown_all().await {
            Ok(resp) => resp,
            Err(e) => DaemonResponse::error(e.to_string()),
        },
        DaemonCommand::Status { target } => {
            let mut status = manager.get_status().await;
            if let Some(target) = &target {
                status.daemons.retain(|d| &d.id == target);
            }
            let text = legacy_status_text(&status);
            DaemonResponse::ok_with_data("Status retrieved", &status).with_legacy_text(text)
        }
        DaemonCommand::Check { target } => {
            // The exact words zmdc.pl prints; the web console string-matches
            // them (#83).
            let word = match target {
                None => {
                    if manager.is_running().await {
                        "running"
                    } else {
                        "stopped"
                    }
                }
                Some(id) => match manager.daemon_state(&id).await {
                    Some(ProcessState::Running | ProcessState::Starting) => "running",
                    Some(ProcessState::Restarting) => "pending",
                    Some(_) => "stopped",
                    None => "unknown",
                },
            };
            DaemonResponse::ok(word).with_legacy_text(word)
        }
        DaemonCommand::LogRot => {
            // SIGWINCH, never SIGHUP: HUP is "reload", which makes zmc drop
            // its camera and the Perl daemons exit for a restart. Nightly log
            // rotation must not interrupt capture (#86; upstream #5063).
            let count = manager.logrot_all().await;
            DaemonResponse::ok(format!("Log rotation signal sent to {} daemons", count))
        }
        DaemonCommand::Version => DaemonResponse::ok(env!("CARGO_PKG_VERSION")),
        // One tracked entry per process: "zmc" + ["-m","1"] is "zmc -m 1" (#84).
        DaemonCommand::Start { daemon, args } => {
            match manager
                .start_daemon(&canonical_daemon_id(&daemon, &args), &[])
                .await
            {
                Ok(resp) => resp,
                Err(e) => DaemonResponse::error(e.to_string()),
            }
        }
        DaemonCommand::Stop { daemon, args } => {
            match manager
                .stop_daemon(&canonical_daemon_id(&daemon, &args))
                .await
            {
                Ok(resp) => resp,
                Err(e) => DaemonResponse::error(e.to_string()),
            }
        }
        DaemonCommand::Restart { daemon, args } => {
            match manager
                .restart_daemon(&canonical_daemon_id(&daemon, &args), &[])
                .await
            {
                Ok(resp) => resp,
                Err(e) => DaemonResponse::error(e.to_string()),
            }
        }
        DaemonCommand::Reload { daemon, args } => {
            match manager
                .reload_daemon(&canonical_daemon_id(&daemon, &args))
                .await
            {
                Ok(resp) => resp,
                Err(e) => DaemonResponse::error(e.to_string()),
            }
        }
        DaemonCommand::PackageStart => {
            // Full system startup - starts all daemons
            match manager.start_all_daemons().await {
                Ok(resp) => resp,
                Err(e) => DaemonResponse::error(e.to_string()),
            }
        }
        DaemonCommand::PackageStop => match manager.shutdown_all().await {
            Ok(resp) => resp,
            Err(e) => DaemonResponse::error(e.to_string()),
        },
        DaemonCommand::PackageRestart => {
            // Stop then start all daemons
            let _ = manager.shutdown_all().await;
            // Small delay to let processes terminate
            tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
            match manager.start_all_daemons().await {
                Ok(resp) => resp,
                Err(e) => DaemonResponse::error(e.to_string()),
            }
        }
        DaemonCommand::ApplyState { state_name } => manager.apply_state(&state_name).await,
    }
}

/// The local wall-clock at which a process with this uptime started. The
/// subtraction is on the instant, so a DST change in between does not shift it.
fn started_at(now: jiff::Timestamp, uptime_seconds: u64, zone: &jiff::tz::TimeZone) -> Civil {
    let uptime = jiff::SignedDuration::from_secs(i64::try_from(uptime_seconds).unwrap_or(i64::MAX));
    now.saturating_sub(uptime)
        .unwrap_or(now)
        .to_zoned(zone.clone())
        .datetime()
}

/// zmdc.pl's `status` output, line for line: the web console looks for
/// `'<command>' running` in it (#83).
fn legacy_status_text(status: &SystemStatus) -> String {
    let now = jiff::Timestamp::now();
    let zone = jiff::tz::TimeZone::system();
    status
        .daemons
        .iter()
        .map(|d| match d.state {
            ProcessState::Running | ProcessState::Starting => {
                let since = started_at(now, d.uptime_seconds.unwrap_or(0), &zone);
                format!(
                    "'{}' running since {}, pid = {}",
                    d.id,
                    since.strftime("%y/%m/%d %H:%M:%S"),
                    d.pid.unwrap_or(0)
                )
            }
            ProcessState::Restarting => format!("'{}' pending", d.id),
            _ => format!("'{}' stopped", d.id),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 03:30 in New York just after the clocks went forward: two hours earlier
    /// was 00:30, not the 01:30 that wall-clock subtraction gives.
    #[test]
    fn a_start_time_is_right_across_a_dst_change() {
        let zone = jiff::tz::TimeZone::get("America/New_York").unwrap();
        let now: jiff::Timestamp = "2026-03-08T07:30:00Z".parse().unwrap();
        assert_eq!(
            started_at(now, 2 * 3600, &zone),
            jiff::civil::date(2026, 3, 8).at(0, 30, 0, 0)
        );
        // An uptime too large to subtract leaves the earliest instant jiff has.
        assert!(started_at(now, u64::MAX, &zone) < jiff::civil::date(1, 1, 1).at(0, 0, 0, 0));
    }
    use crate::daemon::config::DaemonConfig;
    use crate::daemon::ManagedProcess;

    #[tokio::test]
    async fn test_execute_command_check() {
        let config = DaemonConfig::default();
        let manager = Arc::new(DaemonManager::new(config, None));

        let resp = execute_command(DaemonCommand::Check { target: None }, &manager).await;
        assert!(resp.success);
        assert_eq!(resp.message, "stopped");
        assert_eq!(
            resp.to_legacy(),
            "stopped",
            "text clients get the bare word"
        );
    }

    /// `zmdc.pl check zmc -m 1` must answer for that daemon with zmdc.pl's
    /// words, and `status` must print zmdc.pl's line shape (#83).
    #[tokio::test]
    async fn check_and_status_answer_for_one_daemon_in_zmdc_words() {
        let manager = Arc::new(DaemonManager::new(DaemonConfig::default(), None));
        let mut running = ManagedProcess::for_monitor(1, None);
        running.set_state(ProcessState::Running);
        manager.register_daemon(running).await;
        let mut pending = ManagedProcess::for_monitor(2, None);
        pending.set_state(ProcessState::Restarting);
        manager.register_daemon(pending).await;

        let check = |t: &str| DaemonCommand::Check {
            target: Some(t.to_string()),
        };
        assert_eq!(
            execute_command(check("zmc -m 1"), &manager)
                .await
                .to_legacy(),
            "running"
        );
        assert_eq!(
            execute_command(check("zmc -m 2"), &manager)
                .await
                .to_legacy(),
            "pending"
        );
        assert_eq!(
            execute_command(check("zmc -m 9"), &manager)
                .await
                .to_legacy(),
            "unknown"
        );

        let status = execute_command(
            DaemonCommand::Status {
                target: Some("zmc -m 1".to_string()),
            },
            &manager,
        )
        .await;
        let text = status.to_legacy();
        assert!(text.starts_with("'zmc -m 1' running since "), "{text}");
        assert!(
            !text.contains("zmc -m 2"),
            "status for one daemon lists only it"
        );
    }

    /// `zmdc.pl stop zmc -m 1` arrives as daemon "zmc" + args; it must find
    /// the entry keyed "zmc -m 1" rather than report it not found (#84).
    #[tokio::test]
    async fn daemon_plus_args_addresses_the_tracked_entry() {
        let manager = Arc::new(DaemonManager::new(DaemonConfig::default(), None));
        let mut p = ManagedProcess::for_monitor(1, None);
        p.set_state(ProcessState::Running);
        manager.register_daemon(p).await;

        let resp = execute_command(
            DaemonCommand::Stop {
                daemon: "zmc".to_string(),
                args: vec!["-m".to_string(), "1".to_string()],
            },
            &manager,
        )
        .await;
        assert!(resp.success, "{}", resp.message);
        assert_eq!(
            manager.daemon_state("zmc -m 1").await,
            Some(ProcessState::Stopped)
        );
    }

    #[tokio::test]
    async fn a_bound_socket_reads_as_live() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("zmdc.sock");
        assert!(!legacy_socket_is_live(&path).await);
        let _listener = UnixListener::bind(&path).unwrap();
        assert!(legacy_socket_is_live(&path).await);
    }

    /// ApplyState used to return a fake success ("... applied (stub)") while
    /// doing nothing. It now really applies via the DB; with no database
    /// configured it must report an honest error, never a false success.
    #[tokio::test]
    async fn test_apply_state_without_db_is_honest_error() {
        let config = DaemonConfig::default();
        let manager = Arc::new(DaemonManager::new(config, None));

        let resp = execute_command(
            DaemonCommand::ApplyState {
                state_name: "Night".to_string(),
            },
            &manager,
        )
        .await;
        assert!(!resp.success, "no DB configured must be a failure");
        assert!(
            !resp.message.contains("stub"),
            "must not return the old fake-success stub message: {}",
            resp.message
        );
    }

    #[tokio::test]
    async fn test_execute_command_version() {
        let config = DaemonConfig::default();
        let manager = Arc::new(DaemonManager::new(config, None));

        let resp = execute_command(DaemonCommand::Version, &manager).await;
        assert!(resp.success);
        assert!(!resp.message.is_empty());
    }

    #[tokio::test]
    async fn test_execute_command_startup_without_db() {
        // Without database, startup should fail with informative error
        let config = DaemonConfig::default();
        let manager = Arc::new(DaemonManager::new(config, None));

        let resp = execute_command(DaemonCommand::Startup, &manager).await;
        // Should fail because no database is configured
        assert!(!resp.success);
        assert!(resp.message.contains("Database not configured"));
    }

    #[tokio::test]
    async fn test_execute_command_status() {
        let config = DaemonConfig::default();
        let manager = Arc::new(DaemonManager::new(config, None));

        let resp = execute_command(DaemonCommand::Status { target: None }, &manager).await;
        assert!(resp.success);
        assert!(resp.data.is_some());
    }
}
