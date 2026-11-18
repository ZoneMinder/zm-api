//! Native replacement for `zmstats.pl`.
//!
//! Six independent housekeeping jobs on one timer. None of them touch event
//! media or `Events` rows; the worst any can do is delete a retention-bounded
//! row.
//!
//! 1. Sample this server's CPU and memory into `Server_Stats`, and prune that
//!    table to a day.
//! 2. Mirror the same sample onto the server's own `Servers` row.
//! 3. Evict `Monitor_Status` rows whose heartbeat has stopped.
//! 4. Age events out of the `Events_Hour/Day/Week/Month` windows and resync the
//!    counters those windows feed.
//! 5. Prune `Logs` to its configured retention.
//! 6. Prune expired `Sessions`.
//!
//! ## Lock ordering
//!
//! ZoneMinder's own triggers write `Events → Events_* → Event_Summaries`, and
//! its comments record that a multi-table `UPDATE` takes shared locks it holds
//! to commit *regardless of isolation level*, which deadlocks against those
//! triggers. This follows the same order and issues only single-table
//! statements, for the same reason.

use std::sync::Arc;

use sea_orm::{ConnectionTrait, DatabaseConnection, DbErr, FromQueryResult, Statement};
use tracing::{debug, warn};

use crate::configure::maintenance::StatsConfig;

pub struct StatsService {
    db: Arc<DatabaseConnection>,
    config: StatsConfig,
    /// This host's `Servers.Id`, or 0 on a single-server install — matching
    /// what ZoneMinder writes. See `daemon::server_id`.
    server_id: u32,
    /// Previous `/proc/stat` sample, for computing CPU percentages as a delta.
    /// The first pass has nothing to compare against and reports no percentages.
    last_cpu: tokio::sync::Mutex<Option<CpuSample>>,
}

/// What one stats pass could not do.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct StatsReport {
    /// `"<job>: <error>"` for each job that failed.
    pub failures: Vec<String>,
}

/// Cumulative jiffies from `/proc/stat`'s aggregate `cpu` line.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CpuSample {
    pub user: u64,
    pub nice: u64,
    pub system: u64,
    pub idle: u64,
    pub other: u64,
}

impl CpuSample {
    fn total(&self) -> u64 {
        self.user + self.nice + self.system + self.idle + self.other
    }
}

/// CPU time as percentages over the interval between two samples.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct CpuPercentages {
    pub user: f64,
    pub nice: f64,
    pub system: f64,
    pub idle: f64,
    /// Everything that is not idle.
    pub usage: f64,
}

/// Parse the aggregate `cpu` line of `/proc/stat`.
///
/// Fields after the first four are lumped into `other` (iowait, irq, softirq,
/// steal, guest…) so the total is right on any kernel, however many columns it
/// grows.
pub fn parse_proc_stat(contents: &str) -> Option<CpuSample> {
    let line = contents.lines().find(|l| {
        let mut parts = l.split_whitespace();
        parts.next() == Some("cpu")
    })?;

    let values: Vec<u64> = line
        .split_whitespace()
        .skip(1)
        .filter_map(|v| v.parse().ok())
        .collect();
    if values.len() < 4 {
        return None;
    }

    Some(CpuSample {
        user: values[0],
        nice: values[1],
        system: values[2],
        idle: values[3],
        other: values[4..].iter().sum(),
    })
}

/// Percentages between two cumulative samples.
///
/// Returns `None` when the counters did not advance, rather than dividing by
/// zero — which happens on a sample taken twice inside one clock tick.
pub fn cpu_percentages(previous: &CpuSample, current: &CpuSample) -> Option<CpuPercentages> {
    let total = current.total().checked_sub(previous.total())?;
    if total == 0 {
        return None;
    }
    let pct = |now: u64, before: u64| (now.saturating_sub(before) as f64) * 100.0 / total as f64;
    let idle = pct(current.idle, previous.idle);
    Some(CpuPercentages {
        user: pct(current.user, previous.user),
        nice: pct(current.nice, previous.nice),
        system: pct(current.system, previous.system),
        idle,
        usage: 100.0 - idle,
    })
}

/// `ZM_LOG_DATABASE_LIMIT` is dual-typed: all digits means "keep at most this
/// many rows", anything else is a MySQL interval such as `7 day`.
///
/// ZoneMinder interpolates the raw value straight into SQL. Parsing it into
/// this enum first means a `Config` row cannot inject SQL, and an unparseable
/// value disables pruning instead of producing a broken statement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LogLimit {
    /// Keep at most N rows, oldest deleted first.
    Rows(u64),
    /// Delete rows older than N of `unit`.
    Age { amount: u64, unit: String },
}

/// Intervals MySQL accepts here. An allow-list rather than an escape, so no
/// input can become syntax.
const INTERVAL_UNITS: &[&str] = &[
    "SECOND", "MINUTE", "HOUR", "DAY", "WEEK", "MONTH", "QUARTER", "YEAR",
];

pub fn parse_log_limit(raw: &str) -> Option<LogLimit> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    if let Ok(rows) = raw.parse::<u64>() {
        return (rows > 0).then_some(LogLimit::Rows(rows));
    }

    // "7 day", "1 year", and ZoneMinder's trailing-s forms like "7 days".
    let mut parts = raw.split_whitespace();
    let amount: u64 = parts.next()?.parse().ok()?;
    let unit = parts.next()?.trim_end_matches('s').to_uppercase();
    if parts.next().is_some() || amount == 0 {
        return None;
    }
    INTERVAL_UNITS
        .contains(&unit.as_str())
        .then_some(LogLimit::Age { amount, unit })
}

/// ZoneMinder's `Logs.Level` value for AUDIT rows, which have their own
/// (much longer) retention.
const AUDIT_LEVEL: i32 = -5;

impl StatsService {
    pub fn new(db: Arc<DatabaseConnection>, config: StatsConfig) -> Self {
        Self {
            db,
            config,
            server_id: 0,
            last_cpu: tokio::sync::Mutex::new(None),
        }
    }

    /// Record which `Servers` row is ours (`None` = single-server, written as
    /// 0). Without this the stats used to come from a `ZM_SERVER_ID`
    /// environment variable nothing sets (#100).
    pub fn with_server_id(mut self, server_id: Option<u32>) -> Self {
        self.server_id = server_id.unwrap_or(0);
        self
    }

    /// Spawn the periodic loop. Returns immediately.
    pub fn spawn(self: Arc<Self>) {
        let interval = self.config.interval();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(interval);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                ticker.tick().await;
                match self.run_once().await {
                    Ok(report) if !report.failures.is_empty() => {
                        warn!("stats pass: {} job(s) failed", report.failures.len())
                    }
                    Ok(_) => {}
                    Err(e) => warn!("stats pass failed: {e}"),
                }
            }
        });
    }

    /// One full pass. Each job is independent: a failure is recorded and the
    /// rest still run, because a stats daemon that stops entirely because one
    /// query failed is worse than one that skips a job. The failures come back
    /// in the report so a query that is wrong every pass cannot hide behind a
    /// warn line (#101).
    pub async fn run_once(&self) -> Result<StatsReport, DbErr> {
        let mut report = StatsReport::default();
        let jobs: [(&str, Result<(), DbErr>); 5] = [
            ("server load sample", self.sample_server_load().await),
            (
                "monitor status eviction",
                self.evict_stale_monitor_status().await,
            ),
            (
                "event window maintenance",
                self.age_out_event_windows().await,
            ),
            ("log pruning", self.prune_logs().await),
            ("session pruning", self.prune_sessions().await),
        ];
        for (job, outcome) in jobs {
            if let Err(e) = outcome {
                warn!("{job} failed: {e}");
                report.failures.push(format!("{job}: {e}"));
            }
        }
        Ok(report)
    }

    /// Read the current CPU sample, returning percentages against the previous
    /// one. The first call primes the baseline and yields `None`.
    async fn cpu_snapshot(&self) -> Option<CpuPercentages> {
        let contents = tokio::fs::read_to_string("/proc/stat").await.ok()?;
        let current = parse_proc_stat(&contents)?;
        let mut last = self.last_cpu.lock().await;
        let result = last
            .as_ref()
            .and_then(|prev| cpu_percentages(prev, &current));
        *last = Some(current);
        result
    }

    /// Insert one `Server_Stats` row and trim the table to a day.
    async fn sample_server_load(&self) -> Result<(), DbErr> {
        let cpu = self.cpu_snapshot().await;
        let Some(cpu) = cpu else {
            debug!("no CPU delta yet (first pass, or /proc/stat unreadable)");
            return Ok(());
        };

        let backend = self.db.get_database_backend();
        self.db
            .execute_raw(Statement::from_sql_and_values(
                backend,
                "INSERT INTO Server_Stats \
                 (ServerId, TimeStamp, CpuUserPercent, CpuNicePercent, \
                  CpuSystemPercent, CpuIdlePercent, CpuUsagePercent) \
                 VALUES (?, NOW(), ?, ?, ?, ?, ?)",
                [
                    self.server_id.into(),
                    cpu.user.into(),
                    cpu.nice.into(),
                    cpu.system.into(),
                    cpu.idle.into(),
                    cpu.usage.into(),
                ],
            ))
            .await?;

        if let Err(e) = self.update_server_row(&cpu).await {
            warn!("server row update failed: {e}");
        }

        // Bounded per pass so a backlog drains gradually instead of locking the
        // table; the next tick continues.
        self.db
            .execute_raw(Statement::from_string(
                backend,
                "DELETE FROM Server_Stats WHERE TimeStamp < NOW() - INTERVAL 1 DAY LIMIT 500",
            ))
            .await?;
        Ok(())
    }

    /// Drop `Monitor_Status` rows that have stopped being updated.
    ///
    /// The row is a live heartbeat written by the capture daemon; once it stops
    /// the values are stale and misleading, so absence is more honest than a
    /// frozen FPS reading.
    async fn evict_stale_monitor_status(&self) -> Result<(), DbErr> {
        let backend = self.db.get_database_backend();
        let result = self
            .db
            .execute_raw(Statement::from_string(
                backend,
                "DELETE FROM Monitor_Status \
                 WHERE UpdatedOn < DATE_SUB(NOW(), INTERVAL 1 MINUTE)",
            ))
            .await?;
        if result.rows_affected() > 0 {
            debug!(
                "evicted {} stale monitor status rows",
                result.rows_affected()
            );
        }
        Ok(())
    }

    /// Age events out of the four rolling windows, then recompute the counters
    /// for every monitor whose window changed.
    ///
    /// Rows are only ever *removed* here: ZoneMinder's `Events` triggers insert
    /// them. Deleting from a window fires that window's trigger, which
    /// decrements `Event_Summaries` — the explicit recount afterwards corrects
    /// any drift the triggers have accumulated.
    async fn age_out_event_windows(&self) -> Result<(), DbErr> {
        const WINDOWS: &[(&str, &str)] = &[
            ("Events_Hour", "1 HOUR"),
            ("Events_Day", "1 DAY"),
            ("Events_Week", "1 WEEK"),
            ("Events_Month", "1 MONTH"),
        ];

        let backend = self.db.get_database_backend();
        let mut any_pruned = false;

        for (table, interval) in WINDOWS {
            // Chunked: a long-stopped instance can have a very large backlog,
            // and one unbounded DELETE would hold locks across all of it.
            loop {
                let sql = format!(
                    "DELETE FROM {table} \
                     WHERE StartDateTime < DATE_SUB(NOW(), INTERVAL {interval}) LIMIT 1000"
                );
                let removed = self
                    .db
                    .execute_raw(Statement::from_string(backend, sql))
                    .await?
                    .rows_affected();
                if removed > 0 {
                    any_pruned = true;
                    debug!("aged {removed} rows out of {table}");
                }
                if removed < 1000 {
                    break;
                }
            }
        }

        if any_pruned {
            self.resync_window_counters().await?;
        }
        Ok(())
    }

    /// Recompute the Hour/Day/Week/Month counters from the window tables.
    ///
    /// Deliberately does not touch `TotalEvents`, `TotalEventDiskSpace`,
    /// `ArchivedEvents` or `ArchivedEventDiskSpace` — those are derived from
    /// `Events` rather than from a window, and belong to the audit's full
    /// resync. Writing them from here would need a scan of the whole `Events`
    /// table on a timer measured in minutes.
    async fn resync_window_counters(&self) -> Result<(), DbErr> {
        // Read each window's totals with a plain SELECT (a consistent read,
        // no locks), then write each monitor's row by primary key. The single
        // UPDATE driven by correlated subqueries that was here took shared
        // locks on the window rows and held them to commit — the reverse of
        // the order the Events triggers take, so a closing event and this
        // resync could deadlock, and zmc's event close blocked behind it (#99).
        // This is the shape zmstats.pl settled on for the same reason.
        const WINDOWS: &[(&str, &str, &str)] = &[
            ("Events_Hour", "HourEvents", "HourEventDiskSpace"),
            ("Events_Day", "DayEvents", "DayEventDiskSpace"),
            ("Events_Week", "WeekEvents", "WeekEventDiskSpace"),
            ("Events_Month", "MonthEvents", "MonthEventDiskSpace"),
        ];

        #[derive(FromQueryResult)]
        struct Totals {
            monitor_id: i64,
            n: i64,
            bytes: i64,
        }
        #[derive(FromQueryResult)]
        struct Summary {
            monitor_id: i64,
        }

        let backend = self.db.get_database_backend();
        let monitors: Vec<i64> = Summary::find_by_statement(Statement::from_string(
            backend,
            "SELECT CAST(MonitorId AS SIGNED) AS monitor_id FROM Event_Summaries",
        ))
        .all(self.db.as_ref())
        .await?
        .into_iter()
        .map(|s| s.monitor_id)
        .collect();

        for (table, count_col, bytes_col) in WINDOWS {
            let totals = Totals::find_by_statement(Statement::from_string(
                backend,
                format!(
                    "SELECT CAST(MonitorId AS SIGNED) AS monitor_id, \
                            CAST(COUNT(*) AS SIGNED) AS n, \
                            CAST(COALESCE(SUM(DiskSpace),0) AS SIGNED) AS bytes \
                     FROM {table} GROUP BY MonitorId"
                ),
            ))
            .all(self.db.as_ref())
            .await?;
            let by_monitor: std::collections::HashMap<i64, (i64, i64)> = totals
                .into_iter()
                .map(|t| (t.monitor_id, (t.n, t.bytes)))
                .collect();

            for monitor_id in &monitors {
                let (n, bytes) = by_monitor.get(monitor_id).copied().unwrap_or((0, 0));
                self.db
                    .execute_raw(Statement::from_sql_and_values(
                        backend,
                        format!(
                            "UPDATE Event_Summaries SET {count_col} = ?, {bytes_col} = ? \
                             WHERE MonitorId = ?"
                        ),
                        [n.into(), bytes.into(), (*monitor_id).into()],
                    ))
                    .await?;
            }
        }
        Ok(())
    }

    /// Mirror the current load onto this host's own `Servers` row.
    ///
    /// Only meaningful on a multi-server install, where the resolved server
    /// id says which row is ours; a single-server install has no row to
    /// update and this is skipped.
    async fn update_server_row(&self, cpu: &CpuPercentages) -> Result<(), DbErr> {
        let id = self.server_id;
        if id == 0 {
            return Ok(());
        }
        self.db
            .execute_raw(Statement::from_sql_and_values(
                self.db.get_database_backend(),
                "UPDATE Servers SET CpuUserPercent = ?, CpuNicePercent = ?, \
                 CpuSystemPercent = ?, CpuIdlePercent = ?, CpuUsagePercent = ? \
                 WHERE Id = ?",
                [
                    cpu.user.into(),
                    cpu.nice.into(),
                    cpu.system.into(),
                    cpu.idle.into(),
                    cpu.usage.into(),
                    id.into(),
                ],
            ))
            .await?;
        Ok(())
    }

    /// Prune the `Logs` table under its two independent retentions.
    ///
    /// AUDIT rows are kept far longer than ordinary ones (a year against a
    /// week by default), so each is pruned against its own limit rather than
    /// one policy for the table.
    async fn prune_logs(&self) -> Result<(), DbErr> {
        for (setting, fallback, level_predicate) in [
            (
                "ZM_LOG_DATABASE_LIMIT",
                "7 day",
                format!("Level != {AUDIT_LEVEL}"),
            ),
            (
                "ZM_LOG_AUDIT_DATABASE_LIMIT",
                "1 year",
                format!("Level = {AUDIT_LEVEL}"),
            ),
        ] {
            let raw = read_zm_config_string(self.db.as_ref(), setting)
                .await
                .unwrap_or_else(|| fallback.to_string());
            let Some(limit) = parse_log_limit(&raw) else {
                // An empty value disables pruning in ZoneMinder; anything else
                // unparseable is a misconfiguration worth surfacing rather than
                // guessing around.
                if !raw.trim().is_empty() {
                    warn!("{setting} = {raw:?} is not a row count or interval; not pruning");
                }
                continue;
            };
            self.prune_logs_to(&level_predicate, &limit).await?;
        }
        Ok(())
    }

    async fn prune_logs_to(&self, level_predicate: &str, limit: &LogLimit) -> Result<(), DbErr> {
        let backend = self.db.get_database_backend();
        match limit {
            LogLimit::Age { amount, unit } => loop {
                // `amount` is a parsed u64 and `unit` comes from an allow-list,
                // so neither can carry syntax.
                let sql = format!(
                    "DELETE LOW_PRIORITY FROM Logs WHERE {level_predicate} \
                     AND TimeKey < UNIX_TIMESTAMP(NOW() - INTERVAL {amount} {unit}) LIMIT 500"
                );
                let removed = self
                    .db
                    .execute_raw(Statement::from_string(backend, sql))
                    .await?
                    .rows_affected();
                if removed > 0 {
                    debug!("pruned {removed} log rows ({level_predicate})");
                }
                if removed < 500 {
                    break;
                }
            },
            LogLimit::Rows(keep) => {
                use sea_orm::FromQueryResult;
                #[derive(FromQueryResult)]
                struct CountRow {
                    n: i64,
                }
                let total = CountRow::find_by_statement(Statement::from_string(
                    backend,
                    format!("SELECT COUNT(*) AS n FROM Logs WHERE {level_predicate}"),
                ))
                .one(self.db.as_ref())
                .await?
                .map(|r| r.n.max(0) as u64)
                .unwrap_or(0);

                if total > *keep {
                    let excess = total - keep;
                    let sql = format!(
                        "DELETE LOW_PRIORITY FROM Logs WHERE {level_predicate} \
                         ORDER BY TimeKey ASC LIMIT {excess}"
                    );
                    let removed = self
                        .db
                        .execute_raw(Statement::from_string(backend, sql))
                        .await?
                        .rows_affected();
                    debug!("pruned {removed} log rows over the {keep}-row limit");
                }
            }
        }
        Ok(())
    }

    /// Delete sessions whose last access is older than the cookie lifetime.
    ///
    /// These are the PHP web UI's sessions, which nothing else expires.
    async fn prune_sessions(&self) -> Result<(), DbErr> {
        let lifetime = self.cookie_lifetime_secs().await;
        let cutoff = (crate::util::time::unix_now() as u64).saturating_sub(lifetime);
        let backend = self.db.get_database_backend();
        loop {
            let removed = self
                .db
                .execute_raw(Statement::from_sql_and_values(
                    backend,
                    "DELETE FROM Sessions WHERE access < ? LIMIT 500",
                    [cutoff.into()],
                ))
                .await?
                .rows_affected();
            if removed > 0 {
                debug!("pruned {removed} expired sessions");
            }
            if removed < 500 {
                break;
            }
        }
        Ok(())
    }

    /// `ZM_COOKIE_LIFETIME`, defaulting to ZoneMinder's own 3600 seconds.
    async fn cookie_lifetime_secs(&self) -> u64 {
        read_zm_config_u64(self.db.as_ref(), "ZM_COOKIE_LIFETIME")
            .await
            .unwrap_or(3600)
    }
}

/// Read a raw string out of ZoneMinder's `Config` table.
pub(crate) async fn read_zm_config_string(db: &DatabaseConnection, name: &str) -> Option<String> {
    use sea_orm::FromQueryResult;

    #[derive(FromQueryResult)]
    struct Row {
        value: String,
    }

    Row::find_by_statement(Statement::from_sql_and_values(
        db.get_database_backend(),
        "SELECT Value AS value FROM Config WHERE Name = ?",
        [name.into()],
    ))
    .one(db)
    .await
    .ok()?
    .map(|r| r.value)
}

/// Read an integer out of ZoneMinder's `Config` table.
pub(crate) async fn read_zm_config_u64(db: &DatabaseConnection, name: &str) -> Option<u64> {
    use sea_orm::FromQueryResult;

    #[derive(FromQueryResult)]
    struct Row {
        value: String,
    }

    let row = Row::find_by_statement(Statement::from_sql_and_values(
        db.get_database_backend(),
        "SELECT Value AS value FROM Config WHERE Name = ?",
        [name.into()],
    ))
    .one(db)
    .await
    .ok()??;
    row.value.trim().parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "\
cpu  100 20 30 800 5 0 5 0 0 0
cpu0 50 10 15 400 2 0 2 0 0 0
intr 12345
";

    #[test]
    fn parses_the_aggregate_cpu_line() {
        let s = parse_proc_stat(SAMPLE).expect("parse");
        assert_eq!(s.user, 100);
        assert_eq!(s.nice, 20);
        assert_eq!(s.system, 30);
        assert_eq!(s.idle, 800);
        // iowait + irq + softirq + steal + guest + guest_nice, whatever the
        // kernel offers, so the total stays correct as columns are added.
        assert_eq!(s.other, 10);
        assert_eq!(s.total(), 960);
    }

    #[test]
    fn ignores_per_core_lines() {
        // "cpu0" must not be mistaken for "cpu".
        let only_cores = "cpu0 1 1 1 1\nintr 5\n";
        assert!(parse_proc_stat(only_cores).is_none());
    }

    #[test]
    fn rejects_a_truncated_line() {
        assert!(parse_proc_stat("cpu 1 2\n").is_none());
    }

    #[test]
    fn percentages_are_computed_over_the_delta() {
        let before = CpuSample {
            user: 100,
            nice: 0,
            system: 100,
            idle: 800,
            other: 0,
        };
        let after = CpuSample {
            user: 200,
            nice: 0,
            system: 100,
            idle: 1700,
            other: 0,
        };
        // 100 user + 900 idle = 1000 jiffies elapsed.
        let pct = cpu_percentages(&before, &after).expect("delta");
        assert!((pct.user - 10.0).abs() < 1e-9, "{pct:?}");
        assert!((pct.idle - 90.0).abs() < 1e-9, "{pct:?}");
        assert!((pct.usage - 10.0).abs() < 1e-9, "{pct:?}");
        assert!((pct.system - 0.0).abs() < 1e-9, "{pct:?}");
    }

    #[test]
    fn an_unchanged_counter_yields_no_percentages() {
        // Two samples inside one tick must not divide by zero.
        let s = CpuSample {
            user: 1,
            nice: 1,
            system: 1,
            idle: 1,
            other: 0,
        };
        assert!(cpu_percentages(&s, &s).is_none());
    }

    #[test]
    fn a_counter_reset_does_not_panic() {
        // /proc/stat counters are monotonic in practice, but a container
        // restart or a suspended VM can appear to rewind them.
        let before = CpuSample {
            user: 500,
            nice: 0,
            system: 0,
            idle: 500,
            other: 0,
        };
        let after = CpuSample {
            user: 1,
            nice: 0,
            system: 0,
            idle: 1,
            other: 0,
        };
        assert!(cpu_percentages(&before, &after).is_none());
    }

    #[test]
    fn a_bare_number_is_a_row_count() {
        assert_eq!(parse_log_limit("10000"), Some(LogLimit::Rows(10000)));
    }

    #[test]
    fn zoneminder_interval_strings_parse() {
        assert_eq!(
            parse_log_limit("7 day"),
            Some(LogLimit::Age {
                amount: 7,
                unit: "DAY".into()
            })
        );
        assert_eq!(
            parse_log_limit("1 year"),
            Some(LogLimit::Age {
                amount: 1,
                unit: "YEAR".into()
            })
        );
        // ZoneMinder writes both "7 day" and "7 days".
        assert_eq!(
            parse_log_limit("30 days"),
            Some(LogLimit::Age {
                amount: 30,
                unit: "DAY".into()
            })
        );
    }

    /// The whole reason this is parsed rather than interpolated: ZoneMinder
    /// splices `ZM_LOG_DATABASE_LIMIT` straight into its DELETE, so a `Config`
    /// row is SQL. Nothing that is not a plain count or an allow-listed
    /// interval may get through.
    #[test]
    fn hostile_or_malformed_limits_are_rejected() {
        for hostile in [
            "1 day; DROP TABLE Logs",
            "1 DAY) OR 1=1 -- ",
            "7 fortnight",
            "day",
            "7", // handled by the count branch, not here
            "",
            "   ",
            "0",
            "0 day",
            "-1 day",
            "1 day 2 hour",
            "1;day",
        ] {
            let parsed = parse_log_limit(hostile);
            let acceptable = matches!(parsed, None | Some(LogLimit::Rows(_)));
            assert!(acceptable, "{hostile:?} parsed as an interval: {parsed:?}");
            if let Some(LogLimit::Age { ref unit, .. }) = parsed {
                assert!(
                    INTERVAL_UNITS.contains(&unit.as_str()),
                    "{unit} escaped the allow-list"
                );
            }
        }
    }

    #[test]
    fn every_accepted_unit_is_on_the_allow_list() {
        for unit in INTERVAL_UNITS {
            let raw = format!("2 {}", unit.to_lowercase());
            match parse_log_limit(&raw) {
                Some(LogLimit::Age { amount, unit: u }) => {
                    assert_eq!(amount, 2);
                    assert_eq!(&u, unit);
                }
                other => panic!("{raw:?} should parse as an interval, got {other:?}"),
            }
        }
    }
}
