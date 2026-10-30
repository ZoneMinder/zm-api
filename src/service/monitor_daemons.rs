//! Keep a monitor's capture daemons in step with its row (#155).
//!
//! What ZoneMinder's own API does on add, edit, delete and state change:
//! start the daemons of a new monitor; stop them with the old row and start
//! them with the saved one after an edit; stop them before a delete. The
//! daemons are `zmc` and, for a controllable monitor, `zmcontrol.pl`.
//!
//! In takeover mode zm-api's daemon manager does it. In passive mode
//! ZoneMinder's zmdc.pl supervises, so the commands go to its `zmdc.sock`,
//! as ZoneMinder's web UI sends them. Failures are logged, not returned: the
//! row change has happened, and zmwatch (passive) or reconcile (takeover)
//! catches a daemon left in the wrong state.

use tracing::{info, warn};

use crate::daemon::manager::zmc_daemon_id;
use crate::daemon::zmdc_client;
use crate::entity::monitors::Model as MonitorModel;
use crate::entity::sea_orm_active_enums::{Capturing, MonitorType};
use crate::server::state::AppState;

/// A monitor state change requested through the API.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Start,
    Stop,
    Restart,
}

/// The zmdc specs of a monitor's daemons: `zmc`, then `zmcontrol.pl` when
/// the monitor is controllable.
pub fn daemon_specs(m: &MonitorModel) -> Vec<String> {
    specs_for(&m.r#type, &m.device, m.id, m.controllable != 0)
}

fn specs_for(t: &MonitorType, device: &str, id: u32, controllable: bool) -> Vec<String> {
    let mut specs = vec![zmc_daemon_id(t, device, id)];
    if controllable {
        specs.push(format!("zmcontrol.pl --id {id}"));
    }
    specs
}

/// Whether ZoneMinder would run daemons for this row at all.
pub fn should_run(m: &MonitorModel) -> bool {
    runs(&m.capturing, &m.r#type, m.deleted)
}

fn runs(capturing: &Capturing, t: &MonitorType, deleted: i8) -> bool {
    *capturing != Capturing::None && *t != MonitorType::WebSite && deleted == 0
}

/// Whether this host's zmdc.pl runs a monitor's daemons, by ZoneMinder's
/// rule (`Monitor::zmcControl`): every monitor on a host with no server id,
/// else only the monitors whose `ServerId` is this host's. Telling this
/// zmdc.pl to start another server's monitor would capture the camera
/// twice (#229).
pub fn runs_here(this_server: Option<u32>, monitor_server: Option<u32>) -> bool {
    match this_server {
        None => true,
        Some(here) => monitor_server == Some(here),
    }
}

async fn zmdc(state: &AppState, command: &str, m: &MonitorModel) {
    if !runs_here(state.server_id, m.server_id) {
        info!(
            "monitor {}: zmdc {command} skipped, Servers.Id {:?} runs its daemons",
            m.id, m.server_id
        );
        return;
    }
    let socket = state.config.daemon.socket_file();
    for spec in daemon_specs(m) {
        match zmdc_client::request(&socket, command, Some(&spec)).await {
            Ok(_) => info!("zmdc {command} {spec}"),
            Err(e) => warn!("zmdc {command} {spec} failed: {e}"),
        }
    }
}

async fn passive_start(state: &AppState, m: &MonitorModel) {
    if should_run(m) {
        zmdc(state, "start", m).await;
    }
}

fn log_manager(
    id: u32,
    what: &str,
    r: crate::error::AppResult<crate::daemon::ipc::DaemonResponse>,
) {
    match r {
        Ok(resp) if resp.success => info!("monitor {id}: {what}: {}", resp.message),
        Ok(resp) => warn!("monitor {id}: {what}: {}", resp.message),
        Err(e) => warn!("monitor {id}: {what} failed: {e}"),
    }
}

/// A monitor was created.
pub async fn created(state: &AppState, m: &MonitorModel) {
    match &state.daemon_manager {
        Some(dm) => log_manager(m.id, "start", dm.start_monitor(m.id).await),
        None => passive_start(state, m).await,
    }
}

/// A monitor was saved: stop its daemons as they were, start them as saved.
pub async fn updated(state: &AppState, old: &MonitorModel, new: &MonitorModel) {
    match &state.daemon_manager {
        Some(dm) => log_manager(new.id, "restart", dm.restart_monitor(new.id).await),
        None => {
            if old.r#type != MonitorType::WebSite {
                zmdc(state, "stop", old).await;
            }
            passive_start(state, new).await;
        }
    }
}

/// A monitor is about to be deleted.
pub async fn deleting(state: &AppState, m: &MonitorModel) {
    match &state.daemon_manager {
        Some(dm) => log_manager(m.id, "stop", dm.stop_monitor(m.id).await),
        None => zmdc(state, "stop", m).await,
    }
}

/// `PATCH /monitors/{id}/state`.
pub async fn set_state(state: &AppState, m: &MonitorModel, action: Action) {
    match (&state.daemon_manager, action) {
        (Some(dm), Action::Start) => log_manager(m.id, "start", dm.start_monitor(m.id).await),
        (Some(dm), Action::Stop) => log_manager(m.id, "stop", dm.stop_monitor(m.id).await),
        (Some(dm), Action::Restart) => log_manager(m.id, "restart", dm.restart_monitor(m.id).await),
        (None, Action::Start) => passive_start(state, m).await,
        (None, Action::Stop) => zmdc(state, "stop", m).await,
        (None, Action::Restart) => {
            zmdc(state, "stop", m).await;
            passive_start(state, m).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_this_servers_monitors_go_to_its_zmdc() {
        assert!(runs_here(None, None));
        assert!(
            runs_here(None, Some(2)),
            "no server id: everything is local"
        );
        assert!(runs_here(Some(1), Some(1)));
        assert!(!runs_here(Some(1), Some(2)));
        // ZoneMinder's zmcControl: with a server id, a monitor with no
        // ServerId isn't this host's either.
        assert!(!runs_here(Some(1), None));
        assert!(!runs_here(Some(1), Some(0)));
    }

    #[test]
    fn specs_are_zmc_then_zmcontrol_for_a_controllable_monitor() {
        assert_eq!(specs_for(&MonitorType::Ffmpeg, "", 3, false), ["zmc -m 3"]);
        assert_eq!(
            specs_for(&MonitorType::Ffmpeg, "", 3, true),
            ["zmc -m 3", "zmcontrol.pl --id 3"]
        );
        assert_eq!(
            specs_for(&MonitorType::Local, "/dev/video0", 3, false),
            ["zmc -d /dev/video0"]
        );
    }

    #[test]
    fn only_capturing_live_non_website_monitors_run() {
        assert!(runs(&Capturing::Always, &MonitorType::Ffmpeg, 0));
        assert!(!runs(&Capturing::None, &MonitorType::Ffmpeg, 0));
        assert!(!runs(&Capturing::Always, &MonitorType::WebSite, 0));
        assert!(!runs(&Capturing::Always, &MonitorType::Ffmpeg, 1));
    }
}
