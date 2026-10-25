// Integration test helper utilities for database testing

use sea_orm::{ConnectionTrait, Database, DatabaseConnection, DbErr, Statement, Value};
use std::env;
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

/// Get a connection to the test database
///
/// This creates a new database connection for integration tests.
/// Make sure to start the test database first:
/// ```bash
/// ./scripts/test-db.sh start
/// ```
pub async fn get_test_db() -> Result<DatabaseConnection, DbErr> {
    let db_url = env::var("TEST_DATABASE_URL")
        .or_else(|_| env::var("DATABASE_URL"))
        .unwrap_or_else(|_| "mysql://zmuser:zmpass@127.0.0.1:3307/zm_test".to_string());

    Database::connect(with_production_session(&db_url)).await
}

/// `url` with the session options zm-api's own URL carries
/// (`DatabaseConfig::create_url`): `timezone=SYSTEM`, so the session runs in
/// the server's zone as in production (#98). Without it sqlx sets UTC, and a
/// test on a non-UTC server would not see what production sees.
fn with_production_session(url: &str) -> String {
    if !url.starts_with("mysql://") || url.contains("timezone=") {
        return url.to_string();
    }
    let sep = if url.contains('?') { '&' } else { '?' };
    format!("{url}{sep}timezone=SYSTEM")
}

/// Apply zm-api's migrations to the test database, one process at a time.
///
/// nextest runs every test in its own process, so an in-process `OnceCell` does
/// not stop two tests migrating at once; when a migration is pending they both
/// apply it and one fails on a duplicate `seaql_migrations` key. A named
/// MariaDB lock, held on a pinned connection, serializes them across processes.
#[allow(dead_code)]
pub async fn migrate_test_db(db: &DatabaseConnection) {
    use sea_orm::TransactionTrait;

    let pinned = db.begin().await.expect("pin a connection for the lock");
    let lock = |sql: &str| Statement::from_string(sea_orm::DatabaseBackend::MySql, sql.to_string());
    pinned
        .execute_raw(lock("SELECT GET_LOCK('zm_api_test_migrate', 120)"))
        .await
        .expect("take migration lock");
    let result = zm_api::client::database::migrate_database(db).await;
    let _ = pinned
        .execute_raw(lock("SELECT RELEASE_LOCK('zm_api_test_migrate')"))
        .await;
    let _ = pinned.rollback().await;
    result.expect("apply zm-api migrations");
}

/// Stable identifier for the current test run.
#[allow(dead_code)]
pub fn test_run_id() -> &'static str {
    static TEST_RUN_ID: OnceLock<String> = OnceLock::new();
    TEST_RUN_ID
        .get_or_init(|| {
            env::var("TEST_RUN_ID").unwrap_or_else(|_| {
                let nanos = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .expect("time")
                    .as_nanos();
                format!("run_{nanos}")
            })
        })
        .as_str()
}

/// Prefix for test data that should be cleaned up.
#[allow(dead_code)]
pub fn test_prefix() -> String {
    format!("Test_{}", test_run_id())
}

/// Delete rows where `column` starts with `prefix`.
/// Table/column names are controlled by tests; keep inputs literal.
#[allow(dead_code)]
pub async fn cleanup_by_prefix(
    db: &DatabaseConnection,
    table: &str,
    column: &str,
    prefix: &str,
) -> Result<u64, DbErr> {
    let backend = db.get_database_backend();
    let sql = format!("DELETE FROM `{}` WHERE `{}` LIKE ?", table, column);
    let statement =
        Statement::from_sql_and_values(backend, sql, vec![Value::from(format!("{prefix}%"))]);
    let result = db.execute_raw(statement).await?;
    Ok(result.rows_affected())
}

/// Cleanup multiple tables using the same prefix.
#[allow(dead_code)]
pub async fn cleanup_by_prefixes(
    db: &DatabaseConnection,
    prefix: &str,
    targets: &[(&str, &str)],
) -> Result<(), DbErr> {
    for (table, column) in targets {
        let _ = cleanup_by_prefix(db, table, column, prefix).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    #[ignore = "Requires running test database"]
    async fn test_can_connect_to_db() {
        let result = get_test_db().await;
        assert!(result.is_ok(), "Should connect to test database");
    }
}
