//! Event-retention reaper.
//!
//! Periodically enforces, per `Storage` row, a free-space floor / max age /
//! byte quota by deleting whole events oldest-first — media files **and** the
//! DB rows that reference them, so the UI never shows 404 playback. This is the
//! piece ZoneMinder implements with purge filters; here it's a few numeric
//! knobs (see [`crate::configure::retention::RetentionConfig`]).
//!
//! Safety rules (never violated):
//! * `Archived` events are never deleted — re-checked at delete time, not just
//!   when the candidates were listed.
//! * In-progress events (`EndDateTime IS NULL`) are never deleted.
//! * The newest event per monitor is always kept, even if a limit is still
//!   breached — a single huge open event shouldn't be force-killed.
//! * A storage whose directory is missing or empty is skipped: that is what an
//!   unmounted volume looks like, and its free-space reading would be the
//!   parent filesystem's.
//! * An event whose `DiskSpace` was never backfilled is measured on disk
//!   before it counts toward reclaimed space, and the free-space model is
//!   re-read from the filesystem every few deletions, so a pass cannot run
//!   away on a zero-credit loop.
//! * `max_deletes_per_pass` caps one pass on one storage.
//! * If an event's media cannot be removed the pass stops, rather than
//!   continuing to delete rows while the disk stays full.
//!
//! DB-before-disk ordering: a crash mid-delete leaves an orphan *file* (cheap
//! to reclaim) rather than an orphan *row* (shows as broken playback).

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use sea_orm::{
    ColumnTrait, Condition, DatabaseConnection, DbErr, EntityTrait, FromQueryResult, QueryFilter,
    QueryOrder,
};
use tracing::{debug, info, warn};

use crate::configure::retention::RetentionConfig;
use crate::entity::{events, storage};

/// Grace before the first pass after start, so the reaper never fires while
/// volumes are still mounting or the database is still coming up (#104).
const STARTUP_DELAY: Duration = Duration::from_secs(60);

/// Re-read free space from the filesystem after this many deletions, so the
/// credited model cannot drift far from reality (#105).
const RESYNC_EVERY: usize = 25;

pub struct RetentionService {
    db: Arc<DatabaseConnection>,
    config: RetentionConfig,
    /// Which `Servers` row this host is. A storage area that names another
    /// server holds files this host can't see, so it is left alone.
    this_server: Option<u32>,
    announced: crate::cluster::SkipLog,
}

/// Outcome of reaping a single storage, for logging.
#[derive(Default, Debug, PartialEq, Eq)]
pub(crate) struct ReapStats {
    pub(crate) deleted: usize,
    pub(crate) reclaimed: u64,
}

/// What happened to one event the loop asked to delete.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Reaped {
    /// Rows and media gone.
    Done,
    /// Rows gone, media removal failed.
    MediaFailed,
    /// Not deleted: it was archived or re-opened after the candidate list was
    /// taken.
    Skipped,
}

/// Everything the reap loop does outside the rows it was handed. The real
/// implementation talks to the database and filesystem; tests substitute a
/// fake so the loop's guards can be exercised without either.
#[async_trait]
pub(crate) trait ReapIo: Send + Sync {
    /// `(total_bytes, available_bytes)` for the filesystem holding `path`.
    fn fs_total_avail(&self, path: &str) -> Option<(u64, u64)>;
    /// Whether the storage directory exists and has something in it.
    fn storage_present(&self, path: &str) -> bool;
    /// Bytes the event's directory occupies on disk.
    async fn event_bytes(&self, storage_path: &str, ev: &events::Model) -> u64;
    /// Delete the event's rows, then its media (unless the storage's
    /// `DoDelete` is off, in which case ZoneMinder keeps the files — #108).
    async fn delete(&self, st: &storage::Model, ev: &events::Model) -> Result<Reaped, DbErr>;
}

impl RetentionService {
    pub fn new(db: Arc<DatabaseConnection>, config: RetentionConfig) -> Self {
        Self {
            db,
            config,
            this_server: None,
            announced: Default::default(),
        }
    }

    /// Record which `Servers` row is this host (`None` = single-server).
    pub fn with_server_id(mut self, server_id: Option<u32>) -> Self {
        self.this_server = server_id;
        self
    }

    /// Spawn the periodic reaper loop. Returns immediately.
    pub fn spawn(self: Arc<Self>) {
        let interval = self.config.interval();
        tokio::spawn(async move {
            tokio::time::sleep(STARTUP_DELAY).await;
            let mut ticker = tokio::time::interval(interval);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                ticker.tick().await;
                if let Err(e) = self.reap_once().await {
                    warn!("retention pass failed: {e}");
                }
            }
        });
    }

    /// One full evaluation across every storage. Public for tests / manual runs.
    pub async fn reap_once(&self) -> Result<(), DbErr> {
        let storages = storage::Entity::find()
            .order_by_asc(storage::Column::Id)
            .all(self.db.as_ref())
            .await?;
        // Events with StorageId 0 / NULL resolve to the lowest-id storage —
        // identical to `DaemonManager::zmnext_events_root`'s fallback.
        let default_id = storages.first().map(|s| s.id);

        for st in &storages {
            if st.enabled == 0 {
                debug!(
                    "retention: storage {} ({}) is disabled; skipping",
                    st.id, st.path
                );
                continue;
            }
            let is_default = Some(st.id) == default_id;
            match self.reap_storage(st, is_default).await {
                Ok(stats) if stats.deleted > 0 => info!(
                    "retention: storage {} ({}) {} {} events / {:.2} GiB",
                    st.id,
                    st.path,
                    if self.config.dry_run {
                        "would delete"
                    } else {
                        "deleted"
                    },
                    stats.deleted,
                    stats.reclaimed as f64 / GIB,
                ),
                Ok(_) => {}
                Err(e) => warn!("retention: storage {} ({}) failed: {e}", st.id, st.path),
            }
        }
        Ok(())
    }

    /// Reap a single storage in isolation and report how many events were
    /// deleted (or would be, under `dry_run`). Exposed for integration tests
    /// and targeted manual runs so a caller can exercise one storage without
    /// touching every other storage the way [`reap_once`](Self::reap_once)
    /// does. `is_default` mirrors the default-storage sentinel matching in
    /// `reap_once` (events with `StorageId` 0 / NULL belong to the lowest-id
    /// storage).
    pub async fn reap_storage_once(
        &self,
        st: &storage::Model,
        is_default: bool,
    ) -> Result<usize, DbErr> {
        Ok(self.reap_storage(st, is_default).await?.deleted)
    }

    async fn reap_storage(
        &self,
        st: &storage::Model,
        is_default: bool,
    ) -> Result<ReapStats, DbErr> {
        // Another server's storage: its files and its free space are not
        // visible from here, so neither its events nor its rows are touched.
        // The server that owns it runs its own reaper.
        if crate::cluster::skips_peer_storage(&self.announced, "retention", self.this_server, st) {
            return Ok(ReapStats::default());
        }
        // Candidate set: events on this storage that are safe to delete.
        let mut storage_match = Condition::any().add(events::Column::StorageId.eq(st.id));
        if is_default {
            storage_match = storage_match
                .add(events::Column::StorageId.is_null())
                .add(events::Column::StorageId.eq(0));
        }
        let all: Vec<events::Model> = events::Entity::find()
            .filter(storage_match)
            .filter(events::Column::Archived.eq(0))
            .filter(events::Column::EndDateTime.is_not_null())
            .order_by_asc(events::Column::StartDateTime)
            .all(self.db.as_ref())
            .await?;

        // The quota is on everything the storage holds, archived and open
        // events included — not just what is deletable (#110).
        #[derive(FromQueryResult)]
        struct Used {
            bytes: i64,
        }
        let sql = if is_default {
            "SELECT CAST(COALESCE(SUM(DiskSpace),0) AS SIGNED) AS bytes FROM Events \
             WHERE StorageId = ? OR StorageId IS NULL OR StorageId = 0"
        } else {
            "SELECT CAST(COALESCE(SUM(DiskSpace),0) AS SIGNED) AS bytes FROM Events \
             WHERE StorageId = ?"
        };
        let used_total = Used::find_by_statement(sea_orm::Statement::from_sql_and_values(
            self.db.get_database_backend(),
            sql,
            [st.id.into()],
        ))
        .one(self.db.as_ref())
        .await?
        .map(|u| u.bytes.max(0) as u64)
        .unwrap_or(0);

        let io = RealIo {
            db: Arc::clone(&self.db),
        };
        reap_candidates(&self.config, &io, st, all, used_total).await
    }
}

/// The reap loop proper, over an already-listed oldest-first candidate set.
pub(crate) async fn reap_candidates(
    cfg: &RetentionConfig,
    io: &dyn ReapIo,
    st: &storage::Model,
    all: Vec<events::Model>,
    used_total: u64,
) -> Result<ReapStats, DbErr> {
    if all.is_empty() {
        return Ok(ReapStats::default());
    }

    // An absent or empty storage directory is what an unmounted volume looks
    // like. Its rows are fine; its free space would be the parent filesystem's.
    if !io.storage_present(&st.path) {
        warn!(
            "retention: storage {} ({}) is missing or empty on disk (unmounted?); skipping",
            st.id, st.path
        );
        return Ok(ReapStats::default());
    }
    let Some((total, mut avail)) = io.fs_total_avail(&st.path) else {
        warn!(
            "retention: storage {} ({}) free space unreadable; skipping",
            st.id, st.path
        );
        return Ok(ReapStats::default());
    };

    // Total bytes held by this storage (every event, not just candidates).
    let mut used: u64 = used_total;

    // Protect the newest event per monitor: since `all` is oldest-first, the
    // last id seen per monitor is its newest.
    let mut newest: HashMap<u32, u64> = HashMap::new();
    for e in &all {
        newest.insert(e.monitor_id, e.id);
    }
    let protected: HashSet<u64> = newest.into_values().collect();

    let age_cutoff = (cfg.max_age_days > 0).then(|| {
        let age = jiff::SignedDuration::from_secs((cfg.max_age_days as i64).saturating_mul(86_400));
        crate::util::time::local_now().saturating_sub(age)
    });

    let mut stats = ReapStats::default();
    let mut since_resync = 0usize;
    for ev in all {
        if protected.contains(&ev.id) {
            continue;
        }
        let free_pct = if total > 0 {
            avail as f64 / total as f64 * 100.0
        } else {
            100.0
        };
        let over_free = cfg.min_free_pct > 0.0 && free_pct < cfg.min_free_pct;
        let over_bytes = cfg.max_bytes > 0 && used > cfg.max_bytes;
        let too_old = age_cutoff
            .zip(ev.start_date_time)
            .is_some_and(|(cut, start)| crate::util::time::civil(start) < cut);

        if !(over_free || over_bytes || too_old) {
            // Oldest-first: nothing further can be over-age, and space/quota
            // are satisfied, so we're done with this storage.
            break;
        }
        if cfg.max_deletes_per_pass > 0 && stats.deleted as u64 >= cfg.max_deletes_per_pass {
            info!(
                "retention: storage {} ({}) hit max_deletes_per_pass={}; continuing next pass",
                st.id, st.path, cfg.max_deletes_per_pass
            );
            break;
        }

        // A never-backfilled DiskSpace must not count as zero: crediting zero
        // never moves the free-space model, and the loop would drain the table.
        let bytes = match ev.disk_space {
            Some(b) => b,
            None => io.event_bytes(&st.path, &ev).await,
        };

        if cfg.dry_run {
            info!(
                "retention[dry-run]: would delete event {} (monitor {}, {:.1} MiB, start {:?})",
                ev.id,
                ev.monitor_id,
                bytes as f64 / MIB,
                ev.start_date_time
            );
        } else {
            match io.delete(st, &ev).await? {
                Reaped::Done => {
                    // One line per real deletion, with what triggered it — the
                    // retention guide tells operators to read these (#112).
                    let why = [
                        (over_free, "free space below floor"),
                        (over_bytes, "over byte quota"),
                        (too_old, "older than max_age_days"),
                    ]
                    .iter()
                    .filter(|(fired, _)| *fired)
                    .map(|(_, w)| *w)
                    .collect::<Vec<_>>()
                    .join(", ");
                    info!(
                        "retention: deleted event {} (monitor {}, {:.1} MiB): {why}",
                        ev.id,
                        ev.monitor_id,
                        bytes as f64 / MIB
                    );
                }
                Reaped::Skipped => {
                    debug!(
                        "retention: event {} archived or re-opened since listing; left alone",
                        ev.id
                    );
                    continue;
                }
                Reaped::MediaFailed => {
                    stats.deleted += 1;
                    warn!(
                        "retention: storage {} ({}) event {} rows removed but media could not be; \
                         stopping this pass rather than draining rows while the disk stays full",
                        st.id, st.path, ev.id
                    );
                    break;
                }
            }
        }
        avail = avail.saturating_add(bytes);
        used = used.saturating_sub(bytes);
        stats.deleted += 1;
        stats.reclaimed = stats.reclaimed.saturating_add(bytes);

        since_resync += 1;
        if !cfg.dry_run && since_resync >= RESYNC_EVERY {
            if let Some((_, real_avail)) = io.fs_total_avail(&st.path) {
                avail = real_avail;
            }
            since_resync = 0;
        }
    }

    Ok(stats)
}

/// Database + filesystem backing for the loop.
struct RealIo {
    db: Arc<DatabaseConnection>,
}

#[async_trait]
impl ReapIo for RealIo {
    fn fs_total_avail(&self, path: &str) -> Option<(u64, u64)> {
        fs_total_avail(path)
    }

    fn storage_present(&self, path: &str) -> bool {
        std::fs::read_dir(path)
            .map(|mut d| d.next().is_some())
            .unwrap_or(false)
    }

    async fn event_bytes(&self, storage_path: &str, ev: &events::Model) -> u64 {
        let dir = crate::service::event_storage::build_event_directory_path(
            storage_path,
            ev.monitor_id,
            ev.id,
            ev.start_date_time,
            &ev.scheme,
        );
        tokio::task::spawn_blocking(move || dir_size(&dir))
            .await
            .unwrap_or(0)
    }

    async fn delete(&self, st: &storage::Model, ev: &events::Model) -> Result<Reaped, DbErr> {
        // Re-check the safety rules on the live row: the candidate list may be
        // minutes old, and an operator archiving an event in that window must
        // win (#109).
        let live = events::Entity::find_by_id(ev.id)
            .one(self.db.as_ref())
            .await?;
        match live {
            Some(row) if row.archived == 0 && row.end_date_time.is_some() => {}
            _ => return Ok(Reaped::Skipped),
        }

        // DB rows first (children + event in one transaction); files last, so a
        // crash orphans a reclaimable file rather than a broken-playback row.
        // Both the delete and the media removal are shared with the interactive
        // `DELETE /events/{id}` path so the two can never diverge.
        crate::repo::events::delete_with_children(self.db.as_ref(), ev.id).await?;
        if st.do_delete == 0 {
            debug!(
                "retention: event {} rows removed; media kept (Storage.DoDelete off)",
                ev.id
            );
            return Ok(Reaped::Done);
        }
        if crate::service::event_storage::remove_event_dir(&st.path, ev).await {
            Ok(Reaped::Done)
        } else {
            Ok(Reaped::MediaFailed)
        }
    }
}

const GIB: f64 = 1024.0 * 1024.0 * 1024.0;
const MIB: f64 = 1024.0 * 1024.0;

/// `(total_bytes, available_bytes)` for the filesystem holding `path`.
fn fs_total_avail(path: &str) -> Option<(u64, u64)> {
    let s = nix::sys::statvfs::statvfs(Path::new(path)).ok()?;
    let frag = s.fragment_size() as u64;
    Some((s.blocks() as u64 * frag, s.blocks_available() as u64 * frag))
}

/// Sum of file sizes under `dir`, following no symlinks. Missing → 0.
fn dir_size(dir: &Path) -> u64 {
    let mut total = 0u64;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&d) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(meta) = entry.metadata() else { continue };
            if meta.is_dir() {
                stack.push(entry.path());
            } else if meta.is_file() {
                total = total.saturating_add(meta.len());
            }
        }
    }
    total
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entity::sea_orm_active_enums::{Orientation, Scheme, StorageType};
    use std::sync::Mutex;

    fn event(id: u64, monitor_id: u32, disk_space: Option<u64>) -> events::Model {
        use rust_decimal::Decimal;
        let column = crate::util::time::db_datetime;
        let start = jiff::civil::date(2026, 1, 1).at(0, 0, 0, 0)
            + jiff::SignedDuration::from_mins(id as i64);
        events::Model {
            id,
            monitor_id,
            storage_id: Some(1),
            secondary_storage_id: None,
            name: "ev".into(),
            cause: None,
            start_date_time: Some(column(start)),
            end_date_time: Some(column(start + jiff::SignedDuration::from_mins(1))),
            width: 0,
            height: 0,
            length: Decimal::new(0, 0),
            frames: Some(0),
            alarm_frames: Some(0),
            default_video: "ev-video.mp4".into(),
            save_jpe_gs: None,
            tot_score: 0,
            avg_score: None,
            max_score: None,
            max_score_frame_id: None,
            archived: 0,
            videoed: 0,
            uploaded: 0,
            emailed: 0,
            messaged: 0,
            executed: 0,
            notes: None,
            state_id: 1,
            orientation: Orientation::Rotate0,
            disk_space,
            scheme: Scheme::Shallow,
            locked: 0,
            latitude: None,
            longitude: None,
        }
    }

    fn storage() -> storage::Model {
        storage::Model {
            id: 1,
            path: "/var/lib/zoneminder/events".into(),
            name: "store".into(),
            r#type: StorageType::Local,
            url: None,
            disk_space: None,
            scheme: Scheme::Shallow,
            server_id: None,
            do_delete: 1,
            enabled: 1,
        }
    }

    fn cfg(min_free_pct: f64) -> RetentionConfig {
        RetentionConfig {
            enabled: true,
            min_free_pct,
            max_deletes_per_pass: 0,
            ..RetentionConfig::default()
        }
    }

    /// Fake disk + database. Free space really moves as events are deleted,
    /// which is exactly the coupling the real loop has to model correctly.
    struct Fake {
        total: u64,
        avail: Mutex<u64>,
        present: bool,
        bytes_per_event: u64,
        media_fails_for: Option<u64>,
        deleted: Mutex<Vec<u64>>,
    }

    impl Fake {
        fn new(total: u64, avail: u64) -> Self {
            Self {
                total,
                avail: Mutex::new(avail),
                present: true,
                bytes_per_event: 1,
                media_fails_for: None,
                deleted: Mutex::new(Vec::new()),
            }
        }
        fn deleted(&self) -> Vec<u64> {
            self.deleted.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl ReapIo for Fake {
        fn fs_total_avail(&self, _: &str) -> Option<(u64, u64)> {
            Some((self.total, *self.avail.lock().unwrap()))
        }
        fn storage_present(&self, _: &str) -> bool {
            self.present
        }
        async fn event_bytes(&self, _: &str, _: &events::Model) -> u64 {
            self.bytes_per_event
        }
        async fn delete(&self, _: &storage::Model, ev: &events::Model) -> Result<Reaped, DbErr> {
            self.deleted.lock().unwrap().push(ev.id);
            *self.avail.lock().unwrap() += self.bytes_per_event;
            Ok(if self.media_fails_for == Some(ev.id) {
                Reaped::MediaFailed
            } else {
                Reaped::Done
            })
        }
    }

    fn fifty_events_no_disk_space() -> Vec<events::Model> {
        (1..=50).map(|i| event(i, 1, None)).collect()
    }

    /// Regression for #105: with `DiskSpace` NULL the loop credited zero per
    /// deletion, the free-space model never moved, and every event on the
    /// storage except the newest went in one pass.
    #[tokio::test]
    async fn null_disk_space_is_measured_so_the_pass_stops_at_the_floor() {
        // 9% free with a 10% floor: one measured deletion is enough.
        let io = Fake::new(100, 9);
        let stats = reap_candidates(&cfg(10.0), &io, &storage(), fifty_events_no_disk_space(), 0)
            .await
            .unwrap();
        assert_eq!(
            io.deleted(),
            vec![1],
            "one event brings free space to the floor"
        );
        assert_eq!(stats.deleted, 1);
        assert_eq!(stats.reclaimed, 1);
    }

    /// Regression for #104: an unmounted storage looks like an empty directory
    /// on the parent filesystem, whose free space says nothing about the
    /// volume. Its rows must be left alone.
    #[tokio::test]
    async fn absent_storage_directory_is_skipped() {
        let mut io = Fake::new(100, 1);
        io.present = false;
        let stats = reap_candidates(&cfg(10.0), &io, &storage(), fifty_events_no_disk_space(), 0)
            .await
            .unwrap();
        assert!(io.deleted().is_empty());
        assert_eq!(stats, ReapStats::default());
    }

    /// Regression for #106: when media removal fails the pass stops instead
    /// of deleting the next row, and the next, while the disk stays full.
    #[tokio::test]
    async fn media_removal_failure_stops_the_pass() {
        let mut io = Fake::new(100, 0);
        io.media_fails_for = Some(3);
        let stats = reap_candidates(&cfg(50.0), &io, &storage(), fifty_events_no_disk_space(), 0)
            .await
            .unwrap();
        assert_eq!(io.deleted(), vec![1, 2, 3]);
        assert_eq!(stats.deleted, 3);
    }

    #[tokio::test]
    async fn max_deletes_per_pass_caps_one_pass() {
        let io = Fake::new(100, 0);
        let mut c = cfg(50.0);
        c.max_deletes_per_pass = 4;
        let stats = reap_candidates(&c, &io, &storage(), fifty_events_no_disk_space(), 0)
            .await
            .unwrap();
        assert_eq!(stats.deleted, 4);
    }

    /// Nothing breached, nothing deleted — and the byte quota counts the
    /// storage's whole footprint, passed in, not just the candidates (#110).
    #[tokio::test]
    async fn quota_counts_the_whole_storage_and_stops_when_satisfied() {
        let io = Fake::new(100, 90);
        let mut c = cfg(0.0);
        c.max_bytes = 50;
        let events: Vec<events::Model> = (1..=10).map(|i| event(i, 1, Some(10))).collect();
        // 100 bytes used storage-wide, quota 50: five 10-byte deletions.
        let stats = reap_candidates(&c, &io, &storage(), events.clone(), 100)
            .await
            .unwrap();
        assert_eq!(stats.deleted, 5);

        let io = Fake::new(100, 90);
        let stats = reap_candidates(&c, &io, &storage(), events, 40)
            .await
            .unwrap();
        assert_eq!(stats.deleted, 0, "under quota: nothing to do");
    }

    /// max_age_days on its own: everything older than the cutoff goes, the
    /// newest per monitor stays (#111).
    #[tokio::test]
    async fn age_cutoff_reaps_old_events_only() {
        let io = Fake::new(100, 90);
        let mut c = cfg(0.0);
        c.max_age_days = 1;
        let mut events = fifty_events_no_disk_space(); // all dated January 2026
        let mut fresh = event(99, 1, Some(1));
        fresh.start_date_time = Some(crate::util::time::db_datetime(
            crate::util::time::local_now(),
        ));
        events.push(fresh);
        let stats = reap_candidates(&c, &io, &storage(), events, 0)
            .await
            .unwrap();
        assert_eq!(
            stats.deleted, 50,
            "every January event; the fresh one is newest and kept"
        );
        assert!(!io.deleted().contains(&99));
    }

    /// The safety rules from the module docs, on the loop itself: the newest
    /// event per monitor is protected, and a `Skipped` (archived since
    /// listing) event neither counts nor stops the pass.
    #[tokio::test]
    async fn newest_per_monitor_is_kept_and_skipped_events_do_not_count() {
        struct SkipTwo(Fake);
        #[async_trait]
        impl ReapIo for SkipTwo {
            fn fs_total_avail(&self, p: &str) -> Option<(u64, u64)> {
                self.0.fs_total_avail(p)
            }
            fn storage_present(&self, p: &str) -> bool {
                self.0.storage_present(p)
            }
            async fn event_bytes(&self, p: &str, e: &events::Model) -> u64 {
                self.0.event_bytes(p, e).await
            }
            async fn delete(&self, s: &storage::Model, e: &events::Model) -> Result<Reaped, DbErr> {
                if e.id == 2 {
                    return Ok(Reaped::Skipped);
                }
                self.0.delete(s, e).await
            }
        }
        let io = SkipTwo(Fake::new(100, 0));
        let events = vec![
            event(1, 1, Some(1)),
            event(2, 1, Some(1)),
            event(3, 1, Some(1)),
            event(4, 2, Some(1)),
        ];
        let stats = reap_candidates(&cfg(50.0), &io, &storage(), events, 0)
            .await
            .unwrap();
        // 3 and 4 are the newest for monitors 1 and 2; 2 was skipped.
        assert_eq!(io.0.deleted(), vec![1]);
        assert_eq!(stats.deleted, 1);
    }
}
