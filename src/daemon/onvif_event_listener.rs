//! Per-monitor ONVIF PullPoint event listener.
//!
//! This is the integration glue between the ONVIF **Events** service client
//! ([`crate::onvif::events`]) and ZoneMinder's `Events` table. For each monitor
//! whose `ONVIF_Event_Listener` flag is set, a background task:
//!
//! 1. Calls `CreatePullPointSubscription` to establish a subscription.
//! 2. Long-polls `PullMessages` in a loop, translating alarm on/off
//!    [`NotificationMessage`]s into ZM `Events` rows (an alarm rising edge opens
//!    an event; the matching falling edge closes it with an end time + length).
//! 3. Periodically `Renew`s the subscription before it expires.
//! 4. Reconnects with capped exponential backoff on any transport/parse error.
//! 5. Exits cleanly (best-effort `Unsubscribe`) once the [`DaemonManager`]
//!    signals shutdown.
//!
//! The task is spawned from the main session (see `moduleDeclNeeded`) via
//! [`spawn_monitor_event_listener`], which returns the `JoinHandle` so the
//! caller can track it alongside the other supervised loops.
//!
//! ## Subscription-address subtlety
//!
//! `CreatePullPointSubscription` returns a per-subscription
//! [`PullPointSubscription::address`] that all later `PullMessages` / `Renew` /
//! `Unsubscribe` calls must target — *not* the Events service XAddr. The loop
//! threads that address explicitly, exactly as [`EventsClient`] requires.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use sea_orm::ActiveValue::Set;
use tokio::task::JoinHandle;
use tracing::{debug, error, info, instrument, warn};

use crate::daemon::manager::DaemonManager;
use crate::entity::events;
use crate::onvif::error::OnvifError;
use crate::onvif::events::{EventsClient, NotificationMessage, PullPointSubscription};
use crate::repo::events as events_repo;
use crate::server::state::AppState;

/// XML duration string requesting the device keep the subscription alive this
/// long absent a `Renew`. Kept short so a dead listener's subscription expires
/// promptly on the device side.
const SUBSCRIPTION_TERMINATION: &str = "PT60S";

/// Tuning for a single monitor's PullPoint loop.
///
/// All durations are conservative defaults that match the ONVIF Events client's
/// own timing expectations (the transport timeout must comfortably exceed the
/// `PullMessages` long-poll timeout — see [`EventsClient`]).
#[derive(Debug, Clone, PartialEq, Eq, utoipa::ToSchema)]
pub struct OnvifEventListenerConfig {
    /// `PullMessages` long-poll timeout, as an XML duration (e.g. `PT30S`).
    /// The device blocks up to this long waiting for notifications.
    pub pull_timeout: String,
    /// Maximum notifications to request per `PullMessages` batch.
    pub message_limit: u32,
    /// How long before the subscription's termination time to issue a `Renew`.
    /// Renewing early avoids a race where the subscription lapses mid-poll.
    #[schema(value_type = u64)]
    pub renew_interval: Duration,
    /// Initial reconnect backoff after a transport/parse error.
    #[schema(value_type = u64)]
    pub min_backoff: Duration,
    /// Maximum reconnect backoff (the exponential growth is capped here).
    #[schema(value_type = u64)]
    pub max_backoff: Duration,
}

impl Default for OnvifEventListenerConfig {
    fn default() -> Self {
        Self {
            pull_timeout: "PT30S".to_string(),
            message_limit: 100,
            // Subscription lives ~60s (SUBSCRIPTION_TERMINATION); renew at ~40s.
            renew_interval: Duration::from_secs(40),
            min_backoff: Duration::from_secs(1),
            max_backoff: Duration::from_secs(60),
        }
    }
}

/// Spawn the per-monitor ONVIF PullPoint event listener.
///
/// Returns the task's [`JoinHandle`]; the loop runs until the `manager` signals
/// shutdown (observed via [`DaemonManager::is_shutting_down`]) or the monitor's
/// subscription cannot be re-established. `client` must already target the
/// monitor's Events service XAddr with any required WS-Security credentials.
///
/// The `manager` handle is used purely as a shutdown observer so this listener
/// quiesces in lock-step with the other supervised daemon loops.
///
/// `#[instrument]` is applied to the async [`MonitorEventListener::run`] body
/// (where the span lives for the task's lifetime) rather than this synchronous
/// spawner, whose span would close immediately.
pub fn spawn_monitor_event_listener(
    state: AppState,
    monitor_id: u32,
    monitor_name: String,
    alarm_cause: String,
    client: EventsClient,
    manager: Arc<DaemonManager>,
    config: OnvifEventListenerConfig,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let listener = MonitorEventListener {
            state,
            monitor_id,
            monitor_name,
            alarm_cause,
            client,
            manager,
            config,
        };
        listener.run().await;
    })
}

/// Owns the state for one monitor's PullPoint loop.
struct MonitorEventListener {
    state: AppState,
    monitor_id: u32,
    monitor_name: String,
    /// `Cause` written on opened ZM events (the monitor's `ONVIF_Alarm_Text`,
    /// falling back to a generic label).
    alarm_cause: String,
    client: EventsClient,
    manager: Arc<DaemonManager>,
    config: OnvifEventListenerConfig,
}

impl MonitorEventListener {
    /// The outer supervision loop: (re)subscribe with backoff, run the inner
    /// poll loop, and repeat until shutdown.
    #[instrument(skip(self), fields(monitor_id = self.monitor_id))]
    async fn run(self) {
        info!(
            monitor_id = self.monitor_id,
            monitor = %self.monitor_name,
            "ONVIF event listener starting"
        );

        let mut backoff = self.config.min_backoff;
        // Per-source alarm state + the open ZM event id, carried across
        // reconnects so a reconnect mid-alarm doesn't duplicate or orphan rows.
        let mut tracker = AlarmTracker::new();

        loop {
            if self.manager.is_process_exiting() {
                break;
            }

            match self.run_subscription(&mut tracker, &mut backoff).await {
                Ok(()) => {
                    // Clean exit only happens on shutdown; reset backoff.
                    backoff = self.config.min_backoff;
                    if self.manager.is_process_exiting() {
                        break;
                    }
                }
                Err(e) => {
                    warn!(
                        monitor_id = self.monitor_id,
                        error = %e,
                        retry_in = ?backoff,
                        "ONVIF event subscription failed; backing off"
                    );
                    if self.sleep_or_shutdown(backoff).await {
                        break;
                    }
                    backoff = (backoff * 2).min(self.config.max_backoff);
                }
            }
        }

        info!(monitor_id = self.monitor_id, "ONVIF event listener stopped");
    }

    /// Establish one subscription and poll/renew it until shutdown or error.
    async fn run_subscription(
        &self,
        tracker: &mut AlarmTracker,
        backoff: &mut Duration,
    ) -> Result<(), OnvifError> {
        let sub: PullPointSubscription = self
            .client
            .create_pull_point_subscription(Some(SUBSCRIPTION_TERMINATION))
            .await?;
        // A subscription came up: the camera is back, so the next failure
        // starts the backoff from the beginning again. It used to reset only
        // on shutdown, so a camera that flapped once a day was retried at the
        // maximum delay forever (#120).
        *backoff = self.config.min_backoff;

        if sub.address.is_empty() {
            return Err(OnvifError::Parse(
                "CreatePullPointSubscription returned no SubscriptionReference address".to_string(),
            ));
        }

        let address = sub.address.clone();
        debug!(
            monitor_id = self.monitor_id,
            address = %address,
            "ONVIF PullPoint subscription established"
        );

        let mut last_renew = tokio::time::Instant::now();

        loop {
            if self.manager.is_process_exiting() {
                // Best-effort teardown so the device frees the subscription.
                if let Err(e) = self.client.unsubscribe(&address).await {
                    debug!(
                        monitor_id = self.monitor_id,
                        error = %e,
                        "ONVIF Unsubscribe failed during shutdown (ignored)"
                    );
                }
                return Ok(());
            }

            // Renew must not wait for `PullMessages` to return — on a quiet
            // camera a pull only completes every `pull_timeout`, which can
            // exceed the subscription lifetime. But cancelling the pull to
            // renew loses any response the device had already sent: the
            // device removes messages from its queue on delivery (#120). So
            // the pull is pinned and kept running while a renew fires beside
            // it, and only the pull's completion ends the iteration.
            let pull = self.client.pull_messages(
                &address,
                &self.config.pull_timeout,
                self.config.message_limit,
            );
            tokio::pin!(pull);
            let mut renew_at = last_renew + self.config.renew_interval;
            loop {
                tokio::select! {
                    _ = tokio::time::sleep_until(renew_at) => {
                        self.client
                            .renew(&address, SUBSCRIPTION_TERMINATION)
                            .await?;
                        last_renew = tokio::time::Instant::now();
                        renew_at = last_renew + self.config.renew_interval;
                        debug!(monitor_id = self.monitor_id, "renewed ONVIF subscription");
                    }
                    resp = &mut pull => {
                        let resp = resp?;
                        for msg in &resp.messages {
                            self.handle_notification(msg, tracker).await;
                        }
                        break;
                    }
                }
            }
        }
    }

    /// Translate a single notification into a ZM `Events` row transition.
    ///
    /// Only notifications that resolve to a boolean alarm state (via
    /// [`NotificationMessage::alarm_active`]) drive event rows; counters,
    /// strings, and other metadata notifications are logged and ignored.
    async fn handle_notification(&self, msg: &NotificationMessage, tracker: &mut AlarmTracker) {
        let Some(active) = msg.alarm_active() else {
            debug!(
                monitor_id = self.monitor_id,
                topic = msg.topic.as_deref().unwrap_or("?"),
                "ignoring non-alarm ONVIF notification"
            );
            return;
        };

        // Group state by the notification topic so distinct rules
        // (motion vs tamper vs digital input) open distinct events.
        let key = msg.topic.clone().unwrap_or_default();

        match (active, tracker.is_open(&key)) {
            (true, false) => {
                // Rising edge — open a new event.
                match self.open_event(msg).await {
                    Ok(event_id) => {
                        tracker.set_open(key, event_id);
                        info!(
                            monitor_id = self.monitor_id,
                            event_id,
                            topic = msg.topic.as_deref().unwrap_or("?"),
                            "opened ONVIF alarm event"
                        );
                    }
                    Err(e) => {
                        error!(
                            monitor_id = self.monitor_id,
                            error = %e,
                            "failed to open ONVIF alarm event"
                        );
                    }
                }
            }
            (false, true) => {
                // Falling edge — close the open event.
                if let Some(event_id) = tracker.take_open(&key) {
                    if let Err(e) = self.close_event(event_id).await {
                        error!(
                            monitor_id = self.monitor_id,
                            event_id,
                            error = %e,
                            "failed to close ONVIF alarm event"
                        );
                    } else {
                        info!(
                            monitor_id = self.monitor_id,
                            event_id, "closed ONVIF alarm event"
                        );
                    }
                }
            }
            // Repeated same-state notifications (e.g. periodic "still active")
            // are no-ops — the event stays open / stays closed.
            _ => {}
        }
    }

    /// Insert a new ZM `Events` row for an opened alarm, returning its id.
    async fn open_event(&self, msg: &NotificationMessage) -> Result<u64, crate::error::AppError> {
        let start = parse_utc(msg.utc_time.as_deref()).unwrap_or_else(crate::util::time::utc_now);

        let active = events::ActiveModel {
            monitor_id: Set(self.monitor_id),
            name: Set(format!(
                "ONVIF-{}-{}",
                self.monitor_id,
                start.strftime("%Y%m%d%H%M%S")
            )),
            cause: Set(Some(self.alarm_cause.clone())),
            start_date_time: Set(Some(crate::util::time::db_datetime(start))),
            notes: Set(msg.topic.clone()),
            ..Default::default()
        };

        let saved = events_repo::create(&self.state, active).await?;
        Ok(saved.id)
    }

    /// Close an open ZM `Events` row: set its end time and length.
    async fn close_event(&self, event_id: u64) -> Result<(), crate::error::AppError> {
        let Some(event) = events_repo::find_by_id(&self.state, event_id).await? else {
            // Row vanished (deleted externally); nothing to close.
            return Ok(());
        };

        let end = crate::util::time::utc_now();
        let length = event
            .start_date_time
            .map(|start| {
                let elapsed = end.duration_since(crate::util::time::civil(start));
                elapsed.as_millis().max(0) as f64 / 1000.0
            })
            .unwrap_or(0.0);

        let mut active: events::ActiveModel = event.into();
        active.end_date_time = Set(Some(crate::util::time::db_datetime(end)));
        active.length = Set(rust_decimal::Decimal::from_f64_retain(length).unwrap_or_default());
        events_repo::update(&self.state, active).await?;
        Ok(())
    }

    /// Sleep for `dur`, waking early to re-check shutdown. Returns `true` if
    /// shutdown was observed (caller should stop), `false` if the sleep elapsed.
    async fn sleep_or_shutdown(&self, dur: Duration) -> bool {
        let tick = Duration::from_millis(250);
        let deadline = tokio::time::Instant::now() + dur;
        loop {
            if self.manager.is_process_exiting() {
                return true;
            }
            let now = tokio::time::Instant::now();
            if now >= deadline {
                return false;
            }
            tokio::time::sleep(tick.min(deadline - now)).await;
        }
    }
}

/// Tracks which alarm topics currently have an open ZM event, keyed by topic.
struct AlarmTracker {
    open: HashMap<String, u64>,
}

impl AlarmTracker {
    fn new() -> Self {
        Self {
            open: HashMap::new(),
        }
    }

    fn is_open(&self, key: &str) -> bool {
        self.open.contains_key(key)
    }

    fn set_open(&mut self, key: String, event_id: u64) {
        self.open.insert(key, event_id);
    }

    fn take_open(&mut self, key: &str) -> Option<u64> {
        self.open.remove(key)
    }
}

/// Parse an ONVIF UTC time attribute (RFC 3339 / ISO 8601) into a naive UTC
/// datetime. Returns `None` for absent or unparseable values.
fn parse_utc(s: Option<&str>) -> Option<jiff::civil::DateTime> {
    let s = s?;
    s.parse::<jiff::Timestamp>()
        .ok()
        .map(crate::util::time::utc_civil)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::onvif::events::SimpleItem;

    fn motion(active: bool, topic: &str) -> NotificationMessage {
        NotificationMessage {
            topic: Some(topic.to_string()),
            data: vec![SimpleItem {
                name: "IsMotion".to_string(),
                value: active.to_string(),
            }],
            ..Default::default()
        }
    }

    #[test]
    fn default_config_is_internally_consistent() {
        let cfg = OnvifEventListenerConfig::default();
        // Renew must fire before the subscription (PT60S) lapses.
        assert!(cfg.renew_interval < Duration::from_secs(60));
        // Backoff bounds must be ordered.
        assert!(cfg.min_backoff <= cfg.max_backoff);
        assert!(cfg.message_limit > 0);
    }

    #[test]
    fn tracker_open_close_lifecycle() {
        let mut t = AlarmTracker::new();
        let topic = "tns1:VideoSource/MotionAlarm";
        assert!(!t.is_open(topic));
        t.set_open(topic.to_string(), 42);
        assert!(t.is_open(topic));
        assert_eq!(t.take_open(topic), Some(42));
        assert!(!t.is_open(topic));
        assert_eq!(t.take_open(topic), None);
    }

    #[test]
    fn tracker_keys_distinct_topics_independently() {
        let mut t = AlarmTracker::new();
        t.set_open("tns1:VideoSource/MotionAlarm".to_string(), 1);
        t.set_open("tns1:RuleEngine/TamperDetector/Tamper".to_string(), 2);
        assert_eq!(t.take_open("tns1:VideoSource/MotionAlarm"), Some(1));
        // Closing motion must not affect the tamper event.
        assert!(t.is_open("tns1:RuleEngine/TamperDetector/Tamper"));
    }

    #[test]
    fn notification_alarm_state_drives_edges() {
        // Sanity: the source events client resolves these to the expected
        // boolean states this listener keys on.
        assert_eq!(motion(true, "t").alarm_active(), Some(true));
        assert_eq!(motion(false, "t").alarm_active(), Some(false));
    }

    #[test]
    fn parse_utc_handles_rfc3339_and_garbage() {
        assert_eq!(
            parse_utc(Some("2026-06-20T07:50:49Z")),
            Some(jiff::civil::date(2026, 6, 20).at(7, 50, 49, 0))
        );
        assert_eq!(parse_utc(Some("not a time")), None);
        assert_eq!(parse_utc(None), None);
    }
}
