//! The live event stream (`GET /api/v3/events/stream`): one push channel
//! for event start, end and notes changes, and each monitor's alarm state.
//!
//! One watcher serves every subscriber, and runs only while someone is
//! listening:
//! - **Events** come from ZoneMinder's `Events` table: a cursor on `Id`
//!   finds new events, and the open and recently closed ones are re-read for
//!   their end and their notes (object detection writes its result there).
//!   This works for monitors on any server.
//! - **Alarm state** comes from each local monitor's shared memory, read
//!   [`TICK`] times a second. A change in a monitor's last event id there
//!   also triggers an immediate events check, so `event_start` isn't held
//!   to the database poll.
//!
//! Nothing is stored: the cursor is ZoneMinder's own event id, which is also
//! what a reconnecting subscriber resumes from (`since` / `Last-Event-ID`).

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::RwLock;
use std::time::{Duration, Instant};

use sea_orm::{ColumnTrait, EntityTrait, QueryFilter, QueryOrder, QuerySelect};
use serde::{Deserialize, Serialize};
use tokio::sync::{broadcast, Notify};
use tracing::{debug, warn};
use utoipa::ToSchema;

use crate::dto::wrappers::DateTimeWrapper;
use crate::entity::events;
use crate::server::state::AppState;
use crate::zm_shm::{MonitorShm, State};

/// How often shared memory is read.
pub const TICK: Duration = Duration::from_millis(250);
/// How often the events table is polled when shared memory gives no hint.
const DB_POLL: Duration = Duration::from_secs(1);
/// How often the monitor list is re-read (and dead shared memory retried).
const MONITOR_REFRESH: Duration = Duration::from_secs(10);
/// How long a closed event is still watched for notes changes.
const NOTES_WINDOW: Duration = Duration::from_secs(10 * 60);
/// Most events one poll reads; the rest come on the next.
const BATCH: u64 = 200;
/// Most events a reconnecting subscriber is replayed.
pub const REPLAY_LIMIT: u64 = 500;

/// An event, as the stream describes it.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct EventInfo {
    pub event_id: u64,
    pub monitor_id: u32,
    pub name: String,
    pub cause: Option<String>,
    pub notes: Option<String>,
    pub start_date_time: Option<DateTimeWrapper>,
    /// `null` while the event is open.
    pub end_date_time: Option<DateTimeWrapper>,
    pub frames: u32,
    pub alarm_frames: u32,
    pub max_score: u16,
    /// Seconds.
    pub length: f64,
}

impl From<&events::Model> for EventInfo {
    fn from(m: &events::Model) -> Self {
        use crate::util::datetime_to_utc as utc;
        Self {
            event_id: m.id,
            monitor_id: m.monitor_id,
            name: m.name.clone(),
            cause: m.cause.clone(),
            notes: m.notes.clone(),
            start_date_time: m.start_date_time.map(|t| DateTimeWrapper(utc(t))),
            end_date_time: m.end_date_time.map(|t| DateTimeWrapper(utc(t))),
            frames: m.frames.unwrap_or(0),
            alarm_frames: m.alarm_frames.unwrap_or(0),
            max_score: m.max_score.unwrap_or(0),
            length: m.length.to_string().parse().unwrap_or(0.0),
        }
    }
}

/// A monitor's alarm state.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub struct AlarmInfo {
    pub monitor_id: u32,
    /// `idle`, `prealarm`, `alarm`, `alert`, or `unknown` (zmc not running).
    pub state: String,
    /// The last analysed frame's score.
    pub score: u32,
}

/// One message on the stream; `type` names it.
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StreamMessage {
    EventStart(EventInfo),
    EventEnd(EventInfo),
    /// An event's notes changed (object detection results land there).
    EventUpdated(EventInfo),
    AlarmState(AlarmInfo),
}

impl StreamMessage {
    pub fn kind(&self) -> &'static str {
        match self {
            StreamMessage::EventStart(_) => "event_start",
            StreamMessage::EventEnd(_) => "event_end",
            StreamMessage::EventUpdated(_) => "event_updated",
            StreamMessage::AlarmState(_) => "alarm_state",
        }
    }

    pub fn monitor_id(&self) -> u32 {
        match self {
            StreamMessage::EventStart(e)
            | StreamMessage::EventEnd(e)
            | StreamMessage::EventUpdated(e) => e.monitor_id,
            StreamMessage::AlarmState(a) => a.monitor_id,
        }
    }

    /// The SSE id: the event id, so a subscriber can resume after it.
    pub fn event_id(&self) -> Option<u64> {
        match self {
            StreamMessage::EventStart(e)
            | StreamMessage::EventEnd(e)
            | StreamMessage::EventUpdated(e) => Some(e.event_id),
            StreamMessage::AlarmState(_) => None,
        }
    }
}

/// The messages for events read for the first time: a start, and an end
/// too when it already closed.
pub fn first_seen(rows: &[events::Model]) -> Vec<StreamMessage> {
    let mut out = Vec::new();
    for row in rows {
        let info = EventInfo::from(row);
        out.push(StreamMessage::EventStart(info.clone()));
        if row.end_date_time.is_some() {
            out.push(StreamMessage::EventEnd(info));
        }
    }
    out
}

/// Shared by the watcher and every subscriber.
pub struct EventHub {
    tx: broadcast::Sender<StreamMessage>,
    alarm: RwLock<HashMap<u32, AlarmInfo>>,
    running: AtomicBool,
    wake: Notify,
}

impl Default for EventHub {
    fn default() -> Self {
        Self {
            tx: broadcast::channel(1024).0,
            alarm: RwLock::new(HashMap::new()),
            running: AtomicBool::new(false),
            wake: Notify::new(),
        }
    }
}

impl EventHub {
    /// Subscribe, starting the watcher if it isn't running.
    pub fn subscribe(&self, state: &AppState) -> broadcast::Receiver<StreamMessage> {
        let rx = self.tx.subscribe();
        if !self.running.swap(true, Ordering::SeqCst) {
            tokio::spawn(run(state.clone()));
        }
        self.wake.notify_one();
        rx
    }

    /// Each monitor's last known alarm state.
    pub fn alarm_states(&self) -> Vec<AlarmInfo> {
        let mut v: Vec<AlarmInfo> = self
            .alarm
            .read()
            .expect("alarm lock")
            .values()
            .cloned()
            .collect();
        v.sort_by_key(|a| a.monitor_id);
        v
    }

    fn send(&self, msg: StreamMessage) {
        let _ = self.tx.send(msg);
    }
}

fn state_name(s: State) -> &'static str {
    match s {
        State::Idle => "idle",
        State::PreAlarm => "prealarm",
        State::Alarm => "alarm",
        State::Alert => "alert",
        State::Unknown => "unknown",
    }
}

/// Alarm-state bookkeeping, separate from shared memory so it can be tested.
#[derive(Default)]
pub struct AlarmTracker {
    last: HashMap<u32, (String, u64)>,
}

impl AlarmTracker {
    /// Record a monitor's state and last event id. Returns the message for a
    /// state change, and whether the last event id moved.
    pub fn observe(
        &mut self,
        monitor_id: u32,
        state: &str,
        score: u32,
        last_event_id: u64,
    ) -> (Option<AlarmInfo>, bool) {
        let prev = self
            .last
            .insert(monitor_id, (state.to_string(), last_event_id));
        let changed = prev.as_ref().map(|(s, _)| s.as_str()) != Some(state);
        let bumped = prev.is_some_and(|(_, e)| e != last_event_id);
        let msg = changed.then(|| AlarmInfo {
            monitor_id,
            state: state.to_string(),
            score,
        });
        (msg, bumped)
    }

    pub fn forget(&mut self, monitor_id: u32) {
        self.last.remove(&monitor_id);
    }
}

struct Tracked {
    ended: Option<Instant>,
    notes: Option<String>,
}

/// Event bookkeeping, separate from the database so it can be tested.
#[derive(Default)]
pub struct EventTracker {
    /// Highest event id announced; `None` until the first poll.
    pub cursor: Option<u64>,
    tracked: HashMap<u64, Tracked>,
}

impl EventTracker {
    /// New events (id above the cursor, ascending): their messages.
    pub fn on_new(&mut self, rows: &[events::Model], now: Instant) -> Vec<StreamMessage> {
        for row in rows {
            self.cursor = Some(self.cursor.unwrap_or(0).max(row.id));
            self.tracked.insert(
                row.id,
                Tracked {
                    ended: row.end_date_time.map(|_| now),
                    notes: row.notes.clone(),
                },
            );
        }
        first_seen(rows)
    }

    /// The events to re-read: open ones, and ones closed within
    /// [`NOTES_WINDOW`].
    pub fn watched(&mut self, now: Instant) -> Vec<u64> {
        self.tracked.retain(|_, t| {
            t.ended
                .is_none_or(|at| now.duration_since(at) < NOTES_WINDOW)
        });
        let mut ids: Vec<u64> = self.tracked.keys().copied().collect();
        ids.sort_unstable();
        ids
    }

    /// The re-read rows of [`Self::watched`] events: ends and notes changes.
    /// A watched event missing from `rows` was deleted and is dropped.
    pub fn on_refresh(
        &mut self,
        watched: &[u64],
        rows: &[events::Model],
        now: Instant,
    ) -> Vec<StreamMessage> {
        let mut out = Vec::new();
        let by_id: HashMap<u64, &events::Model> = rows.iter().map(|r| (r.id, r)).collect();
        for id in watched {
            let Some(row) = by_id.get(id) else {
                self.tracked.remove(id);
                continue;
            };
            let Some(t) = self.tracked.get_mut(id) else {
                continue;
            };
            if row.notes != t.notes {
                t.notes = row.notes.clone();
                out.push(StreamMessage::EventUpdated(EventInfo::from(*row)));
            }
            if t.ended.is_none() && row.end_date_time.is_some() {
                t.ended = Some(now);
                out.push(StreamMessage::EventEnd(EventInfo::from(*row)));
            }
        }
        out
    }
}

/// The monitors whose shared memory this server holds. On a multi-server
/// install that is the ones whose `ServerId` is this server's: reading the
/// others' would report them `unknown` forever, and their real state comes
/// from the server that runs them (see `crate::cluster::events`).
async fn local_monitors(state: &AppState) -> Vec<u32> {
    use crate::entity::monitors::Column;
    match crate::entity::monitors::Entity::find()
        .select_only()
        .column(Column::Id)
        .column(Column::ServerId)
        .filter(Column::Deleted.eq(0))
        .into_tuple::<(u32, Option<u32>)>()
        .all(state.db())
        .await
    {
        Ok(rows) => owned_here(state.cluster.relays(), state.cluster.this_server(), rows),
        Err(e) => {
            warn!("event stream: listing monitors: {e}");
            Vec::new()
        }
    }
}

/// The ids of the monitors, as `(id, ServerId)`, that this server runs.
/// Every monitor when the install does not relay.
fn owned_here(
    relays: bool,
    this_server: Option<u32>,
    monitors: Vec<(u32, Option<u32>)>,
) -> Vec<u32> {
    monitors
        .into_iter()
        .filter(|(_, server)| {
            !relays || crate::cluster::owner(this_server, *server) == crate::cluster::Owner::Here
        })
        .map(|(id, _)| id)
        .collect()
}

async fn poll_events(state: &AppState, events: &mut EventTracker, hub: &EventHub) {
    let db = state.db();
    let now = Instant::now();
    match events.cursor {
        // Start from now: a subscriber asks for history with `since`.
        None => {
            let max = events::Entity::find()
                .select_only()
                .column(events::Column::Id)
                .order_by_desc(events::Column::Id)
                .into_tuple::<u64>()
                .one(db)
                .await;
            match max {
                Ok(id) => events.cursor = Some(id.unwrap_or(0)),
                Err(e) => warn!("event stream: reading the latest event: {e}"),
            }
            return;
        }
        Some(cursor) => {
            match events::Entity::find()
                .filter(events::Column::Id.gt(cursor))
                .order_by_asc(events::Column::Id)
                .limit(BATCH)
                .all(db)
                .await
            {
                Ok(rows) => {
                    for msg in events.on_new(&rows, now) {
                        hub.send(msg);
                    }
                }
                Err(e) => warn!("event stream: reading new events: {e}"),
            }
        }
    }
    let watched = events.watched(now);
    if watched.is_empty() {
        return;
    }
    match events::Entity::find()
        .filter(events::Column::Id.is_in(watched.iter().copied()))
        .all(db)
        .await
    {
        Ok(rows) => {
            for msg in events.on_refresh(&watched, &rows, now) {
                hub.send(msg);
            }
        }
        Err(e) => warn!("event stream: re-reading events: {e}"),
    }
}

/// The watcher: runs while anyone is subscribed.
async fn run(state: AppState) {
    let hub = state.event_hub.clone();
    let mut alarms = AlarmTracker::default();
    let mut events = EventTracker::default();
    let mut shm: HashMap<u32, MonitorShm> = HashMap::new();
    let mut monitors: Vec<u32> = Vec::new();
    let mut monitors_at: Option<Instant> = None;
    let mut polled_at: Option<Instant> = None;
    let mut idle_since: Option<Instant> = None;
    debug!("event stream: watcher started");
    loop {
        if hub.tx.receiver_count() == 0 {
            // Nobody listening: stop after a grace period, so a client that
            // reconnects at once doesn't restart from scratch.
            let since = *idle_since.get_or_insert_with(Instant::now);
            if since.elapsed() > Duration::from_secs(30) {
                hub.running.store(false, Ordering::SeqCst);
                // A subscriber that arrived between the check and the store
                // started no watcher; carry on for it.
                if hub.tx.receiver_count() == 0 || hub.running.swap(true, Ordering::SeqCst) {
                    debug!("event stream: watcher stopped");
                    return;
                }
            }
        } else {
            idle_since = None;
        }

        let now = Instant::now();
        if monitors_at.is_none_or(|t| now.duration_since(t) >= MONITOR_REFRESH) {
            monitors = local_monitors(&state).await;
            shm.retain(|id, _| monitors.contains(id));
            for &id in &monitors {
                if let std::collections::hash_map::Entry::Vacant(slot) = shm.entry(id) {
                    if let Ok(s) = MonitorShm::connect(id) {
                        slot.insert(s);
                    }
                }
            }
            monitors_at = Some(now);
        }

        let mut bumped = false;
        let mut lost = Vec::new();
        for &id in &monitors {
            let (name, score, last_event) = match shm.get(&id) {
                // A dead heartbeat means zmc stopped (or restarted with a new
                // segment, which the next refresh reconnects to).
                Some(s) if s.is_valid() && s.is_alive(Duration::from_secs(10)) => (
                    state_name(s.get_state()),
                    s.get_last_frame_score(),
                    s.get_last_event_id(),
                ),
                Some(_) => {
                    lost.push(id);
                    ("unknown", 0, 0)
                }
                None => ("unknown", 0, 0),
            };
            let (msg, moved) = alarms.observe(id, name, score, last_event);
            bumped |= moved;
            if let Some(info) = msg {
                hub.alarm
                    .write()
                    .expect("alarm lock")
                    .insert(id, info.clone());
                hub.send(StreamMessage::AlarmState(info));
            }
        }
        // Reconnected by the next monitor refresh.
        for id in lost {
            shm.remove(&id);
        }
        {
            let mut map = hub.alarm.write().expect("alarm lock");
            map.retain(|id, _| monitors.contains(id));
        }
        for id in alarms.last.keys().copied().collect::<Vec<_>>() {
            if !monitors.contains(&id) {
                alarms.forget(id);
            }
        }

        if bumped || polled_at.is_none_or(|t| now.duration_since(t) >= DB_POLL) {
            poll_events(&state, &mut events, &hub).await;
            polled_at = Some(Instant::now());
        }

        tokio::select! {
            _ = tokio::time::sleep(TICK) => {}
            _ = hub.wake.notified() => {}
        }
    }
}

/// Events after `since` for a reconnecting subscriber, oldest first, at most
/// [`REPLAY_LIMIT`], limited to `monitor_ids` (`None`: all).
pub async fn replay(
    state: &AppState,
    since: u64,
    monitor_ids: Option<Vec<u32>>,
) -> crate::error::AppResult<Vec<StreamMessage>> {
    let mut q = events::Entity::find()
        .filter(events::Column::Id.gt(since))
        .order_by_asc(events::Column::Id)
        .limit(REPLAY_LIMIT);
    if let Some(ids) = monitor_ids {
        q = q.filter(events::Column::MonitorId.is_in(ids));
    }
    Ok(first_seen(&q.all(state.db()).await?))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(id: u64, monitor_id: u32, ended: bool, notes: Option<&str>) -> events::Model {
        use crate::entity::sea_orm_active_enums::{Orientation, Scheme};
        let t = crate::util::time::db_datetime("2026-10-24T10:00:00".parse().unwrap());
        events::Model {
            id,
            monitor_id,
            storage_id: Some(0),
            secondary_storage_id: None,
            name: format!("Event-{id}"),
            cause: Some("Motion".into()),
            start_date_time: Some(t),
            end_date_time: ended.then_some(t),
            width: 64,
            height: 48,
            length: sea_orm::prelude::Decimal::new(125, 2),
            frames: Some(10),
            alarm_frames: Some(4),
            default_video: String::new(),
            save_jpe_gs: None,
            tot_score: 0,
            avg_score: None,
            max_score: Some(42),
            max_score_frame_id: None,
            archived: 0,
            videoed: 0,
            uploaded: 0,
            emailed: 0,
            messaged: 0,
            executed: 0,
            notes: notes.map(String::from),
            state_id: 0,
            orientation: Orientation::Rotate0,
            disk_space: None,
            scheme: Scheme::Medium,
            locked: 0,
            latitude: None,
            longitude: None,
        }
    }

    #[test]
    fn only_this_servers_monitors_are_read_from_shared_memory() {
        let rows = vec![(1, Some(1)), (2, Some(2)), (3, None), (4, Some(0))];
        assert_eq!(owned_here(true, Some(1), rows.clone()), [1, 3, 4]);
        // Without relaying (or without a server id) every monitor is read, as before.
        assert_eq!(owned_here(false, Some(1), rows.clone()), [1, 2, 3, 4]);
        assert_eq!(owned_here(true, None, rows), [1, 2, 3, 4]);
    }

    #[test]
    fn messages_serialize_with_their_type() {
        let msg = StreamMessage::EventStart(EventInfo::from(&event(7, 3, false, None)));
        let v = serde_json::to_value(&msg).unwrap();
        assert_eq!(v["type"], "event_start");
        assert_eq!(v["event_id"], 7);
        assert_eq!(v["monitor_id"], 3);
        assert_eq!(v["end_date_time"], serde_json::Value::Null);
        assert_eq!(v["max_score"], 42);
        assert_eq!(v["length"], 1.25);
        let a = StreamMessage::AlarmState(AlarmInfo {
            monitor_id: 3,
            state: "alarm".into(),
            score: 17,
        });
        let v = serde_json::to_value(&a).unwrap();
        assert_eq!(v["type"], "alarm_state");
        assert_eq!(v["state"], "alarm");
        assert_eq!(a.event_id(), None);
    }

    #[test]
    fn alarm_changes_are_reported_once() {
        let mut t = AlarmTracker::default();
        let (m, moved) = t.observe(1, "idle", 0, 5);
        assert_eq!(m.unwrap().state, "idle", "the first reading is reported");
        assert!(!moved);
        assert_eq!(t.observe(1, "idle", 3, 5), (None, false));
        let (m, moved) = t.observe(1, "alarm", 40, 6);
        assert_eq!(m.unwrap().score, 40);
        assert!(moved, "a new last event id");
        assert_eq!(t.observe(1, "alarm", 41, 6), (None, false));
    }

    #[test]
    fn events_start_end_and_update() {
        let now = Instant::now();
        let mut t = EventTracker {
            cursor: Some(9),
            ..Default::default()
        };
        // One open, one that started and ended between polls.
        let msgs = t.on_new(&[event(10, 1, false, None), event(11, 2, true, None)], now);
        let kinds: Vec<_> = msgs
            .iter()
            .map(|m| (m.kind(), m.event_id().unwrap()))
            .collect();
        assert_eq!(
            kinds,
            [("event_start", 10), ("event_start", 11), ("event_end", 11)]
        );
        assert_eq!(t.cursor, Some(11));

        let watched = t.watched(now);
        assert_eq!(watched, [10, 11]);
        // 10 closes; 11 gets a detection result.
        let rows = [
            event(10, 1, true, None),
            event(11, 2, true, Some("detected:person:97%")),
        ];
        let msgs = t.on_refresh(&watched, &rows, now);
        let kinds: Vec<_> = msgs
            .iter()
            .map(|m| (m.kind(), m.event_id().unwrap()))
            .collect();
        assert_eq!(kinds, [("event_end", 10), ("event_updated", 11)]);
        // Nothing new the second time.
        assert!(t.on_refresh(&watched, &rows, now).is_empty());
        // A deleted event is dropped.
        let msgs = t.on_refresh(&watched, &rows[..1], now);
        assert!(msgs.is_empty());
        assert_eq!(t.watched(now), [10]);
        // Closed events age out of the notes window.
        assert!(t
            .watched(now + NOTES_WINDOW + Duration::from_secs(1))
            .is_empty());
    }
}
