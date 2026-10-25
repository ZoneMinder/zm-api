//! Integration tests for the native replacements of `zmaudit.pl` and
//! `zmstats.pl`, against a real schema.
//!
//! The unit tests cover the parsers and the arithmetic; these cover the SQL,
//! which is where a reimplementation of a destructive daemon actually goes
//! wrong. Every deleting path is checked twice: that it removes what it should,
//! and that it leaves alone what it must.
//!
//! Requires the test database — run with:
//!   APP_PROFILE=test-db cargo test --test it_maintenance -- --include-ignored

mod common;

use std::sync::Arc;

use sea_orm::{ConnectionTrait, DatabaseConnection, Statement};
use zm_api::configure::maintenance::{AuditConfig, StatsConfig};
use zm_api::service::maintenance::{audit::AuditService, stats::StatsService};

/// Monitor ids well clear of anything a fixture or a real install would use.
///
/// One per test, because these run in parallel against a shared database and
/// each test's cleanup deletes by monitor id — a single shared id has them
/// deleting each other's fixtures mid-run.
const MON_EMPTY: u32 = 99_311;
const MON_FRAMES: u32 = 99_312;
const MON_UNCLOSED: u32 = 99_313;
const MON_COUNTERS: u32 = 99_314;
const MON_LIVE: u32 = 99_315;
const MON_SUPERSEDED: u32 = 99_316;

/// Every audit pass sweeps the whole database, not just the calling test's
/// monitor, so two tests running passes at once can delete or close each
/// other's fixtures between insert and assertion. The tests take this lock for
/// their whole body; the per-test monitor ids still keep their cleanups apart.
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

async fn exec(db: &DatabaseConnection, sql: impl Into<String>) {
    db.execute_raw(Statement::from_string(
        db.get_database_backend(),
        sql.into(),
    ))
    .await
    .expect("statement");
}

async fn scalar(db: &DatabaseConnection, sql: &str) -> i64 {
    use sea_orm::FromQueryResult;
    #[derive(FromQueryResult)]
    struct Row {
        n: i64,
    }
    Row::find_by_statement(Statement::from_string(
        db.get_database_backend(),
        sql.to_string(),
    ))
    .one(db)
    .await
    .expect("query")
    .map(|r| r.n)
    .unwrap_or(0)
}

/// Remove everything this test file creates, so a failed run cannot poison the
/// next one.
async fn cleanup(db: &DatabaseConnection, monitor: u32, first_event: u64, last_event: u64) {
    exec(
        db,
        format!("DELETE FROM Frames WHERE EventId BETWEEN {first_event} AND {last_event}"),
    )
    .await;
    exec(
        db,
        format!("DELETE FROM Events WHERE MonitorId = {monitor}"),
    )
    .await;
    exec(
        db,
        format!("DELETE FROM Event_Summaries WHERE MonitorId = {monitor}"),
    )
    .await;
    exec(
        db,
        format!("DELETE FROM Monitor_Status WHERE MonitorId = {monitor}"),
    )
    .await;
}

/// Mark a monitor's capture daemon as live right now, as zmc's periodic
/// `Monitor_Status` write does.
async fn mark_capture_live(db: &DatabaseConnection, monitor: u32) {
    exec(
        db,
        format!(
            "INSERT INTO Monitor_Status (MonitorId, Status, CaptureFPS, AnalysisFPS, \
                CaptureBandwidth, UpdatedOn) \
             VALUES ({monitor}, 'Connected', 5.0, 5.0, 0, NOW()) \
             ON DUPLICATE KEY UPDATE Status = 'Connected', UpdatedOn = NOW()"
        ),
    )
    .await;
}

async fn still_open(db: &DatabaseConnection, event: u64) -> bool {
    scalar(
        db,
        &format!("SELECT COUNT(*) AS n FROM Events WHERE Id = {event} AND EndDateTime IS NULL"),
    )
    .await
        == 1
}

/// Regression for #124: an open event with no frame rows on a monitor whose
/// capture daemon is live is a video-only recording in progress, not a leak —
/// neither the empty-event sweep nor the unclosed-event sweep may touch it.
#[tokio::test]
#[ignore = "requires the test database (APP_PROFILE=test-db)"]
async fn an_open_event_on_a_live_capture_daemon_is_left_alone() {
    // Maintenance and retention passes act on the whole database.
    let iso = common::isolated::IsolatedDb::create().await;
    let _serial = SERIAL.lock().await;
    const MON: u32 = MON_LIVE;
    let db = Arc::new(iso.connect().await);
    cleanup(&db, MON, 9_930_040, 9_930_040).await;

    // Live before the event exists: the empty-event test counts candidates
    // globally and runs in parallel, so there must be no window in which this
    // event looks abandoned.
    mark_capture_live(&db, MON).await;
    exec(
        &db,
        format!(
            "INSERT INTO Events (Id, MonitorId, StateId, StartDateTime, EndDateTime, Scheme, Notes) \
             VALUES (9930040, {MON}, 1, DATE_SUB(NOW(), INTERVAL 2 HOUR), NULL, 'Deep', '')"
        ),
    )
    .await;

    let audit = AuditService::new(Arc::clone(&db), audit_config(false));
    audit.run_once().await.expect("pass");
    assert!(
        still_open(&db, 9_930_040).await,
        "the daemon is live and nothing newer exists: the event may still be recording"
    );

    // The daemon stops reporting: now it is the leak the sweep exists for.
    // Give it one old frame first so it is judged by the unclosed-event sweep
    // and not deleted as empty — the empty-event test counts those globally
    // and runs in parallel with this one.
    exec(
        &db,
        "INSERT INTO Frames (EventId, FrameId, Type, TimeStamp, Delta, Score) \
         VALUES (9930040, 1, 'Normal', DATE_SUB(NOW(), INTERVAL 2 HOUR), 0, 0)",
    )
    .await;
    exec(
        &db,
        format!(
            "UPDATE Monitor_Status SET UpdatedOn = DATE_SUB(NOW(), INTERVAL 2 HOUR) \
             WHERE MonitorId = {MON}"
        ),
    )
    .await;
    audit.run_once().await.expect("pass");
    assert!(
        !still_open(&db, 9_930_040).await,
        "closed once the daemon went quiet"
    );

    cleanup(&db, MON, 9_930_040, 9_930_040).await;
}

/// Regression for #124: a live daemon records one event at a time, so an open
/// event with a newer sibling on the same monitor is finished regardless.
#[tokio::test]
#[ignore = "requires the test database (APP_PROFILE=test-db)"]
async fn an_open_event_superseded_on_a_live_monitor_is_closed() {
    // Maintenance and retention passes act on the whole database.
    let iso = common::isolated::IsolatedDb::create().await;
    let _serial = SERIAL.lock().await;
    const MON: u32 = MON_SUPERSEDED;
    let db = Arc::new(iso.connect().await);
    cleanup(&db, MON, 9_930_050, 9_930_051).await;

    mark_capture_live(&db, MON).await;
    exec(
        &db,
        format!(
            "INSERT INTO Events (Id, MonitorId, StateId, StartDateTime, EndDateTime, Scheme, Notes) \
             VALUES (9930050, {MON}, 1, DATE_SUB(NOW(), INTERVAL 2 HOUR), NULL, 'Deep', ''), \
                    (9930051, {MON}, 1, NOW(), NULL, 'Deep', '')"
        ),
    )
    .await;
    // An old frame keeps it out of the empty-event sweep (see the live test)
    // so it is the unclosed-event sweep that closes it.
    exec(
        &db,
        "INSERT INTO Frames (EventId, FrameId, Type, TimeStamp, Delta, Score) \
         VALUES (9930050, 1, 'Normal', DATE_SUB(NOW(), INTERVAL 2 HOUR), 0, 0)",
    )
    .await;

    let audit = AuditService::new(Arc::clone(&db), audit_config(false));
    audit.run_once().await.expect("pass");
    assert!(
        !still_open(&db, 9_930_050).await,
        "superseded by 9930051, so closed"
    );
    assert!(
        still_open(&db, 9_930_051).await,
        "the current event is untouched"
    );

    cleanup(&db, MON, 9_930_050, 9_930_051).await;
}

fn audit_config(dry_run: bool) -> AuditConfig {
    AuditConfig {
        enabled: true,
        dry_run,
        min_age_seconds: 3600,
        ..AuditConfig::default()
    }
}

#[tokio::test]
#[ignore = "requires the test database (APP_PROFILE=test-db)"]
async fn empty_events_are_deleted_but_only_when_old_and_unarchived() {
    // Maintenance and retention passes act on the whole database.
    let iso = common::isolated::IsolatedDb::create().await;
    let _serial = SERIAL.lock().await;
    const MON: u32 = MON_EMPTY;
    let db = Arc::new(iso.connect().await);
    cleanup(&db, MON, 9_930_001, 9_930_004).await;

    // 1: old, no frames, not archived  -> must be deleted
    // 2: old, no frames, ARCHIVED      -> must survive (zmaudit deletes this;
    //                                     its Archived guard never fires
    //                                     because the column is not selected)
    // 3: recent, no frames             -> must survive (still recording)
    // 4: old, has a frame              -> must survive
    for (id, age_secs, archived) in [
        (9_930_001u64, 7200, 0),
        (9_930_002, 7200, 1),
        (9_930_003, 60, 0),
        (9_930_004, 7200, 0),
    ] {
        exec(
            &db,
            format!(
                "INSERT INTO Events (Id, MonitorId, StateId, StartDateTime, Archived, Scheme) \
                 VALUES ({id}, {MON}, 1, DATE_SUB(NOW(), INTERVAL {age_secs} SECOND), \
                 {archived}, 'Deep')"
            ),
        )
        .await;
    }
    exec(
        &db,
        "INSERT INTO Frames (EventId, FrameId, Type, TimeStamp, Delta, Score) \
         VALUES (9930004, 1, 'Normal', NOW(), 0, 0)",
    )
    .await;

    // A dry run must change nothing while still reporting the finding.
    let dry = AuditService::new(Arc::clone(&db), audit_config(true));
    let report = dry.run_once().await.expect("dry pass");
    assert_eq!(report.empty_events, 1, "dry run should find exactly one");
    assert_eq!(
        scalar(
            &db,
            &format!("SELECT COUNT(*) AS n FROM Events WHERE MonitorId={MON}")
        )
        .await,
        4,
        "dry run must not delete anything"
    );

    let live = AuditService::new(Arc::clone(&db), audit_config(false));
    live.run_once().await.expect("live pass");

    let survivors = scalar(
        &db,
        &format!("SELECT COUNT(*) AS n FROM Events WHERE MonitorId={MON}"),
    )
    .await;
    assert_eq!(survivors, 3, "only the old unarchived frameless event goes");
    assert_eq!(
        scalar(&db, "SELECT COUNT(*) AS n FROM Events WHERE Id=9930001").await,
        0
    );
    assert_eq!(
        scalar(&db, "SELECT COUNT(*) AS n FROM Events WHERE Id=9930002").await,
        1,
        "an archived event must never be deleted for being empty"
    );
    assert_eq!(
        scalar(&db, "SELECT COUNT(*) AS n FROM Events WHERE Id=9930003").await,
        1,
        "an event younger than min_age is still recording"
    );

    cleanup(&db, MON, 9_930_001, 9_930_004).await;
}

#[tokio::test]
#[ignore = "requires the test database (APP_PROFILE=test-db)"]
async fn orphaned_frames_are_removed_and_live_ones_kept() {
    // Maintenance and retention passes act on the whole database.
    let iso = common::isolated::IsolatedDb::create().await;
    let _serial = SERIAL.lock().await;
    const MON: u32 = MON_FRAMES;
    let db = Arc::new(iso.connect().await);
    cleanup(&db, MON, 9_930_010, 9_930_011).await;

    exec(
        &db,
        format!(
            "INSERT INTO Events (Id, MonitorId, StateId, StartDateTime, Scheme) \
             VALUES (9930010, {MON}, 1, NOW(), 'Deep')"
        ),
    )
    .await;
    // One frame on a real event, one pointing at an event that never existed.
    exec(
        &db,
        "INSERT INTO Frames (EventId, FrameId, Type, TimeStamp, Delta, Score) \
         VALUES (9930010, 1, 'Normal', NOW(), 0, 0), \
                (9930011, 1, 'Normal', NOW(), 0, 0)",
    )
    .await;

    let audit = AuditService::new(Arc::clone(&db), audit_config(false));
    audit.run_once().await.expect("pass");

    assert_eq!(
        scalar(
            &db,
            "SELECT COUNT(*) AS n FROM Frames WHERE EventId=9930011"
        )
        .await,
        0,
        "the orphan should be gone"
    );
    assert_eq!(
        scalar(
            &db,
            "SELECT COUNT(*) AS n FROM Frames WHERE EventId=9930010"
        )
        .await,
        1,
        "a frame belonging to a live event must be kept"
    );

    cleanup(&db, MON, 9_930_010, 9_930_011).await;
}

#[tokio::test]
#[ignore = "requires the test database (APP_PROFILE=test-db)"]
async fn an_unclosed_event_is_closed_from_its_frames() {
    // Maintenance and retention passes act on the whole database.
    let iso = common::isolated::IsolatedDb::create().await;
    let _serial = SERIAL.lock().await;
    const MON: u32 = MON_UNCLOSED;
    let db = Arc::new(iso.connect().await);
    cleanup(&db, MON, 9_930_020, 9_930_020).await;

    exec(
        &db,
        format!(
            "INSERT INTO Events (Id, MonitorId, StateId, StartDateTime, EndDateTime, Scheme, Notes) \
             VALUES (9930020, {MON}, 1, DATE_SUB(NOW(), INTERVAL 2 HOUR), NULL, 'Deep', '')"
        ),
    )
    .await;
    exec(
        &db,
        "INSERT INTO Frames (EventId, FrameId, Type, TimeStamp, Delta, Score) VALUES \
         (9930020, 1, 'Normal', DATE_SUB(NOW(), INTERVAL 2 HOUR), 0, 0), \
         (9930020, 2, 'Alarm',  DATE_SUB(NOW(), INTERVAL 119 MINUTE), 0, 7), \
         (9930020, 3, 'Alarm',  DATE_SUB(NOW(), INTERVAL 118 MINUTE), 0, 3)",
    )
    .await;

    let audit = AuditService::new(Arc::clone(&db), audit_config(false));
    audit.run_once().await.expect("pass");

    use sea_orm::FromQueryResult;
    // Column types are the schema's, not conveniently uniform: Frames and
    // AlarmFrames are INT UNSIGNED, the scores are smaller still. Cast in SQL
    // so the decode does not depend on remembering each width.
    #[derive(FromQueryResult)]
    struct Closed {
        frames: i64,
        alarm_frames: i64,
        tot_score: i64,
        max_score: i64,
        still_open: i64,
        notes: String,
    }
    let row = Closed::find_by_statement(Statement::from_string(
        db.get_database_backend(),
        "SELECT CAST(Frames AS SIGNED) AS frames, \
                CAST(AlarmFrames AS SIGNED) AS alarm_frames, \
                CAST(TotScore AS SIGNED) AS tot_score, \
                CAST(MaxScore AS SIGNED) AS max_score, \
                CAST(ISNULL(EndDateTime) AS SIGNED) AS still_open, \
                Notes AS notes \
         FROM Events WHERE Id = 9930020"
            .to_string(),
    ))
    .one(db.as_ref())
    .await
    .expect("query")
    .expect("event still present");

    assert_eq!(row.still_open, 0, "EndDateTime should now be set");
    assert_eq!(row.frames, 3);
    assert_eq!(row.alarm_frames, 2, "only frames with a score above zero");
    assert_eq!(row.tot_score, 10);
    assert_eq!(row.max_score, 7);
    assert!(
        row.notes.contains("Recovered."),
        "the repair should be visible, got {:?}",
        row.notes
    );

    cleanup(&db, MON, 9_930_020, 9_930_020).await;
}

#[tokio::test]
#[ignore = "requires the test database (APP_PROFILE=test-db)"]
async fn counter_resync_corrects_drift() {
    // Maintenance and retention passes act on the whole database.
    let iso = common::isolated::IsolatedDb::create().await;
    let _serial = SERIAL.lock().await;
    const MON: u32 = MON_COUNTERS;
    let db = Arc::new(iso.connect().await);
    cleanup(&db, MON, 9_930_030, 9_930_031).await;

    exec(
        &db,
        format!(
            "INSERT INTO Events (Id, MonitorId, StateId, StartDateTime, DiskSpace, Archived, Scheme) \
             VALUES (9930030, {MON}, 1, NOW(), 1000, 0, 'Deep'), \
                    (9930031, {MON}, 1, NOW(), 2000, 1, 'Deep')"
        ),
    )
    .await;
    // Deliberately wrong counters, as a missed trigger would leave them.
    exec(
        &db,
        format!(
            "REPLACE INTO Event_Summaries \
             (MonitorId, TotalEvents, TotalEventDiskSpace, ArchivedEvents, ArchivedEventDiskSpace) \
             VALUES ({MON}, 99, 99999, 99, 99999)"
        ),
    )
    .await;

    let audit = AuditService::new(Arc::clone(&db), audit_config(false));
    let report = audit.run_once().await.expect("pass");
    // The Storage.DiskSpace resync failed silently for its whole life (#90);
    // a job error is now a test failure.
    assert!(report.errors.is_empty(), "{:?}", report.errors);

    let total = scalar(
        &db,
        &format!("SELECT TotalEvents AS n FROM Event_Summaries WHERE MonitorId={MON}"),
    )
    .await;
    let disk = scalar(
        &db,
        &format!("SELECT TotalEventDiskSpace AS n FROM Event_Summaries WHERE MonitorId={MON}"),
    )
    .await;
    let archived = scalar(
        &db,
        &format!("SELECT ArchivedEvents AS n FROM Event_Summaries WHERE MonitorId={MON}"),
    )
    .await;

    assert_eq!(total, 2, "TotalEvents should be recomputed from Events");
    assert_eq!(disk, 3000);
    assert_eq!(archived, 1);

    cleanup(&db, MON, 9_930_030, 9_930_031).await;
}

#[tokio::test]
#[ignore = "requires the test database (APP_PROFILE=test-db)"]
async fn a_stats_pass_runs_clean_against_a_real_schema() {
    // Maintenance and retention passes act on the whole database.
    let iso = common::isolated::IsolatedDb::create().await;
    let _serial = SERIAL.lock().await;
    // Every statement zmstats issues, executed once. This does not assert on
    // counts — the shared test database has other tests' rows in it — but it
    // does prove the SQL is valid against the real schema, which is the failure
    // mode that matters for hand-written statements.
    let db = Arc::new(iso.connect().await);
    let stats = StatsService::new(
        Arc::clone(&db),
        StatsConfig {
            enabled: true,
            interval_seconds: 300,
        },
    );
    let report = stats.run_once().await.expect("a stats pass must not error");
    assert!(report.failures.is_empty(), "{:?}", report.failures);
    // Twice, so the CPU-delta branch runs with a primed baseline too.
    let report = stats.run_once().await.expect("second stats pass");
    assert!(report.failures.is_empty(), "{:?}", report.failures);
}

// ---------------------------------------------------------------------------
// The filesystem half. These are the ones that matter most: zmaudit's
// equivalent computes a path and rm -rf's it, and when the computation is wrong
// it removes an unrelated directory with no error. Everything below checks both
// directions — what is taken, and what must be left.
// ---------------------------------------------------------------------------

use zm_api::configure::maintenance::FilesystemAuditConfig;

const MON_FS: u32 = 99_315;

/// `Storage.Path` is `varchar(64)`, and the platform temp directory can be far
/// longer than that on its own — so these roots are deliberately short rather
/// than under `std::env::temp_dir()`.
fn short_root(tag: &str) -> std::path::PathBuf {
    std::path::PathBuf::from(format!("/tmp/zmfsa-{}-{tag}", std::process::id()))
}

/// Filesystem half only.
///
/// `run_once` sweeps the whole database, so leaving the row-level checks on
/// would have these tests deleting the fixtures of the ones running beside
/// them. Each test exercises one thing.
fn fs_audit_config(dry_run: bool) -> AuditConfig {
    AuditConfig {
        enabled: true,
        dry_run,
        min_age_seconds: 0,
        remove_orphaned_frames: false,
        remove_empty_events: false,
        close_unclosed_events: false,
        resync_counters: false,
        // These assert on a single pass, so no confirmation delay.
        filesystem: FilesystemAuditConfig {
            enabled: true,
            confirmations_required: 1,
            ..FilesystemAuditConfig::default()
        },
        ..AuditConfig::default()
    }
}

/// Point a Storage row at a temporary directory, returning its id.
async fn temp_storage(db: &DatabaseConnection, root: &std::path::Path) -> u16 {
    // A failed run leaves its Storage row behind, and the audit walks *every*
    // enabled storage — so a single failure would otherwise skew every later
    // run. Clear any stragglers whose directory no longer exists.
    exec(
        db,
        "DELETE FROM Storage WHERE Name = 'zm-api-fs-audit-test' \
         AND Path NOT IN (SELECT Path FROM (SELECT Path FROM Storage) x WHERE 0)",
    )
    .await;
    let path = root.to_string_lossy().replace('\'', "''");
    exec(
        db,
        format!(
            "INSERT INTO Storage (Name, Path, Type, Scheme, Enabled, DoDelete) \
             VALUES ('zm-api-fs-audit-test', '{path}', 'local', 'Shallow', 1, 1)"
        ),
    )
    .await;
    use sea_orm::FromQueryResult;
    #[derive(FromQueryResult)]
    struct Row {
        n: i64,
    }
    Row::find_by_statement(Statement::from_string(
        db.get_database_backend(),
        "SELECT CAST(LAST_INSERT_ID() AS SIGNED) AS n".to_string(),
    ))
    .one(db)
    .await
    .expect("query")
    .map(|r| r.n as u16)
    .expect("storage id")
}

async fn drop_storage(db: &DatabaseConnection, id: u16) {
    exec(db, format!("DELETE FROM Storage WHERE Id = {id}")).await;
}

#[tokio::test]
#[ignore = "requires the test database (APP_PROFILE=test-db)"]
async fn an_orphan_directory_is_quarantined_and_a_live_one_is_not() {
    // Maintenance and retention passes act on the whole database.
    let iso = common::isolated::IsolatedDb::create().await;
    let _serial = SERIAL.lock().await;
    let db = Arc::new(iso.connect().await);
    let root = short_root("orphan");
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();

    // Two Shallow event directories: one with a row, one without.
    for id in [9_930_040u64, 9_930_041] {
        let dir = root.join(MON_FS.to_string()).join(id.to_string());
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(format!("{id}-video.mp4")), b"x").unwrap();
    }

    exec(&db, "DELETE FROM Events WHERE Id IN (9930040, 9930041)").await;
    let storage_id = temp_storage(&db, &root).await;
    exec(
        &db,
        format!(
            "INSERT INTO Events (Id, MonitorId, StateId, StorageId, StartDateTime, Scheme) \
             VALUES (9930040, {MON_FS}, 1, {storage_id}, NOW(), 'Shallow')"
        ),
    )
    .await;

    let audit = AuditService::new(Arc::clone(&db), fs_audit_config(false));
    // The audit walks every enabled storage, so the aggregate counter is not
    // this test's to assert on — the filesystem under its own root is.
    audit.run_once().await.expect("pass");

    // The event with a row is untouched, in place.
    assert!(
        root.join(MON_FS.to_string()).join("9930040").is_dir(),
        "a directory whose event still exists must not be moved"
    );
    // The orphan is gone from its original location...
    assert!(
        !root.join(MON_FS.to_string()).join("9930041").exists(),
        "the orphan should have been moved out"
    );
    // ...and recoverable in quarantine, not deleted.
    let quarantine = root.join(".zm-api-quarantine");
    let recovered: Vec<_> = walkdir_files(&quarantine);
    assert!(
        recovered.iter().any(|p| p.contains("event-9930041")),
        "the orphan must be recoverable in quarantine, found: {recovered:?}"
    );
    assert!(
        recovered.iter().any(|p| p.ends_with("9930041-video.mp4")),
        "its contents must survive the move, found: {recovered:?}"
    );

    exec(&db, "DELETE FROM Events WHERE Id = 9930040").await;
    drop_storage(&db, storage_id).await;
    std::fs::remove_dir_all(&root).ok();
}

#[tokio::test]
#[ignore = "requires the test database (APP_PROFILE=test-db)"]
async fn a_dry_run_moves_nothing() {
    // Maintenance and retention passes act on the whole database.
    let iso = common::isolated::IsolatedDb::create().await;
    let _serial = SERIAL.lock().await;
    let db = Arc::new(iso.connect().await);
    let root = short_root("dry");
    let _ = std::fs::remove_dir_all(&root);

    let dir = root.join(MON_FS.to_string()).join("9930050");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("9930050-video.mp4"), b"x").unwrap();

    let storage_id = temp_storage(&db, &root).await;
    let audit = AuditService::new(Arc::clone(&db), fs_audit_config(true));
    audit.run_once().await.expect("pass");

    assert!(
        dir.is_dir(),
        "dry_run must write nothing at all — unlike zmaudit's --report"
    );
    assert!(
        !root.join(".zm-api-quarantine").exists(),
        "a dry run should not even create the quarantine directory"
    );

    drop_storage(&db, storage_id).await;
    std::fs::remove_dir_all(&root).ok();
}

#[tokio::test]
#[ignore = "requires the test database (APP_PROFILE=test-db)"]
async fn an_unmounted_storage_refuses_rather_than_orphaning_everything() {
    // Maintenance and retention passes act on the whole database.
    let iso = common::isolated::IsolatedDb::create().await;
    let _serial = SERIAL.lock().await;
    // The scenario that makes this dangerous: a volume that failed to mount
    // presents an empty directory, and every event looks orphaned.
    let db = Arc::new(iso.connect().await);
    let root = short_root("unmnt");
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();

    exec(&db, "DELETE FROM Events WHERE Id = 9930060").await;
    let storage_id = temp_storage(&db, &root).await;
    exec(
        &db,
        format!(
            "INSERT INTO Events (Id, MonitorId, StateId, StorageId, StartDateTime, Scheme) \
             VALUES (9930060, {MON_FS}, 1, {storage_id}, DATE_SUB(NOW(), INTERVAL 1 DAY), 'Shallow')"
        ),
    )
    .await;

    let mut config = fs_audit_config(false);
    config.filesystem.remove_rows_without_media = true;
    let audit = AuditService::new(Arc::clone(&db), config);
    let report = audit.run_once().await.expect("pass");

    assert_eq!(report.rows_without_media, 0, "nothing may be deleted");
    assert!(
        report
            .refusals
            .iter()
            .any(|r| r.contains("monitor directories")),
        "the refusal should say why, got {:?}",
        report.refusals
    );
    assert_eq!(
        scalar(&db, "SELECT COUNT(*) AS n FROM Events WHERE Id=9930060").await,
        1,
        "the event row must survive an empty storage"
    );

    exec(&db, "DELETE FROM Events WHERE Id = 9930060").await;
    drop_storage(&db, storage_id).await;
    std::fs::remove_dir_all(&root).ok();
}

#[tokio::test]
#[ignore = "requires the test database (APP_PROFILE=test-db)"]
async fn an_unidentifiable_directory_is_reported_but_left_alone() {
    // Maintenance and retention passes act on the whole database.
    let iso = common::isolated::IsolatedDb::create().await;
    let _serial = SERIAL.lock().await;
    // zmaudit reconstructs a timestamp from the path and deletes these. Without
    // an id there is no way to ask whether it is orphaned, so it stays.
    let db = Arc::new(iso.connect().await);
    let root = short_root("unid");
    let _ = std::fs::remove_dir_all(&root);

    let mystery = root.join(MON_FS.to_string()).join("something");
    std::fs::create_dir_all(&mystery).unwrap();
    std::fs::write(mystery.join("readme.txt"), b"not an event").unwrap();

    let storage_id = temp_storage(&db, &root).await;
    let audit = AuditService::new(Arc::clone(&db), fs_audit_config(false));
    let report = audit.run_once().await.expect("pass");

    assert!(
        report.unidentified_dirs >= 1,
        "the directory should be reported as unidentifiable"
    );
    assert!(mystery.is_dir(), "an unidentifiable directory must be left");
    assert!(
        !root.join(".zm-api-quarantine").exists(),
        "nothing under this root should have been quarantined"
    );

    drop_storage(&db, storage_id).await;
    std::fs::remove_dir_all(&root).ok();
}

/// Every file under `dir`, as strings, for readable assertions.
fn walkdir_files(dir: &std::path::Path) -> Vec<String> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&d) else {
            continue;
        };
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p.clone());
            }
            out.push(p.to_string_lossy().into_owned());
        }
    }
    out
}
