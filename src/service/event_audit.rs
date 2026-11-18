//! Legacy's "Audit Events Report" (`views/report_event_audit.php`) (#36).

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use crate::util::time;
use jiff::civil::DateTime as Civil;
use sea_orm::{ColumnTrait, EntityTrait, PaginatorTrait, QueryFilter, QueryOrder};

use crate::dto::response::event_audit::{EventAuditResponse, MonitorEventAudit};
use crate::entity::events;
use crate::error::{AppError, AppResult};
use crate::server::state::AppState;
use crate::service::event_storage::{build_event_directory_path, resolve_event_storage_path};
use crate::service::monitor_acl::MonitorScope;
use crate::util::authz::Level;

/// Most events one report checks on disk. A wider window is a 400: the
/// report stats every event's media.
pub const MAX_EVENTS: u64 = 10_000;

/// Whether an event's media is there: `{dir}/{DefaultVideo}`, or the
/// directory itself for an event with no video (JPEG-only), as legacy's
/// `Event::file_exists`/`file_size` do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaCheck {
    Present,
    Missing,
    Empty,
}

pub async fn check_media(dir: &Path, default_video: &str) -> MediaCheck {
    if default_video.is_empty() {
        return match tokio::fs::metadata(dir).await {
            Ok(m) if m.is_dir() => MediaCheck::Present,
            _ => MediaCheck::Missing,
        };
    }
    match tokio::fs::metadata(dir.join(default_video)).await {
        Ok(m) if m.len() == 0 => MediaCheck::Empty,
        Ok(_) => MediaCheck::Present,
        Err(_) => MediaCheck::Missing,
    }
}

/// One monitor's events in id order: count, span, and the gaps between them.
#[derive(Debug, Default)]
struct Acc {
    count: u64,
    first_id: u64,
    last_id: u64,
    first_start: Option<Civil>,
    last_end: Option<Civil>,
    prev_end: Option<Civil>,
    min_gap: Option<i64>,
    max_gap: Option<i64>,
    missing: Vec<u64>,
    zero: Vec<u64>,
}

impl Acc {
    fn push(&mut self, id: u64, start: Option<Civil>, end: Option<Civil>) {
        if self.count == 0 {
            self.first_id = id;
            self.first_start = start;
        }
        if let (Some(prev), Some(start)) = (self.prev_end, start) {
            let gap = start.duration_since(prev).as_secs();
            self.min_gap = Some(self.min_gap.map_or(gap, |g| g.min(gap)));
            self.max_gap = Some(self.max_gap.map_or(gap, |g| g.max(gap)));
        }
        self.count += 1;
        self.last_id = id;
        self.last_end = end.or(self.last_end);
        self.prev_end = end;
    }
}

/// A local-time column value, as UTC, the way every event response reports it.
fn rfc3339(t: Civil) -> String {
    crate::util::naive_local_to_utc(t)
        .strftime("%Y-%m-%dT%H:%M:%SZ")
        .to_string()
}

pub async fn event_audit(
    state: &AppState,
    start: Civil,
    end: Civil,
    monitor_id: Option<u32>,
    scope: &MonitorScope,
) -> AppResult<EventAuditResponse> {
    if end <= start {
        return Err(AppError::BadRequestError("end must be after start".into()));
    }
    // Events overlapping the window, as legacy selects them.
    let mut query = events::Entity::find()
        .filter(events::Column::EndDateTime.gt(time::db_datetime(start)))
        .filter(events::Column::StartDateTime.lt(time::db_datetime(end)));
    match monitor_id {
        Some(id) if scope.allows(id, Level::View) => {
            query = query.filter(events::Column::MonitorId.eq(id));
        }
        Some(_) => query = query.filter(events::Column::MonitorId.is_in(Vec::<u32>::new())),
        None => {
            if let Some(ids) = scope.visible_ids(Level::View) {
                query = query.filter(events::Column::MonitorId.is_in(ids));
            }
        }
    }
    let total = query.clone().count(state.db()).await?;
    if total > MAX_EVENTS {
        return Err(AppError::BadRequestError(format!(
            "{total} events in the window; narrow it to at most {MAX_EVENTS}"
        )));
    }
    let rows = query
        .order_by_asc(events::Column::Id)
        .all(state.db())
        .await?;

    // Events on another server's storage can't be checked from here: their
    // files are not visible, so every one would be reported as missing. They
    // are counted, and their media left to the server that holds it.
    let elsewhere: std::collections::HashSet<u16> = crate::repo::storage::find_all(state.db())
        .await?
        .into_iter()
        .filter(|st| {
            matches!(
                state.cluster.owner_of_storage(st.server_id),
                crate::cluster::Owner::Peer(_)
            )
        })
        .map(|st| st.id)
        .collect();

    // Storage roots resolve once per storage, not once per event.
    let mut roots: HashMap<Option<u16>, Option<String>> = HashMap::new();
    let mut by_monitor: BTreeMap<u32, Acc> = BTreeMap::new();
    for e in &rows {
        let acc = by_monitor.entry(e.monitor_id).or_default();
        acc.push(
            e.id,
            e.start_date_time.map(time::civil),
            e.end_date_time.map(time::civil),
        );

        if e.storage_id.is_some_and(|id| elsewhere.contains(&id)) {
            continue;
        }
        let root = match roots.get(&e.storage_id) {
            Some(root) => root.clone(),
            None => {
                let root = resolve_event_storage_path(state, e).await.ok();
                roots.insert(e.storage_id, root.clone());
                root
            }
        };
        let check = match root {
            Some(root) => {
                let dir: PathBuf = build_event_directory_path(
                    &root,
                    e.monitor_id,
                    e.id,
                    e.start_date_time,
                    &e.scheme,
                );
                check_media(&dir, &e.default_video).await
            }
            None => MediaCheck::Missing,
        };
        match check {
            MediaCheck::Missing => acc.missing.push(e.id),
            MediaCheck::Empty => acc.zero.push(e.id),
            MediaCheck::Present => {}
        }
    }

    Ok(EventAuditResponse {
        start: rfc3339(start),
        end: rfc3339(end),
        monitors: by_monitor
            .into_iter()
            .map(|(monitor_id, a)| MonitorEventAudit {
                monitor_id,
                event_count: a.count,
                first_event_id: a.first_id,
                last_event_id: a.last_id,
                first_start: a.first_start.map(rfc3339),
                last_end: a.last_end.map(rfc3339),
                min_gap_seconds: a.min_gap,
                max_gap_seconds: a.max_gap,
                missing_files: a.missing,
                zero_size: a.zero,
            })
            .collect(),
    })
}

/// The default window, as legacy's: the hour ending an hour ago.
pub fn default_window() -> (Civil, Civil) {
    // In the server-local time the event columns hold (#173).
    let now = time::local_now();
    (
        now.saturating_sub(jiff::SignedDuration::from_hours(2)),
        now.saturating_sub(jiff::SignedDuration::from_hours(1)),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(s: &str) -> Option<Civil> {
        Some(Civil::strptime("%Y-%m-%d %H:%M:%S", s).unwrap())
    }

    #[test]
    fn gaps_run_from_one_events_end_to_the_next_ones_start() {
        let mut a = Acc::default();
        a.push(1, t("2026-10-18 10:00:00"), t("2026-10-18 10:01:00"));
        a.push(2, t("2026-10-18 10:01:30"), t("2026-10-18 10:02:00")); // 30 s gap
        a.push(3, t("2026-10-18 10:01:50"), t("2026-10-18 10:05:00")); // 10 s overlap
        assert_eq!((a.count, a.first_id, a.last_id), (3, 1, 3));
        assert_eq!((a.min_gap, a.max_gap), (Some(-10), Some(30)));
        assert_eq!(a.last_end, t("2026-10-18 10:05:00"));

        let mut one = Acc::default();
        one.push(9, t("2026-10-18 10:00:00"), None);
        assert_eq!((one.min_gap, one.max_gap), (None, None));
    }

    #[tokio::test]
    async fn media_is_present_missing_or_empty() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("1-video.mp4"), b"x").unwrap();
        std::fs::write(dir.path().join("2-video.mp4"), b"").unwrap();
        assert_eq!(
            check_media(dir.path(), "1-video.mp4").await,
            MediaCheck::Present
        );
        assert_eq!(
            check_media(dir.path(), "2-video.mp4").await,
            MediaCheck::Empty
        );
        assert_eq!(
            check_media(dir.path(), "3-video.mp4").await,
            MediaCheck::Missing
        );
        // No video: the directory decides.
        assert_eq!(check_media(dir.path(), "").await, MediaCheck::Present);
        assert_eq!(
            check_media(&dir.path().join("nope"), "").await,
            MediaCheck::Missing
        );
    }
}
