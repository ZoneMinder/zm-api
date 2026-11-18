//! On-demand work from a running zm-next worker: take a snapshot now, describe
//! the scene now. Both go over the monitor's stream socket through
//! [`crate::streaming::source::SourceRouter::send_command`].

use std::path::Path;
use std::time::Duration;

use serde_json::{json, Value};

use crate::dto::response::monitor_ondemand::DescribeNowResponse;
use crate::error::{AppError, AppResult};
use crate::server::state::AppState;
use crate::service::monitor_acl::MonitorScope;
use crate::streaming::source::command::CommandError;

/// A JPEG the worker just wrote, plus what it reported about it.
#[derive(Debug)]
pub struct SnapshotNow {
    pub jpeg: Vec<u8>,
    pub path: String,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub pts_usec: Option<i64>,
}

/// Ask the worker's `store_snapshot` for a JPEG of the current frame.
pub async fn snapshot(
    state: &AppState,
    monitor_id: u32,
    scope: &MonitorScope,
    stream_id: Option<u32>,
) -> AppResult<SnapshotNow> {
    let mut command = json!({ "cmd": "snapshot_now" });
    if let Some(s) = stream_id {
        command["stream_id"] = s.into();
    }
    let timeout = Duration::from_secs(state.config.zmnext.commands.snapshot_timeout_secs);
    let detail = send(state, monitor_id, scope, command, timeout).await?;
    snapshot_from_detail(&detail).await
}

/// Ask the worker's `describe_vlm` to describe the current scene.
pub async fn describe(
    state: &AppState,
    monitor_id: u32,
    scope: &MonitorScope,
    prompt: Option<String>,
    stream_id: Option<u32>,
) -> AppResult<DescribeNowResponse> {
    let mut command = json!({ "cmd": "describe_now" });
    if let Some(p) = prompt.filter(|p| !p.trim().is_empty()) {
        command["prompt"] = p.into();
    }
    if let Some(s) = stream_id {
        command["stream_id"] = s.into();
    }
    let timeout = Duration::from_secs(state.config.zmnext.commands.describe_timeout_secs);
    let detail = send(state, monitor_id, scope, command, timeout).await?;
    Ok(describe_from_detail(monitor_id, &detail))
}

async fn send(
    state: &AppState,
    monitor_id: u32,
    scope: &MonitorScope,
    command: Value,
    timeout: Duration,
) -> AppResult<Value> {
    // 404 for a missing or hidden monitor, before anything else.
    crate::service::monitor::get_by_id(state, monitor_id, scope).await?;
    if !state.config.zmnext.enabled
        || !crate::repo::monitors::use_zmnext(state.db(), monitor_id).await
    {
        return Err(AppError::ConflictError(format!(
            "monitor {monitor_id} is not running on zm-next; on-demand snapshots and \
             descriptions need UseZmNext set and [zmnext].enabled"
        )));
    }
    let router = state.source_router.as_ref().ok_or_else(|| {
        AppError::ServiceUnavailableError(
            "live streaming is disabled, so there is no worker connection".into(),
        )
    })?;
    let _slot = IN_FLIGHT
        .try_acquire(monitor_id, MAX_IN_FLIGHT_PER_MONITOR)
        .ok_or_else(|| {
            AppError::ServiceUnavailableError(format!(
                "monitor {monitor_id}'s worker is already answering \
                 {MAX_IN_FLIGHT_PER_MONITOR} requests; try again shortly"
            ))
        })?;
    router
        .send_command(monitor_id, command, timeout)
        .await
        .map_err(command_error)
}

/// The most on-demand commands one monitor's worker is sent at a time. Anyone
/// who may view the monitor can ask for a description, which runs a language
/// model on the worker for up to the describe timeout. Unlimited, a viewer in
/// a loop would keep the worker's model busy and starve its own detection.
const MAX_IN_FLIGHT_PER_MONITOR: usize = 2;

static IN_FLIGHT: once_cell::sync::Lazy<InFlight> = once_cell::sync::Lazy::new(InFlight::default);

/// How many commands each monitor's worker is answering.
#[derive(Default)]
struct InFlight(std::sync::Mutex<std::collections::HashMap<u32, usize>>);

/// One command in flight, counted until dropped.
struct InFlightSlot<'a> {
    counts: &'a InFlight,
    monitor_id: u32,
}

impl InFlight {
    fn try_acquire(&self, monitor_id: u32, limit: usize) -> Option<InFlightSlot<'_>> {
        let mut counts = self.0.lock().unwrap_or_else(|p| p.into_inner());
        let held = counts.entry(monitor_id).or_insert(0);
        if *held >= limit {
            return None;
        }
        *held += 1;
        Some(InFlightSlot {
            counts: self,
            monitor_id,
        })
    }
}

impl Drop for InFlightSlot<'_> {
    fn drop(&mut self) {
        let mut counts = self.counts.0.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(held) = counts.get_mut(&self.monitor_id) {
            *held -= 1;
            if *held == 0 {
                counts.remove(&self.monitor_id);
            }
        }
    }
}

fn command_error(e: CommandError) -> AppError {
    // All of these mean the worker, not the caller, couldn't deliver.
    AppError::ServiceUnavailableError(e.to_string())
}

async fn snapshot_from_detail(detail: &Value) -> AppResult<SnapshotNow> {
    let path = detail
        .get("path")
        .and_then(Value::as_str)
        .filter(|p| !p.is_empty())
        .ok_or_else(|| AppError::InternalServerError("snapshot result carried no path".into()))?;
    // The worker is a local process zm-api spawned, so its path is trusted to
    // be on this host; still refuse to serve anything that isn't a JPEG.
    let is_jpeg = Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("jpg") || e.eq_ignore_ascii_case("jpeg"));
    if !is_jpeg {
        return Err(AppError::InternalServerError(format!(
            "snapshot result path is not a JPEG: {path}"
        )));
    }
    let jpeg = tokio::fs::read(path).await.map_err(|e| {
        AppError::ServiceUnavailableError(format!("worker wrote {path} but it can't be read: {e}"))
    })?;
    Ok(SnapshotNow {
        jpeg,
        path: path.to_string(),
        width: u32_field(detail, "width"),
        height: u32_field(detail, "height"),
        pts_usec: detail.get("pts_usec").and_then(Value::as_i64),
    })
}

fn describe_from_detail(monitor_id: u32, detail: &Value) -> DescribeNowResponse {
    let str_field = |k: &str| detail.get(k).and_then(Value::as_str).map(str::to_string);
    DescribeNowResponse {
        monitor_id,
        text: str_field("text").unwrap_or_default(),
        prompt: str_field("prompt"),
        model: str_field("model"),
        frames: u32_field(detail, "frames"),
        pts_usec: detail.get("pts_usec").and_then(Value::as_i64),
        stream_id: u32_field(detail, "stream_id"),
    }
}

fn u32_field(detail: &Value, key: &str) -> Option<u32> {
    detail
        .get(key)
        .and_then(Value::as_u64)
        .and_then(|v| u32::try_from(v).ok())
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_monitors_worker_is_sent_only_so_many_commands_at_once() {
        let counts = super::InFlight::default();
        let a = counts.try_acquire(7, 2).unwrap();
        let _b = counts.try_acquire(7, 2).unwrap();
        assert!(counts.try_acquire(7, 2).is_none());
        // Another monitor has its own allowance.
        assert!(counts.try_acquire(8, 2).is_some());
        // A finished command frees its place.
        drop(a);
        assert!(counts.try_acquire(7, 2).is_some());
    }

    use super::*;

    #[tokio::test]
    async fn snapshot_detail_reads_the_jpeg_it_names() {
        let dir = std::env::temp_dir().join(format!("zm_ondemand_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("snap.jpg");
        std::fs::write(&file, [0xFF, 0xD8, 0xFF, 0xD9]).unwrap();

        let detail = json!({
            "event": "EventSnapshot", "path": file.to_string_lossy(),
            "width": 1280, "height": 720, "pts_usec": 4666667, "ok": true,
        });
        let snap = snapshot_from_detail(&detail).await.unwrap();
        assert_eq!(snap.jpeg, vec![0xFF, 0xD8, 0xFF, 0xD9]);
        assert_eq!((snap.width, snap.height), (Some(1280), Some(720)));
        assert_eq!(snap.pts_usec, Some(4666667));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn snapshot_detail_refuses_non_jpeg_and_missing_paths() {
        assert!(snapshot_from_detail(&json!({"path": "/etc/passwd"}))
            .await
            .is_err());
        assert!(snapshot_from_detail(&json!({"ok": true})).await.is_err());
        let missing = snapshot_from_detail(&json!({"path": "/nonexistent/zm/a.jpg"})).await;
        assert!(matches!(missing, Err(AppError::ServiceUnavailableError(_))));
    }

    #[test]
    fn describe_detail_maps_fields() {
        let detail = json!({
            "type": "description", "text": "A person at the door.", "prompt": "p",
            "model": "qwen", "frames": 3, "pts_usec": 42, "stream_id": 1,
            "on_demand": true, "ok": true, "request_id": 9,
        });
        let r = describe_from_detail(5, &detail);
        assert_eq!(r.monitor_id, 5);
        assert_eq!(r.text, "A person at the door.");
        assert_eq!(r.model.as_deref(), Some("qwen"));
        assert_eq!(r.frames, Some(3));
        assert_eq!(r.stream_id, Some(1));
    }
}
