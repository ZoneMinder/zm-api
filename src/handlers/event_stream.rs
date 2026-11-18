//! `GET /api/v3/events/stream`: live events and alarm states as
//! Server-Sent Events. See [`crate::service::event_stream`].

use std::collections::VecDeque;
use std::convert::Infallible;
use std::time::Duration;

use axum::extract::{Query, State};
use axum::http::HeaderMap;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::Extension;
use futures_util::Stream;
use serde::Deserialize;
use tokio::sync::broadcast;
use utoipa::IntoParams;

use crate::error::{AppResponseError, AppResult};
use crate::server::state::AppState;
use crate::service::event_stream::{self, StreamMessage};
use crate::service::monitor_acl::{self, MonitorScope};
use crate::util::authz::{Feature, Level};
use crate::util::claim::UserClaims;

/// How often a subscriber's access is checked again.
const RECHECK: Duration = Duration::from_secs(60);
/// Heartbeat comment interval.
const HEARTBEAT: Duration = Duration::from_secs(15);

#[derive(Debug, Default, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct EventStreamQuery {
    /// Only this monitor.
    pub monitor_id: Option<u32>,
    /// Replay events with a higher id first (as `Last-Event-ID` does), at
    /// most 500.
    pub since: Option<u64>,
}

struct Subscriber {
    state: AppState,
    claims: UserClaims,
    scope: MonitorScope,
    monitor: Option<u32>,
    queue: VecDeque<StreamMessage>,
    rx: broadcast::Receiver<StreamMessage>,
    recheck: tokio::time::Interval,
    /// The access token's expiry: the stream ends there, and the client
    /// reconnects with a fresh token and `Last-Event-ID`.
    expires: tokio::time::Instant,
}

impl Subscriber {
    fn shows(&self, msg: &StreamMessage) -> bool {
        let m = msg.monitor_id();
        self.monitor.is_none_or(|f| f == m) && self.scope.allows(m, Level::View)
    }
}

fn sse(msg: &StreamMessage) -> Event {
    let event = Event::default()
        .event(msg.kind())
        .json_data(msg)
        .unwrap_or_else(|_| Event::default().event(msg.kind()));
    match msg.event_id() {
        Some(id) => event.id(id.to_string()),
        None => event,
    }
}

/// The scope to keep streaming with, or `None` when the user has lost
/// Events access, been disabled, or had their tokens revoked.
async fn still_allowed(state: &AppState, claims: &UserClaims) -> Option<MonitorScope> {
    if state.revocations.is_revoked(claims.uid, claims.iat) {
        return None;
    }
    let user = crate::repo::users::find_by_id(state.db(), claims.uid)
        .await
        .ok()??;
    if user.enabled == 0 {
        return None;
    }
    let perms = crate::util::authz::effective_permissions(state.db(), &user)
        .await
        .ok()?;
    if perms.level(Feature::Events) < Level::View {
        return None;
    }
    monitor_acl::resolve(state.db(), claims.uid, perms.monitors)
        .await
        .ok()
}

/// Live events and alarm states (Server-Sent Events)
///
/// One stream for every monitor the caller may see (or `monitor_id`). The
/// SSE `event` name is the message `type`:
///
/// - `alarm_state` `{monitor_id, state, score}`: `idle`, `prealarm`,
///   `alarm`, `alert`, or `unknown`. Sent for every monitor on connect,
///   then on each change.
/// - `event_start`, `event_end`, `event_updated` (its notes changed, where
///   object detection writes): `{event_id, monitor_id, name, cause, notes,
///   start_date_time, end_date_time, frames, alarm_frames, max_score, length}`.
///   The SSE `id` is the event id: reconnect with `Last-Event-ID` (or
///   `?since=`) to replay what was missed, up to 500 events.
/// - `lagged`: the subscriber fell behind and missed messages; reconnect
///   with `Last-Event-ID`.
///
/// A heartbeat comment every 15 s. The stream ends when the access token
/// expires, or when the user loses access (checked every minute).
#[utoipa::path(
    get,
    path = "/api/v3/events/stream",
    operation_id = "streamEvents",
    tag = "Events",
    params(EventStreamQuery),
    responses(
        (status = 200, description = "Server-Sent Events", content_type = "text/event-stream"),
        (status = 401, description = "No or bad token", body = AppResponseError),
        (status = 403, description = "No Events access", body = AppResponseError)
    ),
    security(("jwt" = []))
)]
pub async fn stream_events(
    State(state): State<AppState>,
    Extension(claims): Extension<UserClaims>,
    scope: MonitorScope,
    Query(q): Query<EventStreamQuery>,
    headers: HeaderMap,
) -> AppResult<Sse<impl Stream<Item = Result<Event, Infallible>>>> {
    let since = q.since.or_else(|| {
        headers
            .get("last-event-id")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.trim().parse().ok())
    });
    // Subscribe before the snapshot and replay, so nothing falls between.
    let rx = state.event_hub.subscribe(&state);

    let mut sub = Subscriber {
        expires: tokio::time::Instant::now()
            + Duration::from_secs((claims.exp - crate::util::time::unix_now()).max(0) as u64),
        state: state.clone(),
        claims,
        scope,
        monitor: q.monitor_id,
        queue: VecDeque::new(),
        rx,
        recheck: tokio::time::interval(RECHECK),
    };
    sub.recheck.reset();

    let alarms: Vec<StreamMessage> = state
        .event_hub
        .alarm_states()
        .into_iter()
        .map(StreamMessage::AlarmState)
        .collect();
    let mut first: Vec<StreamMessage> = alarms.into_iter().filter(|m| sub.shows(m)).collect();
    if let Some(since) = since {
        let visible = match (sub.scope.visible_ids(Level::View), sub.monitor) {
            (None, None) => None,
            (None, Some(m)) => Some(vec![m]),
            (Some(ids), None) => Some(ids),
            (Some(ids), Some(m)) => Some(ids.into_iter().filter(|&i| i == m).collect()),
        };
        first.extend(event_stream::replay(&state, since, visible).await?);
    }
    sub.queue.extend(first);

    let stream = futures_util::stream::unfold(sub, |mut sub| async move {
        loop {
            if let Some(msg) = sub.queue.pop_front() {
                return Some((Ok(sse(&msg)), sub));
            }
            tokio::select! {
                r = sub.rx.recv() => match r {
                    Ok(msg) if sub.shows(&msg) => return Some((Ok(sse(&msg)), sub)),
                    Ok(_) => continue,
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        let event = Event::default().event("lagged").data(n.to_string());
                        return Some((Ok(event), sub));
                    }
                    Err(broadcast::error::RecvError::Closed) => return None,
                },
                _ = sub.recheck.tick() => match still_allowed(&sub.state, &sub.claims).await {
                    Some(scope) => sub.scope = scope,
                    None => return None,
                },
                _ = tokio::time::sleep_until(sub.expires) => return None,
            }
        }
    });
    Ok(Sse::new(stream).keep_alive(KeepAlive::new().interval(HEARTBEAT)))
}
