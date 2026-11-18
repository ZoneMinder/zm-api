//! Event on-disk storage location and media lifecycle.
//!
//! ZoneMinder writes each event's media (recorded video, per-frame JPEGs, the
//! HLS index, etc.) into a directory under a `Storages.Path` root, laid out by
//! the storage *scheme*. This module is the single source of truth for
//! computing that directory, shared by playback (which serves the files) and
//! deletion (which removes them) so the two can never disagree about where an
//! event lives — a disagreement would mean either serving the wrong files or
//! deleting the wrong directory.

use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use tracing::{debug, info, warn};

use crate::entity::events::Model as EventModel;
use crate::entity::sea_orm_active_enums::Scheme;
use crate::error::{AppError, AppResult};
use crate::repo;
use crate::server::state::AppState;

/// Resolve the storage path for an event, falling back to the config default
/// when the event's storage row is missing, and rejecting any value that
/// contains a `..` traversal component.
///
/// `Storages.Path` is only writable by an admin (`System:Edit`), but the API
/// runs as `www-data` and serves/removes files based on that path — a
/// malicious or mistaken admin entry like `/var/cache/zm/events/..` could let
/// an otherwise-permitted operation resolve outside the events tree. Reject
/// those at the DB-read boundary.
/// The event's own directory: its storage root laid out by its scheme.
pub(crate) async fn event_directory(state: &AppState, event: &EventModel) -> AppResult<PathBuf> {
    let storage = resolve_event_storage_path(state, event).await?;
    Ok(build_event_directory_path(
        &storage,
        event.monitor_id,
        event.id,
        event.start_date_time,
        &event.scheme,
    ))
}

pub(crate) async fn resolve_event_storage_path(
    state: &AppState,
    event: &EventModel,
) -> AppResult<String> {
    Ok(resolve_event_storage(state, event).await?.0)
}

/// The event's storage root and whether that storage's `DoDelete` allows its
/// media to be removed (#108). One lookup serves both, so the delete path
/// makes no more queries than it did before.
async fn resolve_event_storage(state: &AppState, event: &EventModel) -> AppResult<(String, bool)> {
    let (storage_path, do_delete) = if is_default_storage(event.storage_id) {
        default_storage(state).await?
    } else {
        let sid = event.storage_id.expect("non-default storage_id is Some");
        match repo::storage::find_by_id(state.db(), sid).await? {
            Some(s) => (s.path, s.do_delete != 0),
            None => {
                warn!(
                    "Storage {} not found in database, using default storage",
                    sid
                );
                default_storage(state).await?
            }
        }
    };

    if crate::util::path::contains_traversal(&storage_path) {
        warn!(
            "Refusing storage path with '..' traversal: {:?}",
            storage_path
        );
        return Err(AppError::InternalServerError(
            "storage path contains '..' traversal".to_string(),
        ));
    }

    Ok((storage_path, do_delete))
}

/// True iff `storage_id` denotes ZoneMinder's primary/default storage rather
/// than a real `Storage` row. ZoneMinder writes `Events.StorageId = 0` (and
/// occasionally NULL) as a sentinel meaning "the default storage" — it is not a
/// foreign key, so looking up row 0 always misses.
pub(crate) fn is_default_storage(storage_id: Option<u16>) -> bool {
    matches!(storage_id, None | Some(0))
}

/// Resolve the default events directory: ZoneMinder's primary storage row when
/// the `Storage` table is populated, otherwise the configured `events_dir`.
/// Default storage root and its `DoDelete`; the configured events dir (with
/// deletion allowed) when the `Storage` table has no default row.
async fn default_storage(state: &AppState) -> AppResult<(String, bool)> {
    if let Some(s) = repo::storage::find_default(state.db()).await? {
        return Ok((s.path, s.do_delete != 0));
    }
    Ok((state.config.streaming.zoneminder.events_dir.clone(), true))
}

/// Build the event directory path based on ZoneMinder's storage scheme.
///
/// The returned path is the event's **own leaf directory** in every scheme, so
/// it holds only that event's media (removing it does not touch siblings):
/// - **Deep**: `{storage}/{MonitorId}/{YY/MM/DD/HH/MM/SS}/`
/// - **Medium**: `{storage}/{MonitorId}/{YYYY-MM-DD}/{EventId}/`
/// - **Shallow**: `{storage}/{MonitorId}/{EventId}/`
pub(crate) fn build_event_directory_path(
    storage_path: &str,
    monitor_id: u32,
    event_id: u64,
    start_time: Option<sea_orm::prelude::DateTime>,
    scheme: &Scheme,
) -> PathBuf {
    let start_time = start_time.map(crate::util::time::civil);
    let base = PathBuf::from(storage_path).join(monitor_id.to_string());

    match scheme {
        Scheme::Deep => {
            // Deep: {storage}/{monitor_id}/{YY/MM/DD/HH/MM/SS}/
            if let Some(dt) = start_time {
                base.join(format!("{:02}", dt.year() % 100))
                    .join(format!("{:02}", dt.month()))
                    .join(format!("{:02}", dt.day()))
                    .join(format!("{:02}", dt.hour()))
                    .join(format!("{:02}", dt.minute()))
                    .join(format!("{:02}", dt.second()))
            } else {
                // Fallback to shallow if no start time
                base.join(event_id.to_string())
            }
        }
        Scheme::Medium => {
            // Medium: {storage}/{monitor_id}/{YYYY-MM-DD}/{event_id}/
            if let Some(dt) = start_time {
                base.join(format!(
                    "{:04}-{:02}-{:02}",
                    dt.year(),
                    dt.month(),
                    dt.day()
                ))
                .join(event_id.to_string())
            } else {
                // Fallback to shallow if no start time
                base.join(event_id.to_string())
            }
        }
        Scheme::Shallow => {
            // Shallow: {storage}/{monitor_id}/{event_id}/
            base.join(event_id.to_string())
        }
    }
}

/// Remove an event's on-disk media (its leaf directory, plus the `.{id}`
/// lookup symlink that the Deep scheme leaves beside the monitor directory).
///
/// Best-effort: a filesystem failure is logged but does not fail the caller —
/// the database row is the authoritative record of an event's existence, so a
/// rare orphaned directory is preferable to a delete that reports failure
/// after the row is already gone. An already-absent directory is not an error.
pub(crate) async fn delete_event_media(state: &AppState, event: &EventModel) -> AppResult<()> {
    let (storage_path, do_delete) = resolve_event_storage(state, event).await?;
    // ZoneMinder keeps the files on a storage whose DoDelete is off (#108).
    if !do_delete {
        debug!(
            "Event {} rows removed; media kept (Storage.DoDelete is off)",
            event.id
        );
        return Ok(());
    }
    // Best-effort for the interactive path: the rows are gone either way and
    // the filesystem audit reclaims a leftover directory.
    let _ = remove_event_dir(&storage_path, event).await;
    Ok(())
}

/// Whether `dir` holds this event's own media (or nothing at all). A Deep
/// directory is reached by a path derived from `StartDateTime`, and a clock
/// or timezone difference between zmc and the database makes that path a
/// neighbouring event's (#107). Removing it must be refused.
fn directory_names_event(dir: &Path, event_id: u64) -> bool {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return true; // absent: nothing to protect, nothing to remove
    };
    let names: Vec<String> = entries
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    names.is_empty()
        || crate::service::maintenance::audit::filesystem::identify_from_entries(&names)
            == Some(event_id)
}

/// Remove an event's on-disk media given an already-resolved `storage_path`
/// (its `Storages.Path` root). The retention reaper calls this directly with
/// the storage row it already loaded, avoiding a redundant lookup; the
/// interactive delete path reaches it via [`delete_event_media`]. Sharing this
/// one function is what keeps the reaper and the endpoint from ever computing
/// the event directory two different ways.
///
/// Returns whether the media is gone: `true` when the directory was removed
/// or was already absent, `false` when removal failed (logged). The reaper
/// stops its pass on `false` so it cannot keep deleting rows while the disk
/// stays full (#106).
pub(crate) async fn remove_event_dir(storage_path: &str, event: &EventModel) -> bool {
    let derived = build_event_directory_path(
        storage_path,
        event.monitor_id,
        event.id,
        event.start_date_time,
        &event.scheme,
    );

    // Deep: ZoneMinder leaves `{monitor}/yy/mm/dd/.{id} -> HH/MM/SS`. That
    // link is the event's own claim to a directory; the timestamp-derived
    // path is only a guess, and a wrong guess is another event (#107).
    let mut dir = derived.clone();
    let mut deep_marker: Option<PathBuf> = None;
    if matches!(event.scheme, Scheme::Deep) {
        if let Some(day) = derived.ancestors().nth(3) {
            let link = day.join(format!(".{}", event.id));
            if let Ok(target) = std::fs::read_link(&link) {
                dir = day.join(target);
            }
            deep_marker = Some(link);
        }
    }

    // Defence in depth: never remove the storage root or a bare monitor
    // directory. The path is built from typed integers/dates and a
    // traversal-checked root, so it always descends at least to the event's own
    // leaf — but refuse to proceed if that invariant is ever violated.
    let monitor_root = PathBuf::from(storage_path).join(event.monitor_id.to_string());
    if !dir.starts_with(&monitor_root) || dir == monitor_root {
        warn!(
            "Refusing to remove suspicious event directory {:?} (monitor root {:?})",
            dir, monitor_root
        );
        return false;
    }
    if matches!(event.scheme, Scheme::Deep) && !directory_names_event(&dir, event.id) {
        warn!(
            "Refusing to remove {:?}: it does not hold event {}'s media",
            dir, event.id
        );
        return false;
    }

    let removed = match tokio::fs::remove_dir_all(&dir).await {
        Ok(()) => {
            info!("Removed event {} media directory {:?}", event.id, dir);
            true
        }
        Err(e) if e.kind() == ErrorKind::NotFound => {
            debug!(
                "Event {} media directory {:?} already absent",
                event.id, dir
            );
            true
        }
        Err(e) => {
            warn!(
                "Failed to remove event {} media directory {:?}: {e}",
                event.id, dir
            );
            false
        }
    };

    // The Deep scheme's `.{id}` marker lives in the yy/mm/dd directory (it
    // used to be unlinked from the monitor root, leaving the real one
    // dangling — #107); remove it so it isn't left pointing at nothing.
    if let Some(link) = deep_marker {
        if let Err(e) = tokio::fs::remove_file(&link).await {
            if e.kind() != ErrorKind::NotFound {
                debug!("Could not remove Deep-scheme symlink {:?}: {e}", link);
            }
        }
    }

    removed
}

#[cfg(test)]
mod tests {
    use super::*;

    fn column(s: &str) -> sea_orm::prelude::DateTime {
        crate::util::time::db_datetime(s.parse().unwrap())
    }

    fn sample_dt() -> sea_orm::prelude::DateTime {
        column("2024-03-09T14:05:06")
    }

    #[test]
    fn shallow_uses_event_id_leaf() {
        let p = build_event_directory_path("/events", 7, 467, Some(sample_dt()), &Scheme::Shallow);
        assert_eq!(p, PathBuf::from("/events/7/467"));
    }

    #[test]
    fn medium_uses_date_then_event_id() {
        let p = build_event_directory_path("/events", 7, 467, Some(sample_dt()), &Scheme::Medium);
        assert_eq!(p, PathBuf::from("/events/7/2024-03-09/467"));
    }

    #[test]
    fn deep_uses_two_digit_timestamp_path() {
        let p = build_event_directory_path("/events", 7, 467, Some(sample_dt()), &Scheme::Deep);
        assert_eq!(p, PathBuf::from("/events/7/24/03/09/14/05/06"));
    }

    #[test]
    fn deep_zero_pads_and_wraps_year_to_two_digits() {
        // Year 2008 -> "08"; single-digit month/day/time zero-padded.
        let dt = column("2008-01-02T00:00:00");
        let p = build_event_directory_path("/srv", 1, 5, Some(dt), &Scheme::Deep);
        assert_eq!(p, PathBuf::from("/srv/1/08/01/02/00/00/00"));
    }

    #[test]
    fn deep_and_medium_fall_back_to_shallow_without_start_time() {
        assert_eq!(
            build_event_directory_path("/events", 7, 467, None, &Scheme::Deep),
            PathBuf::from("/events/7/467")
        );
        assert_eq!(
            build_event_directory_path("/events", 7, 467, None, &Scheme::Medium),
            PathBuf::from("/events/7/467")
        );
    }

    #[test]
    fn is_default_storage_treats_zero_and_null_as_default() {
        assert!(is_default_storage(None));
        assert!(is_default_storage(Some(0)));
        assert!(!is_default_storage(Some(1)));
        assert!(!is_default_storage(Some(7)));
    }

    fn mk_event(id: u64, monitor_id: u32, storage_id: Option<u16>, scheme: Scheme) -> EventModel {
        use crate::entity::sea_orm_active_enums::Orientation;
        use rust_decimal::Decimal;
        EventModel {
            id,
            monitor_id,
            storage_id,
            secondary_storage_id: None,
            name: "ev".into(),
            cause: None,
            start_date_time: None,
            end_date_time: None,
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
            disk_space: None,
            scheme,
            locked: 0,
            latitude: None,
            longitude: None,
        }
    }

    fn deep_event(id: u64, monitor_id: u32, start: &str) -> EventModel {
        let mut e = mk_event(id, monitor_id, Some(1), Scheme::Deep);
        e.start_date_time = Some(column(start));
        e
    }

    /// Deep scheme, #107: the derived timestamp path belongs to a neighbour
    /// (the row's StartDateTime is an hour off the directory zmc wrote);
    /// the `.{id}` marker says where the event really is. The real directory
    /// goes, the neighbour stays, and the marker is unlinked from the day
    /// directory where it actually lives.
    #[cfg(unix)]
    #[tokio::test]
    async fn deep_delete_follows_the_marker_not_the_derived_timestamp() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_string_lossy().into_owned();
        let day = tmp.path().join("3/26/01/15");
        let real = day.join("11/15/00");
        let neighbour = day.join("10/15/00");
        std::fs::create_dir_all(&real).unwrap();
        std::fs::create_dir_all(&neighbour).unwrap();
        std::fs::write(real.join("500-video.mp4"), b"").unwrap();
        std::fs::write(neighbour.join("501-video.mp4"), b"").unwrap();
        std::os::unix::fs::symlink("11/15/00", day.join(".500")).unwrap();

        // Derived path for 10:15:00 is the neighbour's directory.
        let removed = remove_event_dir(&root, &deep_event(500, 3, "2026-01-15 10:15:00")).await;

        assert!(removed);
        assert!(!real.exists(), "the event's real directory is removed");
        assert!(
            neighbour.join("501-video.mp4").exists(),
            "the neighbour is untouched"
        );
        assert!(
            !day.join(".500").exists(),
            "the marker in the day directory is unlinked"
        );
    }

    /// Deep scheme with no marker: the derived directory holds another
    /// event's recording, so removal is refused (#107).
    #[tokio::test]
    async fn deep_delete_refuses_a_directory_naming_another_event() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_string_lossy().into_owned();
        let derived = tmp.path().join("3/26/01/15/10/15/00");
        std::fs::create_dir_all(&derived).unwrap();
        std::fs::write(derived.join("501-video.mp4"), b"").unwrap();

        let removed = remove_event_dir(&root, &deep_event(500, 3, "2026-01-15 10:15:00")).await;

        assert!(!removed);
        assert!(derived.join("501-video.mp4").exists());
    }

    fn mk_storage(id: u16, path: String) -> crate::entity::storage::Model {
        use crate::entity::sea_orm_active_enums::StorageType;
        crate::entity::storage::Model {
            id,
            path,
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

    /// Deleting an event removes exactly its own leaf directory and leaves a
    /// sibling event's directory (under the same monitor) untouched.
    #[tokio::test]
    async fn delete_event_media_removes_only_the_event_leaf() {
        use sea_orm::{DatabaseBackend, MockDatabase};

        let tmp = tempfile::tempdir().unwrap();
        let monitor_id = 3u32;
        let event_id = 42u64;
        let monitor_dir = tmp.path().join(monitor_id.to_string());
        let event_dir = monitor_dir.join(event_id.to_string());
        let sibling_dir = monitor_dir.join("99");
        std::fs::create_dir_all(&event_dir).unwrap();
        std::fs::create_dir_all(&sibling_dir).unwrap();
        std::fs::write(event_dir.join("42-video.mp4"), b"video").unwrap();
        std::fs::write(sibling_dir.join("99-video.mp4"), b"other").unwrap();

        let db = MockDatabase::new(DatabaseBackend::MySql)
            .append_query_results::<crate::entity::storage::Model, _, _>(vec![vec![mk_storage(
                1,
                tmp.path().to_string_lossy().into_owned(),
            )]])
            .into_connection();
        let state = AppState::for_test_with_db(db);

        let event = mk_event(event_id, monitor_id, Some(1), Scheme::Shallow);
        delete_event_media(&state, &event).await.unwrap();

        assert!(
            !event_dir.exists(),
            "the event's own directory must be removed"
        );
        assert!(
            sibling_dir.exists(),
            "a sibling event's directory must not be touched"
        );
    }

    /// An already-absent media directory is not an error (idempotent cleanup).
    #[tokio::test]
    async fn delete_event_media_tolerates_missing_directory() {
        use sea_orm::{DatabaseBackend, MockDatabase};

        let tmp = tempfile::tempdir().unwrap();
        let db = MockDatabase::new(DatabaseBackend::MySql)
            .append_query_results::<crate::entity::storage::Model, _, _>(vec![vec![mk_storage(
                1,
                tmp.path().to_string_lossy().into_owned(),
            )]])
            .into_connection();
        let state = AppState::for_test_with_db(db);

        // Nothing created on disk; the resolved directory does not exist.
        let event = mk_event(7, 1, Some(1), Scheme::Shallow);
        delete_event_media(&state, &event)
            .await
            .expect("absent media directory must not be an error");
    }
}
