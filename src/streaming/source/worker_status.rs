//! Live status of a zm-next worker, folded from its hello and status EVENTs.
//!
//! Sources (zm-next `docs/Worker_Control_Protocol.md`, "Worker hello" and
//! "Status"):
//!
//! * the WorkerHello (`0x14`): versions, state, pipeline hash;
//! * `0x0401` worker_state, `0x0402` stream_auth_failed, `0x0403` worker_degraded;
//! * the canonical `0x0101` connection_failed, `0x0102` connection_restored,
//!   `0x0105` capture_failed and `0x0106` capture_resumed, per stream.
//!
//! [`WorkerStatus::apply`] returns a [`StatusChange`] for each event that
//! changed something, which the router broadcasts to SSE subscribers.

use std::collections::BTreeMap;

use serde::Serialize;
use serde_json::{Map, Value};
use utoipa::ToSchema;

use super::protocol::{
    MonitorEvent, WorkerHello, EVENT_CAPTURE_FAILED, EVENT_CAPTURE_RESUMED,
    EVENT_CONNECTION_FAILED, EVENT_CONNECTION_RESTORED, EVENT_STREAM_AUTH_FAILED,
    EVENT_WORKER_DEGRADED, EVENT_WORKER_STATE,
};

/// Health of one capture stream.
#[derive(Debug, Clone, PartialEq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum StreamState {
    Streaming,
    ConnectionFailed,
    CaptureFailed,
    /// The camera rejected the credentials; the worker retries slowly.
    AuthFailed,
}

#[derive(Debug, Clone, PartialEq, Serialize, ToSchema)]
pub struct StreamHealth {
    pub state: StreamState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retry_in_sec: Option<f64>,
    /// Consecutive failed attempts, when the worker reports it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attempt: Option<u64>,
    /// Unix milliseconds of the event that set this state.
    pub since_ms: i64,
}

/// What zm-api knows about a worker right now.
#[derive(Debug, Clone, Default, PartialEq, Serialize, ToSchema)]
pub struct WorkerStatus {
    /// `unconfigured` | `configuring` | `running` | `stopping`, from the hello
    /// or the latest worker_state event.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pipeline_hash: Option<String>,
    /// zm-next version and commit from the hello.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub zm_next_version: Option<String>,
    /// Control protocol version from the hello; absent for a worker that
    /// predates it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub control_protocol: Option<u32>,
    pub control_peer: bool,
    /// Keyed by the worker's stream id.
    pub streams: BTreeMap<u32, StreamHealth>,
    /// Component → effect, for each dependency the worker reports as down.
    pub degraded: BTreeMap<String, String>,
}

/// One change, as sent to SSE subscribers.
#[derive(Debug, Clone, PartialEq, Serialize, ToSchema)]
pub struct StatusChange {
    pub monitor_id: u32,
    /// `hello`, `worker_state`, `stream_auth_failed`, `worker_degraded`,
    /// `connection_failed`, `connection_restored`, `capture_failed`,
    /// `capture_resumed` or `disconnected`.
    pub kind: String,
    /// The event's detail as the worker sent it (empty for hello/disconnect).
    #[schema(value_type = Object)]
    pub detail: Value,
    pub at_ms: i64,
}

fn now_ms() -> i64 {
    jiff::Timestamp::now().as_millisecond()
}

fn str_field(d: &Map<String, Value>, k: &str) -> Option<String> {
    d.get(k).and_then(Value::as_str).map(str::to_string)
}

impl WorkerStatus {
    /// Record a hello from a fresh connection. Streams and degradations are
    /// kept: the worker replays its status on connect.
    pub fn apply_hello(&mut self, monitor_id: u32, hello: &WorkerHello) -> StatusChange {
        if !hello.state.is_empty() {
            self.state = Some(hello.state.clone());
        }
        self.pipeline_hash = hello.pipeline_hash.clone();
        self.zm_next_version = Some(if hello.zm_next.commit.is_empty() {
            hello.zm_next.version.clone()
        } else {
            format!("{} ({})", hello.zm_next.version, hello.zm_next.commit)
        });
        self.control_protocol = Some(hello.protocol.control).filter(|v| *v > 0);
        self.control_peer = hello.control_peer;
        StatusChange {
            monitor_id,
            kind: "hello".into(),
            detail: Value::Null,
            at_ms: now_ms(),
        }
    }

    /// Fold a status EVENT in. `None` for codes that aren't status.
    pub fn apply(&mut self, monitor_id: u32, ev: &MonitorEvent) -> Option<StatusChange> {
        let kind = match ev.code {
            EVENT_WORKER_STATE => "worker_state",
            EVENT_STREAM_AUTH_FAILED => "stream_auth_failed",
            EVENT_WORKER_DEGRADED => "worker_degraded",
            EVENT_CONNECTION_FAILED => "connection_failed",
            EVENT_CONNECTION_RESTORED => "connection_restored",
            EVENT_CAPTURE_FAILED => "capture_failed",
            EVENT_CAPTURE_RESUMED => "capture_resumed",
            _ => return None,
        };
        let detail = ev.detail_object().unwrap_or_default();
        let at = ev
            .wall_clock_us
            .map(|us| (us / 1000) as i64)
            .unwrap_or_else(now_ms);
        let stream_id = detail
            .get("stream_id")
            .and_then(Value::as_u64)
            .map(|v| v as u32)
            .unwrap_or(0);
        let retry = detail.get("retry_in_sec").and_then(Value::as_f64);
        let attempt = detail.get("attempt").and_then(Value::as_u64);
        let error = str_field(&detail, "error").or_else(|| {
            // A canonical lifecycle event with a plain-text message.
            ev.message.clone().filter(|_| ev.detail_object().is_none())
        });

        let mut set_stream = |state: StreamState, error: Option<String>| {
            self.streams.insert(
                stream_id,
                StreamHealth {
                    state,
                    error,
                    retry_in_sec: retry,
                    attempt,
                    since_ms: at,
                },
            );
        };
        match ev.code {
            EVENT_WORKER_STATE => {
                self.state = str_field(&detail, "state").or(self.state.take());
                self.reason = str_field(&detail, "reason");
                if let Some(h) = str_field(&detail, "pipeline_hash") {
                    self.pipeline_hash = Some(h);
                }
            }
            EVENT_STREAM_AUTH_FAILED => set_stream(StreamState::AuthFailed, error),
            EVENT_CONNECTION_FAILED => set_stream(StreamState::ConnectionFailed, error),
            EVENT_CAPTURE_FAILED => set_stream(StreamState::CaptureFailed, error),
            EVENT_CONNECTION_RESTORED | EVENT_CAPTURE_RESUMED => {
                set_stream(StreamState::Streaming, None)
            }
            EVENT_WORKER_DEGRADED => {
                let component = str_field(&detail, "component").unwrap_or_else(|| "unknown".into());
                match str_field(&detail, "effect") {
                    // An empty effect means the component recovered.
                    Some(effect) if !effect.is_empty() => {
                        self.degraded.insert(component, effect);
                    }
                    _ => {
                        self.degraded.remove(&component);
                    }
                }
            }
            _ => unreachable!("matched above"),
        }
        Some(StatusChange {
            monitor_id,
            kind: kind.into(),
            detail: Value::Object(detail),
            at_ms: at,
        })
    }

    /// The connection closed: the worker state is unknown until the next hello.
    pub fn apply_disconnect(&mut self, monitor_id: u32) -> StatusChange {
        self.state = None;
        self.control_peer = false;
        StatusChange {
            monitor_id,
            kind: "disconnected".into(),
            detail: Value::Null,
            at_ms: now_ms(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::streaming::source::protocol::parse_worker_hello;
    use serde_json::json;

    fn detail_event(code: u16, detail: Value) -> MonitorEvent {
        MonitorEvent {
            code,
            json_detail: Some(detail.to_string()),
            wall_clock_us: Some(1_700_000_000_000_000),
            ..MonitorEvent::default()
        }
    }

    fn message_event(code: u16, detail: Value) -> MonitorEvent {
        MonitorEvent {
            code,
            message: Some(detail.to_string()),
            wall_clock_us: Some(1_700_000_001_000_000),
            ..MonitorEvent::default()
        }
    }

    #[test]
    fn hello_sets_versions_and_state() {
        let hello = parse_worker_hello(
            br#"{"protocol":{"canonical":1,"control":1},"zm_next":{"version":"0.1.0","commit":"9217bdf"},
                 "state":"running","pipeline_hash":"sha256:ab","control_peer":true}"#,
        )
        .unwrap();
        let mut s = WorkerStatus::default();
        let change = s.apply_hello(3, &hello);
        assert_eq!(change.kind, "hello");
        assert_eq!(s.state.as_deref(), Some("running"));
        assert_eq!(s.pipeline_hash.as_deref(), Some("sha256:ab"));
        assert_eq!(s.zm_next_version.as_deref(), Some("0.1.0 (9217bdf)"));
        assert_eq!(s.control_protocol, Some(1));
        assert!(s.control_peer);
    }

    /// The auth-failure and recovery sequence, using the detail fields from the
    /// doc's Status table. Replace with zm-next's contract transcript.
    #[test]
    fn auth_failure_then_recovery() {
        let mut s = WorkerStatus::default();
        let c = s
            .apply(
                3,
                &detail_event(
                    EVENT_STREAM_AUTH_FAILED,
                    json!({"stream_id": 0, "retry_in_sec": 60}),
                ),
            )
            .unwrap();
        assert_eq!(c.kind, "stream_auth_failed");
        assert_eq!(c.at_ms, 1_700_000_000_000);
        assert_eq!(s.streams[&0].state, StreamState::AuthFailed);
        assert_eq!(s.streams[&0].retry_in_sec, Some(60.0));

        // Canonical lifecycle codes carry their detail in the message TLV.
        s.apply(
            3,
            &message_event(EVENT_CONNECTION_RESTORED, json!({"stream_id": 0})),
        )
        .unwrap();
        assert_eq!(s.streams[&0].state, StreamState::Streaming);
        assert_eq!(s.streams[&0].retry_in_sec, None);
    }

    #[test]
    fn connection_and_capture_failures_are_per_stream() {
        let mut s = WorkerStatus::default();
        s.apply(
            1,
            &message_event(
                EVENT_CONNECTION_FAILED,
                json!({"stream_id": 1, "error": "Connection refused", "retry_in_sec": 5, "attempt": 2}),
            ),
        );
        s.apply(
            1,
            &message_event(
                EVENT_CAPTURE_FAILED,
                json!({"stream_id": 0, "error": "EOF"}),
            ),
        );
        assert_eq!(s.streams[&1].state, StreamState::ConnectionFailed);
        assert_eq!(s.streams[&1].attempt, Some(2));
        assert_eq!(s.streams[&0].state, StreamState::CaptureFailed);
        s.apply(
            1,
            &message_event(EVENT_CAPTURE_RESUMED, json!({"stream_id": 0})),
        );
        assert_eq!(s.streams[&0].state, StreamState::Streaming);
        assert_eq!(s.streams[&1].state, StreamState::ConnectionFailed);
    }

    #[test]
    fn plain_text_lifecycle_message_becomes_the_error() {
        // zmc-style: no JSON, just a message.
        let mut s = WorkerStatus::default();
        let ev = MonitorEvent {
            code: EVENT_CONNECTION_FAILED,
            message: Some("camera unreachable".into()),
            ..MonitorEvent::default()
        };
        s.apply(2, &ev);
        assert_eq!(s.streams[&0].error.as_deref(), Some("camera unreachable"));
    }

    #[test]
    fn worker_state_and_degraded() {
        let mut s = WorkerStatus::default();
        s.apply(
            4,
            &detail_event(
                EVENT_WORKER_STATE,
                json!({"state": "configuring", "pipeline_hash": "sha256:1", "reason": "configure"}),
            ),
        );
        assert_eq!(s.state.as_deref(), Some("configuring"));
        assert_eq!(s.reason.as_deref(), Some("configure"));
        s.apply(
            4,
            &detail_event(
                EVENT_WORKER_DEGRADED,
                json!({"component": "zm-infer", "effect": "detection skipped"}),
            ),
        );
        assert_eq!(s.degraded["zm-infer"], "detection skipped");
        s.apply(
            4,
            &detail_event(
                EVENT_WORKER_DEGRADED,
                json!({"component": "zm-infer", "effect": ""}),
            ),
        );
        assert!(s.degraded.is_empty());
    }

    #[test]
    fn other_codes_are_not_status() {
        let mut s = WorkerStatus::default();
        assert!(s
            .apply(1, &detail_event(0x0301, json!({"objects": []})))
            .is_none());
        assert_eq!(s, WorkerStatus::default());
    }

    #[test]
    fn disconnect_clears_state_but_keeps_last_known_streams() {
        let mut s = WorkerStatus {
            state: Some("running".into()),
            control_peer: true,
            ..Default::default()
        };
        s.apply(
            1,
            &message_event(EVENT_CONNECTION_FAILED, json!({"stream_id": 0})),
        );
        let c = s.apply_disconnect(1);
        assert_eq!(c.kind, "disconnected");
        assert_eq!(s.state, None);
        assert!(!s.control_peer);
        assert!(s.streams.contains_key(&0));
    }
}
