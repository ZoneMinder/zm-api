//! Native replacement for `zmaudit.pl` — database-side consistency.
//!
//! Four checks, each independently switchable:
//!
//! * orphaned `Frames` / `Stats` rows whose event is gone
//! * events that never recorded a frame
//! * events left unclosed by a capture daemon that died
//! * `Event_Summaries` and `Storage.DiskSpace` counters that have drifted
//!
//! ## Two deliberate departures from the Perl
//!
//! **Archived events are genuinely skipped.** zmaudit means to skip them when
//! deleting frameless events — `if ($$event{Archived})` — but its `SELECT` list
//! is `E.Id, E.StartDateTime, F.EventId`, so `Archived` is never fetched, the
//! guard is always false, and archived events are deleted. Archiving an event
//! is a user saying "keep this"; the query here fetches the column and honours
//! it.
//!
//! **`dry_run` means dry.** zmaudit's `--report` suppresses its deletes but
//! still performs row updates, empty-directory removal, stray-image unlinking,
//! log pruning and counter resyncs, so "just report" is not what it does. Here
//! nothing is written when `dry_run` is set.
//!
//! ## What is not here yet
//!
//! The filesystem half — reconciling event directories against `Events` rows in
//! both directions. That is where zmaudit's real hazard lives: the `rm -rf`
//! target is *derived* from `StartDateTime` formatted in the process's local
//! timezone, so a timezone mismatch between the recording daemon and the
//! auditor aims it at a directory that was never the event's. Doing that safely
//! wants its own change, and `service::event_storage` already centralises the
//! path derivation it will need.

pub mod filesystem;

use std::sync::Arc;

use sea_orm::{ConnectionTrait, DatabaseConnection, DbErr, FromQueryResult, Statement};
use tracing::{debug, info, warn};

use crate::configure::maintenance::AuditConfig;

pub struct AuditService {
    db: Arc<DatabaseConnection>,
    config: AuditConfig,
    /// Which `Servers` row this host is. A storage area that names another
    /// server holds files this host can't see, so it is left alone.
    this_server: Option<u32>,
    announced: crate::cluster::SkipLog,
    /// Orphans seen on previous passes, and how many times.
    ///
    /// Nothing is acted on until it has been seen `confirmations_required`
    /// times running, so a row committed after a walk began — or a directory
    /// created just before its row — is never mistaken for an orphan. Held in
    /// memory rather than persisted: a restart resets the count, which errs
    /// toward doing nothing.
    seen_orphans: tokio::sync::Mutex<OrphanTracker>,
}

/// Consecutive-sighting counts, keyed separately for the two directions.
#[derive(Default)]
struct OrphanTracker {
    directories: std::collections::HashMap<std::path::PathBuf, u32>,
    rows: std::collections::HashMap<u64, u32>,
}

impl OrphanTracker {
    /// Record this pass's candidates and return those seen enough times.
    ///
    /// Anything absent from `current` is dropped, so the count means
    /// *consecutive* sightings — an orphan that reappears starts again.
    fn confirm<K: Clone + std::hash::Hash + Eq>(
        counts: &mut std::collections::HashMap<K, u32>,
        current: &[K],
        required: u32,
    ) -> Vec<K> {
        let present: std::collections::HashSet<&K> = current.iter().collect();
        counts.retain(|k, _| present.contains(k));

        // One sighting per pass, however many times a key was reported: two
        // Storage rows sharing a path used to confirm an orphan in one pass (#93).
        let mut confirmed = Vec::new();
        for key in present {
            let count = counts.entry(key.clone()).or_insert(0);
            *count += 1;
            if *count >= required.max(1) {
                confirmed.push(key.clone());
            }
        }
        confirmed
    }
}

/// What one pass found, and whether it acted.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct AuditReport {
    pub orphaned_frames: u64,
    pub orphaned_stats: u64,
    pub empty_events: u64,
    pub unclosed_events: u64,
    /// Directories with no `Events` row, moved to quarantine.
    pub quarantined_dirs: u64,
    /// `Events` rows whose directory was not found on disk.
    pub rows_without_media: u64,
    /// Directories that look like events but name no id. Never touched.
    pub unidentified_dirs: u64,
    /// Storages where a precondition or the derivation canary refused the
    /// filesystem half. Present in the report so a refusal is visible rather
    /// than only in the log.
    pub refusals: Vec<String>,
    /// Jobs that failed this pass. A job that fails every pass used to be
    /// visible only as a warn line; the DiskSpace resync failed that way for
    /// its whole life (#90).
    pub errors: Vec<String>,
    pub dry_run: bool,
}

impl AuditReport {
    fn fail(&mut self, job: &str, e: DbErr) {
        warn!("{job} failed: {e}");
        self.errors.push(format!("{job}: {e}"));
    }

    pub fn total(&self) -> u64 {
        self.orphaned_frames
            + self.orphaned_stats
            + self.empty_events
            + self.unclosed_events
            + self.quarantined_dirs
            + self.rows_without_media
    }

    pub fn is_clean(&self) -> bool {
        self.total() == 0 && self.errors.is_empty()
    }
}

#[derive(FromQueryResult)]
struct CountRow {
    n: i64,
}

#[derive(FromQueryResult)]
struct IdRow {
    id: u64,
}

/// Just enough of an event to derive where it should be on disk.
#[derive(FromQueryResult)]
struct EventLocation {
    id: u64,
    monitor_id: u32,
    scheme: String,
    start_date_time: Option<sea_orm::prelude::DateTime>,
}

impl AuditService {
    pub fn new(db: Arc<DatabaseConnection>, config: AuditConfig) -> Self {
        Self {
            db,
            config,
            this_server: None,
            announced: Default::default(),
            seen_orphans: tokio::sync::Mutex::new(OrphanTracker::default()),
        }
    }

    /// Record which `Servers` row is this host (`None` = single-server).
    pub fn with_server_id(mut self, server_id: Option<u32>) -> Self {
        self.this_server = server_id;
        self
    }

    /// The storage rows whose files this host can see. Walking another
    /// server's storage would find nothing, call every event on it
    /// media-less, and delete their rows.
    async fn local_storages(&self) -> Result<Vec<crate::entity::storage::Model>, DbErr> {
        use sea_orm::EntityTrait;
        Ok(crate::entity::storage::Entity::find()
            .all(self.db.as_ref())
            .await?
            .into_iter()
            .filter(|st| {
                !crate::cluster::skips_peer_storage(&self.announced, "audit", self.this_server, st)
            })
            .collect())
    }

    pub fn spawn(self: Arc<Self>) {
        let interval = self.config.interval();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(interval);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                ticker.tick().await;
                match self.run_once().await {
                    Ok(report) if report.is_clean() => debug!("audit pass: nothing to do"),
                    Ok(report) => info!(
                        "audit pass{}: {} orphaned frames, {} orphaned stats, \
                         {} empty events, {} unclosed events",
                        if report.dry_run { " (dry run)" } else { "" },
                        report.orphaned_frames,
                        report.orphaned_stats,
                        report.empty_events,
                        report.unclosed_events,
                    ),
                    Err(e) => warn!("audit pass failed: {e}"),
                }
            }
        });
    }

    /// One full pass. Jobs are independent — one failing does not stop the rest.
    pub async fn run_once(&self) -> Result<AuditReport, DbErr> {
        let mut report = AuditReport {
            dry_run: self.config.dry_run,
            ..Default::default()
        };

        if self.config.remove_orphaned_frames {
            match self.sweep_orphaned_children().await {
                Ok((frames, stats)) => {
                    report.orphaned_frames = frames;
                    report.orphaned_stats = stats;
                }
                Err(e) => report.fail("orphan sweep", e),
            }
        }
        if self.config.remove_empty_events {
            match self.remove_empty_events().await {
                Ok(n) => report.empty_events = n,
                Err(e) => report.fail("empty-event sweep", e),
            }
        }
        if self.config.close_unclosed_events {
            match self.close_unclosed_events().await {
                Ok(n) => report.unclosed_events = n,
                Err(e) => report.fail("unclosed-event sweep", e),
            }
        }
        if self.config.resync_counters {
            if let Err(e) = self.resync_counters().await {
                report.fail("counter resync", e);
            }
        }
        if self.config.filesystem.enabled {
            if let Err(e) = self.reconcile_filesystem(&mut report).await {
                report.fail("filesystem reconciliation", e);
            }
        }

        Ok(report)
    }

    /// Reconcile event directories against `Events` rows, per storage.
    ///
    /// Every storage is checked independently and a refusal on one does not
    /// stop the others — a single unmounted volume should not disable the
    /// audit for the whole install.
    async fn reconcile_filesystem(&self, report: &mut AuditReport) -> Result<(), DbErr> {
        let fs = &self.config.filesystem;
        let storages = self.local_storages().await?;

        // Candidates gathered across all storages, so the confirmation tracker
        // sees one coherent view per pass rather than one per storage.
        let mut orphan_dirs: Vec<(std::path::PathBuf, u64)> = Vec::new();
        let mut rows_missing: Vec<u64> = Vec::new();
        let mut walked_roots: std::collections::HashSet<std::path::PathBuf> = Default::default();

        for st in &storages {
            if st.enabled == 0 {
                continue;
            }
            let root = std::path::PathBuf::from(&st.path);
            // Two enabled Storage rows on one directory would report every
            // orphan twice per pass (#93). Walk each directory once.
            let canonical = std::fs::canonicalize(&root).unwrap_or_else(|_| root.clone());
            if !walked_roots.insert(canonical) {
                warn!(
                    "storage {} shares its path {} with another enabled storage; \
                     walking it once",
                    st.id, st.path
                );
                continue;
            }

            let walk = tokio::task::spawn_blocking({
                let root = root.clone();
                let quarantine = fs.quarantine_dir.clone();
                let depth = fs.max_depth;
                move || filesystem::walk_storage(&root, depth, &quarantine)
            })
            .await
            .map_err(|e| DbErr::Custom(format!("storage walk panicked: {e}")))?;

            if let Err(refusal) = filesystem::check_preconditions(&root, &walk) {
                warn!("storage {}: {refusal}", st.id);
                report.refusals.push(refusal.to_string());
                continue;
            }
            report.unidentified_dirs += walk.unidentified.len() as u64;
            for dir in &walk.unidentified {
                debug!("not identifiable, leaving alone: {}", dir.path.display());
            }

            // Which of the events we found does the database still have?
            let found_ids: Vec<u64> = walk.events.iter().map(|e| e.event_id).collect();
            let known = self.known_event_ids(&found_ids).await?;

            // Canary: for events present in both, does derivation agree with
            // where they actually are? Systemic disagreement means something
            // about the layout is not what we think, so nothing is moved.
            let derived = self.derive_paths(&st.path, &known).await?;
            match filesystem::derivation_canary(&walk.events, &derived, 0.1) {
                Ok((checked, mismatched)) if mismatched > 0 => debug!(
                    "storage {}: {mismatched}/{checked} events not where derivation expects",
                    st.id
                ),
                Ok(_) => {}
                Err(refusal) => {
                    warn!("storage {}: {refusal}", st.id);
                    report.refusals.push(refusal.to_string());
                    continue;
                }
            }

            for event in &walk.events {
                if known.contains_key(&event.event_id)
                    || !filesystem::is_within_root(&root, &event.path)
                {
                    continue;
                }
                // A directory can exist before its row is committed; the
                // grace period the config promises has to apply here too (#95).
                if filesystem::is_younger_than(&event.path, self.config.min_age_seconds) {
                    debug!(
                        "{} is younger than min_age; not an orphan yet",
                        event.path.display()
                    );
                    continue;
                }
                orphan_dirs.push((event.path.clone(), event.event_id));
            }

            if fs.remove_rows_without_media {
                let on_disk: std::collections::HashSet<u64> = found_ids.into_iter().collect();
                let absent = self.rows_absent_from(&st.id, &on_disk).await?;
                // A volume that failed to mount still gets a monitor directory
                // and a few new events written under the bare mount point, so
                // the walk passes its preconditions while every older row has
                // "no media". More rows missing than events found is that
                // shape, not a few broken rows; refuse (#89).
                if absent.len() > on_disk.len() {
                    let refusal = format!(
                        "storage {} ({}): {} rows have no media but only {} events are on disk; \
                         refusing to delete rows (unmounted volume?)",
                        st.id,
                        st.path,
                        absent.len(),
                        on_disk.len()
                    );
                    warn!("{refusal}");
                    report.refusals.push(refusal);
                } else {
                    rows_missing.extend(absent);
                }
            }
        }

        // Only act on what has been seen often enough.
        let mut tracker = self.seen_orphans.lock().await;
        let required = fs.confirmations_required;
        let dir_keys: Vec<std::path::PathBuf> =
            orphan_dirs.iter().map(|(p, _)| p.clone()).collect();
        let confirmed_dirs = OrphanTracker::confirm(&mut tracker.directories, &dir_keys, required);
        let confirmed_rows = OrphanTracker::confirm(&mut tracker.rows, &rows_missing, required);
        drop(tracker);

        let pending = dir_keys.len() - confirmed_dirs.len();
        if pending > 0 {
            debug!("{pending} orphan directories awaiting confirmation");
        }

        if fs.quarantine_orphaned_dirs && !confirmed_dirs.is_empty() {
            let ids: std::collections::HashMap<&std::path::PathBuf, u64> =
                orphan_dirs.iter().map(|(p, id)| (p, *id)).collect();
            report.quarantined_dirs = self.quarantine(&confirmed_dirs, &ids, &storages).await;
        }

        if fs.remove_rows_without_media && !confirmed_rows.is_empty() {
            report.rows_without_media = self.delete_rows(&confirmed_rows).await?;
        }

        self.sweep_quarantine(&storages).await;
        Ok(())
    }

    /// Which of these event ids still exist, with what the row says about
    /// their location.
    async fn known_event_ids(
        &self,
        ids: &[u64],
    ) -> Result<std::collections::HashMap<u64, EventLocation>, DbErr> {
        if ids.is_empty() {
            return Ok(Default::default());
        }
        let list = ids
            .iter()
            .map(|i| i.to_string())
            .collect::<Vec<_>>()
            .join(",");
        let rows = EventLocation::find_by_statement(Statement::from_string(
            self.db.get_database_backend(),
            format!(
                "SELECT Id AS id, MonitorId AS monitor_id, Scheme AS scheme, \
                        StartDateTime AS start_date_time \
                 FROM Events WHERE Id IN ({list})"
            ),
        ))
        .all(self.db.as_ref())
        .await?;
        Ok(rows.into_iter().map(|r| (r.id, r)).collect())
    }

    /// Where derivation says each known event should be. Canary input only —
    /// never used to locate anything for deletion.
    async fn derive_paths(
        &self,
        storage_path: &str,
        known: &std::collections::HashMap<u64, EventLocation>,
    ) -> Result<std::collections::HashMap<u64, std::path::PathBuf>, DbErr> {
        use crate::entity::sea_orm_active_enums::Scheme;
        Ok(known
            .values()
            .map(|e| {
                let scheme = match e.scheme.as_str() {
                    "Deep" => Scheme::Deep,
                    "Medium" => Scheme::Medium,
                    _ => Scheme::Shallow,
                };
                (
                    e.id,
                    crate::service::event_storage::build_event_directory_path(
                        storage_path,
                        e.monitor_id,
                        e.id,
                        e.start_date_time,
                        &scheme,
                    ),
                )
            })
            .collect())
    }

    /// Events belonging to this storage, old enough to judge, that the walk
    /// did not find on disk.
    async fn rows_absent_from(
        &self,
        storage_id: &u16,
        on_disk: &std::collections::HashSet<u64>,
    ) -> Result<Vec<u64>, DbErr> {
        let min_age = self.config.min_age_seconds;
        let rows = IdRow::find_by_statement(Statement::from_string(
            self.db.get_database_backend(),
            format!(
                // Unbounded on purpose: the cap belongs on deletions
                // (`delete_rows`), not on which rows get looked at (#91).
                "SELECT Id AS id FROM Events \
                 WHERE StorageId = {storage_id} \
                   AND Archived = 0 \
                   AND StartDateTime IS NOT NULL \
                   AND StartDateTime < DATE_SUB(NOW(), INTERVAL {min_age} SECOND)"
            ),
        ))
        .all(self.db.as_ref())
        .await?;
        Ok(rows
            .into_iter()
            .map(|r| r.id)
            .filter(|id| !on_disk.contains(id))
            .collect())
    }

    /// Move confirmed orphan directories into quarantine.
    ///
    /// A rename inside the storage root, so it is atomic and reversible for
    /// `quarantine_retention_days`. Returns how many moved.
    async fn quarantine(
        &self,
        dirs: &[std::path::PathBuf],
        ids: &std::collections::HashMap<&std::path::PathBuf, u64>,
        storages: &[crate::entity::storage::Model],
    ) -> u64 {
        let fs = &self.config.filesystem;
        let stamp = jiff::Timestamp::now().strftime("%Y%m%dT%H%M%S").to_string();
        let cap = self.config.max_deletes_per_pass;
        let mut moved = 0u64;

        for dir in dirs.iter().take(cap) {
            let Some(root) = storages
                .iter()
                .map(|s| std::path::PathBuf::from(&s.path))
                .find(|r| filesystem::is_within_root(r, dir))
            else {
                warn!("{} is not under any storage root; skipping", dir.display());
                continue;
            };

            let target = filesystem::quarantine_target(
                &root,
                &fs.quarantine_dir,
                &stamp,
                ids.get(dir).copied(),
                dir,
            );

            if self.config.dry_run {
                info!(
                    "dry run: would quarantine {} -> {}",
                    dir.display(),
                    target.display()
                );
                moved += 1;
                continue;
            }

            if let Some(parent) = target.parent() {
                if let Err(e) = std::fs::create_dir_all(parent) {
                    warn!("cannot create quarantine {}: {e}", parent.display());
                    continue;
                }
            }
            match std::fs::rename(dir, &target) {
                Ok(()) => {
                    info!("quarantined {} -> {}", dir.display(), target.display());
                    moved += 1;
                }
                Err(e) => warn!("cannot quarantine {}: {e}", dir.display()),
            }
        }

        if dirs.len() > cap {
            warn!(
                "{} orphaned directories exceeds max_deletes_per_pass ({cap}); \
                 quarantined {cap} this pass. If this repeats, check the storage \
                 configuration before assuming they are really orphaned.",
                dirs.len()
            );
        }
        moved
    }

    async fn delete_rows(&self, ids: &[u64]) -> Result<u64, DbErr> {
        if self.config.dry_run {
            info!("dry run: would delete {} events with no media", ids.len());
            return Ok(ids.len() as u64);
        }
        let list = ids
            .iter()
            .take(self.config.max_deletes_per_pass)
            .map(|i| i.to_string())
            .collect::<Vec<_>>()
            .join(",");
        let removed = self
            .db
            .execute_raw(Statement::from_string(
                self.db.get_database_backend(),
                format!("DELETE FROM Events WHERE Id IN ({list})"),
            ))
            .await?
            .rows_affected();
        info!("deleted {removed} events whose media was missing");
        Ok(removed)
    }

    /// Remove quarantine batches past their retention window.
    async fn sweep_quarantine(&self, storages: &[crate::entity::storage::Model]) {
        let fs = &self.config.filesystem;
        if self.config.dry_run || fs.quarantine_retention_days == 0 {
            return;
        }
        let now = crate::util::time::utc_now();
        for st in storages {
            let qroot = std::path::PathBuf::from(&st.path).join(&fs.quarantine_dir);
            for batch in
                filesystem::expired_quarantine_batches(&qroot, now, fs.quarantine_retention_days)
            {
                // Containment re-checked at the point of deletion, not just
                // when the path was built.
                if !filesystem::is_within_root(&qroot, &batch) {
                    warn!(
                        "refusing to remove {} : outside quarantine",
                        batch.display()
                    );
                    continue;
                }
                match std::fs::remove_dir_all(&batch) {
                    Ok(()) => info!("removed expired quarantine batch {}", batch.display()),
                    Err(e) => warn!("cannot remove {}: {e}", batch.display()),
                }
            }
        }
    }

    /// `Frames` and `Stats` rows whose event no longer exists.
    ///
    /// Pure database garbage — nothing can reach them, and `Frames` in
    /// particular is the largest table in most installs.
    async fn sweep_orphaned_children(&self) -> Result<(u64, u64), DbErr> {
        let frames = self.delete_children_of_missing_events("Frames").await?;
        let stats = self.delete_children_of_missing_events("Stats").await?;
        Ok((frames, stats))
    }

    /// Delete `table` rows whose `EventId` has no `Events` row.
    ///
    /// The orphan ids are read first (a consistent read, no locks), then
    /// deleted by `EventId IN (...)` in small batches so the delete uses the
    /// `EventId` index and locks only the rows it removes. A single
    /// `DELETE ... WHERE NOT EXISTS` scanned the whole table under next-key
    /// locks whenever one orphan existed, on the largest table in the schema
    /// (#94).
    async fn delete_children_of_missing_events(&self, table: &str) -> Result<u64, DbErr> {
        let backend = self.db.get_database_backend();
        let cap = self.config.max_deletes_per_pass;

        let ids: Vec<u64> = IdRow::find_by_statement(Statement::from_string(
            backend,
            format!(
                "SELECT DISTINCT c.EventId AS id FROM {table} c \
                 LEFT JOIN Events e ON e.Id = c.EventId \
                 WHERE e.Id IS NULL LIMIT {cap}"
            ),
        ))
        .all(self.db.as_ref())
        .await?
        .into_iter()
        .map(|r| r.id)
        .collect();

        if ids.is_empty() {
            return Ok(0);
        }
        if self.config.dry_run {
            info!(
                "dry run: would delete {table} rows for {} missing events",
                ids.len()
            );
            return Ok(ids.len() as u64);
        }

        let mut removed = 0u64;
        for chunk in ids.chunks(100) {
            let list = chunk
                .iter()
                .map(|i| i.to_string())
                .collect::<Vec<_>>()
                .join(",");
            removed += self
                .db
                .execute_raw(Statement::from_string(
                    backend,
                    format!("DELETE FROM {table} WHERE EventId IN ({list})"),
                ))
                .await?
                .rows_affected();
        }
        info!("deleted {removed} orphaned rows from {table}");
        Ok(removed)
    }

    /// Events that recorded no frames, are past the grace period, and whose
    /// capture daemon is demonstrably done with them.
    ///
    /// An event has a row before it has frames, so without the age guard this
    /// races the capture daemon and deletes recordings in progress; a
    /// video-only event never gets frames, so the age guard alone still
    /// deleted live recordings (#124). Archived events are excluded — see the
    /// module note.
    async fn remove_empty_events(&self) -> Result<u64, DbErr> {
        use sea_orm::EntityTrait;
        let mut predicate = empty_event_predicate(self.config.min_age_seconds);
        // An event on another server's storage is that server's to judge: its
        // row goes only with its files.
        let elsewhere: Vec<String> = crate::entity::storage::Entity::find()
            .all(self.db.as_ref())
            .await?
            .into_iter()
            .filter(|st| {
                matches!(
                    crate::cluster::storage_owner(self.this_server, st.server_id),
                    crate::cluster::Owner::Peer(_)
                )
            })
            .map(|st| st.id.to_string())
            .collect();
        if !elsewhere.is_empty() {
            predicate.push_str(&format!(
                " AND (StorageId IS NULL OR StorageId NOT IN ({}))",
                elsewhere.join(",")
            ));
        }
        self.delete_where("Events", &predicate).await
    }

    /// Close events whose capture daemon died mid-recording.
    ///
    /// An update, never a delete: end time, length, frame and score totals are
    /// recomputed from the frames that did land, and the event is marked
    /// recovered so the repair is visible rather than silent.
    async fn close_unclosed_events(&self) -> Result<u64, DbErr> {
        let predicate = unclosed_event_predicate(self.config.min_age_seconds);
        let backend = self.db.get_database_backend();
        let find = format!(
            "SELECT Id AS id FROM Events WHERE {predicate} LIMIT {}",
            self.config.max_deletes_per_pass
        );
        let ids: Vec<u64> = IdRow::find_by_statement(Statement::from_string(backend, find))
            .all(self.db.as_ref())
            .await?
            .into_iter()
            .map(|r| r.id)
            .collect();

        if ids.is_empty() || self.config.dry_run {
            if !ids.is_empty() {
                debug!("dry run: would close {} unclosed events", ids.len());
            }
            return Ok(ids.len() as u64);
        }

        for id in &ids {
            // Single-table UPDATE driven by subqueries over Frames. A
            // multi-table UPDATE would hold shared locks on the joined rows to
            // commit and deadlock against ZoneMinder's own event triggers.
            let sql = format!(
                "UPDATE Events SET \
                   EndDateTime = COALESCE( \
                     (SELECT MAX(TimeStamp) FROM Frames WHERE Frames.EventId = {id}), \
                     StartDateTime), \
                   Frames = (SELECT COUNT(*) FROM Frames WHERE Frames.EventId = {id}), \
                   AlarmFrames = (SELECT COUNT(*) FROM Frames \
                     WHERE Frames.EventId = {id} AND Score > 0), \
                   TotScore = (SELECT COALESCE(SUM(Score),0) FROM Frames \
                     WHERE Frames.EventId = {id}), \
                   MaxScore = (SELECT COALESCE(MAX(Score),0) FROM Frames \
                     WHERE Frames.EventId = {id}), \
                   Length = TIMESTAMPDIFF(SECOND, StartDateTime, COALESCE( \
                     (SELECT MAX(TimeStamp) FROM Frames WHERE Frames.EventId = {id}), \
                     StartDateTime)), \
                   Notes = CONCAT_WS(' ', Notes, 'Recovered.') \
                 WHERE Id = {id} AND EndDateTime IS NULL"
            );
            self.db
                .execute_raw(Statement::from_string(backend, sql))
                .await?;
        }
        info!("closed {} unclosed events", ids.len());
        Ok(ids.len() as u64)
    }

    /// Recompute `Event_Summaries` and `Storage.DiskSpace` from the rows they
    /// summarise.
    ///
    /// Both drift for different reasons. `Event_Summaries` is trigger-maintained,
    /// so a trigger that did not fire is never self-corrected. `Storage.DiskSpace`
    /// is adjusted *incrementally* by application code, so a crash between
    /// deleting an event and adjusting the total leaves it permanently wrong.
    ///
    /// The `DiskSpace` update is guarded by a compare-and-swap on the value we
    /// read. Because that column is adjusted relatively rather than recomputed,
    /// writing a stale absolute snapshot would actively undo a concurrent
    /// adjustment instead of being corrected on the next pass.
    async fn resync_counters(&self) -> Result<(), DbErr> {
        if self.config.dry_run {
            debug!("dry run: skipping counter resync");
            return Ok(());
        }
        let backend = self.db.get_database_backend();

        // Read the totals with a plain SELECT, then write each monitor's row
        // by primary key. An UPDATE driven by correlated subqueries over Events
        // takes shared locks on the Events rows it reads and holds them to
        // commit — the reverse of the order ZoneMinder's Events triggers take,
        // which is a deadlock against a closing event (same shape as #99).
        #[derive(FromQueryResult)]
        struct MonitorTotals {
            monitor_id: i64,
            total: i64,
            total_bytes: i64,
            archived: i64,
            archived_bytes: i64,
        }
        let totals = MonitorTotals::find_by_statement(Statement::from_string(
            backend,
            "SELECT CAST(MonitorId AS SIGNED) AS monitor_id, \
                    CAST(COUNT(*) AS SIGNED) AS total, \
                    CAST(COALESCE(SUM(DiskSpace),0) AS SIGNED) AS total_bytes, \
                    CAST(COALESCE(SUM(Archived = 1),0) AS SIGNED) AS archived, \
                    CAST(COALESCE(SUM(CASE WHEN Archived = 1 THEN DiskSpace ELSE 0 END),0) \
                         AS SIGNED) AS archived_bytes \
             FROM Events GROUP BY MonitorId",
        ))
        .all(self.db.as_ref())
        .await?;
        let mut by_monitor: std::collections::HashMap<i64, MonitorTotals> =
            totals.into_iter().map(|t| (t.monitor_id, t)).collect();
        #[derive(FromQueryResult)]
        struct SummaryRow {
            monitor_id: i64,
        }
        let summaries: Vec<i64> = SummaryRow::find_by_statement(Statement::from_string(
            backend,
            "SELECT CAST(MonitorId AS SIGNED) AS monitor_id FROM Event_Summaries",
        ))
        .all(self.db.as_ref())
        .await?
        .into_iter()
        .map(|r| r.monitor_id)
        .collect();
        for monitor_id in summaries {
            let (total, total_bytes, archived, archived_bytes) = by_monitor
                .remove(&monitor_id)
                .map(|t| (t.total, t.total_bytes, t.archived, t.archived_bytes))
                .unwrap_or((0, 0, 0, 0));
            self.db
                .execute_raw(Statement::from_sql_and_values(
                    backend,
                    "UPDATE Event_Summaries SET TotalEvents = ?, TotalEventDiskSpace = ?, \
                     ArchivedEvents = ?, ArchivedEventDiskSpace = ? WHERE MonitorId = ?",
                    [
                        total.into(),
                        total_bytes.into(),
                        archived.into(),
                        archived_bytes.into(),
                        monitor_id.into(),
                    ],
                ))
                .await?;
        }

        // Every unsigned column is cast: sqlx refuses to decode an UNSIGNED
        // column into a signed Rust integer, and the SUM of an unsigned column
        // is a DECIMAL. This SELECT failed on both for as long as it existed,
        // the error was swallowed, and Storage.DiskSpace was never resynced (#90).
        #[derive(FromQueryResult)]
        struct StorageRow {
            id: i64,
            current: Option<i64>,
            actual: i64,
        }

        let rows = StorageRow::find_by_statement(Statement::from_string(
            backend,
            "SELECT CAST(s.Id AS SIGNED) AS id, CAST(s.DiskSpace AS SIGNED) AS current, \
                    CAST(COALESCE((SELECT SUM(e.DiskSpace) FROM Events e \
                              WHERE e.StorageId = s.Id), 0) AS SIGNED) AS actual \
             FROM Storage s",
        ))
        .all(self.db.as_ref())
        .await?;

        for row in rows {
            if row.current == Some(row.actual) {
                continue;
            }
            // `<=>` is null-safe, so a NULL current value matches the NULL we
            // read rather than failing the guard forever.
            let updated = self
                .db
                .execute_raw(Statement::from_sql_and_values(
                    backend,
                    "UPDATE Storage SET DiskSpace = ? WHERE Id = ? AND DiskSpace <=> ?",
                    [row.actual.into(), row.id.into(), row.current.into()],
                ))
                .await?
                .rows_affected();
            if updated == 0 {
                debug!(
                    "storage {} disk space changed under us; leaving it for the next pass",
                    row.id
                );
            } else {
                info!(
                    "storage {} disk space corrected: {:?} -> {}",
                    row.id, row.current, row.actual
                );
            }
        }
        Ok(())
    }

    /// Count matching rows, then delete them unless this is a dry run.
    ///
    /// Bounded by `max_deletes_per_pass`: a misconfigured storage path can make
    /// a great many rows look orphaned at once, and the cap keeps the blast
    /// radius recoverable while the log makes the cause obvious.
    /// Delete up to `max_deletes_per_pass` rows of `table` matching
    /// `predicate`, selecting the ids first and deleting by id. The predicate
    /// may reference `table` itself in a subquery (the empty-event rule looks
    /// for a newer event on the same monitor), which MySQL refuses inside a
    /// DELETE on that table (error 1093); a SELECT has no such limit.
    async fn delete_where(&self, table: &str, predicate: &str) -> Result<u64, DbErr> {
        let backend = self.db.get_database_backend();

        let count = CountRow::find_by_statement(Statement::from_string(
            backend,
            format!("SELECT COUNT(*) AS n FROM {table} WHERE {predicate}"),
        ))
        .one(self.db.as_ref())
        .await?
        .map(|r| r.n.max(0) as u64)
        .unwrap_or(0);

        if count == 0 {
            return Ok(0);
        }
        if self.config.dry_run {
            info!("dry run: would delete {count} rows from {table}");
            return Ok(count);
        }

        let limit = self.config.max_deletes_per_pass;
        if count as usize > limit {
            warn!(
                "{count} orphaned rows in {table} exceeds max_deletes_per_pass ({limit}); \
                 deleting {limit} this pass. If this repeats, check the storage \
                 configuration before assuming the rows are really orphaned."
            );
        }

        let ids: Vec<u64> = IdRow::find_by_statement(Statement::from_string(
            backend,
            format!("SELECT Id AS id FROM {table} WHERE {predicate} LIMIT {limit}"),
        ))
        .all(self.db.as_ref())
        .await?
        .into_iter()
        .map(|r| r.id)
        .collect();

        let mut removed = 0u64;
        for chunk in ids.chunks(100) {
            let list = chunk
                .iter()
                .map(|i| i.to_string())
                .collect::<Vec<_>>()
                .join(",");
            removed += self
                .db
                .execute_raw(Statement::from_string(
                    backend,
                    format!("DELETE FROM {table} WHERE Id IN ({list})"),
                ))
                .await?
                .rows_affected();
        }
        info!("deleted {removed} orphaned rows from {table}");
        Ok(removed)
    }
}

/// Open events that are past the grace period and demonstrably not being
/// written any more: their frames have stopped for `min_age` (#96) *and*
/// [`not_being_written`] holds (#124).
fn unclosed_event_predicate(min_age: u64) -> String {
    format!(
        "EndDateTime IS NULL \
         AND StartDateTime IS NOT NULL \
         AND StartDateTime < DATE_SUB(NOW(), INTERVAL {min_age} SECOND) \
         AND NOT EXISTS (SELECT 1 FROM Frames \
             WHERE Frames.EventId = Events.Id \
               AND Frames.TimeStamp > DATE_SUB(NOW(), INTERVAL {min_age} SECOND)) \
         AND {}",
        not_being_written(min_age)
    )
}

/// Events that recorded no frames, are past the grace period, and are not a
/// recording in progress ([`not_being_written`], #124). Archived events are
/// excluded — see the module note. ONVIF event-listener rows are excluded by
/// name: that listener records alarms straight into `Events` without frames
/// (see `daemon::onvif_event_listener::open_event`), so to this rule every
/// one of them looks empty (#97).
fn empty_event_predicate(min_age: u64) -> String {
    format!(
        "Archived = 0 \
         AND StartDateTime IS NOT NULL \
         AND StartDateTime < DATE_SUB(NOW(), INTERVAL {min_age} SECOND) \
         AND Name NOT LIKE 'ONVIF-%' \
         AND NOT EXISTS (SELECT 1 FROM Frames WHERE Frames.EventId = Events.Id) \
         AND {}",
        not_being_written(min_age)
    )
}

/// Evidence that zmc is no longer writing the event, for use in a predicate
/// over `Events`.
///
/// Frame freshness alone is not enough: a video-only event has no `Frames`
/// rows at all, so it looked abandoned from birth while zmc was still writing
/// it — and was deleted as empty, or closed as unclosed, after `min_age`
/// (#124). One of these must hold instead:
///
/// * a newer event exists on the same monitor — zmc records one event at a
///   time, so this one is finished whatever else is true;
/// * the capture daemon is not live — no `Monitor_Status` row marked
///   Connected and refreshed within `min_age`. zmc writes that row every
///   `fps_report_interval` frames in either supervision mode, which is what
///   lets the audit judge liveness without asking the supervisor;
/// * the event is older than the monitor's `SectionLength` plus the grace
///   period — zmc ends a section at `SectionLength` in every recording mode,
///   so a live daemon cannot still be inside it. A monitor with no section
///   length (0) gets no cap.
fn not_being_written(min_age: u64) -> String {
    format!(
        "(EXISTS (SELECT 1 FROM Events later \
              WHERE later.MonitorId = Events.MonitorId AND later.Id > Events.Id) \
          OR NOT EXISTS (SELECT 1 FROM Monitor_Status ms \
              WHERE ms.MonitorId = Events.MonitorId \
                AND ms.Status = 'Connected' \
                AND ms.UpdatedOn > DATE_SUB(NOW(), INTERVAL {min_age} SECOND)) \
          OR EXISTS (SELECT 1 FROM Monitors m \
              WHERE m.Id = Events.MonitorId \
                AND m.SectionLength > 0 \
                AND Events.StartDateTime < DATE_SUB(NOW(), \
                    INTERVAL (m.SectionLength + {min_age}) SECOND)))"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_clean_report_is_clean() {
        assert!(AuditReport::default().is_clean());
    }

    #[test]
    fn the_total_counts_every_category() {
        let r = AuditReport {
            orphaned_frames: 1,
            orphaned_stats: 2,
            empty_events: 3,
            unclosed_events: 4,
            quarantined_dirs: 5,
            rows_without_media: 6,
            // Not counted: nothing was done to these.
            unidentified_dirs: 99,
            refusals: vec!["storage 2 unmounted".into()],
            errors: vec![],
            dry_run: false,
        };
        assert_eq!(
            r.total(),
            21,
            "unidentified directories are observed, not acted on, so they \
             must not count toward the total"
        );
        assert!(!r.is_clean());
    }

    /// The empty-event predicate is the one that deletes user data, so pin its
    /// three guards rather than trusting a reading of the SQL.
    #[test]
    fn the_empty_event_predicate_guards_archived_and_age() {
        let predicate = empty_event_predicate(7200);

        // Archived events are kept. zmaudit intends this but never fetches the
        // column, so its guard is dead and it deletes them.
        assert!(predicate.contains("Archived = 0"));
        // In-flight events are out of range.
        assert!(predicate.contains("INTERVAL 7200 SECOND"));
        // An event with no start time has no age to judge, so it is left alone
        // rather than deleted unconditionally.
        assert!(predicate.contains("StartDateTime IS NOT NULL"));
        // Only events with no frames at all.
        assert!(predicate.contains("NOT EXISTS"));
        // ONVIF listener events never have frames and are not empty (#97).
        assert!(predicate.contains("Name NOT LIKE 'ONVIF-%'"));
    }

    /// A job failure is part of the report, not just a log line: a query that
    /// fails every pass (the DiskSpace resync did, #90) must fail a test that
    /// asserts a clean pass.
    #[test]
    fn a_failed_job_makes_the_report_unclean() {
        let mut r = AuditReport::default();
        assert!(r.is_clean());
        r.fail("counter resync", DbErr::Custom("boom".into()));
        assert!(!r.is_clean());
        assert_eq!(r.errors.len(), 1);
        assert!(
            r.errors[0].starts_with("counter resync: "),
            "{:?}",
            r.errors
        );
        assert!(r.errors[0].contains("boom"), "{:?}", r.errors);
    }

    /// Two Storage rows on one path report the same orphan twice in one pass;
    /// that is one sighting, not two (#93).
    #[test]
    fn duplicate_sightings_in_one_pass_count_once() {
        let mut counts = std::collections::HashMap::new();
        let a = std::path::PathBuf::from("/s/1/100");
        assert!(OrphanTracker::confirm(&mut counts, &[a.clone(), a.clone()], 2).is_empty());
        assert_eq!(
            OrphanTracker::confirm(&mut counts, std::slice::from_ref(&a), 2),
            vec![a]
        );
    }

    /// The confirmation tracker is what stops the walk/commit race turning a
    /// live event into an orphan, so its counting has to be exactly
    /// *consecutive*.
    #[test]
    fn an_orphan_must_be_seen_twice_running_before_it_counts() {
        let mut counts = std::collections::HashMap::new();
        let a = std::path::PathBuf::from("/s/1/100");

        // First sighting: not yet actionable.
        assert!(OrphanTracker::confirm(&mut counts, std::slice::from_ref(&a), 2).is_empty());
        // Second consecutive sighting: now it is.
        assert_eq!(
            OrphanTracker::confirm(&mut counts, std::slice::from_ref(&a), 2),
            vec![a]
        );
    }

    #[test]
    fn a_gap_resets_the_count() {
        // The race this exists for: a directory looks orphaned, then its row
        // commits, then a later unrelated pass sees it again. It must start
        // from zero, not carry a stale sighting.
        let mut counts = std::collections::HashMap::new();
        let a = std::path::PathBuf::from("/s/1/100");

        assert!(OrphanTracker::confirm(&mut counts, std::slice::from_ref(&a), 2).is_empty());
        // Absent this pass — the entry is forgotten entirely.
        assert!(OrphanTracker::confirm(&mut counts, &[], 2).is_empty());
        assert!(
            counts.is_empty(),
            "a vanished candidate must not be retained"
        );
        // Seen again: back to one sighting, still not actionable.
        assert!(OrphanTracker::confirm(&mut counts, std::slice::from_ref(&a), 2).is_empty());
    }

    #[test]
    fn candidates_are_tracked_independently() {
        let mut counts = std::collections::HashMap::new();
        let a = std::path::PathBuf::from("/s/1/100");
        let b = std::path::PathBuf::from("/s/1/200");

        OrphanTracker::confirm(&mut counts, std::slice::from_ref(&a), 2);
        // `b` is new this pass, so only `a` reaches two sightings.
        let confirmed = OrphanTracker::confirm(&mut counts, &[a.clone(), b.clone()], 2);
        assert_eq!(confirmed, vec![a]);
        // `b` gets there on the next one.
        let confirmed = OrphanTracker::confirm(&mut counts, std::slice::from_ref(&b), 2);
        assert_eq!(confirmed, vec![b]);
    }

    #[test]
    fn requiring_one_confirmation_acts_immediately() {
        // An operator who does not want the two-pass delay can say so, and
        // zero must behave as one rather than as "never".
        let mut counts = std::collections::HashMap::new();
        let a = std::path::PathBuf::from("/s/1/100");
        assert_eq!(
            OrphanTracker::confirm(&mut counts, std::slice::from_ref(&a), 1),
            vec![a.clone()]
        );

        let mut counts = std::collections::HashMap::new();
        assert_eq!(
            OrphanTracker::confirm(&mut counts, std::slice::from_ref(&a), 0),
            vec![a]
        );
    }
}
