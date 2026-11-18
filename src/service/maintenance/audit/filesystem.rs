//! Reconciling event directories on disk against `Events` rows.
//!
//! # Enumerate, never derive
//!
//! `zmaudit.pl` computes an event's directory from its `StartDateTime` and
//! `rm -rf`s the result. That computation can be wrong for several reasons —
//! a timezone mismatch between the recording daemon and the auditor, a
//! corrupted or NULL timestamp, a `Scheme` changed after the event was
//! recorded, a wrong `StorageId` — and when it is wrong, the thing removed is
//! some unrelated directory. Nothing reports an error.
//!
//! So no destructive action here ever acts on a path this module constructed.
//! Directories are found by walking, identified from evidence *inside* them,
//! and only a path that was actually enumerated is ever moved. A directory
//! nothing identifies is left alone and logged, where zmaudit reconstructs a
//! timestamp from the path and deletes it.
//!
//! Path derivation is still used, but only as a **canary**: for events matched
//! by both routes, the derived path is compared against the enumerated one, and
//! sustained disagreement disables the destructive half and says so. That turns
//! the invisible failure into an alarm instead of trusting the assumption.
//!
//! # Moves, not deletes
//!
//! Orphans are renamed into a quarantine directory under the same storage root.
//! A rename within one filesystem is atomic and free, and it means enabling
//! this is recoverable: an operator has `quarantine_retention_days` to look at
//! what was taken before a later sweep removes it.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use tracing::{debug, warn};

/// An event directory found by walking, with the id it identified itself as.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FoundEvent {
    /// The path as enumerated. Never reconstructed.
    pub path: PathBuf,
    pub event_id: u64,
}

/// A directory that looks like an event but names no id.
///
/// Never removed: without an id there is no way to ask whether it is orphaned,
/// and "unidentifiable" is exactly the case where a wrong answer is expensive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnidentifiedDirectory {
    pub path: PathBuf,
    pub reason: &'static str,
}

#[derive(Debug, Default)]
pub struct WalkResult {
    pub events: Vec<FoundEvent>,
    pub unidentified: Vec<UnidentifiedDirectory>,
    /// Monitor directories seen. Zero means the storage looks empty, which is
    /// treated as misconfiguration rather than as "everything is orphaned".
    pub monitor_dirs: usize,
}

/// Does this directory entry name identify an event, and as which id?
///
/// Recognised evidence, in order of confidence:
///
/// * `{id}-video.mp4` — the recording itself, named for its event
/// * `.{id}` — the marker ZoneMinder drops beside an event
/// * `{id}-{seq}-capture.jpg` / `{id}-{seq}-analyse.jpg` — frame stills
///
/// Ordering matters only in that all of them must agree; a directory whose
/// contents name two different events is not identified at all.
pub fn identify_from_entries(entries: &[String]) -> Option<u64> {
    let mut found: HashSet<u64> = HashSet::new();

    for entry in entries {
        if let Some(rest) = entry.strip_suffix("-video.mp4") {
            if let Ok(id) = rest.parse::<u64>() {
                found.insert(id);
                continue;
            }
        }
        if let Some(rest) = entry.strip_prefix('.') {
            if !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit()) {
                if let Ok(id) = rest.parse::<u64>() {
                    found.insert(id);
                    continue;
                }
            }
        }
        for suffix in ["-capture.jpg", "-analyse.jpg"] {
            if let Some(rest) = entry.strip_suffix(suffix) {
                // `{id}-{seq}` — take the leading id.
                if let Some((id, seq)) = rest.split_once('-') {
                    if !seq.is_empty()
                        && seq.bytes().all(|b| b.is_ascii_digit())
                        && !id.is_empty()
                        && id.bytes().all(|b| b.is_ascii_digit())
                    {
                        if let Ok(id) = id.parse::<u64>() {
                            found.insert(id);
                        }
                    }
                }
            }
        }
    }

    // Exactly one id, or nothing. Disagreement means we do not know.
    (found.len() == 1).then(|| *found.iter().next().unwrap())
}

/// Whether a directory *name* is an event id, given where it sits.
///
/// Only trusted directly under a monitor directory (Shallow) or under a
/// `YYYY-MM-DD` directory (Medium). Under the Deep scheme the leaf is a
/// two-digit second, which would otherwise be read as event 7.
pub fn identify_from_name(dir_name: &str, parent: ParentKind) -> Option<u64> {
    if !matches!(parent, ParentKind::Monitor | ParentKind::DateDir) {
        return None;
    }
    if dir_name.is_empty() || !dir_name.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    // A Deep second/minute/hour component is always two digits and would be a
    // implausibly small event id; requiring more than two digits keeps the two
    // schemes apart even if the walk mis-classified the parent.
    if dir_name.len() <= 2 {
        return None;
    }
    dir_name.parse().ok()
}

/// What kind of directory a candidate's parent is, which decides whether the
/// candidate's own name can be trusted as an event id.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParentKind {
    /// `{storage}/{monitor_id}`
    Monitor,
    /// `{storage}/{monitor_id}/{YYYY-MM-DD}` — the Medium scheme.
    DateDir,
    /// Anything inside the Deep scheme's `yy/mm/dd/HH/MM` chain.
    DeepComponent,
}

/// `YYYY-MM-DD`, the Medium scheme's day directory.
pub fn is_date_dir(name: &str) -> bool {
    let bytes = name.as_bytes();
    bytes.len() == 10
        && bytes[4] == b'-'
        && bytes[7] == b'-'
        && bytes
            .iter()
            .enumerate()
            .all(|(i, b)| matches!(i, 4 | 7) || b.is_ascii_digit())
}

/// Is `candidate` inside `root` after resolving symlinks?
///
/// Everything destructive passes through here. A storage root an operator can
/// configure, containing symlinks an attacker or an accident could have placed,
/// must not let a move reach outside the tree being audited.
pub fn is_within_root(root: &Path, candidate: &Path) -> bool {
    let (Ok(root), Ok(candidate)) = (root.canonicalize(), candidate.canonicalize()) else {
        // Cannot prove containment, so refuse.
        return false;
    };
    candidate.starts_with(&root) && candidate != root
}

/// Walk one storage root, collecting event directories.
///
/// Bounded by `max_depth` below each monitor directory, and it never follows a
/// symlinked directory — a link out of the tree would otherwise pull unrelated
/// paths into the candidate set.
pub fn walk_storage(root: &Path, max_depth: usize, quarantine_dir: &str) -> WalkResult {
    let mut result = WalkResult::default();

    let Ok(entries) = std::fs::read_dir(root) else {
        warn!("cannot read storage root {}", root.display());
        return result;
    };

    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        // Monitor directories are numeric. Skip our own quarantine, and skip
        // anything else at this level rather than guessing.
        if name == quarantine_dir || !name.bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        let path = entry.path();
        if !path.is_dir() || path.is_symlink() {
            continue;
        }
        result.monitor_dirs += 1;
        walk_below_monitor(&path, ParentKind::Monitor, 0, max_depth, &mut result);
    }

    result
}

fn walk_below_monitor(
    dir: &Path,
    kind: ParentKind,
    depth: usize,
    max_depth: usize,
    result: &mut WalkResult,
) {
    if depth > max_depth {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };

    let mut children: Vec<(PathBuf, String)> = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let path = entry.path();
        // Never descend through a symlink: it can point anywhere.
        if path.is_symlink() {
            continue;
        }
        if path.is_dir() {
            children.push((path, name));
        }
    }

    for (child, name) in children {
        let file_names = list_file_names(&child);

        // Evidence inside the directory is the strongest signal and works for
        // every scheme.
        if let Some(event_id) = identify_from_entries(&file_names) {
            // In the Deep scheme the day directory holds `.{id} -> HH/MM/SS`
            // pointing at the event. A day with one event is then identified
            // by that marker alone; the event is the link's target, not the
            // day directory (#92). This is what zmaudit's readlink does.
            let path = deep_marker_target(&child, event_id).unwrap_or(child);
            result.events.push(FoundEvent { path, event_id });
            continue;
        }

        // Otherwise the name, but only where the scheme makes it an id.
        if let Some(event_id) = identify_from_name(&name, kind) {
            result.events.push(FoundEvent {
                path: child,
                event_id,
            });
            continue;
        }

        // Not an event directory — descend, if it is a shape we recognise.
        let child_kind = if is_date_dir(&name) {
            ParentKind::DateDir
        } else {
            ParentKind::DeepComponent
        };
        let has_subdirs = std::fs::read_dir(&child)
            .map(|mut it| it.any(|e| e.as_ref().map(|e| e.path().is_dir()).unwrap_or(false)))
            .unwrap_or(false);

        if has_subdirs {
            walk_below_monitor(&child, child_kind, depth + 1, max_depth, result);
        } else if !file_names.is_empty() {
            // Files but nothing that names an event. Could be an event whose
            // recording never started, or something else entirely; either way
            // there is no id to check against the database.
            result.unidentified.push(UnidentifiedDirectory {
                path: child,
                reason: "contains files but nothing naming an event id",
            });
        }
        // A directory with neither files nor subdirectories is just empty;
        // removing empties is a separate, much safer job.
    }
}

/// If `dir/.{id}` is a symlink to a directory, that directory.
fn deep_marker_target(dir: &Path, event_id: u64) -> Option<PathBuf> {
    let marker = dir.join(format!(".{event_id}"));
    if !marker.is_symlink() {
        return None;
    }
    let target = dir.join(std::fs::read_link(&marker).ok()?);
    target.is_dir().then_some(target)
}

/// Whether `path` was modified less than `min_age_seconds` ago. Unreadable
/// metadata counts as young: better to wait a pass than to act on a guess.
pub fn is_younger_than(path: &Path, min_age_seconds: u64) -> bool {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.elapsed().ok())
        .map(|age| age.as_secs() < min_age_seconds)
        .unwrap_or(true)
}

fn list_file_names(dir: &Path) -> Vec<String> {
    std::fs::read_dir(dir)
        .map(|entries| {
            entries
                .flatten()
                .filter(|e| e.path().is_file() || e.path().is_symlink())
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default()
}

/// Why a pass refused to do anything destructive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    StorageMissing(PathBuf),
    StorageNotADirectory(PathBuf),
    NoMonitorDirectories(PathBuf),
    DerivationMismatch { checked: usize, mismatched: usize },
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Refusal::StorageMissing(p) => write!(
                f,
                "storage path {} does not exist — a storage that failed to mount \
                 looks exactly like every event being orphaned",
                p.display()
            ),
            Refusal::StorageNotADirectory(p) => {
                write!(f, "storage path {} is not a directory", p.display())
            }
            Refusal::NoMonitorDirectories(p) => write!(
                f,
                "no monitor directories under {} — treating an empty storage as \
                 misconfiguration rather than as a database full of orphans",
                p.display()
            ),
            Refusal::DerivationMismatch {
                checked,
                mismatched,
            } => write!(
                f,
                "{mismatched} of {checked} matched events are not where their \
                 StartDateTime says they should be. Something disagrees about the \
                 layout — most likely a timezone difference from the recording \
                 daemon, or a Scheme changed after these events were written. \
                 Not touching the filesystem until that is understood"
            ),
        }
    }
}

/// Preconditions that must hold before anything is moved or deleted.
pub fn check_preconditions(root: &Path, walk: &WalkResult) -> Result<(), Refusal> {
    if !root.exists() {
        return Err(Refusal::StorageMissing(root.to_path_buf()));
    }
    if !root.is_dir() {
        return Err(Refusal::StorageNotADirectory(root.to_path_buf()));
    }
    if walk.monitor_dirs == 0 {
        return Err(Refusal::NoMonitorDirectories(root.to_path_buf()));
    }
    Ok(())
}

/// Compare where events *are* against where derivation says they *should be*.
///
/// Not used to find anything — purely a check that the assumption underpinning
/// every derived path still holds. `zmaudit.pl`'s timezone bug would show here
/// as a near-total mismatch, on an install where it silently removes the wrong
/// directories.
///
/// A small number of mismatches is normal: an event moved between storages, or
/// recorded before a scheme change. The threshold catches systemic disagreement,
/// not individual drift.
pub fn derivation_canary(
    found: &[FoundEvent],
    derived: &HashMap<u64, PathBuf>,
    tolerated_fraction: f64,
) -> Result<(usize, usize), Refusal> {
    let mut checked = 0usize;
    let mut mismatched = 0usize;

    for event in found {
        let Some(expected) = derived.get(&event.event_id) else {
            continue;
        };
        checked += 1;
        if !same_path(expected, &event.path) {
            mismatched += 1;
            debug!(
                "event {} is at {} but derivation says {}",
                event.event_id,
                event.path.display(),
                expected.display()
            );
        }
    }

    if checked == 0 {
        return Ok((0, 0));
    }
    if (mismatched as f64 / checked as f64) > tolerated_fraction {
        return Err(Refusal::DerivationMismatch {
            checked,
            mismatched,
        });
    }
    Ok((checked, mismatched))
}

/// Compare two paths, resolving them when possible so a symlinked storage root
/// does not read as a mismatch.
fn same_path(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
}

/// Where a quarantined directory goes: `{root}/{quarantine}/{stamp}/{id}`.
///
/// The event id is in the name so an operator can find a specific event, and
/// the timestamp groups a pass together and drives expiry.
pub fn quarantine_target(
    root: &Path,
    quarantine_dir: &str,
    stamp: &str,
    event_id: Option<u64>,
    original: &Path,
) -> PathBuf {
    let leaf = match event_id {
        Some(id) => format!("event-{id}"),
        None => original
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "unnamed".to_string()),
    };
    root.join(quarantine_dir).join(stamp).join(leaf)
}

/// Parse the `YYYYmmddTHHMMSS` stamp a quarantine batch is filed under.
pub fn parse_quarantine_stamp(name: &str) -> Option<jiff::civil::DateTime> {
    jiff::civil::DateTime::strptime("%Y%m%dT%H%M%S", name).ok()
}

/// Quarantine batches older than the retention window.
pub fn expired_quarantine_batches(
    quarantine_root: &Path,
    now: jiff::civil::DateTime,
    retention_days: u64,
) -> Vec<PathBuf> {
    if retention_days == 0 {
        return Vec::new();
    }
    let window = jiff::SignedDuration::from_secs((retention_days as i64).saturating_mul(86_400));
    let cutoff = now.saturating_sub(window);
    let Ok(entries) = std::fs::read_dir(quarantine_root) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            let stamp = parse_quarantine_stamp(&name)?;
            (stamp < cutoff).then(|| e.path())
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn a_video_file_names_its_event() {
        assert_eq!(
            identify_from_entries(&names(&["1234-video.mp4", "snapshot.jpg"])),
            Some(1234)
        );
    }

    #[test]
    fn a_dot_marker_names_its_event() {
        assert_eq!(identify_from_entries(&names(&[".987"])), Some(987));
    }

    /// Deep scheme: `{monitor}/yy/mm/dd/.{id} -> HH/MM/SS`. A day directory
    /// with a single event used to be recorded as the event itself, so the
    /// derivation canary saw every such event as misplaced and the real leaf
    /// was never enumerated (#92).
    #[cfg(unix)]
    #[test]
    fn a_deep_marker_symlink_resolves_to_the_event_leaf() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let day = root.join("1/26/01/15");
        let leaf = day.join("10/15/00");
        std::fs::create_dir_all(&leaf).unwrap();
        std::fs::write(leaf.join("4711-video.mp4"), b"").unwrap();
        std::os::unix::fs::symlink("10/15/00", day.join(".4711")).unwrap();

        let walk = walk_storage(root, 6, ".zm-api-quarantine");
        let found: Vec<(u64, PathBuf)> = walk
            .events
            .iter()
            .map(|e| (e.event_id, e.path.clone()))
            .collect();
        assert_eq!(found, vec![(4711, leaf)]);
    }

    #[test]
    fn a_fresh_directory_is_younger_than_min_age() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(is_younger_than(tmp.path(), 3600));
        assert!(!is_younger_than(tmp.path(), 0));
        assert!(is_younger_than(Path::new("/nonexistent/zm-api-test"), 1));
    }

    #[test]
    fn frame_stills_name_their_event() {
        assert_eq!(
            identify_from_entries(&names(&["55-00001-capture.jpg", "55-00002-analyse.jpg"])),
            Some(55)
        );
    }

    #[test]
    fn contradictory_evidence_identifies_nothing() {
        // Two different events' files in one directory means we do not know
        // which event it is, and a wrong answer here moves the wrong data.
        assert_eq!(
            identify_from_entries(&names(&["10-video.mp4", "20-video.mp4"])),
            None
        );
    }

    #[test]
    fn unrecognised_contents_identify_nothing() {
        assert_eq!(
            identify_from_entries(&names(&["snapshot.jpg", "notes.txt", ".DS_Store"])),
            None
        );
        assert_eq!(identify_from_entries(&[]), None);
    }

    #[test]
    fn a_numeric_leaf_is_an_id_only_where_the_scheme_allows() {
        // Shallow and Medium: the leaf really is the event id.
        assert_eq!(identify_from_name("4711", ParentKind::Monitor), Some(4711));
        assert_eq!(identify_from_name("4711", ParentKind::DateDir), Some(4711));
        // Deep: the leaf is a two-digit second. Reading "07" as event 7 would
        // check the wrong event and quarantine a live recording.
        assert_eq!(identify_from_name("07", ParentKind::DeepComponent), None);
        assert_eq!(identify_from_name("4711", ParentKind::DeepComponent), None);
        // Even under a permissive parent, a two-digit name is a Deep component.
        assert_eq!(identify_from_name("07", ParentKind::Monitor), None);
    }

    #[test]
    fn date_directories_are_recognised() {
        assert!(is_date_dir("2026-08-23"));
        assert!(!is_date_dir("2026-8-23"));
        assert!(!is_date_dir("26/08/23"));
        assert!(!is_date_dir("4711"));
        assert!(!is_date_dir(""));
    }

    #[test]
    fn containment_rejects_escapes() {
        let root = std::env::temp_dir().join(format!("zm-fsaudit-root-{}", std::process::id()));
        let inside = root.join("1").join("4711");
        std::fs::create_dir_all(&inside).unwrap();

        assert!(is_within_root(&root, &inside));
        // The root itself is not a candidate.
        assert!(!is_within_root(&root, &root));
        // Anything outside is refused, however it is spelled.
        assert!(!is_within_root(&root, &root.join("..")));
        assert!(!is_within_root(&root, Path::new("/etc")));
        // A path that does not exist cannot be proven contained.
        assert!(!is_within_root(&root, &root.join("nope")));

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_symlink_out_of_the_tree_is_not_contained() {
        let base = std::env::temp_dir().join(format!("zm-fsaudit-link-{}", std::process::id()));
        let root = base.join("storage");
        let outside = base.join("outside");
        std::fs::create_dir_all(root.join("1")).unwrap();
        std::fs::create_dir_all(&outside).unwrap();

        let link = root.join("1").join("escape");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&outside, &link).unwrap();

        #[cfg(unix)]
        assert!(
            !is_within_root(&root, &link),
            "a symlink pointing outside the storage root must not be treated as inside"
        );

        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn preconditions_refuse_a_missing_or_empty_storage() {
        let missing = PathBuf::from("/nonexistent/zm-storage");
        let empty_walk = WalkResult::default();
        assert!(matches!(
            check_preconditions(&missing, &empty_walk),
            Err(Refusal::StorageMissing(_))
        ));

        let root = std::env::temp_dir().join(format!("zm-fsaudit-pre-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        // Exists, but the walk found no monitor directories: a storage that
        // failed to mount, not a database full of orphans.
        assert!(matches!(
            check_preconditions(&root, &empty_walk),
            Err(Refusal::NoMonitorDirectories(_))
        ));

        let populated = WalkResult {
            monitor_dirs: 1,
            ..Default::default()
        };
        assert!(check_preconditions(&root, &populated).is_ok());

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn the_canary_passes_when_derivation_agrees() {
        let found = vec![
            FoundEvent {
                path: PathBuf::from("/s/1/26/08/23/10/00/00"),
                event_id: 1,
            },
            FoundEvent {
                path: PathBuf::from("/s/1/26/08/23/11/00/00"),
                event_id: 2,
            },
        ];
        let derived: HashMap<u64, PathBuf> = [
            (1, PathBuf::from("/s/1/26/08/23/10/00/00")),
            (2, PathBuf::from("/s/1/26/08/23/11/00/00")),
        ]
        .into_iter()
        .collect();
        assert_eq!(derivation_canary(&found, &derived, 0.1), Ok((2, 0)));
    }

    #[test]
    fn the_canary_catches_a_systemic_timezone_shift() {
        // Exactly what zmaudit's bug looks like: every derived path is an hour
        // out, so every rm -rf would target a directory that is not the event's.
        let found: Vec<FoundEvent> = (0..10)
            .map(|i| FoundEvent {
                path: PathBuf::from(format!("/s/1/26/08/23/10/{i:02}/00")),
                event_id: i,
            })
            .collect();
        let derived: HashMap<u64, PathBuf> = (0..10)
            .map(|i| (i, PathBuf::from(format!("/s/1/26/08/23/11/{i:02}/00"))))
            .collect();

        assert!(matches!(
            derivation_canary(&found, &derived, 0.1),
            Err(Refusal::DerivationMismatch {
                checked: 10,
                mismatched: 10
            })
        ));
    }

    #[test]
    fn the_canary_tolerates_isolated_drift() {
        // One event moved between storages should not stop the pass.
        let found: Vec<FoundEvent> = (0..20)
            .map(|i| FoundEvent {
                path: PathBuf::from(format!("/s/1/{i}")),
                event_id: i,
            })
            .collect();
        let mut derived: HashMap<u64, PathBuf> = (0..20)
            .map(|i| (i, PathBuf::from(format!("/s/1/{i}"))))
            .collect();
        derived.insert(7, PathBuf::from("/other/1/7"));

        assert_eq!(derivation_canary(&found, &derived, 0.1), Ok((20, 1)));
    }

    #[test]
    fn quarantine_targets_are_named_for_their_event() {
        let target = quarantine_target(
            Path::new("/srv/events"),
            ".zm-api-quarantine",
            "20260823T101500",
            Some(4711),
            Path::new("/srv/events/1/26/08/23/10/15/00"),
        );
        assert_eq!(
            target,
            PathBuf::from("/srv/events/.zm-api-quarantine/20260823T101500/event-4711")
        );
    }

    #[test]
    fn expired_batches_are_selected_by_their_stamp() {
        let root = std::env::temp_dir().join(format!("zm-fsaudit-q-{}", std::process::id()));
        std::fs::create_dir_all(root.join("20260801T000000")).unwrap();
        std::fs::create_dir_all(root.join("20260822T000000")).unwrap();
        std::fs::create_dir_all(root.join("not-a-stamp")).unwrap();

        let now = jiff::civil::date(2026, 8, 23).at(12, 0, 0, 0);
        let expired = expired_quarantine_batches(&root, now, 7);

        assert_eq!(expired.len(), 1, "only the batch older than 7 days");
        assert!(expired[0].ends_with("20260801T000000"));

        // Retention 0 keeps everything, which is a choice, not an accident.
        assert!(expired_quarantine_batches(&root, now, 0).is_empty());

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn the_walk_finds_all_three_schemes_and_skips_the_rest() {
        let root = std::env::temp_dir().join(format!("zm-fsaudit-walk-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);

        // Shallow: {mon}/{id}
        std::fs::create_dir_all(root.join("1/4711")).unwrap();
        std::fs::write(root.join("1/4711/4711-video.mp4"), b"x").unwrap();

        // Medium: {mon}/{YYYY-MM-DD}/{id}
        std::fs::create_dir_all(root.join("2/2026-08-23/5000")).unwrap();
        std::fs::write(root.join("2/2026-08-23/5000/snapshot.jpg"), b"x").unwrap();

        // Deep: {mon}/{yy}/{mm}/{dd}/{HH}/{MM}/{SS}
        std::fs::create_dir_all(root.join("3/26/08/23/10/15/00")).unwrap();
        std::fs::write(root.join("3/26/08/23/10/15/00/.6001"), b"").unwrap();

        // Not an event: files but nothing naming an id.
        std::fs::create_dir_all(root.join("4/mystery")).unwrap();
        std::fs::write(root.join("4/mystery/readme.txt"), b"x").unwrap();

        // Our own quarantine must never be walked back into.
        std::fs::create_dir_all(root.join(".zm-api-quarantine/20260801T000000/event-9")).unwrap();

        let walk = walk_storage(&root, 6, ".zm-api-quarantine");

        let mut ids: Vec<u64> = walk.events.iter().map(|e| e.event_id).collect();
        ids.sort_unstable();
        assert_eq!(ids, vec![4711, 5000, 6001], "found: {:?}", walk.events);
        assert_eq!(walk.monitor_dirs, 4);

        // Medium's numeric leaf was identified by name even with no id-bearing
        // file, and the paths are the ones walked, not reconstructed.
        let medium = walk.events.iter().find(|e| e.event_id == 5000).unwrap();
        assert!(medium.path.ends_with("2/2026-08-23/5000"));

        assert_eq!(walk.unidentified.len(), 1);
        assert!(walk.unidentified[0].path.ends_with("4/mystery"));

        assert!(
            !walk.events.iter().any(|e| e.event_id == 9),
            "the quarantine directory must not be re-walked"
        );

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn the_walk_does_not_follow_symlinked_directories() {
        let base = std::env::temp_dir().join(format!("zm-fsaudit-nofollow-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let root = base.join("storage");
        let elsewhere = base.join("elsewhere/9999");
        std::fs::create_dir_all(root.join("1")).unwrap();
        std::fs::create_dir_all(&elsewhere).unwrap();
        std::fs::write(elsewhere.join("9999-video.mp4"), b"x").unwrap();

        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(base.join("elsewhere"), root.join("1/linked")).unwrap();
            let walk = walk_storage(&root, 6, ".zm-api-quarantine");
            assert!(
                !walk.events.iter().any(|e| e.event_id == 9999),
                "a symlink must not pull outside directories into the candidate set"
            );
        }

        std::fs::remove_dir_all(&base).ok();
    }
}
