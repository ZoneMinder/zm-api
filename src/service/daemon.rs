//! Daemon controller service layer.

use sea_orm::{ActiveModelTrait, ColumnTrait, EntityTrait, QueryFilter};
use tracing::{debug, info, warn};

use crate::daemon::ipc::DaemonResponse;
use crate::dto::response::daemon::{
    DaemonActionResponse, DaemonListResponse, DaemonStatusResponse, SystemStatusResponse,
};
use crate::entity::monitors;
use crate::entity::sea_orm_active_enums::{Analysing, Capturing, Function, Recording};
use crate::error::{AppError, AppResult};
use crate::server::state::AppState;

// Passive mode (no daemon manager of our own): ZoneMinder's zmdc.pl supervises
// the daemons, and these read and control them through its `zmdc.sock`, the
// way ZoneMinder's web UI does (#155, #156).

/// ZoneMinder's `zmdc.sock` (`ZM_PATH_SOCKS`, from zm.conf).
fn zmdc_socket(state: &AppState) -> std::path::PathBuf {
    state.config.daemon.socket_file()
}

/// What zmdc.pl reports it supervises.
async fn zmdc_status(state: &AppState) -> AppResult<Vec<crate::daemon::ipc::ProcessStatus>> {
    let reply = crate::daemon::zmdc_client::request(&zmdc_socket(state), "status", None)
        .await
        .map_err(|e| {
            AppError::ServiceUnavailableError(format!("ZoneMinder's zmdc.sock is unreachable: {e}"))
        })?;
    Ok(crate::daemon::zmdc_client::parse_status(
        &reply,
        chrono::Local::now().naive_local(),
    ))
}

/// Ask zmdc.pl to `start`/`stop`/`restart`/`reload` the daemon `id`
/// (`zmc -m 1`). The spec is checked against the same allowlist zm-api's own
/// supervisor uses before it reaches zmdc.pl.
async fn zmdc_control(
    state: &AppState,
    command: &str,
    id: &str,
) -> AppResult<DaemonActionResponse> {
    let mut words = id.split_whitespace();
    let daemon = words.next().unwrap_or_default();
    let args: Vec<String> = words.map(str::to_string).collect();
    crate::daemon::manager::validate_daemon_spec(daemon, &args)
        .map_err(|e| AppError::BadRequestError(format!("Invalid daemon spec: {e}")))?;
    let reply = crate::daemon::zmdc_client::request(&zmdc_socket(state), command, Some(id))
        .await
        .map_err(|e| {
            AppError::ServiceUnavailableError(format!("ZoneMinder's zmdc.sock is unreachable: {e}"))
        })?;
    Ok(zmdc_reply_to_action(&reply, command, id))
}

/// zmdc.pl answers a command with the log lines it wrote for it; its
/// failures read "Invalid daemon …", "Can't find …" or "No process …". A
/// `restart` of a process it doesn't know prints "Can't find …" and then
/// starts it, so a start line overrides those.
fn zmdc_reply_to_action(reply: &str, command: &str, id: &str) -> DaemonActionResponse {
    let text = reply.trim();
    let complained = text.lines().any(|l| {
        let l = l.trim_start();
        l.starts_with("Invalid") || l.starts_with("Can't") || l.starts_with("No process")
    });
    let started = text.contains(" starting at ") || text.contains(" started at ");
    let failed = complained && !started;
    let message = if text.is_empty() {
        format!("{command} {id} sent to zmdc")
    } else {
        text.to_string()
    };
    if failed {
        DaemonActionResponse::error(message)
    } else {
        DaemonActionResponse::success(message)
    }
}

/// Get the status of all daemons.
pub async fn get_all_daemons(state: &AppState) -> AppResult<DaemonListResponse> {
    let Some(manager) = state.daemon_manager.as_ref() else {
        return Ok(DaemonListResponse {
            daemons: zmdc_status(state)
                .await?
                .into_iter()
                .map(Into::into)
                .collect(),
        });
    };

    let status = manager.get_status().await;
    Ok(DaemonListResponse {
        daemons: status.daemons.into_iter().map(Into::into).collect(),
    })
}

/// Get the status of a specific daemon.
pub async fn get_daemon(state: &AppState, id: &str) -> AppResult<DaemonStatusResponse> {
    let found = match state.daemon_manager.as_ref() {
        Some(manager) => manager.get_daemon_status(id).await,
        None => zmdc_status(state).await?.into_iter().find(|p| p.id == id),
    };
    found.map(Into::into).ok_or_else(|| {
        AppError::NotFoundError(crate::error::Resource {
            resource_type: crate::error::ResourceType::Config,
            details: vec![("daemon_id".to_string(), id.to_string())],
        })
    })
}

/// Start a daemon.
pub async fn start_daemon(
    state: &AppState,
    id: &str,
    args: &[String],
) -> AppResult<DaemonActionResponse> {
    // Refuse a command that isn't one of ZoneMinder's daemons with 400, not a
    // 200 that says it failed.
    if let Err(reason) = crate::daemon::manager::check_daemon_spec(id, args) {
        return Err(AppError::BadRequestError(format!(
            "Invalid daemon spec: {reason}"
        )));
    }
    let Some(manager) = state.daemon_manager.as_ref() else {
        let spec = std::iter::once(id)
            .chain(args.iter().map(String::as_str))
            .collect::<Vec<_>>()
            .join(" ");
        return zmdc_control(state, "start", &spec).await;
    };

    info!("Starting daemon: {} with args: {:?}", id, args);
    let resp = manager.start_daemon(id, args).await?;
    Ok(response_to_action(resp))
}

/// Stop a daemon.
pub async fn stop_daemon(state: &AppState, id: &str) -> AppResult<DaemonActionResponse> {
    let Some(manager) = state.daemon_manager.as_ref() else {
        return zmdc_control(state, "stop", id).await;
    };

    info!("Stopping daemon: {}", id);
    let resp = manager.stop_daemon(id).await?;
    Ok(response_to_action(resp))
}

/// Restart a daemon.
pub async fn restart_daemon(state: &AppState, id: &str) -> AppResult<DaemonActionResponse> {
    let Some(manager) = state.daemon_manager.as_ref() else {
        return zmdc_control(state, "restart", id).await;
    };

    info!("Restarting daemon: {}", id);
    // Pass empty args - daemon ID (e.g., "zmc -m 1") contains the args
    let resp = manager.restart_daemon(id, &[]).await?;
    Ok(response_to_action(resp))
}

/// Reload a daemon (SIGHUP).
pub async fn reload_daemon(state: &AppState, id: &str) -> AppResult<DaemonActionResponse> {
    let Some(manager) = state.daemon_manager.as_ref() else {
        return zmdc_control(state, "reload", id).await;
    };

    info!("Reloading daemon: {}", id);
    let resp = manager.reload_daemon(id).await?;
    Ok(response_to_action(resp))
}

/// Get system status.
pub async fn get_system_status(state: &AppState) -> AppResult<SystemStatusResponse> {
    let status = match state.daemon_manager.as_ref() {
        Some(manager) => manager.get_status().await,
        // Passive: ZoneMinder's daemons, and this host's load, memory and
        // disk, which need no daemon manager. `running` is whether zmdc.pl
        // answered.
        None => {
            let daemons = zmdc_status(state).await;
            crate::daemon::ipc::SystemStatus {
                running: daemons.is_ok(),
                daemons: daemons.unwrap_or_default(),
                stats: crate::daemon::stats::collect_stats().ok(),
            }
        }
    };
    let mut response: SystemStatusResponse = status.into();
    // Live DB pool usage (the legacy header's `DB: n/max`). Read here rather
    // than in the IPC stats struct, which only carries OS metrics.
    if let Some(stats) = response.stats.as_mut() {
        let (used, max) = db_pool_usage(state);
        stats.db_connections = used;
        stats.db_max_connections = max;
    }
    Ok(response)
}

/// Current and maximum database connections, if the pool can be inspected.
fn db_pool_usage(state: &AppState) -> (Option<u32>, Option<u32>) {
    let pool = state.db().get_mysql_connection_pool();
    (Some(pool.size()), Some(state.config.db.max_connections))
}

/// Perform full system startup.
///
/// This delegates to DaemonManager.start_all_daemons() which:
/// 1. Queries monitors (Capturing != None, not WebSite type)
/// 2. For Local type: starts `zmc -d <device>`
/// 3. For other types: starts `zmc -m <id>`
/// 4. Starts zma for monitors with Modect/Mocord function
/// 5. Starts zmcontrol.pl for controllable monitors
/// 6. Starts zmtrack.pl for monitors with motion tracking
/// 7. Starts singleton daemons (zmfilter.pl, zmstats.pl, etc.)
pub async fn system_startup(state: &AppState) -> AppResult<DaemonActionResponse> {
    let manager = state
        .daemon_manager
        .as_ref()
        .ok_or_else(|| AppError::ServiceUnavailableError("Daemon manager not available".into()))?;

    info!("Starting ZoneMinder system");

    // Delegate to manager which has full startup logic matching zmpkg.pl
    let resp = manager.start_all_daemons().await?;
    Ok(response_to_action(resp))
}

/// Perform full system shutdown.
pub async fn system_shutdown(state: &AppState) -> AppResult<DaemonActionResponse> {
    let manager = state
        .daemon_manager
        .as_ref()
        .ok_or_else(|| AppError::ServiceUnavailableError("Daemon manager not available".into()))?;

    info!("Shutting down all daemons");
    let resp = manager.shutdown_all().await?;
    Ok(response_to_action(resp))
}

/// Perform full system restart (stop all, then start all).
pub async fn system_restart(state: &AppState) -> AppResult<DaemonActionResponse> {
    info!("Restarting ZoneMinder system");

    // First shutdown
    let shutdown_result = system_shutdown(state).await;
    if let Err(e) = &shutdown_result {
        warn!("Shutdown phase had issues: {}", e);
    }

    // Small delay to let processes fully terminate
    tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;

    // Then startup
    let startup_result = system_startup(state).await?;

    Ok(DaemonActionResponse::success(format!(
        "System restarted: {}",
        startup_result.message
    )))
}

/// Trigger log rotation for all daemons (SIGHUP).
pub async fn system_logrot(state: &AppState) -> AppResult<DaemonActionResponse> {
    let manager = state
        .daemon_manager
        .as_ref()
        .ok_or_else(|| AppError::ServiceUnavailableError("Daemon manager not available".into()))?;

    info!("Triggering log rotation for all daemons");

    let daemon_ids = manager.list_daemon_ids().await;
    let mut reloaded = 0;
    let mut failed = 0;

    for id in daemon_ids {
        // SIGWINCH (reopen logs), not SIGHUP (reload, which restarts
        // capture) — see DaemonManager::logrot_daemon (#86).
        match manager.logrot_daemon(&id).await {
            Ok(resp) if resp.success => {
                reloaded += 1;
                debug!("Sent SIGWINCH to {}", id);
            }
            Ok(resp) => {
                // Daemon might not be running
                debug!("Could not reload {}: {}", id, resp.message);
            }
            Err(e) => {
                failed += 1;
                warn!("Error reloading {}: {}", id, e);
            }
        }
    }

    Ok(DaemonActionResponse::success(format!(
        "Log rotation triggered: {} daemons signaled, {} failed",
        reloaded, failed
    )))
}

/// Apply a run state's definition to the monitor rows and flip `IsActive`, all
/// in one transaction; returns the number of monitors updated.
///
/// The `States.Definition` is a comma list of per-monitor entries, in one of
/// two formats:
/// - Legacy 3-part `id:function:enabled` — sets Function + Capturing.
/// - New 4-part `id:capturing:analysing:recording` — sets those three.
///
/// This is the shared DB-side of applying a state, used by both the HTTP
/// handler (`apply_state`) and the legacy IPC `ApplyState` command
/// (`DaemonManager::apply_state`). No daemon restart or run-mode logic lives
/// here — callers decide that.
pub async fn apply_state_to_db(
    db: &sea_orm::DatabaseConnection,
    state_name: &str,
) -> AppResult<u32> {
    use crate::entity::states;
    use sea_orm::ActiveValue::Set;

    info!("Applying state to database: {}", state_name);

    // Look up the state by name
    let zm_state = states::Entity::find()
        .filter(states::Column::Name.eq(state_name))
        .one(db)
        .await?
        .ok_or_else(|| {
            AppError::NotFoundError(crate::error::Resource {
                resource_type: crate::error::ResourceType::Config,
                details: vec![("state_name".to_string(), state_name.to_string())],
            })
        })?;

    // Parse the state definition
    // Two formats supported:
    // - Legacy (3-part): "id:function:enabled" e.g., "1:Monitor:1,2:Modect:1,3:None:0"
    // - New (4-part): "id:capturing:analysing:recording" e.g., "1:Always:Always:OnMotion"
    // Apply the monitor config changes, activate this state, and deactivate
    // every other state as ONE atomic unit. A partial failure here previously
    // left monitors half-migrated between run states with inconsistent IsActive
    // flags. The daemon restart stays OUTSIDE the transaction — it is an
    // external side effect and runs only after the DB changes commit.
    // REVIEW_FIXES_PLAN §4.2.
    use sea_orm::sea_query::Expr;
    use sea_orm::TransactionTrait;

    let state_name_owned = state_name.to_string();
    let updated_monitors = db
        .transaction::<_, u32, AppError>(move |txn| {
            Box::pin(async move {
                let definition = zm_state.definition.clone();
                let mut updated_monitors = 0u32;

                for entry in definition.split(',') {
                    let entry = entry.trim();
                    if entry.is_empty() {
                        continue;
                    }

                    let parts: Vec<&str> = entry.split(':').collect();

                    match parts.len() {
                        3 => {
                            // Legacy format: id:function:enabled
                            let monitor_id = match parts[0].parse::<u32>() {
                                Ok(id) => id,
                                Err(_) => {
                                    warn!("Invalid monitor ID in state definition: {}", parts[0]);
                                    continue;
                                }
                            };

                            let function = match parse_function(parts[1]) {
                                Some(f) => f,
                                None => {
                                    warn!(
                                        "Unknown function '{}' for monitor {}",
                                        parts[1], monitor_id
                                    );
                                    continue;
                                }
                            };

                            let enabled: u8 = parts[2].parse().unwrap_or(1);

                            // Map legacy enabled field to Capturing:
                            // enabled=0 → Capturing::None (disabled)
                            // enabled=1 → Capturing::Always (default active state)
                            let capturing = if enabled == 0 {
                                Capturing::None
                            } else {
                                Capturing::Always
                            };

                            // Update monitor in database
                            let monitor = monitors::Entity::find_by_id(monitor_id).one(txn).await?;

                            if let Some(monitor) = monitor {
                                let mut active: monitors::ActiveModel = monitor.into();
                                active.function = Set(function.clone());
                                active.capturing = Set(capturing.clone());
                                active.update(txn).await?;
                                updated_monitors += 1;
                                debug!(
                                    "Updated monitor {} (legacy): function={:?}, capturing={:?}",
                                    monitor_id, function, capturing
                                );
                            }
                        }
                        4 => {
                            // New format: id:capturing:analysing:recording
                            let monitor_id = match parts[0].parse::<u32>() {
                                Ok(id) => id,
                                Err(_) => {
                                    warn!("Invalid monitor ID in state definition: {}", parts[0]);
                                    continue;
                                }
                            };

                            let capturing = match parse_capturing(parts[1]) {
                                Some(c) => c,
                                None => {
                                    warn!(
                                        "Unknown capturing value '{}' for monitor {}",
                                        parts[1], monitor_id
                                    );
                                    continue;
                                }
                            };

                            let analysing = match parse_analysing(parts[2]) {
                                Some(a) => a,
                                None => {
                                    warn!(
                                        "Unknown analysing value '{}' for monitor {}",
                                        parts[2], monitor_id
                                    );
                                    continue;
                                }
                            };

                            let recording = match parse_recording(parts[3]) {
                                Some(r) => r,
                                None => {
                                    warn!(
                                        "Unknown recording value '{}' for monitor {}",
                                        parts[3], monitor_id
                                    );
                                    continue;
                                }
                            };

                            // Update monitor in database
                            let monitor = monitors::Entity::find_by_id(monitor_id).one(txn).await?;

                            if let Some(monitor) = monitor {
                                let mut active: monitors::ActiveModel = monitor.into();
                                active.capturing = Set(capturing.clone());
                                active.analysing = Set(analysing.clone());
                                active.recording = Set(recording.clone());
                                active.update(txn).await?;
                                updated_monitors += 1;
                                debug!(
                        "Updated monitor {} (new): capturing={:?}, analysing={:?}, recording={:?}",
                        monitor_id, capturing, analysing, recording
                    );
                            }
                        }
                        _ => {
                            if !entry.is_empty() {
                                warn!(
                                    "Invalid state definition entry (expected 3 or 4 parts): {}",
                                    entry
                                );
                            }
                            continue;
                        }
                    }
                }

                // Mark this state as active.
                let mut active_state: states::ActiveModel = zm_state.into();
                active_state.is_active = Set(1);
                active_state.update(txn).await?;

                // Deactivate every other state in a single UPDATE (no N+1 loop).
                states::Entity::update_many()
                    .col_expr(states::Column::IsActive, Expr::value(0u8))
                    .filter(states::Column::Name.ne(state_name_owned.as_str()))
                    .exec(txn)
                    .await?;

                Ok(updated_monitors)
            })
        })
        .await
        .map_err(|e| match e {
            sea_orm::TransactionError::Connection(db) => AppError::from(db),
            sea_orm::TransactionError::Transaction(app) => app,
        })?;

    Ok(updated_monitors)
}

/// Apply a named run state over the HTTP API.
///
/// Reconfigures monitors + activates the state via [`apply_state_to_db`], then
/// decides whether to restart daemons based on run mode:
/// - Takeover/active mode (daemon manager present): zm-api supervises the
///   ZoneMinder daemons, so restart them to pick up the new monitor config.
/// - Passive mode (no daemon manager, the default): ZoneMinder's zmdc.pl
///   supervises, and it doesn't watch the database. Each monitor whose modes
///   the state changed gets what an edit sends zmdc.pl, stop then start
///   (#204), in the background so a large state doesn't hold the request
///   open. There is no whole-system restart here: `system_restart` needs the
///   daemon manager.
pub async fn apply_state(state: &AppState, state_name: &str) -> AppResult<DaemonActionResponse> {
    // Passive mode moves only the monitors the state changes, so it needs
    // their rows from before.
    let before = if state.daemon_manager.is_none() {
        monitors::Entity::find().all(state.db.as_ref()).await?
    } else {
        Vec::new()
    };
    let updated_monitors = apply_state_to_db(state.db.as_ref(), state_name).await?;

    if state.daemon_manager.is_some() {
        info!(
            "State '{}' applied to {} monitors, restarting supervised daemons",
            state_name, updated_monitors
        );
        // The state is committed either way; a failed restart is reported,
        // not turned into a 500 that reads as "nothing happened" (#117).
        match system_restart(state).await {
            Ok(restart) => Ok(DaemonActionResponse::success(format!(
                "State '{}' applied: {} monitors updated. {}",
                state_name, updated_monitors, restart.message
            ))),
            Err(e) => {
                warn!("state '{state_name}' applied but the daemon restart failed: {e}");
                Ok(DaemonActionResponse::error(format!(
                    "State '{}' applied: {} monitors updated, but the daemon restart failed: {e}",
                    state_name, updated_monitors
                )))
            }
        }
    } else {
        // ZoneMinder doesn't pick the change up from the database by itself
        // (#204). Each monitor whose modes changed gets what an edit sends
        // zmdc.pl: stop as it was, start as it is now.
        let after = monitors::Entity::find().all(state.db.as_ref()).await?;
        let moved: Vec<(monitors::Model, monitors::Model)> = changed_modes(&before, &after)
            .into_iter()
            .map(|(old, new)| (old.clone(), new.clone()))
            .collect();
        let count = moved.len();
        // Each zmdc call can take seconds; don't make the caller wait for
        // all of them.
        let bg = state.clone();
        tokio::spawn(async move {
            for (old, new) in &moved {
                crate::service::monitor_daemons::updated(&bg, old, new).await;
            }
        });
        info!(
            "State '{}' applied to {} monitors; restarting the daemons of {} through zmdc.pl",
            state_name, updated_monitors, count
        );
        Ok(DaemonActionResponse::success(format!(
            "State '{}' applied: {} monitors updated, {} with daemons restarting",
            state_name, updated_monitors, count
        )))
    }
}

/// The monitors whose Capturing, Analysing or Recording differ between two
/// reads, as (before, after).
fn changed_modes<'a>(
    before: &'a [monitors::Model],
    after: &'a [monitors::Model],
) -> Vec<(&'a monitors::Model, &'a monitors::Model)> {
    after
        .iter()
        .filter_map(|new| {
            let old = before.iter().find(|m| m.id == new.id)?;
            (old.capturing != new.capturing
                || old.analysing != new.analysing
                || old.recording != new.recording)
                .then_some((old, new))
        })
        .collect()
}

/// Parse a function string to the Function enum.
fn parse_function(s: &str) -> Option<Function> {
    match s {
        "None" => Some(Function::None),
        "Monitor" => Some(Function::Monitor),
        "Modect" => Some(Function::Modect),
        "Record" => Some(Function::Record),
        "Mocord" => Some(Function::Mocord),
        "Nodect" => Some(Function::Nodect),
        _ => None,
    }
}

/// Parse a capturing string to the Capturing enum.
fn parse_capturing(s: &str) -> Option<Capturing> {
    match s {
        "None" => Some(Capturing::None),
        "Ondemand" => Some(Capturing::Ondemand),
        "Always" => Some(Capturing::Always),
        _ => None,
    }
}

/// Parse an analysing string to the Analysing enum.
fn parse_analysing(s: &str) -> Option<Analysing> {
    match s {
        "None" => Some(Analysing::None),
        "Always" => Some(Analysing::Always),
        _ => None,
    }
}

/// Parse a recording string to the Recording enum.
fn parse_recording(s: &str) -> Option<Recording> {
    match s {
        "None" => Some(Recording::None),
        "OnMotion" => Some(Recording::OnMotion),
        "Always" => Some(Recording::Always),
        _ => None,
    }
}

/// Ensure the States table is sane.
///
/// This function ensures:
/// 1. A "default" state exists (creates one if missing)
/// 2. Exactly one state is marked as active (fixes if not)
///
/// This matches the behavior of zmpkg.pl which ensures the states table
/// has a default state and exactly one active state at system startup.
pub async fn ensure_state_sanity(db: &sea_orm::DatabaseConnection) -> AppResult<()> {
    use crate::entity::states;
    use sea_orm::{ActiveModelTrait, ActiveValue::Set, QueryOrder};

    info!("Checking state table sanity");

    // Step 1: Ensure "default" state exists
    let default_states: Vec<states::Model> = states::Entity::find()
        .filter(states::Column::Name.eq("default"))
        .order_by_asc(states::Column::Id)
        .all(db)
        .await?;

    // Remove duplicate "default" states if any (keep the first one)
    if default_states.len() > 1 {
        warn!(
            "Found {} duplicate 'default' states, removing extras",
            default_states.len() - 1
        );
        for state in default_states.iter().skip(1) {
            states::Entity::delete_by_id(state.id).exec(db).await?;
        }
    }

    // Create default state if missing
    let default_state = if default_states.is_empty() {
        info!("Creating missing 'default' state");
        let new_default = states::ActiveModel {
            name: Set("default".to_string()),
            definition: Set(String::new()),
            is_active: Set(1),
            ..Default::default()
        };
        new_default.insert(db).await?
    } else {
        default_states.into_iter().next().unwrap()
    };

    // Step 2: Ensure exactly one active state
    let active_states: Vec<states::Model> = states::Entity::find()
        .filter(states::Column::IsActive.eq(1u8))
        .all(db)
        .await?;

    let active_count = active_states.len();
    if active_count != 1 {
        info!("Found {} active states (expected 1), fixing", active_count);

        // Reset all states to inactive
        use sea_orm::sea_query::Expr;
        states::Entity::update_many()
            .col_expr(states::Column::IsActive, Expr::value(0u8))
            .exec(db)
            .await?;

        // Set default state as active
        let mut active_default: states::ActiveModel = default_state.into();
        active_default.is_active = Set(1);
        active_default.update(db).await?;

        info!("Set 'default' state as the active state");
    }

    debug!("State table sanity check complete");
    Ok(())
}

/// Convert daemon response to action response.
fn response_to_action(resp: DaemonResponse) -> DaemonActionResponse {
    if resp.success {
        DaemonActionResponse::success(resp.message)
    } else {
        DaemonActionResponse::error(resp.message)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zmdc_failures_are_reported_as_failures() {
        assert!(
            zmdc_reply_to_action(
                "'zmc -m 1' starting at 26/10/18 10:00:00, pid = 3\n",
                "start",
                "zmc -m 1"
            )
            .success
        );
        assert!(zmdc_reply_to_action("", "stop", "zmc -m 1").success);
        for bad in [
            "No process with command of 'zmc -m 9' is running\n",
            "Can't find process with command of 'zmc -m 9'\n",
            "Invalid daemon 'x' specified\n",
        ] {
            assert!(
                !zmdc_reply_to_action(bad, "stop", "zmc -m 9").success,
                "{bad}"
            );
        }
        // restart of an unknown process starts it.
        assert!(zmdc_reply_to_action(
            "Can't find process with command of 'zmc -m 9'\n'zmc -m 9' starting at 26/10/18 10:00:00\n",
            "restart",
            "zmc -m 9"
        )
        .success);
    }

    #[test]
    fn test_response_to_action_success() {
        let resp = DaemonResponse {
            success: true,
            message: "OK".to_string(),
            data: None,
            legacy_text: None,
        };
        let action = response_to_action(resp);
        assert!(action.success);
        assert_eq!(action.message, "OK");
    }

    #[test]
    fn test_response_to_action_error() {
        let resp = DaemonResponse {
            success: false,
            message: "Failed".to_string(),
            data: None,
            legacy_text: None,
        };
        let action = response_to_action(resp);
        assert!(!action.success);
        assert_eq!(action.message, "Failed");
    }

    #[test]
    fn test_parse_function() {
        assert_eq!(parse_function("None"), Some(Function::None));
        assert_eq!(parse_function("Monitor"), Some(Function::Monitor));
        assert_eq!(parse_function("Modect"), Some(Function::Modect));
        assert_eq!(parse_function("Record"), Some(Function::Record));
        assert_eq!(parse_function("Mocord"), Some(Function::Mocord));
        assert_eq!(parse_function("Nodect"), Some(Function::Nodect));
        assert_eq!(parse_function("invalid"), None);
        assert_eq!(parse_function(""), None);
    }

    #[test]
    fn test_parse_capturing() {
        assert_eq!(parse_capturing("None"), Some(Capturing::None));
        assert_eq!(parse_capturing("Ondemand"), Some(Capturing::Ondemand));
        assert_eq!(parse_capturing("Always"), Some(Capturing::Always));
        assert_eq!(parse_capturing("invalid"), None);
        assert_eq!(parse_capturing(""), None);
    }

    #[test]
    fn test_parse_analysing() {
        assert_eq!(parse_analysing("None"), Some(Analysing::None));
        assert_eq!(parse_analysing("Always"), Some(Analysing::Always));
        assert_eq!(parse_analysing("invalid"), None);
        assert_eq!(parse_analysing(""), None);
    }

    #[test]
    fn test_parse_recording() {
        assert_eq!(parse_recording("None"), Some(Recording::None));
        assert_eq!(parse_recording("OnMotion"), Some(Recording::OnMotion));
        assert_eq!(parse_recording("Always"), Some(Recording::Always));
        assert_eq!(parse_recording("invalid"), None);
        assert_eq!(parse_recording(""), None);
    }
}
