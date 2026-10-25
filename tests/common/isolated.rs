//! A database of a test's own (#241).
//!
//! Integration tests share one database and the tests in a binary run in
//! parallel. Most only touch rows they made, but some act on the whole
//! database: the media job runner takes every queued job, and maintenance
//! and retention passes sweep every event. Two such tests interfere.
//!
//! [`IsolatedDb::create`] makes a fresh database with ZoneMinder's schema (as
//! `scripts/setup-ci-db.sh` loads it) and drops it when the handle goes. It
//! needs `TEST_DATABASE_ADMIN_URL`, a server URL with rights to create
//! databases (CI sets root's). Without it the test uses the shared database
//! and takes turns with the other isolated tests in its binary, so they still
//! can't collide.
#![allow(dead_code)]

use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};

use sea_orm::{Database, DatabaseConnection};

use super::test_db::get_test_db;

/// The schema the test database gets, with `name` as its database name:
/// `zm_create.sql.in` with its `source` lines inlined from `db/`, then the
/// Config rows when `db/zm_config.sql` exists.
fn schema_sql(name: &str) -> String {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let create = std::fs::read_to_string(root.join("zm_create.sql.in")).expect("zm_create.sql.in");
    let mut out = String::with_capacity(create.len() * 2);
    for line in create.lines() {
        if let Some(rest) = line.strip_prefix("source @PKGDATADIR@/db/") {
            let file = rest.trim().trim_end_matches(';');
            out.push_str(&std::fs::read_to_string(root.join("db").join(file)).expect(file));
        } else {
            out.push_str(line);
        }
        out.push('\n');
    }
    if let Ok(config) = std::fs::read_to_string(root.join("db/zm_config.sql")) {
        out.push_str(&config);
    }
    out.replace("@ZM_DB_NAME@", name)
        .replace("@ZM_MYSQL_ENGINE@", "InnoDB")
        .replace("@PKGDATADIR@", ".")
}

/// Run `sql` through the mysql client against the server in `url`.
fn mysql(url: &url::Url, sql: &str) {
    let client = ["mariadb", "mysql"]
        .into_iter()
        .find(|c| Command::new(c).arg("--version").output().is_ok())
        .expect("a mysql or mariadb client");
    let mut child = Command::new(client)
        .arg(format!("-h{}", url.host_str().unwrap_or("127.0.0.1")))
        .arg(format!("-P{}", url.port().unwrap_or(3306)))
        .arg(format!("-u{}", url.username()))
        .arg(format!(
            "-p{}",
            urlencoding::decode(url.password().unwrap_or("")).unwrap()
        ))
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("run the mysql client");
    use std::io::Write;
    child
        .stdin
        .take()
        .unwrap()
        .write_all(sql.as_bytes())
        .expect("send SQL");
    let out = child.wait_with_output().expect("mysql client");
    let errors: Vec<_> = String::from_utf8_lossy(&out.stderr)
        .lines()
        .filter(|l| l.starts_with("ERROR") && !l.contains("ERROR 1304"))
        .map(str::to_string)
        .collect();
    assert!(errors.is_empty(), "loading the test schema: {errors:?}");
}

/// A test's own database, dropped with the handle.
pub struct IsolatedDb {
    url: String,
    admin: Option<(url::Url, String)>,
    _turn: Option<tokio::sync::MutexGuard<'static, ()>>,
}

/// Isolated tests take turns on the shared database when they can't have
/// their own.
static SHARED_TURN: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

impl IsolatedDb {
    pub async fn create() -> Self {
        let Ok(admin) = std::env::var("TEST_DATABASE_ADMIN_URL") else {
            let turn = SHARED_TURN.lock().await;
            let url = std::env::var("TEST_DATABASE_URL")
                .or_else(|_| std::env::var("DATABASE_URL"))
                .unwrap_or_else(|_| "mysql://zmuser:zmpass@127.0.0.1:3307/zm_test".into());
            return Self {
                url,
                admin: None,
                _turn: Some(turn),
            };
        };
        static N: AtomicU32 = AtomicU32::new(0);
        let name = format!(
            "zm_test_iso_{}_{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        );
        let admin = url::Url::parse(&admin).expect("TEST_DATABASE_ADMIN_URL");
        let sql = format!(
            "DROP DATABASE IF EXISTS `{name}`; CREATE DATABASE `{name}`;\n{}",
            schema_sql(&name)
        );
        let load = admin.clone();
        tokio::task::spawn_blocking(move || mysql(&load, &sql))
            .await
            .unwrap();
        let mut url = admin.clone();
        url.set_path(&format!("/{name}"));
        Self {
            url: url.to_string(),
            admin: Some((admin, name)),
            _turn: None,
        }
    }

    /// A new connection to this database, with the session options zm-api's
    /// own URL carries.
    pub async fn connect(&self) -> DatabaseConnection {
        if self.admin.is_none() {
            return get_test_db().await.expect("shared test database");
        }
        let sep = if self.url.contains('?') { '&' } else { '?' };
        Database::connect(format!("{}{sep}timezone=SYSTEM", self.url))
            .await
            .expect("connect to the isolated test database")
    }
}

impl Drop for IsolatedDb {
    fn drop(&mut self) {
        if let Some((admin, name)) = self.admin.take() {
            mysql(&admin, &format!("DROP DATABASE IF EXISTS `{name}`;"));
        }
    }
}
