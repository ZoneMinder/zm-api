//! Recording migrations as applied on a database whose schema already
//! embodies them.
//!
//! The baseline creates the 1.39.1 schema and the `upstream` migrations carry
//! it forward one ZoneMinder version at a time, so "this database is at
//! ZoneMinder version V" means: the baseline and every upstream migration at
//! or below V have, in effect, run. Two paths arrive here with such a
//! database:
//!
//! - `migrator bridge`: a legacy MySQL install walked the raw `zm_update`
//!   chain to the latest vendored version.
//! - CI / dev environments that load `zm_create.sql.in` directly: the current
//!   schema, created externally.
//!
//! Both record the migrations they embody and let `Migrator::up` run the
//! rest — newer upstream mirrors and zm-api's own tables. Older schemas are
//! refused with a pointer to `migrator bridge`.

use sea_orm::{ConnectionTrait, DatabaseBackend, DatabaseConnection, DbErr, Statement};
use sea_orm_migration::prelude::*;
use sea_orm_migration::MigratorTrait;
use tracing::{info, warn};

use super::legacy_bridge::{chain::CUTOVER_ZM_VERSION, version_lt};
use super::{upstream, Migrator};

/// The baseline migration's recorded version string. Must match
/// `m00000000_000001_zm_baseline::Migration::name()` (asserted in tests).
pub const BASELINE_VERSION: &str = "m00000000_000001_zm_baseline";

/// Every migration a database at ZoneMinder `version` already embodies: the
/// baseline (1.39.1) and each upstream mirror at or below that version.
pub fn migrations_embodied_by(version: &str) -> Vec<String> {
    let mut names = vec![BASELINE_VERSION.to_string()];
    names.extend(
        upstream::VERSIONS
            .iter()
            .filter(|v| !version_lt(version, v))
            .map(|v| upstream::migration_name(v)),
    );
    names
}

async fn exists(
    conn: &DatabaseConnection,
    mysql: &str,
    pg: &str,
    params: Vec<sea_orm::Value>,
) -> Result<bool, DbErr> {
    let stmt = match conn.get_database_backend() {
        DatabaseBackend::MySql => {
            Statement::from_sql_and_values(DatabaseBackend::MySql, mysql, params)
        }
        _ => Statement::from_sql_and_values(DatabaseBackend::Postgres, pg, params),
    };
    Ok(conn.query_one_raw(stmt).await?.is_some())
}

pub(crate) async fn table_exists(conn: &DatabaseConnection, table: &str) -> Result<bool, DbErr> {
    exists(
        conn,
        "SELECT 1 FROM information_schema.TABLES WHERE TABLE_SCHEMA = DATABASE() AND TABLE_NAME = ?",
        "SELECT 1 FROM information_schema.tables WHERE table_schema = current_schema() AND table_name = $1",
        vec![table.into()],
    )
    .await
}

async fn column_exists(
    conn: &DatabaseConnection,
    table: &str,
    column: &str,
) -> Result<bool, DbErr> {
    exists(
        conn,
        "SELECT 1 FROM information_schema.COLUMNS WHERE TABLE_SCHEMA = DATABASE() AND TABLE_NAME = ? AND COLUMN_NAME = ?",
        "SELECT 1 FROM information_schema.columns WHERE table_schema = current_schema() AND table_name = $1 AND column_name = $2",
        vec![table.into(), column.into()],
    )
    .await
}

pub(crate) async fn migration_recorded(
    conn: &DatabaseConnection,
    name: &str,
) -> Result<bool, DbErr> {
    if !table_exists(conn, "seaql_migrations").await? {
        return Ok(false);
    }
    exists(
        conn,
        "SELECT version FROM seaql_migrations WHERE version = ?",
        "SELECT version FROM seaql_migrations WHERE version = $1",
        vec![name.into()],
    )
    .await
}

async fn config_value(conn: &DatabaseConnection, name: &str) -> Result<Option<String>, DbErr> {
    if !table_exists(conn, "Config").await? {
        return Ok(None);
    }
    let stmt = match conn.get_database_backend() {
        DatabaseBackend::MySql => Statement::from_sql_and_values(
            DatabaseBackend::MySql,
            "SELECT Value AS value FROM Config WHERE Name = ?",
            [name.into()],
        ),
        _ => Statement::from_sql_and_values(
            DatabaseBackend::Postgres,
            "SELECT \"Value\" AS value FROM \"Config\" WHERE \"Name\" = $1",
            [name.into()],
        ),
    };
    Ok(conn
        .query_one_raw(stmt)
        .await?
        .and_then(|r| r.try_get::<String>("", "value").ok())
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty()))
}

/// Record `names` as applied in `seaql_migrations` without running them.
pub async fn record_applied(conn: &DatabaseConnection, names: &[String]) -> Result<(), DbErr> {
    Migrator::install(conn).await?;
    let now = crate::util::time::unix_now();
    for name in names {
        if migration_recorded(conn, name).await? {
            continue;
        }
        let insert = Query::insert()
            .into_table(Alias::new("seaql_migrations"))
            .columns([Alias::new("version"), Alias::new("applied_at")])
            .values_panic([name.as_str().into(), now.into()])
            .to_owned();
        conn.execute_raw(conn.get_database_backend().build(&insert))
            .await?;
    }
    Ok(())
}

/// Record the baseline and every upstream migration at or below `version`.
pub async fn record_through_version(conn: &DatabaseConnection, version: &str) -> Result<(), DbErr> {
    let names = migrations_embodied_by(version);
    info!(
        "recording {} migrations as applied (schema is at ZoneMinder {version})",
        names.len()
    );
    record_applied(conn, &names).await
}

/// Which ZoneMinder version an externally created schema is at: the Config
/// row ZoneMinder itself maintains, else the latest vendored version when
/// the schema carries that version's markers.
pub(crate) async fn detect_version(conn: &DatabaseConnection) -> Result<Option<String>, DbErr> {
    if let Some(v) = config_value(conn, "ZM_DYN_DB_VERSION").await? {
        return Ok(Some(v));
    }
    // 1.39.36 dropped Frames.Id and 1.39.39 Controls.Type; 1.39.35 added
    // Frames.AudioLevel.
    let latest = table_exists(conn, "MonitorActions").await?
        && column_exists(conn, "Monitors", "AudioAlarmScore").await?
        && column_exists(conn, "Frames", "AudioLevel").await?
        && !column_exists(conn, "Frames", "Id").await?
        && !column_exists(conn, "Controls", "Type").await?;
    Ok(latest.then(|| CUTOVER_ZM_VERSION.to_string()))
}

/// Stamp an existing ZoneMinder schema with the migrations it embodies, so
/// `Migrator::up` runs only what is newer. Returns true when a stamp was
/// recorded; false when the database is fresh (let the baseline run) or
/// already stamped. Errors when the schema is older than the baseline or
/// its version cannot be determined. A schema newer than the latest mirrored
/// version is stamped with everything known.
pub async fn stamp_baseline_if_schema_current(conn: &DatabaseConnection) -> Result<bool, DbErr> {
    if !table_exists(conn, "Monitors").await? {
        return Ok(false); // fresh database: let the baseline run
    }
    if migration_recorded(conn, BASELINE_VERSION).await? {
        return Ok(false); // already stamped or applied
    }
    let Some(version) = detect_version(conn).await? else {
        return Err(DbErr::Custom(
            "existing ZoneMinder schema of unknown version (no ZM_DYN_DB_VERSION in Config \
             and not the latest layout); run `migrator bridge` to bring it onto the \
             migration system"
                .into(),
        ));
    };
    if version_lt(&version, "1.39.1") {
        return Err(DbErr::Custom(format!(
            "existing ZoneMinder schema at {version} predates the 1.39.1 baseline; \
             run `migrator bridge` to upgrade it onto the migration system"
        )));
    }
    if version_lt(CUTOVER_ZM_VERSION, &version) {
        // ZoneMinder's own packages can run ahead of the updates mirrored
        // here; a host tracking upstream master does so within days of any
        // schema change. Such a schema embodies every mirror this build
        // knows, so stamp them all and let `up` create zm-api's own tables.
        // Refusing instead left those tables missing on first install. The
        // newer upstream columns stay unknown to this build until they are
        // mirrored, and the mirrors are re-runnable when they arrive.
        warn!(
            "existing ZoneMinder schema at {version} is newer than the {CUTOVER_ZM_VERSION} \
             this zm-api mirrors; stamping every known migration. Columns added upstream \
             since {CUTOVER_ZM_VERSION} are not available through the API yet"
        );
    }
    info!("existing ZoneMinder schema at {version} found; stamping the migrations it embodies");
    record_through_version(conn, &version).await?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sea_orm_migration::MigrationName;

    #[test]
    fn baseline_version_matches_migration_name() {
        assert_eq!(
            super::super::m00000000_000001_zm_baseline::Migration.name(),
            BASELINE_VERSION
        );
    }

    /// A schema at version V embodies the baseline and every mirror ≤ V — and
    /// nothing newer, or `up` would skip a change the database lacks.
    #[test]
    fn embodied_set_stops_at_the_version() {
        let at_1_39_1 = migrations_embodied_by("1.39.1");
        assert_eq!(at_1_39_1, vec![BASELINE_VERSION.to_string()]);

        let at_1_39_10 = migrations_embodied_by("1.39.10");
        assert_eq!(at_1_39_10.last().map(String::as_str), Some("zm_1_39_10"));
        assert!(!at_1_39_10.iter().any(|n| n == "zm_1_39_12"));

        // A version with no update file of its own embodies everything below it.
        let at_1_39_11 = migrations_embodied_by("1.39.11");
        assert_eq!(at_1_39_10, at_1_39_11);

        let all = migrations_embodied_by(CUTOVER_ZM_VERSION);
        assert_eq!(all.len(), 1 + upstream::VERSIONS.len());
    }

    /// A ZoneMinder ahead of the mirrored chain (upstream master a few
    /// updates on) embodies everything this build knows about.
    #[test]
    fn embodied_set_for_a_newer_schema_is_every_known_migration() {
        assert!(version_lt(CUTOVER_ZM_VERSION, "1.39.999"));
        assert_eq!(
            migrations_embodied_by("1.39.999"),
            migrations_embodied_by(CUTOVER_ZM_VERSION)
        );
        assert_eq!(
            migrations_embodied_by("1.40.0"),
            migrations_embodied_by(CUTOVER_ZM_VERSION)
        );
    }

    /// Needs STAMP_TEST_DATABASE_URL pointing at a database freshly loaded
    /// from zm_create.sql.in (no seaql_migrations), e.g. the parity_legacy
    /// DB left behind by scripts/schema-parity.sh.
    #[tokio::test]
    #[ignore]
    async fn stamps_externally_created_schema_then_migrates() {
        let url = std::env::var("STAMP_TEST_DATABASE_URL").expect("STAMP_TEST_DATABASE_URL");
        let conn = sea_orm::Database::connect(&url).await.expect("connect");
        assert!(stamp_baseline_if_schema_current(&conn)
            .await
            .expect("first stamp"));
        assert!(!stamp_baseline_if_schema_current(&conn)
            .await
            .expect("second stamp is a no-op"));
        Migrator::up(&conn, None)
            .await
            .expect("remaining migrations apply cleanly");
    }
}
